//! Portable core of `zigbee-rs-hub`.
//!
//! - [`board`]: wiring of the Espressif Thread Border Router / Zigbee Gateway
//!   board (ESP32-S3 host, ESP32-H2 NCP), checked against schematic v1.3.
//! - [`lines`]: esptool auto-reset semantics (DTR/RTS → EN/BOOT) used by the
//!   phase-0 USB bridge and, later, by the hub's NCP lifecycle.
//! - [`layout`]: host flash layouts (16 MB and 8 MB production, 4 MB dev)
//!   and their boot check.
//! - [`profile`]: board profiles (Thread BR board, SONOFF Dongle-M, ESP32-S3
//!   SuperMini + EFR32MG24 bench setup): NCP UART, reset/loader lines,
//!   Ethernet, LEDs, button, console, flash/PSRAM, with a wiring check.
//! - [`zrhp`]: re-export of the `zigbee-zrhp` protocol crate (phase 1).

#![no_std]

pub use zigbee_zrhp as zrhp;

pub mod board;
pub mod layout;
pub mod lines;
pub mod profile;
