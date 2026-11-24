//! Requests to the coordinator and their outcomes (design §2.2).

use crate::{ApsRequest, CoordError, GpRequest};

/// Identifies one submitted request; allocated by the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(pub u32);

/// What the engine asks the coordinator to do. Everything except
/// [`Request::Aps`] is a control request and a barrier: nothing submitted
/// later reaches the NCP before it completes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// APS data (ZCL or ZDO).
    Aps(ApsRequest),
    /// Green Power sink or remote provisioning request.
    Gp(GpRequest),
    /// Group membership of one hub endpoint.
    Group {
        /// Hub endpoint.
        endpoint: u8,
        /// Group ID.
        group: u16,
        /// `true` add, `false` remove.
        add: bool,
    },
    /// Remove a device from the network.
    RemoveDevice {
        /// Device IEEE.
        ieee: u64,
        /// Its short address, if known.
        nwk: Option<u16>,
    },
    /// Open (or close, 0 s) joining.
    PermitJoin {
        /// Seconds.
        duration_s: u8,
        /// One router, or all devices.
        target: Option<u16>,
    },
    /// The devices the NCP already knows ([`Reply::Inventory`]).
    DeviceInventory,
}

/// Intermediate outcome; at most once per request, before its [`Terminal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// The NCP took the request.
    Accepted {
        /// ZCL TSN the NCP chose (GP remote provisioning).
        zcl_tsn: Option<u8>,
    },
}

/// Final outcome; exactly one per request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terminal {
    /// An acknowledged unicast was confirmed.
    Delivered,
    /// Accepted without positive delivery (broadcast, group, no APS ACK
    /// requested).
    Sent,
    /// The NCP reported a delivery failure.
    NotDelivered(CoordError),
    /// The NCP refused the request.
    Refused(CoordError),
    /// Unknown outcome (timeout, reset, local error).
    Unknown(CoordError),
    /// Cancelled before it reached the NCP.
    Cancelled,
    /// A control request completed.
    Done(Reply),
}

/// Result of a control request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// Nothing to return.
    None,
    /// [`Request::DeviceInventory`].
    Inventory(Box<Inventory>),
}

/// Devices the NCP knows (its device, child and neighbor tables), so that
/// the hub can import devices that joined before it adopted the network.
/// Mirrors `hub_ncp::inventory::Inventory`; the ZRHP adapter maps it (PR G).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    /// Devices, the coordinator excluded.
    pub devices: Vec<InventoryDevice>,
    /// Backend requests the NCP did not implement (for the log).
    pub unsupported: Vec<u16>,
}

/// One device of an [`Inventory`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InventoryDevice {
    /// IEEE address.
    pub ieee: u64,
    /// Short address, if known.
    pub nwk: Option<u16>,
    /// MAC capability, if known.
    pub capability: Option<u8>,
    /// Neighbor-table device type (0 coordinator, 1 router, 2 end device).
    pub device_type: Option<u8>,
}
