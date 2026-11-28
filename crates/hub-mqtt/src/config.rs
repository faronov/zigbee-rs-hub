//! Bridge settings (`mqtt/config/get`, `mqtt/config/set`), stored as one
//! JSON string by the platform ([`SettingsStore`]: NVS on the S3, a file or
//! RAM for `hubctl`). The password is never returned by the API.

use serde_json::{Map, Value as J};

use crate::obj::Obj;

/// Default base topic (Zigbee2MQTT's).
pub const DEFAULT_BASE: &str = "zigbee2mqtt";
/// Default discovery prefix (Home Assistant's).
pub const DEFAULT_DISCOVERY_PREFIX: &str = "homeassistant";

/// The settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MqttConfig {
    /// The bridge runs.
    pub enabled: bool,
    /// Broker host name or address (plain MQTT over TCP: the hub has no
    /// TLS).
    pub host: String,
    /// Broker port.
    pub port: u16,
    /// Client identifier; empty: `zigbee-rs-hub-<6 hex of the coordinator IEEE>`.
    pub client_id: String,
    /// User name (empty: none).
    pub username: String,
    /// Password (empty: none).
    pub password: String,
    /// Base topic.
    pub base_topic: String,
    /// Keepalive, s.
    pub keepalive_s: u16,
    /// Home Assistant MQTT discovery (off by default: the hub's own HA
    /// integration creates the same entities).
    pub discovery: bool,
    /// Discovery prefix.
    pub discovery_prefix: String,
    /// Device state messages retained (Zigbee2MQTT's per-device `retain`,
    /// default off).
    pub retain_state: bool,
    /// Availability messages retained (Zigbee2MQTT retains them).
    pub retain_availability: bool,
    /// QoS of device state messages (0 or 1; Zigbee2MQTT: 0).
    pub qos: u8,
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: 1883,
            client_id: String::new(),
            username: String::new(),
            password: String::new(),
            base_topic: DEFAULT_BASE.into(),
            keepalive_s: 60,
            discovery: false,
            discovery_prefix: DEFAULT_DISCOVERY_PREFIX.into(),
            retain_state: false,
            retain_availability: true,
            qos: 0,
        }
    }
}

fn bad(m: impl Into<String>) -> String {
    m.into()
}

/// A topic prefix without wildcards, empty levels or surrounding slashes.
pub fn valid_prefix(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.contains(['+', '#', '\0'])
        && !s.starts_with('/')
        && !s.ends_with('/')
        && !s.contains("//")
}

impl MqttConfig {
    /// The stored form (with the password).
    pub fn to_store(&self) -> String {
        let mut v = self.to_api();
        v["password"] = self.password.clone().into();
        if let Some(o) = v.as_object_mut() {
            o.remove("password_set");
        }
        v.to_string()
    }

    /// The API form: the password replaced by `password_set`.
    pub fn to_api(&self) -> J {
        Obj::new()
            .with("enabled", self.enabled)
            .with("host", self.host.clone())
            .with("port", self.port)
            .with("client_id", self.client_id.clone())
            .with("username", self.username.clone())
            .with("password_set", !self.password.is_empty())
            .with("base_topic", self.base_topic.clone())
            .with("keepalive_s", self.keepalive_s)
            .with("discovery", self.discovery)
            .with("discovery_prefix", self.discovery_prefix.clone())
            .with("retain_state", self.retain_state)
            .with("retain_availability", self.retain_availability)
            .with("qos", self.qos)
            .j()
    }

    /// Stored settings; unreadable ones give the defaults.
    pub fn from_store(s: Option<&str>) -> Self {
        let mut c = Self::default();
        if let Some(v) = s.and_then(|s| serde_json::from_str::<J>(s).ok())
            && let Some(o) = v.as_object()
        {
            // Stored values were validated when they were set.
            let _ = c.apply(o, false);
        }
        c
    }

    /// Applies the fields present in `o` (absent ones keep their value;
    /// `password` replaces the password, `""` removes it); with `check` the
    /// result is validated. A new `host` or `username` without a
    /// `password` in the same request removes the stored one: it was given
    /// for the old broker and user, and is never sent to another.
    pub fn apply(&mut self, o: &Map<String, J>, check: bool) -> Result<(), String> {
        let mut c = self.clone();
        for (k, v) in o {
            let s = || {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| bad(format!("{k}: a string")))
            };
            let b = || {
                v.as_bool()
                    .ok_or_else(|| bad(format!("{k}: true or false")))
            };
            let n = |lo: u64, hi: u64| {
                v.as_u64()
                    .filter(|x| (lo..=hi).contains(x))
                    .ok_or_else(|| bad(format!("{k}: {lo}..{hi}")))
            };
            match k.as_str() {
                "enabled" => c.enabled = b()?,
                "host" => c.host = s()?.trim().to_string(),
                "port" => c.port = n(1, 65535)? as u16,
                "client_id" => c.client_id = s()?.trim().to_string(),
                "username" => c.username = s()?,
                "password" => c.password = s()?,
                "base_topic" => c.base_topic = s()?.trim().to_string(),
                "keepalive_s" => c.keepalive_s = n(5, 3600)? as u16,
                "discovery" => c.discovery = b()?,
                "discovery_prefix" => c.discovery_prefix = s()?.trim().to_string(),
                "retain_state" => c.retain_state = b()?,
                "retain_availability" => c.retain_availability = b()?,
                "qos" => c.qos = n(0, 1)? as u8,
                // Read-only fields a client may echo back.
                "password_set" | "id" | "type" => {}
                other if check => return Err(bad(format!("unknown setting {other:?}"))),
                _ => {}
            }
        }
        if (c.host != self.host || c.username != self.username) && !o.contains_key("password") {
            c.password.clear();
        }
        if check {
            c.validate()?;
        }
        *self = c;
        Ok(())
    }

    /// Checks the settings.
    pub fn validate(&self) -> Result<(), String> {
        if self.enabled && self.host.is_empty() {
            return Err(bad("host: required when enabled"));
        }
        if self.host.len() > 128 || self.host.contains(['/', ' ']) {
            return Err(bad(
                "host: a host name or address (no scheme: plain MQTT over TCP)",
            ));
        }
        if !valid_prefix(&self.base_topic) {
            return Err(bad(
                "base_topic: no wildcards, empty levels or leading/trailing '/'",
            ));
        }
        if !valid_prefix(&self.discovery_prefix) {
            return Err(bad(
                "discovery_prefix: no wildcards, empty levels or leading/trailing '/'",
            ));
        }
        if self.client_id.len() > 64 {
            return Err(bad("client_id: at most 64 characters"));
        }
        if self.username.len() > 64 || self.password.len() > 64 {
            return Err(bad("username/password: at most 64 bytes each"));
        }
        Ok(())
    }
}

/// Where the settings live.
pub trait SettingsStore: Send + Sync {
    /// The stored JSON.
    fn load(&self) -> Option<String>;
    /// Store the JSON.
    fn save(&self, json: &str) -> Result<(), String>;
}

/// Settings in RAM (tests; `hubctl serve` without `--mqtt-settings`).
#[derive(Default)]
pub struct MemoryStore(pub std::sync::Mutex<Option<String>>);

impl SettingsStore for MemoryStore {
    fn load(&self) -> Option<String> {
        self.0.lock().unwrap().clone()
    }
    fn save(&self, json: &str) -> Result<(), String> {
        *self.0.lock().unwrap() = Some(json.to_string());
        Ok(())
    }
}

/// Settings in a file.
pub struct FileStore(pub std::path::PathBuf);

impl SettingsStore for FileStore {
    fn load(&self) -> Option<String> {
        std::fs::read_to_string(&self.0).ok()
    }
    fn save(&self, json: &str) -> Result<(), String> {
        std::fs::write(&self.0, json).map_err(|e| format!("{}: {e}", self.0.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn apply_validate_and_round_trip() {
        let mut c = MqttConfig::default();
        assert_eq!(c.base_topic, "zigbee2mqtt");
        assert!(!c.discovery, "discovery is opt-in");
        let set =
            json!({"enabled": true, "host": "192.168.1.5", "password": "pw", "discovery": true});
        c.apply(set.as_object().unwrap(), true).unwrap();
        assert!(c.enabled && c.discovery);
        let api = c.to_api();
        assert_eq!(api["password_set"], true);
        assert!(api.get("password").is_none(), "{api}");
        assert_eq!(MqttConfig::from_store(Some(&c.to_store())), c);
        // Absent fields keep their value; invalid input changes nothing.
        let before = c.clone();
        for bad in [
            json!({"base_topic": "z/#"}),
            json!({"base_topic": "/z"}),
            json!({"port": 0}),
            json!({"host": "mqtts://x"}),
            json!({"enabled": true, "host": ""}),
            json!({"qos": 2}),
            json!({"colour": "red"}),
        ] {
            assert!(c.apply(bad.as_object().unwrap(), true).is_err(), "{bad}");
            assert_eq!(c, before);
        }
        c.apply(json!({"password": ""}).as_object().unwrap(), true)
            .unwrap();
        assert_eq!(c.to_api()["password_set"], false);
        // The same host and user sent back (a form): the password stays.
        let set = json!({"host": "192.168.1.5", "username": "hub", "password": "pw"});
        c.apply(set.as_object().unwrap(), true).unwrap();
        let same = json!({"host": "192.168.1.5", "username": "hub", "port": 1884});
        c.apply(same.as_object().unwrap(), true).unwrap();
        assert_eq!(c.password, "pw");
        // Another broker or user without a password: the stored one goes.
        for other in [json!({"host": "10.0.0.9"}), json!({"username": "other"})] {
            c.apply(set.as_object().unwrap(), true).unwrap();
            c.apply(other.as_object().unwrap(), true).unwrap();
            assert_eq!(
                (c.password.as_str(), &c.to_api()["password_set"]),
                ("", &json!(false)),
                "{other}"
            );
        }
        // With a password in the same request: that one.
        c.apply(
            json!({"host": "10.0.0.8", "password": "new"})
                .as_object()
                .unwrap(),
            true,
        )
        .unwrap();
        assert_eq!(c.password, "new");
        // The stored form loads with its password.
        assert_eq!(MqttConfig::from_store(Some(&c.to_store())).password, "new");
        assert_eq!(
            MqttConfig::from_store(Some("garbage")),
            MqttConfig::default()
        );
    }
}
