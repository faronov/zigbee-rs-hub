//! Coordinator backend boundary of zigbee-rs-hub (`docs/coordinator-boundary.md`
//! §2): the types and traits between the device engine and application on
//! one side and a coordinator backend (ZRHP, EZSP, ZNP) on the other. No
//! backend protocol crate is a dependency.
//!
//! - [`Coordinator`]: non-blocking request submission ([`Request`]) with
//!   one [`Terminal`] outcome per [`RequestId`], [`CoordEvent`]s (through
//!   the bounded [`EventQueue`]) and a [`CoordStatus`] snapshot;
//! - [`NetworkAdmin`]: blocking administration for job threads;
//! - [`RadioControl`]: Wi-Fi coexistence, called by the firmware;
//! - [`CoordError`], [`Capabilities`], [`NetworkIdentity`] and
//!   [`NetworkOwnership`];
//! - the ownership tag of the hub's network-scoped records ([`OwnerTag`])
//!   and their [`RegistryState`];
//! - the APS and Green Power types the engine exchanges with the backend
//!   ([`ApsRequest`], [`ApsIndication`], [`gp`]), re-exported by
//!   `hub-devices` at their old paths.

#![warn(missing_docs)]

mod admin;
mod aps;
mod caps;
mod coord;
mod error;
pub mod gp;
mod identity;
mod owner;
mod queue;
mod request;

pub use admin::{
    BackupChild, BackupDevice, BackupItem, BackupLinkKey, BackupSink, Beacon, ChannelChange, Child,
    DiagCounter, Diagnostics, EnergyChannel, FormParams, FrameCounters, FwImage, FwPhase, FwSlot,
    FwSource, NcpFirmware, NcpStatus, Neighbor, NetworkAdmin, RestoreBlob, RestoreEncoder,
    RestoreOptions, RestoreReport, Route, TablesRead, restore_note,
};
pub use aps::{ApsDst, ApsIndication, ApsRequest, TxOptions};
pub use caps::{
    BackupSupport, Capabilities, FwUpdateMethod, GpSupport, OwnershipModel, RadioOps, Tables,
};
pub use coord::{
    BackendConfig, CoordEvent, CoordStatus, Coordinator, EndpointConfig, NcpInfo, NetChange,
    NetState, NetworkParams, RadioControl, SessionEvent, SessionState,
};
pub use error::{BackendKind, Code, CoordError, Described, Kind};
pub use gp::{GpEvent, GpRequest};
pub use identity::{NetworkIdentity, NetworkOwnership};
pub use owner::{OwnerTag, RegistryState};
pub use queue::{Doorbell, EVENT_SLOTS, EventQueue};
pub use request::{Inventory, InventoryDevice, Progress, Reply, Request, RequestId, Terminal};

// The backend's event queue preallocates 64 slots: keep them within 4 KiB.
// Today 56 bytes on 64-bit hosts and 48 on the ESP32-S3, set by the inline
// `ApsIndication`.
const _: () = assert!(size_of::<CoordEvent>() <= 64);
