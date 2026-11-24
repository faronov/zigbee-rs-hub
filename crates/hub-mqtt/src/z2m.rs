//! Hub entities as Zigbee2MQTT sees devices: friendly names, the flat JSON
//! state of `<base>/<friendly_name>`, `exposes` (Zigbee2MQTT's schema:
//! `numeric`, `binary`, `enum`, `text`, `composite`, and the `light`,
//! `switch`, `cover`, `climate`, `lock`, `fan` specifics), and `/set` /
//! `/get` payloads turned into hub entity commands.
//!
//! Property names are the entity keys, which for catalog definitions are
//! the Zigbee2MQTT expose names already (the catalog is generated from
//! zigbee-herdsman-converters). Generic-tier keys get Zigbee2MQTT's names
//! (`switch` → `state`, `battery_voltage` → `voltage` in mV) and composite
//! platforms their usual features (lights: `state`, `brightness` 0–254,
//! `color_temp` in mired, `color` {x, y} / {hue, saturation}). A property
//! that two entities of one device would share (the same key on two
//! endpoints) gets the endpoint name as a suffix (`state_l1`), like
//! Zigbee2MQTT's multi-endpoint devices.

use std::collections::BTreeMap;

use serde_json::{Map, Value as J};

use hub_devices::vecmap::btree_map;

use crate::model::{Device, Entity, Model, Platform, State};
use crate::obj::{Obj, put};

/// Zigbee2MQTT access bits.
pub mod access {
    /// Published in the state.
    pub const STATE: u8 = 1;
    /// Settable (`/set`).
    pub const SET: u8 = 2;
    /// Readable on request (`/get`).
    pub const GET: u8 = 4;
}

// ---------------------------------------------------------------------------
// Friendly names

/// A hub device name as a Zigbee2MQTT friendly name: no MQTT wildcards, no
/// empty or surrounding levels, at most one `/`, and nothing Zigbee2MQTT
/// would read as an endpoint or a command topic (`…/1`, `…/set`).
pub fn sanitize(name: &str) -> String {
    let mut s: String = name
        .trim()
        .chars()
        .map(|c| {
            if matches!(c, '+' | '#' | '\0') || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    while s.contains("//") {
        s = s.replace("//", "/");
    }
    let mut s = s.trim_matches('/').trim().to_string();
    let segs: Vec<&str> = s.split('/').collect();
    let bad_seg = |x: &&str| {
        matches!(*x, "set" | "get" | "availability" | "action")
            || (!x.is_empty() && x.chars().all(|c| c.is_ascii_digit()))
    };
    if segs.len() > 2 || (segs.len() == 2 && (segs[1..].iter().any(bad_seg) || segs[0] == "bridge"))
    {
        s = s.replace('/', "_");
    }
    if s == "bridge" {
        s.push('_');
    }
    s
}

/// Checks a name a client asks for (`bridge/request/device/rename`).
pub fn check_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("friendly_name must not be empty".into());
    }
    if sanitize(name) != name {
        return Err(format!(
            "friendly name {name:?} is not allowed (no '+', '#', empty levels, more than one '/', a trailing '/<number>' or '/set')"
        ));
    }
    Ok(())
}

/// Friendly names of the devices: the hub device name, made unique. A
/// device that has a name keeps it while it stays unique; a later device
/// with the same name gets `_<last 4 hex of the IEEE>`.
#[derive(Debug, Default, Clone)]
pub struct Names {
    by_id: BTreeMap<String, String>,
    by_name: BTreeMap<String, String>,
}

/// A friendly-name change: `(device id, old, new)`.
pub type Rename = (String, Option<String>, Option<String>);

impl Names {
    /// The friendly name of a device.
    pub fn get(&self, id: &str) -> Option<&str> {
        self.by_id.get(id).map(String::as_str)
    }

    /// The device of a friendly name or `0x…` IEEE address.
    pub fn lookup(&self, name: &str) -> Option<&str> {
        self.by_name.get(name).map(String::as_str)
    }

    /// Re-derives the names; returns the changes (new devices: old `None`;
    /// gone ones: new `None`).
    pub fn assign(&mut self, devices: &BTreeMap<String, Box<Device>>) -> Vec<Rename> {
        let want = |d: &Device| {
            let s = sanitize(&d.name);
            if s.is_empty() { d.ieee_address() } else { s }
        };
        let mut next: BTreeMap<String, String> = BTreeMap::new();
        // IEEE addresses resolve to their device, never to another name.
        let mut taken: Vec<String> = devices.values().map(|d| d.ieee_address()).collect();
        let mut take = |n: &String| {
            if taken.contains(n) {
                false
            } else {
                taken.push(n.clone());
                true
            }
        };
        // Keep what still fits the device's name.
        for (id, d) in devices {
            let w = want(d);
            if let Some(prev) = self.by_id.get(id)
                && (*prev == w
                    || prev
                        .strip_prefix(&w)
                        .is_some_and(|r| r.starts_with('_') && r.len() == 5))
                && (prev != &d.ieee_address() || w == d.ieee_address())
                && take(prev) | (prev == &d.ieee_address())
            {
                next.insert(id.clone(), prev.clone());
            }
        }
        for (id, d) in devices {
            if next.contains_key(id) {
                continue;
            }
            let w = want(d);
            let ieee = d.ieee_address();
            let tail = &ieee[ieee.len() - 4..];
            let alt = format!("{w}_{tail}");
            let name = if w == ieee || take(&w) {
                w
            } else if take(&alt) {
                alt
            } else {
                ieee
            };
            next.insert(id.clone(), name);
        }
        let mut changes = Vec::new();
        for (id, n) in &next {
            let old = self.by_id.get(id);
            if old != Some(n) {
                changes.push((id.clone(), old.cloned(), Some(n.clone())));
            }
        }
        for (id, old) in &self.by_id {
            if !next.contains_key(id) {
                changes.push((id.clone(), Some(old.clone()), None));
            }
        }
        self.by_name = btree_map(next.iter().map(|(id, n)| (n.clone(), id.clone())));
        for (id, d) in devices {
            self.by_name
                .entry(d.ieee_address())
                .or_insert_with(|| id.clone());
        }
        self.by_id = next;
        changes
    }
}

// ---------------------------------------------------------------------------
// Device layout: which properties each entity produces

/// How an entity maps to Zigbee2MQTT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// One property (sensors, numbers, selects, texts, binary sensors).
    Simple,
    /// `ON`/`OFF` (switch platform).
    Switch,
    /// A button: a set-only enum.
    Button,
    /// `action` values.
    Action,
    /// Light features.
    Light,
    /// Cover (or valve) features.
    Cover,
    /// Climate features.
    Climate,
    /// Lock features.
    Lock,
    /// Fan features.
    Fan,
    /// Siren on/off.
    Siren,
}

/// One entity's place in its device's Zigbee2MQTT view.
#[derive(Debug, Clone)]
pub struct Feat<'a> {
    /// The entity.
    pub e: &'a Entity,
    /// Its mapping.
    pub kind: Kind,
    /// The property (simple kinds) or the entity key (composite).
    pub prop: String,
    /// Endpoint suffix of every property of this entity.
    pub suffix: Option<String>,
}

impl Feat<'_> {
    /// A property name with this entity's suffix.
    pub fn p(&self, base: &str) -> String {
        match &self.suffix {
            Some(s) => format!("{base}_{s}"),
            None => base.to_string(),
        }
    }

    fn base_props(&self, setpoint: &str) -> Vec<String> {
        match self.kind {
            Kind::Simple | Kind::Switch | Kind::Button | Kind::Siren => vec![self.prop.clone()],
            Kind::Action => Vec::new(),
            Kind::Light => ["state", "brightness", "color_temp", "color", "color_mode"]
                .map(String::from)
                .to_vec(),
            Kind::Cover => ["state", "position", "tilt"].map(String::from).to_vec(),
            Kind::Climate => [
                "local_temperature",
                setpoint,
                "system_mode",
                "running_state",
                "preset",
                "fan_mode",
            ]
            .map(String::from)
            .to_vec(),
            Kind::Lock => ["state", "lock_state"].map(String::from).to_vec(),
            Kind::Fan => ["fan_state", "fan_mode"].map(String::from).to_vec(),
        }
    }
}

/// A device's Zigbee2MQTT view.
#[derive(Debug)]
pub struct Layout<'a> {
    /// The device.
    pub dev: &'a Device,
    /// Its entities that map to something.
    pub feats: Vec<Feat<'a>>,
    /// The climate setpoint property name.
    pub setpoint: &'static str,
}

/// The endpoint name of an entity: the name suffix the hub gives keys on
/// named endpoints (`state l1` → `l1`), else the endpoint number.
fn ep_name(e: &Entity) -> String {
    let lead = format!("{} ", e.key.replace('_', " "));
    e.name
        .as_deref()
        .and_then(|n| n.strip_prefix(&lead))
        .map(|r| r.trim().to_lowercase().replace(' ', "_"))
        .filter(|r| !r.is_empty() && !r.contains(['/', '+', '#']))
        .unwrap_or_else(|| e.ep.to_string())
}

/// Tuya EF00 devices (`TS0601`, `_TZE…` manufacturers) call the heating
/// setpoint `current_heating_setpoint` in Zigbee2MQTT; ZCL thermostats
/// `occupied_heating_setpoint`.
fn setpoint_name(d: &Device) -> &'static str {
    let tuya = d
        .model_id
        .as_deref()
        .is_some_and(|m| m.starts_with("TS0601"))
        || d.manufacturer
            .as_deref()
            .is_some_and(|m| m.starts_with("_T"));
    if tuya {
        "current_heating_setpoint"
    } else {
        "occupied_heating_setpoint"
    }
}

impl<'a> Layout<'a> {
    /// The view of `dev`.
    pub fn of(m: &'a Model, dev: &'a Device) -> Self {
        let ents: Vec<&Entity> = m.entities_of(&dev.id).filter(|e| !e.orphaned).collect();
        let has_voltage = ents.iter().any(|e| e.key == "voltage");
        let setpoint = setpoint_name(dev);
        let mut feats: Vec<Feat> = ents
            .iter()
            .filter_map(|e| {
                let kind = match e.platform {
                    Platform::Sensor
                    | Platform::BinarySensor
                    | Platform::Number
                    | Platform::Select
                    | Platform::Text => {
                        // Device fields: `linkquality` comes from the device
                        // record; `last_seen` is off in Zigbee2MQTT by default.
                        if matches!(e.key.as_str(), "linkquality" | "last_seen") {
                            return None;
                        }
                        Kind::Simple
                    }
                    Platform::Switch => Kind::Switch,
                    Platform::Button => Kind::Button,
                    Platform::Event => Kind::Action,
                    Platform::Light => Kind::Light,
                    Platform::Cover | Platform::Valve => Kind::Cover,
                    Platform::Climate => Kind::Climate,
                    Platform::Lock => Kind::Lock,
                    Platform::Fan => Kind::Fan,
                    Platform::Siren => Kind::Siren,
                    Platform::Other => return None,
                };
                let prop = match (e.key.as_str(), kind) {
                    ("switch", Kind::Switch) => "state".to_string(),
                    ("battery_voltage", _) if !has_voltage && e.unit.as_deref() == Some("V") => {
                        "voltage".to_string()
                    }
                    (k, _) => k.to_string(),
                };
                Some(Feat {
                    e,
                    kind,
                    prop,
                    suffix: None,
                })
            })
            .collect();
        // Properties two entities would share get endpoint suffixes.
        let mut props: Vec<String> = Vec::new();
        for f in &feats {
            props.extend(f.base_props(setpoint));
        }
        let shared = |p: &String| props.iter().filter(|x| *x == p).count() > 1;
        let actions = feats.iter().filter(|f| f.kind == Kind::Action).count();
        let mut used: Vec<(String, String)> = Vec::new();
        for f in &mut feats {
            let clash = if f.kind == Kind::Action {
                actions > 1
            } else {
                f.base_props(setpoint).iter().any(shared)
            };
            if clash {
                let mut s = ep_name(f.e);
                let k = (f.prop.clone(), s.clone());
                if used.contains(&k) {
                    s = format!("{}_{}", f.e.ep, f.e.key);
                } else {
                    used.push(k);
                }
                f.suffix = Some(s);
            }
        }
        Layout {
            dev,
            feats,
            setpoint,
        }
    }

    /// Readable on request: mains devices (sleepy ones answer reads only
    /// when they poll).
    fn get_bit(&self) -> u8 {
        if self.dev.sleepy() { 0 } else { access::GET }
    }
}

// ---------------------------------------------------------------------------
// State

fn on_off(v: &J) -> Option<&'static str> {
    match v {
        J::Bool(true) => Some("ON"),
        J::Bool(false) => Some("OFF"),
        J::String(s) if s.eq_ignore_ascii_case("on") => Some("ON"),
        J::String(s) if s.eq_ignore_ascii_case("off") => Some("OFF"),
        _ => None,
    }
}

fn mired(kelvin: f64) -> Option<i64> {
    (kelvin > 0.0).then(|| (1_000_000.0 / kelvin).round() as i64)
}

/// Integral floats as integers (`100`, not `100.0`), as Zigbee2MQTT
/// publishes them.
pub fn tidy(v: J) -> J {
    match &v {
        J::Number(n) if n.is_f64() => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => J::from(f as i64),
            _ => v,
        },
        _ => v,
    }
}

fn set(out: &mut Map<String, J>, k: String, v: J) {
    if !v.is_null() {
        out.insert(k, tidy(v));
    }
}

/// The properties of one entity's state.
pub fn feat_state(l: &Layout, f: &Feat, st: &State, out: &mut Map<String, J>) {
    let a = st;
    let attr = |k: &str| a.attr(k).cloned().unwrap_or(J::Null);
    match f.kind {
        Kind::Simple => {
            let v = if f.prop == "voltage" && f.e.key == "battery_voltage" {
                st.state
                    .as_f64()
                    .map_or(J::Null, |v| J::from((v * 1000.0).round() as i64))
            } else {
                st.state.clone()
            };
            set(out, f.p(&f.prop), v);
        }
        Kind::Switch | Kind::Siren => set(
            out,
            f.p(&f.prop),
            on_off(&st.state).map_or(J::Null, J::from),
        ),
        Kind::Button | Kind::Action => {}
        Kind::Light => {
            set(
                out,
                f.p("state"),
                on_off(&st.state).map_or(J::Null, J::from),
            );
            set(out, f.p("brightness"), attr("brightness"));
            set(
                out,
                f.p("color_temp"),
                a.attr("color_temp_kelvin")
                    .and_then(J::as_f64)
                    .and_then(mired)
                    .map_or(J::Null, J::from),
            );
            let mut color = Map::new();
            if let Some([x, y]) = a.attr("xy_color").and_then(J::as_array).map(Vec::as_slice) {
                put(&mut color, "x", x.clone());
                put(&mut color, "y", y.clone());
            }
            if let Some([h, s]) = a.attr("hs_color").and_then(J::as_array).map(Vec::as_slice) {
                // `h`/`s` for Home Assistant's JSON light schema.
                put(&mut color, "h", tidy(h.clone()));
                put(&mut color, "hue", tidy(h.clone()));
                put(&mut color, "s", tidy(s.clone()));
                put(&mut color, "saturation", tidy(s.clone()));
            }
            if !color.is_empty() {
                out.insert(f.p("color"), J::Object(color));
            }
            set(out, f.p("color_mode"), attr("color_mode"));
        }
        Kind::Cover => {
            let s = st.state.as_str().map(|s| match s {
                "open" | "opening" => "OPEN".to_string(),
                "closed" | "closing" => "CLOSE".to_string(),
                o => o.to_uppercase(),
            });
            set(out, f.p("state"), s.map_or(J::Null, J::from));
            set(out, f.p("position"), attr("current_position"));
            set(out, f.p("tilt"), attr("current_tilt_position"));
        }
        Kind::Climate => {
            set(out, f.p("system_mode"), st.state.clone());
            set(out, f.p("local_temperature"), attr("current_temperature"));
            set(out, f.p(l.setpoint), attr("target_temperature"));
            let run = a.attr("hvac_action").and_then(J::as_str).map(|s| match s {
                "heating" | "preheating" => "heat",
                "cooling" => "cool",
                "fan" => "fan_only",
                "drying" => "dry",
                _ => "idle",
            });
            set(out, f.p("running_state"), run.map_or(J::Null, J::from));
            set(out, f.p("preset"), attr("preset"));
            set(out, f.p("fan_mode"), attr("fan_mode"));
        }
        Kind::Lock => {
            let (s, ls) = match st.state.as_str() {
                Some("locked" | "locking") => (J::from("LOCK"), J::from("locked")),
                Some("unlocked" | "unlocking" | "open") => (J::from("UNLOCK"), J::from("unlocked")),
                Some("jammed") => (J::Null, J::from("not_fully_locked")),
                _ => (J::Null, J::Null),
            };
            set(out, f.p("state"), s);
            set(out, f.p("lock_state"), ls);
        }
        Kind::Fan => {
            set(
                out,
                f.p("fan_state"),
                on_off(&st.state).map_or(J::Null, J::from),
            );
            let mode = a
                .attr("preset_mode")
                .and_then(J::as_str)
                .map(str::to_string)
                .or_else(|| {
                    a.attr("percentage").and_then(J::as_f64).map(|p| {
                        match p {
                            p if p <= 0.0 => "off",
                            p if p <= 34.0 => "low",
                            p if p <= 67.0 => "medium",
                            _ => "high",
                        }
                        .to_string()
                    })
                });
            set(out, f.p("fan_mode"), mode.map_or(J::Null, J::from));
        }
    }
}

/// The full state of a device (`<base>/<friendly_name>`): every entity's
/// properties plus `linkquality`; unknown values are left out.
pub fn device_state(l: &Layout) -> Map<String, J> {
    let mut out = Map::new();
    for f in &l.feats {
        if let Some(st) = &f.e.st {
            feat_state(l, f, st, &mut out);
        }
    }
    if let Some(q) = l.dev.lqi {
        put(&mut out, "linkquality", q.into());
    }
    out
}

/// The `action` value of an event of entity `f`.
pub fn action_value(f: &Feat, event_type: &str) -> String {
    match &f.suffix {
        Some(s) => format!("{event_type}_{s}"),
        None => event_type.to_string(),
    }
}

/// The light state of one light entity without suffixes (the per-endpoint
/// topic `<base>/<name>/<endpoint>` of multi-endpoint lights, for Home
/// Assistant's JSON light schema).
pub fn light_state(l: &Layout, f: &Feat, st: &State) -> Map<String, J> {
    let plain = Feat {
        suffix: None,
        ..f.clone()
    };
    let mut out = Map::new();
    feat_state(l, &plain, st, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Exposes

/// `Local temperature` from `local_temperature`.
pub fn label(name: &str) -> String {
    let s = name.replace('_', " ");
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => s,
    }
}

fn base(ty: &str, name: &str, prop: &str, acc: u8) -> Map<String, J> {
    let mut m = Map::new();
    put(&mut m, "type", ty.into());
    put(&mut m, "name", name.into());
    put(&mut m, "label", label(name).into());
    put(&mut m, "property", prop.into());
    put(&mut m, "access", acc.into());
    m
}

fn numeric(
    name: &str,
    prop: &str,
    acc: u8,
    unit: Option<&str>,
    min: Option<f64>,
    max: Option<f64>,
    step: Option<f64>,
) -> Map<String, J> {
    let mut m = base("numeric", name, prop, acc);
    if let Some(u) = unit {
        put(&mut m, "unit", u.into());
    }
    let num = |x: f64| {
        if x.fract() == 0.0 && x.abs() < 1e15 {
            J::from(x as i64)
        } else {
            J::from(x)
        }
    };
    for (k, v) in [("value_min", min), ("value_max", max), ("value_step", step)] {
        if let Some(v) = v {
            m.insert(k.into(), num(v));
        }
    }
    m
}

fn binary(name: &str, prop: &str, acc: u8, on: J, off: J) -> Map<String, J> {
    let mut m = base("binary", name, prop, acc);
    put(&mut m, "value_on", on);
    put(&mut m, "value_off", off);
    m
}

fn enumeration(name: &str, prop: &str, acc: u8, values: Vec<String>) -> Map<String, J> {
    let mut m = base("enum", name, prop, acc);
    put(&mut m, "values", values.into());
    m
}

fn finish(mut m: Map<String, J>, f: &Feat) -> J {
    if let Some(s) = &f.suffix {
        put(&mut m, "endpoint", s.clone().into());
    }
    if let Some(c @ ("config" | "diagnostic")) = f.e.category.as_deref() {
        put(&mut m, "category", c.into());
    }
    J::Object(m)
}

fn composite(ty: &str, f: &Feat, features: Vec<Map<String, J>>) -> J {
    let mut m = Map::new();
    put(&mut m, "type", ty.into());
    m.insert(
        "features".into(),
        J::Array(
            features
                .into_iter()
                .map(|mut x| {
                    if let Some(s) = &f.suffix {
                        put(&mut x, "endpoint", s.clone().into());
                    }
                    J::Object(x)
                })
                .collect(),
        ),
    );
    finish(m, f)
}

/// The exposes of one entity (none for `action` entities: see
/// [`exposes`]).
pub fn feat_exposes(l: &Layout, f: &Feat) -> Vec<J> {
    let e = f.e;
    let get = l.get_bit();
    let rw = access::STATE | access::SET | get;
    let ro = access::STATE | get;
    let p = |b: &str| f.p(b);
    let one = |x: Map<String, J>| vec![finish(x, f)];
    match f.kind {
        Kind::Simple => {
            let prop = p(&f.prop);
            let writable = matches!(
                e.platform,
                Platform::Number | Platform::Select | Platform::Text
            );
            let acc = if writable { rw } else { ro };
            let options = e.cap_list("options");
            match e.platform {
                Platform::BinarySensor => {
                    one(binary(&f.prop, &prop, acc, J::Bool(true), J::Bool(false)))
                }
                Platform::Select => one(enumeration(&f.prop, &prop, acc, options)),
                Platform::Text => one(base("text", &f.prop, &prop, acc)),
                Platform::Number => one(numeric(
                    &f.prop,
                    &prop,
                    acc,
                    e.unit.as_deref(),
                    e.cap_f64("native_min_value"),
                    e.cap_f64("native_max_value"),
                    e.cap_f64("native_step"),
                )),
                _ if !options.is_empty() => one(enumeration(&f.prop, &prop, acc, options)),
                _ => {
                    let numeric_state = e.unit.is_some()
                        || e.state_class.is_some()
                        || e.st
                            .as_ref()
                            .is_none_or(|s| s.state.is_number() || s.state.is_null());
                    if !numeric_state || e.device_class.as_deref() == Some("enum") {
                        return one(base("text", &f.prop, &prop, acc));
                    }
                    let (unit, min, max) = match (f.prop.as_str(), e.unit.as_deref()) {
                        ("voltage", Some("V")) if e.key == "battery_voltage" => {
                            (Some("mV"), None, None)
                        }
                        ("battery", u) => (u, Some(0.0), Some(100.0)),
                        (_, u) => (u, None, None),
                    };
                    one(numeric(&f.prop, &prop, acc, unit, min, max, None))
                }
            }
        }
        Kind::Switch => {
            let mut b = binary("state", &p(&f.prop), rw, "ON".into(), "OFF".into());
            if f.prop == "state" {
                put(&mut b, "name", "state".into());
                put(&mut b, "value_toggle", "TOGGLE".into());
                vec![composite("switch", f, vec![b])]
            } else {
                put(&mut b, "name", f.prop.clone().into());
                put(&mut b, "label", label(&f.prop).into());
                put(&mut b, "value_toggle", "TOGGLE".into());
                one(b)
            }
        }
        Kind::Siren => one(binary(
            &f.prop,
            &p(&f.prop),
            access::STATE | access::SET,
            "ON".into(),
            "OFF".into(),
        )),
        Kind::Button => one(enumeration(
            &f.prop,
            &p(&f.prop),
            access::SET,
            vec![f.prop.clone()],
        )),
        Kind::Action => Vec::new(),
        Kind::Light => {
            let modes = e.cap_list("supported_color_modes");
            let mut fs = Vec::new();
            let mut st = binary("state", &p("state"), rw, "ON".into(), "OFF".into());
            put(&mut st, "value_toggle", "TOGGLE".into());
            fs.push(st);
            if modes.iter().any(|m| m != "onoff") {
                fs.push(numeric(
                    "brightness",
                    &p("brightness"),
                    rw,
                    None,
                    Some(0.0),
                    Some(254.0),
                    None,
                ));
            }
            if modes.iter().any(|m| m == "color_temp") {
                let lo = e
                    .cap_f64("max_color_temp_kelvin")
                    .and_then(mired)
                    .unwrap_or(150);
                let hi = e
                    .cap_f64("min_color_temp_kelvin")
                    .and_then(mired)
                    .unwrap_or(500);
                fs.push(numeric(
                    "color_temp",
                    &p("color_temp"),
                    rw,
                    Some("mired"),
                    Some(lo as f64),
                    Some(hi as f64),
                    None,
                ));
            }
            let comp = |name: &str, lbl: &str, parts: [(&str, &str); 2]| {
                let mut c = base("composite", name, &p("color"), rw);
                put(&mut c, "label", lbl.into());
                c.insert(
                    "features".into(),
                    J::Array(
                        parts
                            .iter()
                            .map(|(n, _)| J::Object(numeric(n, n, rw, None, None, None, None)))
                            .collect(),
                    ),
                );
                c
            };
            if modes.iter().any(|m| m == "xy") {
                fs.push(comp("color_xy", "Color (X/Y)", [("x", ""), ("y", "")]));
            }
            if modes.iter().any(|m| m == "hs") {
                fs.push(comp(
                    "color_hs",
                    "Color (HS)",
                    [("hue", ""), ("saturation", "")],
                ));
            }
            vec![composite("light", f, fs)]
        }
        Kind::Cover => {
            let sf = e.cap_list("supported_features");
            let mut fs = vec![enumeration(
                "state",
                &p("state"),
                access::STATE | access::SET,
                ["OPEN", "CLOSE", "STOP"].map(String::from).to_vec(),
            )];
            if sf.iter().any(|x| x == "set_position") {
                fs.push(numeric(
                    "position",
                    &p("position"),
                    rw,
                    Some("%"),
                    Some(0.0),
                    Some(100.0),
                    None,
                ));
            }
            if sf.iter().any(|x| x == "set_tilt_position") {
                fs.push(numeric(
                    "tilt",
                    &p("tilt"),
                    rw,
                    Some("%"),
                    Some(0.0),
                    Some(100.0),
                    None,
                ));
            }
            vec![composite("cover", f, fs)]
        }
        Kind::Climate => {
            let unit = Some("°C");
            let mut fs = vec![numeric(
                "local_temperature",
                &p("local_temperature"),
                ro,
                unit,
                None,
                None,
                None,
            )];
            fs.push(numeric(
                l.setpoint,
                &p(l.setpoint),
                rw,
                unit,
                Some(e.cap_f64("min_temp").unwrap_or(7.0)),
                Some(e.cap_f64("max_temp").unwrap_or(35.0)),
                Some(e.cap_f64("target_temperature_step").unwrap_or(0.5)),
            ));
            let modes = e.cap_list("hvac_modes");
            if !modes.is_empty() {
                fs.push(enumeration("system_mode", &p("system_mode"), rw, modes));
            }
            fs.push(enumeration(
                "running_state",
                &p("running_state"),
                ro,
                ["idle", "heat", "cool", "fan_only"]
                    .map(String::from)
                    .to_vec(),
            ));
            for (cap, name) in [("preset_modes", "preset"), ("fan_modes", "fan_mode")] {
                let v = e.cap_list(cap);
                if !v.is_empty() {
                    fs.push(enumeration(name, &p(name), rw, v));
                }
            }
            vec![composite("climate", f, fs)]
        }
        Kind::Lock => vec![composite(
            "lock",
            f,
            vec![
                binary("state", &p("state"), rw, "LOCK".into(), "UNLOCK".into()),
                enumeration(
                    "lock_state",
                    &p("lock_state"),
                    ro,
                    ["not_fully_locked", "locked", "unlocked"]
                        .map(String::from)
                        .to_vec(),
                ),
            ],
        )],
        Kind::Fan => {
            let mut st = binary("state", &p("fan_state"), rw, "ON".into(), "OFF".into());
            put(&mut st, "label", "State".into());
            let mut mode = enumeration(
                "mode",
                &p("fan_mode"),
                rw,
                ["off", "low", "medium", "high", "on", "auto", "smart"]
                    .map(String::from)
                    .to_vec(),
            );
            put(&mut mode, "label", "Mode".into());
            vec![composite("fan", f, vec![st, mode])]
        }
    }
}

/// The `action` values of a device (every event entity, suffixed when there
/// are several).
pub fn action_values(l: &Layout) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for f in l.feats.iter().filter(|f| f.kind == Kind::Action) {
        for t in f.e.cap_list("event_types") {
            let a = action_value(f, &t);
            if !v.contains(&a) {
                v.push(a);
            }
        }
    }
    v
}

/// The `linkquality` expose every Zigbee2MQTT device has.
pub fn linkquality_expose() -> J {
    let mut m = numeric(
        "linkquality",
        "linkquality",
        access::STATE,
        Some("lqi"),
        Some(0.0),
        Some(255.0),
        None,
    );
    put(
        &mut m,
        "description",
        "Link quality (signal strength)".into(),
    );
    put(&mut m, "category", "diagnostic".into());
    J::Object(m)
}

/// All exposes of a device, in entity order, `action` and `linkquality`
/// last.
pub fn exposes(l: &Layout) -> Vec<J> {
    let mut out: Vec<J> = Vec::new();
    for f in &l.feats {
        out.extend(feat_exposes(l, f));
    }
    let actions = action_values(l);
    if l.feats.iter().any(|f| f.kind == Kind::Action) {
        let mut a = enumeration("action", "action", access::STATE, actions);
        put(
            &mut a,
            "description",
            "Triggered action (e.g. a button click)".into(),
        );
        out.push(J::Object(a));
    }
    if !l.dev.is_gp() {
        out.push(linkquality_expose());
    }
    out
}

// ---------------------------------------------------------------------------
// bridge/devices, bridge/event

/// Zigbee2MQTT's device `type`.
pub fn device_type(d: &Device) -> &'static str {
    match (d.is_gp(), d.power.as_str()) {
        (true, _) => "GreenPower",
        (_, "mains") => "Router",
        _ => "EndDevice",
    }
}

/// The `definition` of a device (`null` while it has no entities yet).
pub fn definition(l: &Layout) -> J {
    let d = l.dev;
    if d.interview == "in_progress" && l.feats.is_empty() {
        return J::Null;
    }
    let model = d
        .definition_model
        .clone()
        .or_else(|| d.model.clone())
        .or_else(|| d.model_id.clone())
        .unwrap_or_default();
    let vendor = d.manufacturer.clone().unwrap_or_default();
    let (description, source) = if d.has_definition {
        (format!("{vendor} {model}").trim().to_string(), "native")
    } else {
        (
            "Automatically generated definition".to_string(),
            "generated",
        )
    };
    Obj::new()
        .with("model", model)
        .with("vendor", vendor)
        .with("description", description)
        .with("exposes", exposes(l))
        .with("options", J::Array(Vec::new()))
        .with("supports_ota", false)
        .with("source", source)
        .j()
}

/// One `bridge/devices` entry.
pub fn bridge_device(l: &Layout, friendly: &str) -> J {
    let d = l.dev;
    let done = d.interview != "in_progress";
    Obj::new()
        .with("ieee_address", d.ieee_address())
        .with("type", device_type(d))
        .with("network_address", d.nwk)
        .with("supported", d.has_definition)
        .with("friendly_name", friendly)
        .with("disabled", d.disabled)
        .with("description", J::Null)
        .with("definition", definition(l))
        .with(
            "power_source",
            match d.power.as_str() {
                "mains" => "Mains (single phase)",
                "battery" => "Battery",
                _ => "Unknown",
            },
        )
        .with("model_id", d.model_id.clone())
        .with("manufacturer", d.manufacturer.clone())
        .with("software_build_id", d.sw_version.clone())
        .with("date_code", J::Null)
        .with("interview_completed", done)
        .with("interviewing", !done)
        .with(
            "interview_state",
            if done { "SUCCESSFUL" } else { "IN_PROGRESS" },
        )
        .with("endpoints", p0())
        .j()
}

/// The coordinator's `bridge/devices` entry.
pub fn coordinator_device(ieee: &str) -> J {
    Obj::new()
        .with("ieee_address", format!("0x{ieee}"))
        .with("type", "Coordinator")
        .with("network_address", 0)
        .with("supported", true)
        .with("friendly_name", "Coordinator")
        .with("disabled", false)
        .with("description", J::Null)
        .with("definition", J::Null)
        .with("power_source", "Mains (single phase)")
        .with("interview_completed", true)
        .with("interviewing", false)
        .with("interview_state", "SUCCESSFUL")
        .with("endpoints", p0())
        .j()
}

// ---------------------------------------------------------------------------
// /set and /get

/// `{}`.
fn p0() -> J {
    J::Object(Map::new())
}

/// `{k: v}`.
fn p1(k: &str, v: impl Into<J>) -> J {
    Obj::new().with(k, v).j()
}

/// A hub entity command.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// `entities/command`.
    Command {
        /// Entity.
        uid: String,
        /// HA service name.
        command: &'static str,
        /// Parameters.
        params: J,
    },
    /// `entities/refresh`.
    Refresh(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Value,
    LightState,
    Brightness,
    BrightnessStep,
    ColorTemp,
    ColorTempStep,
    Color,
    CoverState,
    Position,
    Tilt,
    SystemMode,
    Setpoint,
    Preset,
    FanModeClimate,
    LockState,
    FanState,
    FanMode,
    ReadOnly,
}

/// Property → `(feature index, role)`.
struct Roles(Vec<(String, usize, Role)>);

impl Roles {
    fn get(&self, k: &str) -> Option<(usize, Role)> {
        self.0
            .iter()
            .find(|(p, _, _)| p == k)
            .map(|(_, i, r)| (*i, *r))
    }
}

fn roles(l: &Layout) -> Roles {
    let mut r = Vec::new();
    for (i, f) in l.feats.iter().enumerate() {
        let mut add = |b: &str, role: Role| {
            r.push((f.p(b), i, role));
        };
        match f.kind {
            Kind::Simple | Kind::Switch | Kind::Button | Kind::Siren => add(&f.prop, Role::Value),
            Kind::Action => {}
            Kind::Light => {
                add("state", Role::LightState);
                add("brightness", Role::Brightness);
                add("brightness_step", Role::BrightnessStep);
                add("color_temp", Role::ColorTemp);
                add("color_temp_step", Role::ColorTempStep);
                add("color", Role::Color);
                add("color_mode", Role::ReadOnly);
            }
            Kind::Cover => {
                add("state", Role::CoverState);
                add("position", Role::Position);
                add("tilt", Role::Tilt);
            }
            Kind::Climate => {
                add("system_mode", Role::SystemMode);
                for s in [
                    "occupied_heating_setpoint",
                    "current_heating_setpoint",
                    "occupied_cooling_setpoint",
                ] {
                    add(s, Role::Setpoint);
                }
                add("preset", Role::Preset);
                add("fan_mode", Role::FanModeClimate);
                add("local_temperature", Role::ReadOnly);
                add("running_state", Role::ReadOnly);
            }
            Kind::Lock => {
                add("state", Role::LockState);
                add("lock_state", Role::ReadOnly);
            }
            Kind::Fan => {
                add("fan_state", Role::FanState);
                add("fan_mode", Role::FanMode);
            }
        }
    }
    Roles(r)
}

/// `ON` → Some(true), `OFF` → Some(false), `TOGGLE` → None (also bools).
fn switch_value(v: &J) -> Result<Option<bool>, String> {
    match v {
        J::Bool(b) => Ok(Some(*b)),
        J::String(s) if s.eq_ignore_ascii_case("on") => Ok(Some(true)),
        J::String(s) if s.eq_ignore_ascii_case("off") => Ok(Some(false)),
        J::String(s) if s.eq_ignore_ascii_case("toggle") => Ok(None),
        _ => Err(format!("{v} is not ON, OFF or TOGGLE")),
    }
}

fn number(v: &J) -> Result<f64, String> {
    match v {
        J::Number(n) => n.as_f64().ok_or_else(|| format!("{v} is not a number")),
        J::String(s) => s.trim().parse().map_err(|_| format!("{v} is not a number")),
        _ => Err(format!("{v} is not a number")),
    }
}

fn text(v: &J) -> String {
    match v {
        J::String(s) => s.clone(),
        o => o.to_string(),
    }
}

/// sRGB (0–255) to CIE 1931 xy.
pub fn rgb_to_xy(r: f64, g: f64, b: f64) -> (f64, f64) {
    let lin = |c: f64| {
        let c = (c / 255.0).clamp(0.0, 1.0);
        if c > 0.04045 {
            ((c + 0.055) / 1.055).powf(2.4)
        } else {
            c / 12.92
        }
    };
    let (r, g, b) = (lin(r), lin(g), lin(b));
    let x = r * 0.4124 + g * 0.3576 + b * 0.1805;
    let y = r * 0.2126 + g * 0.7152 + b * 0.0722;
    let z = r * 0.0193 + g * 0.1192 + b * 0.9505;
    let sum = x + y + z;
    if sum <= 0.0 {
        return (0.0, 0.0);
    }
    let round = |v: f64| (v * 10000.0).round() / 10000.0;
    (round(x / sum), round(y / sum))
}

/// A Zigbee2MQTT `color` value as HA `xy_color` / `hs_color` parameters.
fn color_params(v: &J, params: &mut Map<String, J>) -> Result<(), String> {
    let o = match v {
        J::Object(o) => o.clone(),
        J::String(s) if s.starts_with('#') => {
            let mut o = Map::new();
            put(&mut o, "hex", s.clone().into());
            o
        }
        _ => return Err(format!("color {v} is not an object")),
    };
    let n = |k: &str| o.get(k).map(number).transpose();
    if let (Some(x), Some(y)) = (n("x")?, n("y")?) {
        put(params, "xy_color", J::from(vec![x, y]));
    } else if let (Some(h), Some(s)) = (n("hue")?.or(n("h")?), n("saturation")?.or(n("s")?)) {
        put(params, "hs_color", J::from(vec![h, s]));
    } else if let Some(hex) = o.get("hex").and_then(J::as_str) {
        let h = hex.trim_start_matches('#');
        let c = |i: usize| u8::from_str_radix(h.get(i..i + 2).unwrap_or("zz"), 16).map(f64::from);
        let (Ok(r), Ok(g), Ok(b)) = (c(0), c(2), c(4)) else {
            return Err(format!("color hex {hex:?}"));
        };
        let (x, y) = rgb_to_xy(r, g, b);
        put(params, "xy_color", J::from(vec![x, y]));
    } else if let (Some(r), Some(g), Some(b)) = (n("r")?, n("g")?, n("b")?) {
        let (x, y) = rgb_to_xy(r, g, b);
        put(params, "xy_color", J::from(vec![x, y]));
    } else {
        return Err(format!("color {v}: x/y, hue/saturation, hex or r/g/b"));
    }
    Ok(())
}

#[derive(Default)]
struct LightReq {
    state: Option<Option<bool>>,
    params: Map<String, J>,
}

/// `/set` payload → commands. `ep` is the endpoint of an endpoint topic
/// (`<name>/<ep>/set`). Unknown properties and invalid values are errors
/// (the other properties of the payload still apply).
pub fn set_actions(
    l: &Layout,
    payload: &Map<String, J>,
    ep: Option<&str>,
) -> (Vec<Action>, Vec<String>) {
    let roles = roles(l);
    let mut acts = Vec::new();
    let mut errs = Vec::new();
    let mut lights: Vec<(usize, LightReq)> = Vec::new();
    let find = |k: &str| {
        ep.and_then(|ep| roles.get(&format!("{k}_{ep}")))
            .or_else(|| roles.get(k))
    };
    fn light(v: &mut Vec<(usize, LightReq)>, i: usize) -> &mut LightReq {
        let at = match v.iter().position(|(j, _)| *j == i) {
            Some(at) => at,
            None => {
                v.push((i, LightReq::default()));
                v.len() - 1
            }
        };
        &mut v[at].1
    }
    // `transition` is a light option when the payload sets a light, else a
    // property of its own (the generic `transition` number).
    let light_keys = payload
        .keys()
        .any(|k| find(k).is_some_and(|(i, _)| l.feats[i].kind == Kind::Light));
    let transition = payload
        .get("transition")
        .filter(|_| light_keys)
        .map(number)
        .transpose();
    let transition = match transition {
        Ok(t) => t,
        Err(e) => {
            errs.push(format!("transition: {e}"));
            None
        }
    };
    let cmd = |uid: &str, command: &'static str, params: J| Action::Command {
        uid: uid.to_string(),
        command,
        params,
    };
    for (k, v) in payload {
        if k == "transition" && light_keys {
            continue;
        }
        let Some((i, role)) = find(k) else {
            errs.push(format!("no property {k:?}"));
            continue;
        };
        let f = &l.feats[i];
        let uid = f.e.uid.as_str();
        let cur = f.e.st.as_ref();
        let r: Result<(), String> = (|| {
            match role {
                Role::ReadOnly => return Err(format!("{k} is read-only")),
                Role::Value => match (f.kind, f.e.platform) {
                    (Kind::Switch, _) | (Kind::Siren, _) => {
                        let c = match switch_value(v)? {
                            Some(true) => "turn_on",
                            Some(false) => "turn_off",
                            None if f.kind == Kind::Switch => "toggle",
                            None => {
                                let on = cur.and_then(|s| on_off(&s.state)) == Some("ON");
                                if on { "turn_off" } else { "turn_on" }
                            }
                        };
                        acts.push(cmd(uid, c, p0()));
                    }
                    (Kind::Button, _) => acts.push(cmd(uid, "press", p0())),
                    (_, Platform::Number) => {
                        acts.push(cmd(uid, "set_value", p1("value", number(v)?)))
                    }
                    (_, Platform::Text) => acts.push(cmd(uid, "set_value", p1("value", text(v)))),
                    (_, Platform::Select) => {
                        acts.push(cmd(uid, "select_option", p1("option", text(v))))
                    }
                    _ => return Err(format!("{k} is read-only")),
                },
                Role::LightState => light(&mut lights, i).state = Some(switch_value(v)?),
                Role::Brightness => {
                    let b = number(v)?;
                    let r = light(&mut lights, i);
                    if b <= 0.0 {
                        r.state = Some(Some(false));
                    } else {
                        put(
                            &mut r.params,
                            "brightness",
                            J::from(b.round().clamp(1.0, 254.0) as i64),
                        );
                    }
                }
                Role::BrightnessStep => {
                    let now = cur
                        .and_then(|s| s.attr("brightness"))
                        .and_then(J::as_f64)
                        .unwrap_or(0.0);
                    let b = (now + number(v)?).clamp(0.0, 254.0);
                    let r = light(&mut lights, i);
                    if b <= 0.0 {
                        r.state = Some(Some(false));
                    } else {
                        put(&mut r.params, "brightness", J::from(b.round() as i64));
                    }
                }
                Role::ColorTemp | Role::ColorTempStep => {
                    let mut mr = number(v)?;
                    if role == Role::ColorTempStep {
                        let now = cur
                            .and_then(|s| s.attr("color_temp_kelvin"))
                            .and_then(J::as_f64)
                            .and_then(mired);
                        mr += now.ok_or("color_temp_step: color temperature unknown")? as f64;
                    }
                    if mr <= 0.0 {
                        return Err(format!("{k}: {mr} mired"));
                    }
                    put(
                        &mut light(&mut lights, i).params,
                        "color_temp_kelvin",
                        J::from((1_000_000.0 / mr).round() as i64),
                    );
                }
                Role::Color => color_params(v, &mut light(&mut lights, i).params)?,
                Role::CoverState => {
                    let s = text(v).to_ascii_lowercase();
                    let valve = f.e.platform == Platform::Valve;
                    let c = match (s.as_str(), valve) {
                        ("open", false) => "open_cover",
                        ("close", false) => "close_cover",
                        ("stop", false) => "stop_cover",
                        ("open" | "on", true) => "open_valve",
                        ("close" | "off", true) => "close_valve",
                        ("stop", true) => "stop_valve",
                        _ => return Err(format!("{k}: OPEN, CLOSE or STOP")),
                    };
                    acts.push(cmd(uid, c, p0()));
                }
                Role::Position => {
                    let p = number(v)?;
                    if f.e.platform == Platform::Valve {
                        acts.push(cmd(uid, "set_valve_position", p1("position", p)));
                    } else {
                        acts.push(cmd(uid, "set_cover_position", p1("position", p)));
                    }
                }
                Role::Tilt => acts.push(cmd(
                    uid,
                    "set_cover_tilt_position",
                    p1("tilt_position", number(v)?),
                )),
                Role::SystemMode => acts.push(cmd(uid, "set_hvac_mode", p1("hvac_mode", text(v)))),
                Role::Setpoint => {
                    acts.push(cmd(uid, "set_temperature", p1("temperature", number(v)?)))
                }
                Role::Preset => acts.push(cmd(uid, "set_preset_mode", p1("preset_mode", text(v)))),
                Role::FanModeClimate => {
                    acts.push(cmd(uid, "set_fan_mode", p1("fan_mode", text(v))))
                }
                Role::LockState => {
                    let c = match text(v).to_ascii_uppercase().as_str() {
                        "LOCK" => "lock",
                        "UNLOCK" => "unlock",
                        _ => return Err(format!("{k}: LOCK or UNLOCK")),
                    };
                    acts.push(cmd(uid, c, p0()));
                }
                Role::FanState => {
                    let c = match switch_value(v)? {
                        Some(true) => "turn_on",
                        Some(false) => "turn_off",
                        None => {
                            if cur.and_then(|s| on_off(&s.state)) == Some("ON") {
                                "turn_off"
                            } else {
                                "turn_on"
                            }
                        }
                    };
                    acts.push(cmd(uid, c, p0()));
                }
                Role::FanMode => {
                    let mode = text(v).to_ascii_lowercase();
                    acts.push(match mode.as_str() {
                        "off" => cmd(uid, "turn_off", p0()),
                        "on" => cmd(uid, "turn_on", p0()),
                        "low" => cmd(uid, "set_percentage", p1("percentage", 33)),
                        "medium" => cmd(uid, "set_percentage", p1("percentage", 66)),
                        "high" => cmd(uid, "set_percentage", p1("percentage", 100)),
                        _ => cmd(uid, "set_preset_mode", p1("preset_mode", mode)),
                    });
                }
            }
            Ok(())
        })();
        if let Err(e) = r {
            errs.push(e);
        }
    }
    for (i, mut r) in lights {
        let uid = l.feats[i].e.uid.as_str();
        if let Some(t) = transition {
            put(&mut r.params, "transition", J::from(t));
        }
        let c = match r.state {
            Some(Some(false)) => {
                r.params.retain(|k, _| k == "transition");
                "turn_off"
            }
            Some(None) if r.params.keys().all(|k| k == "transition") => "toggle",
            _ => "turn_on",
        };
        acts.push(cmd(uid, c, J::Object(r.params)));
    }
    (acts, errs)
}

/// `/get` payload → reads (one per entity).
pub fn get_actions(
    l: &Layout,
    payload: &Map<String, J>,
    ep: Option<&str>,
) -> (Vec<Action>, Vec<String>) {
    let roles = roles(l);
    let mut uids: Vec<String> = Vec::new();
    let mut errs = Vec::new();
    for k in payload.keys() {
        let hit = ep
            .and_then(|ep| roles.get(&format!("{k}_{ep}")))
            .or_else(|| roles.get(k.as_str()));
        match hit {
            Some((i, _)) => {
                let u = &l.feats[i].e.uid;
                if !uids.contains(u) {
                    uids.push(u.clone());
                }
            }
            None if k == "linkquality" => {}
            None => errs.push(format!("no property {k:?}")),
        }
    }
    (uids.into_iter().map(Action::Refresh).collect(), errs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, name: &str) -> Device {
        Device {
            id: id.into(),
            ieee: Some(id[3..].into()),
            name: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn names_are_sanitized_and_unique() {
        assert_eq!(sanitize(" Lamp #1 "), "Lamp _1");
        assert_eq!(sanitize("kitchen/lamp"), "kitchen/lamp");
        assert_eq!(sanitize("a/b/c"), "a_b_c");
        assert_eq!(sanitize("lamp/2"), "lamp_2");
        assert_eq!(sanitize("lamp/set"), "lamp_set");
        assert_eq!(sanitize("bridge"), "bridge_");
        assert_eq!(sanitize("/x//y/"), "x/y");
        assert!(check_name("kitchen/lamp").is_ok());
        assert!(
            check_name("lamp/1").is_err() && check_name("a+b").is_err() && check_name(" ").is_err()
        );

        let mut devs = BTreeMap::new();
        devs.insert(
            "zb:00158d0000000002".to_string(),
            Box::new(dev("zb:00158d0000000002", "TS0201")),
        );
        let mut n = Names::default();
        let ch = n.assign(&devs);
        assert_eq!(
            ch,
            vec![("zb:00158d0000000002".into(), None, Some("TS0201".into()))]
        );
        // A second device of the same model sorts first but does not take
        // the name the first one has.
        devs.insert(
            "zb:00158d0000000001".to_string(),
            Box::new(dev("zb:00158d0000000001", "TS0201")),
        );
        let ch = n.assign(&devs);
        assert_eq!(
            ch,
            vec![(
                "zb:00158d0000000001".into(),
                None,
                Some("TS0201_0001".into())
            )]
        );
        assert_eq!(n.lookup("TS0201"), Some("zb:00158d0000000002"));
        assert_eq!(n.lookup("0x00158d0000000001"), Some("zb:00158d0000000001"));
        // Renamed: the new name, the old one is free again.
        devs.get_mut("zb:00158d0000000002").unwrap().name = "kitchen".into();
        let ch = n.assign(&devs);
        assert_eq!(
            ch,
            vec![(
                "zb:00158d0000000002".into(),
                Some("TS0201".into()),
                Some("kitchen".into())
            )]
        );
        assert!(n.assign(&devs).is_empty(), "stable");
        devs.remove("zb:00158d0000000001");
        assert_eq!(
            n.assign(&devs),
            vec![(
                "zb:00158d0000000001".into(),
                Some("TS0201_0001".into()),
                None
            )]
        );
    }

    #[test]
    fn rgb() {
        assert_eq!(rgb_to_xy(255.0, 0.0, 0.0), (0.6401, 0.33));
        assert_eq!(rgb_to_xy(0.0, 0.0, 0.0), (0.0, 0.0));
    }
}
