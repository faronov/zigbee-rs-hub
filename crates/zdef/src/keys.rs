//! Key tables v1 (spec 8.2).
//!
//! Every map in a definition uses small unsigned integer keys. There are
//! three tables: top level, entity (also used for `parts` and
//! `generic.patch` entries) and one shared table for all other sub-maps.
//! Keys 64..=127 are the optional-extension range of every map: a decoder of
//! the same `zdef` major ignores them; any other unknown key is an error.
//!
//! This module is the authoritative source of the numbers; `zdefc`, the
//! JSON Schema (`schema/zdef-v1.schema.json`) and the spec tables follow it.
//! The tables are append-only within `zdef: 1`.

/// First key of the optional-extension range.
pub const EXT_FIRST: u64 = 64;
/// Last key of the optional-extension range.
pub const EXT_LAST: u64 = 127;

pub fn is_extension(k: u64) -> bool {
    (EXT_FIRST..=EXT_LAST).contains(&k)
}

macro_rules! table {
    ($name:ident, $lookup:ident, $rev:ident, { $($k:literal => $s:literal),* $(,)? }) => {
        pub const $name: &[(u16, &str)] = &[$(($k, $s)),*];
        /// Name -> key.
        pub fn $lookup(name: &str) -> Option<u16> {
            $name.iter().find(|e| e.1 == name).map(|e| e.0)
        }
        /// Key -> name.
        pub fn $rev(key: u64) -> Option<&'static str> {
            $name.iter().find(|e| e.0 as u64 == key).map(|e| e.1)
        }
    };
}

table!(TOP, top_key, top_name, {
    0 => "zdef", 1 => "id", 2 => "revision", 3 => "vendor", 4 => "model", 5 => "description",
    6 => "match", 7 => "white_label", 8 => "requires", 9 => "device", 10 => "generic",
    11 => "endpoints", 12 => "tuya", 13 => "configure", 14 => "entities", 15 => "gp",
    16 => "ignore", 17 => "consts", 18 => "source", 19 => "mfr", 20 => "zdef_fragment",
    21 => "templates", 22 => "priority", 23 => "self_description", 31 => "x-wasm",
});

table!(ENT, ent_key, ent_name, {
    0 => "key", 1 => "platform", 2 => "device_class", 3 => "unit", 4 => "state_class",
    5 => "category", 6 => "name", 7 => "ep", 8 => "zcl", 9 => "tuya", 10 => "struct",
    11 => "gp", 12 => "from", 13 => "to", 14 => "mul", 15 => "div", 16 => "offset",
    17 => "precision", 18 => "invalid", 19 => "map", 20 => "value_on", 21 => "value_off",
    22 => "min", 23 => "max", 24 => "step", 25 => "mode", 26 => "access", 27 => "report",
    28 => "enabled", 29 => "reset_after", 30 => "parts", 31 => "event_types",
    32 => "triggers", 33 => "press", 34 => "codec", 35 => "present_if", 36 => "attrs",
    37 => "hub", 38 => "invert",
});

table!(SUB, sub_key, sub_name, {
    0 => "cluster", 1 => "attr", 2 => "type", 3 => "mfr", 4 => "ep", 5 => "id", 6 => "attrs",
    7 => "clusters", 8 => "value", 9 => "codec", 10 => "tag", 11 => "cmd", 12 => "report",
    13 => "gp_cmd", 14 => "when", 15 => "event", 16 => "write", 17 => "any", 18 => "prefix",
    19 => "manufacturer", 20 => "model", 21 => "device_id", 22 => "manufacturer_id",
    23 => "model_id", 24 => "src_id", 25 => "mask", 26 => "gp", 27 => "bind", 28 => "read",
    29 => "power", 30 => "sleepy", 31 => "battery", 32 => "voltage_to_percent",
    33 => "checkin_s", 34 => "exclude", 35 => "patch", 36 => "disable", 37 => "only_endpoints",
    38 => "time_sync", 39 => "magic_packet", 40 => "query", 41 => "security_min",
    42 => "report_interval_s", 43 => "vendor",
    // 44..: codec names and parameters, composite part names (prototype order kept)
    44 => "tuya_schedule_day", 45 => "day", 46 => "transitions",
    47 => "hvac_mode", 48 => "hvac_action", 49 => "target_temperature", 50 => "current_temperature",
    51 => "mfr_code", 52 => "app_version", 53 => "stack_version", 54 => "hw_version",
    55 => "sw_build", 56 => "endpoints", 57 => "profile", 58 => "in", 59 => "out",
    60 => "min", 61 => "max", 62 => "payload", 63 => "default",
    // 64..=127: optional extensions
    128 => "group", 129 => "checkin", 130 => "interval_qs", 131 => "availability",
    132 => "timeout_s", 133 => "ota", 134 => "ignore_commands", 135 => "ieee",
    136 => "commands", 137 => "server", 138 => "client", 139 => "force_time_sync_s",
    140 => "bind_basic", 141 => "gateway_status", 142 => "send", 143 => "mcu_version",
    144 => "optimistic", 145 => "dedup", 146 => "interval_s", 147 => "tuya",
    148 => "zcl_tagged", 149 => "tuya_schedule_week", 150 => "variant", 151 => "bcd_time",
    152 => "hhmm", 153 => "ascii", 154 => "utf16be_hex", 155 => "bitmap_flags",
    // composite part names (HA property names), continued
    160 => "on_off", 161 => "brightness", 162 => "color_temp", 163 => "color_xy",
    164 => "color_hs", 165 => "color_mode", 166 => "effect", 167 => "position", 168 => "tilt",
    169 => "state", 170 => "preset", 171 => "fan_mode", 172 => "percentage", 173 => "direction",
    174 => "oscillating", 175 => "target_temperature_low", 176 => "target_temperature_high",
    177 => "current_humidity", 178 => "target_humidity", 179 => "swing_mode", 180 => "tone",
    181 => "volume", 182 => "duration", 183 => "valve_position", 184 => "running_state",
});

/// Part names usable as keys of `parts` (a subset of [`SUB`]).
pub const PART_NAMES: &[&str] = &[
    "hvac_mode",
    "hvac_action",
    "target_temperature",
    "current_temperature",
    "on_off",
    "brightness",
    "color_temp",
    "color_xy",
    "color_hs",
    "color_mode",
    "effect",
    "position",
    "tilt",
    "state",
    "preset",
    "fan_mode",
    "percentage",
    "direction",
    "oscillating",
    "target_temperature_low",
    "target_temperature_high",
    "current_humidity",
    "target_humidity",
    "swing_mode",
    "tone",
    "volume",
    "duration",
    "valve_position",
    "running_state",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn unique(t: &[(u16, &str)]) {
        for (i, a) in t.iter().enumerate() {
            assert!(!is_extension(a.0 as u64), "{} in extension range", a.1);
            for b in &t[i + 1..] {
                assert!(a.0 != b.0 && a.1 != b.1, "duplicate {:?} {:?}", a, b);
            }
        }
    }

    #[test]
    fn tables_unique() {
        unique(TOP);
        unique(ENT);
        unique(SUB);
        for p in PART_NAMES {
            assert!(sub_key(p).is_some(), "{p}");
        }
    }
}
