//! ZCL codec: data types, frames, foundation commands and the cluster IDs
//! the engine knows.
//!
//! zigbee-rs' `zigbee-zcl` crate was evaluated for reuse and not taken: it
//! is the device-side (server) implementation with `heapless` buffers fixed
//! at 32 B per string and 128 B per payload (self-description chunks are
//! 48 B, Lumi structures and Tuya schedules longer), it lacks the odd-width
//! integer types the hub must decode (`int24` InstantaneousDemand, `uint40`,
//! `int48`…) and the composite types, and it has no client-side builders or
//! response parsers. This module is the small std host codec instead.

pub mod frame;
pub mod types;

pub use frame::{Direction, FrameType, ReportingConfig, Status, ZclFrame, ZclHeader};
pub use types::{CodecError, TypedValue, ZclType, ZclValue};

/// Home Automation profile.
pub const PROFILE_HA: u16 = 0x0104;
/// ZLL profile.
pub const PROFILE_ZLL: u16 = 0xC05E;
/// Green Power profile.
pub const PROFILE_GP: u16 = 0xA1E0;

/// Cluster identifiers used by the engine (ZCL r8; zigbee-reference KB).
#[allow(missing_docs)]
pub mod cluster {
    pub const BASIC: u16 = 0x0000;
    pub const POWER_CONFIG: u16 = 0x0001;
    pub const DEVICE_TEMPERATURE: u16 = 0x0002;
    pub const IDENTIFY: u16 = 0x0003;
    pub const GROUPS: u16 = 0x0004;
    pub const SCENES: u16 = 0x0005;
    pub const ON_OFF: u16 = 0x0006;
    pub const LEVEL: u16 = 0x0008;
    pub const ANALOG_INPUT: u16 = 0x000C;
    pub const BINARY_INPUT: u16 = 0x000F;
    pub const MULTISTATE_INPUT: u16 = 0x0012;
    pub const TIME: u16 = 0x000A;
    pub const OTA: u16 = 0x0019;
    pub const POLL_CONTROL: u16 = 0x0020;
    pub const GREEN_POWER: u16 = 0x0021;
    pub const DOOR_LOCK: u16 = 0x0101;
    pub const WINDOW_COVERING: u16 = 0x0102;
    pub const THERMOSTAT: u16 = 0x0201;
    pub const FAN_CONTROL: u16 = 0x0202;
    pub const THERMOSTAT_UI: u16 = 0x0204;
    pub const COLOR: u16 = 0x0300;
    pub const ILLUMINANCE: u16 = 0x0400;
    pub const TEMPERATURE: u16 = 0x0402;
    pub const PRESSURE: u16 = 0x0403;
    pub const HUMIDITY: u16 = 0x0405;
    pub const OCCUPANCY: u16 = 0x0406;
    pub const SOIL_MOISTURE: u16 = 0x0408;
    pub const CO: u16 = 0x040C;
    pub const CO2: u16 = 0x040D;
    pub const PM25: u16 = 0x042A;
    pub const FORMALDEHYDE: u16 = 0x042B;
    pub const PM1: u16 = 0x042C;
    pub const PM10: u16 = 0x042D;
    pub const TVOC: u16 = 0x042E;
    pub const RADON: u16 = 0x042F;
    pub const IAS_ZONE: u16 = 0x0500;
    pub const IAS_ACE: u16 = 0x0501;
    pub const IAS_WD: u16 = 0x0502;
    pub const METERING: u16 = 0x0702;
    pub const ELECTRICAL: u16 = 0x0B04;
    pub const DIAGNOSTICS: u16 = 0x0B05;
    pub const TUYA: u16 = 0xEF00;
    pub const ZRS_SCHEMA: u16 = 0xFC6B;
    pub const ZRS_SETTINGS: u16 = 0xFC6C;
}
