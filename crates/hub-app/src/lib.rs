//! zigbee-rs-hub application runtime (stage 2): the device and entity
//! engine (`hub-devices`) on the ZRHP host core (`hub-ncp`).
//!
//! | Module | Contents |
//! |---|---|
//! | [`transport`] | the engine's `ZigbeeTransport` on a `hub_coord::Coordinator` (ZRHP: `hub_ncp::coord::ZrhpCoordinator`) |
//! | [`registry`] | device registry and user definitions in the host store journal (`hubdata`) |
//! | [`app`] | the app thread: engine, `ApiServer` sessions, registry, 1 s tick |
//! | [`catalog`] | the definitions catalog: trusted keys (slot 0, slot 1 for rotation), bank/manifest verification, the store in `hubdata` (bank slots A/B, active-bank record, manifest, local definitions) and the service (update checks, downloads, on-demand fetch, uploads, `catalog/*` API) |
//! | [`automations`] | local automations: rules (triggers → conditions → actions) and schedules evaluated on the app thread, `automations/*` API, rules in the host store journal (`auto/<id>`) |
//! | [`admin`] | network and hub maintenance API (energy scan, channel, TX power, diagnostics, map, backup/restore, reset; Wi-Fi, token, reboot, mode, logs) |
//! | [`ocb`] | Open Coordinator Backup JSON ⇄ ZRHP record streams |
//! | `ota` | (feature `ota`) OTA Upgrade server (cluster 0x0019, `hub-ota`): streamed images, `ota/*` requests, `ota` topic |
//! | `ota_store` | (feature `ota-store`) the image store in the inactive app slot, its lock against the hub's own update |
//! | [`topology`] | network map: coordinator tables and Mgmt_Lqi/Mgmt_Rtg over the air |
//! | [`platform`] | what the app needs from the machine ([`Platform`]), log ring |
//!
//! The network side of the WebSocket JSON API (a desktop server in
//! `hubctl serve`, `esp_http_server` on the S3) only moves text frames
//! between sockets and [`App::connect`] / [`App::text`] / a [`SessionSink`].

pub mod admin;
pub mod app;
pub mod automations;
pub mod catalog;
pub mod clock;
pub mod jobs;
pub mod jw;
pub mod laststate;
pub mod ocb;
#[cfg(feature = "ota")]
pub mod ota;
#[cfg(feature = "ota-store")]
pub mod ota_store;
pub mod platform;
pub mod registry;
pub mod topology;
pub mod transport;

pub use app::{App, AppConfig, AppStatus, CatalogSetup, Pieces, RequestHook, SessionSink, builtin_catalog};
#[cfg(feature = "ota-store")]
pub use ota_store::OtaFlash;
pub use platform::{LogRing, Platform};

#[cfg(all(test, feature = "ota"))]
mod ota_tests;
#[cfg(test)]
mod baseline;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_baseline;
#[cfg(test)]
mod tests_groups;
#[cfg(test)]
mod tests_owner;
