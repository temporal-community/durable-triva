//! The vibration motor, as data plus a setter.
//!
//! Patterns used to be `async fn`s that awaited between pulses. They now
//! describe themselves as steps and the UI thread advances them, because that
//! thread also samples the buttons every 5 ms: a pattern that blocked for its
//! 300 ms would drop presses on the floor, which is the exact fault the
//! dedicated thread exists to prevent.

use std::time::Duration;

use anyhow::{Context, Result};
use esp_idf_svc::hal::ledc::LedcDriver;

const ORIGINAL_STRENGTH: u8 = 155;
const SOFT_STRENGTH: u8 = 110;
const FIRM_STRENGTH: u8 = 200;
const ORIGINAL_PULSE: Duration = Duration::from_millis(35);
const PATTERN_GAP: Duration = Duration::from_millis(80);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HapticEvent {
    SleepCountdown,
    Correct,
    Wrong,
    Crash,
    Recovered,
    Powerup,
    Winner,
    RoundOver,
}

/// One step of a pattern: hold the motor at `strength` for `duration`.
/// Strength zero is the silence between pulses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HapticStep {
    pub strength: u8,
    pub duration: Duration,
}

const fn pulse(strength: u8) -> HapticStep {
    HapticStep {
        strength,
        duration: ORIGINAL_PULSE,
    }
}

const GAP: HapticStep = HapticStep {
    strength: 0,
    duration: PATTERN_GAP,
};

// Named so each pattern is a real `'static` slice rather than a temporary.
const SINGLE: [HapticStep; 1] = [pulse(ORIGINAL_STRENGTH)];
const DOUBLE_SOFT: [HapticStep; 3] = [pulse(SOFT_STRENGTH), GAP, pulse(SOFT_STRENGTH)];
const DOUBLE: [HapticStep; 3] = [pulse(ORIGINAL_STRENGTH), GAP, pulse(ORIGINAL_STRENGTH)];
const THUMP: [HapticStep; 1] = [HapticStep {
    strength: FIRM_STRENGTH,
    duration: Duration::from_millis(120),
}];
const RISE: [HapticStep; 5] = [
    pulse(SOFT_STRENGTH),
    GAP,
    pulse(135),
    GAP,
    pulse(ORIGINAL_STRENGTH),
];

/// The steps that make up one event's feel.
#[must_use]
pub fn pattern(event: HapticEvent) -> &'static [HapticStep] {
    match event {
        HapticEvent::SleepCountdown | HapticEvent::Correct | HapticEvent::RoundOver => &SINGLE,
        HapticEvent::Wrong => &DOUBLE_SOFT,
        HapticEvent::Crash => &THUMP,
        HapticEvent::Recovered | HapticEvent::Powerup => &DOUBLE,
        HapticEvent::Winner => &RISE,
    }
}

pub struct BadgeHaptics {
    driver: LedcDriver<'static>,
}

impl BadgeHaptics {
    pub fn new(mut driver: LedcDriver<'static>) -> Result<Self> {
        driver.set_duty(0).context("turn haptic motor off")?;
        Ok(Self { driver })
    }

    /// Drives the motor at `strength`, where zero is off.
    pub fn set(&mut self, strength: u8) -> Result<()> {
        let duty = self.driver.get_max_duty() * u32::from(strength) / u32::from(u8::MAX);
        self.driver.set_duty(duty).context("set haptic strength")
    }

    pub fn off(&mut self) -> Result<()> {
        self.set(0)
    }
}
