use std::collections::VecDeque;

use anyhow::Result;
pub use badge_input::Buttons;
use esp_idf_svc::hal::gpio::{Gpio0, Gpio7, Gpio17, Gpio18, Input, PinDriver, Pull};

pub struct BadgeInput {
    up: PinDriver<'static, Input>,
    right: PinDriver<'static, Input>,
    down: PinDriver<'static, Input>,
    // GPIO0 is fixed by the badge PCB. Holding LEFT while resetting can select
    // the ESP ROM bootloader, so release it before power-up or reset.
    left: PinDriver<'static, Input>,
    /// Gestures queued by the USB HIL reader, sampled ahead of the real pins.
    ///
    /// Only `inject_answer` fills this, and that is gated on the `hil` feature,
    /// so in the image people carry the queue exists but nothing can ever put
    /// anything in it. `sample` is deliberately left identical in both builds:
    /// it runs on the badge's one runtime thread every 20 ms and is not worth
    /// giving two shapes.
    injected: VecDeque<Buttons>,
}

impl BadgeInput {
    pub fn new(
        up: Gpio7<'static>,
        right: Gpio18<'static>,
        down: Gpio17<'static>,
        left: Gpio0<'static>,
    ) -> Result<Self> {
        Ok(Self {
            up: PinDriver::input(up, Pull::Up)?,
            right: PinDriver::input(right, Pull::Up)?,
            down: PinDriver::input(down, Pull::Up)?,
            left: PinDriver::input(left, Pull::Up)?,
            injected: VecDeque::new(),
        })
    }

    #[cfg(feature = "hil")]
    pub fn inject_answer(&mut self, index: u8) -> bool {
        let Some(gesture) = badge_input::answer_gesture(index) else {
            return false;
        };
        if !self.injected.is_empty() {
            return false;
        }
        self.injected.extend(gesture);
        true
    }

    pub fn sample(&mut self) -> Buttons {
        if let Some(buttons) = self.injected.pop_front() {
            return buttons;
        }
        Buttons {
            up: self.up.is_low(),
            right: self.right.is_low(),
            down: self.down.is_low(),
            left: self.left.is_low(),
        }
    }
}
