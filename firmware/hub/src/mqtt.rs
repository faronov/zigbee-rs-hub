//! The MQTT bridge (`zrh_espapp::mqtt`) with its settings in NVS
//! `hub/mqtt`.

use std::sync::Arc;

use zrh_espapp::mqtt::{SETTINGS_MAX, SettingsStore};

use crate::mode;

struct Nvs;

impl SettingsStore for Nvs {
    fn load(&self) -> Option<String> {
        mode::get_long_str("mqtt", SETTINGS_MAX)
    }
    fn save(&self, json: &str) -> Result<(), String> {
        if json.len() >= SETTINGS_MAX {
            return Err(format!("settings too long ({} B)", json.len()));
        }
        mode::set_str("mqtt", Some(json))
    }
}

/// Starts the service (the bridge thread only when enabled).
pub fn start(app: &hub_app::App, r: &hub_core::resources::Resources) -> hub_mqtt::MqttService {
    zrh_espapp::mqtt::start(app, r, Arc::new(Nvs))
}
