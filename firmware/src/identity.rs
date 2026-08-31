use anyhow::{Result, bail};

pub use temporal_trivia_shared::identity::BadgeIdentity;
use temporal_trivia_shared::identity::identity_from_mac;

/// Reads this badge's factory MAC and derives its identity from it.
///
/// The derivation itself lives in `temporal_trivia_shared::identity`, where it
/// is host-testable and sits next to the controller's matching validator. All
/// that remains here is the part that genuinely needs the device.
pub fn factory_identity() -> Result<BadgeIdentity> {
    let mut mac = [0_u8; 6];
    // SAFETY: `mac` provides six writable bytes, which is the buffer contract
    // of `esp_efuse_mac_get_default`; the pointer is not retained after return.
    let result = unsafe { esp_idf_svc::sys::esp_efuse_mac_get_default(mac.as_mut_ptr()) };
    if result != 0 {
        bail!("read factory MAC failed with code {result}");
    }
    Ok(identity_from_mac(mac))
}
