//! Minimal IS31FL3731 support for Durable Trivia result feedback.
//!
//! The production badge firmware has a feature-rich C++ matrix driver. This
//! standalone Rust image needs only two images, so it keeps the integration
//! deliberately small: initialize frame zero, write one 8x8 mask, and advance
//! a two-flash result animation from the UI thread that already owns I2C.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use esp_idf_svc::hal::{
    delay::TickType,
    gpio::{Gpio9, Output, PinDriver},
    i2c::I2cDriver,
};

const ADDRESS: u8 = 0x74;
const COMMAND_REGISTER: u8 = 0xfd;
const FUNCTION_BANK: u8 = 0x0b;
const REG_CONFIG: u8 = 0x00;
const REG_PICTURE_FRAME: u8 = 0x01;
const REG_AUDIO_SYNC: u8 = 0x06;
const REG_SHUTDOWN: u8 = 0x0a;
const PWM_BASE: u8 = 0x24;
const FRAME: u8 = 0;
const PWM_CHANNELS: usize = 144;
const PWM_CHUNK: usize = 24;
const BRIGHTNESS: u8 = 48;
const WRITE_TIMEOUT: TickType = TickType::new_millis(50);

const FIRST_OFF: Duration = Duration::from_millis(250);
const SECOND_ON: Duration = Duration::from_millis(400);
const SECOND_OFF: Duration = Duration::from_millis(650);

const CHECKMARK: [u8; 8] = [
    0b0000_0001,
    0b0000_0010,
    0b1000_0100,
    0b0100_1000,
    0b0011_0000,
    0b0000_0000,
    0b0000_0000,
    0b0000_0000,
];

const X_MARK: [u8; 8] = [
    0b1000_0001,
    0b0100_0010,
    0b0010_0100,
    0b0001_1000,
    0b0001_1000,
    0b0010_0100,
    0b0100_0010,
    0b1000_0001,
];

#[derive(Clone, Copy)]
enum Icon {
    Correct,
    Wrong,
}

impl Icon {
    const fn mask(self) -> &'static [u8; 8] {
        match self {
            Self::Correct => &CHECKMARK,
            Self::Wrong => &X_MARK,
        }
    }
}

#[derive(Clone, Copy)]
struct Feedback {
    icon: Icon,
    started: Instant,
}

pub struct LedMatrix {
    _enable: PinDriver<'static, Output>,
    feedback: Option<Feedback>,
    visible: bool,
}

impl LedMatrix {
    pub fn new(enable: Gpio9<'static>, i2c: &mut I2cDriver<'static>) -> Result<Self> {
        let mut enable = PinDriver::output(enable).context("configure matrix enable GPIO9")?;
        enable.set_high().context("enable LED matrix")?;
        std::thread::sleep(Duration::from_millis(2));

        let mut matrix = Self {
            _enable: enable,
            feedback: None,
            visible: false,
        };
        matrix.initialize(i2c)?;
        Ok(matrix)
    }

    pub fn start_feedback(
        &mut self,
        i2c: &mut I2cDriver<'static>,
        correct: bool,
        now: Instant,
    ) -> Result<()> {
        let icon = if correct { Icon::Correct } else { Icon::Wrong };
        self.render(i2c, icon.mask())?;
        self.feedback = Some(Feedback { icon, started: now });
        self.visible = true;
        Ok(())
    }

    pub fn advance(&mut self, i2c: &mut I2cDriver<'static>, now: Instant) -> Result<()> {
        let Some(feedback) = self.feedback else {
            return Ok(());
        };
        let elapsed = now.saturating_duration_since(feedback.started);
        let should_be_visible =
            elapsed < FIRST_OFF || (elapsed >= SECOND_ON && elapsed < SECOND_OFF);
        if should_be_visible != self.visible {
            if should_be_visible {
                self.render(i2c, feedback.icon.mask())?;
            } else {
                self.clear(i2c)?;
            }
            self.visible = should_be_visible;
        }
        if elapsed >= SECOND_OFF {
            self.feedback = None;
        }
        Ok(())
    }

    pub fn clear(&mut self, i2c: &mut I2cDriver<'static>) -> Result<()> {
        self.write_pwm(i2c, &[0; PWM_CHANNELS])?;
        self.visible = false;
        Ok(())
    }

    fn initialize(&mut self, i2c: &mut I2cDriver<'static>) -> Result<()> {
        self.write_register(i2c, FUNCTION_BANK, REG_SHUTDOWN, 0x00)?;
        std::thread::sleep(Duration::from_millis(10));
        self.write_register(i2c, FUNCTION_BANK, REG_SHUTDOWN, 0x01)?;
        self.write_register(i2c, FUNCTION_BANK, REG_CONFIG, 0x00)?;
        self.write_register(i2c, FUNCTION_BANK, REG_PICTURE_FRAME, FRAME)?;
        self.write_register(i2c, FUNCTION_BANK, REG_AUDIO_SYNC, 0x00)?;

        self.select_bank(i2c, FRAME)?;
        for register in 0x00..=0x11 {
            self.write_bytes(i2c, &[register, 0xff])?;
        }
        self.clear(i2c)
    }

    fn render(&mut self, i2c: &mut I2cDriver<'static>, mask: &[u8; 8]) -> Result<()> {
        let mut pwm = [0_u8; PWM_CHANNELS];
        for (y, row) in mask.iter().enumerate() {
            for x in 0..8 {
                if row & (0x80 >> x) == 0 {
                    continue;
                }
                // Same mounted-panel polarity as the canonical firmware's
                // `setFlipped(true)` path.
                let hardware_x = y;
                let hardware_y = 7 - x;
                pwm[hardware_x + hardware_y * 16] = BRIGHTNESS;
            }
        }
        self.write_pwm(i2c, &pwm)
    }

    fn write_pwm(&self, i2c: &mut I2cDriver<'static>, pwm: &[u8; PWM_CHANNELS]) -> Result<()> {
        self.select_bank(i2c, FRAME)?;
        for (chunk_index, chunk) in pwm.chunks(PWM_CHUNK).enumerate() {
            let mut packet = [0_u8; PWM_CHUNK + 1];
            packet[0] = PWM_BASE
                + u8::try_from(chunk_index * PWM_CHUNK)
                    .expect("six 24-byte chunks fit in the PWM register page");
            packet[1..].copy_from_slice(chunk);
            self.write_bytes(i2c, &packet)?;
        }
        Ok(())
    }

    fn write_register(
        &self,
        i2c: &mut I2cDriver<'static>,
        bank: u8,
        register: u8,
        value: u8,
    ) -> Result<()> {
        self.select_bank(i2c, bank)?;
        self.write_bytes(i2c, &[register, value])
    }

    fn select_bank(&self, i2c: &mut I2cDriver<'static>, bank: u8) -> Result<()> {
        self.write_bytes(i2c, &[COMMAND_REGISTER, bank])
    }

    fn write_bytes(&self, i2c: &mut I2cDriver<'static>, bytes: &[u8]) -> Result<()> {
        i2c.write(ADDRESS, bytes, WRITE_TIMEOUT.ticks())
            .context("write IS31FL3731 register")?;
        Ok(())
    }
}
