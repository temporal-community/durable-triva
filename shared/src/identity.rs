//! How a badge derives its Temporal identity from its factory MAC.
//!
//! This lives here rather than in the firmware because two crates care about
//! the format and only one of them can build for the badge: `firmware`
//! *produces* these strings from efuse, and `web` *validates* them when it
//! counts which Workers polling the Task Queue are badges. Keeping the
//! producer and the validator in one place is what lets a test prove they
//! still agree -- the firmware half is otherwise unreachable from a host test.

/// Prefix on the badge id derived from a factory MAC.
pub const BADGE_ID_PREFIX: &str = "esp32-";

/// Prefix a badge Worker registers its Temporal identity under.
pub const BADGE_WORKER_PREFIX: &str = "badge/";

/// A badge's stable id and its human-readable callsign.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadgeIdentity {
    /// `esp32-` followed by the twelve lowercase hex digits of the MAC.
    pub id: String,
    /// `ADJECTIVE-MASCOT-XX`, the name shown on the panel and the scoreboard.
    pub callsign: String,
}

/// Chosen by the high nibble of the fifth MAC byte.
const ADJECTIVES: [&str; 16] = [
    "BRAVE", "QUICK", "BOLD", "CALM", "KEEN", "LUCKY", "RUSTY", "SWIFT", "WILD", "BRIGHT", "COZY",
    "EPIC", "NIMBLE", "SOLID", "SUPER", "TINY",
];

/// Chosen by the low nibble of the fifth MAC byte.
const MASCOTS: [&str; 16] = [
    "CRAB", "FERRIS", "FOX", "OWL", "OTTER", "YAK", "MOTH", "GECKO", "BEAR", "MOOSE", "PANDA",
    "RAVEN", "SEAL", "TIGER", "WOLF", "WREN",
];

/// Derives a badge's identity from its six-byte factory MAC.
///
/// Total: both nibble lookups are masked to 4 bits, so every MAC maps onto a
/// name and there is no failure case. Two badges collide on the callsign only
/// if they share the last two MAC bytes.
#[must_use]
pub fn identity_from_mac(mac: [u8; 6]) -> BadgeIdentity {
    let adjective = ADJECTIVES[usize::from((mac[4] >> 4) & 0x0f)];
    let mascot = MASCOTS[usize::from(mac[4] & 0x0f)];
    BadgeIdentity {
        id: format!(
            "{BADGE_ID_PREFIX}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        ),
        callsign: format!("{adjective}-{mascot}-{:02X}", mac[5]),
    }
}

/// Whether a Temporal Worker identity belongs to a badge.
///
/// The controller counts badge Workers on the shared Task Queue to size the
/// round, so a Mac Worker must not be counted as a badge.
#[must_use]
pub fn is_badge_worker_identity(identity: &str) -> bool {
    identity.starts_with(BADGE_WORKER_PREFIX) || identity.starts_with(BADGE_ID_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mac_becomes_the_documented_id_and_callsign() {
        let identity = identity_from_mac([0xe8, 0x3d, 0xc1, 0xf9, 0x4b, 0xc8]);
        assert_eq!(identity.id, "esp32-e83dc1f94bc8");
        // 0x4b: high nibble 4 -> KEEN, low nibble 11 -> RAVEN; 0xc8 -> C8.
        assert_eq!(identity.callsign, "KEEN-RAVEN-C8");
    }

    #[test]
    fn what_the_firmware_produces_is_what_the_controller_accepts() {
        // The one test the old layout could not have: the producer lived in a
        // crate that will not build for a host, so the two halves of this
        // format were only ever checked by eye.
        for byte in 0..=u8::MAX {
            let identity = identity_from_mac([byte, 0, 0, 0, byte, byte]);
            assert!(
                is_badge_worker_identity(&identity.id),
                "controller would not count {} as a badge",
                identity.id
            );
        }
        assert!(is_badge_worker_identity("badge/KEEN-RAVEN-C8"));
        assert!(!is_badge_worker_identity("63305@Fatehowler.local"));
    }

    #[test]
    fn every_adjective_and_mascot_is_reachable() {
        // A four-bit nibble indexes a sixteen-entry table, so the tables are
        // exhausted exactly. A shorter table would panic in the firmware.
        let mut adjectives = std::collections::BTreeSet::new();
        let mut mascots = std::collections::BTreeSet::new();
        for byte in 0..=u8::MAX {
            let callsign = identity_from_mac([0, 0, 0, 0, byte, 0]).callsign;
            let mut parts = callsign.split('-');
            adjectives.insert(parts.next().expect("adjective").to_owned());
            mascots.insert(parts.next().expect("mascot").to_owned());
        }
        assert_eq!(adjectives.len(), ADJECTIVES.len());
        assert_eq!(mascots.len(), MASCOTS.len());
    }

    #[test]
    fn distinct_macs_get_distinct_ids() {
        let a = identity_from_mac([1, 2, 3, 4, 5, 6]);
        let b = identity_from_mac([1, 2, 3, 4, 5, 7]);
        assert_ne!(a.id, b.id);
        assert_ne!(a.callsign, b.callsign, "the last byte is in the callsign");
    }
}
