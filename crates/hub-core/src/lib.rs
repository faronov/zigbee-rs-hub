//! Portable core of `zigbee-rs-hub`.
//!
//! - [`board`]: wiring of the Espressif Thread Border Router / Zigbee Gateway
//!   board (ESP32-S3 host, ESP32-H2 NCP), checked against schematic v1.3.
//! - [`lines`]: esptool auto-reset semantics (DTR/RTS → EN/BOOT) used by the
//!   phase-0 USB bridge and, later, by the hub's NCP lifecycle.
//! - [`layout`]: S3 flash layouts (8 MB production, 4 MB dev) and their
//!   boot check.
//! - [`setup`]: when the setup access point (first-time Wi-Fi setup) is open.
//! - [`dns`]: the setup AP's catch-all DNS answers and the mDNS responder.
//! - [`zrhp`]: re-export of the `zigbee-zrhp` protocol crate (phase 1).

#![no_std]

pub use zigbee_zrhp as zrhp;

pub mod board;
pub mod dns;
pub mod layout;
pub mod lines;
pub mod resources;
pub mod setup;
