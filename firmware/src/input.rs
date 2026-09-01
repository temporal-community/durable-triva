//! The four face buttons, sampled on a thread of their own.
//!
//! This used to be sampled from inside the Activity, on the Tokio runtime the
//! Temporal Worker shares. The Worker's TLS work is CPU-bound and does not
//! yield, so while it retried a connection the sampling loop fell from 50 Hz
//! to 6 Hz -- measured, on `KEEN-RAVEN-C8`, at `ticks=24` per 4 seconds
//! against a healthy badge's 115. A press shorter than the gap between two
//! samples was not delayed, it was gone, and the badge read as frozen.
//!
//! A FreeRTOS task is scheduled preemptively, so this keeps its cadence
//! whatever the runtime is doing, and the gestures it recognises are queued
//! rather than sampled. The Activity can be starved and still lose nothing.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
pub use badge_input::Buttons;
use badge_input::{ButtonState, Choice, PANIC_HOLD};
use esp_idf_svc::hal::gpio::{Gpio0, Gpio7, Gpio17, Gpio18, Input, PinDriver, Pull};

/// Fast enough that the shortest deliberate tap spans several samples.
///
/// This must stay at or above one scheduler tick. ESP-IDF rounds a shorter
/// sleep down to `vTaskDelay(0)`, and because pthreads outrank the main task
/// this loop would then busy-spin and starve the rest of the firmware -- the
/// badge hung before Wi-Fi. `CONFIG_FREERTOS_HZ=1000` makes a tick 1 ms.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(5);
/// Choices are drained within a poll or two; this only bounds a pathological
/// backlog, and dropping the oldest keeps the most recent intent.
const MAX_QUEUED_CHOICES: usize = 8;

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

/// State the sampler publishes for the rest of the firmware to read.
#[derive(Default)]
struct Shared {
    choices: Mutex<VecDeque<Choice>>,
    /// Gestures queued by the USB HIL reader, played back ahead of the pins.
    injected: Mutex<VecDeque<Buttons>>,
    /// How long DOWN has been held, in milliseconds; zero when it is up.
    down_held_ms: AtomicU32,
    /// Whether every button is currently released.
    all_released: AtomicBool,
    /// Sampler turns, so a caller can prove this thread is still running.
    ticks: AtomicU32,
}

/// Reads the buttons. Cloneable; every clone sees the same sampler.
#[derive(Clone)]
pub struct ButtonReader {
    shared: Arc<Shared>,
}

impl ButtonReader {
    /// Starts the sampler and returns a handle to it.
    ///
    /// `armed` gates gesture recognition: choices are only produced while an
    /// Activity owns the answer controls. `powerup_active` suppresses input
    /// under an overlay, exactly as it did when this ran inline.
    pub fn start(
        up: Gpio7<'static>,
        right: Gpio18<'static>,
        down: Gpio17<'static>,
        left: Gpio0<'static>,
        armed: Arc<AtomicBool>,
        powerup_active: Arc<AtomicBool>,
    ) -> Result<Self> {
        let pins = Pins {
            up: PinDriver::input(up, Pull::Up)?,
            right: PinDriver::input(right, Pull::Up)?,
            down: PinDriver::input(down, Pull::Up)?,
            left: PinDriver::input(left, Pull::Up)?,
        };
        let shared = Arc::new(Shared {
            all_released: AtomicBool::new(true),
            ..Default::default()
        });
        let reader = Self {
            shared: Arc::clone(&shared),
        };
        thread::Builder::new()
            .name("buttons".to_owned())
            // The loop is allocation-free, but a panic unwinding through it
            // is not, and 4 KiB left no room to report one.
            .stack_size(8 * 1024)
            .spawn(move || sample_forever(&pins, &shared, &armed, &powerup_active))
            .context("start button sampler")?;
        Ok(reader)
    }

    /// Takes the oldest gesture the sampler has recognised, if any.
    pub fn next_choice(&self) -> Option<Choice> {
        self.shared.choices.lock().ok()?.pop_front()
    }

    /// Forgets any gesture recognised before now.
    ///
    /// Called when a question opens so an answer aimed at the previous screen
    /// cannot be delivered to this one.
    pub fn discard_pending(&self) {
        if let Ok(mut choices) = self.shared.choices.lock() {
            choices.clear();
        }
    }

    /// How long DOWN has been held, for the sleep gesture.
    pub fn down_held(&self) -> Duration {
        Duration::from_millis(u64::from(self.shared.down_held_ms.load(Ordering::Acquire)))
    }

    pub fn all_released(&self) -> bool {
        self.shared.all_released.load(Ordering::Acquire)
    }

    /// Sampler turns since boot. Compare two readings to prove it is alive.
    pub fn ticks(&self) -> u32 {
        self.shared.ticks.load(Ordering::Acquire)
    }

    /// Queues a synthetic gesture for the USB HIL acceptance harness.
    #[cfg(feature = "hil")]
    pub fn inject_answer(&self, index: u8) -> bool {
        let Some(gesture) = badge_input::answer_gesture(index) else {
            return false;
        };
        let Ok(mut injected) = self.shared.injected.lock() else {
            return false;
        };
        if !injected.is_empty() {
            return false;
        }
        injected.extend(gesture);
        true
    }
}

fn sample_forever(
    pins: &Pins,
    shared: &Arc<Shared>,
    armed: &Arc<AtomicBool>,
    powerup_active: &Arc<AtomicBool>,
) {
    let mut state = ButtonState::default();
    let mut was_armed = false;
    let mut down_since: Option<Instant> = None;

    loop {
        let buttons = shared
            .injected
            .lock()
            .ok()
            .and_then(|mut injected| injected.pop_front())
            .unwrap_or_else(|| pins.sample());

        shared.all_released.store(!buttons.any(), Ordering::Release);
        down_since = if buttons.down {
            Some(down_since.unwrap_or_else(Instant::now))
        } else {
            None
        };
        shared.down_held_ms.store(
            down_since.map_or(0, |since| since.elapsed().as_millis() as u32),
            Ordering::Release,
        );

        let is_armed = armed.load(Ordering::Acquire);
        if is_armed {
            if !was_armed {
                // A press already in progress belongs to the previous screen.
                state = if buttons.any() {
                    ButtonState::SuppressedUntilRelease
                } else {
                    ButtonState::default()
                };
            }
            let (next, choice) = state.advance(
                buttons,
                powerup_active.load(Ordering::Acquire),
                Instant::now(),
                PANIC_HOLD,
            );
            state = next;
            if let Some(choice) = choice
                && let Ok(mut choices) = shared.choices.lock()
            {
                if choices.len() >= MAX_QUEUED_CHOICES {
                    choices.pop_front();
                }
                choices.push_back(choice);
            }
        } else {
            state = ButtonState::default();
        }
        was_armed = is_armed;

        shared.ticks.fetch_add(1, Ordering::Release);
        thread::sleep(SAMPLE_INTERVAL);
    }
}
