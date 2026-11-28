//! IKEA remotes whose firmware binds to a group only.
//!
//! zigbee-herdsman-converters' `ikeaConfigureRemote` (E1524/E1810, E1743,
//! E1766, E1841): firmware before 2.3.075 accepts a binding to one group
//! only, later firmware to a device endpoint only. zhc binds On/Off to its
//! default bind group 901 (`constants.defaultBindGroup`) on the older
//! firmware, and herdsman's coordinator endpoint is a member of 901 and of
//! 0x0000, the group such a remote sends to while it is bound to none.
//! The hub does the same at configure (`configure::build_ops`): its
//! endpoint joins both groups (kept across NCP resets by
//! `Hub::groups_resync`), the remote's On/Off is bound to 901 and its
//! client clusters are not bound to the hub (it refuses that). Its
//! groupcasts reach the hub and fire the remote's events like unicasts. A
//! press while it is unbound reaches the hub through group 0x0000 and
//! sends the waiting bind. Newer firmware gets the generic binds to the
//! hub endpoint; without a known SWBuildID nothing changes.

use crate::fingerprint::Fingerprint;

/// zhc's default bind group.
pub const BIND_GROUP: u16 = 901;

/// Groups the hub endpoint joins for such a remote.
pub const HUB_GROUPS: [u16; 2] = [0x0000, BIND_GROUP];

/// Models of `ikeaConfigureRemote`.
const MODELS: [&str; 4] = [
    "TRADFRI remote control",
    "TRADFRI on/off switch",
    "TRADFRI open/close remote",
    "KNYCKLAN Open/Close remote",
];

/// An IKEA remote whose SWBuildID is below 2.3.075 (compared as zhc
/// does: dot-separated numbers, a part that is not a number never newer).
pub fn group_only(fp: &Fingerprint) -> bool {
    let (Some(model), Some(sw)) = (fp.model_id.as_deref(), fp.sw_build_id.as_deref()) else {
        return false;
    };
    if !MODELS.contains(&model) {
        return false;
    }
    let mut v = sw.split('.').map(|n| n.trim().parse::<u32>().ok());
    let (a, b, c) = (v.next().flatten(), v.next().flatten(), v.next().flatten());
    let newer = a.is_some_and(|a| a > 2)
        || (a == Some(2) && b.is_some_and(|b| b > 3))
        || (a == Some(2) && b == Some(3) && c.is_some_and(|c| c >= 75));
    !newer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firmware_versions_as_zhc_compares_them() {
        let fp = |model: &str, sw: &str| Fingerprint {
            model_id: Some(model.into()),
            sw_build_id: Some(sw.into()),
            ..Default::default()
        };
        for (sw, group) in [
            ("2.3.014", true),
            ("2.3.074", true),
            ("1.2.214", true),
            ("2.3.075", false),
            ("2.3.080", false),
            ("2.4.5", false),
            ("24.4.5", false),
        ] {
            assert_eq!(group_only(&fp("TRADFRI remote control", sw)), group, "{sw}");
        }
        assert!(group_only(&fp("TRADFRI on/off switch", "2.2.010")));
        assert!(!group_only(&fp("Remote Control N2", "2.3.014")));
        assert!(!group_only(&Fingerprint {
            model_id: Some("TRADFRI remote control".into()),
            ..Default::default()
        }));
    }
}
