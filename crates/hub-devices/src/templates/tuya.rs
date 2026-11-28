//! Template `tuya`: zigbee-herdsman-converters' Tuya helpers for devices
//! with standard clusters (zhc `lib/tuya.ts`).
//!
//! - `tuya_on_off` (`tuya.modernExtend.tuyaOnOff`, TS011F plugs, switch
//!   modules, DIN relays): the switches stay generic; the helper's
//!   settings on On/Off 0x8000–0x8002, 0x5000, the Tuya clusters 0xE001 and
//!   0xEF00 become selects and switches as zhc maps them, the generic
//!   power-on behaviour (0x4003) gives way to Tuya's, and `onOffCountdown`
//!   adds a `countdown` per endpoint (On/Off 0x4001 in s, set with On With
//!   Timed Off). `inchingSwitch` is not implemented.
//! - `operation_mode` (`tuya.fz/tz.operation_mode`, TS004F remotes and
//!   knobs): On/Off 0x8004, `command` (the device sends ZCL commands: the
//!   generic remote events stay) or `event` (scene commands).
//! - `multi_action` (`tuya.fz.multi_action`): On/Off 0xFD single, double,
//!   hold; 0xFC rotate right, left (knobs). `switch_scene`: 0xFD with the
//!   scene number.
//! - `tuya_led_indicator`, `relay_din_led_indicator`: On/Off 0x8001.
//! - `electricity_measurement_poll`: Electrical Measurement (and Metering)
//!   read every 60 s.

use super::{Cx, K, Row, TExpr, build, label, tx};
use crate::model::EventSel;
use crate::zcl::ZclType;

const E8: ZclType = ZclType::ENUM8;
const POB: &[(i64, &str)] = &[(0, "off"), (1, "on"), (2, "previous")];
const BACKLIGHT: &[(i64, &str)] = &[(0, "off"), (1, "normal"), (2, "inverted")];

/// Entities by `tuyaOnOff` argument (first: one per endpoint) or helper.
const ROWS: &[(&str, Row)] = &[
    (
        "powerOnBehavior2",
        (
            "power_on_behavior",
            K::Select(POB),
            0xE001,
            0xD010,
            E8,
            false,
        ),
    ),
    (
        "powerOnBehavior3",
        (
            "power_on_behavior",
            K::Select(POB),
            0xEF00,
            0x4002,
            E8,
            false,
        ),
    ),
    (
        "switchMode",
        (
            "switch_mode",
            K::Select(&[(0, "switch"), (1, "scene")]),
            0xE001,
            0xD020,
            E8,
            false,
        ),
    ),
    (
        "onOffCountdown",
        (
            "countdown",
            K::Number(0.0, 43200.0, "s"),
            0x0006,
            0x4001,
            ZclType::U16,
            false,
        ),
    ),
    (
        "powerOnBehavior",
        (
            "power_on_behavior",
            K::Select(POB),
            0x0006,
            0x8002,
            E8,
            false,
        ),
    ),
    (
        "powerOutageMemory",
        (
            "power_outage_memory",
            K::Select(&[(0, "off"), (1, "on"), (2, "restore")]),
            0x0006,
            0x8002,
            E8,
            false,
        ),
    ),
    (
        "switchType",
        (
            "switch_type",
            K::Select(&[(0, "toggle"), (1, "state"), (2, "momentary")]),
            0xE001,
            0xD030,
            E8,
            false,
        ),
    ),
    (
        "switchTypeButton",
        (
            "switch_type_button",
            K::Select(&[(0, "release"), (1, "press")]),
            0xE001,
            0xD030,
            E8,
            false,
        ),
    ),
    (
        "switchTypeCurtain",
        (
            "switch_type_curtain",
            K::Select(&[
                (0, "flip-switch"),
                (1, "sync-switch"),
                (2, "button-switch"),
                (3, "button2-switch"),
            ]),
            0xE001,
            0xD030,
            E8,
            false,
        ),
    ),
    (
        "backlightModeLowMediumHigh",
        (
            "backlight_mode",
            K::Select(&[(0, "low"), (1, "medium"), (2, "high")]),
            0x0006,
            0x8001,
            E8,
            false,
        ),
    ),
    (
        "backlightModeOffNormalInverted",
        (
            "backlight_mode",
            K::Select(BACKLIGHT),
            0x0006,
            0x8001,
            E8,
            false,
        ),
    ),
    (
        "indicatorMode",
        (
            "indicator_mode",
            K::Select(&[(0, "off"), (1, "off/on"), (2, "on/off"), (3, "on")]),
            0x0006,
            0x8001,
            E8,
            false,
        ),
    ),
    (
        "indicatorModeNoneRelayPos",
        (
            "indicator_mode",
            K::Select(&[(0, "none"), (1, "relay"), (2, "pos")]),
            0x0006,
            0x8001,
            E8,
            false,
        ),
    ),
    (
        "backlightModeOffOn",
        ("backlight_mode", K::Switch(1), 0x0006, 0x5000, E8, false),
    ),
    (
        "childLock",
        (
            "child_lock",
            K::Switch(-1),
            0x0006,
            0x8000,
            ZclType::BOOL,
            false,
        ),
    ),
    (
        "operation_mode",
        (
            "operation_mode",
            K::Select(&[(0, "command"), (1, "event")]),
            0x0006,
            0x8004,
            E8,
            false,
        ),
    ),
    (
        "tuya_led_indicator",
        (
            "backlight_mode",
            K::Select(BACKLIGHT),
            0x0006,
            0x8001,
            E8,
            false,
        ),
    ),
    (
        "relay_din_led_indicator",
        (
            "indicator_mode",
            K::Select(&[(0, "off"), (1, "on_off"), (2, "off_on")]),
            0x0006,
            0x8001,
            E8,
            false,
        ),
    ),
];

/// Arguments of `tuyaOnOff` implemented: those of the first 15 [`ROWS`]
/// and two that add no entity of their own.
const ON_OFF_ARGS: &[&str] = &[
    "powerOnBehavior2",
    "powerOnBehavior3",
    "switchMode",
    "onOffCountdown",
    "powerOnBehavior",
    "powerOutageMemory",
    "switchType",
    "switchTypeButton",
    "switchTypeCurtain",
    "backlightModeLowMediumHigh",
    "backlightModeOffNormalInverted",
    "indicatorMode",
    "indicatorModeNoneRelayPos",
    "backlightModeOffOn",
    "childLock",
    "endpoints",
    "electricalMeasurements",
];

/// Expands helper `f` with arguments `p`; `false`: not implemented.
pub(crate) fn feature<'a>(cx: &mut Cx<'a>, f: &str, p: &'a serde_json::Value) -> bool {
    let on = |k: &str| {
        p.get(k)
            .is_some_and(|v| !matches!(v, serde_json::Value::Bool(false) | serde_json::Value::Null))
    };
    match f {
        "tuya_on_off" => {
            cx.args(f, p, ON_OFF_ARGS);
            let eps = cx.eps(p, "endpoints", f);
            // zhc: powerOutageMemory and powerOnBehavior2, else
            // powerOnBehavior3, else powerOnBehavior (by default).
            let memory = on("powerOutageMemory") || on("powerOnBehavior2");
            for (i, (arg, row)) in ROWS[..15].iter().enumerate() {
                let set = match *arg {
                    "powerOnBehavior3" => !memory && on(arg),
                    "powerOnBehavior" => {
                        !memory && !on("powerOnBehavior3") && p.get(arg) != Some(&false.into())
                    }
                    _ => on(arg),
                };
                if set {
                    for &ep in if i < 4 { &eps[..] } else { &[None] } {
                        cx.add(build(row, ep));
                    }
                }
            }
            if on("electricalMeasurements") {
                cx.provide(&["power", "current", "voltage", "energy"]);
            }
            // The switches are the generic tier's; Tuya's power-on
            // behaviour replaces the ZCL one.
            cx.provide(&["switch", "state"]);
            cx.x.drop.push(("power_on_behavior", None));
        }
        "operation_mode" | "tuya_led_indicator" | "relay_din_led_indicator" => {
            let r = ROWS.iter().find(|r| r.0 == f).unwrap();
            cx.add(build(&r.1, (f == "operation_mode").then_some(1)));
            cx.x.remote |= f == "operation_mode";
        }
        "multi_action" => {
            for (id, map) in [
                (0xFD, &[(0, "single"), (1, "double"), (2, "hold")][..]),
                (0xFC, &[(0, "rotate_right"), (1, "rotate_left")]),
            ] {
                let types: Vec<String> = map.iter().map(|m| m.1.to_string()).collect();
                let ev = EventSel::Expr(tx(label(map, "", true, None)));
                cx.on(None, 0x0006, id as u8, ev, &types);
            }
            cx.x.remote = true;
        }
        "switch_scene" => {
            let ev = EventSel::Fixed("switch_scene".into());
            let t = cx.on(None, 0x0006, 0xFD, ev, &["switch_scene".into()]);
            t.attrs.push(("scene".into(), tx(TExpr::Byte)));
        }
        "electricity_measurement_poll" => {
            cx.args(
                f,
                p,
                &["electricalMeasurement", "metering", "optionDescription"],
            );
            let em = p.get("electricalMeasurement") != Some(&false.into());
            cx.x.poll = u8::from(em) | u8::from(on("metering")) << 1;
        }
        _ => return false,
    }
    true
}
