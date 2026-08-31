use std::time::Duration;

use anyhow::{Context, Result, bail};
use rand::{Rng, SeedableRng, rngs::StdRng};
use reqwest::{Client, StatusCode, header};
use serde::{Deserialize, Serialize};
use temporal_trivia_shared::GameStatus;
use tokio::task::JoinSet;
use uuid::Uuid;

const DEFAULT_PHONE_COUNT: usize = 100;
const MAX_PHONE_COUNT: usize = 1_000;
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const CRASH_BLACKOUT: Duration = Duration::from_secs(6);
const TRANSIENT_RETRY_DELAY: Duration = Duration::from_millis(250);
const TRANSIENT_RETRY_LIMIT: usize = 20;

#[derive(Clone, Debug, Deserialize)]
struct PhoneState {
    callsign: String,
    status: GameStatus,
    assignment: Option<Assignment>,
}

#[derive(Clone, Debug, Deserialize)]
struct Assignment {
    activity_id: String,
    workflow_run_id: String,
    attempt: u32,
    simulation_correct_index: Option<u8>,
}

#[derive(Debug, Serialize)]
struct AssignmentRequest<'a> {
    activity_id: &'a str,
    workflow_run_id: &'a str,
    attempt: u32,
}

#[derive(Debug, Serialize)]
struct AnswerRequest<'a> {
    activity_id: &'a str,
    workflow_run_id: &'a str,
    selected_index: u8,
}

#[derive(Default)]
struct SessionResult {
    answers: u32,
    crashes: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    let count = std::env::args()
        .nth(1)
        .map(|value| {
            value
                .parse::<usize>()
                .context("phone count must be an integer")
        })
        .transpose()?
        .unwrap_or(DEFAULT_PHONE_COUNT);
    if !(1..=MAX_PHONE_COUNT).contains(&count) {
        bail!("phone count must be between 1 and {MAX_PHONE_COUNT}");
    }
    let base_url = std::env::var("PHONE_BASE_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_owned())
        .trim_end_matches('/')
        .to_owned();
    let client = Client::builder().build()?;
    let mut sessions = JoinSet::new();
    for index in 0..count {
        let client = client.clone();
        let base_url = base_url.clone();
        sessions.spawn(async move { run_session(client, base_url, index).await });
    }
    println!("Started {count} end-to-end phone sessions against {base_url}");
    let mut answers = 0_u32;
    let mut crashes = 0_u32;
    while let Some(result) = sessions.join_next().await {
        let result = result.context("phone simulator task panicked")??;
        answers += result.answers;
        crashes += result.crashes;
    }
    println!("Phone simulation finished: {answers} answers, {crashes} crashes");
    Ok(())
}

async fn run_session(client: Client, base_url: String, index: usize) -> Result<SessionResult> {
    let mut rng = StdRng::seed_from_u64(0xD0A8_1E00 + index as u64);
    // Give each load-test client a stable cookie up front. This keeps a
    // transient first-request failure from minting a second phantom player,
    // while the normal browser path still exercises API-issued cookies.
    let session_id = Uuid::from_u128(0xD0A8_1E00_0000_0000_0000_0000_0000_0000 + index as u128);
    let cookie = format!("durable_trivia_session={session_id}");
    let mut state = get_state(&client, &base_url, &cookie).await?;
    let callsign = state.callsign.clone();
    let mut result = SessionResult::default();
    let mut last_assignment: Option<(String, u32)> = None;
    loop {
        if state.status == GameStatus::Finished {
            return Ok(result);
        }
        if let Some(assignment) = state.assignment.clone() {
            let key = (assignment.activity_id.clone(), assignment.attempt);
            if last_assignment.as_ref() != Some(&key) {
                last_assignment = Some(key);
                if rng.random_bool(0.05) {
                    if post_assignment(&client, &base_url, &cookie, "/api/phone/crash", &assignment)
                        .await?
                    {
                        result.crashes += 1;
                        tokio::time::sleep(CRASH_BLACKOUT).await;
                        let _ = post_assignment(
                            &client,
                            &base_url,
                            &cookie,
                            "/api/phone/recovered",
                            &assignment,
                        )
                        .await;
                    }
                } else {
                    let delay_seconds = rng.random_range(1..=10);
                    if keep_alive_for(&client, &base_url, &cookie, &assignment, delay_seconds)
                        .await?
                    {
                        let correct = rng.random_bool(0.8);
                        let correct_index = assignment.simulation_correct_index.context(
                            "phone API must run with PHONE_SIMULATION=1 for load testing",
                        )?;
                        let selected_index = if correct {
                            correct_index
                        } else {
                            (correct_index + 1) % 4
                        };
                        let response = client
                            .post(format!("{base_url}/api/phone/answer"))
                            .header(header::COOKIE, &cookie)
                            .json(&AnswerRequest {
                                activity_id: &assignment.activity_id,
                                workflow_run_id: &assignment.workflow_run_id,
                                selected_index,
                            })
                            .send()
                            .await?;
                        if response.status().is_success() {
                            result.answers += 1;
                        } else if response.status() != StatusCode::CONFLICT {
                            bail!("{callsign} answer failed: {}", response.text().await?);
                        }
                    }
                }
            }
        } else {
            last_assignment = None;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
        state = get_state(&client, &base_url, &cookie).await?;
    }
}

async fn get_state(client: &Client, base_url: &str, cookie: &str) -> Result<PhoneState> {
    for attempt in 1..=TRANSIENT_RETRY_LIMIT {
        let response = client
            .get(format!("{base_url}/api/phone/state"))
            .header(header::COOKIE, cookie)
            .send()
            .await?;
        if response.status().is_success() {
            return Ok(response.json().await?);
        }
        if !response.status().is_server_error() || attempt == TRANSIENT_RETRY_LIMIT {
            return Err(response.error_for_status().unwrap_err().into());
        }
        tokio::time::sleep(TRANSIENT_RETRY_DELAY).await;
    }
    unreachable!("retry loop always returns")
}

async fn keep_alive_for(
    client: &Client,
    base_url: &str,
    cookie: &str,
    assignment: &Assignment,
    seconds: u64,
) -> Result<bool> {
    for _ in 0..seconds {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if !post_assignment(client, base_url, cookie, "/api/phone/heartbeat", assignment).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn post_assignment(
    client: &Client,
    base_url: &str,
    cookie: &str,
    path: &str,
    assignment: &Assignment,
) -> Result<bool> {
    let response = client
        .post(format!("{base_url}{path}"))
        .header(header::COOKIE, cookie)
        .json(&AssignmentRequest {
            activity_id: &assignment.activity_id,
            workflow_run_id: &assignment.workflow_run_id,
            attempt: assignment.attempt,
        })
        .send()
        .await?;
    if response.status().is_success() {
        return Ok(true);
    }
    if response.status() == StatusCode::CONFLICT {
        return Ok(false);
    }
    bail!("{path} failed: {}", response.text().await?)
}
