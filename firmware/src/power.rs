use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Result, bail};
use esp_idf_svc::sys;

use crate::{
    display::BadgeDisplay,
    haptics::{self, HapticEvent, SharedHaptics},
    input::ButtonReader,
    with_display_if_idle,
};

const SLEEP_ARM_DELAY: Duration = Duration::from_millis(250);
const SLEEP_HOLD: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

// Keep the original Echo revision nets (MCU_SLEEP/INT_PWR) in the mask, but
// also arm the physical face-button GPIOs. The tested badge does not pull
// either shared net low when UP is pressed, so relying on GPIO10/GPIO13 alone
// leaves it permanently asleep. GPIO0/LEFT is a boot strap; a short tap should
// release before the ROM samples it, but LEFT is validated separately.
const BUTTON_WAKE_GPIOS: [sys::gpio_num_t; 6] = [
    sys::gpio_num_t_GPIO_NUM_0,
    sys::gpio_num_t_GPIO_NUM_7,
    sys::gpio_num_t_GPIO_NUM_10,
    sys::gpio_num_t_GPIO_NUM_13,
    sys::gpio_num_t_GPIO_NUM_17,
    sys::gpio_num_t_GPIO_NUM_18,
];
const BUTTON_WAKE_MASK: u64 = (1_u64 << sys::gpio_num_t_GPIO_NUM_0)
    | (1_u64 << sys::gpio_num_t_GPIO_NUM_7)
    | (1_u64 << sys::gpio_num_t_GPIO_NUM_10)
    | (1_u64 << sys::gpio_num_t_GPIO_NUM_13)
    | (1_u64 << sys::gpio_num_t_GPIO_NUM_17)
    | (1_u64 << sys::gpio_num_t_GPIO_NUM_18);

pub async fn monitor(
    display: Arc<Mutex<BadgeDisplay>>,
    input: ButtonReader,
    haptics: SharedHaptics,
    activity_active: Arc<AtomicBool>,
    powerup_active: Arc<AtomicBool>,
    callsign: String,
) -> Result<()> {
    let mut shown_second: Option<u64> = None;

    loop {
        if activity_active.load(Ordering::Acquire) || powerup_active.load(Ordering::Acquire) {
            shown_second = None;
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }

        // The button sampler owns the pins and times the hold, so this loop
        // being slow can delay the countdown but can no longer misread it.
        let elapsed = input.down_held();
        if elapsed.is_zero() {
            if shown_second.take().is_some() {
                // A question can be assigned between the check at the top of
                // this loop and here, so ownership is re-tested under the
                // display lock rather than trusted from a few lines ago.
                with_display_if_idle(&display, &activity_active, |screen| {
                    screen.show_waiting(&callsign)
                })?;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }
        if elapsed >= SLEEP_HOLD {
            log::info!("DOWN held for 3 seconds; entering deep sleep");
            if shown_second != Some(0) {
                shown_second = Some(0);
                with_display_if_idle(&display, &activity_active, |screen| {
                    screen.show_sleep_countdown(&callsign, 0)
                })?;
                haptics::play(&haptics, HapticEvent::SleepCountdown).await;
            }
            if !with_display_if_idle(&display, &activity_active, |screen| {
                screen.show_sleeping(&callsign)
            })? {
                // A question arrived while the countdown was finishing. The
                // player is in a round now; sleeping would drop them out of it.
                shown_second = None;
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
            while !input.all_released() {
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            with_display_if_idle(&display, &activity_active, |screen| screen.power_off())?;
            haptics::off(&haptics).await?;
            enter_deep_sleep()?;
        }

        if elapsed >= SLEEP_ARM_DELAY {
            let remaining_ms = SLEEP_HOLD.saturating_sub(elapsed).as_millis() as u64;
            let remaining_seconds = remaining_ms.div_ceil(1000);
            if shown_second != Some(remaining_seconds) {
                shown_second = Some(remaining_seconds);
                with_display_if_idle(&display, &activity_active, |screen| {
                    screen.show_sleep_countdown(&callsign, remaining_seconds)
                })?;
                haptics::play(&haptics, HapticEvent::SleepCountdown).await;
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn check(code: sys::esp_err_t, operation: &str) -> Result<()> {
    if code != sys::ESP_OK {
        bail!("{operation} failed with ESP-IDF error {code}");
    }
    Ok(())
}

fn enter_deep_sleep() -> Result<()> {
    // SAFETY: every GPIO in `BUTTON_WAKE_GPIOS` is RTC-capable on the ESP32-S3
    // used by this fixed badge revision. Calls are sequenced according to the
    // ESP-IDF deep-sleep API, and `check` validates every fallible return code.
    unsafe {
        check(
            sys::esp_sleep_disable_wakeup_source(sys::esp_sleep_source_t_ESP_SLEEP_WAKEUP_ALL),
            "disable existing wake sources",
        )?;
        check(
            sys::esp_sleep_pd_config(
                sys::esp_sleep_pd_domain_t_ESP_PD_DOMAIN_RTC_PERIPH,
                sys::esp_sleep_pd_option_t_ESP_PD_OPTION_ON,
            ),
            "keep RTC peripherals powered",
        )?;
        for gpio in BUTTON_WAKE_GPIOS {
            check(sys::rtc_gpio_init(gpio), "initialize button wake GPIO")?;
            check(
                sys::rtc_gpio_set_direction(gpio, sys::rtc_gpio_mode_t_RTC_GPIO_MODE_INPUT_ONLY),
                "set button wake GPIO input",
            )?;
            check(sys::rtc_gpio_pullup_en(gpio), "enable wake pull-up")?;
            check(sys::rtc_gpio_pulldown_dis(gpio), "disable wake pull-down")?;
        }
        check(
            sys::esp_sleep_enable_ext1_wakeup(
                BUTTON_WAKE_MASK,
                sys::esp_sleep_ext1_wakeup_mode_t_ESP_EXT1_WAKEUP_ANY_LOW,
            ),
            "arm any-button wake",
        )?;
        log::info!("Deep sleep armed; face buttons wake directly with GPIO10/GPIO13 fallback");
        sys::esp_deep_sleep_start();
    }
}
