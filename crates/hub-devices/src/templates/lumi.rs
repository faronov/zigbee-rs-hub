//! Template `lumi`: zigbee-herdsman-converters' Xiaomi/Aqara helpers
//! (zhc `lib/lumi.ts`).
//!
//! - Struct values (`lumi_basic`: Basic 0xFF01; `lumi_specific`,
//!   `lumi_electricity_meter` and the modern `lumi_on_off`: Lumi 0xFCC0
//!   0x00F7, manufacturer 0x115F): zhc's `numericAttributes2Payload` tags as
//!   sensors, for the exposes `params.keys` lists (a partial definition:
//!   battery and voltage with a battery curve, device temperature, power
//!   outage count) — voltage (tag 1, mV; a mains device's tag 150, V) and
//!   the battery from it (`device.battery`), device temperature (3), power
//!   outage count (5, less one), trigger count (6), temperature, humidity,
//!   pressure (100–102, /100), contact (100), energy (149), current (151),
//!   power (152). The old Basic 0xFF02 structure is taken as its 0xFF01
//!   (`hub::lumi`).
//! - Settings and readings on one attribute ([`ROWS`]): 0xFCC0 with the
//!   manufacturer code (operation mode of an Opple remote 0x0009 or of a
//!   switch's buttons 0x0200, power outage memory 0x0201, auto off 0x0202,
//!   LED off at night 0x0203, flip indicator 0x00F0, click mode 0x0125,
//!   switch type 0x000A, mode switch 0x0004, overload protection 0x020B,
//!   button lock 0x0200, relay lock 0x0285, power-on behaviour 0x0517,
//!   button switch mode 0x0226, consumer connected 0x0207, motion
//!   detection interval 0x0102, sensitivity 0x010C, trigger indicator
//!   0x0152), old switches' operation mode on Basic 0xFF22/0xFF23, power
//!   (Analog Input), contact (On/Off), illuminance.
//! - Motion: occupancy (0xFCC0 0x0112 or Occupancy Sensing) off after the
//!   detection interval (default 60 s, RTCGQ14LM 30 s) + 2 s, illuminance
//!   from 0x0112.
//! - Actions (`action` event entity, triggers on Report Attributes,
//!   [`TriggerSource::Attr`]): Multistate Input presentValue clicks by
//!   model (`lumi_action_multistate`; Opple remotes `button_<n>_<click>`
//!   with a release 5 s after a hold), On/Off attribute reports
//!   (`lumi_action`; the modern `lumiAction`: presentValue lookups), the
//!   WXKG01LM's press and release (hold after 1 s) and its click count
//!   0x8000 (`lumi_action_wxkg01_lm`). Opple remotes' commands in command
//!   mode (`lumi_action_on` … `lumi_action_move_color_temp`) make no
//!   action: the definitions write event mode.
//! - `lumi_set_event_mode` (configure writes 0xFCC0 0x0009 = 1),
//!   `lumi_prevent_reset` ([`PREVENT_RESET`]), `lumi_prevent_leave` (0xFCC0
//!   0x00FC reported false is written true), `lumi_on_off` (the generic
//!   switches, none on the button endpoints 4–6), `lumi_temperature` (the
//!   generic temperature).
//!
//! Not implemented (the definition is refused): cubes, lights, curtains,
//! locks, alarms, the settings old models write as Basic 0xFFF0 frames,
//! the old wall switches' hold and release (QBKG03/04/11/21/22LM).

use serde_json::Value as J;

use super::{Cx, K, Row, TExpr, build, cat, h, lab, tx};
use crate::expr::Value;
use crate::model::{AttrTiming, Binding, Codec, ConfigureOp, StateClass, TriggerSource};
use crate::zcl::ZclType;

const LUMI: u16 = 0xFCC0;
const U8: ZclType = ZclType::U8;
const BOOL: ZclType = ZclType::BOOL;

/// zhc `lumiPreventReset`: a Basic 0xFFF0 report starting with the first
/// bytes asks for the answer, written back to Basic 0xFFF0 (octets,
/// manufacturer 0x115F) on endpoint 1.
pub const PREVENT_RESET: ([u8; 5], [u8; 9]) = (
    [0xAA, 0x10, 0x05, 0x41, 0x87],
    [0xAA, 0x10, 0x05, 0x41, 0x47, 0x01, 0x01, 0x10, 0x01],
);

/// [`Cx::flags`]: a Basic 0xFF01 struct source.
const BASIC: u8 = 1;
/// [`Cx::flags`]: a Lumi 0xFCC0 0x00F7 struct source.
const SPECIFIC: u8 = 2;

const DECOUPLED: &[(i64, &str)] = &[(0, "decoupled"), (1, "control_relay")];
const OFF_ON: &[(i64, &str)] = &[(0, "command"), (1, "event")];

/// Entities on one attribute: a helper's (by its name) or, from
/// [`R_CONSUMER`] on, those other helpers and arguments add.
const ROWS: &[(u32, Row)] = &[
    (
        h("lumi_flip_indicator_light"),
        ("flip_indicator_light", K::Switch(1), LUMI, 0x00F0, U8, true),
    ),
    (
        h("lumi_switch_power_outage_memory"),
        (
            "power_outage_memory",
            K::Switch(-1),
            LUMI,
            0x0201,
            BOOL,
            true,
        ),
    ),
    (
        h("lumi_led_disabled_night"),
        (
            "led_disabled_night",
            K::Switch(-1),
            LUMI,
            0x0203,
            BOOL,
            true,
        ),
    ),
    (
        h("lumi_auto_off"),
        ("auto_off", K::Switch(-1), LUMI, 0x0202, BOOL, true),
    ),
    (
        h("lumi_socket_button_lock"),
        ("button_lock", K::Switch(0), LUMI, 0x0200, U8, true),
    ),
    (
        h("lumi_switch_mode_switch"),
        (
            "mode_switch",
            K::Select(&[(1, "quick_mode"), (4, "anti_flicker_mode")]),
            LUMI,
            0x0004,
            ZclType::U16,
            true,
        ),
    ),
    (
        h("lumi_switch_type"),
        (
            "switch_type",
            K::Select(&[(1, "toggle"), (2, "momentary")]),
            LUMI,
            0x000A,
            U8,
            true,
        ),
    ),
    (
        h("lumi_switch_click_mode"),
        (
            "click_mode",
            K::Select(&[(1, "fast"), (2, "multi")]),
            LUMI,
            0x0125,
            U8,
            true,
        ),
    ),
    (
        h("lumi_switch_click_mode_zn"),
        (
            "click_mode",
            K::Select(&[(1, "fast"), (2, "multi")]),
            LUMI,
            0x0286,
            U8,
            true,
        ),
    ),
    (
        h("lumi_overload_protection"),
        (
            "overload_protection",
            K::Number(100.0, 3840.0, "W"),
            LUMI,
            0x020B,
            ZclType::SINGLE,
            true,
        ),
    ),
    (
        h("lumi_operation_mode_opple"),
        ("operation_mode", K::Select(OFF_ON), LUMI, 0x0009, U8, true),
    ),
    (
        h("lumi_button_switch_mode"),
        (
            "button_switch_mode",
            K::Select(&[(0, "relay"), (1, "relay_and_usb")]),
            LUMI,
            0x0226,
            U8,
            true,
        ),
    ),
    (
        h("lumi_detection_interval"),
        (
            "detection_interval",
            K::Number(2.0, 65.0, "s"),
            LUMI,
            0x0102,
            U8,
            true,
        ),
    ),
    (
        h("lumi_motion_sensitivity"),
        (
            "motion_sensitivity",
            K::Select(&[(1, "low"), (2, "medium"), (3, "high")]),
            LUMI,
            0x010C,
            U8,
            true,
        ),
    ),
    (
        h("lumi_trigger_indicator"),
        ("trigger_indicator", K::Switch(1), LUMI, 0x0152, U8, true),
    ),
    (
        h("lumi_power"),
        (
            "power",
            K::Sensor("power", "W"),
            0x000C,
            0x0055,
            ZclType::SINGLE,
            false,
        ),
    ),
    (
        h("lumi_contact"),
        ("contact", K::Binary("door"), 0x0006, 0x0000, BOOL, false),
    ),
    (
        h("lumi_illuminance"),
        (
            "illuminance",
            K::Sensor("illuminance", "lx"),
            0x0400,
            0x0000,
            ZclType::U16,
            false,
        ),
    ),
    // R_CONSUMER
    (
        0,
        (
            "consumer_connected",
            K::Binary("plug"),
            LUMI,
            0x0207,
            BOOL,
            true,
        ),
    ),
    // R_POB
    (
        0,
        (
            "power_on_behavior",
            K::Select(&[(0, "on"), (1, "previous"), (2, "off"), (3, "inverted")]),
            LUMI,
            0x0517,
            U8,
            true,
        ),
    ),
    // R_OPERATION: `lumi_on_off` arguments from here.
    (
        h("operationMode"),
        (
            "operation_mode",
            K::Select(DECOUPLED),
            LUMI,
            0x0200,
            U8,
            true,
        ),
    ),
    (
        h("lockRelay"),
        ("lock_relay", K::Switch(1), LUMI, 0x0285, U8, true),
    ),
    // R_BASIC_MODE
    (
        0,
        (
            "operation_mode",
            K::Select(&[(0x12, "control_relay"), (0xFE, "decoupled")]),
            0,
            0xFF22,
            U8,
            true,
        ),
    ),
    (
        0,
        (
            "operation_mode_left",
            K::Select(&[(0x12, "control_left_relay"), (0xFE, "decoupled")]),
            0,
            0xFF22,
            U8,
            true,
        ),
    ),
    (
        0,
        (
            "operation_mode_right",
            K::Select(&[(0x22, "control_right_relay"), (0xFE, "decoupled")]),
            0,
            0xFF23,
            U8,
            true,
        ),
    ),
    // R_OCCUPANCY: Occupancy Sensing, Lumi 0x0112, its illuminance.
    (
        0,
        (
            "occupancy",
            K::Binary("occupancy"),
            0x0406,
            0x0000,
            ZclType::MAP8,
            false,
        ),
    ),
    (
        0,
        (
            "occupancy",
            K::Binary("occupancy"),
            LUMI,
            0x0112,
            ZclType::U32,
            true,
        ),
    ),
    (
        0,
        (
            "illuminance",
            K::Sensor("illuminance", "lx"),
            LUMI,
            0x0112,
            ZclType::U32,
            true,
        ),
    ),
];

const R_POM: usize = 1;
const R_CONSUMER: usize = 18;
const R_POB: usize = 19;
const R_OPERATION: usize = 20;
const R_BASIC_MODE: usize = 22;
const R_OCCUPANCY: usize = 25;

/// The helpers that are not one row of [`ROWS`], in the order [`feature`]
/// matches them; from `lumi_action_on` on, Opple remotes' commands in
/// command mode (no action: their definitions write event mode).
const HELPERS: &[u32] = &[
    h("lumi_basic"),
    h("lumi_specific"),
    h("lumi_electricity_meter"),
    h("lumi_prevent_reset"),
    h("lumi_prevent_leave"),
    h("lumi_set_event_mode"),
    h("lumi_temperature"),
    h("lumi_on_off"),
    h("lumi_switch_operation_mode_opple"),
    h("lumi_operation_mode_basic"),
    h("lumi_switch_operation_mode_basic"),
    h("lumi_occupancy"),
    h("lumi_occupancy_illuminance"),
    h("lumi_action_multistate"),
    h("lumi_action"),
    h("lumi_action_wxkg01_lm"),
    h("lumi_action_on"),
    h("lumi_action_off"),
    h("lumi_action_step"),
    h("lumi_action_stop"),
    h("lumi_action_move"),
    h("lumi_action_step_color_temp"),
    h("lumi_action_move_color_temp"),
];

/// Struct values: expose key, tag, kind, divisor (0: none), expression
/// (1 battery, 2 less one, 3 trigger count); tags below 100 are
/// diagnostics.
const TAGS: &[(&str, u8, K, u16, u8)] = &[
    ("battery", 1, K::Sensor("battery", "%"), 0, 1),
    ("voltage", 1, K::Sensor("voltage", "mV"), 0, 0),
    (
        "device_temperature",
        3,
        K::Sensor("temperature", "°C"),
        0,
        0,
    ),
    ("power_outage_count", 5, K::Sensor("", ""), 0, 2),
    ("trigger_count", 6, K::Sensor("", ""), 0, 3),
    ("temperature", 100, K::Sensor("temperature", "°C"), 100, 0),
    ("humidity", 101, K::Sensor("humidity", "%"), 100, 0),
    ("pressure", 102, K::Sensor("pressure", "hPa"), 100, 0),
    ("contact", 100, K::Binary("door"), 0, 0),
    ("energy", 149, K::Sensor("energy", "kWh"), 0, 0),
    ("voltage", 150, K::Sensor("voltage", "V"), 10, 0),
    ("current", 151, K::Sensor("current", "A"), 1000, 0),
    ("power", 152, K::Sensor("power", "W"), 0, 0),
];

const MULTI: &[(i64, &str)] = &[
    (0, "hold"),
    (1, "single"),
    (2, "double"),
    (3, "triple"),
    (255, "release"),
];
const CLICKS: &[(i64, &str)] = &[
    (1, "single"),
    (2, "double"),
    (3, "triple"),
    (4, "quadruple"),
];

// Model flags ([`MODELS`]).
/// Opple remote: `button_<endpoint>_<click>`, released 5 s after a hold.
const OPPLE: u16 = 1;
/// presentValue 5 quintuple, 6 many.
const MANY: u16 = 2;
/// presentValue 16 hold, 17 release, 18 shake (WXKG12LM).
const SHAKE: u16 = 4;
/// A cube: its Multistate Input is not clicks.
const CUBE: u16 = 8;
/// Power outage memory written as a Basic 0xFFF0 frame.
const FFF0_MEMORY: u16 = 16;
/// LED and auto off written as Basic 0xFFF0 frames.
const FFF0_LED: u16 = 32;
/// An old wall switch: `lumi_action` holds and releases.
const OLD_WALL: u16 = 64;
/// `lumi_action` click count On/Off 0x8000 (WXKG11LM).
const COUNT: u16 = 128;
/// zhc's modern `lumiAction` without arguments (ZNQBKG38LM).
const MODERN: u16 = 256;
/// Detection interval 30 s by default (RTCGQ14LM).
const INTERVAL_30: u16 = 512;
/// Voltage in 10 mV (WS-K05E).
const VOLTAGE_10MV: u16 = 1024;
/// Energy in Wh (LLKZMK12LM).
const ENERGY_WH: u16 = 2048;

/// What a helper does differently by model (zhc's lists): flags, the
/// [`BUTTONS`] of its Multistate clicks (low nibble, index + 1; an Opple
/// remote's button count) and of its On/Off clicks (high nibble).
const MODELS: &[(u32, u16, u8)] = &[
    (h("WXKG02LM_rev1"), 0, 0x10),
    (h("WXKG02LM_rev2"), 0, 0x11),
    (h("WXKG07LM"), 0, 0x11),
    (h("WXKG15LM"), 0, 1),
    (h("WXKG17LM"), 0, 1),
    (h("WXKG22LM"), 0, 1),
    (h("QBKG12LM"), 0, 0x92),
    (h("QBKG24LM"), 0, 2),
    (h("QBKG39LM"), 0, 3),
    (h("QBKG41LM"), 0, 3),
    (h("WS-EUK02"), 0, 3),
    (h("WS-EUK04"), 0, 3),
    (h("QBKG18LM"), 0, 3),
    (h("QBKG20LM"), 0, 3),
    (h("QBKG28LM"), 0, 3),
    (h("QBKG31LM"), 0, 3),
    (h("ZNQBKG25LM"), 0, 3),
    (h("QBKG25LM"), 0, 4),
    (h("QBKG26LM"), 0, 4),
    (h("QBKG29LM"), 0, 4),
    (h("QBKG32LM"), 0, 4),
    (h("QBKG33LM"), 0, 4),
    (h("QBKG34LM"), 0, 4),
    (h("ZNQBKG31LM"), 0, 4),
    (h("ZNQBKG26LM"), 0, 4),
    (h("ZNQBKG39LM"), 0, 5),
    (h("ZNQBKG40LM"), 0, 6),
    (h("ZNQBKG41LM"), 0, 7),
    (h("WS-USC02"), 0, 8),
    (h("WS-USC04"), 0, 8),
    (h("WXKG04LM"), OPPLE | MANY, 3),
    (h("WXCJKG11LM"), OPPLE | MANY, 2),
    (h("WXCJKG12LM"), OPPLE | MANY, 4),
    (h("WXCJKG13LM"), OPPLE | MANY, 6),
    (h("WXKG13LM"), MANY, 0),
    (h("WXKG12LM"), SHAKE, 0),
    (h("MFKZQ01LM"), CUBE, 0),
    (h("CTP-R01"), CUBE, 0),
    (h("ZNCZ02LM"), FFF0_MEMORY, 0),
    (h("QBCZ11LM"), FFF0_MEMORY, 0),
    (h("LLKZMK11LM"), FFF0_MEMORY, 0),
    (h("QBKG11LM"), FFF0_MEMORY | OLD_WALL, 0),
    (h("ZNCZ11LM"), FFF0_LED, 0),
    (h("QBKG03LM"), OLD_WALL, 0),
    (h("QBKG04LM"), OLD_WALL, 0),
    (h("QBKG21LM"), OLD_WALL, 0),
    (h("QBKG22LM"), OLD_WALL, 0),
    (h("WXKG11LM"), COUNT, 0),
    (h("ZNQBKG38LM"), MODERN, 0),
    (h("RTCGQ14LM"), INTERVAL_30, 0),
    (h("WS-K05E"), VOLTAGE_10MV, 0),
    (h("LLKZMK12LM"), ENERGY_WH, 0),
];

/// Buttons by endpoint (zhc's `buttonLookup`s).
const BUTTONS: &[&[(u8, &str)]] = &[
    &[(1, "left"), (2, "right"), (3, "both")],
    &[(5, "left"), (6, "right"), (7, "both")],
    &[(41, "left"), (42, "right"), (51, "both")],
    &[
        (41, "left"),
        (42, "center"),
        (43, "right"),
        (51, "left_center"),
        (52, "left_right"),
        (53, "center_right"),
        (61, "all"),
    ],
    &[(1, "top"), (2, "bottom")],
    &[(1, "top"), (2, "center"), (3, "bottom")],
    &[(1, "top"), (2, "center"), (3, "bottom"), (4, "wireless")],
    &[(41, "top"), (42, "bottom"), (51, "both")],
    &[(4, "left"), (5, "right"), (6, "both")],
];

/// [`MODELS`] of model `m`.
fn model(m: &str) -> (u16, u8) {
    let m = h(m);
    MODELS
        .iter()
        .find(|x| x.0 == m)
        .map_or((0, 0), |x| (x.1, x.2))
}

/// One button: endpoint, event type affix.
type Button = (Option<u8>, String);

/// The buttons of [`BUTTONS`] `n` (index + 1; 0: one, any endpoint),
/// `<click>_<button>`.
fn buttons(n: u8) -> Vec<Button> {
    match n.checked_sub(1).and_then(|i| BUTTONS.get(usize::from(i))) {
        Some(b) => b.iter().map(|(e, n)| (Some(*e), cat("_", n, ""))).collect(),
        None => vec![(None, String::new())],
    }
}

/// Click triggers on attribute `at` of `cluster`, per button: the affix
/// follows the click (`prefix`: precedes it).
#[inline(never)]
fn clicks(
    cx: &mut Cx<'_>,
    btns: Vec<Button>,
    (cluster, at, prefix): (u16, u16, bool),
    timing: AttrTiming,
    acts: &[(i64, String)],
    other: Option<&str>,
) {
    for (ep, a) in btns {
        let src = TriggerSource::Attr {
            ep,
            cluster,
            attr: at,
            timing,
        };
        let affix = if prefix { (&*a, "") } else { ("", &*a) };
        cx.on_label(src, acts.to_vec(), affix, other);
    }
}

const PV: (u16, u16, bool) = (0x0012, 0x0055, false);
const ON_OFF: (u16, u16, bool) = (0x0006, 0x0000, false);
const COUNT_8000: (u16, u16, bool) = (0x0006, 0x8000, false);

/// The names this template implements, as one number
/// ([`super::REVISION`]).
pub(crate) const NAMES: u32 = {
    let mut x = super::fold(0, HELPERS);
    let mut i = 0;
    while i < ROWS.len() {
        x = super::fold(x, &[ROWS[i].0]);
        i += 1;
    }
    i = 0;
    while i < MODELS.len() {
        x = super::fold(x, &[MODELS[i].0]);
        i += 1;
    }
    x = super::fold(x, METER_ARGS);
    x = super::fold(x, ON_OFF_ARGS);
    super::fold(x, ACTION_ARGS)
};

/// Arguments of `lumi_electricity_meter`.
const METER_ARGS: &[u32] = &[h("energy"), h("voltage"), h("current")];
/// Arguments of `lumi_on_off`.
const ON_OFF_ARGS: &[u32] = &[
    h("endpointNames"),
    h("deviceTemperature"),
    h("powerOutageCount"),
    h("powerOutageMemory"),
    H_OPERATION,
    H_LOCK,
];
/// Arguments of `lumi_action`.
const ACTION_ARGS: &[u32] = &[
    h("actionLookup"),
    h("buttonLookup"),
    h("endpointNames"),
    h("extraActions"),
];

const H_POM: u32 = h("lumi_switch_power_outage_memory");
const H_CLICK: u32 = h("lumi_switch_click_mode");
const H_OPERATION: u32 = h("operationMode");
const H_LOCK: u32 = h("lockRelay");

/// Expands helper `f` with arguments `p`; `false`: not implemented.
pub(crate) fn feature<'a>(cx: &mut Cx<'a>, f: &str, p: &'a J) -> bool {
    let m = cx.d.model.as_str();
    let (fl, b) = model(m);
    let hf = h(f);
    if (hf == H_POM && fl & FFF0_MEMORY != 0)
        || (fl & FFF0_LED != 0 && (hf == ROWS[2].0 || hf == ROWS[3].0))
    {
        return false;
    }
    // ZNQBKG switches: click mode 0x0286 (the row after 0x0125's).
    let zn = hf == H_CLICK && m.starts_with("ZNQBKG");
    if let Some(i) = ROWS[..R_CONSUMER].iter().position(|r| r.0 == hf) {
        cx.add(build(&ROWS[i + usize::from(zn)].1, None));
        return true;
    }
    let Some(i) = HELPERS.iter().position(|x| *x == hf) else {
        return false;
    };
    match i {
        0 => cx.flags |= BASIC,
        1 => cx.flags |= SPECIFIC,
        2 => {
            cx.args(f, p, METER_ARGS);
            cx.flags |= SPECIFIC;
        }
        3 => cx.x.lumi |= 1,
        4 => cx.x.lumi |= 2,
        5 => cx.x.configure.push(ConfigureOp::Write {
            ep: Some(1),
            cluster: LUMI,
            attr: 0x0009,
            ty: U8,
            value: Value::Int(1),
            mfr: Some(0x115F),
        }),
        6 => cx.provide(&["temperature"]),
        7 => on_off(cx, f, p),
        8 => {
            // One per button the exposes list (`operation_mode_<endpoint>`).
            let mut eps = Vec::new();
            for k in cx.keys.iter().flatten() {
                if let Some(s) = k.strip_prefix("operation_mode") {
                    eps.push(s.strip_prefix('_').and_then(|n| cx.ep_of(n)));
                }
            }
            if cx.keys.is_none() {
                eps.push(None);
            }
            for ep in eps {
                cx.add(build(&ROWS[R_OPERATION].1, ep));
            }
        }
        // Old switches' operation mode, unless a 0xFCC0 one came first.
        9 | 10
            if !cx
                .x
                .entities
                .iter()
                .any(|e| e.key.starts_with("operation_mode")) =>
        {
            let left = cx.d.endpoints.values().any(|n| n == "left");
            let rows = if cx.wants("operation_mode_left", left) {
                R_BASIC_MODE + 1..R_BASIC_MODE + 3
            } else {
                R_BASIC_MODE..R_BASIC_MODE + 1
            };
            for r in &ROWS[rows] {
                cx.add(build(&r.1, Some(1)));
            }
        }
        11 | 12 => {
            // Motion is reported, never its end.
            let lumi = i == 12;
            let mut e = build(&ROWS[R_OCCUPANCY + usize::from(lumi)].1, None);
            if lumi {
                e.value.transforms.from = Some(tx(TExpr::True));
            }
            let s = if fl & INTERVAL_30 != 0 { 30 } else { 60 };
            e.reset_after = Some(tx(TExpr::Interval(s)));
            cx.add(e);
            if lumi {
                let mut e = build(&ROWS[R_OCCUPANCY + 2].1, None);
                e.value.transforms.from = Some(tx(TExpr::Lux));
                cx.add(e);
            }
        }
        13 => {
            if fl & CUBE != 0 {
                return false;
            }
            cx.provide(&["action"]);
            let mut acts = lab(MULTI);
            if fl & SHAKE != 0 {
                acts.extend(lab(&[(16, "hold"), (17, "release"), (18, "shake")]));
            }
            if fl & MANY != 0 {
                acts.extend(lab(&[(5, "quintuple"), (6, "many")]));
            }
            if fl & OPPLE != 0 {
                // One per button endpoint 1..n of the model (zhc's
                // `msg.endpoint.ID`; not the endpoints the interview got).
                let mut btns = Vec::new();
                for ep in 1..=b & 15 {
                    btns.push((Some(ep), cat("button_", &ep.to_string(), "_")));
                }
                let pre = (PV.0, PV.1, true);
                clicks(cx, btns, pre, AttrTiming::HoldRelease, &acts, None);
            } else {
                clicks(cx, buttons(b & 15), PV, AttrTiming::Now, &acts, None);
            }
        }
        14 => return action(cx, fl, b >> 4, p),
        15 => {
            let press = lab(&[(0, "hold"), (1, "single"), (255, "release")]);
            clicks(
                cx,
                buttons(0),
                ON_OFF,
                AttrTiming::PressRelease,
                &press,
                None,
            );
            let n = AttrTiming::Now;
            clicks(cx, buttons(0), COUNT_8000, n, &lab(CLICKS), Some("many"));
        }
        _ => {}
    }
    true
}

/// zhc `lumi.modernExtend.lumiOnOff`.
fn on_off<'a>(cx: &mut Cx<'a>, f: &str, p: &'a J) {
    cx.args(f, p, ON_OFF_ARGS);
    let eps = cx.eps(p, "endpointNames", f);
    for (k, v) in p.as_object().into_iter().flatten() {
        // With arguments: with lumi_specific.
        cx.flags |= SPECIFIC;
        let r = match (h(k), v) {
            (H_OPERATION, J::Bool(true)) => R_OPERATION,
            (H_LOCK, J::Bool(true)) => R_OPERATION + 1,
            _ => continue,
        };
        for &ep in &eps {
            cx.add(build(&ROWS[r].1, ep));
        }
    }
    match p.get("powerOutageMemory").and_then(J::as_str) {
        Some("binary") => cx.add(build(&ROWS[R_POM].1, None)),
        Some("enum") => cx.add(build(&ROWS[R_POB].1, None)),
        Some(_) => cx.lack("lumi_on_off.powerOutageMemory"),
        None => {}
    }
    cx.provide(&["switch", "state"]);
    for ep in 4..=6 {
        cx.x.drop.push(("switch", Some(ep)));
    }
}

/// zhc `lumi.fromZigbee.lumi_action` (On/Off reports, buttons `b`; not
/// the old wall switches' hold and release), or with arguments (and for
/// ZNQBKG38LM) `lumi.modernExtend.lumiAction` (presentValue).
fn action<'a>(cx: &mut Cx<'a>, fl: u16, b: u8, p: &'a J) -> bool {
    cx.provide(&["action"]);
    let o = p.as_object().filter(|o| !o.is_empty());
    if o.is_none() && fl & MODERN == 0 {
        if fl & OLD_WALL != 0 {
            return false;
        }
        let acts = if fl & COUNT != 0 {
            let n = AttrTiming::Now;
            clicks(cx, buttons(0), COUNT_8000, n, &lab(CLICKS), None);
            lab(&[(1, "single")])
        } else {
            lab(&[(0, "single"), (1, "single")])
        };
        clicks(cx, buttons(b), ON_OFF, AttrTiming::Now, &acts, None);
        return true;
    }
    cx.args("lumi_action", p, ACTION_ARGS);
    let mut acts = pairs(p.get("actionLookup"));
    let mut btns = Vec::new();
    for (e, n) in pairs(p.get("buttonLookup")) {
        btns.push((Some(e as u8), cat("_", &n, "")));
    }
    if btns.is_empty() {
        let eps = cx.eps(p, "endpointNames", "lumi_action");
        let names = p.get("endpointNames").and_then(J::as_array);
        for (ep, n) in eps.into_iter().zip(names.into_iter().flatten()) {
            if let (Some(_), Some(n)) = (ep, n.as_str()) {
                btns.push((ep, cat("_", n, "")));
            }
        }
    }
    if btns.is_empty() {
        btns = buttons(0);
    }
    if acts.is_empty() {
        acts = lab(&[(1, "single")]);
    }
    clicks(cx, btns, PV, AttrTiming::Now, &acts, None);
    true
}

/// The numbers of an object of names (`{name: number}`), by number.
fn pairs(v: Option<&J>) -> Vec<(i64, String)> {
    let mut out = Vec::new();
    for (k, r) in v.and_then(J::as_object).into_iter().flatten() {
        if let Some(r) = r.as_i64() {
            out.push((r, k.clone()));
        }
    }
    out
}

/// After the helpers: the struct values of the exposes.
pub(crate) fn finish(cx: &mut Cx<'_>) {
    let (cluster, attr, mfr) = if cx.flags & BASIC != 0 {
        // Both sources (zhc registers both): the 0x00F7 struct is read as
        // the 0xFF01 one (the same tags).
        if cx.flags & SPECIFIC != 0 {
            cx.x.lumi |= 4;
        }
        (0x0000, 0xFF01, None)
    } else if cx.flags & SPECIFIC != 0 {
        (LUMI, 0x00F7, Some(0x115F))
    } else {
        return;
    };
    if cx.flags & SPECIFIC != 0 && cx.wants("consumer_connected", false) {
        cx.add(build(&ROWS[R_CONSUMER].1, None));
    }
    let (fl, _) = model(&cx.d.model);
    let battery = cx.d.device.battery.is_some();
    for &(key, tag, k, div, from) in TAGS {
        let given = cx.provided.iter().any(|p| p == key);
        let fits = match tag {
            1 => battery,
            150 => !battery,
            _ => true,
        };
        let dflt = tag == 3 || tag == 5 || (battery && tag == 1);
        if given || !fits || !cx.wants(key, dflt) {
            continue;
        }
        let mut e = build(&(key, k, 0, 0, ZclType::NO_DATA, false), None);
        e.value.binding = Binding::Struct {
            cluster,
            attr,
            mfr,
            codec: Codec::ZclTagged,
            tag,
        };
        let t = &mut e.value.transforms;
        let div = match tag {
            150 if fl & VOLTAGE_10MV != 0 => 100,
            149 if fl & ENERGY_WH != 0 => 1000,
            _ => div,
        };
        if div != 0 {
            t.div = Some(f64::from(div));
        }
        t.from = match from {
            1 => Some(tx(TExpr::Battery)),
            2 => Some(tx(TExpr::Less1)),
            3 => Some(tx(TExpr::Count16)),
            _ => None,
        };
        if from >= 2 || tag == 149 {
            e.state_class = Some(StateClass::TotalIncreasing);
        }
        if tag < 100 {
            e.category = Some(crate::model::Category::Diagnostic);
        }
        // Device temperature and power outage count: disabled by default.
        e.enabled = tag != 3 && tag != 5;
        cx.add(e);
    }
}
