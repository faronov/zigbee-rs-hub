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
//!   scene number. Both keep the generic remote events (5.4) on the same
//!   `action` entity.
//! - `tuya_led_indicator`, `relay_din_led_indicator`: On/Off 0x8001.
//! - `electricity_measurement_poll`: Electrical Measurement (and Metering)
//!   read every 60 s.

use super::{Cx, K, Row, TExpr, build, h, lab, tx};
use crate::model::TriggerSource;
use crate::zcl::ZclType;

const E8: ZclType = ZclType::ENUM8;
const POB: &[(i64, &str)] = &[(0, "off"), (1, "on"), (2, "previous")];
const BACKLIGHT: &[(i64, &str)] = &[(0, "off"), (1, "normal"), (2, "inverted")];

/// Entities by `tuyaOnOff` argument (first: one per endpoint) or helper.
const ROWS: &[(u32, Row)] = &[
    (
        h("powerOnBehavior2"),
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
        h("powerOnBehavior3"),
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
        h("switchMode"),
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
        h("onOffCountdown"),
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
        h("powerOnBehavior"),
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
        h("powerOutageMemory"),
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
        h("switchType"),
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
        h("switchTypeButton"),
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
        h("switchTypeCurtain"),
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
        h("backlightModeLowMediumHigh"),
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
        h("backlightModeOffNormalInverted"),
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
        h("indicatorMode"),
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
        h("indicatorModeNoneRelayPos"),
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
        h("backlightModeOffOn"),
        ("backlight_mode", K::Switch(1), 0x0006, 0x5000, E8, false),
    ),
    (
        h("childLock"),
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
        h("operation_mode"),
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
        h("tuya_led_indicator"),
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
        h("relay_din_led_indicator"),
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
const ON_OFF_ARGS: &[u32] = &[
    h("powerOnBehavior2"),
    h("powerOnBehavior3"),
    h("switchMode"),
    h("onOffCountdown"),
    h("powerOnBehavior"),
    h("powerOutageMemory"),
    h("switchType"),
    h("switchTypeButton"),
    h("switchTypeCurtain"),
    h("backlightModeLowMediumHigh"),
    h("backlightModeOffNormalInverted"),
    h("indicatorMode"),
    h("indicatorModeNoneRelayPos"),
    h("backlightModeOffOn"),
    h("childLock"),
    h("endpoints"),
    h("electricalMeasurements"),
];

const H_POB: u32 = h("powerOnBehavior");
const H_POB3: u32 = h("powerOnBehavior3");
const H_OPERATION_MODE: u32 = h("operation_mode");

/// Expands helper `f` with arguments `p`; `false`: not implemented.
pub(crate) fn feature<'a>(cx: &mut Cx<'a>, f: &str, p: &'a serde_json::Value) -> bool {
    // The arguments given (not false or null), and those given false.
    let (mut on, mut off) = (Vec::new(), Vec::new());
    for (k, v) in p.as_object().into_iter().flatten() {
        match v {
            serde_json::Value::Null => {}
            serde_json::Value::Bool(false) => off.push(h(k)),
            _ => on.push(h(k)),
        }
    }
    let hf = h(f);
    match hf {
        H_ON_OFF => {
            cx.args(f, p, ON_OFF_ARGS);
            let eps = cx.eps(p, "endpoints", f);
            // zhc: powerOutageMemory and powerOnBehavior2, else
            // powerOnBehavior3, else powerOnBehavior (by default).
            let memory = on.contains(&ROWS[5].0) || on.contains(&ROWS[0].0);
            for (i, (arg, row)) in ROWS[..15].iter().enumerate() {
                let set = match *arg {
                    H_POB3 => !memory && on.contains(arg),
                    H_POB => !memory && !on.contains(&H_POB3) && !off.contains(arg),
                    _ => on.contains(arg),
                };
                if set {
                    for &ep in if i < 4 { &eps[..] } else { &[None] } {
                        cx.add(build(row, ep));
                    }
                }
            }
            if on.contains(&h("electricalMeasurements")) {
                cx.provide(&["power", "current", "voltage", "energy"]);
            }
            // The switches are the generic tier's; Tuya's power-on
            // behaviour replaces the ZCL one.
            cx.provide(&["switch", "state"]);
            cx.x.drop.push(("power_on_behavior", None));
        }
        H_MULTI_ACTION => {
            for (id, map) in [
                (0xFD, &[(0, "single"), (1, "double"), (2, "hold")][..]),
                (0xFC, &[(0, "rotate_right"), (1, "rotate_left")]),
            ] {
                cx.on_label(on_off(id), lab(map), ("", ""), None);
            }
            cx.x.remote = true;
        }
        H_SWITCH_SCENE => {
            let t = cx.on_label(on_off(0xFD), Vec::new(), ("", ""), Some("switch_scene"));
            t.attrs.push(("scene".into(), tx(TExpr::Byte)));
            // The remote's On/Off and Level Control commands stay events.
            cx.x.remote = true;
        }
        H_POLL => {
            cx.args(f, p, POLL_ARGS);
            let em = !off.contains(&POLL_ARGS[0]);
            cx.x.poll = u8::from(em) | u8::from(on.contains(&POLL_ARGS[1])) << 1;
        }
        // operation_mode, tuya_led_indicator, relay_din_led_indicator
        _ => {
            let Some(r) = ROWS[15..].iter().find(|r| r.0 == hf) else {
                return false;
            };
            let mode = hf == H_OPERATION_MODE;
            cx.add(build(&r.1, mode.then_some(1)));
            cx.x.remote |= mode;
        }
    }
    true
}

/// The names this template implements, as one number
/// ([`super::REVISION`]).
pub(crate) const NAMES: u32 = {
    let mut x = super::fold(0, ON_OFF_ARGS);
    let mut i = 0;
    while i < ROWS.len() {
        x = super::fold(x, &[ROWS[i].0]);
        i += 1;
    }
    x = super::fold(x, POLL_ARGS);
    super::fold(x, &[H_ON_OFF, H_MULTI_ACTION, H_SWITCH_SCENE, H_POLL])
};

/// Arguments of `electricity_measurement_poll`.
const POLL_ARGS: &[u32] = &[
    h("electricalMeasurement"),
    h("metering"),
    h("optionDescription"),
];

const H_ON_OFF: u32 = h("tuya_on_off");
const H_MULTI_ACTION: u32 = h("multi_action");
const H_SWITCH_SCENE: u32 = h("switch_scene");
const H_POLL: u32 = h("electricity_measurement_poll");

/// An On/Off cluster command of the device.
fn on_off(id: u8) -> TriggerSource {
    TriggerSource::Cmd {
        ep: None,
        cluster: 0x0006,
        id,
        mfr: None,
        global: false,
    }
}
