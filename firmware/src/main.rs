mod display;
mod haptics;
mod hil;
mod identity;
mod input;
mod model;
mod power;
mod session;

use std::{
    convert::TryInto,
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    hal::{
        ledc::{LedcDriver, LedcTimerDriver, config::TimerConfig},
        peripherals::Peripherals,
        units::Hertz,
    },
    io::vfs::MountedEventfs,
    nvs::EspDefaultNvsPartition,
    sntp::{EspSntp, SyncStatus},
    wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi},
};
use rustls::{RootCertStore, client::WebPkiServerVerifier};
use temporalio_client::{
    ActivityIdentifier, Client, ClientOptions, Connection, ConnectionOptions, RpcOptions,
    TlsOptions, WorkflowQueryOptions, WorkflowSignalOptions,
};
use temporalio_common::{protos::TaskToken, worker::WorkerDeploymentOptions};
use temporalio_macros::{activities, workflow, workflow_methods};
use temporalio_sdk::{
    Runtime, SyncWorkflowContext, Worker, WorkerOptions, WorkflowContext, WorkflowContextView,
    WorkflowResult,
    activities::{ActivityContext, ActivityError},
    runtime::RuntimeOptions,
};
use temporalio_sdk_core::{ActivitySlotKind, FixedSizeSlotSupplier, TunerBuilder, Url};

use badge_input::{ButtonState, Choice, PANIC_HOLD};
use badge_screen::Status;

use crate::{
    display::BadgeDisplay,
    haptics::{BadgeHaptics, HapticEvent, SharedHaptics},
    identity::{BadgeIdentity, factory_identity},
    input::BadgeInput,
    model::{
        BADGE_CRASH_BLACKOUT_MS, BADGE_HEARTBEAT_INTERVAL_MS, BADGE_TASK_QUEUE, BadgeAnswer,
        BadgeEvent, GameInput, GameSnapshot, QuestionTask,
    },
    session::SessionStore,
};

include!(concat!(env!("OUT_DIR"), "/firmware_config.rs"));
const HEARTBEAT_BLACKOUT: Duration = Duration::from_millis(BADGE_CRASH_BLACKOUT_MS);
const WORKER_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const MAX_ACTIVITY_RUNTIME: Duration = Duration::from_secs(120);
const POWERUP_OVERLAY: Duration = Duration::from_millis(1_500);
const POWERUP_FRESHNESS_MS: u64 = 5_000;
/// Long enough to read the verdict before the waiting screen returns.
const FEEDBACK_HOLD: Duration = Duration::from_millis(1_100);
const GAME_SIGNAL_TIMEOUT: Duration = Duration::from_millis(750);
const ACTIVE_WORKFLOW_ID: &str = "temporal-trivia-active";
/// How long a badge waits past the deadline for the Workflow to publish the
/// final standings before it gives up and shows RESULT PENDING.
const RESULT_WATCH_INTERVAL: Duration = Duration::from_secs(1);
const RESULT_WATCH_POLLS: u32 = 45;
/// Keep final standings readable, then make an idle recovery reserve visibly
/// ready for the next round instead of leaving stale results on screen.
const RESULT_HOLD: Duration = Duration::from_secs(5);

type SharedDisplay = Arc<Mutex<BadgeDisplay>>;
type SharedInput = Arc<Mutex<BadgeInput>>;
type SharedQuestion = Arc<Mutex<Option<QuestionTask>>>;

#[workflow]
#[derive(Default)]
struct GameWorkflow;

#[workflow_methods]
impl GameWorkflow {
    #[run]
    async fn run(
        _ctx: &mut WorkflowContext<Self>,
        _input: GameInput,
    ) -> WorkflowResult<GameSnapshot> {
        unreachable!("badge firmware never registers the Workflow implementation")
    }

    #[signal]
    fn badge_started(&mut self, _ctx: &mut SyncWorkflowContext<Self>, _event: BadgeEvent) {}

    #[signal]
    fn panic_event(&mut self, _ctx: &mut SyncWorkflowContext<Self>, _event: BadgeEvent) {}

    #[signal]
    fn recovered(&mut self, _ctx: &mut SyncWorkflowContext<Self>, _event: BadgeEvent) {}

    #[query]
    fn snapshot(&self, _ctx: &WorkflowContextView) -> GameSnapshot {
        GameSnapshot::default()
    }
}

type GameWorkflowRun = <GameWorkflow as temporalio_common::HasWorkflowDefinition>::Run;

struct BadgeActivities {
    display: SharedDisplay,
    haptics: SharedHaptics,
    input: SharedInput,
    identity: BadgeIdentity,
    session: Arc<SessionStore>,
    result_watcher: Mutex<Option<ResultWatcher>>,
    activity_active: Arc<AtomicBool>,
    powerup_active: Arc<AtomicBool>,
    current_question: SharedQuestion,
    /// Points a correct answer is currently worth, refreshed by the power-up
    /// poller. The badge shows feedback before the Workflow scores the answer,
    /// so without this it would always claim 1 even under double points.
    point_value: Arc<AtomicI32>,
}

struct ResultWatcher {
    game_id: String,
    task: tokio::task::JoinHandle<()>,
}

struct ActivityActiveGuard(Arc<AtomicBool>);

struct CurrentQuestionGuard(SharedQuestion);

struct PowerupActiveGuard(Arc<AtomicBool>);

impl ActivityActiveGuard {
    fn new(active: Arc<AtomicBool>) -> Self {
        active.store(true, Ordering::Release);
        Self(active)
    }
}

impl Drop for ActivityActiveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl CurrentQuestionGuard {
    fn new(current: SharedQuestion, task: QuestionTask) -> Result<Self, ActivityError> {
        *current
            .lock()
            .map_err(|_| anyhow!("question lock poisoned"))? = Some(task);
        Ok(Self(current))
    }
}

impl Drop for CurrentQuestionGuard {
    fn drop(&mut self) {
        if let Ok(mut current) = self.0.lock() {
            *current = None;
        }
    }
}

impl PowerupActiveGuard {
    fn new(active: Arc<AtomicBool>) -> Self {
        active.store(true, Ordering::Release);
        Self(active)
    }
}

impl Drop for PowerupActiveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[activities]
impl BadgeActivities {
    #[activity(name = "trivia.answer_question")]
    #[allow(dead_code)]
    async fn answer_question(
        self: Arc<Self>,
        ctx: ActivityContext,
        task: QuestionTask,
    ) -> Result<BadgeAnswer, ActivityError> {
        let _active = ActivityActiveGuard::new(Arc::clone(&self.activity_active));
        let _current = CurrentQuestionGuard::new(Arc::clone(&self.current_question), task.clone())?;
        log::info!(
            "Question {} attempt {} heartbeat timeout {:?}",
            task.question.id,
            ctx.info().attempt,
            ctx.info().heartbeat_timeout
        );
        // The Activity payload already contains everything needed to draw the
        // question. Do that before NVS or Cloud telemetry so a slow Signal can
        // never make a newly assigned badge look frozen.
        show_question(&self.display, &self.identity.callsign, &task)?;
        Self::record_activity_heartbeat(&ctx).await?;
        self.session
            .begin_game(&task.game_id, task.deadline_unix_ms)?;
        self.start_result_watcher(&ctx, &task)?;
        Self::record_activity_heartbeat(&ctx).await?;

        let event = BadgeEvent {
            badge_id: self.identity.id.clone(),
            callsign: self.identity.callsign.clone(),
            question_id: task.question.id.clone(),
            attempt: ctx.info().attempt,
        };
        if let Some(handle) = ctx.workflow_handle::<GameWorkflow>() {
            // This Signal is observational telemetry, not part of accepting an
            // answer. Keep it off the Activity's heartbeat-critical path: on
            // the ESP32 runtime an unhealthy Signal has outlived its local
            // timeout and allowed the server heartbeat timeout to expire.
            let start_event = event.clone();
            tokio::spawn(async move {
                match tokio::time::timeout(
                    GAME_SIGNAL_TIMEOUT,
                    handle.signal(
                        GameWorkflow::badge_started,
                        start_event,
                        WorkflowSignalOptions::default(),
                    ),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => log::warn!("could not signal badge start: {error}"),
                    Err(_) => {
                        log::warn!("badge start Signal exceeded 750 ms; continuing Activity")
                    }
                }
            });
        }
        Self::record_activity_heartbeat(&ctx).await?;

        // Report every real Temporal attempt before this Worker refuses a
        // question it already abandoned. That keeps the public attempt count
        // aligned with ActivityContext even when the same badge polls a retry.
        if self
            .session
            .is_abandoned(&task.game_id, &task.question.id)?
        {
            tokio::time::sleep(Duration::from_millis(250)).await;
            return Err(anyhow!("badge already abandoned this question").into());
        }
        log::info!("Question {} preparation complete", task.question.id);

        let activity_deadline_unix_ms = task.latest_possible_deadline_unix_ms();
        match self
            .wait_for_choice(&ctx, activity_deadline_unix_ms)
            .await?
        {
            Choice::Answer(selected_index) => {
                log::info!(
                    "Input selected answer={} question={}",
                    selected_index,
                    task.question.id
                );
                let correct = selected_index == task.question.correct_index;
                let points = self.point_value.load(Ordering::Acquire);
                show_feedback(
                    &self.display,
                    &self.identity.callsign,
                    correct,
                    if correct { points } else { -points },
                )?;
                haptics::play(
                    &self.haptics,
                    if correct {
                        HapticEvent::Correct
                    } else {
                        HapticEvent::Wrong
                    },
                )
                .await;
                tokio::time::sleep(FEEDBACK_HOLD).await;
                show_waiting(&self.display, &self.identity.callsign)?;
                let answer = BadgeAnswer {
                    badge_id: self.identity.id.clone(),
                    callsign: self.identity.callsign.clone(),
                    question_id: task.question.id.clone(),
                    selected_index,
                };
                // A wrong answer is a valid game result, not an infrastructure
                // failure. Complete the Activity normally so Temporal retries
                // only genuine Worker loss and heartbeat timeouts.
                Ok(answer)
            }
            Choice::Panic => {
                self.session.abandon(&task.game_id, &task.question.id)?;
                show_panic(&self.display, &self.identity.callsign)?;
                haptics::play(&self.haptics, HapticEvent::Crash).await;
                if let Some(handle) = ctx.workflow_handle::<GameWorkflow>() {
                    match tokio::time::timeout(
                        GAME_SIGNAL_TIMEOUT,
                        handle.signal(
                            GameWorkflow::panic_event,
                            event.clone(),
                            WorkflowSignalOptions::default(),
                        ),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => log::warn!("could not signal panic: {error}"),
                        Err(_) => {
                            log::warn!("panic Signal exceeded 750 ms; starting heartbeat blackout")
                        }
                    }
                }
                log::warn!(
                    "simulated crash: suppressing heartbeats for {} seconds",
                    HEARTBEAT_BLACKOUT.as_secs()
                );
                // Intentionally do not heartbeat or complete. Temporal's
                // heartbeat timeout retries this Activity on another Worker.
                tokio::time::sleep(HEARTBEAT_BLACKOUT).await;
                show_recovered(&self.display, &self.identity.callsign)?;
                haptics::play(&self.haptics, HapticEvent::Recovered).await;
                if let Some(handle) = ctx.workflow_handle::<GameWorkflow>() {
                    match tokio::time::timeout(
                        GAME_SIGNAL_TIMEOUT,
                        handle.signal(
                            GameWorkflow::recovered,
                            event,
                            WorkflowSignalOptions::default(),
                        ),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => log::warn!("could not signal recovery: {error}"),
                        Err(_) => log::warn!("recovery Signal exceeded 750 ms; retrying Activity"),
                    }
                }
                Err(anyhow!("simulated badge Worker crash after heartbeat timeout").into())
            }
        }
    }
}

impl BadgeActivities {
    async fn wait_for_choice(
        &self,
        ctx: &ActivityContext,
        deadline_unix_ms: u64,
    ) -> Result<Choice, ActivityError> {
        // Temporal cancellation and the Workflow deadline remain authoritative.
        // This monotonic ceiling prevents a stale build-time clock fallback from
        // leaving the physical badge stuck in an Activity indefinitely.
        let local_deadline = Instant::now() + MAX_ACTIVITY_RUNTIME;
        let mut state = if self.sample_buttons()?.any() {
            // A press already in progress belongs to the previous screen. Keep
            // suppressing it until release, but remain inside the heartbeat
            // loop so a held or electrically stuck button cannot make a
            // healthy Worker look dead to Temporal.
            ButtonState::SuppressedUntilRelease
        } else {
            ButtonState::default()
        };
        let heartbeat_interval = Duration::from_millis(BADGE_HEARTBEAT_INTERVAL_MS);
        let mut last_heartbeat = Instant::now()
            .checked_sub(heartbeat_interval)
            .unwrap_or_else(Instant::now);
        loop {
            if ctx.is_cancelled() {
                log::warn!(
                    "Activity attempt {} was cancelled by Temporal",
                    ctx.info().attempt
                );
                return Err(ActivityError::cancelled());
            }
            let now_unix_ms = unix_ms();
            if now_unix_ms >= deadline_unix_ms {
                log::warn!(
                    "Activity attempt {} reached game deadline: now={} deadline={}",
                    ctx.info().attempt,
                    now_unix_ms,
                    deadline_unix_ms
                );
                return Err(ActivityError::cancelled());
            }
            if Instant::now() >= local_deadline {
                log::warn!(
                    "Activity attempt {} reached local runtime ceiling",
                    ctx.info().attempt
                );
                return Err(ActivityError::cancelled());
            }
            if last_heartbeat.elapsed() >= heartbeat_interval {
                Self::record_activity_heartbeat(ctx).await?;
                last_heartbeat = Instant::now();
            }
            let (next, choice) = state.advance(
                self.sample_buttons()?,
                self.powerup_active.load(Ordering::Acquire),
                Instant::now(),
                PANIC_HOLD,
            );
            state = next;
            if let Some(choice) = choice {
                return Ok(choice);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn sample_buttons(&self) -> Result<input::Buttons, ActivityError> {
        Ok(self
            .input
            .lock()
            .map_err(|_| anyhow!("input lock poisoned"))?
            .sample())
    }

    async fn record_activity_heartbeat(ctx: &ActivityContext) -> Result<(), ActivityError> {
        // Keep Core's local Activity heartbeat state current. On ESP32, the
        // queued Worker path alone has not reliably reached Temporal before
        // the server timeout, even with a one-second throttle.
        ctx.record_heartbeat(())
            .await
            .map_err(|error| anyhow!("could not encode Activity heartbeat: {error}"))?;

        // Await a direct server acknowledgement as well. This prevents a lost
        // Core-to-server heartbeat path from looking healthy until Temporal
        // reassigns the question to another badge.
        let handle = ctx
            .client()
            .get_async_activity_handle(ActivityIdentifier::from_task_token(TaskToken(
                ctx.info().task_token.clone(),
            )));
        let response = match handle.heartbeat(Some(()), RpcOptions::default()).await {
            Ok(response) => response,
            Err(error) => {
                log::error!("Activity heartbeat RPC failed: {error}");
                return Err(anyhow!("Activity heartbeat RPC failed: {error}").into());
            }
        };
        if response.cancel_requested || response.activity_paused || response.activity_reset {
            log::warn!(
                "Activity heartbeat response stopped attempt {}: cancel={} paused={} reset={}",
                ctx.info().attempt,
                response.cancel_requested,
                response.activity_paused,
                response.activity_reset
            );
            return Err(ActivityError::cancelled());
        }
        Ok(())
    }

    fn start_result_watcher(
        &self,
        ctx: &ActivityContext,
        task: &QuestionTask,
    ) -> Result<(), ActivityError> {
        let Some(handle) = ctx.workflow_handle::<GameWorkflow>() else {
            return Ok(());
        };
        let mut watcher = self
            .result_watcher
            .lock()
            .map_err(|_| anyhow!("watcher lock poisoned"))?;
        if let Some(current) = watcher.as_ref()
            && current.game_id == task.game_id
            && !current.task.is_finished()
        {
            return Ok(());
        }
        if let Some(previous) = watcher.take() {
            previous.task.abort();
        }
        let display = Arc::clone(&self.display);
        let haptics = Arc::clone(&self.haptics);
        let identity = self.identity.clone();
        let deadline_unix_ms = task.deadline_unix_ms;
        let game_id = task.game_id.clone();
        let task = tokio::spawn(async move {
            let wait_ms = deadline_unix_ms.saturating_sub(unix_ms());
            tokio::time::sleep(Duration::from_millis(wait_ms)).await;
            for _ in 0..RESULT_WATCH_POLLS {
                match handle
                    .query(GameWorkflow::snapshot, (), WorkflowQueryOptions::default())
                    .await
                {
                    Ok(snapshot) if snapshot.status == model::GameStatus::Finished => {
                        let won = snapshot.winners.contains(&identity.callsign);
                        if let Ok(mut screen) = display.lock()
                            && let Err(error) =
                                screen.show_results(&identity.callsign, &identity.id, &snapshot)
                        {
                            log::error!("show final results: {error:#}");
                        }
                        haptics::play(
                            &haptics,
                            if won {
                                HapticEvent::Winner
                            } else {
                                HapticEvent::RoundOver
                            },
                        )
                        .await;
                        tokio::time::sleep(RESULT_HOLD).await;
                        match show_waiting(&display, &identity.callsign) {
                            Ok(()) => log::info!(
                                "Result hold complete; {} returned to waiting",
                                identity.callsign
                            ),
                            Err(error) => log::error!("restore waiting screen: {error:#}"),
                        }
                        return;
                    }
                    Ok(_) => {}
                    Err(error) => log::warn!("result query pending: {error}"),
                }
                tokio::time::sleep(RESULT_WATCH_INTERVAL).await;
            }
            if let Ok(mut screen) = display.lock() {
                let _ = screen.show_status(&identity.callsign, Status::ResultPending);
            }
        });
        *watcher = Some(ResultWatcher { game_id, task });
        Ok(())
    }
}

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    validate_config()?;
    let identity = factory_identity()?;
    log::info!("Temporal Trivia badge booting as {}", identity.callsign);

    let peripherals = Peripherals::take().context("take ESP32 peripherals")?;
    let display = Arc::new(Mutex::new(BadgeDisplay::new(
        peripherals.i2c0,
        peripherals.pins.gpio4,
        peripherals.pins.gpio5,
    )?));
    let input = Arc::new(Mutex::new(BadgeInput::new(
        peripherals.pins.gpio7,
        peripherals.pins.gpio18,
        peripherals.pins.gpio17,
        peripherals.pins.gpio0,
    )?));
    let haptic_timer = LedcTimerDriver::new(
        peripherals.ledc.timer0,
        &TimerConfig::new().frequency(Hertz(80)),
    )
    .context("configure haptic PWM timer")?;
    let haptics = Arc::new(tokio::sync::Mutex::new(BadgeHaptics::new(
        LedcDriver::new(
            peripherals.ledc.channel0,
            haptic_timer,
            peripherals.pins.gpio6,
        )
        .context("configure GPIO6 haptic PWM")?,
    )?));
    show_status(&display, &identity.callsign, Status::Booting)?;

    let sys_loop = EspSystemEventLoop::take().context("take system event loop")?;
    let nvs_partition = EspDefaultNvsPartition::take().context("take default NVS")?;
    let session = Arc::new(SessionStore::new(nvs_partition.clone())?);
    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs_partition))?,
        sys_loop,
    )?;
    show_status(&display, &identity.callsign, Status::ConnectingWifi)?;
    connect_wifi(&mut wifi)?;
    show_status(&display, &identity.callsign, Status::SyncingTime)?;
    let (_sntp, used_network_time) = sync_clock()?;
    if !used_network_time {
        log::warn!("using firmware build timestamp for TLS validation");
    }

    let _eventfs = MountedEventfs::mount(5).context("mount eventfd VFS for Tokio")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build single-thread Tokio runtime")?;
    runtime.block_on(Box::pin(run_worker(
        display, input, haptics, identity, session,
    )))
}

async fn run_worker(
    display: SharedDisplay,
    input: SharedInput,
    haptics: SharedHaptics,
    identity: BadgeIdentity,
    session: Arc<SessionStore>,
) -> Result<()> {
    let runtime_options = RuntimeOptions::builder()
        .heartbeat_interval(Some(WORKER_HEARTBEAT_INTERVAL))
        .build()
        .map_err(|error| anyhow!(error))?;
    let sdk_runtime = Runtime::new_assume_tokio(runtime_options)?;
    let target = if TEMPORAL_ADDRESS.contains("://") {
        TEMPORAL_ADDRESS.to_owned()
    } else {
        format!("https://{TEMPORAL_ADDRESS}")
    };
    show_status(&display, &identity.callsign, Status::ConnectingCloud)?;
    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let verifier = WebPkiServerVerifier::builder(Arc::new(roots))
        .build()
        .map_err(|error| anyhow!("build WebPKI verifier: {error}"))?;
    let options = ConnectionOptions::new(Url::from_str(&target)?)
        .api_key(TEMPORAL_API_KEY)
        .tls_options(TlsOptions::builder().server_cert_verifier(verifier).build())
        .build();
    let connection = Connection::connect(options).await?;
    let client = Client::new(connection, ClientOptions::new(TEMPORAL_NAMESPACE).build())?;
    // A physical badge has one screen and one set of buttons, so it must never
    // execute two question Activities concurrently.
    let mut tuner = TunerBuilder::default();
    tuner.activity_slot_supplier(Arc::new(FixedSizeSlotSupplier::<ActivitySlotKind>::new(1)));
    let activity_active = Arc::new(AtomicBool::new(false));
    let powerup_active = Arc::new(AtomicBool::new(false));
    let point_value = Arc::new(AtomicI32::new(1));
    let current_question = Arc::new(Mutex::new(None));
    let worker_identity = format!("badge/{}", identity.callsign);
    let worker_options = WorkerOptions::new(BADGE_TASK_QUEUE)
        .client_identity_override(worker_identity.clone())
        .max_heartbeat_throttle_interval(Duration::from_millis(BADGE_HEARTBEAT_INTERVAL_MS))
        .tuner(Arc::new(tuner.build()))
        .deployment_options(WorkerDeploymentOptions::from_build_id(
            "temporal-trivia-badge-0.1.0".to_owned(),
        ))
        .register_activities(BadgeActivities {
            display: Arc::clone(&display),
            haptics: Arc::clone(&haptics),
            input: Arc::clone(&input),
            identity: identity.clone(),
            session,
            result_watcher: Mutex::new(None),
            activity_active: Arc::clone(&activity_active),
            powerup_active: Arc::clone(&powerup_active),
            point_value: Arc::clone(&point_value),
            current_question: Arc::clone(&current_question),
        })
        .build();
    let powerup_client = client.clone();
    let mut worker = Worker::new(&sdk_runtime, client, worker_options)
        .map_err(|error| anyhow!(error.to_string()))?;
    show_waiting(&display, &identity.callsign)?;
    hil::start(
        Arc::clone(&input),
        Arc::clone(&activity_active),
        Arc::clone(&current_question),
        identity.callsign.clone(),
    )?;
    let sleep_display = Arc::clone(&display);
    let sleep_input = Arc::clone(&input);
    let sleep_haptics = Arc::clone(&haptics);
    let sleep_callsign = identity.callsign.clone();
    let powerup_display = Arc::clone(&display);
    let powerup_haptics = Arc::clone(&haptics);
    let powerup_callsign = identity.callsign.clone();
    let powerup_flag = Arc::clone(&powerup_active);
    tokio::spawn(async move {
        monitor_powerups(
            powerup_client,
            powerup_display,
            powerup_haptics,
            powerup_callsign,
            current_question,
            powerup_flag,
            point_value,
        )
        .await;
    });
    tokio::spawn(async move {
        if let Err(error) = power::monitor(
            sleep_display,
            sleep_input,
            sleep_haptics,
            activity_active,
            powerup_active,
            sleep_callsign,
        )
        .await
        {
            log::error!("sleep monitor stopped: {error:#}");
        }
    });
    log::info!("Polling trivia queue {BADGE_TASK_QUEUE} as {worker_identity}");
    worker.run().await?;
    Ok(())
}

async fn monitor_powerups(
    client: Client,
    display: SharedDisplay,
    haptics: SharedHaptics,
    callsign: String,
    current_question: SharedQuestion,
    powerup_active: Arc<AtomicBool>,
    point_value: Arc<AtomicI32>,
) {
    let handle = client.get_workflow_handle::<GameWorkflowRun>(ACTIVE_WORKFLOW_ID);
    let mut game_id = None;
    let mut sequence = 0;
    let mut consecutive_errors = 0_u32;
    loop {
        match handle
            .query(GameWorkflow::snapshot, (), WorkflowQueryOptions::default())
            .await
        {
            Err(error) => {
                consecutive_errors = consecutive_errors.saturating_add(1);
                // Every 120th is once a minute at the 500 ms poll interval.
                // Silence here is what made a badge that had lost Temporal
                // look identical to one with no power-ups to show.
                if consecutive_errors == 1 || consecutive_errors.is_multiple_of(120) {
                    log::warn!("power-up poll has failed {consecutive_errors} time(s): {error}");
                }
            }
            Ok(snapshot) => {
                consecutive_errors = 0;
                let doubled = snapshot
                    .chaos
                    .double_points_until_unix_ms
                    .is_some_and(|until| until > unix_ms());
                point_value.store(if doubled { 2 } else { 1 }, Ordering::Release);
                if snapshot.game_id != game_id {
                    game_id.clone_from(&snapshot.game_id);
                    sequence = 0;
                }
                if let Some(notice) = snapshot.chaos.latest_powerup
                    && notice.sequence > sequence
                {
                    sequence = notice.sequence;
                    if snapshot.status == model::GameStatus::Running
                        && unix_ms().saturating_sub(notice.issued_unix_ms) <= POWERUP_FRESHNESS_MS
                    {
                        let _overlay = PowerupActiveGuard::new(Arc::clone(&powerup_active));
                        if let Ok(mut screen) = display.lock() {
                            match screen.show_powerup(&callsign, notice.command) {
                                Ok(()) => log::info!(
                                    "Displayed Temporal power-up {:?} sequence {}",
                                    notice.command,
                                    notice.sequence
                                ),
                                Err(error) => log::error!("show power-up: {error:#}"),
                            }
                        }
                        haptics::play(&haptics, HapticEvent::Powerup).await;
                        tokio::time::sleep(POWERUP_OVERLAY).await;
                        let question = current_question.lock().ok().and_then(|task| task.clone());
                        if let Ok(mut screen) = display.lock() {
                            let result = if let Some(task) = question {
                                screen.show_question(&callsign, &task.question)
                            } else {
                                screen.show_waiting(&callsign)
                            };
                            if let Err(error) = result {
                                log::error!("restore screen after power-up: {error:#}");
                            }
                        }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Runs one drawing call against the shared display.
///
/// Six wrappers below each repeated the same lock-and-translate preamble; the
/// only thing that varied was which `BadgeDisplay` method ran. They keep their
/// own names because the call sites read better as `show_panic(...)` than as a
/// closure, but the poisoned-lock handling now exists once.
fn with_display<T>(
    display: &SharedDisplay,
    draw: impl FnOnce(&mut BadgeDisplay) -> Result<T>,
) -> Result<T> {
    let mut screen = display
        .lock()
        .map_err(|_| anyhow!("display lock poisoned"))?;
    draw(&mut screen)
}

fn show_status(display: &SharedDisplay, title: &str, status: Status) -> Result<()> {
    with_display(display, |screen| screen.show_status(title, status))
}

fn show_question(display: &SharedDisplay, callsign: &str, task: &QuestionTask) -> Result<()> {
    with_display(display, |screen| {
        screen.show_question(callsign, &task.question)
    })
}

fn show_feedback(
    display: &SharedDisplay,
    callsign: &str,
    correct: bool,
    score_delta: i32,
) -> Result<()> {
    with_display(display, |screen| {
        screen.show_feedback(callsign, correct, score_delta)
    })
}

fn show_panic(display: &SharedDisplay, callsign: &str) -> Result<()> {
    with_display(display, |screen| screen.show_panic(callsign))
}

fn show_recovered(display: &SharedDisplay, callsign: &str) -> Result<()> {
    with_display(display, |screen| screen.show_recovered(callsign))
}

fn show_waiting(display: &SharedDisplay, callsign: &str) -> Result<()> {
    with_display(display, |screen| screen.show_waiting(callsign))
}

fn validate_config() -> Result<()> {
    for (name, value) in [
        ("BADGE_WIFI_SSID", WIFI_SSID),
        ("TEMPORAL_ADDRESS", TEMPORAL_ADDRESS),
        ("TEMPORAL_NAMESPACE", TEMPORAL_NAMESPACE),
        ("TEMPORAL_API_KEY", TEMPORAL_API_KEY),
    ] {
        if value.is_empty() {
            bail!("missing build-time setting {name}");
        }
    }
    Ok(())
}

fn connect_wifi(wifi: &mut BlockingWifi<EspWifi<'static>>) -> Result<()> {
    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: WIFI_SSID
            .try_into()
            .map_err(|_| anyhow!("Wi-Fi SSID is too long"))?,
        password: WIFI_PASS
            .try_into()
            .map_err(|_| anyhow!("Wi-Fi password is too long"))?,
        auth_method: if WIFI_PASS.is_empty() {
            AuthMethod::None
        } else {
            AuthMethod::WPA2Personal
        },
        ..Default::default()
    }))?;
    wifi.start().context("start Wi-Fi")?;
    wifi.connect().context("join Wi-Fi")?;
    wifi.wait_netif_up().context("wait for DHCP")?;
    Ok(())
}

fn sync_clock() -> Result<(EspSntp<'static>, bool)> {
    let sntp = EspSntp::new_default().context("start SNTP")?;
    for _ in 0..200 {
        if sntp.get_sync_status() == SyncStatus::Completed {
            return Ok((sntp, true));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let build_epoch = BUILD_UNIX_EPOCH
        .parse::<i64>()
        .context("parse firmware build timestamp")?;
    if build_epoch < 1_700_000_000 {
        bail!("SNTP timed out and firmware build timestamp is invalid");
    }
    let timestamp = esp_idf_svc::sys::timeval {
        tv_sec: build_epoch,
        tv_usec: 0,
    };
    // SAFETY: `timestamp` is a valid timeval for the duration of the call and
    // the timezone pointer is null, as required when no timezone is supplied.
    let result = unsafe { esp_idf_svc::sys::settimeofday(&timestamp, std::ptr::null()) };
    if result != 0 {
        bail!("SNTP timed out and settimeofday failed with code {result}");
    }
    Ok((sntp, false))
}
