mod display;
mod haptics;
#[cfg(feature = "hil")]
mod hil;
mod identity;
mod model;
mod power;
mod session;
mod ui;

use std::{
    convert::TryInto,
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering},
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
    TlsOptions, WorkflowQueryOptions, WorkflowSignalOptions, errors::AsyncActivityError,
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

use badge_input::Choice;
use badge_screen::Status;

use crate::{
    display::BadgeDisplay,
    haptics::BadgeHaptics,
    identity::{BadgeIdentity, factory_identity},
    model::{
        BADGE_CRASH_BLACKOUT_MS, BADGE_HEARTBEAT_INTERVAL_MS, BADGE_TASK_QUEUE, BadgeAnswer,
        BadgeEvent, GameInput, GameSnapshot, QuestionTask, heartbeat_budget_exhausted,
    },
    session::SessionStore,
    ui::{Ui, UiRequest},
};

include!(concat!(env!("OUT_DIR"), "/firmware_config.rs"));
const HEARTBEAT_BLACKOUT: Duration = Duration::from_millis(BADGE_CRASH_BLACKOUT_MS);
const WORKER_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const MAX_ACTIVITY_RUNTIME: Duration = Duration::from_secs(120);
const POWERUP_FRESHNESS_MS: u64 = 5_000;
/// How often a badge asks the Workflow whether a power-up has landed.
///
/// This runs on the same single-threaded runtime as the Activity poller, over
/// TLS, forever. At 500 ms it was two Cloud queries a second competing with
/// the poll that actually delivers questions, and the first question of a
/// round took as long as sixteen seconds to reach a badge. One second still
/// beats the 1.5 s overlay it drives, and between rounds there is no power-up
/// to miss -- only a question to be ready for.
const POWERUP_POLL_ACTIVE: Duration = Duration::from_millis(1_000);
const POWERUP_POLL_IDLE: Duration = Duration::from_millis(4_000);
/// Long enough to read the verdict before the waiting screen returns.
const FEEDBACK_HOLD: Duration = Duration::from_millis(1_100);
const GAME_SIGNAL_TIMEOUT: Duration = Duration::from_millis(750);
const ACTIVE_WORKFLOW_ID: &str = "temporal-trivia-active";
/// How long a badge waits past the deadline for the Workflow to publish the
/// final standings before it gives up and shows RESULT PENDING.
const RESULT_WATCH_INTERVAL: Duration = Duration::from_secs(1);
const RESULT_WATCH_POLLS: u32 = 45;
/// Keep final standings readable, then make an idle badge visibly ready for
/// the next round instead of leaving stale results on screen.
const RESULT_HOLD: Duration = Duration::from_secs(5);
/// Pause before a reboot, so the log drains and a busy access point gets a
/// moment before the next association attempt.
const RESTART_BACKOFF: Duration = Duration::from_secs(2);
/// How long a badge waits before failing an Activity for a question it has
/// already abandoned, so the retry has a moment to reach a different Worker.
const ABANDONED_REFUSAL_BACKOFF: Duration = Duration::from_millis(250);
/// How often to report an input path that has produced nothing.
///
/// A badge that silently ignores every press looks identical to one that is
/// waiting for one. `SuppressedUntilRelease` and a stuck `powerup_active` both
/// do exactly that, for the whole life of a question, and neither said a word.
const INPUT_DIAGNOSTIC_INTERVAL: Duration = Duration::from_millis(2_000);

#[cfg(feature = "hil")]
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
    ui: Ui,
    identity: BadgeIdentity,
    session: Arc<SessionStore>,
    result_watcher: Mutex<Option<ResultWatcher>>,
    /// Only the USB acceptance harness reads these. The UI thread already
    /// knows whether a question owns the controls, so a shipped badge has no
    /// reason to maintain a second copy of the answer.
    #[cfg(feature = "hil")]
    activity_active: Arc<AtomicBool>,
    #[cfg(feature = "hil")]
    current_question: SharedQuestion,
    /// Points a correct answer is currently worth, refreshed by the power-up
    /// poller. The badge shows feedback before the Workflow scores the answer,
    /// so without this it would always claim 1 even under double points.
    point_value: Arc<AtomicI32>,
}

/// Heartbeats an Activity from its own task, so the input loop never waits on
/// the network.
///
/// The loop used to `await` a gRPC round trip inline every second. `sample`
/// reads the pins at an instant, so while that await was outstanding the badge
/// was not merely slow to notice a press -- it could not see one at all. On a
/// congested network that dropped physical presses outright and left the
/// answer haptic trailing the button by however long Temporal took to answer.
struct ActivityHeartbeat {
    /// Temporal asked this attempt to stop.
    stopped: Arc<AtomicBool>,
    /// Milliseconds since `base` at the last acknowledged heartbeat.
    ///
    /// 32 bits because the Xtensa target has no 64-bit atomics, and an
    /// Activity is capped at 95 seconds by schedule-to-close anyway.
    last_ack_ms: Arc<AtomicU32>,
    base: Instant,
    task: tokio::task::JoinHandle<()>,
}

impl ActivityHeartbeat {
    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    /// How long this attempt has gone without an acknowledged heartbeat.
    fn silent_ms(&self) -> u64 {
        let now = self.base.elapsed().as_millis() as u32;
        u64::from(now.saturating_sub(self.last_ack_ms.load(Ordering::Acquire)))
    }
}

impl Drop for ActivityHeartbeat {
    /// Dropping stops the heartbeats. The deliberate crash relies on this:
    /// it drops the monitor and then sleeps out the blackout in silence.
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// What one Activity heartbeat means for the attempt that sent it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Heartbeat {
    /// Temporal acknowledged it; the attempt is alive.
    Acknowledged,
    /// The RPC did not land. Harmless until the budget runs out.
    Missed,
    /// Temporal told this attempt to stop.
    Stopped,
}

struct ResultWatcher {
    game_id: String,
    task: tokio::task::JoinHandle<()>,
}

#[cfg(feature = "hil")]
struct ActivityActiveGuard(Arc<AtomicBool>);

#[cfg(feature = "hil")]
struct CurrentQuestionGuard(SharedQuestion);

#[cfg(feature = "hil")]
impl ActivityActiveGuard {
    fn new(active: Arc<AtomicBool>) -> Self {
        active.store(true, Ordering::Release);
        Self(active)
    }
}

#[cfg(feature = "hil")]
impl Drop for ActivityActiveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(feature = "hil")]
impl CurrentQuestionGuard {
    fn new(current: SharedQuestion, task: QuestionTask) -> Result<Self, ActivityError> {
        *current
            .lock()
            .map_err(|_| anyhow!("question lock poisoned"))? = Some(task);
        Ok(Self(current))
    }
}

#[cfg(feature = "hil")]
impl Drop for CurrentQuestionGuard {
    fn drop(&mut self) {
        if let Ok(mut current) = self.0.lock() {
            *current = None;
        }
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
        log::info!(
            "Question {} attempt {} heartbeat timeout {:?}",
            task.question.id,
            ctx.info().attempt,
            ctx.info().heartbeat_timeout
        );
        let event = BadgeEvent {
            badge_id: self.identity.id.clone(),
            callsign: self.identity.callsign.clone(),
            question_id: task.question.id.clone(),
            attempt: ctx.info().attempt,
        };

        // Refuse an abandoned question before anything is drawn, written or
        // awaited. A badge leaving its crash blackout is the freest poller on
        // the queue and so the likeliest to be handed its own abandoned
        // question back; drawing it would flash a question the player can
        // never answer, and the NVS write and heartbeat RPCs behind it were
        // three Cloud round trips spent to say no. The Signal still reports
        // the attempt, but it is spawned and costs this path nothing.
        // Built, not started: an async fn does nothing until it is awaited, so
        // this decides *when* the attempt is reported without deciding whether.
        let report_attempt = Self::badge_started_signal(&ctx, event.clone());

        if self
            .session
            .is_abandoned(&task.game_id, &task.question.id)?
        {
            // The constraint from docs/planned-changes.md: every real Temporal
            // attempt is reported before the abandon path returns, so the
            // public attempt count stays aligned with ActivityContext.
            report_attempt.await;
            log::warn!(
                "Refusing abandoned question {}; leaving it for another Worker",
                task.question.id
            );
            tokio::time::sleep(ABANDONED_REFUSAL_BACKOFF).await;
            return Err(anyhow!("badge already abandoned this question").into());
        }

        #[cfg(feature = "hil")]
        let _active = ActivityActiveGuard::new(Arc::clone(&self.activity_active));
        #[cfg(feature = "hil")]
        let _current = CurrentQuestionGuard::new(Arc::clone(&self.current_question), task.clone())?;
        // Retire a watcher still holding the previous round's standings before
        // anything is drawn. It restores the waiting screen when its hold ends,
        // and the heartbeat and NVS writes below are long enough for that to
        // land on top of a question this Activity had already painted.
        self.start_result_watcher(&ctx, &task)?;
        // The Activity payload already contains everything needed to draw the
        // question. Do that before NVS or Cloud telemetry so a slow Signal can
        // never make a newly assigned badge look frozen.
        self.ui.show(UiRequest::Question(Box::new(task.clone())));
        // From here until this Activity lets go, heartbeats are somebody
        // else's job. Nothing below may block on the network.
        let heartbeat = Self::spawn_heartbeat(&ctx);
        self.session
            .begin_game(&task.game_id, task.deadline_unix_ms)?;
        log::info!(
            "Question {} preparation complete (heap={} low={})",
            task.question.id,
            free_heap(),
            lowest_heap()
        );

        let activity_deadline_unix_ms = task.latest_possible_deadline_unix_ms();
        // Report the attempt alongside the wait rather than before it. A
        // detached task would be simpler, but an orphan can deliver
        // `badge_started` after a retry has begun on another badge, and the
        // Workflow would then attribute the next handoff to the wrong badge.
        // `join` keeps the Signal inside this Activity's own lifetime, and the
        // wait is always the longer of the two, so it costs nothing.
        let (choice, ()) = futures::future::join(
            self.wait_for_choice(&ctx, &heartbeat, activity_deadline_unix_ms),
            report_attempt,
        )
        .await;
        match choice? {
            Choice::Answer(selected_index) => {
                log::info!(
                    "Input selected answer={} question={}",
                    selected_index,
                    task.question.id
                );
                let correct = selected_index == task.question.correct_index;
                let points = self.point_value.load(Ordering::Acquire);
                self.ui.show(UiRequest::Feedback {
                    correct,
                    score_delta: if correct { points } else { -points },
                });
                tokio::time::sleep(FEEDBACK_HOLD).await;
                self.ui.show(UiRequest::Waiting);
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
                // Stop heartbeating before anything else: this is the whole
                // point of the gesture, and the blackout below has to outlast
                // Temporal's timeout in silence.
                drop(heartbeat);
                self.session.abandon(&task.game_id, &task.question.id)?;
                self.ui.show(UiRequest::Crashed);
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
                self.ui.show(UiRequest::Recovered);
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
        heartbeat: &ActivityHeartbeat,
        deadline_unix_ms: u64,
    ) -> Result<Choice, ActivityError> {
        // Temporal cancellation and the Workflow deadline remain authoritative.
        // This monotonic ceiling prevents a stale build-time clock fallback from
        // leaving the physical badge stuck in an Activity indefinitely.
        let local_deadline = Instant::now() + MAX_ACTIVITY_RUNTIME;
        // Anything the sampler recognised before this question opened was
        // aimed at the previous screen.
        let opened_at = Instant::now();
        let mut last_diagnostic = Instant::now();
        let opening_ticks = self.ui.ticks();
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
            if heartbeat.stopped() {
                return Err(ActivityError::cancelled());
            }
            let silent_ms = heartbeat.silent_ms();
            if heartbeat_budget_exhausted(silent_ms) {
                log::error!(
                    "no Activity heartbeat acknowledged for {silent_ms} ms; giving up attempt {}",
                    ctx.info().attempt
                );
                return Err(
                    anyhow!("no Activity heartbeat acknowledged for {silent_ms} ms").into(),
                );
            }
            if let Some(choice) = self.ui.next_choice() {
                return Ok(choice);
            }
            // The sampler's tick count is the honest health signal now: this
            // loop being slow only delays an answer, but the sampler stalling
            // would lose one.
            if last_diagnostic.elapsed() >= INPUT_DIAGNOSTIC_INTERVAL {
                last_diagnostic = Instant::now();
                log::warn!(
                    "Input idle {} ms into attempt {}: sampler_ticks={} silent_hb={} ms heap={} low={}",
                    opened_at.elapsed().as_millis(),
                    ctx.info().attempt,
                    self.ui.ticks().saturating_sub(opening_ticks),
                    heartbeat.silent_ms(),
                    free_heap(),
                    lowest_heap()
                );
                log::warn!("main task stack headroom: {} bytes", ui::stack_headroom());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Sends one heartbeat and reports what it means, without deciding
    /// anything. Only the caller knows how long this attempt has already gone
    /// unacknowledged, and that is what a missed RPC has to be judged against.
    async fn heartbeat_once(ctx: &ActivityContext) -> Heartbeat {
        // Keep Core's local Activity heartbeat state current. On ESP32, the
        // queued Worker path alone has not reliably reached Temporal before
        // the server timeout, even with a one-second throttle.
        if let Err(error) = ctx.record_heartbeat(()).await {
            log::warn!("could not encode Activity heartbeat: {error}");
            return Heartbeat::Missed;
        }

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
            // Definitive, not transient: the Activity has completed, been
            // cancelled, or its Workflow has closed. Spending the whole
            // tolerance budget on a question that no longer exists just leaves
            // it on screen for another ten seconds.
            Err(AsyncActivityError::NotFound(status)) => {
                log::info!(
                    "Activity attempt {} is no longer known to Temporal: {status}",
                    ctx.info().attempt
                );
                return Heartbeat::Stopped;
            }
            Err(error) => {
                log::warn!("Activity heartbeat RPC failed: {error}");
                return Heartbeat::Missed;
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
            return Heartbeat::Stopped;
        }
        Heartbeat::Acknowledged
    }

    /// Starts heartbeating this attempt on its own task.
    ///
    /// The returned handle stops the heartbeats when it is dropped, which is
    /// what makes the deliberate crash gesture silent.
    fn spawn_heartbeat(ctx: &ActivityContext) -> ActivityHeartbeat {
        let stopped = Arc::new(AtomicBool::new(false));
        let last_ack_ms = Arc::new(AtomicU32::new(0));
        let base = Instant::now();
        let task = tokio::spawn({
            let ctx = ctx.clone();
            let stopped = Arc::clone(&stopped);
            let last_ack_ms = Arc::clone(&last_ack_ms);
            async move {
                loop {
                    match Self::heartbeat_once(&ctx).await {
                        Heartbeat::Acknowledged => {
                            last_ack_ms.store(base.elapsed().as_millis() as u32, Ordering::Release)
                        }
                        // Judging a miss needs to know how long this attempt
                        // has been silent overall, which is the input loop's
                        // business. This task only reports.
                        Heartbeat::Missed => {
                            log::warn!("Activity heartbeat missed for {}", ctx.info().attempt);
                        }
                        Heartbeat::Stopped => {
                            stopped.store(true, Ordering::Release);
                            return;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(BADGE_HEARTBEAT_INTERVAL_MS)).await;
                }
            }
        });
        ActivityHeartbeat {
            stopped,
            last_ack_ms,
            base,
            task,
        }
    }

    /// Reports a real Temporal attempt to the Workflow.
    ///
    /// Observational telemetry, not part of accepting an answer, so it is
    /// bounded and its failures are logged rather than returned: on the ESP32
    /// runtime an unhealthy Signal has outlived its local timeout and let the
    /// server heartbeat timeout expire behind it.
    async fn badge_started_signal(ctx: &ActivityContext, event: BadgeEvent) {
        let Some(handle) = ctx.workflow_handle::<GameWorkflow>() else {
            return;
        };
        match tokio::time::timeout(
            GAME_SIGNAL_TIMEOUT,
            handle.signal(
                GameWorkflow::badge_started,
                event,
                WorkflowSignalOptions::default(),
            ),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => log::warn!("could not signal badge start: {error}"),
            Err(_) => log::warn!("badge start Signal exceeded 750 ms; continuing Activity"),
        }
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
        let ui = self.ui.clone();
        let identity = self.identity.clone();
        let deadline_unix_ms = task.deadline_unix_ms;
        let game_id = task.game_id.clone();
        let watched_game_id = game_id.clone();
        let task = tokio::spawn(async move {
            let wait_ms = deadline_unix_ms.saturating_sub(unix_ms());
            log::info!("Result watcher armed for {watched_game_id}; {wait_ms} ms to the deadline");
            tokio::time::sleep(Duration::from_millis(wait_ms)).await;
            for _ in 0..RESULT_WATCH_POLLS {
                match handle
                    .query(GameWorkflow::snapshot, (), WorkflowQueryOptions::default())
                    .await
                {
                    Ok(snapshot) if snapshot.status == model::GameStatus::Finished => {
                        log::info!(
                            "Round {watched_game_id} finished; showing standings for {}",
                            identity.callsign
                        );
                        ui.show(UiRequest::Results(Box::new(snapshot)));
                        tokio::time::sleep(RESULT_HOLD).await;
                        // The UI thread refuses both of these while a question
                        // owns the screen, so a badge already back in play
                        // keeps its question and this simply does nothing.
                        if ui.answering() {
                            log::info!(
                                "Result hold complete; {} is already back on a question",
                                identity.callsign
                            );
                        } else {
                            ui.show(UiRequest::Waiting);
                            log::info!(
                                "Result hold complete; {} returned to waiting",
                                identity.callsign
                            );
                        }
                        return;
                    }
                    Ok(_) => {}
                    Err(error) => log::warn!("result query pending: {error}"),
                }
                tokio::time::sleep(RESULT_WATCH_INTERVAL).await;
            }
            log::warn!(
                "No final standings for {} after {RESULT_WATCH_POLLS} polls; showing RESULT PENDING",
                identity.callsign
            );
            ui.show(UiRequest::ResultPending);
        });
        *watcher = Some(ResultWatcher { game_id, task });
        Ok(())
    }
}

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    install_panic_reporter();
    // A build with no credentials in it will never work, so say so once and
    // stop rather than rebooting forever over it.
    validate_config()?;

    // Everything after this can fail transiently: an access point that is busy,
    // a Cloud connection reset, an SNTP server that does not answer. A badge
    // that exits on any of them sits powered, lit and useless until somebody
    // unplugs it -- which is precisely how this firmware spent an evening
    // looking frozen, once as a stopped Worker and once as a failed Wi-Fi join.
    // The next boot is a fresh attempt at all of it, and a badge that keeps
    // retrying is the only behaviour a booth can use.
    if let Err(error) = boot() {
        log::error!("badge boot failed: {error:#}");
    }
    restart()
}

/// Brings the badge up and runs it. Only returns if something went wrong.
fn boot() -> Result<()> {
    let identity = factory_identity()?;
    log::info!("Temporal Trivia badge booting as {}", identity.callsign);
    // Internal DRAM is the scarce resource on this chip and the one TLS needs.
    // Print it before anything has been allocated, so a build that starves the
    // handshake is obvious from the first lines of a boot log.
    log::info!("free internal DRAM at boot: {} bytes", free_heap());

    let peripherals = Peripherals::take().context("take ESP32 peripherals")?;
    let display = BadgeDisplay::new(
        peripherals.i2c0,
        peripherals.pins.gpio4,
        peripherals.pins.gpio5,
    )?;
    let haptic_timer = LedcTimerDriver::new(
        peripherals.ledc.timer0,
        &TimerConfig::new().frequency(Hertz(80)),
    )
    .context("configure haptic PWM timer")?;
    let haptics = BadgeHaptics::new(
        LedcDriver::new(
            peripherals.ledc.channel0,
            haptic_timer,
            peripherals.pins.gpio6,
        )
        .context("configure GPIO6 haptic PWM")?,
    )?;
    // The UI thread takes the screen, the buttons and the motor, and it is the
    // only thing that ever touches them. It starts before Temporal does so the
    // badge can say what it is doing while it connects.
    let ui = ui::start(
        display,
        haptics,
        peripherals.pins.gpio7,
        peripherals.pins.gpio18,
        peripherals.pins.gpio17,
        peripherals.pins.gpio0,
        identity.callsign.clone(),
        identity.id.clone(),
    )?;
    ui.show(UiRequest::Status(Status::Booting));

    let sys_loop = EspSystemEventLoop::take().context("take system event loop")?;
    let nvs_partition = EspDefaultNvsPartition::take().context("take default NVS")?;
    let session = Arc::new(SessionStore::new(nvs_partition.clone())?);
    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs_partition))?,
        sys_loop,
    )?;
    ui.show(UiRequest::Status(Status::ConnectingWifi));
    connect_wifi(&mut wifi)?;
    ui.show(UiRequest::Status(Status::SyncingTime));
    let (_sntp, used_network_time) = sync_clock()?;
    if !used_network_time {
        log::warn!("using firmware build timestamp for TLS validation");
    }

    let _eventfs = MountedEventfs::mount(5).context("mount eventfd VFS for Tokio")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        // A current-thread runtime still carries a blocking pool, and its
        // default ceiling is 512 threads. Every reconnect resolves DNS through
        // spawn_blocking, so a badge on a flaky link accumulates threads --
        // and a FreeRTOS task stack has to come from internal DRAM, of which
        // this chip has about 512 KiB no matter how much PSRAM is fitted.
        // Exhausting it makes newlib's lock_init_generic fail to allocate a
        // semaphore and abort().
        //
        // Bounded, not starved. Two was tried and is too few: DNS and the
        // SDK's own blocking work could not both get a thread, so the Cloud
        // connection never finished establishing and the badge never reached
        // `Polling trivia queue` at all. Four costs at most 32 KiB and leaves
        // room for both.
        .max_blocking_threads(4)
        // Keep those two rather than paying to recreate them every reconnect.
        .thread_keep_alive(Duration::from_secs(600))
        .build()
        .context("build single-thread Tokio runtime")?;
    // Anything that reaches here is a badge that has stopped working, so a
    // setup failure reboots for the same reason the Worker's own exit does.
    match runtime.block_on(Box::pin(run_worker(ui, identity, session))) {
        Ok(()) => log::error!("badge Worker returned unexpectedly"),
        Err(error) => log::error!("badge Worker could not start: {error:#}"),
    }
    restart()
}

/// Runs the badge's Temporal Worker. Only ever leaves by rebooting: the `?`
/// paths below are setup failures, and everything after them ends in
/// [`restart`].
async fn run_worker(ui: Ui, identity: BadgeIdentity, session: Arc<SessionStore>) -> Result<()> {
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
    ui.show(UiRequest::Status(Status::ConnectingCloud));
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
    let point_value = Arc::new(AtomicI32::new(1));
    #[cfg(feature = "hil")]
    let activity_active = Arc::new(AtomicBool::new(false));
    #[cfg(feature = "hil")]
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
            ui: ui.clone(),
            identity: identity.clone(),
            session,
            result_watcher: Mutex::new(None),
            #[cfg(feature = "hil")]
            activity_active: Arc::clone(&activity_active),
            point_value: Arc::clone(&point_value),
            #[cfg(feature = "hil")]
            current_question: Arc::clone(&current_question),
        })
        .build();
    // T14: the acceptance runner needs to know this Worker is polling. It
    // used to wait for the boot log line below, which is printed once and is
    // therefore already gone when a port is opened without resetting the badge.
    let worker_polling = Arc::new(AtomicBool::new(false));
    let powerup_client = client.clone();
    let mut worker = Worker::new(&sdk_runtime, client, worker_options)
        .map_err(|error| anyhow!(error.to_string()))?;
    ui.show(UiRequest::Waiting);
    #[cfg(feature = "hil")]
    hil::start(
        ui.clone(),
        Arc::clone(&activity_active),
        Arc::clone(&current_question),
        Arc::clone(&worker_polling),
        identity.callsign.clone(),
    )?;
    let powerup_ui = ui.clone();
    tokio::spawn(async move {
        monitor_powerups(powerup_client, powerup_ui, point_value).await;
    });
    log::info!("Polling trivia queue {BADGE_TASK_QUEUE} as {worker_identity}");
    worker_polling.store(true, Ordering::Release);

    // `worker.run()` returning at all means this badge has stopped polling.
    // Letting that unwind out of `main` runs every destructor and leaves the
    // board powered, lit and useless until somebody unplugs it -- which reads
    // to a player exactly like a badge frozen on the waiting screen. A soak
    // caught both badges doing this within a second of each other after ten
    // minutes, silently. There is nothing a stopped Worker can do but come
    // back, so say why and reboot into a fresh connection.
    match worker.run().await {
        Ok(()) => log::error!("Temporal Worker stopped on its own; restarting the badge"),
        Err(error) => log::error!("Temporal Worker failed: {error}; restarting the badge"),
    }
    worker_polling.store(false, Ordering::Release);
    restart()
}

/// Reboots the badge. Never returns.
fn restart() -> ! {
    // Long enough for the log to drain before the reset takes the UART, and
    // long enough not to hammer an access point that is already refusing us:
    // a tight reboot loop over a failed Wi-Fi join makes the next attempt
    // less likely to succeed, not more.
    std::thread::sleep(RESTART_BACKOFF);
    // SAFETY: an unconditional ESP-IDF reset with no arguments and no state.
    unsafe { esp_idf_svc::sys::esp_restart() }
}

async fn monitor_powerups(client: Client, ui: Ui, point_value: Arc<AtomicI32>) {
    let handle = client.get_workflow_handle::<GameWorkflowRun>(ACTIVE_WORKFLOW_ID);
    let mut game_id = None;
    let mut sequence = 0;
    let mut consecutive_errors = 0_u32;
    let mut poll_interval;
    loop {
        match handle
            .query(GameWorkflow::snapshot, (), WorkflowQueryOptions::default())
            .await
        {
            Err(error) => {
                consecutive_errors = consecutive_errors.saturating_add(1);
                // Roughly once a minute at either poll interval. Silence here
                // is what made a badge that had lost Temporal look identical
                // to one with no power-ups to show.
                if consecutive_errors == 1 || consecutive_errors.is_multiple_of(60) {
                    log::warn!("power-up poll has failed {consecutive_errors} time(s): {error}");
                }
                poll_interval = POWERUP_POLL_IDLE;
            }
            Ok(snapshot) => {
                consecutive_errors = 0;
                poll_interval = if snapshot.status == model::GameStatus::Running {
                    POWERUP_POLL_ACTIVE
                } else {
                    POWERUP_POLL_IDLE
                };
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
                        // The UI thread owns the overlay's lifetime and knows
                        // what to put back underneath it, so this just asks.
                        log::info!(
                            "Temporal power-up {:?} sequence {}",
                            notice.command,
                            notice.sequence
                        );
                        ui.show(UiRequest::Powerup(notice.command));
                    }
                }
            }
        }
        tokio::time::sleep(poll_interval).await;
    }
}

/// Free *internal* heap, in bytes.
///
/// The distinction is the whole point. `esp_get_free_heap_size` counts PSRAM
/// too, and reported 8.2 MB free at the moment of a crash caused by internal
/// DRAM exhaustion. FreeRTOS objects and task stacks can only come from
/// internal memory, so that is the number worth watching.
///
/// Every Activity spawns a heartbeat task and a Signal task, and a busy round
/// runs forty of them. A badge that panics only under high answer throughput
/// looks like exhaustion, so the number is worth carrying in the one line the
/// input loop already prints.
fn free_heap() -> u32 {
    // SAFETY: a plain size query against the allocator, no state involved.
    unsafe {
        esp_idf_svc::sys::heap_caps_get_free_size(esp_idf_svc::sys::MALLOC_CAP_INTERNAL) as u32
    }
}

/// Smallest free internal heap seen since boot, where a slow leak shows up.
fn lowest_heap() -> u32 {
    // SAFETY: as above.
    unsafe {
        esp_idf_svc::sys::heap_caps_get_minimum_free_size(esp_idf_svc::sys::MALLOC_CAP_INTERNAL)
            as u32
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Reports a Rust panic before the process aborts.
///
/// Every badge fault so far has been a `BREAK` followed by a double exception
/// in the handler itself, which loses the message and the backtrace with it.
/// A panic hook runs first, while the stack is still whatever the panicking
/// code left, and `log::error!` reaches the UART without waiting for a
/// handler that may not survive. If a fault prints nothing from here, it was
/// never a Rust panic and the search moves to ESP-IDF.
fn install_panic_reporter() {
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map_or_else(|| "unknown".to_owned(), ToString::to_string);
        log::error!(
            "RUST PANIC at {location}: {}",
            info.payload_as_str().unwrap_or("<no message>")
        );
        log::error!(
            "panicking task stack headroom: {} bytes",
            ui::stack_headroom()
        );
        ui::log_every_task_stack();
    }));
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
