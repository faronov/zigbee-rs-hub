//! Portable core of `zigbee-rs-hub`.
//!
//! - [`board`]: wiring of the Espressif Thread Border Router / Zigbee Gateway
//!   board (ESP32-S3 host, ESP32-H2 NCP), checked against schematic v1.3.
//! - [`lines`]: esptool auto-reset semantics (DTR/RTS → EN/BOOT) used by the
//!   phase-0 USB bridge and, later, by the hub's NCP lifecycle.
//! - [`layout`]: S3 flash layouts (8 MB production, 4 MB dev) and their
//!   boot check.
//! - [`profile`]: board profiles (Thread BR board, SONOFF Dongle-M): NCP
//!   UART, reset/loader lines, Ethernet, LED, button, console, flash/PSRAM,
//!   with a wiring check.
//! - [`resources`]: buffer limits from the PSRAM detected at boot.
//! - [`zrhp`]: re-export of the `zigbee-zrhp` protocol crate (phase 1).

#![no_std]

pub use zigbee_zrhp as zrhp;

pub mod board;
pub mod layout;
pub mod lines;
pub mod profile;
pub mod resources;
