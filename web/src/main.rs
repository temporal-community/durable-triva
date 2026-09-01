mod model;
mod questions;
mod workflow;

use temporal_trivia_web::cloud;

use std::{
    collections::HashSet,
    convert::Infallible,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response, Sse, sse::Event},
    routing::{get, post},
};
use futures::{Stream, StreamExt};
use qrcode::{QrCode, render::svg};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use temporalio_client::{
    Client, NamespacedClient, WorkflowDescribeOptions, WorkflowExecuteUpdateOptions,
    WorkflowExecution, WorkflowHandle, WorkflowListOptions, WorkflowQueryOptions,
    WorkflowStartOptions, tonic::Request,
};
use temporalio_common::protos::temporal::api::enums::v1::{
    TaskQueueType, WorkflowIdConflictPolicy, WorkflowIdReusePolicy,
};
use temporalio_common::protos::temporal::api::{
    taskqueue::v1::TaskQueue, workflowservice::v1::DescribeTaskQueueRequest,
};
use temporalio_common::telemetry::TelemetryOptions;
use temporalio_sdk::{Runtime, Worker, WorkerOptions, runtime::RuntimeOptions};
use tokio::sync::{Mutex, RwLock, broadcast};
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;

use crate::{
    model::{
        BADGE_TASK_QUEUE, ChaosCommand, GAME_SECONDS, GameInput, GameSnapshot, GameStatus,
        RoundMemo, WEB_TASK_QUEUE, is_badge_worker_identity,
    },
    workflow::{GameWorkflow, GameWorkflowRun},
};

const INDEX_HTML: &str = include_str!("../static/index.html");
// Assets are embedded rather than served from disk so the binary stays
// self-contained, but they live on their own routes so index.html remains
// readable. Revalidate on reload because these stable URLs are not content
// hashed and may change when a new controller binary is deployed.
const ORBIT_RINGS_SVG: &[u8] = include_bytes!("../static/orbit-rings.svg");
const ASTRONAUT_SVG: &[u8] = include_bytes!("../static/astronaut.svg");
const TEMPORAL_LOGO_HORIZONTAL_LIGHT_SVG: &[u8] =
    include_bytes!("../static/temporal-logo-horizontal-light.svg");
const SPACE_GROTESK_TTF: &[u8] = include_bytes!("../static/space-grotesk.ttf");
const SPACE_MONO_TTF: &[u8] = include_bytes!("../static/space-mono.ttf");
const ASSET_CACHE_CONTROL: &str = "no-cache";
const ACTIVE_WORKFLOW_ID: &str = "temporal-trivia-active";
const SUPERVISOR_RESTART_EXIT: i32 = 75;
const WORKFLOW_POLL_INTERVAL: Duration = Duration::from_millis(250);
const WORKFLOW_MAX_ERROR_BACKOFF: Duration = Duration::from_secs(4);
const MAX_BACKLOG_OVERRIDE: usize = 100;
const ROUND_HISTORY_LIMIT: usize = 12;
const ROUND_HISTORY_SCAN_LIMIT: usize = 100;
/// Questions dealt to a round.
///
/// The whole deck travels in `GameInput` and lands in
/// `WorkflowExecutionStarted`, where it stays for the life of the execution.
/// Ten badges over the 90 second ceiling get through roughly 200 -- a badge
/// needs a few seconds to read and press -- so 500 put about 70 KB of undealt
/// questions into every History.
const DECK_SIZE: usize = 150;
const ACTIVE_BADGE_POLLER_MAX_AGE: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct AppState {
    client: Client,
    snapshot: Arc<RwLock<GameSnapshot>>,
    active_workflow: Arc<Mutex<Option<String>>>,
    events: broadcast::Sender<String>,
    instance_id: String,
    restored_snapshot_digest: Arc<RwLock<String>>,
    /// Whether a Workflow Query to Temporal has actually succeeded.
    ///
    /// The recovery proof is the page that claims this process rebuilt its
    /// state from Temporal rather than from memory, so reporting a literal
    /// `true` made the one claim worth checking unfalsifiable.
    temporal_query_succeeded: Arc<AtomicBool>,
}

#[derive(Default, Deserialize)]
struct StartRequest {
    backlog_override: Option<usize>,
}

#[derive(Debug, Serialize)]
struct RoundSummary {
    game_id: String,
    run_id: String,
    closed_unix_ms: Option<u64>,
    winners: Vec<String>,
    badge_count: i64,
    correct_answers: i64,
    wrong_answers: i64,
    crashes: i64,
    reassignments: i64,
    heartbeat_timeouts: i64,
    activity_attempts: i64,
}

#[derive(Debug, Serialize)]
struct RecoveryProof {
    process_id: u32,
    instance_id: String,
    temporal_query_succeeded: bool,
    restored_snapshot_digest: String,
    snapshot_digest: String,
    snapshot: GameSnapshot,
}

#[derive(Debug, Serialize)]
struct WorkflowDetails {
    workflow_id: String,
    run_id: String,
    namespace: String,
    temporal_ui_url: String,
}

#[derive(Debug, Serialize)]
struct PhoneConfig {
    url: String,
}

#[derive(Debug)]
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect_cloud().await?;
    let runtime = Runtime::new_assume_tokio(
        RuntimeOptions::builder()
            .telemetry_options(TelemetryOptions::builder().build())
            .build()
            .map_err(|error| anyhow!(error))?,
    )?;
    let worker_options = WorkerOptions::new(WEB_TASK_QUEUE)
        .register_workflow::<GameWorkflow>()?
        // The SDK's detector treats FuturesUnordered's forwarding wakers as
        // external even when every contained future is an SDK Activity/timer.
        // Core's own FuturesUnordered test uses this same opt-out.
        .detect_nondeterministic_futures(false)
        .build();
    let mut worker = Worker::new(&runtime, client.clone(), worker_options)
        .map_err(|error| anyhow!(error.to_string()))?;

    let (events, _) = broadcast::channel(128);
    let state = AppState {
        client,
        snapshot: Arc::new(RwLock::new(GameSnapshot::default())),
        active_workflow: Arc::new(Mutex::new(None)),
        events,
        instance_id: Uuid::new_v4().to_string(),
        restored_snapshot_digest: Arc::new(RwLock::new(String::new())),
        temporal_query_succeeded: Arc::new(AtomicBool::new(false)),
    };
    let server = async move {
        // Poll the Workflow concurrently with `worker.run()`, but do not
        // accept HTTP connections until Temporal has answered the restoration
        // query. The browser therefore keeps its frozen board throughout a
        // supervised restart instead of seeing this process's empty default.
        resume_active_game(state.clone()).await;
        let app = Router::new()
            .route("/", get(index))
            .route("/assets/orbit-rings.svg", get(orbit_rings_asset))
            .route("/assets/astronaut.svg", get(astronaut_asset))
            .route(
                "/assets/temporal-logo-horizontal-light.svg",
                get(temporal_logo_horizontal_light_asset),
            )
            .route("/assets/space-grotesk.ttf", get(space_grotesk_asset))
            .route("/assets/space-mono.ttf", get(space_mono_asset))
            .route("/assets/phone-qr.svg", get(phone_qr_asset))
            .route("/api/state", get(current_state))
            .route("/api/phone-config", get(phone_config))
            .route("/api/recovery", get(recovery_proof))
            .route("/api/workflow", get(workflow_details))
            .route("/api/events", get(event_stream))
            .route("/api/start", post(start_game))
            .route("/api/chaos/{command}", post(apply_chaos))
            .route("/api/end-round", post(end_round))
            .route("/api/history", get(round_history))
            .route("/api/crash-worker", post(crash_worker))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
        println!("Temporal Trivia controller: http://127.0.0.1:3000");
        axum::serve(listener, app)
            .await
            .map_err(|error| anyhow!(error))
    };
    let worker_run = async move {
        worker
            .run()
            .await
            .map_err(|error| anyhow!(error.to_string()))
    };
    tokio::try_join!(worker_run, server)?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

fn asset(content_type: &'static str, body: &'static [u8]) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, ASSET_CACHE_CONTROL),
        ],
        body,
    )
        .into_response()
}

async fn orbit_rings_asset() -> Response {
    asset("image/svg+xml", ORBIT_RINGS_SVG)
}

async fn astronaut_asset() -> Response {
    asset("image/svg+xml", ASTRONAUT_SVG)
}

async fn temporal_logo_horizontal_light_asset() -> Response {
    asset("image/svg+xml", TEMPORAL_LOGO_HORIZONTAL_LIGHT_SVG)
}

async fn space_grotesk_asset() -> Response {
    asset("font/ttf", SPACE_GROTESK_TTF)
}

async fn space_mono_asset() -> Response {
    asset("font/ttf", SPACE_MONO_TTF)
}

async fn phone_qr_asset() -> Result<Response, ApiError> {
    let url = phone_public_url();
    let code = QrCode::new(url.as_bytes())
        .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let body = code.render::<svg::Color>().quiet_zone(true).build();
    Ok((
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, ASSET_CACHE_CONTROL),
        ],
        body,
    )
        .into_response())
}

async fn phone_config() -> Json<PhoneConfig> {
    Json(PhoneConfig {
        url: phone_public_url(),
    })
}

fn phone_public_url() -> String {
    std::env::var("PUBLIC_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".to_owned())
}

async fn current_state(State(state): State<AppState>) -> Json<GameSnapshot> {
    Json(state.snapshot.read().await.clone())
}

async fn recovery_proof(State(state): State<AppState>) -> Json<RecoveryProof> {
    let snapshot = state.snapshot.read().await.clone();
    Json(RecoveryProof {
        process_id: std::process::id(),
        instance_id: state.instance_id.clone(),
        temporal_query_succeeded: state.temporal_query_succeeded.load(Ordering::Acquire),
        restored_snapshot_digest: state.restored_snapshot_digest.read().await.clone(),
        snapshot_digest: snapshot_digest(&snapshot),
        snapshot,
    })
}

async fn workflow_details(
    State(state): State<AppState>,
) -> Result<Json<WorkflowDetails>, ApiError> {
    if state.snapshot.read().await.game_id.is_none() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "no game has been started".to_owned(),
        ));
    }
    let handle = state
        .client
        .get_workflow_handle::<GameWorkflowRun>(ACTIVE_WORKFLOW_ID);
    let description = handle
        .describe(WorkflowDescribeOptions::default())
        .await
        .map_err(|error| ApiError(StatusCode::BAD_GATEWAY, error.to_string()))?;
    let namespace = state.client.namespace().to_owned();
    let run_id = description.run_id().to_owned();
    Ok(Json(WorkflowDetails {
        workflow_id: ACTIVE_WORKFLOW_ID.to_owned(),
        run_id: run_id.clone(),
        namespace: namespace.clone(),
        temporal_ui_url: format!(
            "https://cloud.temporal.io/namespaces/{namespace}/workflows/{ACTIVE_WORKFLOW_ID}/{run_id}/history"
        ),
    }))
}

async fn event_stream(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let receiver = BroadcastStream::new(state.events.subscribe());
    let stream = receiver.filter_map(|message| async move {
        match message {
            Ok(json) => Some(Ok(Event::default().data(json))),
            Err(error) => {
                eprintln!("SSE subscriber lagged and skipped updates: {error}");
                None
            }
        }
    });
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

async fn start_game(
    State(state): State<AppState>,
    Json(request): Json<StartRequest>,
) -> Result<Json<GameSnapshot>, ApiError> {
    if request
        .backlog_override
        .is_some_and(|value| !(1..=MAX_BACKLOG_OVERRIDE).contains(&value))
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("backlog override must be between 1 and {MAX_BACKLOG_OVERRIDE}"),
        ));
    }
    let detected_badge_count = active_badge_count(&state.client)
        .await
        .map_err(|error| ApiError(StatusCode::BAD_GATEWAY, error.to_string()))?;
    let deck = questions::build_deck(rand::random(), DECK_SIZE)
        .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let game_id = format!("trivia-{}", Uuid::new_v4().simple());
    {
        let mut active = state.active_workflow.lock().await;
        if active.is_some() {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "a game is already running".to_owned(),
            ));
        }
        // Reserve the single-game slot before the asynchronous Cloud call so
        // two simultaneous button presses cannot start two Workflows.
        *active = Some(game_id.clone());
    }

    let input = GameInput {
        game_id: game_id.clone(),
        questions: deck,
        duration_seconds: GAME_SECONDS,
        backlog_override: request.backlog_override,
        detected_badge_count: Some(detected_badge_count),
        index_search_attributes: std::env::var("TRIVIA_SEARCH_ATTRIBUTES").as_deref() == Ok("1"),
    };
    let handle_result = state
        .client
        .start_workflow(
            GameWorkflow::run,
            input,
            WorkflowStartOptions::new(WEB_TASK_QUEUE, ACTIVE_WORKFLOW_ID)
                .id_reuse_policy(WorkflowIdReusePolicy::AllowDuplicate)
                .id_conflict_policy(WorkflowIdConflictPolicy::Fail)
                .build(),
        )
        .await;
    let handle = match handle_result {
        Ok(handle) => handle,
        Err(error) => {
            let mut active = state.active_workflow.lock().await;
            if active.as_deref() == Some(&game_id) {
                *active = None;
            }
            return Err(ApiError(StatusCode::BAD_GATEWAY, error.to_string()));
        }
    };

    let starting = GameSnapshot {
        game_id: Some(game_id.clone()),
        status: GameStatus::Running,
        detected_badge_count: detected_badge_count as u32,
        ..Default::default()
    };
    publish(&state, starting.clone()).await;
    tokio::spawn(observe_workflow(state.clone(), handle, game_id));
    Ok(Json(starting))
}

async fn active_badge_count(client: &Client) -> Result<usize> {
    let request = DescribeTaskQueueRequest {
        namespace: client.namespace(),
        task_queue: Some(TaskQueue {
            name: BADGE_TASK_QUEUE.to_owned(),
            ..Default::default()
        }),
        task_queue_type: TaskQueueType::Activity as i32,
        ..Default::default()
    };
    let pollers = client
        .connection()
        .workflow_service()
        .describe_task_queue(Request::new(request))
        .await
        .context("describe badge Activity Task Queue")?
        .into_inner()
        .pollers;
    let now_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let oldest_active_seconds = now_seconds - ACTIVE_BADGE_POLLER_MAX_AGE.as_secs() as i64;
    Ok(pollers
        .into_iter()
        .filter(|poller| is_badge_worker_identity(&poller.identity))
        .filter(|poller| {
            poller
                .last_access_time
                .as_ref()
                .is_some_and(|last_access| last_access.seconds >= oldest_active_seconds)
        })
        .map(|poller| poller.identity)
        .collect::<HashSet<_>>()
        .len())
}

/// Update rejections come back as `Update failed:` and mean the Workflow's
/// validator declined, which is a conflict rather than a transport failure.
fn update_error(error: impl std::fmt::Display) -> ApiError {
    let message = error.to_string();
    let status = if message.starts_with("Update failed:") {
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_GATEWAY
    };
    ApiError(status, message)
}

async fn end_round(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    if state.active_workflow.lock().await.is_none() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "no game is running".to_owned(),
        ));
    }
    let snapshot = state
        .client
        .get_workflow_handle::<GameWorkflowRun>(ACTIVE_WORKFLOW_ID)
        .execute_update(
            GameWorkflow::end_round,
            (),
            WorkflowExecuteUpdateOptions::default(),
        )
        .await
        .map_err(update_error)?;
    publish(&state, snapshot.clone()).await;
    Ok(Json(
        serde_json::json!({ "accepted": true, "snapshot": snapshot }),
    ))
}

async fn apply_chaos(
    State(state): State<AppState>,
    AxumPath(command): AxumPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let command = match command.as_str() {
        "double-points" => ChaosCommand::DoublePoints,
        "rust-only" => ChaosCommand::RustOnly,
        "sudden-death" => ChaosCommand::SuddenDeath,
        "extend" => ChaosCommand::ExtendThirtySeconds,
        _ => {
            return Err(ApiError(
                StatusCode::NOT_FOUND,
                format!("unknown chaos command: {command}"),
            ));
        }
    };
    if state.active_workflow.lock().await.is_none() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "no game is running".to_owned(),
        ));
    }
    let handle = state
        .client
        .get_workflow_handle::<GameWorkflowRun>(ACTIVE_WORKFLOW_ID);
    let snapshot = handle
        .execute_update(
            GameWorkflow::apply_chaos,
            command,
            WorkflowExecuteUpdateOptions::default(),
        )
        .await
        .map_err(update_error)?;
    publish(&state, snapshot.clone()).await;
    Ok(Json(
        serde_json::json!({ "accepted": true, "snapshot": snapshot }),
    ))
}

async fn crash_worker() -> Result<Json<serde_json::Value>, ApiError> {
    if std::env::var("TRIVIA_SUPERVISED").as_deref() != Ok("1") {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "controller was not started with ./run-web.sh; refusing an unrecoverable exit"
                .to_owned(),
        ));
    }
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::process::exit(SUPERVISOR_RESTART_EXIT);
    });
    Ok(Json(serde_json::json!({
        "accepted": true,
        "message": "Temporal is holding the game while the Mac Worker restarts"
    })))
}

async fn round_history(State(state): State<AppState>) -> Result<Json<Vec<RoundSummary>>, ApiError> {
    let stream = state.client.list_workflows(
        format!("WorkflowId = '{ACTIVE_WORKFLOW_ID}' AND ExecutionStatus = 'Completed'"),
        WorkflowListOptions::builder()
            .limit(ROUND_HISTORY_SCAN_LIMIT)
            .build(),
    );
    tokio::pin!(stream);
    let mut rounds = Vec::new();
    while let Some(execution) = stream.next().await {
        let execution =
            execution.map_err(|error| ApiError(StatusCode::BAD_GATEWAY, error.to_string()))?;
        if let Some(summary) = round_summary(&execution) {
            rounds.push(summary);
        }
    }
    rounds.sort_by_key(|round| std::cmp::Reverse(round.closed_unix_ms.unwrap_or_default()));
    rounds.truncate(ROUND_HISTORY_LIMIT);
    Ok(Json(rounds))
}

fn round_summary(execution: &WorkflowExecution) -> Option<RoundSummary> {
    let memo: RoundMemo = execution.memo().get("TriviaRoundSummary").ok()??;
    Some(RoundSummary {
        game_id: memo.game_id,
        run_id: execution.run_id().to_owned(),
        closed_unix_ms: execution.close_time().map(system_time_unix_ms),
        winners: memo.winners,
        badge_count: memo.badge_count,
        correct_answers: memo.correct_answers,
        wrong_answers: memo.wrong_answers,
        crashes: memo.crashes,
        reassignments: memo.reassignments,
        heartbeat_timeouts: memo.heartbeat_timeouts,
        activity_attempts: memo.activity_attempts,
    })
}

fn system_time_unix_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// The Workflow Query the recovery proof rests on.
///
/// A trait so the health flag's transitions can be exercised without a live
/// `Client`: the real implementation talks to Temporal, and the tests hand in
/// a source that answers or refuses on demand.
trait SnapshotQuery {
    fn snapshot(&self) -> impl Future<Output = Result<GameSnapshot>> + Send;
}

impl SnapshotQuery for WorkflowHandle<Client, GameWorkflowRun> {
    async fn snapshot(&self) -> Result<GameSnapshot> {
        self.query(GameWorkflow::snapshot, (), WorkflowQueryOptions::default())
            .await
            .map_err(|error| anyhow!("query Workflow snapshot: {error}"))
    }
}

/// What a failed Query means for the claim that Temporal answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OnQueryFailure {
    /// Startup: a failure means this process did *not* rebuild from Temporal,
    /// so the recovery proof must stop claiming it did.
    Retract,
    /// Steady state: a transient poll failure does not undo a rebuild that
    /// already happened.
    Keep,
}

/// Runs the Query and records whether Temporal actually answered.
async fn query_recording_health<Q: SnapshotQuery>(
    query: &Q,
    health: &AtomicBool,
    on_failure: OnQueryFailure,
) -> Result<GameSnapshot> {
    match query.snapshot().await {
        Ok(snapshot) => {
            health.store(true, Ordering::Release);
            Ok(snapshot)
        }
        Err(error) => {
            if on_failure == OnQueryFailure::Retract {
                health.store(false, Ordering::Release);
            }
            Err(error)
        }
    }
}

async fn resume_active_game(state: AppState) {
    let handle = state
        .client
        .get_workflow_handle::<GameWorkflowRun>(ACTIVE_WORKFLOW_ID);
    let Ok(snapshot) = query_recording_health(
        &handle,
        &state.temporal_query_succeeded,
        OnQueryFailure::Retract,
    )
    .await
    else {
        let snapshot = state.snapshot.read().await;
        let digest = snapshot_digest(&snapshot);
        *state.restored_snapshot_digest.write().await = digest;
        return;
    };
    *state.restored_snapshot_digest.write().await = snapshot_digest(&snapshot);
    let Some(game_id) = snapshot.game_id.clone() else {
        return;
    };
    let running = snapshot.status == GameStatus::Running;
    if running {
        *state.active_workflow.lock().await = Some(game_id.clone());
    }
    publish(&state, snapshot).await;
    if running {
        tokio::spawn(observe_workflow(state, handle, game_id));
    }
}

async fn observe_workflow(
    state: AppState,
    handle: WorkflowHandle<Client, GameWorkflowRun>,
    game_id: String,
) {
    let mut consecutive_errors = 0_u8;
    loop {
        if state.active_workflow.lock().await.as_deref() != Some(&game_id) {
            return;
        }
        match query_recording_health(
            &handle,
            &state.temporal_query_succeeded,
            OnQueryFailure::Keep,
        )
        .await
        {
            Ok(snapshot) => {
                consecutive_errors = 0;
                let finished = snapshot.status == GameStatus::Finished;
                publish(&state, snapshot).await;
                if finished {
                    let mut active = state.active_workflow.lock().await;
                    if active.as_deref() == Some(&game_id) {
                        *active = None;
                    }
                    return;
                }
            }
            Err(error) => {
                consecutive_errors = consecutive_errors.saturating_add(1);
                if consecutive_errors == 1 || consecutive_errors.is_multiple_of(20) {
                    eprintln!("Workflow query failed repeatedly: {error}");
                }
            }
        }
        tokio::time::sleep(observer_delay(consecutive_errors)).await;
    }
}

fn observer_delay(consecutive_errors: u8) -> Duration {
    if consecutive_errors == 0 {
        return WORKFLOW_POLL_INTERVAL;
    }
    let exponent = u32::from(consecutive_errors.saturating_sub(1).min(4));
    WORKFLOW_POLL_INTERVAL
        .saturating_mul(2_u32.pow(exponent))
        .min(WORKFLOW_MAX_ERROR_BACKOFF)
}

fn snapshot_digest(snapshot: &GameSnapshot) -> String {
    let bytes = serde_json::to_vec(snapshot).expect("GameSnapshot always serializes");
    format!("{:x}", Sha256::digest(bytes))
}

async fn publish(state: &AppState, snapshot: GameSnapshot) {
    *state.snapshot.write().await = snapshot.clone();
    if let Ok(json) = serde_json::to_string(&snapshot) {
        let _ = state.events.send(json);
    }
}

async fn connect_cloud() -> Result<Client> {
    let profile = cloud::load_profile()?;
    cloud::connect(&profile).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `SnapshotQuery` that answers or refuses on command, so the health
    /// flag can be driven through every transition without a live `Client`.
    struct FakeQuery {
        answers: std::sync::Mutex<Vec<bool>>,
    }

    impl FakeQuery {
        fn new(answers: [bool; 3]) -> Self {
            let mut answers = answers.to_vec();
            answers.reverse();
            Self {
                answers: std::sync::Mutex::new(answers),
            }
        }
    }

    impl SnapshotQuery for FakeQuery {
        async fn snapshot(&self) -> Result<GameSnapshot> {
            if self.answers.lock().expect("fake lock").pop() == Some(true) {
                Ok(GameSnapshot::default())
            } else {
                Err(anyhow!("Temporal did not answer"))
            }
        }
    }

    #[tokio::test]
    async fn a_failed_query_at_startup_retracts_the_recovery_claim() {
        let health = AtomicBool::new(false);
        let query = FakeQuery::new([true, false, true]);

        // A first answer earns the claim.
        assert!(
            query_recording_health(&query, &health, OnQueryFailure::Retract)
                .await
                .is_ok()
        );
        assert!(health.load(Ordering::Acquire));

        // A failure on the startup path takes it back: this process did not
        // rebuild from Temporal, whatever its in-memory snapshot says.
        assert!(
            query_recording_health(&query, &health, OnQueryFailure::Retract)
                .await
                .is_err()
        );
        assert!(
            !health.load(Ordering::Acquire),
            "a failed resume must not keep claiming Temporal answered"
        );

        // And it can be earned back.
        assert!(
            query_recording_health(&query, &health, OnQueryFailure::Retract)
                .await
                .is_ok()
        );
        assert!(health.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn a_transient_poll_failure_does_not_retract_a_rebuild() {
        let health = AtomicBool::new(false);
        let query = FakeQuery::new([true, false, false]);

        assert!(
            query_recording_health(&query, &health, OnQueryFailure::Keep)
                .await
                .is_ok()
        );
        assert!(health.load(Ordering::Acquire));

        for _ in 0..2 {
            assert!(
                query_recording_health(&query, &health, OnQueryFailure::Keep)
                    .await
                    .is_err()
            );
            assert!(
                health.load(Ordering::Acquire),
                "the rebuild already happened; a dropped poll does not undo it"
            );
        }
    }

    #[tokio::test]
    async fn the_recovery_claim_starts_false() {
        // The bug this replaced was a literal `true` in the response body, so
        // the untouched default is the case worth pinning.
        let health = AtomicBool::new(false);
        assert!(!health.load(Ordering::Acquire));
        let query = FakeQuery::new([false, false, false]);
        assert!(
            query_recording_health(&query, &health, OnQueryFailure::Keep)
                .await
                .is_err()
        );
        assert!(
            !health.load(Ordering::Acquire),
            "never answered, never claims it did"
        );
    }

    #[test]
    fn observer_backoff_is_bounded() {
        assert_eq!(observer_delay(0), Duration::from_millis(250));
        assert_eq!(observer_delay(1), Duration::from_millis(250));
        assert_eq!(observer_delay(2), Duration::from_millis(500));
        assert_eq!(observer_delay(20), Duration::from_secs(4));
    }

    #[test]
    fn snapshot_digest_is_stable_and_state_sensitive() {
        let snapshot = GameSnapshot::default();
        assert_eq!(snapshot_digest(&snapshot), snapshot_digest(&snapshot));
        let mut changed = snapshot;
        changed.activity_attempts = 1;
        assert_ne!(
            snapshot_digest(&changed),
            snapshot_digest(&GameSnapshot::default())
        );
    }

    use temporal_trivia_shared::identity_from_mac;

    #[test]
    fn badge_worker_identity_accepts_named_and_legacy_badges_only() {
        // The prefixes now come from shared::identity alongside the firmware
        // code that produces them; this keeps the controller's own use of the
        // predicate covered from here.
        assert!(is_badge_worker_identity("badge/KEEN-RAVEN-C8"));
        assert!(is_badge_worker_identity(
            &identity_from_mac([0xe8, 0x3d, 0xc1, 0xf9, 0x4b, 0xc8]).id
        ));
        assert!(!is_badge_worker_identity("63305@Fatehowler.local"));
    }
}
