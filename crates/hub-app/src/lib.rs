//! zigbee-rs-hub application runtime (stage 2): the device and entity
//! engine (`hub-devices`) on the ZRHP host core (`hub-ncp`).
//!
//! | Module | Contents |
//! |---|---|
//! | [`convert`] | ZRHP wire events/requests ⇄ the engine's owned types |
//! | [`transport`] | the engine's `ZigbeeTransport` on `hub_ncp::Client` |
//! | [`registry`] | device registry and user definitions in the host store journal (`hubdata`) |
//! | [`app`] | the app thread: engine, `ApiServer` sessions, registry, 1 s tick |
//! | [`catalog`] | the definitions catalog's trusted keys (slot 0, slot 1 for rotation) and bank/manifest verification |
//! | [`admin`] | network and hub maintenance API (energy scan, channel, TX power, diagnostics, map, backup/restore, reset; Wi-Fi, token, reboot, mode, logs) |
//! | [`ocb`] | Open Coordinator Backup JSON ⇄ ZRHP record streams |
//! | [`topology`] | network map: coordinator tables and Mgmt_Lqi/Mgmt_Rtg over the air |
//! | [`platform`] | what the app needs from the machine ([`Platform`]), log ring |
//!
//! The network side of the WebSocket JSON API (a desktop server in
//! `hubctl serve`, `esp_http_server` on the S3) only moves text frames
//! between sockets and [`App::connect`] / [`App::text`] / a [`SessionSink`].

pub mod admin;
pub mod app;
pub mod catalog;
pub mod jobs;
pub mod jw;
pub mod convert;
pub mod ocb;
pub mod platform;
pub mod registry;
pub mod topology;
pub mod transport;

pub use app::{App, AppConfig, AppStatus, SessionSink, builtin_catalog};
pub use platform::{LogRing, Platform};

#[cfg(test)]
mod tests;
