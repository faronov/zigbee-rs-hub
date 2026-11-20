//! zigbee-rs-hub application runtime (stage 2): the device and entity
//! engine (`hub-devices`) on the ZRHP host core (`hub-ncp`).
//!
//! | Module | Contents |
//! |---|---|
//! | [`convert`] | ZRHP wire events/requests ⇄ the engine's owned types |
//! | [`transport`] | the engine's `ZigbeeTransport` on `hub_ncp::Client` |
//! | [`registry`] | device registry and user definitions in the host store journal (`hubdata`) |
//! | [`app`] | the app thread: engine, `ApiServer` sessions, registry, 1 s tick |
//!
//! The network side of the WebSocket JSON API (a desktop server in
//! `hubctl serve`, `esp_http_server` on the S3) only moves text frames
//! between sockets and [`App::connect`] / [`App::text`] / a [`SessionSink`].

pub mod app;
pub mod convert;
pub mod registry;
pub mod transport;

pub use app::{App, AppConfig, AppStatus, SessionSink, builtin_catalog};

#[cfg(test)]
mod tests;
