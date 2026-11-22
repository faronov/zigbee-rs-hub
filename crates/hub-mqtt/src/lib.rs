//! zigbee-rs-hub MQTT bridge: the hub's devices on an MQTT broker with
//! Zigbee2MQTT's topics and payloads, so tools built for Zigbee2MQTT
//! (Node-RED, ioBroker, openHAB, Home Assistant's MQTT discovery) work with
//! the hub. Plain MQTT over TCP (the hub has no TLS), to a broker on the
//! LAN.
//!
//! | Module | Contents |
//! |---|---|
//! | [`config`] | settings, their validation and storage |
//! | [`model`] | the bridge's compact copy of devices, entities and states |
//! | [`z2m`] | friendly names, state payloads, exposes, `/set` and `/get` |
//! | [`discovery`] | Home Assistant MQTT discovery configs |
//! | [`bridge`] | the state machine: API frames and MQTT messages in, publications out |
//! | [`service`] | the bridge thread, reconnects, the `mqtt/*` API requests |
//! | [`codec`], [`tcp`], [`broker`] | MQTT 3.1.1 packets, a desktop client, a test broker |
//!
//! The bridge is a client of the hub's own WebSocket JSON API (a trusted
//! in-process session), so it sees exactly what the web UI and the Home
//! Assistant integration see.

pub mod bridge;
pub mod broker;
pub mod codec;
pub mod config;
pub mod discovery;
pub mod model;
mod obj;
pub mod service;
pub mod tcp;
pub mod z2m;

pub use bridge::{Bridge, Limits, Msg};
pub use config::{FileStore, MemoryStore, MqttConfig, SettingsStore};
pub use service::{
    ConnectOptions, Event, EventFn, MqttService, ServiceConfig, Status, Transport, TransportFactory,
};

#[cfg(test)]
mod tests;
