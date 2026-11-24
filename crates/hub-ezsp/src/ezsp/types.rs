//! EmberZNet types carried by the core-stage EZSP frames.
//!
//! Numeric values come from the Gecko SDK 4.4.6 headers (`ezsp-enum.h`,
//! `ember-types.h`, `error-def.h`; EZSP 13) and the Simplicity SDK
//! 2024.6.0 / 2025.6.3 headers (`ezsp-enum.h`, `sl_zigbee_types.h`,
//! `sl_status.h`; EZSP 14 and 18), cross-checked with the Simplicity SDK
//! EZSP Reference Guide (UG100 successor, docs.silabs.com 9.1.2) §3 "Type
//! Definitions", "Structure Definitions" and the enumeration tables.
//! Codes are newtypes so that values unknown to this crate survive a
//! decode/encode round trip.

use super::version::EzspVersion;
use super::wire::{DecodeError, Reader, Writer};

macro_rules! code_type {
    ($(#[$m:meta])* $name:ident($repr:ty) { $($c:ident = $v:expr),* $(,)? }) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
        pub struct $name(pub $repr);

        #[allow(missing_docs)]
        impl $name {
            $(pub const $c: $name = $name($v);)*

            /// Symbolic name, if the value is one this crate knows.
            pub fn name(self) -> Option<&'static str> {
                match self.0 {
                    $(v if v == $v => Some(stringify!($c)),)*
                    _ => None,
                }
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self.name() {
                    Some(n) => write!(f, "{}::{}", stringify!($name), n),
                    None => write!(f, "{}(0x{:X})", stringify!($name), self.0),
                }
            }
        }
    };
}

code_type! {
    /// `EzspStatus` / `sl_zigbee_ezsp_status_t` (u8). Identical values in
    /// EZSP 13–18 (UG100 successor §3 table; `ezsp-enum.h`).
    EzspStatus(u8) {
        SUCCESS = 0x00,
        ASH_IN_PROGRESS = 0x20,
        HOST_FATAL_ERROR = 0x21,
        ASH_NCP_FATAL_ERROR = 0x22,
        DATA_FRAME_TOO_LONG = 0x23,
        DATA_FRAME_TOO_SHORT = 0x24,
        NO_TX_SPACE = 0x25,
        NO_RX_SPACE = 0x26,
        NO_RX_DATA = 0x27,
        NOT_CONNECTED = 0x28,
        ERROR_VERSION_NOT_SET = 0x30,
        ERROR_INVALID_FRAME_ID = 0x31,
        ERROR_WRONG_DIRECTION = 0x32,
        ERROR_TRUNCATED = 0x33,
        ERROR_OVERFLOW = 0x34,
        ERROR_OUT_OF_MEMORY = 0x35,
        ERROR_INVALID_VALUE = 0x36,
        ERROR_INVALID_ID = 0x37,
        ERROR_INVALID_CALL = 0x38,
        ERROR_NO_RESPONSE = 0x39,
        ERROR_COMMAND_TOO_LONG = 0x40,
        ERROR_QUEUE_FULL = 0x41,
        ERROR_COMMAND_FILTERED = 0x42,
        ERROR_SECURITY_KEY_ALREADY_SET = 0x43,
        ERROR_SECURITY_TYPE_INVALID = 0x44,
        ERROR_SECURITY_PARAMETERS_INVALID = 0x45,
        ERROR_SECURITY_PARAMETERS_ALREADY_SET = 0x46,
        ERROR_SECURITY_KEY_NOT_SET = 0x47,
        ERROR_SECURITY_PARAMETERS_NOT_SET = 0x48,
        ERROR_UNSUPPORTED_CONTROL = 0x49,
        ERROR_UNSECURE_FRAME = 0x4A,
        ASH_ERROR_VERSION = 0x50,
        ASH_ERROR_TIMEOUTS = 0x51,
        ASH_ERROR_RESET_FAIL = 0x52,
        ASH_ERROR_NCP_RESET = 0x53,
        ERROR_SERIAL_INIT = 0x54,
        ASH_ERROR_NCP_TYPE = 0x55,
        ASH_ERROR_RESET_METHOD = 0x56,
        ASH_ERROR_XON_XOFF = 0x57,
        NO_ERROR = 0xFF,
    }
}

code_type! {
    /// `EmberStatus` (u8), EZSP 13 (Gecko SDK 4.4.6 `error-def.h`).
    EmberStatus(u8) {
        SUCCESS = 0x00,
        ERR_FATAL = 0x01,
        BAD_ARGUMENT = 0x02,
        NOT_FOUND = 0x03,
        NO_BUFFERS = 0x18,
        DELIVERY_FAILED = 0x66,
        INVALID_CALL = 0x70,
        MAX_MESSAGE_LIMIT_REACHED = 0x72,
        MESSAGE_TOO_LONG = 0x74,
        NETWORK_UP = 0x90,
        NETWORK_DOWN = 0x91,
        NOT_JOINED = 0x93,
        JOIN_FAILED = 0x94,
        MOVE_FAILED = 0x96,
        CANNOT_JOIN_AS_ROUTER = 0x98,
        CHANNEL_CHANGED = 0x9B,
        NETWORK_OPENED = 0x9C,
        NETWORK_CLOSED = 0x9D,
        NETWORK_BUSY = 0xA1,
        NO_BEACONS = 0xAB,
        STACK_AND_HARDWARE_MISMATCH = 0xB0,
        INDEX_OUT_OF_RANGE = 0xB1,
        TABLE_FULL = 0xB4,
    }
}

code_type! {
    /// `sl_status_t` (u32), EZSP ≥ 14 (Simplicity SDK `sl_status.h`,
    /// identical in 2024.6.0 and 2025.6.3 for these values; `NO_APS_ACK`
    /// exists only in 2025.6.x).
    SlStatus(u32) {
        OK = 0x0000,
        FAIL = 0x0001,
        INVALID_STATE = 0x0002,
        NOT_READY = 0x0003,
        BUSY = 0x0004,
        TIMEOUT = 0x0007,
        NETWORK_UP = 0x0015,
        NETWORK_DOWN = 0x0016,
        NOT_JOINED = 0x0017,
        ALLOCATION_FAILED = 0x0019,
        FULL = 0x001C,
        INVALID_PARAMETER = 0x0021,
        INVALID_CONFIGURATION = 0x0023,
        INVALID_INDEX = 0x0027,
        NOT_FOUND = 0x002D,
        ALREADY_EXISTS = 0x002E,
        MESSAGE_TOO_LONG = 0x0038,
        ZIGBEE_DELIVERY_FAILED = 0x0C02,
        ZIGBEE_MAX_MESSAGE_LIMIT_REACHED = 0x0C03,
        ZIGBEE_NODE_ID_CHANGED = 0x0C07,
        ZIGBEE_PAN_ID_CHANGED = 0x0C16,
        ZIGBEE_CHANNEL_CHANGED = 0x0C17,
        ZIGBEE_NETWORK_OPENED = 0x0C18,
        ZIGBEE_NETWORK_CLOSED = 0x0C19,
        ZIGBEE_EZSP_ERROR = 0x0C1E,
        ZIGBEE_NO_APS_ACK = 0x0C20,
    }
}

/// Which status type a frame carries in EZSP 13. EZSP ≥ 14 carries
/// `sl_status_t` everywhere (Simplicity SDK `command-functions.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFamily {
    /// `EzspStatus`: configuration, values, endpoints.
    Ezsp,
    /// `EmberStatus`: stack calls.
    Ember,
}

/// A status as received, typed by the negotiated version.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    /// EZSP 13 `EzspStatus`.
    Ezsp(EzspStatus),
    /// EZSP 13 `EmberStatus`.
    Ember(EmberStatus),
    /// EZSP ≥ 14 `sl_status_t`.
    Sl(SlStatus),
}

/// Version-independent meaning of a [`Status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatusKind {
    Success,
    NetworkUp,
    NetworkDown,
    NotJoined,
    NetworkOpened,
    NetworkClosed,
    DeliveryFailed,
    MaxMessageLimitReached,
    MessageTooLong,
    InvalidCall,
    InvalidValue,
    InvalidId,
    OutOfMemory,
    /// `SL_STATUS_ZIGBEE_EZSP_ERROR`: an EZSP-level error whose detail is
    /// not carried on the wire in EZSP ≥ 14.
    EzspError,
    /// Anything else; inspect the raw [`Status`].
    Other,
}

impl Status {
    /// Width-correct decode for the negotiated version.
    pub fn read(
        r: &mut Reader<'_>,
        v: EzspVersion,
        family: StatusFamily,
    ) -> Result<Status, DecodeError> {
        Ok(if v.uses_sl_status() {
            Status::Sl(SlStatus(r.u32()?))
        } else {
            match family {
                StatusFamily::Ezsp => Status::Ezsp(EzspStatus(r.u8()?)),
                StatusFamily::Ember => Status::Ember(EmberStatus(r.u8()?)),
            }
        })
    }

    /// Encodes with the width of the variant; [`Status::for_version`]
    /// builds the variant that matches a version.
    pub fn write(self, w: &mut Writer) {
        match self {
            Status::Ezsp(s) => w.u8(s.0),
            Status::Ember(s) => w.u8(s.0),
            Status::Sl(s) => w.u32(s.0),
        }
    }

    /// Whether the variant is the one `v` uses for `family`.
    pub fn matches(self, v: EzspVersion, family: StatusFamily) -> bool {
        matches!(
            (self, v.uses_sl_status(), family),
            (Status::Sl(_), true, _)
                | (Status::Ezsp(_), false, StatusFamily::Ezsp)
                | (Status::Ember(_), false, StatusFamily::Ember)
        )
    }

    /// The status value `v` uses for `kind` in `family`, if one exists.
    /// EZSP ≥ 14 reports EZSP-level errors as `SL_STATUS_ZIGBEE_EZSP_ERROR`
    /// (UG100 successor §4 getConfigurationValue, setConfigurationValue,
    /// addEndpoint).
    // UNVERIFIED: that the detail EzspStatus is not sent on the wire in
    // EZSP ≥ 14 (the SDK host keeps it in a host-side "last status").
    pub fn for_version(v: EzspVersion, family: StatusFamily, kind: StatusKind) -> Option<Status> {
        use StatusKind as K;
        if v.uses_sl_status() {
            let s = match kind {
                K::Success => SlStatus::OK,
                K::NetworkUp => SlStatus::NETWORK_UP,
                K::NetworkDown => SlStatus::NETWORK_DOWN,
                K::NotJoined => SlStatus::NOT_JOINED,
                K::NetworkOpened => SlStatus::ZIGBEE_NETWORK_OPENED,
                K::NetworkClosed => SlStatus::ZIGBEE_NETWORK_CLOSED,
                K::DeliveryFailed => SlStatus::ZIGBEE_DELIVERY_FAILED,
                K::MaxMessageLimitReached => SlStatus::ZIGBEE_MAX_MESSAGE_LIMIT_REACHED,
                K::MessageTooLong => SlStatus::MESSAGE_TOO_LONG,
                K::InvalidCall | K::InvalidValue | K::InvalidId | K::OutOfMemory | K::EzspError => {
                    SlStatus::ZIGBEE_EZSP_ERROR
                }
                K::Other => return None,
            };
            return Some(Status::Sl(s));
        }
        match family {
            StatusFamily::Ezsp => {
                let s = match kind {
                    K::Success => EzspStatus::SUCCESS,
                    K::InvalidCall => EzspStatus::ERROR_INVALID_CALL,
                    K::InvalidValue => EzspStatus::ERROR_INVALID_VALUE,
                    K::InvalidId => EzspStatus::ERROR_INVALID_ID,
                    K::OutOfMemory => EzspStatus::ERROR_OUT_OF_MEMORY,
                    _ => return None,
                };
                Some(Status::Ezsp(s))
            }
            StatusFamily::Ember => {
                let s = match kind {
                    K::Success => EmberStatus::SUCCESS,
                    K::NetworkUp => EmberStatus::NETWORK_UP,
                    K::NetworkDown => EmberStatus::NETWORK_DOWN,
                    K::NotJoined => EmberStatus::NOT_JOINED,
                    K::NetworkOpened => EmberStatus::NETWORK_OPENED,
                    K::NetworkClosed => EmberStatus::NETWORK_CLOSED,
                    K::DeliveryFailed => EmberStatus::DELIVERY_FAILED,
                    K::MaxMessageLimitReached => EmberStatus::MAX_MESSAGE_LIMIT_REACHED,
                    K::MessageTooLong => EmberStatus::MESSAGE_TOO_LONG,
                    K::InvalidCall => EmberStatus::INVALID_CALL,
                    _ => return None,
                };
                Some(Status::Ember(s))
            }
        }
    }

    /// Version-independent meaning.
    pub fn kind(self) -> StatusKind {
        use StatusKind as K;
        match self {
            Status::Ezsp(s) => match s {
                EzspStatus::SUCCESS => K::Success,
                EzspStatus::ERROR_INVALID_CALL => K::InvalidCall,
                EzspStatus::ERROR_INVALID_VALUE => K::InvalidValue,
                EzspStatus::ERROR_INVALID_ID => K::InvalidId,
                EzspStatus::ERROR_OUT_OF_MEMORY => K::OutOfMemory,
                _ => K::Other,
            },
            Status::Ember(s) => match s {
                EmberStatus::SUCCESS => K::Success,
                EmberStatus::NETWORK_UP => K::NetworkUp,
                EmberStatus::NETWORK_DOWN => K::NetworkDown,
                EmberStatus::NOT_JOINED => K::NotJoined,
                EmberStatus::NETWORK_OPENED => K::NetworkOpened,
                EmberStatus::NETWORK_CLOSED => K::NetworkClosed,
                EmberStatus::DELIVERY_FAILED => K::DeliveryFailed,
                EmberStatus::MAX_MESSAGE_LIMIT_REACHED => K::MaxMessageLimitReached,
                EmberStatus::MESSAGE_TOO_LONG => K::MessageTooLong,
                EmberStatus::INVALID_CALL => K::InvalidCall,
                _ => K::Other,
            },
            Status::Sl(s) => match s {
                SlStatus::OK => K::Success,
                SlStatus::NETWORK_UP => K::NetworkUp,
                SlStatus::NETWORK_DOWN => K::NetworkDown,
                SlStatus::NOT_JOINED => K::NotJoined,
                SlStatus::ZIGBEE_NETWORK_OPENED => K::NetworkOpened,
                SlStatus::ZIGBEE_NETWORK_CLOSED => K::NetworkClosed,
                SlStatus::ZIGBEE_DELIVERY_FAILED => K::DeliveryFailed,
                SlStatus::ZIGBEE_MAX_MESSAGE_LIMIT_REACHED => K::MaxMessageLimitReached,
                SlStatus::MESSAGE_TOO_LONG => K::MessageTooLong,
                SlStatus::ZIGBEE_EZSP_ERROR => K::EzspError,
                _ => K::Other,
            },
        }
    }

    /// `SUCCESS` / `SL_STATUS_OK`.
    pub fn is_success(self) -> bool {
        self.kind() == StatusKind::Success
    }
}

impl std::fmt::Debug for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::Ezsp(s) => s.fmt(f),
            Status::Ember(s) => s.fmt(f),
            Status::Sl(s) => s.fmt(f),
        }
    }
}

code_type! {
    /// `EzspConfigId` / `sl_zigbee_ezsp_config_id_t` (u8); the IDs used by
    /// the core stage. Same values in EZSP 13–18 (0x01 was renamed from
    /// PACKET_BUFFER_COUNT to PACKET_BUFFER_HEAP_SIZE and is read-only in
    /// EZSP ≥ 14; UG100 successor §2 "Configuration Values").
    ConfigId(u8) {
        PACKET_BUFFER_COUNT = 0x01,
        NEIGHBOR_TABLE_SIZE = 0x02,
        APS_UNICAST_MESSAGE_COUNT = 0x03,
        BINDING_TABLE_SIZE = 0x04,
        ADDRESS_TABLE_SIZE = 0x05,
        MULTICAST_TABLE_SIZE = 0x06,
        ROUTE_TABLE_SIZE = 0x07,
        DISCOVERY_TABLE_SIZE = 0x08,
        STACK_PROFILE = 0x0C,
        SECURITY_LEVEL = 0x0D,
        MAX_HOPS = 0x10,
        MAX_END_DEVICE_CHILDREN = 0x11,
        INDIRECT_TRANSMISSION_TIMEOUT = 0x12,
        END_DEVICE_POLL_TIMEOUT = 0x13,
        TRUST_CENTER_ADDRESS_CACHE_SIZE = 0x19,
        SOURCE_ROUTE_TABLE_SIZE = 0x1A,
        FRAGMENT_WINDOW_SIZE = 0x1C,
        FRAGMENT_DELAY_MS = 0x1D,
        KEY_TABLE_SIZE = 0x1E,
        APS_ACK_TIMEOUT = 0x1F,
        APPLICATION_ZDO_FLAGS = 0x2A,
        BROADCAST_TABLE_SIZE = 0x2B,
        SUPPORTED_NETWORKS = 0x2D,
    }
}

code_type! {
    /// `EzspValueId` / `sl_zigbee_ezsp_value_id_t` (u8); the IDs used by
    /// the core stage (same values in EZSP 13–18).
    ValueId(u8) {
        UART_SYNCH_CALLBACKS = 0x04,
        MAXIMUM_INCOMING_TRANSFER_SIZE = 0x05,
        MAXIMUM_OUTGOING_TRANSFER_SIZE = 0x06,
        STACK_TOKEN_WRITING = 0x07,
        VERSION_INFO = 0x11,
    }
}

code_type! {
    /// `EmberNetworkStatus` (u8). The SDK headers list S2S states before
    /// LEAVING (= 6); the UG100 successor table shows LEAVING = 4, which
    /// contradicts `sl_zigbee_types.h`; the headers win.
    NetworkStatus(u8) {
        NO_NETWORK = 0,
        JOINING_NETWORK = 1,
        JOINED_NETWORK = 2,
        JOINED_NETWORK_NO_PARENT = 3,
        JOINED_NETWORK_S2S_INITIATOR = 4,
        JOINED_NETWORK_S2S_TARGET = 5,
        LEAVING_NETWORK = 6,
    }
}

code_type! {
    /// `EmberNodeType` (u8).
    NodeType(u8) {
        UNKNOWN_DEVICE = 0,
        COORDINATOR = 1,
        ROUTER = 2,
        END_DEVICE = 3,
        SLEEPY_END_DEVICE = 4,
    }
}

code_type! {
    /// `EmberJoinMethod` (u8).
    JoinMethod(u8) {
        USE_MAC_ASSOCIATION = 0,
        USE_NWK_REJOIN = 1,
        USE_NWK_REJOIN_HAVE_NWK_KEY = 2,
        USE_CONFIGURED_NWK_STATE = 3,
    }
}

code_type! {
    /// `EmberOutgoingMessageType` (u8). The SDK headers enumerate the alias
    /// types before BROADCAST (= 6); the UG100 successor table shows
    /// BROADCAST = 4; the headers win.
    OutgoingMessageType(u8) {
        DIRECT = 0,
        VIA_ADDRESS_TABLE = 1,
        VIA_BINDING = 2,
        MULTICAST = 3,
        MULTICAST_WITH_ALIAS = 4,
        BROADCAST_WITH_ALIAS = 5,
        BROADCAST = 6,
    }
}

code_type! {
    /// `EmberIncomingMessageType` (u8).
    IncomingMessageType(u8) {
        UNICAST = 0,
        UNICAST_REPLY = 1,
        MULTICAST = 2,
        MULTICAST_LOOPBACK = 3,
        BROADCAST = 4,
        BROADCAST_LOOPBACK = 5,
    }
}

code_type! {
    /// `EmberDeviceUpdate` (u8), status of trustCenter(Post)JoinHandler.
    DeviceUpdate(u8) {
        STANDARD_SECURITY_SECURED_REJOIN = 0,
        STANDARD_SECURITY_UNSECURED_JOIN = 1,
        DEVICE_LEFT = 2,
        STANDARD_SECURITY_UNSECURED_REJOIN = 3,
    }
}

code_type! {
    /// `EmberJoinDecision` (u8).
    JoinDecision(u8) {
        USE_PRECONFIGURED_KEY = 0,
        SEND_KEY_IN_THE_CLEAR = 1,
        DENY_JOIN = 2,
        NO_ACTION = 3,
        ALLOW_REJOINS_ONLY = 4,
    }
}

code_type! {
    /// `EmberNetworkInitBitmask` (u16), the only field of
    /// `EmberNetworkInitStruct` on the wire.
    NetworkInitBitmask(u16) {
        NO_OPTIONS = 0x0000,
        PARENT_INFO_IN_TOKEN = 0x0001,
        END_DEVICE_REJOIN_ON_REBOOT = 0x0002,
    }
}

/// `EmberApsOption` bitmask (u16, `ember-types.h`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ApsOptions(pub u16);

#[allow(missing_docs)]
impl ApsOptions {
    pub const NONE: ApsOptions = ApsOptions(0x0000);
    pub const ENCRYPTION: ApsOptions = ApsOptions(0x0020);
    pub const RETRY: ApsOptions = ApsOptions(0x0040);
    pub const ENABLE_ROUTE_DISCOVERY: ApsOptions = ApsOptions(0x0100);
    pub const FORCE_ROUTE_DISCOVERY: ApsOptions = ApsOptions(0x0200);
    pub const SOURCE_EUI64: ApsOptions = ApsOptions(0x0400);
    pub const DESTINATION_EUI64: ApsOptions = ApsOptions(0x0800);
    pub const ENABLE_ADDRESS_DISCOVERY: ApsOptions = ApsOptions(0x1000);
    pub const POLL_RESPONSE: ApsOptions = ApsOptions(0x2000);
    pub const ZDO_RESPONSE_REQUIRED: ApsOptions = ApsOptions(0x4000);
    pub const FRAGMENT: ApsOptions = ApsOptions(0x8000);

    /// Whether every bit of `other` is set.
    pub fn contains(self, other: ApsOptions) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for ApsOptions {
    type Output = ApsOptions;
    fn bitor(self, rhs: ApsOptions) -> ApsOptions {
        ApsOptions(self.0 | rhs.0)
    }
}

impl std::fmt::Debug for ApsOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ApsOptions(0x{:04X})", self.0)
    }
}

/// 16-bit network address.
pub type NodeId = u16;

/// IEEE address. `0` is the least significant byte, as sent on the wire
/// (UG100 successor §2 "Byte Order": EUI64 fields are little endian).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct Eui64(pub [u8; 8]);

impl Eui64 {
    /// From the conventional 64-bit value.
    pub fn from_u64(v: u64) -> Eui64 {
        Eui64(v.to_le_bytes())
    }

    /// The conventional 64-bit value.
    pub fn to_u64(self) -> u64 {
        u64::from_le_bytes(self.0)
    }
}

impl std::fmt::Debug for Eui64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Eui64({:016X})", self.to_u64())
    }
}

impl std::fmt::Display for Eui64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016X}", self.to_u64())
    }
}

/// `EmberApsFrame` / `sl_zigbee_aps_frame_t`: 11 bytes on the wire in
/// EZSP 13–18 (`ezsp-frame-utilities.c` append/fetch). The C struct of the
/// Simplicity SDK and the UG100 successor table also list `radius`, but
/// it is not serialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ApsFrame {
    pub profile_id: u16,
    pub cluster_id: u16,
    pub source_endpoint: u8,
    pub destination_endpoint: u8,
    pub options: ApsOptions,
    pub group_id: u16,
    pub sequence: u8,
}

impl ApsFrame {
    pub(crate) fn write(&self, w: &mut Writer) {
        w.u16(self.profile_id);
        w.u16(self.cluster_id);
        w.u8(self.source_endpoint);
        w.u8(self.destination_endpoint);
        w.u16(self.options.0);
        w.u16(self.group_id);
        w.u8(self.sequence);
    }

    pub(crate) fn read(r: &mut Reader<'_>) -> Result<ApsFrame, DecodeError> {
        Ok(ApsFrame {
            profile_id: r.u16()?,
            cluster_id: r.u16()?,
            source_endpoint: r.u8()?,
            destination_endpoint: r.u8()?,
            options: ApsOptions(r.u16()?),
            group_id: r.u16()?,
            sequence: r.u8()?,
        })
    }
}

/// `EmberNetworkParameters` / `sl_zigbee_network_parameters_t`: 20 bytes,
/// same layout in EZSP 13–18 (UG100 successor §3; `ezsp-frame-utilities.c`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NetworkParameters {
    pub extended_pan_id: [u8; 8],
    pub pan_id: u16,
    /// dBm (`int8_t radioTxPower` in the C struct).
    pub radio_tx_power: i8,
    pub radio_channel: u8,
    pub join_method: JoinMethod,
    pub nwk_manager_id: NodeId,
    pub nwk_update_id: u8,
    pub channels: u32,
}

impl NetworkParameters {
    pub(crate) fn write(&self, w: &mut Writer) {
        w.bytes(&self.extended_pan_id);
        w.u16(self.pan_id);
        w.u8(self.radio_tx_power as u8);
        w.u8(self.radio_channel);
        w.u8(self.join_method.0);
        w.u16(self.nwk_manager_id);
        w.u8(self.nwk_update_id);
        w.u32(self.channels);
    }

    pub(crate) fn read(r: &mut Reader<'_>) -> Result<NetworkParameters, DecodeError> {
        Ok(NetworkParameters {
            extended_pan_id: r.array()?,
            pan_id: r.u16()?,
            radio_tx_power: r.i8()?,
            radio_channel: r.u8()?,
            join_method: JoinMethod(r.u8()?),
            nwk_manager_id: r.u16()?,
            nwk_update_id: r.u8()?,
            channels: r.u32()?,
        })
    }
}

/// `EmberMulticastTableEntry`: `multicastId` (u16) and `endpoint` (u8)
/// on the wire; `networkIndex` is not serialized (EZSP 13–18).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MulticastTableEntry {
    pub multicast_id: u16,
    /// 0 when the entry is unused.
    pub endpoint: u8,
}

impl MulticastTableEntry {
    pub(crate) fn write(&self, w: &mut Writer) {
        w.u16(self.multicast_id);
        w.u8(self.endpoint);
    }

    pub(crate) fn read(r: &mut Reader<'_>) -> Result<MulticastTableEntry, DecodeError> {
        Ok(MulticastTableEntry {
            multicast_id: r.u16()?,
            endpoint: r.u8()?,
        })
    }
}

/// Reception details of incomingMessageHandler, normalized across
/// versions. EZSP 13 sends `lastHopLqi, lastHopRssi, sender, bindingIndex,
/// addressIndex` as separate parameters; EZSP ≥ 14 sends
/// `sl_zigbee_rx_packet_info_t` (18 bytes: sender_short_id, sender_long_id,
/// binding_index, address_index, last_hop_lqi, last_hop_rssi,
/// last_hop_timestamp).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RxPacketInfo {
    pub sender: NodeId,
    /// EZSP ≥ 14 only. Meaningful only when the APS options carry
    /// [`ApsOptions::SOURCE_EUI64`] (UG100 successor §3).
    pub sender_eui64: Option<Eui64>,
    /// 0xFF when no binding matches.
    pub binding_index: u8,
    /// 0xFF when no address-table entry matches.
    pub address_index: u8,
    pub last_hop_lqi: u8,
    pub last_hop_rssi: i8,
    /// EZSP ≥ 14 only: SFD timestamp.
    pub last_hop_timestamp: Option<u32>,
}
