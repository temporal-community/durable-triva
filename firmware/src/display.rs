//! I2C transport for the badge OLED.
//!
//! Every screen is composed by the `badge-screen` crate, which has no ESP-IDF
//! dependency and is unit tested and previewed on a development host. This file
//! owns the panel and nothing else.

use anyhow::{Context, Result, bail};
use badge_screen::{Canvas, Status, WIDTH};
use esp_idf_svc::hal::{
    delay::TickType,
    gpio::{Gpio4, Gpio5},
    i2c::{I2C0, I2cConfig, I2cDriver},
    units::KiloHertz,
};

use crate::model::{ChaosCommand, GameSnapshot, Question};

const ADDRESS: u8 = 0x3c;
/// Every OLED write is bounded rather than waiting forever.
///
/// The badge runs one current-thread Tokio runtime, so an I2C transfer that
/// never returns does not merely freeze the screen -- it stops the Activity
/// heartbeat loop, the Worker poller and the sleep monitor with it, and only a
/// reset recovers. A stuck bus is a real prospect on a badge people carry;
/// 50 ms is far more than the ~0.4 ms a 17-byte write needs at 400 kHz.
const WRITE_TIMEOUT: TickType = TickType::new_millis(50);
/// The SSD1306 accepts a control byte plus payload per write.
const MAX_PACKET: usize = 32;
const FRAME_CHUNK: usize = 16;

pub struct BadgeDisplay {
    i2c: I2cDriver<'static>,
    canvas: Canvas,
}

impl BadgeDisplay {
    pub fn new(i2c: I2C0<'static>, sda: Gpio4<'static>, scl: Gpio5<'static>) -> Result<Self> {
        let config = I2cConfig::new().baudrate(KiloHertz(400).into());
        let mut display = Self {
            i2c: I2cDriver::new(i2c, sda, scl, &config).context("initialize OLED I2C")?,
            canvas: Canvas::new(),
        };
        display.command(&[
            0xae, 0xd5, 0x80, 0xa8, 0x3f, 0xd3, 0x00, 0x40, 0x8d, 0x14, 0x20, 0x00, 0xa1, 0xc8,
            0xda, 0x12, 0x81, 0x50, 0xd9, 0xf1, 0xdb, 0x40, 0xa4, 0xa6, 0xaf,
        ])?;
        Ok(display)
    }

    pub fn show_status(&mut self, callsign: &str, status: Status) -> Result<()> {
        self.canvas.status(callsign, status);
        self.flush()
    }

    pub fn show_waiting(&mut self, callsign: &str) -> Result<()> {
        self.canvas.waiting(callsign);
        self.flush()
    }

    pub fn show_powerup(&mut self, callsign: &str, command: ChaosCommand) -> Result<()> {
        self.canvas.powerup(callsign, command);
        self.flush()
    }

    pub fn show_sleep_countdown(&mut self, callsign: &str, seconds: u64) -> Result<()> {
        self.canvas.sleep_countdown(callsign, seconds);
        self.flush()
    }

    pub fn show_sleeping(&mut self, callsign: &str) -> Result<()> {
        self.canvas.sleeping(callsign);
        self.flush()
    }

    pub fn show_question(&mut self, callsign: &str, question: &Question) -> Result<()> {
        self.canvas.question(callsign, question);
        self.flush()
    }

    /// `score_delta` is the value Temporal will record, so the badge agrees with
    /// the board while double points is active.
    pub fn show_feedback(&mut self, callsign: &str, correct: bool, score_delta: i32) -> Result<()> {
        self.canvas.feedback(callsign, correct, score_delta);
        self.flush()
    }

    pub fn show_panic(&mut self, callsign: &str) -> Result<()> {
        self.canvas.panic(callsign);
        self.flush()
    }

    pub fn show_recovered(&mut self, callsign: &str) -> Result<()> {
        self.canvas.recovered(callsign);
        self.flush()
    }

    pub fn show_results(
        &mut self,
        callsign: &str,
        badge_id: &str,
        snapshot: &GameSnapshot,
    ) -> Result<()> {
        self.canvas.results(callsign, badge_id, snapshot);
        self.flush()
    }

    pub fn power_off(&mut self) -> Result<()> {
        self.canvas.clear();
        self.flush()?;
        self.command(&[0xae])
    }

    fn command(&mut self, commands: &[u8]) -> Result<()> {
        let packet_len = commands.len() + 1;
        if packet_len > MAX_PACKET {
            bail!("OLED command is too long: {} bytes", commands.len());
        }
        let mut packet = [0_u8; MAX_PACKET];
        packet[1..packet_len].copy_from_slice(commands);
        self.i2c
            .write(ADDRESS, &packet[..packet_len], WRITE_TIMEOUT.ticks())
            .context("write OLED command")?;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        let last_column = u8::try_from(WIDTH - 1).unwrap_or(u8::MAX);
        self.command(&[0x21, 0, last_column, 0x22, 0, 7])?;
        for chunk in self.canvas.bits().chunks(FRAME_CHUNK) {
            let mut packet = [0_u8; FRAME_CHUNK + 1];
            packet[0] = 0x40;
            packet[1..].copy_from_slice(chunk);
            self.i2c
                .write(ADDRESS, &packet, WRITE_TIMEOUT.ticks())
                .context("write OLED framebuffer chunk")?;
        }
        Ok(())
    }
}
