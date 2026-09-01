//! Deep sleep, and the wake sources that bring the badge back.
//!
//! The gesture that triggers this lives in [`crate::ui`], which owns the
//! buttons and the screen. What is left here is the ESP-IDF sequence itself.

use anyhow::{Result, bail};
use esp_idf_svc::sys;

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

fn check(code: sys::esp_err_t, operation: &str) -> Result<()> {
    if code != sys::ESP_OK {
        bail!("{operation} failed with ESP-IDF error {code}");
    }
    Ok(())
}

pub fn enter_deep_sleep() -> Result<()> {
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
