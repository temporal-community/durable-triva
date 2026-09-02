//! The badge's screen, buttons and motor, owned by one thread.
//!
//! These three used to be `Arc<Mutex<_>>`s shared between the Activity, the
//! sleep monitor, the power-up poller and the result watcher, all running on
//! the Tokio runtime the Temporal Worker shares. That cost us twice.
//!
//! It cost responsiveness: the Worker's TLS work is CPU-bound and does not
//! yield, so while it retried a connection the input loop fell from 28 Hz to
//! 6 Hz -- measured on hardware -- and presses shorter than the gap between
//! samples were not delayed but lost.
//!
//! It cost correctness: four writers to one screen, with no answer to "who
//! owns it right now", meant a stale restore could erase a live question and
//! nothing would redraw it.
//!
//! Nothing in here touches the network. Nothing outside it touches the
//! hardware. The Temporal side asks for screens by sending [`UiRequest`] and
//! reads answers back with [`Ui::next_choice`].

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{Receiver, Sender, TryRecvError, channel},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use badge_input::{ButtonState, Buttons, Choice, PANIC_HOLD};
use badge_screen::Status;
use esp_idf_svc::hal::gpio::{Gpio0, Gpio7, Gpio17, Gpio18, Input, PinDriver, Pull};

use crate::{
    display::BadgeDisplay,
    haptics::{BadgeHaptics, HapticEvent, HapticStep, pattern},
    model::{ChaosCommand, GameSnapshot, QuestionTask},
    power,
};

/// Fast enough that the shortest deliberate tap spans several samples.
///
/// This must stay at or above one scheduler tick. ESP-IDF rounds a shorter
/// sleep down to `vTaskDelay(0)`, and because pthreads outrank the main task
/// this loop would then busy-spin and starve the rest of the firmware -- the
/// badge hung before Wi-Fi. `CONFIG_FREERTOS_HZ=1000` makes a tick 1 ms.
const TICK: Duration = Duration::from_millis(5);
/// Choices are drained within a poll or two; this only bounds a pathological
/// backlog, and dropping the oldest keeps the most recent intent.
const MAX_QUEUED_CHOICES: usize = 8;
const POWERUP_OVERLAY: Duration = Duration::from_millis(1_500);
const SLEEP_ARM_DELAY: Duration = Duration::from_millis(250);
const SLEEP_HOLD: Duration = Duration::from_secs(3);
/// How often to report this thread's worst-case stack headroom.
///
/// Every panic so far has been a double exception, which on Xtensa means a
/// fault inside the exception handler and almost always a stack that ran out.
/// The backtrace is destroyed by definition in that case, so the honest way
/// to find it is to watch the headroom rather than the wreckage.
const STACK_REPORT_INTERVAL: Duration = Duration::from_secs(15);

/// Bytes of stack this task has never used, at its worst moment so far.
pub fn stack_headroom() -> u32 {
    // SAFETY: a read of the current task's own FreeRTOS bookkeeping, with the
    // null handle meaning "me", which is always valid from a running task.
    unsafe { esp_idf_svc::sys::uxTaskGetStackHighWaterMark(std::ptr::null_mut()) }
}

/// Reports the worst-case stack headroom of every task in the system.
///
/// The three this firmware creates are not the only ones that can overflow,
/// and the canary fires on whichever does, so a full sweep is the only way to
/// know. Everything here is deliberately kept off the caller's stack: the
/// first version put a 32-entry status array and a Vec of formatted names on
/// it, cost 7 KiB, and left the UI thread it was measuring with 876 bytes --
/// a measurement that endangers its subject measures nothing.
pub fn log_every_task_stack() {
    const MAX_TASKS: usize = 24;
    // SAFETY: FreeRTOS fills at most `MAX_TASKS` entries of a heap buffer we
    // own and sized, and returns how many it wrote. A null run-time pointer
    // means "no run-time stats", which this build does not enable.
    let mut statuses: Box<[esp_idf_svc::sys::TaskStatus_t; MAX_TASKS]> =
        Box::new(unsafe { std::mem::zeroed() });
    let count = unsafe {
        esp_idf_svc::sys::uxTaskGetSystemState(
            statuses.as_mut_ptr(),
            MAX_TASKS as u32,
            std::ptr::null_mut(),
        ) as usize
    }
    .min(MAX_TASKS);
    for status in &statuses[..count] {
        // SAFETY: FreeRTOS always points pcTaskName at a NUL-terminated name
        // that outlives this read.
        let name = unsafe { std::ffi::CStr::from_ptr(status.pcTaskName) };
        log::warn!(
            "task stack headroom: {:>6} bytes  {}",
            status.usStackHighWaterMark,
            name.to_string_lossy()
        );
    }
}

/// A screen the Temporal side wants shown.
///
/// Sequencing stays with the caller: it already knows whether a wrong answer
/// should linger before the waiting screen returns, and encoding that here
/// would split one decision across two threads.
pub enum UiRequest {
    Status(Status),
    Waiting,
    Question(Box<QuestionTask>),
    Feedback {
        correct: bool,
        score_delta: i32,
    },
    Crashed,
    Recovered,
    Results(Box<GameSnapshot>),
    ResultPending,
    Powerup(ChaosCommand),
    #[cfg(feature = "hil")]
    InjectAnswer(u8),
    #[cfg(feature = "hil")]
    InjectCrash,
}

#[derive(Default)]
struct Shared {
    choices: Mutex<VecDeque<Choice>>,
    /// Loop turns, so a caller can prove this thread is still running.
    ticks: AtomicU32,
    /// Whether a question currently owns the controls.
    answering: AtomicBool,
}

/// A handle to the UI thread. Cloneable; every clone drives the same thread.
#[derive(Clone)]
pub struct Ui {
    requests: Sender<UiRequest>,
    shared: Arc<Shared>,
}

impl Ui {
    /// Asks for a screen. Never blocks, and never fails the caller: a UI that
    /// has stopped is worth a log line, not a failed Activity.
    pub fn show(&self, request: UiRequest) {
        if self.requests.send(request).is_err() {
            log::error!("UI thread is gone; screen request dropped");
        }
    }

    /// Takes the oldest gesture recognised since the last call.
    pub fn next_choice(&self) -> Option<Choice> {
        self.shared.choices.lock().ok()?.pop_front()
    }

    /// UI loop turns since boot. Compare two readings to prove it is alive.
    pub fn ticks(&self) -> u32 {
        self.shared.ticks.load(Ordering::Acquire)
    }

    /// Whether a question currently owns the controls.
    pub fn answering(&self) -> bool {
        self.shared.answering.load(Ordering::Acquire)
    }
}

struct Pins {
    up: PinDriver<'static, Input>,
    right: PinDriver<'static, Input>,
    down: PinDriver<'static, Input>,
    // GPIO0 is fixed by the badge PCB. Holding LEFT while resetting can select
    // the ESP ROM bootloader, so release it before power-up or reset.
    left: PinDriver<'static, Input>,
}

impl Pins {
    fn sample(&self) -> Buttons {
        Buttons {
            up: self.up.is_low(),
            right: self.right.is_low(),
            down: self.down.is_low(),
            left: self.left.is_low(),
        }
    }
}

/// What the badge is currently showing.
enum Screen {
    /// A status or result screen. The sleep gesture is armed here and only
    /// here, so a player mid-question can never put the badge to sleep.
    Idle,
    Question(Box<QuestionTask>),
}

/// Plays one haptic pattern a step at a time, without blocking the loop.
#[derive(Default)]
struct Motor {
    steps: &'static [HapticStep],
    index: usize,
    step_until: Option<Instant>,
}

impl Motor {
    fn play(&mut self, event: HapticEvent, haptics: &mut BadgeHaptics, now: Instant) {
        self.steps = pattern(event);
        self.index = 0;
        self.start_step(haptics, now);
    }

    fn start_step(&mut self, haptics: &mut BadgeHaptics, now: Instant) {
        match self.steps.get(self.index) {
            Some(step) => {
                if let Err(error) = haptics.set(step.strength) {
                    log::error!("haptic step: {error:#}");
                }
                self.step_until = Some(now + step.duration);
            }
            None => {
                if let Err(error) = haptics.off() {
                    log::error!("haptic stop: {error:#}");
                }
                self.step_until = None;
            }
        }
    }

    fn advance(&mut self, haptics: &mut BadgeHaptics, now: Instant) {
        if self.step_until.is_some_and(|until| now >= until) {
            self.index += 1;
            self.start_step(haptics, now);
        }
    }

    fn stop(&mut self, haptics: &mut BadgeHaptics) {
        self.steps = &[];
        self.step_until = None;
        let _ = haptics.off();
    }
}

/// Starts the UI thread and returns a handle to it.
#[allow(clippy::too_many_arguments)]
pub fn start(
    display: BadgeDisplay,
    haptics: BadgeHaptics,
    up: Gpio7<'static>,
    right: Gpio18<'static>,
    down: Gpio17<'static>,
    left: Gpio0<'static>,
    callsign: String,
    badge_id: String,
) -> Result<Ui> {
    let pins = Pins {
        up: PinDriver::input(up, Pull::Up)?,
        right: PinDriver::input(right, Pull::Up)?,
        down: PinDriver::input(down, Pull::Up)?,
        left: PinDriver::input(left, Pull::Up)?,
    };
    let (requests, inbox) = channel();
    let shared = Arc::new(Shared::default());
    let ui = Ui {
        requests,
        shared: Arc::clone(&shared),
    };
    thread::Builder::new()
        .name("badge-ui".to_owned())
        // Sized from this thread's own high-water mark, which has never gone
        // below 11.7 KiB free of 16: it uses about 4 KiB. Every task stack is
        // internal DRAM, the scarcest memory on this chip, so 8 KiB keeps a
        // wide margin for a panic unwinding through here and gives 8 back.
        .stack_size(8 * 1024)
        .spawn(move || {
            let mut state = UiThread {
                display,
                haptics,
                pins,
                shared,
                callsign,
                badge_id,
                screen: Screen::Idle,
                buttons: ButtonState::default(),
                motor: Motor::default(),
                overlay_until: None,
                injected: VecDeque::new(),
                down_since: None,
                countdown_shown: None,
                last_stack_report: Instant::now(),
            };
            state.run(&inbox);
        })
        .context("start badge UI thread")?;
    Ok(ui)
}

struct UiThread {
    display: BadgeDisplay,
    haptics: BadgeHaptics,
    pins: Pins,
    shared: Arc<Shared>,
    callsign: String,
    badge_id: String,
    screen: Screen,
    buttons: ButtonState,
    motor: Motor,
    overlay_until: Option<Instant>,
    injected: VecDeque<Buttons>,
    down_since: Option<Instant>,
    countdown_shown: Option<u64>,
    last_stack_report: Instant,
}

impl UiThread {
    fn run(&mut self, inbox: &Receiver<UiRequest>) {
        loop {
            let now = Instant::now();
            loop {
                match inbox.try_recv() {
                    Ok(request) => self.apply(request, now),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        log::error!("UI request channel closed; stopping");
                        return;
                    }
                }
            }
            self.motor.advance(&mut self.haptics, now);
            self.expire_overlay(now);
            let buttons = self.sample();
            self.advance_buttons(buttons, now);
            self.advance_sleep(buttons, now);
            if self.last_stack_report.elapsed() >= STACK_REPORT_INTERVAL {
                self.last_stack_report = now;
                log_every_task_stack();
            }
            self.shared.ticks.fetch_add(1, Ordering::Release);
            thread::sleep(TICK);
        }
    }

    fn sample(&mut self) -> Buttons {
        self.injected
            .pop_front()
            .unwrap_or_else(|| self.pins.sample())
    }

    fn advance_buttons(&mut self, buttons: Buttons, now: Instant) {
        if !matches!(self.screen, Screen::Question(_)) {
            self.buttons = ButtonState::default();
            return;
        }
        let (next, choice) =
            self.buttons
                .advance(buttons, self.overlay_until.is_some(), now, PANIC_HOLD);
        self.buttons = next;
        if let Some(choice) = choice
            && let Ok(mut choices) = self.shared.choices.lock()
        {
            if choices.len() >= MAX_QUEUED_CHOICES {
                choices.pop_front();
            }
            choices.push_back(choice);
        }
    }

    /// The sleep gesture, armed only on an idle screen.
    fn advance_sleep(&mut self, buttons: Buttons, now: Instant) {
        if !matches!(self.screen, Screen::Idle) || self.overlay_until.is_some() {
            self.down_since = None;
            self.countdown_shown = None;
            return;
        }
        if !buttons.down {
            if self.down_since.take().is_some() && self.countdown_shown.take().is_some() {
                self.draw(|screen, callsign, _| screen.show_waiting(callsign));
            }
            return;
        }
        let elapsed = self.down_since.get_or_insert(now).elapsed();
        if elapsed >= SLEEP_HOLD {
            self.sleep_now();
            return;
        }
        if elapsed >= SLEEP_ARM_DELAY {
            let remaining = SLEEP_HOLD.saturating_sub(elapsed).as_millis() as u64;
            let seconds = remaining.div_ceil(1000);
            if self.countdown_shown != Some(seconds) {
                self.countdown_shown = Some(seconds);
                self.draw(|screen, callsign, _| screen.show_sleep_countdown(callsign, seconds));
                self.motor
                    .play(HapticEvent::SleepCountdown, &mut self.haptics, now);
            }
        }
    }

    fn sleep_now(&mut self) {
        log::info!("DOWN held for 3 seconds; entering deep sleep");
        self.draw(|screen, callsign, _| screen.show_sleep_countdown(callsign, 0));
        self.motor.play(
            HapticEvent::SleepCountdown,
            &mut self.haptics,
            Instant::now(),
        );
        self.draw(|screen, callsign, _| screen.show_sleeping(callsign));
        // Let go before the badge does, or it wakes on the button still down.
        while self.pins.sample().any() {
            thread::sleep(TICK);
        }
        self.motor.stop(&mut self.haptics);
        self.draw(|screen, _, _| screen.power_off());
        if let Err(error) = power::enter_deep_sleep() {
            log::error!("deep sleep failed: {error:#}");
        }
    }

    fn expire_overlay(&mut self, now: Instant) {
        if self.overlay_until.is_some_and(|until| now >= until) {
            self.overlay_until = None;
            self.redraw_current();
        }
    }

    fn redraw_current(&mut self) {
        match &self.screen {
            Screen::Question(task) => {
                let question = task.question.clone();
                self.draw(move |screen, callsign, _| screen.show_question(callsign, &question));
            }
            Screen::Idle => self.draw(|screen, callsign, _| screen.show_waiting(callsign)),
        }
    }

    fn apply(&mut self, request: UiRequest, now: Instant) {
        match request {
            UiRequest::Status(status) => {
                self.screen = Screen::Idle;
                self.draw(move |screen, callsign, _| screen.show_status(callsign, status));
            }
            UiRequest::Waiting => {
                self.screen = Screen::Idle;
                self.draw(|screen, callsign, _| screen.show_waiting(callsign));
            }
            UiRequest::Question(task) => {
                // Whatever the buttons were doing belonged to the last screen.
                if let Ok(mut choices) = self.shared.choices.lock() {
                    choices.clear();
                }
                self.buttons = if self.pins.sample().any() {
                    ButtonState::SuppressedUntilRelease
                } else {
                    ButtonState::default()
                };
                let question = task.question.clone();
                self.screen = Screen::Question(task);
                self.draw(move |screen, callsign, _| screen.show_question(callsign, &question));
            }
            UiRequest::Feedback {
                correct,
                score_delta,
            } => {
                self.screen = Screen::Idle;
                self.draw(move |screen, callsign, _| {
                    screen.show_feedback(callsign, correct, score_delta)
                });
                self.motor.play(
                    if correct {
                        HapticEvent::Correct
                    } else {
                        HapticEvent::Wrong
                    },
                    &mut self.haptics,
                    now,
                );
            }
            UiRequest::Crashed => {
                self.screen = Screen::Idle;
                self.draw(|screen, callsign, _| screen.show_panic(callsign));
                self.motor.play(HapticEvent::Crash, &mut self.haptics, now);
            }
            UiRequest::Recovered => {
                self.screen = Screen::Idle;
                self.draw(|screen, callsign, _| screen.show_recovered(callsign));
                self.motor
                    .play(HapticEvent::Recovered, &mut self.haptics, now);
            }
            UiRequest::Results(snapshot) => {
                // A question that arrived while the round was closing outranks
                // the standings: the player can still answer it.
                if matches!(self.screen, Screen::Question(_)) {
                    log::info!("{} holds a question; standings not shown", self.callsign);
                    return;
                }
                let won = snapshot.winners.contains(&self.callsign);
                self.draw(move |screen, callsign, badge_id| {
                    screen.show_results(callsign, badge_id, &snapshot)
                });
                self.motor.play(
                    if won {
                        HapticEvent::Winner
                    } else {
                        HapticEvent::RoundOver
                    },
                    &mut self.haptics,
                    now,
                );
            }
            UiRequest::ResultPending => {
                if matches!(self.screen, Screen::Question(_)) {
                    return;
                }
                self.draw(|screen, callsign, _| {
                    screen.show_status(callsign, Status::ResultPending)
                });
            }
            UiRequest::Powerup(command) => {
                self.overlay_until = Some(now + POWERUP_OVERLAY);
                self.draw(move |screen, callsign, _| screen.show_powerup(callsign, command));
                self.motor
                    .play(HapticEvent::Powerup, &mut self.haptics, now);
            }
            #[cfg(feature = "hil")]
            UiRequest::InjectAnswer(index) => {
                if let Some(gesture) = badge_input::answer_gesture(index) {
                    self.injected.extend(gesture);
                }
            }
            #[cfg(feature = "hil")]
            UiRequest::InjectCrash => {
                let frames = (PANIC_HOLD.as_millis() / TICK.as_millis()) as usize * 3 / 2;
                self.injected.extend(badge_input::crash_gesture(frames));
            }
        }
        self.shared.answering.store(
            matches!(self.screen, Screen::Question(_)),
            Ordering::Release,
        );
    }

    fn draw(&mut self, paint: impl FnOnce(&mut BadgeDisplay, &str, &str) -> Result<()>) {
        if let Err(error) = paint(&mut self.display, &self.callsign, &self.badge_id) {
            log::error!("draw: {error:#}");
        }
    }
}
