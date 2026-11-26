//! The Zigbee2MQTT-compatible MQTT bridge (`hub-mqtt`) on ESP-IDF's
//! esp-mqtt client over plain TCP (the hub has no TLS; the SSL and
//! WebSocket transports are off in the firmwares' `sdkconfig.defaults`),
//! settings in the firmware's NVS (a [`SettingsStore`]). Started with the
//! app; it connects once the network is up (the bridge retries with
//! backoff) and only when enabled in the settings.
//!
//! Feature `mqtt-lite` uses the bridge's own MQTT 3.1.1 client on lwIP
//! sockets instead (`hub_mqtt::tcp`): esp-mqtt brings esp-tls and the
//! mbedtls TLS layer into the image even for plain TCP.

use std::sync::Arc;
#[cfg(not(feature = "mqtt-lite"))]
use std::time::Duration;

#[cfg(not(feature = "mqtt-lite"))]
use esp_idf_svc::mqtt::client::{
    Details, EspMqttClient, EspMqttEvent, EventPayload, LwtConfiguration, MqttClientConfiguration, QoS,
};
use esp_idf_svc::sys;
#[cfg(not(feature = "mqtt-lite"))]
use hub_mqtt::{ConnDiag, ConnectOptions, Event, EventFn, Msg};
pub use hub_mqtt::SettingsStore;
use hub_mqtt::{Limits, ServiceConfig, Transport};

/// Settings JSON stored at most (NVS strings take up to 4000 B).
pub const SETTINGS_MAX: usize = 1024;

#[cfg(not(feature = "mqtt-lite"))]
fn qos(q: u8) -> QoS {
    if q == 0 { QoS::AtMostOnce } else { QoS::AtLeastOnce }
}

// esp-idf-svc 0.53 turns every error event into ESP_FAIL; the cause
// (CONNACK return code, socket error) is in the raw event it wraps. Only
// valid for the exact version pinned in Cargo.toml; with esp-idf-svc 0.54
// read `EventPayload::Error(&EspMqttError)` instead and delete this.
#[cfg(not(feature = "mqtt-lite"))]
const _: () = assert!(size_of::<EspMqttEvent<'static>>() == size_of::<&sys::esp_mqtt_event_t>());

/// Records the cause of an esp-mqtt error event for the `disconnected`
/// that follows it.
#[cfg(not(feature = "mqtt-lite"))]
fn record_error(e: &EspMqttEvent<'_>, diag: &mut ConnDiag) {
    // SAFETY: EspMqttEvent is a newtype over `&esp_mqtt_event_t` (its size
    // is checked above), valid for the callback.
    let raw: &sys::esp_mqtt_event_t = unsafe { core::mem::transmute_copy(e) };
    let Some(h) = (unsafe { raw.error_handle.as_ref() }) else { return };
    match h.error_type {
        sys::esp_mqtt_error_type_t_MQTT_ERROR_TYPE_CONNECTION_REFUSED => diag.refused(h.connect_return_code as u8),
        sys::esp_mqtt_error_type_t_MQTT_ERROR_TYPE_TCP_TRANSPORT => {
            let errno = h.esp_transport_sock_errno;
            let sock = std::io::Error::from_raw_os_error(errno);
            match h.esp_tls_last_esp_err {
                sys::ESP_ERR_ESP_TLS_TCP_CLOSED_FIN => diag.closed(),
                sys::ESP_ERR_ESP_TLS_CANNOT_RESOLVE_HOSTNAME => diag.error("cannot resolve the host name"),
                sys::ESP_ERR_ESP_TLS_CONNECTION_TIMEOUT => diag.error("connection timed out"),
                sys::ESP_ERR_ESP_TLS_FAILED_CONNECT_TO_HOST if errno != 0 => diag.error(format!("cannot connect: {sock}")),
                sys::ESP_ERR_ESP_TLS_FAILED_CONNECT_TO_HOST => diag.error("cannot connect"),
                _ if matches!(sock.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted) => {
                    diag.closed()
                }
                _ if errno != 0 => diag.error(sock.to_string()),
                e => diag.error(format!("transport error 0x{e:x}")),
            }
        }
        _ => {}
    }
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
        let mut diag = ConnDiag::new(o.username.is_none());
        let client = EspMqttClient::new_cb(&url, &conf, move |e| match e.payload() {
            EventPayload::Connected(_) => {
                diag.connected();
                events(Event::Connected)
            }
            EventPayload::Disconnected => events(Event::Disconnected(diag.ended())),
            // The bridge logs the cause with the disconnection.
            EventPayload::Error(_) => record_error(&e, &mut diag),
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

/// Starts the service (the bridge thread only when enabled), its settings
/// in `settings`.
pub fn start(app: &hub_app::App, r: &hub_core::resources::Resources, settings: Arc<dyn SettingsStore>) -> hub_mqtt::MqttService {
    let mut seed = [0u8; 3];
    crate::fill_random(&mut seed);
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
    hub_mqtt::MqttService::start(app, settings, factory, svc)
}
