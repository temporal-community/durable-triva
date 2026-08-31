use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use temporal_trivia_shared::{
    BadgeAnswer, BadgeEvent, GameInput, GameSnapshot, GameStatus, PhoneActivityReady,
    PhoneAssignment, PhoneJoin, PhoneRosterSnapshot, PhoneSessionSnapshot, PowerupNotice,
    parse_env,
};
use temporalio_client::{
    ActivityIdentifier, Client, ClientOptions, Connection, ConnectionOptions, RpcOptions,
    TlsOptions, WorkflowQueryOptions, WorkflowSignalOptions,
};
use temporalio_macros::{workflow, workflow_methods};
use temporalio_sdk::{SyncWorkflowContext, WorkflowContext, WorkflowContextView, WorkflowResult};
use temporalio_sdk_core::Url;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

const ACTIVE_WORKFLOW_ID: &str = "temporal-trivia-active";
const SESSION_COOKIE: &str = "durable_trivia_session";
const PHONE_HTML: &str = include_str!("../../static/phone.html");
const SPACE_GROTESK_TTF: &[u8] = include_bytes!("../../static/space-grotesk.ttf");
const SPACE_MONO_TTF: &[u8] = include_bytes!("../../static/space-mono.ttf");
const ASTRONAUT_SVG: &[u8] = include_bytes!("../../static/astronaut.svg");

#[workflow]
#[derive(Default)]
struct GameWorkflow;

type GameWorkflowRun = <GameWorkflow as temporalio_common::HasWorkflowDefinition>::Run;

#[workflow_methods]
impl GameWorkflow {
    #[run]
    async fn run(
        _ctx: &mut WorkflowContext<Self>,
        _input: GameInput,
    ) -> WorkflowResult<GameSnapshot> {
        unreachable!("the phone API does not register a Workflow Worker")
    }

    #[signal]
    fn phone_joined(&mut self, _ctx: &mut SyncWorkflowContext<Self>, _join: PhoneJoin) {}

    #[signal]
    fn phone_activity_ready(
        &mut self,
        _ctx: &mut SyncWorkflowContext<Self>,
        _ready: PhoneActivityReady,
    ) {
    }

    #[signal]
    fn phone_crashed(&mut self, _ctx: &mut SyncWorkflowContext<Self>, _event: BadgeEvent) {}

    #[signal]
    fn recovered(&mut self, _ctx: &mut SyncWorkflowContext<Self>, _event: BadgeEvent) {}

    #[query]
    fn snapshot(&self, _ctx: &WorkflowContextView) -> GameSnapshot {
        GameSnapshot::default()
    }

    #[query]
    fn phone_session(
        &self,
        _ctx: &WorkflowContextView,
        _session_id: String,
    ) -> PhoneSessionSnapshot {
        PhoneSessionSnapshot::default()
    }

    #[query]
    fn phone_roster(&self, _ctx: &WorkflowContextView) -> PhoneRosterSnapshot {
        PhoneRosterSnapshot::default()
    }
}

#[derive(Clone)]
struct AppState {
    client: Client,
    roster: Arc<RwLock<PhoneRosterSnapshot>>,
    pending_joins: Arc<Mutex<HashSet<String>>>,
}

#[derive(Debug)]
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

#[derive(Debug, Deserialize)]
struct AssignmentRequest {
    activity_id: String,
    workflow_run_id: String,
    attempt: u32,
}

#[derive(Debug, Deserialize)]
struct AnswerRequest {
    activity_id: String,
    workflow_run_id: String,
    selected_index: u8,
}

#[derive(Debug, Clone, Serialize)]
struct PublicQuestion {
    id: String,
    category: String,
    prompt: String,
    answers: [String; 4],
}

#[derive(Debug, Serialize)]
struct PublicAssignment {
    activity_id: String,
    workflow_run_id: String,
    attempt: u32,
    question: PublicQuestion,
    #[serde(skip_serializing_if = "Option::is_none")]
    simulation_correct_index: Option<u8>,
}

#[derive(Debug, Serialize)]
struct PhoneStateResponse {
    callsign: String,
    game_id: Option<String>,
    status: GameStatus,
    deadline_unix_ms: Option<u64>,
    assignment: Option<PublicAssignment>,
    score: i32,
    correct: u32,
    wrong: u32,
    crashes: u32,
    rank: Option<u32>,
    winners: Vec<String>,
    latest_powerup: Option<PowerupNotice>,
}

#[derive(Clone)]
struct Session {
    id: String,
    callsign: String,
    set_cookie: Option<HeaderValue>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect_cloud().await?;
    let roster = query_roster(&client).await.unwrap_or_default();
    let state = AppState {
        client,
        roster: Arc::new(RwLock::new(roster)),
        pending_joins: Arc::new(Mutex::new(HashSet::new())),
    };
    tokio::spawn(refresh_roster(state.clone()));
    let app = Router::new()
        .route("/", get(index))
        .route("/assets/space-grotesk.ttf", get(space_grotesk_asset))
        .route("/assets/space-mono.ttf", get(space_mono_asset))
        .route("/assets/astronaut.svg", get(astronaut_asset))
        .route("/api/phone/state", get(phone_state))
        .route("/api/phone/heartbeat", post(phone_heartbeat))
        .route("/api/phone/answer", post(phone_answer))
        .route("/api/phone/crash", post(phone_crash))
        .route("/api/phone/recovered", post(phone_recovered))
        .with_state(state);
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_owned());
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .with_context(|| format!("bind phone API on port {port}"))?;
    println!("Durable Trivia phone API listening on 0.0.0.0:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(PHONE_HTML)
}

fn asset(content_type: &'static str, body: &'static [u8]) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        body,
    )
        .into_response()
}

async fn space_grotesk_asset() -> Response {
    asset("font/ttf", SPACE_GROTESK_TTF)
}

async fn space_mono_asset() -> Response {
    asset("font/ttf", SPACE_MONO_TTF)
}

async fn astronaut_asset() -> Response {
    asset("image/svg+xml", ASTRONAUT_SVG)
}

async fn phone_state(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let session = session_from_headers(&headers)?;
    let roster = state.roster.read().await.clone();
    let view = roster
        .sessions
        .get(&session.id)
        .cloned()
        .unwrap_or_else(|| PhoneSessionSnapshot {
            session_id: session.id.clone(),
            callsign: session.callsign.clone(),
            game_id: roster.game_id.clone(),
            status: roster.status.clone(),
            deadline_unix_ms: roster.deadline_unix_ms,
            winners: roster.winners.clone(),
            latest_powerup: roster.latest_powerup.clone(),
            ..Default::default()
        });
    if roster.status == GameStatus::Running && !roster.sessions.contains_key(&session.id) {
        let should_signal = state.pending_joins.lock().await.insert(session.id.clone());
        if should_signal
            && let Err(error) = state
                .client
                .get_workflow_handle::<GameWorkflowRun>(ACTIVE_WORKFLOW_ID)
                .signal(
                    GameWorkflow::phone_joined,
                    PhoneJoin {
                        session_id: session.id.clone(),
                        callsign: session.callsign.clone(),
                    },
                    WorkflowSignalOptions::default(),
                )
                .await
        {
            state.pending_joins.lock().await.remove(&session.id);
            eprintln!("phone join will retry: {error}");
        }
    };
    json_with_session(public_state(view, &session.callsign), session)
}

async fn query_roster(client: &Client) -> Result<PhoneRosterSnapshot> {
    client
        .get_workflow_handle::<GameWorkflowRun>(ACTIVE_WORKFLOW_ID)
        .query(
            GameWorkflow::phone_roster,
            (),
            WorkflowQueryOptions::default(),
        )
        .await
        .map_err(|error| anyhow!(error.to_string()))
}

async fn refresh_roster(state: AppState) {
    loop {
        if let Ok(roster) = query_roster(&state.client).await {
            state
                .pending_joins
                .lock()
                .await
                .retain(|session_id| !roster.sessions.contains_key(session_id));
            *state.roster.write().await = roster;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn phone_heartbeat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AssignmentRequest>,
) -> Result<Response, ApiError> {
    let session = session_from_headers(&headers)?;
    state
        .client
        .get_async_activity_handle(activity_identifier(
            &request.workflow_run_id,
            &request.activity_id,
        ))
        .heartbeat(None::<()>, RpcOptions::default())
        .await
        .map_err(async_activity_error)?;
    json_with_session(serde_json::json!({ "accepted": true }), session)
}

async fn phone_answer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AnswerRequest>,
) -> Result<Response, ApiError> {
    if request.selected_index > 3 {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "selected_index must be between 0 and 3".to_owned(),
        ));
    }
    let session = session_from_headers(&headers)?;
    state
        .client
        .get_async_activity_handle(activity_identifier(
            &request.workflow_run_id,
            &request.activity_id,
        ))
        .complete(
            Some(BadgeAnswer {
                badge_id: session.id.clone(),
                callsign: session.callsign.clone(),
                question_id: request.activity_id,
                selected_index: request.selected_index,
            }),
            RpcOptions::default(),
        )
        .await
        .map_err(async_activity_error)?;
    json_with_session(serde_json::json!({ "accepted": true }), session)
}

async fn phone_crash(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AssignmentRequest>,
) -> Result<Response, ApiError> {
    let session = session_from_headers(&headers)?;
    let event = BadgeEvent {
        badge_id: session.id.clone(),
        callsign: session.callsign.clone(),
        question_id: request.activity_id,
        attempt: request.attempt,
    };
    state
        .client
        .get_workflow_handle::<GameWorkflowRun>(ACTIVE_WORKFLOW_ID)
        .signal(
            GameWorkflow::phone_crashed,
            event,
            WorkflowSignalOptions::default(),
        )
        .await
        .map_err(temporal_error)?;
    json_with_session(
        serde_json::json!({ "accepted": true, "blackout_ms": 6000 }),
        session,
    )
}

async fn phone_recovered(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AssignmentRequest>,
) -> Result<Response, ApiError> {
    let session = session_from_headers(&headers)?;
    let event = BadgeEvent {
        badge_id: session.id.clone(),
        callsign: session.callsign.clone(),
        question_id: request.activity_id,
        attempt: request.attempt,
    };
    state
        .client
        .get_workflow_handle::<GameWorkflowRun>(ACTIVE_WORKFLOW_ID)
        .signal(
            GameWorkflow::recovered,
            event,
            WorkflowSignalOptions::default(),
        )
        .await
        .map_err(temporal_error)?;
    json_with_session(serde_json::json!({ "accepted": true }), session)
}

fn activity_identifier(workflow_run_id: &str, activity_id: &str) -> ActivityIdentifier {
    ActivityIdentifier::by_id_workflow(
        ACTIVE_WORKFLOW_ID,
        workflow_run_id.to_owned(),
        activity_id.to_owned(),
    )
}

fn public_state(view: PhoneSessionSnapshot, fallback_callsign: &str) -> PhoneStateResponse {
    let player = view.player.unwrap_or_default();
    PhoneStateResponse {
        callsign: if view.callsign.is_empty() {
            fallback_callsign.to_owned()
        } else {
            view.callsign
        },
        game_id: view.game_id,
        status: view.status,
        deadline_unix_ms: view.deadline_unix_ms,
        assignment: view.assignment.map(public_assignment),
        score: player.score,
        correct: player.correct,
        wrong: player.wrong,
        crashes: player.panics,
        rank: view.rank,
        winners: view.winners,
        latest_powerup: view.latest_powerup,
    }
}

fn public_assignment(assignment: PhoneAssignment) -> PublicAssignment {
    let question = assignment.task.question;
    let simulation_correct_index =
        (std::env::var("PHONE_SIMULATION").as_deref() == Ok("1")).then_some(question.correct_index);
    PublicAssignment {
        activity_id: assignment.activity_id,
        workflow_run_id: assignment.workflow_run_id,
        attempt: assignment.attempt,
        question: PublicQuestion {
            id: question.id,
            category: question.category,
            prompt: question.prompt,
            answers: question.answers,
        },
        simulation_correct_index,
    }
}

fn session_from_headers(headers: &HeaderMap) -> Result<Session, ApiError> {
    let existing = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|cookie| {
                let (name, value) = cookie.trim().split_once('=')?;
                (name == SESSION_COOKIE).then(|| value.to_owned())
            })
        })
        .filter(|value| Uuid::parse_str(value).is_ok());
    let (id, is_new) = existing.map_or_else(
        || (Uuid::new_v4().to_string(), true),
        |value| (value, false),
    );
    let set_cookie = is_new
        .then(|| {
            let secure = if std::env::var("PHONE_COOKIE_SECURE").as_deref() == Ok("0") {
                ""
            } else {
                "; Secure"
            };
            HeaderValue::from_str(&format!(
                "{SESSION_COOKIE}={id}; Path=/; Max-Age=31536000; HttpOnly; SameSite=Lax{secure}"
            ))
        })
        .transpose()
        .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    Ok(Session {
        callsign: phone_callsign(&id),
        id,
        set_cookie,
    })
}

fn phone_callsign(session_id: &str) -> String {
    const ADJECTIVES: [&str; 12] = [
        "BOLD", "BRAVE", "CALM", "KEEN", "LUCKY", "NIMBLE", "QUICK", "RUSTY", "SOLID", "SWIFT",
        "WISE", "ZESTY",
    ];
    const ANIMALS: [&str; 12] = [
        "BADGER", "CRAB", "FALCON", "FOX", "GECKO", "MOTH", "OTTER", "RAVEN", "SEAL", "SHARK",
        "SQUID", "WOLF",
    ];
    let digest = Sha256::digest(session_id.as_bytes());
    format!(
        "{}-{}-{:02}",
        ADJECTIVES[digest[0] as usize % ADJECTIVES.len()],
        ANIMALS[digest[1] as usize % ANIMALS.len()],
        u16::from_be_bytes([digest[2], digest[3]]) % 100
    )
}

fn json_with_session(value: impl Serialize, session: Session) -> Result<Response, ApiError> {
    let mut response = Json(value).into_response();
    if let Some(cookie) = session.set_cookie {
        response.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    Ok(response)
}

fn temporal_error(error: impl std::fmt::Display) -> ApiError {
    ApiError(StatusCode::BAD_GATEWAY, error.to_string())
}

fn async_activity_error(error: impl std::fmt::Display) -> ApiError {
    let message = error.to_string();
    let status = if message.contains("Activity not found")
        || message.contains("activity not found")
        || message.contains("already completed")
    {
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_GATEWAY
    };
    ApiError(status, message)
}

async fn connect_cloud() -> Result<Client> {
    let settings = read_cloud_settings()?;
    let address = required(&settings, "TEMPORAL_ADDRESS")?;
    let target = if address.contains("://") {
        address.to_owned()
    } else {
        format!("https://{address}")
    };
    let options = ConnectionOptions::new(Url::from_str(&target)?)
        .api_key(required(&settings, "TEMPORAL_API_KEY")?)
        .tls_options(TlsOptions::default())
        .build();
    let connection = Connection::connect(options).await?;
    Client::new(
        connection,
        ClientOptions::new(required(&settings, "TEMPORAL_NAMESPACE")?).build(),
    )
    .map_err(Into::into)
}

fn read_cloud_settings() -> Result<HashMap<String, String>> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let project = manifest.parent().context("locate repository root")?;
    let local_path = project.join(".env");
    let fallback_path = project.join(".env.temporal");
    let path = std::env::var_os("TEMPORAL_ENV_FILE")
        .map(PathBuf::from)
        .or_else(|| local_path.is_file().then_some(local_path))
        .or_else(|| fallback_path.is_file().then_some(fallback_path));
    let mut settings = path
        .as_deref()
        .map(read_env_file)
        .transpose()?
        .unwrap_or_default();
    for key in ["TEMPORAL_ADDRESS", "TEMPORAL_NAMESPACE", "TEMPORAL_API_KEY"] {
        if let Ok(value) = std::env::var(key) {
            settings.insert(key.to_owned(), value);
        }
    }
    Ok(settings)
}

fn read_env_file(path: &Path) -> Result<HashMap<String, String>> {
    parse_env(&std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?)
        .map_err(|error| anyhow!("parse {}: {error}", path.display()))
}

fn required<'a>(settings: &'a HashMap<String, String>, name: &str) -> Result<&'a str> {
    let value = settings.get(name).map(String::as_str).unwrap_or("");
    if value.is_empty() {
        bail!("missing {name}; set it in the environment or .env.temporal");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use temporal_trivia_shared::QuestionTask;

    #[test]
    fn callsign_is_stable_and_badge_shaped() {
        let callsign = phone_callsign("d47fa275-71b5-475a-a690-e6ee9e5d55f0");
        assert_eq!(
            callsign,
            phone_callsign("d47fa275-71b5-475a-a690-e6ee9e5d55f0")
        );
        assert_eq!(callsign.split('-').count(), 3);
    }

    #[test]
    fn public_assignment_does_not_serialize_correct_index() {
        let assignment = PhoneAssignment {
            activity_id: "q-1".to_owned(),
            workflow_run_id: "run".to_owned(),
            attempt: 1,
            task: QuestionTask {
                game_id: "g".to_owned(),
                deadline_unix_ms: 1,
                max_deadline_unix_ms: 1,
                question: temporal_trivia_shared::Question {
                    id: "q-1".to_owned(),
                    category: "rust".to_owned(),
                    difficulty: "easy".to_owned(),
                    prompt: "Question?".to_owned(),
                    answers: ["A", "B", "C", "D"].map(str::to_owned),
                    correct_index: 2,
                },
            },
        };
        let json = serde_json::to_string(&public_assignment(assignment)).unwrap();
        assert!(!json.contains("correct_index"));
    }
}
