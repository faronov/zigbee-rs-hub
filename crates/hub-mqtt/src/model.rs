//! The bridge's copy of the hub's devices, entities and states, built from
//! the WebSocket JSON API records (device-support 4.1, 4.2, 4.5): only the
//! fields the Zigbee2MQTT mapping needs, so a large network costs little
//! RAM. The subscription snapshot is split into its records without parsing
//! it as one JSON tree ([`array_items`]).

use hub_vecmap::VecMap;
use serde_json::{Map, Value as J};

/// HA platform of an entity (4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum Platform {
    Sensor,
    BinarySensor,
    Number,
    Select,
    Switch,
    Button,
    Text,
    Light,
    Cover,
    Climate,
    Lock,
    Fan,
    Event,
    Siren,
    Valve,
    Other,
}

impl Platform {
    /// From the record's `platform`.
    pub fn parse(s: &str) -> Self {
        match s {
            "sensor" => Self::Sensor,
            "binary_sensor" => Self::BinarySensor,
            "number" => Self::Number,
            "select" => Self::Select,
            "switch" => Self::Switch,
            "button" => Self::Button,
            "text" => Self::Text,
            "light" => Self::Light,
            "cover" => Self::Cover,
            "climate" => Self::Climate,
            "lock" => Self::Lock,
            "fan" => Self::Fan,
            "event" => Self::Event,
            "siren" => Self::Siren,
            "valve" => Self::Valve,
            _ => Self::Other,
        }
    }
}

fn s(v: &J, k: &str) -> Option<String> {
    v.get(k).and_then(J::as_str).map(str::to_string)
}

/// A device (4.1).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Device {
    /// `zb:<ieee>` or `gp:…`.
    pub id: String,
    /// 16 hex digits (Zigbee).
    pub ieee: Option<String>,
    /// 8 hex digits (Green Power).
    pub src_id: Option<String>,
    /// NWK address.
    pub nwk: Option<u16>,
    /// User name, else model.
    pub name: String,
    /// Manufacturer (definition vendor or Basic).
    pub manufacturer: Option<String>,
    /// Model (definition model or Basic).
    pub model: Option<String>,
    /// Basic model identifier.
    pub model_id: Option<String>,
    /// The resolved definition's model.
    pub definition_model: Option<String>,
    /// Basic SW build ID.
    pub sw_version: Option<String>,
    /// `mains`, `battery`, `energy_harvesting`.
    pub power: String,
    /// Available.
    pub available: bool,
    /// Last LQI.
    pub lqi: Option<u8>,
    /// `complete`, `partial`, `in_progress`.
    pub interview: String,
    /// Disabled by the user.
    pub disabled: bool,
    /// A definition beyond the generic tier resolved.
    pub has_definition: bool,
}

impl Device {
    /// From a device record.
    pub fn from_record(v: &J) -> Option<Self> {
        let layers = v.get("definition").and_then(J::as_array);
        Some(Self {
            id: s(v, "id")?,
            ieee: s(v, "ieee"),
            src_id: s(v, "src_id"),
            nwk: v.get("nwk").and_then(J::as_u64).map(|n| n as u16),
            name: s(v, "name").unwrap_or_default(),
            manufacturer: s(v, "manufacturer"),
            model: s(v, "model"),
            model_id: s(v, "model_id"),
            definition_model: s(v, "definition_model"),
            sw_version: s(v, "sw_version"),
            power: s(v, "power").unwrap_or_else(|| "mains".into()),
            available: v.get("available").and_then(J::as_bool).unwrap_or(true),
            lqi: v.get("lqi").and_then(J::as_u64).map(|n| n.min(255) as u8),
            interview: s(v, "interview").unwrap_or_else(|| "complete".into()),
            disabled: v.get("disabled").and_then(J::as_bool).unwrap_or(false),
            has_definition: layers.is_some_and(|l| {
                l.iter().any(|x| {
                    x.get("source")
                        .and_then(J::as_str)
                        .is_some_and(|s| s != "generic")
                })
            }),
        })
    }

    /// `0x` + 16 hex digits: the IEEE, or the GP source ID in the low
    /// 32 bits (as Zigbee2MQTT shows Green Power devices).
    pub fn ieee_address(&self) -> String {
        match (&self.ieee, &self.src_id) {
            (Some(i), _) => format!("0x{i}"),
            (None, Some(s)) => format!("0x00000000{s:0>8}"),
            _ => format!("0x{}", self.id.replace([':', '/'], "")),
        }
    }

    /// Green Power device.
    pub fn is_gp(&self) -> bool {
        self.id.starts_with("gp:")
    }

    /// Battery or energy harvesting.
    pub fn sleepy(&self) -> bool {
        self.power != "mains"
    }
}

/// An entity (4.2).
#[derive(Debug, Clone, PartialEq)]
pub struct Entity {
    /// `<device>:<ep>:<key>`.
    pub uid: String,
    /// Endpoint.
    pub ep: u8,
    /// Key.
    pub key: String,
    /// Platform.
    pub platform: Platform,
    /// HA device class.
    pub device_class: Option<String>,
    /// HA unit.
    pub unit: Option<String>,
    /// HA state class.
    pub state_class: Option<String>,
    /// `config`, `diagnostic`.
    pub category: Option<String>,
    /// Enabled by default.
    pub enabled_default: bool,
    /// Display name (`None`: derived).
    pub name: Option<String>,
    /// Platform capabilities (options, limits, modes …) as JSON text: read
    /// only for exposes and discovery, and a map node per entity costs
    /// more than the text.
    caps: Option<Box<str>>,
    /// Orphaned (no binding any more).
    pub orphaned: bool,
    /// The last state.
    pub st: Option<State>,
}

impl Entity {
    /// From an entity record (its `uid` must start with its `device`).
    pub fn from_record(v: &J) -> Option<Self> {
        let dev = v.get("device").and_then(J::as_str)?;
        Some(Self {
            uid: s(v, "uid").filter(|u| u.starts_with(dev))?,
            ep: v.get("endpoint").and_then(J::as_u64).unwrap_or(1) as u8,
            key: s(v, "key")?,
            platform: Platform::parse(v.get("platform").and_then(J::as_str).unwrap_or("")),
            device_class: s(v, "device_class"),
            unit: s(v, "unit"),
            state_class: s(v, "state_class"),
            category: s(v, "category"),
            enabled_default: v
                .get("enabled_default")
                .and_then(J::as_bool)
                .unwrap_or(true),
            name: s(v, "name"),
            caps: v
                .get("capabilities")
                .filter(|c| c.as_object().is_some_and(|o| !o.is_empty()))
                .map(|c| c.to_string().into_boxed_str()),
            orphaned: v.get("orphaned").and_then(J::as_bool).unwrap_or(false),
            st: None,
        })
    }

    /// The device ID (the UID without `:<ep>:<key>`).
    pub fn device(&self) -> &str {
        let mut it = self.uid.rsplitn(3, ':');
        let _ = (it.next(), it.next());
        it.next().unwrap_or("")
    }

    /// A capability.
    pub fn cap(&self, k: &str) -> Option<J> {
        let mut m: Map<String, J> = serde_json::from_str(self.caps.as_deref()?).ok()?;
        m.remove(k)
    }

    /// A capability as a list of strings.
    pub fn cap_list(&self, k: &str) -> Vec<String> {
        match self.cap(k) {
            Some(J::Array(a)) => a
                .into_iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// A numeric capability.
    pub fn cap_f64(&self, k: &str) -> Option<f64> {
        self.cap(k).and_then(|v| v.as_f64())
    }
}

/// An entity's state (4.5): HA state and attributes (a few pairs at most:
/// a list costs less than a map).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    /// State.
    pub state: J,
    /// Attributes.
    pub attrs: Vec<(String, J)>,
}

impl State {
    /// From a state message; `None` without `uid`.
    pub fn from_msg(v: &J) -> Option<(String, Self, Option<bool>)> {
        let uid = s(v, "uid")?;
        let attrs = match v.get("attributes") {
            Some(J::Object(o)) => o.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            _ => Vec::new(),
        };
        let st = Self {
            state: v.get("state").cloned().unwrap_or(J::Null),
            attrs,
        };
        Some((uid, st, v.get("available").and_then(J::as_bool)))
    }

    /// An attribute.
    pub fn attr(&self, k: &str) -> Option<&J> {
        self.attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v)
    }
}

/// Devices, entities and states.
#[derive(Debug, Default)]
pub struct Model {
    /// Devices by ID (boxed: inserts move the later entries).
    pub devices: VecMap<String, Box<Device>>,
    /// Entities by UID, with their states.
    pub entities: VecMap<String, Box<Entity>>,
}

impl Model {
    /// The entities of a device, in UID order.
    pub fn entities_of<'a>(&'a self, dev: &'a str) -> impl Iterator<Item = &'a Entity> + 'a {
        let from = format!("{dev}:");
        self.entities
            .range(from.clone()..)
            .take_while(move |(k, _)| k.starts_with(&from))
            .map(|(_, e)| &**e)
            .filter(move |e| e.device() == dev)
    }

    /// Removes a device and its entities and states.
    pub fn remove_device(&mut self, id: &str) -> Option<Box<Device>> {
        let uids: Vec<String> = self.entities_of(id).map(|e| e.uid.clone()).collect();
        for u in uids {
            self.entities.remove(&u);
        }
        self.devices.remove(id)
    }
}

/// The text of each element of the JSON array that is the value of `key` in
/// the object `text` (at any depth, first occurrence), without building the
/// whole tree. `None` if the key is absent.
pub fn array_items<'a>(text: &'a str, key: &str) -> Option<Vec<&'a str>> {
    let start = value_start(text, key)?;
    let b = text.as_bytes();
    if b.get(start) != Some(&b'[') {
        return None;
    }
    let mut items = Vec::new();
    let mut i = start + 1;
    let mut depth = 0usize;
    let mut item_start = None;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            match c {
                b'\\' => i += 1,
                b'"' => in_str = false,
                _ => {}
            }
        } else {
            match c {
                b'"' => {
                    in_str = true;
                    item_start.get_or_insert(i);
                }
                b'{' | b'[' => {
                    item_start.get_or_insert(i);
                    depth += 1;
                }
                b'}' | b']' if depth > 0 => depth -= 1,
                b']' => {
                    if let Some(s) = item_start {
                        items.push(text[s..i].trim());
                    }
                    return Some(items);
                }
                b',' if depth == 0 => {
                    if let Some(s) = item_start.take() {
                        items.push(text[s..i].trim());
                    }
                }
                b' ' | b'\n' | b'\r' | b'\t' => {}
                _ => {
                    item_start.get_or_insert(i);
                }
            }
        }
        i += 1;
    }
    None
}

/// Index of the value of `"key":` in `text` (first occurrence outside
/// strings).
fn value_start(text: &str, key: &str) -> Option<usize> {
    let pat = format!("\"{key}\"");
    let b = text.as_bytes();
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            match c {
                b'\\' => i += 1,
                b'"' => in_str = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            if text[i..].starts_with(&pat) {
                let mut j = i + pat.len();
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if b.get(j) == Some(&b':') {
                    j += 1;
                    while j < b.len() && b[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    return Some(j);
                }
            }
            in_str = true;
        }
        i += 1;
    }
    None
}

/// The text of the object value of `key` (first occurrence).
pub fn object_text<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let start = value_start(text, key)?;
    let b = text.as_bytes();
    if b.get(start) != Some(&b'{') {
        return None;
    }
    let mut depth = 0usize;
    let mut in_str = false;
    let mut i = start;
    while i < b.len() {
        let c = b[i];
        if in_str {
            match c {
                b'\\' => i += 1,
                b'"' => in_str = false,
                _ => {}
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&text[start..=i]);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_split_without_a_tree() {
        let t = r#"{"type":"event","id":2,"seq":5,"event":{"topic":"snapshot","devices":[{"id":"zb:1","name":"a]\"b"},{"id":"zb:2","x":[1,[2]]}],"entities":[],"states":[ {"uid":"u","state":1.5} , {"uid":"v","state":"x,y"} ],"network":{"channel":15,"ncp":{"fw":"1"}}}}"#;
        let d = array_items(t, "devices").unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(serde_json::from_str::<J>(d[0]).unwrap()["name"], "a]\"b");
        assert_eq!(serde_json::from_str::<J>(d[1]).unwrap()["x"][1][0], 2);
        assert_eq!(array_items(t, "entities").unwrap().len(), 0);
        let st = array_items(t, "states").unwrap();
        assert_eq!(
            st,
            vec![r#"{"uid":"u","state":1.5}"#, r#"{"uid":"v","state":"x,y"}"#]
        );
        assert_eq!(array_items(t, "gp"), None);
        assert_eq!(
            serde_json::from_str::<J>(object_text(t, "network").unwrap()).unwrap()["ncp"]["fw"],
            "1"
        );
        // A key inside a string is not a key.
        assert_eq!(
            array_items(r#"{"a":"\"devices\":[1]","devices":[2]}"#, "devices").unwrap(),
            vec!["2"]
        );
    }

    #[test]
    fn records() {
        let d = Device::from_record(&serde_json::json!({
            "id": "gp:017171f8", "ieee": null, "src_id": "017171f8", "name": "BGT1", "power": "battery",
            "definition": [{"source": "generic"}], "available": false, "lqi": 300
        }))
        .unwrap();
        assert_eq!(d.ieee_address(), "0x00000000017171f8");
        assert!(d.is_gp() && d.sleepy() && !d.available && !d.has_definition);
        assert_eq!(d.lqi, Some(255));
        let mut m = Model::default();
        m.devices.insert(d.id.clone(), Box::new(d));
        for (uid, key) in [
            ("gp:017171f8:242:temperature", "temperature"),
            ("gp:017171f9:242:x", "x"),
        ] {
            let e = Entity::from_record(&serde_json::json!({"uid": uid, "device": &uid[..11], "key": key, "platform": "sensor", "endpoint": 242})).unwrap();
            m.entities.insert(uid.into(), Box::new(e));
        }
        assert_eq!(m.entities_of("gp:017171f8").count(), 1);
        m.remove_device("gp:017171f8");
        assert_eq!(m.entities.len(), 1);
    }
}
