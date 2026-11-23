//! The Zigbee2MQTT-compatible MQTT bridge (`hub-mqtt`) on the S3: ESP-IDF's
//! esp-mqtt client over plain TCP (the hub has no TLS; the SSL and
//! WebSocket transports are off in `sdkconfig.defaults`), settings in NVS
//! `hub/mqtt`. Started with the app; it connects once Wi-Fi is up (the
//! bridge retries with backoff) and only when enabled in the settings.
//!
//! Feature `mqtt-lite` uses the bridge's own MQTT 3.1.1 client on lwIP
//! sockets instead (`hub_mqtt::tcp`): esp-mqtt brings esp-tls and the
//! mbedtls TLS layer into the image even for plain TCP.

use std::sync::Arc;
#[cfg(not(feature = "mqtt-lite"))]
use std::time::Duration;

#[cfg(not(feature = "mqtt-lite"))]
use esp_idf_svc::mqtt::client::{Details, EspMqttClient, EventPayload, LwtConfiguration, MqttClientConfiguration, QoS};
use esp_idf_svc::sys;
#[cfg(not(feature = "mqtt-lite"))]
use hub_mqtt::{ConnectOptions, Event, EventFn, Msg};
use hub_mqtt::{Limits, ServiceConfig, SettingsStore, Transport};
#[cfg(not(feature = "mqtt-lite"))]
use log::warn;

use crate::mode;

/// Settings JSON stored at most (NVS strings take up to 4000 B).
const SETTINGS_MAX: usize = 1024;

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

#[cfg(not(feature = "mqtt-lite"))]
fn qos(q: u8) -> QoS {
    if q == 0 { QoS::AtMostOnce } else { QoS::AtLeastOnce }
}

/// esp-mqtt, one client per connection (no automatic reconnect: the
/// bridge reconnects with backoff).
#[cfg(not(feature = "mqtt-lite"))]
#[derive(Default)]
struct Esp {
    client: Option<EspMqttClient<'static>>,
}

#[cfg(not(feature = "mqtt-lite"))]
impl Transport for Esp {
    fn connect(&mut self, o: &ConnectOptions, events: EventFn) -> Result<(), String> {
        self.client = None;
        let url = format!("mqtt://{}:{}", o.host, o.port);
        let conf = MqttClientConfiguration {
            client_id: Some(&o.client_id),
            keep_alive_interval: Some(Duration::from_secs(u64::from(o.keepalive_s))),
            reconnect_timeout: None,
            network_timeout: Duration::from_secs(10),
            lwt: Some(LwtConfiguration { topic: &o.will.topic, payload: &o.will.payload, qos: qos(o.will.qos), retain: o.will.retain }),
            username: o.username.as_deref(),
            password: o.password.as_deref(),
            task_stack: 5120,
            buffer_size: 1024,
            out_buffer_size: 1024,
            // QoS 1 messages awaiting PUBACK (availability, bridge state).
            outbox_limit: Some(8 * 1024),
            ..Default::default()
        };
        let client = EspMqttClient::new_cb(&url, &conf, move |e| match e.payload() {
            EventPayload::Connected(_) => events(Event::Connected),
            EventPayload::Disconnected => events(Event::Disconnected("disconnected".into())),
            EventPayload::Error(e) => warn!("mqtt: esp-mqtt: {e}"),
            // Messages larger than the input buffer arrive in chunks: a
            // command is never that large; dropped.
            EventPayload::Received { topic: Some(t), data, details: Details::Complete, .. } => {
                events(Event::Message(t.to_string(), data.to_vec()))
            }
            _ => {}
        })
        .map_err(|e| format!("esp-mqtt: {e}"))?;
        self.client = Some(client);
        Ok(())
    }

    fn disconnect(&mut self) {
        // Dropping stops and destroys the client (its task included).
        self.client = None;
    }

    fn publish(&mut self, m: &Msg) -> Result<(), String> {
        let c = self.client.as_mut().ok_or("not connected")?;
        c.publish(&m.topic, qos(m.qos), m.retain, &m.payload).map(|_| ()).map_err(|e| e.to_string())
    }

    fn subscribe(&mut self, filters: &[String]) -> Result<(), String> {
        let c = self.client.as_mut().ok_or("not connected")?;
        for f in filters {
            c.subscribe(f, QoS::AtMostOnce).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

fn internal_free() -> usize {
    unsafe { sys::heap_caps_get_free_size(sys::MALLOC_CAP_INTERNAL) }
}

fn stack_free() -> usize {
    unsafe { sys::uxTaskGetStackHighWaterMark(std::ptr::null_mut()) as usize }
}

/// Starts the service (the bridge thread only when enabled).
pub fn start(app: &hub_app::App, r: &hub_core::resources::Resources) -> hub_mqtt::MqttService {
    let mut seed = [0u8; 3];
    (crate::esp::rng())(&mut seed);
    let svc = ServiceConfig {
        stack_size: 16 * 1024,
        // Without PSRAM the queue and `bridge/devices` stay small.
        limits: Limits { queue_bytes: r.mqtt_queue_bytes, devices_bytes: r.mqtt_devices_bytes },
        client_suffix: seed.iter().map(|b| format!("{b:02x}")).collect(),
        stack_free: Some(stack_free),
        // Without PSRAM the Rust heap is internal RAM, shared with Wi-Fi and
        // the engine: the bridge starts only with room to spare.
        heap_free: r.heap_guard.then_some(internal_free as fn() -> usize),
        min_free: 64 * 1024,
    };
    #[cfg(not(feature = "mqtt-lite"))]
    let factory: hub_mqtt::TransportFactory = Arc::new(|| Box::new(Esp::default()) as Box<dyn Transport>);
    // Commands are small: larger packets are skipped; 6 KiB reader stack.
    #[cfg(feature = "mqtt-lite")]
    let factory: hub_mqtt::TransportFactory =
        Arc::new(|| Box::new(hub_mqtt::tcp::TcpTransport::with_limits(4096, 6 * 1024)) as Box<dyn Transport>);
    hub_mqtt::MqttService::start(app, Arc::new(Nvs), factory, svc)
}
