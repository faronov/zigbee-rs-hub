//! Typed EZSP commands, responses and callbacks of the core stage.
//!
//! Parameter layouts follow the UG100 successor (Simplicity SDK EZSP
//! Reference Guide, docs.silabs.com 9.1.2) §4 "Configuration Frames", §6
//! "Networking Frames", §8 "Messaging Frames", §10 "Trust Center Frames",
//! §5 "Utilities Frames" for EZSP ≥ 14, and the generated
//! `command-functions.h` / `ezsp-frame-utilities.c` of Gecko SDK 4.4.6 for
//! EZSP 13. Frame IDs are identical in EZSP 13–18 (`ezsp-enum.h`).
//!
//! The coordinator frames (policies, security state, forming and leaving,
//! scans, radio power, tables, counters, security manager keys,
//! concentrator, channel change) follow the same sources; their EZSP 13
//! shapes were read in Gecko SDK 4.4.6 `command-functions.h` and
//! `ezsp-frame-utilities.{c,h}`, the EZSP ≥ 14 ones in Simplicity SDK
//! 2024.6.0 and 2025.6.3. Differences besides the status width:
//! `leaveNetwork` gains an options byte, `exportKey` answers `status, key,
//! context` instead of `key, status`, `importTransientKey` loses its flags
//! byte and `lookupNodeIdByEui64` gains a status
//! ([`EzspVersion::simplicity_layout`]). `startScan` and the security
//! manager frames already return `sl_status_t` in EZSP 13
//! ([`StatusFamily::Sl`]).
//!
//! Every type encodes and decodes in both directions so the simulator can
//! play the NCP and tests can round-trip.

use super::types::*;
use super::version::EzspVersion;
use super::wire::{DecodeError, Reader, Writer};

/// Frame IDs (16-bit, extended format).
pub mod id {
    pub const VERSION: u16 = 0x0000;
    pub const ADD_ENDPOINT: u16 = 0x0002;
    pub const NOP: u16 = 0x0005;
    pub const CALLBACK: u16 = 0x0006;
    pub const NO_CALLBACKS: u16 = 0x0007;
    pub const GET_MFG_TOKEN: u16 = 0x000B;
    pub const SET_MFG_TOKEN: u16 = 0x000C;
    pub const SET_CONCENTRATOR: u16 = 0x0010;
    pub const NETWORK_INIT: u16 = 0x0017;
    pub const NETWORK_STATE: u16 = 0x0018;
    pub const STACK_STATUS_HANDLER: u16 = 0x0019;
    pub const START_SCAN: u16 = 0x001A;
    pub const NETWORK_FOUND_HANDLER: u16 = 0x001B;
    pub const SCAN_COMPLETE_HANDLER: u16 = 0x001C;
    pub const STOP_SCAN: u16 = 0x001D;
    pub const FORM_NETWORK: u16 = 0x001E;
    pub const LEAVE_NETWORK: u16 = 0x0020;
    pub const PERMIT_JOINING: u16 = 0x0022;
    pub const CHILD_JOIN_HANDLER: u16 = 0x0023;
    /// `trustCenterJoinHandler` in EZSP 13, renamed
    /// `trustCenterPostJoinHandler` in EZSP ≥ 14 with the same ID and
    /// layout.
    pub const TRUST_CENTER_JOIN_HANDLER: u16 = 0x0024;
    pub const GET_EUI64: u16 = 0x0026;
    pub const GET_NODE_ID: u16 = 0x0027;
    pub const GET_NETWORK_PARAMETERS: u16 = 0x0028;
    pub const MAXIMUM_PAYLOAD_LENGTH: u16 = 0x0033;
    pub const SEND_UNICAST: u16 = 0x0034;
    pub const SEND_BROADCAST: u16 = 0x0036;
    pub const SEND_MULTICAST: u16 = 0x0038;
    pub const MESSAGE_SENT_HANDLER: u16 = 0x003F;
    pub const INCOMING_MESSAGE_HANDLER: u16 = 0x0045;
    pub const ENERGY_SCAN_RESULT_HANDLER: u16 = 0x0048;
    pub const GET_RANDOM_NUMBER: u16 = 0x0049;
    pub const GET_CHILD_DATA: u16 = 0x004A;
    pub const GET_CONFIGURATION_VALUE: u16 = 0x0052;
    pub const SET_CONFIGURATION_VALUE: u16 = 0x0053;
    pub const SET_POLICY: u16 = 0x0055;
    pub const INVALID_COMMAND: u16 = 0x0058;
    pub const INCOMING_ROUTE_RECORD_HANDLER: u16 = 0x0059;
    pub const LOOKUP_NODE_ID_BY_EUI64: u16 = 0x0060;
    pub const GET_MULTICAST_TABLE_ENTRY: u16 = 0x0063;
    pub const SET_MULTICAST_TABLE_ENTRY: u16 = 0x0064;
    pub const READ_AND_CLEAR_COUNTERS: u16 = 0x0065;
    pub const SET_INITIAL_SECURITY_STATE: u16 = 0x0068;
    pub const GET_CURRENT_SECURITY_STATE: u16 = 0x0069;
    pub const CLEAR_TRANSIENT_LINK_KEYS: u16 = 0x006B;
    pub const GET_NEIGHBOR: u16 = 0x0079;
    pub const NEIGHBOR_COUNT: u16 = 0x007A;
    pub const GET_ROUTE_TABLE_ENTRY: u16 = 0x007B;
    pub const ID_CONFLICT_HANDLER: u16 = 0x007C;
    pub const LAUNCH_STANDALONE_BOOTLOADER: u16 = 0x008F;
    pub const SET_RADIO_POWER: u16 = 0x0099;
    pub const ZIGBEE_KEY_ESTABLISHMENT_HANDLER: u16 = 0x009B;
    pub const ENERGY_SCAN_REQUEST: u16 = 0x009C;
    pub const GET_VALUE: u16 = 0x00AA;
    pub const SET_VALUE: u16 = 0x00AB;
    pub const SET_CHILD_DATA: u16 = 0x00AC;
    pub const CLEAR_KEY_TABLE: u16 = 0x00B1;
    pub const INCOMING_NETWORK_STATUS_HANDLER: u16 = 0x00C4;
    pub const READ_COUNTERS: u16 = 0x00F1;
    pub const GET_TOKEN_DATA: u16 = 0x0102;
    pub const SET_TOKEN_DATA: u16 = 0x0103;
    pub const IMPORT_LINK_KEY: u16 = 0x010E;
    pub const EXPORT_LINK_KEY_BY_INDEX: u16 = 0x010F;
    pub const IMPORT_TRANSIENT_KEY: u16 = 0x0111;
    pub const EXPORT_KEY: u16 = 0x0114;
    pub const GET_NETWORK_KEY_INFO: u16 = 0x0116;

    // Not typed yet (Green Power follow-up); the IDs are the same in EZSP
    // 13–18 (`ezsp-enum.h` of Gecko SDK 4.4.6 and Simplicity SDK 2024.6.0
    // / 2025.6.3).
    pub const GP_SINK_TABLE_INIT: u16 = 0x0070;
    pub const GPEP_INCOMING_MESSAGE_HANDLER: u16 = 0x00C5;
    pub const D_GP_SEND: u16 = 0x00C6;
    pub const D_GP_SENT_HANDLER: u16 = 0x00C7;
    pub const GP_PROXY_TABLE_GET_ENTRY: u16 = 0x00C8;
    pub const GP_PROXY_TABLE_PROCESS_GP_PAIRING: u16 = 0x00C9;
    pub const GP_SINK_TABLE_SET_ENTRY: u16 = 0x00DF;
    pub const GP_SINK_COMMISSION: u16 = 0x010A;
    pub const GET_APS_KEY_INFO: u16 = 0x010C;
}

/// `EMBER_NULL_NODE_ID` / `SL_ZIGBEE_NULL_NODE_ID`: no alias.
pub const NULL_NODE_ID: NodeId = 0xFFFF;
/// `RX_ON_WHEN_IDLE_BROADCAST_ADDRESS`.
pub const RX_ON_WHEN_IDLE_BROADCAST: NodeId = 0xFFFD;

/// A command could not be encoded for the negotiated version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// EZSP 13 message tags are 8 bits.
    TagTooWide(u16),
    /// Payload or value longer than its `uint8_t` length field.
    TooLong(usize),
    /// More clusters than the `uint8_t` count fields allow.
    TooManyClusters,
    /// Version-specific fields built for another version.
    WrongLayout(&'static str),
    /// A status variant of another version (NCP side).
    StatusWidth,
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::TagTooWide(t) => {
                write!(f, "message tag {t} does not fit EZSP 13's 8-bit tag")
            }
            EncodeError::TooLong(n) => write!(f, "{n} bytes exceed a uint8_t length"),
            EncodeError::TooManyClusters => write!(f, "more than 255 clusters"),
            EncodeError::WrongLayout(what) => {
                write!(f, "{what} fields do not match the negotiated EZSP version")
            }
            EncodeError::StatusWidth => {
                write!(f, "status type does not match the negotiated EZSP version")
            }
        }
    }
}

impl std::error::Error for EncodeError {}

/// `addEndpoint` parameters.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Endpoint {
    pub endpoint: u8,
    pub profile_id: u16,
    pub device_id: u16,
    /// Device version and flags (`appFlags`, `deviceVersion` in EZSP 13).
    pub app_flags: u8,
    pub input_clusters: Vec<u16>,
    pub output_clusters: Vec<u16>,
}

/// Version-dependent fields of `sendBroadcast`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BroadcastExtra {
    /// EZSP 13: none.
    V13,
    /// EZSP ≥ 14: `alias` ([`NULL_NODE_ID`] for none) and its NWK sequence.
    V14 { alias: NodeId, nwk_sequence: u8 },
}

impl BroadcastExtra {
    /// No alias, in the layout of `v`.
    pub fn none(v: EzspVersion) -> BroadcastExtra {
        if v.alias_fields() {
            BroadcastExtra::V14 {
                alias: NULL_NODE_ID,
                nwk_sequence: 0,
            }
        } else {
            BroadcastExtra::V13
        }
    }
}

/// Version-dependent fields of `sendMulticast`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MulticastExtra {
    /// EZSP 13: hops non-members forward the message (7+ = infinite).
    V13 { nonmember_radius: u8 },
    /// EZSP ≥ 14.
    V14 {
        broadcast_addr: u16,
        alias: NodeId,
        nwk_sequence: u8,
    },
}

impl MulticastExtra {
    /// No alias, in the layout of `v`.
    // UNVERIFIED: the meaning of `broadcastAddr` (EZSP ≥ 14) is not
    // documented (UG100 successor §8 repeats the nonmemberRadius text);
    // 0xFFFD follows the Zigbee-compliant multicast destination named under
    // SEND_MULTICASTS_TO_SLEEPY_ADDRESS (UG100 successor §2).
    pub fn none(v: EzspVersion, nonmember_radius: u8) -> MulticastExtra {
        if v.alias_fields() {
            MulticastExtra::V14 {
                broadcast_addr: RX_ON_WHEN_IDLE_BROADCAST,
                alias: NULL_NODE_ID,
                nwk_sequence: 0,
            }
        } else {
            MulticastExtra::V13 { nonmember_radius }
        }
    }
}

/// Host → NCP commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Must be the first command after reset; see the session for the
    /// legacy framing of the first exchange.
    Version {
        desired: u8,
    },
    GetConfigurationValue {
        id: ConfigId,
    },
    SetConfigurationValue {
        id: ConfigId,
        value: u16,
    },
    GetValue {
        id: ValueId,
    },
    SetValue {
        id: ValueId,
        value: Vec<u8>,
    },
    AddEndpoint(Endpoint),
    NetworkInit {
        bitmask: NetworkInitBitmask,
    },
    NetworkState,
    GetNetworkParameters,
    GetEui64,
    GetNodeId,
    /// 0 closes, 0xFF opens indefinitely, else seconds.
    PermitJoining {
        duration: u8,
    },
    SendUnicast {
        kind: OutgoingMessageType,
        index_or_destination: u16,
        aps: ApsFrame,
        tag: u16,
        message: Vec<u8>,
    },
    SendBroadcast {
        destination: NodeId,
        extra: BroadcastExtra,
        aps: ApsFrame,
        radius: u8,
        tag: u16,
        message: Vec<u8>,
    },
    SendMulticast {
        aps: ApsFrame,
        hops: u8,
        extra: MulticastExtra,
        tag: u16,
        message: Vec<u8>,
    },
    SetMulticastTableEntry {
        index: u8,
        entry: MulticastTableEntry,
    },
    GetMulticastTableEntry {
        index: u8,
    },
    MaximumPayloadLength,
    Nop,
    /// `setPolicy`: `decision` is a [`DecisionId`] or, for the trust center
    /// policy, a [`tc_policy`] bitmask.
    SetPolicy {
        policy: PolicyId,
        decision: u8,
    },
    SetInitialSecurityState(InitialSecurityState),
    FormNetwork(NetworkParameters),
    /// `leaveNetwork`; EZSP ≥ 14 sends `options` (0: none), EZSP 13 nothing.
    LeaveNetwork {
        options: u8,
    },
    /// `duration`: exponent, 15.36 ms × (2ⁿ + 1) per channel.
    StartScan {
        scan_type: ScanType,
        channel_mask: u32,
        duration: u8,
    },
    StopScan,
    SetRadioPower {
        power: i8,
    },
    GetNeighbor {
        index: u8,
    },
    NeighborCount,
    GetRouteTableEntry {
        index: u8,
    },
    GetChildData {
        index: u8,
    },
    ReadCounters,
    ReadAndClearCounters,
    /// `exportKey` (EZSP 13) / `secManExportKey` (EZSP ≥ 14).
    ExportKey(SecManContext),
    GetNetworkKeyInfo,
    /// `importTransientKey`; `flags` is sent by EZSP 13 only.
    ImportTransientKey {
        eui64: Eui64,
        key: KeyData,
        flags: u8,
    },
    ClearTransientLinkKeys,
    SetConcentrator {
        on: bool,
        concentrator_type: u16,
        min_time: u16,
        max_time: u16,
        route_error_threshold: u8,
        delivery_failure_threshold: u8,
        max_hops: u8,
    },
    /// ZDO Mgmt_NWK_Update_req; `duration` 0xFE is a channel change
    /// (`emberChannelChangeRequest`, Gecko SDK 4.4.6
    /// `zigbee-device-stack.h`).
    EnergyScanRequest {
        target: NodeId,
        channels: u32,
        duration: u8,
        count: u16,
    },
    GetRandomNumber,
    GetMfgToken {
        token: MfgTokenId,
    },
    /// `setMfgToken`: only while the token is erased (once).
    SetMfgToken {
        token: MfgTokenId,
        data: Vec<u8>,
    },
    LookupNodeIdByEui64 {
        eui64: Eui64,
    },
    GetCurrentSecurityState,
    /// `exportLinkKeyByIndex` (EZSP 13) / `secManExportLinkKeyByIndex`
    /// (EZSP ≥ 14): the key table entry `index`.
    ExportLinkKeyByIndex {
        index: u8,
    },
    /// `importLinkKey` / `secManImportLinkKey`: same layout in 13–18.
    ImportLinkKey {
        index: u8,
        eui64: Eui64,
        key: KeyData,
    },
    ClearKeyTable,
    /// `getTokenData` (token interface): `token` is a creator or NVM3 key
    /// ([`token`]).
    GetTokenData {
        token: u32,
        index: u32,
    },
    SetTokenData {
        token: u32,
        index: u32,
        data: Vec<u8>,
    },
    /// `setChildData`: writes the child table token entry `index`.
    SetChildData {
        index: u8,
        data: ChildData,
    },
    /// Any other frame; parameters are passed through.
    Raw {
        frame_id: u16,
        params: Vec<u8>,
    },
}

fn tag(w: &mut Writer, v: EzspVersion, tag: u16) -> Result<(), EncodeError> {
    if v.wide_message_tag() {
        w.u16(tag);
    } else {
        w.u8(u8::try_from(tag).map_err(|_| EncodeError::TagTooWide(tag))?);
    }
    Ok(())
}

fn read_tag(r: &mut Reader<'_>, v: EzspVersion) -> Result<u16, DecodeError> {
    if v.wide_message_tag() {
        r.u16()
    } else {
        r.u8().map(u16::from)
    }
}

fn payload(w: &mut Writer, m: &[u8]) -> Result<(), EncodeError> {
    if m.len() > 255 {
        return Err(EncodeError::TooLong(m.len()));
    }
    w.len_prefixed(m);
    Ok(())
}

/// `EmberTokenData` / `sl_zigbee_token_data_t` on the wire: `size` as a u32,
/// then `size` bytes (the generated code casts it to `uint8_t`).
fn token_data(w: &mut Writer, d: &[u8]) -> Result<(), EncodeError> {
    if d.len() > 255 {
        return Err(EncodeError::TooLong(d.len()));
    }
    w.u32(d.len() as u32);
    w.bytes(d);
    Ok(())
}

fn read_token_data(r: &mut Reader<'_>) -> Result<Vec<u8>, DecodeError> {
    let size = r.u32()? as u8;
    Ok(r.bytes(usize::from(size))?.to_vec())
}

impl Command {
    /// 16-bit frame ID.
    pub fn frame_id(&self) -> u16 {
        match self {
            Command::Version { .. } => id::VERSION,
            Command::GetConfigurationValue { .. } => id::GET_CONFIGURATION_VALUE,
            Command::SetConfigurationValue { .. } => id::SET_CONFIGURATION_VALUE,
            Command::GetValue { .. } => id::GET_VALUE,
            Command::SetValue { .. } => id::SET_VALUE,
            Command::AddEndpoint(_) => id::ADD_ENDPOINT,
            Command::NetworkInit { .. } => id::NETWORK_INIT,
            Command::NetworkState => id::NETWORK_STATE,
            Command::GetNetworkParameters => id::GET_NETWORK_PARAMETERS,
            Command::GetEui64 => id::GET_EUI64,
            Command::GetNodeId => id::GET_NODE_ID,
            Command::PermitJoining { .. } => id::PERMIT_JOINING,
            Command::SendUnicast { .. } => id::SEND_UNICAST,
            Command::SendBroadcast { .. } => id::SEND_BROADCAST,
            Command::SendMulticast { .. } => id::SEND_MULTICAST,
            Command::SetMulticastTableEntry { .. } => id::SET_MULTICAST_TABLE_ENTRY,
            Command::GetMulticastTableEntry { .. } => id::GET_MULTICAST_TABLE_ENTRY,
            Command::MaximumPayloadLength => id::MAXIMUM_PAYLOAD_LENGTH,
            Command::Nop => id::NOP,
            Command::SetPolicy { .. } => id::SET_POLICY,
            Command::SetInitialSecurityState(_) => id::SET_INITIAL_SECURITY_STATE,
            Command::FormNetwork(_) => id::FORM_NETWORK,
            Command::LeaveNetwork { .. } => id::LEAVE_NETWORK,
            Command::StartScan { .. } => id::START_SCAN,
            Command::StopScan => id::STOP_SCAN,
            Command::SetRadioPower { .. } => id::SET_RADIO_POWER,
            Command::GetNeighbor { .. } => id::GET_NEIGHBOR,
            Command::NeighborCount => id::NEIGHBOR_COUNT,
            Command::GetRouteTableEntry { .. } => id::GET_ROUTE_TABLE_ENTRY,
            Command::GetChildData { .. } => id::GET_CHILD_DATA,
            Command::ReadCounters => id::READ_COUNTERS,
            Command::ReadAndClearCounters => id::READ_AND_CLEAR_COUNTERS,
            Command::ExportKey(_) => id::EXPORT_KEY,
            Command::GetNetworkKeyInfo => id::GET_NETWORK_KEY_INFO,
            Command::ImportTransientKey { .. } => id::IMPORT_TRANSIENT_KEY,
            Command::ClearTransientLinkKeys => id::CLEAR_TRANSIENT_LINK_KEYS,
            Command::SetConcentrator { .. } => id::SET_CONCENTRATOR,
            Command::EnergyScanRequest { .. } => id::ENERGY_SCAN_REQUEST,
            Command::GetRandomNumber => id::GET_RANDOM_NUMBER,
            Command::GetMfgToken { .. } => id::GET_MFG_TOKEN,
            Command::LookupNodeIdByEui64 { .. } => id::LOOKUP_NODE_ID_BY_EUI64,
            Command::SetMfgToken { .. } => id::SET_MFG_TOKEN,
            Command::GetCurrentSecurityState => id::GET_CURRENT_SECURITY_STATE,
            Command::ExportLinkKeyByIndex { .. } => id::EXPORT_LINK_KEY_BY_INDEX,
            Command::ImportLinkKey { .. } => id::IMPORT_LINK_KEY,
            Command::ClearKeyTable => id::CLEAR_KEY_TABLE,
            Command::GetTokenData { .. } => id::GET_TOKEN_DATA,
            Command::SetTokenData { .. } => id::SET_TOKEN_DATA,
            Command::SetChildData { .. } => id::SET_CHILD_DATA,
            Command::Raw { frame_id, .. } => *frame_id,
        }
    }

    /// Encodes the parameters for `v`.
    pub fn encode_params(&self, v: EzspVersion) -> Result<Vec<u8>, EncodeError> {
        let mut w = Writer::new();
        match self {
            Command::Version { desired } => w.u8(*desired),
            Command::GetConfigurationValue { id } => w.u8(id.0),
            Command::SetConfigurationValue { id, value } => {
                w.u8(id.0);
                w.u16(*value);
            }
            Command::GetValue { id } => w.u8(id.0),
            Command::SetValue { id, value } => {
                w.u8(id.0);
                payload(&mut w, value)?;
            }
            Command::AddEndpoint(e) => {
                let ni = u8::try_from(e.input_clusters.len())
                    .map_err(|_| EncodeError::TooManyClusters)?;
                let no = u8::try_from(e.output_clusters.len())
                    .map_err(|_| EncodeError::TooManyClusters)?;
                w.u8(e.endpoint);
                w.u16(e.profile_id);
                w.u16(e.device_id);
                w.u8(e.app_flags);
                w.u8(ni);
                w.u8(no);
                for c in e.input_clusters.iter().chain(&e.output_clusters) {
                    w.u16(*c);
                }
            }
            Command::NetworkInit { bitmask } => w.u16(bitmask.0),
            Command::NetworkState
            | Command::GetNetworkParameters
            | Command::GetEui64
            | Command::GetNodeId
            | Command::MaximumPayloadLength
            | Command::Nop
            | Command::StopScan
            | Command::NeighborCount
            | Command::ReadCounters
            | Command::ReadAndClearCounters
            | Command::GetNetworkKeyInfo
            | Command::ClearTransientLinkKeys
            | Command::GetRandomNumber
            | Command::GetCurrentSecurityState
            | Command::ClearKeyTable => {}
            Command::SetPolicy { policy, decision } => {
                w.u8(policy.0);
                w.u8(*decision);
            }
            Command::SetInitialSecurityState(state) => state.write(&mut w),
            Command::FormNetwork(p) => p.write(&mut w),
            Command::LeaveNetwork { options } => {
                if v.simplicity_layout() {
                    w.u8(*options);
                } else if *options != 0 {
                    return Err(EncodeError::WrongLayout("leaveNetwork options"));
                }
            }
            Command::StartScan {
                scan_type,
                channel_mask,
                duration,
            } => {
                w.u8(scan_type.0);
                w.u32(*channel_mask);
                w.u8(*duration);
            }
            Command::SetRadioPower { power } => w.u8(*power as u8),
            Command::GetNeighbor { index }
            | Command::GetRouteTableEntry { index }
            | Command::GetChildData { index } => w.u8(*index),
            Command::ExportKey(context) => context.write(&mut w),
            Command::ImportTransientKey { eui64, key, flags } => {
                w.bytes(&eui64.0);
                w.bytes(key);
                if !v.simplicity_layout() {
                    w.u8(*flags);
                } else if *flags != 0 {
                    return Err(EncodeError::WrongLayout("importTransientKey flags"));
                }
            }
            Command::SetConcentrator {
                on,
                concentrator_type,
                min_time,
                max_time,
                route_error_threshold,
                delivery_failure_threshold,
                max_hops,
            } => {
                w.bool(*on);
                w.u16(*concentrator_type);
                w.u16(*min_time);
                w.u16(*max_time);
                w.u8(*route_error_threshold);
                w.u8(*delivery_failure_threshold);
                w.u8(*max_hops);
            }
            Command::EnergyScanRequest {
                target,
                channels,
                duration,
                count,
            } => {
                w.u16(*target);
                w.u32(*channels);
                w.u8(*duration);
                w.u16(*count);
            }
            Command::GetMfgToken { token } => w.u8(token.0),
            Command::SetMfgToken { token, data } => {
                w.u8(token.0);
                payload(&mut w, data)?;
            }
            Command::LookupNodeIdByEui64 { eui64 } => w.bytes(&eui64.0),
            Command::ExportLinkKeyByIndex { index } => w.u8(*index),
            Command::ImportLinkKey { index, eui64, key } => {
                w.u8(*index);
                w.bytes(&eui64.0);
                w.bytes(key);
            }
            Command::GetTokenData { token, index } => {
                w.u32(*token);
                w.u32(*index);
            }
            Command::SetTokenData { token, index, data } => {
                w.u32(*token);
                w.u32(*index);
                token_data(&mut w, data)?;
            }
            Command::SetChildData { index, data } => {
                w.u8(*index);
                data.write(&mut w);
            }
            Command::PermitJoining { duration } => w.u8(*duration),
            Command::SendUnicast {
                kind,
                index_or_destination,
                aps,
                tag: t,
                message,
            } => {
                w.u8(kind.0);
                w.u16(*index_or_destination);
                aps.write(&mut w);
                tag(&mut w, v, *t)?;
                payload(&mut w, message)?;
            }
            Command::SendBroadcast {
                destination,
                extra,
                aps,
                radius,
                tag: t,
                message,
            } => {
                match (extra, v.alias_fields()) {
                    (BroadcastExtra::V13, false) => w.u16(*destination),
                    (
                        BroadcastExtra::V14 {
                            alias,
                            nwk_sequence,
                        },
                        true,
                    ) => {
                        w.u16(*alias);
                        w.u16(*destination);
                        w.u8(*nwk_sequence);
                    }
                    _ => return Err(EncodeError::WrongLayout("sendBroadcast")),
                }
                aps.write(&mut w);
                w.u8(*radius);
                tag(&mut w, v, *t)?;
                payload(&mut w, message)?;
            }
            Command::SendMulticast {
                aps,
                hops,
                extra,
                tag: t,
                message,
            } => {
                aps.write(&mut w);
                w.u8(*hops);
                match (extra, v.alias_fields()) {
                    (MulticastExtra::V13 { nonmember_radius }, false) => w.u8(*nonmember_radius),
                    (
                        MulticastExtra::V14 {
                            broadcast_addr,
                            alias,
                            nwk_sequence,
                        },
                        true,
                    ) => {
                        w.u16(*broadcast_addr);
                        w.u16(*alias);
                        w.u8(*nwk_sequence);
                    }
                    _ => return Err(EncodeError::WrongLayout("sendMulticast")),
                }
                tag(&mut w, v, *t)?;
                payload(&mut w, message)?;
            }
            Command::SetMulticastTableEntry { index, entry } => {
                w.u8(*index);
                entry.write(&mut w);
            }
            Command::GetMulticastTableEntry { index } => w.u8(*index),
            Command::Raw { params, .. } => w.bytes(params),
        }
        Ok(w.into_inner())
    }

    /// NCP side: decodes a command. `Ok(None)` for a frame ID outside the
    /// typed set.
    pub fn decode(
        frame_id: u16,
        v: EzspVersion,
        params: &[u8],
    ) -> Result<Option<Command>, DecodeError> {
        let mut r = Reader::new(params);
        let cmd = match frame_id {
            id::VERSION => Command::Version { desired: r.u8()? },
            id::GET_CONFIGURATION_VALUE => Command::GetConfigurationValue {
                id: ConfigId(r.u8()?),
            },
            id::SET_CONFIGURATION_VALUE => Command::SetConfigurationValue {
                id: ConfigId(r.u8()?),
                value: r.u16()?,
            },
            id::GET_VALUE => Command::GetValue {
                id: ValueId(r.u8()?),
            },
            id::SET_VALUE => Command::SetValue {
                id: ValueId(r.u8()?),
                value: r.len_prefixed()?,
            },
            id::ADD_ENDPOINT => {
                let endpoint = r.u8()?;
                let profile_id = r.u16()?;
                let device_id = r.u16()?;
                let app_flags = r.u8()?;
                let ni = r.u8()?;
                let no = r.u8()?;
                let input_clusters = (0..ni).map(|_| r.u16()).collect::<Result<_, _>>()?;
                let output_clusters = (0..no).map(|_| r.u16()).collect::<Result<_, _>>()?;
                Command::AddEndpoint(Endpoint {
                    endpoint,
                    profile_id,
                    device_id,
                    app_flags,
                    input_clusters,
                    output_clusters,
                })
            }
            id::NETWORK_INIT => Command::NetworkInit {
                bitmask: NetworkInitBitmask(r.u16()?),
            },
            id::NETWORK_STATE => Command::NetworkState,
            id::GET_NETWORK_PARAMETERS => Command::GetNetworkParameters,
            id::GET_EUI64 => Command::GetEui64,
            id::GET_NODE_ID => Command::GetNodeId,
            id::PERMIT_JOINING => Command::PermitJoining { duration: r.u8()? },
            id::SEND_UNICAST => Command::SendUnicast {
                kind: OutgoingMessageType(r.u8()?),
                index_or_destination: r.u16()?,
                aps: ApsFrame::read(&mut r)?,
                tag: read_tag(&mut r, v)?,
                message: r.len_prefixed()?,
            },
            id::SEND_BROADCAST => {
                let (destination, extra) = if v.alias_fields() {
                    let alias = r.u16()?;
                    let destination = r.u16()?;
                    (
                        destination,
                        BroadcastExtra::V14 {
                            alias,
                            nwk_sequence: r.u8()?,
                        },
                    )
                } else {
                    (r.u16()?, BroadcastExtra::V13)
                };
                Command::SendBroadcast {
                    destination,
                    extra,
                    aps: ApsFrame::read(&mut r)?,
                    radius: r.u8()?,
                    tag: read_tag(&mut r, v)?,
                    message: r.len_prefixed()?,
                }
            }
            id::SEND_MULTICAST => {
                let aps = ApsFrame::read(&mut r)?;
                let hops = r.u8()?;
                let extra = if v.alias_fields() {
                    MulticastExtra::V14 {
                        broadcast_addr: r.u16()?,
                        alias: r.u16()?,
                        nwk_sequence: r.u8()?,
                    }
                } else {
                    MulticastExtra::V13 {
                        nonmember_radius: r.u8()?,
                    }
                };
                Command::SendMulticast {
                    aps,
                    hops,
                    extra,
                    tag: read_tag(&mut r, v)?,
                    message: r.len_prefixed()?,
                }
            }
            id::SET_MULTICAST_TABLE_ENTRY => Command::SetMulticastTableEntry {
                index: r.u8()?,
                entry: MulticastTableEntry::read(&mut r)?,
            },
            id::GET_MULTICAST_TABLE_ENTRY => Command::GetMulticastTableEntry { index: r.u8()? },
            id::MAXIMUM_PAYLOAD_LENGTH => Command::MaximumPayloadLength,
            id::NOP => Command::Nop,
            id::SET_POLICY => Command::SetPolicy {
                policy: PolicyId(r.u8()?),
                decision: r.u8()?,
            },
            id::SET_INITIAL_SECURITY_STATE => {
                Command::SetInitialSecurityState(InitialSecurityState::read(&mut r)?)
            }
            id::FORM_NETWORK => Command::FormNetwork(NetworkParameters::read(&mut r)?),
            id::LEAVE_NETWORK => Command::LeaveNetwork {
                options: if v.simplicity_layout() { r.u8()? } else { 0 },
            },
            id::START_SCAN => Command::StartScan {
                scan_type: ScanType(r.u8()?),
                channel_mask: r.u32()?,
                duration: r.u8()?,
            },
            id::STOP_SCAN => Command::StopScan,
            id::SET_RADIO_POWER => Command::SetRadioPower { power: r.i8()? },
            id::GET_NEIGHBOR => Command::GetNeighbor { index: r.u8()? },
            id::NEIGHBOR_COUNT => Command::NeighborCount,
            id::GET_ROUTE_TABLE_ENTRY => Command::GetRouteTableEntry { index: r.u8()? },
            id::GET_CHILD_DATA => Command::GetChildData { index: r.u8()? },
            id::READ_COUNTERS => Command::ReadCounters,
            id::READ_AND_CLEAR_COUNTERS => Command::ReadAndClearCounters,
            id::EXPORT_KEY => Command::ExportKey(SecManContext::read(&mut r)?),
            id::GET_NETWORK_KEY_INFO => Command::GetNetworkKeyInfo,
            id::IMPORT_TRANSIENT_KEY => Command::ImportTransientKey {
                eui64: Eui64(r.array()?),
                key: r.array()?,
                flags: if v.simplicity_layout() { 0 } else { r.u8()? },
            },
            id::CLEAR_TRANSIENT_LINK_KEYS => Command::ClearTransientLinkKeys,
            id::SET_CONCENTRATOR => Command::SetConcentrator {
                on: r.bool()?,
                concentrator_type: r.u16()?,
                min_time: r.u16()?,
                max_time: r.u16()?,
                route_error_threshold: r.u8()?,
                delivery_failure_threshold: r.u8()?,
                max_hops: r.u8()?,
            },
            id::ENERGY_SCAN_REQUEST => Command::EnergyScanRequest {
                target: r.u16()?,
                channels: r.u32()?,
                duration: r.u8()?,
                count: r.u16()?,
            },
            id::GET_RANDOM_NUMBER => Command::GetRandomNumber,
            id::GET_MFG_TOKEN => Command::GetMfgToken {
                token: MfgTokenId(r.u8()?),
            },
            id::LOOKUP_NODE_ID_BY_EUI64 => Command::LookupNodeIdByEui64 {
                eui64: Eui64(r.array()?),
            },
            id::SET_MFG_TOKEN => Command::SetMfgToken {
                token: MfgTokenId(r.u8()?),
                data: r.len_prefixed()?,
            },
            id::GET_CURRENT_SECURITY_STATE => Command::GetCurrentSecurityState,
            id::EXPORT_LINK_KEY_BY_INDEX => Command::ExportLinkKeyByIndex { index: r.u8()? },
            id::IMPORT_LINK_KEY => Command::ImportLinkKey {
                index: r.u8()?,
                eui64: Eui64(r.array()?),
                key: r.array()?,
            },
            id::CLEAR_KEY_TABLE => Command::ClearKeyTable,
            id::GET_TOKEN_DATA => Command::GetTokenData {
                token: r.u32()?,
                index: r.u32()?,
            },
            id::SET_TOKEN_DATA => Command::SetTokenData {
                token: r.u32()?,
                index: r.u32()?,
                data: read_token_data(&mut r)?,
            },
            id::SET_CHILD_DATA => Command::SetChildData {
                index: r.u8()?,
                data: ChildData::read(&mut r)?,
            },
            _ => return Ok(None),
        };
        r.finish()?;
        Ok(Some(cmd))
    }
}

/// NCP → host responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Version {
        protocol_version: u8,
        stack_type: u8,
        stack_version: super::version::StackVersion,
    },
    GetConfigurationValue {
        status: Status,
        value: u16,
    },
    SetConfigurationValue {
        status: Status,
    },
    GetValue {
        status: Status,
        value: Vec<u8>,
    },
    SetValue {
        status: Status,
    },
    AddEndpoint {
        status: Status,
    },
    /// `NOT_JOINED` when no network is stored.
    NetworkInit {
        status: Status,
    },
    NetworkState {
        state: NetworkStatus,
    },
    GetNetworkParameters {
        status: Status,
        node_type: NodeType,
        parameters: NetworkParameters,
    },
    GetEui64 {
        eui64: Eui64,
    },
    GetNodeId {
        node_id: NodeId,
    },
    PermitJoining {
        status: Status,
    },
    /// `sequence`: APS counter the message will use.
    SendUnicast {
        status: Status,
        sequence: u8,
    },
    SendBroadcast {
        status: Status,
        sequence: u8,
    },
    SendMulticast {
        status: Status,
        sequence: u8,
    },
    SetMulticastTableEntry {
        status: Status,
    },
    GetMulticastTableEntry {
        status: Status,
        entry: MulticastTableEntry,
    },
    MaximumPayloadLength {
        aps_length: u8,
    },
    Nop,
    SetPolicy {
        status: Status,
    },
    SetInitialSecurityState {
        status: Status,
    },
    FormNetwork {
        status: Status,
    },
    LeaveNetwork {
        status: Status,
    },
    /// `sl_status_t` in every version.
    StartScan {
        status: Status,
    },
    StopScan {
        status: Status,
    },
    SetRadioPower {
        status: Status,
    },
    GetNeighbor {
        status: Status,
        entry: NeighborTableEntry,
    },
    NeighborCount {
        count: u8,
    },
    GetRouteTableEntry {
        status: Status,
        entry: RouteTableEntry,
    },
    GetChildData {
        status: Status,
        data: ChildData,
    },
    /// One value per `EmberCounterType`: 41 in EZSP 13–16, 42 in 18.
    ReadCounters {
        values: Vec<u16>,
    },
    ReadAndClearCounters {
        values: Vec<u16>,
    },
    /// `sl_status_t` in every version. EZSP 13 sends `key, status`; EZSP ≥
    /// 14 `status, key, context` (`context` is `None` for 13).
    ExportKey {
        status: Status,
        key: KeyData,
        context: Option<SecManContext>,
    },
    GetNetworkKeyInfo {
        status: Status,
        info: NetworkKeyInfo,
    },
    ImportTransientKey {
        status: Status,
    },
    ClearTransientLinkKeys,
    SetConcentrator {
        status: Status,
    },
    EnergyScanRequest {
        status: Status,
    },
    GetRandomNumber {
        status: Status,
        value: u16,
    },
    /// The token bytes (length-prefixed on the wire).
    GetMfgToken {
        data: Vec<u8>,
    },
    /// 0xFFFE when unknown; `status` exists from EZSP 14.
    LookupNodeIdByEui64 {
        status: Option<Status>,
        node_id: NodeId,
    },
    SetMfgToken {
        status: Status,
    },
    GetCurrentSecurityState {
        status: Status,
        state: CurrentSecurityState,
    },
    /// `sl_status_t` in every version. EZSP 13 sends `eui64, key,
    /// metadata, status`; EZSP ≥ 14 `status, context, key, metadata` (the
    /// partner's EUI64 is the context's; `context` is `None` for 13).
    ExportLinkKeyByIndex {
        status: Status,
        eui64: Eui64,
        key: KeyData,
        metadata: ApsKeyMetadata,
        context: Option<SecManContext>,
    },
    ImportLinkKey {
        status: Status,
    },
    ClearKeyTable {
        status: Status,
    },
    GetTokenData {
        status: Status,
        data: Vec<u8>,
    },
    SetTokenData {
        status: Status,
    },
    SetChildData {
        status: Status,
    },
    /// `invalidCommand` (0x0058): the NCP rejected the command.
    InvalidCommand {
        reason: EzspStatus,
    },
    /// Response to [`Command::Raw`].
    Raw {
        frame_id: u16,
        params: Vec<u8>,
    },
}

impl Response {
    /// Frame ID the NCP uses for this response.
    pub fn frame_id(&self) -> u16 {
        match self {
            Response::Version { .. } => id::VERSION,
            Response::GetConfigurationValue { .. } => id::GET_CONFIGURATION_VALUE,
            Response::SetConfigurationValue { .. } => id::SET_CONFIGURATION_VALUE,
            Response::GetValue { .. } => id::GET_VALUE,
            Response::SetValue { .. } => id::SET_VALUE,
            Response::AddEndpoint { .. } => id::ADD_ENDPOINT,
            Response::NetworkInit { .. } => id::NETWORK_INIT,
            Response::NetworkState { .. } => id::NETWORK_STATE,
            Response::GetNetworkParameters { .. } => id::GET_NETWORK_PARAMETERS,
            Response::GetEui64 { .. } => id::GET_EUI64,
            Response::GetNodeId { .. } => id::GET_NODE_ID,
            Response::PermitJoining { .. } => id::PERMIT_JOINING,
            Response::SendUnicast { .. } => id::SEND_UNICAST,
            Response::SendBroadcast { .. } => id::SEND_BROADCAST,
            Response::SendMulticast { .. } => id::SEND_MULTICAST,
            Response::SetMulticastTableEntry { .. } => id::SET_MULTICAST_TABLE_ENTRY,
            Response::GetMulticastTableEntry { .. } => id::GET_MULTICAST_TABLE_ENTRY,
            Response::MaximumPayloadLength { .. } => id::MAXIMUM_PAYLOAD_LENGTH,
            Response::Nop => id::NOP,
            Response::SetPolicy { .. } => id::SET_POLICY,
            Response::SetInitialSecurityState { .. } => id::SET_INITIAL_SECURITY_STATE,
            Response::FormNetwork { .. } => id::FORM_NETWORK,
            Response::LeaveNetwork { .. } => id::LEAVE_NETWORK,
            Response::StartScan { .. } => id::START_SCAN,
            Response::StopScan { .. } => id::STOP_SCAN,
            Response::SetRadioPower { .. } => id::SET_RADIO_POWER,
            Response::GetNeighbor { .. } => id::GET_NEIGHBOR,
            Response::NeighborCount { .. } => id::NEIGHBOR_COUNT,
            Response::GetRouteTableEntry { .. } => id::GET_ROUTE_TABLE_ENTRY,
            Response::GetChildData { .. } => id::GET_CHILD_DATA,
            Response::ReadCounters { .. } => id::READ_COUNTERS,
            Response::ReadAndClearCounters { .. } => id::READ_AND_CLEAR_COUNTERS,
            Response::ExportKey { .. } => id::EXPORT_KEY,
            Response::GetNetworkKeyInfo { .. } => id::GET_NETWORK_KEY_INFO,
            Response::ImportTransientKey { .. } => id::IMPORT_TRANSIENT_KEY,
            Response::ClearTransientLinkKeys => id::CLEAR_TRANSIENT_LINK_KEYS,
            Response::SetConcentrator { .. } => id::SET_CONCENTRATOR,
            Response::EnergyScanRequest { .. } => id::ENERGY_SCAN_REQUEST,
            Response::GetRandomNumber { .. } => id::GET_RANDOM_NUMBER,
            Response::GetMfgToken { .. } => id::GET_MFG_TOKEN,
            Response::LookupNodeIdByEui64 { .. } => id::LOOKUP_NODE_ID_BY_EUI64,
            Response::SetMfgToken { .. } => id::SET_MFG_TOKEN,
            Response::GetCurrentSecurityState { .. } => id::GET_CURRENT_SECURITY_STATE,
            Response::ExportLinkKeyByIndex { .. } => id::EXPORT_LINK_KEY_BY_INDEX,
            Response::ImportLinkKey { .. } => id::IMPORT_LINK_KEY,
            Response::ClearKeyTable { .. } => id::CLEAR_KEY_TABLE,
            Response::GetTokenData { .. } => id::GET_TOKEN_DATA,
            Response::SetTokenData { .. } => id::SET_TOKEN_DATA,
            Response::SetChildData { .. } => id::SET_CHILD_DATA,
            Response::InvalidCommand { .. } => id::INVALID_COMMAND,
            Response::Raw { frame_id, .. } => *frame_id,
        }
    }

    /// The status field, for responses that have one.
    pub fn status(&self) -> Option<Status> {
        match self {
            Response::GetConfigurationValue { status, .. }
            | Response::SetConfigurationValue { status }
            | Response::GetValue { status, .. }
            | Response::SetValue { status }
            | Response::AddEndpoint { status }
            | Response::NetworkInit { status }
            | Response::GetNetworkParameters { status, .. }
            | Response::PermitJoining { status }
            | Response::SendUnicast { status, .. }
            | Response::SendBroadcast { status, .. }
            | Response::SendMulticast { status, .. }
            | Response::SetMulticastTableEntry { status }
            | Response::GetMulticastTableEntry { status, .. }
            | Response::SetPolicy { status }
            | Response::SetInitialSecurityState { status }
            | Response::FormNetwork { status }
            | Response::LeaveNetwork { status }
            | Response::StartScan { status }
            | Response::StopScan { status }
            | Response::SetRadioPower { status }
            | Response::GetNeighbor { status, .. }
            | Response::GetRouteTableEntry { status, .. }
            | Response::GetChildData { status, .. }
            | Response::ExportKey { status, .. }
            | Response::GetNetworkKeyInfo { status, .. }
            | Response::ImportTransientKey { status }
            | Response::SetConcentrator { status }
            | Response::EnergyScanRequest { status }
            | Response::GetRandomNumber { status, .. }
            | Response::SetMfgToken { status }
            | Response::GetCurrentSecurityState { status, .. }
            | Response::ExportLinkKeyByIndex { status, .. }
            | Response::ImportLinkKey { status }
            | Response::ClearKeyTable { status }
            | Response::GetTokenData { status, .. }
            | Response::SetTokenData { status }
            | Response::SetChildData { status } => Some(*status),
            Response::LookupNodeIdByEui64 { status, .. } => *status,
            _ => None,
        }
    }

    /// The response carries key material (network, trust center or link
    /// keys): never formatted into logs or errors.
    pub fn has_secret(&self) -> bool {
        matches!(
            self,
            Response::ExportKey { .. } | Response::ExportLinkKeyByIndex { .. }
        )
    }

    /// Host side: decodes the response with frame ID `frame_id`.
    pub fn decode(frame_id: u16, v: EzspVersion, params: &[u8]) -> Result<Response, DecodeError> {
        use StatusFamily::{Ember, Ezsp, Sl};
        let mut r = Reader::new(params);
        let st = |r: &mut Reader<'_>, f| Status::read(r, v, f);
        let resp = match frame_id {
            id::VERSION => Response::Version {
                protocol_version: r.u8()?,
                stack_type: r.u8()?,
                stack_version: super::version::StackVersion(r.u16()?),
            },
            id::GET_CONFIGURATION_VALUE => Response::GetConfigurationValue {
                status: st(&mut r, Ezsp)?,
                value: r.u16()?,
            },
            id::SET_CONFIGURATION_VALUE => Response::SetConfigurationValue {
                status: st(&mut r, Ezsp)?,
            },
            id::GET_VALUE => Response::GetValue {
                status: st(&mut r, Ezsp)?,
                value: r.len_prefixed()?,
            },
            id::SET_VALUE => Response::SetValue {
                status: st(&mut r, Ezsp)?,
            },
            id::ADD_ENDPOINT => Response::AddEndpoint {
                status: st(&mut r, Ezsp)?,
            },
            id::NETWORK_INIT => Response::NetworkInit {
                status: st(&mut r, Ember)?,
            },
            id::NETWORK_STATE => Response::NetworkState {
                state: NetworkStatus(r.u8()?),
            },
            id::GET_NETWORK_PARAMETERS => Response::GetNetworkParameters {
                status: st(&mut r, Ember)?,
                node_type: NodeType(r.u8()?),
                parameters: NetworkParameters::read(&mut r)?,
            },
            id::GET_EUI64 => Response::GetEui64 {
                eui64: Eui64(r.array()?),
            },
            id::GET_NODE_ID => Response::GetNodeId { node_id: r.u16()? },
            id::PERMIT_JOINING => Response::PermitJoining {
                status: st(&mut r, Ember)?,
            },
            id::SEND_UNICAST => Response::SendUnicast {
                status: st(&mut r, Ember)?,
                sequence: r.u8()?,
            },
            id::SEND_BROADCAST => Response::SendBroadcast {
                status: st(&mut r, Ember)?,
                sequence: r.u8()?,
            },
            id::SEND_MULTICAST => Response::SendMulticast {
                status: st(&mut r, Ember)?,
                sequence: r.u8()?,
            },
            id::SET_MULTICAST_TABLE_ENTRY => Response::SetMulticastTableEntry {
                status: st(&mut r, Ember)?,
            },
            id::GET_MULTICAST_TABLE_ENTRY => Response::GetMulticastTableEntry {
                status: st(&mut r, Ember)?,
                entry: MulticastTableEntry::read(&mut r)?,
            },
            id::MAXIMUM_PAYLOAD_LENGTH => Response::MaximumPayloadLength {
                aps_length: r.u8()?,
            },
            id::NOP => Response::Nop,
            id::SET_POLICY => Response::SetPolicy {
                status: st(&mut r, Ezsp)?,
            },
            id::SET_INITIAL_SECURITY_STATE => Response::SetInitialSecurityState {
                status: st(&mut r, Ember)?,
            },
            id::FORM_NETWORK => Response::FormNetwork {
                status: st(&mut r, Ember)?,
            },
            id::LEAVE_NETWORK => Response::LeaveNetwork {
                status: st(&mut r, Ember)?,
            },
            id::START_SCAN => Response::StartScan {
                status: st(&mut r, Sl)?,
            },
            id::STOP_SCAN => Response::StopScan {
                status: st(&mut r, Ember)?,
            },
            id::SET_RADIO_POWER => Response::SetRadioPower {
                status: st(&mut r, Ember)?,
            },
            id::GET_NEIGHBOR => Response::GetNeighbor {
                status: st(&mut r, Ember)?,
                entry: NeighborTableEntry::read(&mut r)?,
            },
            id::NEIGHBOR_COUNT => Response::NeighborCount { count: r.u8()? },
            id::GET_ROUTE_TABLE_ENTRY => Response::GetRouteTableEntry {
                status: st(&mut r, Ember)?,
                entry: RouteTableEntry::read(&mut r)?,
            },
            id::GET_CHILD_DATA => Response::GetChildData {
                status: st(&mut r, Ember)?,
                data: ChildData::read(&mut r)?,
            },
            id::READ_COUNTERS | id::READ_AND_CLEAR_COUNTERS => {
                let mut values = Vec::with_capacity(params.len() / 2);
                while r.remaining() >= 2 {
                    values.push(r.u16()?);
                }
                if frame_id == id::READ_COUNTERS {
                    Response::ReadCounters { values }
                } else {
                    Response::ReadAndClearCounters { values }
                }
            }
            id::EXPORT_KEY if v.simplicity_layout() => Response::ExportKey {
                status: st(&mut r, Sl)?,
                key: r.array()?,
                context: Some(SecManContext::read(&mut r)?),
            },
            id::EXPORT_KEY => {
                let key = r.array()?;
                Response::ExportKey {
                    status: st(&mut r, Sl)?,
                    key,
                    context: None,
                }
            }
            id::GET_NETWORK_KEY_INFO => Response::GetNetworkKeyInfo {
                status: st(&mut r, Sl)?,
                info: NetworkKeyInfo::read(&mut r)?,
            },
            id::IMPORT_TRANSIENT_KEY => Response::ImportTransientKey {
                status: st(&mut r, Sl)?,
            },
            id::CLEAR_TRANSIENT_LINK_KEYS => Response::ClearTransientLinkKeys,
            id::SET_CONCENTRATOR => Response::SetConcentrator {
                status: st(&mut r, Ember)?,
            },
            id::ENERGY_SCAN_REQUEST => Response::EnergyScanRequest {
                status: st(&mut r, Ember)?,
            },
            id::GET_RANDOM_NUMBER => Response::GetRandomNumber {
                status: st(&mut r, Ember)?,
                value: r.u16()?,
            },
            id::GET_MFG_TOKEN => Response::GetMfgToken {
                data: r.len_prefixed()?,
            },
            id::LOOKUP_NODE_ID_BY_EUI64 => Response::LookupNodeIdByEui64 {
                status: if v.simplicity_layout() {
                    Some(st(&mut r, Sl)?)
                } else {
                    None
                },
                node_id: r.u16()?,
            },
            id::SET_MFG_TOKEN => Response::SetMfgToken {
                status: st(&mut r, Ember)?,
            },
            id::GET_CURRENT_SECURITY_STATE => Response::GetCurrentSecurityState {
                status: st(&mut r, Ember)?,
                state: CurrentSecurityState::read(&mut r)?,
            },
            id::EXPORT_LINK_KEY_BY_INDEX if v.simplicity_layout() => {
                let status = st(&mut r, Sl)?;
                let context = SecManContext::read(&mut r)?;
                Response::ExportLinkKeyByIndex {
                    status,
                    eui64: context.eui64,
                    key: r.array()?,
                    metadata: ApsKeyMetadata::read(&mut r)?,
                    context: Some(context),
                }
            }
            id::EXPORT_LINK_KEY_BY_INDEX => {
                let eui64 = Eui64(r.array()?);
                let key = r.array()?;
                let metadata = ApsKeyMetadata::read(&mut r)?;
                Response::ExportLinkKeyByIndex {
                    status: st(&mut r, Sl)?,
                    eui64,
                    key,
                    metadata,
                    context: None,
                }
            }
            id::IMPORT_LINK_KEY => Response::ImportLinkKey {
                status: st(&mut r, Sl)?,
            },
            id::CLEAR_KEY_TABLE => Response::ClearKeyTable {
                status: st(&mut r, Ember)?,
            },
            id::GET_TOKEN_DATA => Response::GetTokenData {
                status: st(&mut r, Ember)?,
                data: read_token_data(&mut r)?,
            },
            id::SET_TOKEN_DATA => Response::SetTokenData {
                status: st(&mut r, Ember)?,
            },
            id::SET_CHILD_DATA => Response::SetChildData {
                status: st(&mut r, Ember)?,
            },
            id::INVALID_COMMAND => Response::InvalidCommand {
                reason: EzspStatus(r.u8()?),
            },
            _ => {
                return Ok(Response::Raw {
                    frame_id,
                    params: params.to_vec(),
                });
            }
        };
        r.finish()?;
        Ok(resp)
    }

    /// NCP side: encodes the parameters for `v`.
    pub fn encode_params(&self, v: EzspVersion) -> Result<Vec<u8>, EncodeError> {
        use StatusFamily::{Ember, Ezsp, Sl};
        let mut w = Writer::new();
        let st = |w: &mut Writer, s: &Status, f| {
            if !s.matches(v, f) {
                return Err(EncodeError::StatusWidth);
            }
            s.write(w);
            Ok(())
        };
        match self {
            Response::Version {
                protocol_version,
                stack_type,
                stack_version,
            } => {
                w.u8(*protocol_version);
                w.u8(*stack_type);
                w.u16(stack_version.0);
            }
            Response::GetConfigurationValue { status, value } => {
                st(&mut w, status, Ezsp)?;
                w.u16(*value);
            }
            Response::SetConfigurationValue { status }
            | Response::SetValue { status }
            | Response::AddEndpoint { status } => st(&mut w, status, Ezsp)?,
            Response::GetValue { status, value } => {
                st(&mut w, status, Ezsp)?;
                payload(&mut w, value)?;
            }
            Response::NetworkInit { status }
            | Response::PermitJoining { status }
            | Response::SetMulticastTableEntry { status } => st(&mut w, status, Ember)?,
            Response::NetworkState { state } => w.u8(state.0),
            Response::GetNetworkParameters {
                status,
                node_type,
                parameters,
            } => {
                st(&mut w, status, Ember)?;
                w.u8(node_type.0);
                parameters.write(&mut w);
            }
            Response::GetEui64 { eui64 } => w.bytes(&eui64.0),
            Response::GetNodeId { node_id } => w.u16(*node_id),
            Response::SendUnicast { status, sequence }
            | Response::SendBroadcast { status, sequence }
            | Response::SendMulticast { status, sequence } => {
                st(&mut w, status, Ember)?;
                w.u8(*sequence);
            }
            Response::GetMulticastTableEntry { status, entry } => {
                st(&mut w, status, Ember)?;
                entry.write(&mut w);
            }
            Response::MaximumPayloadLength { aps_length } => w.u8(*aps_length),
            Response::Nop | Response::ClearTransientLinkKeys => {}
            Response::SetPolicy { status } => st(&mut w, status, Ezsp)?,
            Response::SetInitialSecurityState { status }
            | Response::FormNetwork { status }
            | Response::LeaveNetwork { status }
            | Response::StopScan { status }
            | Response::SetRadioPower { status }
            | Response::SetConcentrator { status }
            | Response::EnergyScanRequest { status } => st(&mut w, status, Ember)?,
            Response::StartScan { status } | Response::ImportTransientKey { status } => {
                st(&mut w, status, Sl)?
            }
            Response::GetNeighbor { status, entry } => {
                st(&mut w, status, Ember)?;
                entry.write(&mut w);
            }
            Response::NeighborCount { count } => w.u8(*count),
            Response::GetRouteTableEntry { status, entry } => {
                st(&mut w, status, Ember)?;
                entry.write(&mut w);
            }
            Response::GetChildData { status, data } => {
                st(&mut w, status, Ember)?;
                data.write(&mut w);
            }
            Response::ReadCounters { values } | Response::ReadAndClearCounters { values } => {
                for value in values {
                    w.u16(*value);
                }
            }
            Response::ExportKey {
                status,
                key,
                context,
            } => match (v.simplicity_layout(), context) {
                (true, Some(context)) => {
                    st(&mut w, status, Sl)?;
                    w.bytes(key);
                    context.write(&mut w);
                }
                (false, None) => {
                    w.bytes(key);
                    st(&mut w, status, Sl)?;
                }
                _ => return Err(EncodeError::WrongLayout("exportKey")),
            },
            Response::GetNetworkKeyInfo { status, info } => {
                st(&mut w, status, Sl)?;
                info.write(&mut w);
            }
            Response::GetRandomNumber { status, value } => {
                st(&mut w, status, Ember)?;
                w.u16(*value);
            }
            Response::GetMfgToken { data } => payload(&mut w, data)?,
            Response::LookupNodeIdByEui64 { status, node_id } => {
                match (v.simplicity_layout(), status) {
                    (true, Some(status)) => st(&mut w, status, Sl)?,
                    (false, None) => {}
                    _ => return Err(EncodeError::WrongLayout("lookupNodeIdByEui64")),
                }
                w.u16(*node_id);
            }
            Response::SetMfgToken { status }
            | Response::ClearKeyTable { status }
            | Response::SetTokenData { status }
            | Response::SetChildData { status } => st(&mut w, status, Ember)?,
            Response::GetCurrentSecurityState { status, state } => {
                st(&mut w, status, Ember)?;
                state.write(&mut w);
            }
            Response::ExportLinkKeyByIndex {
                status,
                eui64,
                key,
                metadata,
                context,
            } => match (v.simplicity_layout(), context) {
                (true, Some(context)) => {
                    st(&mut w, status, Sl)?;
                    context.write(&mut w);
                    w.bytes(key);
                    metadata.write(&mut w);
                }
                (false, None) => {
                    w.bytes(&eui64.0);
                    w.bytes(key);
                    metadata.write(&mut w);
                    st(&mut w, status, Sl)?;
                }
                _ => return Err(EncodeError::WrongLayout("exportLinkKeyByIndex")),
            },
            Response::ImportLinkKey { status } => st(&mut w, status, Sl)?,
            Response::GetTokenData { status, data } => {
                st(&mut w, status, Ember)?;
                token_data(&mut w, data)?;
            }
            Response::InvalidCommand { reason } => w.u8(reason.0),
            Response::Raw { params, .. } => w.bytes(params),
        }
        Ok(w.into_inner())
    }
}

/// NCP → host callbacks (asynchronous on UART by default: UG100 successor
/// §2 "UART Interface Callbacks").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Callback {
    /// `stackStatusHandler`: NETWORK_UP, NETWORK_DOWN, NETWORK_OPENED, …
    StackStatus { status: Status },
    /// `messageSentHandler`. `status` is success only if an ACK was
    /// received from the destination (UG100 successor §8); for broadcasts
    /// and multicasts it reports submission. `message` is empty unless
    /// the messageContentsInCallback policy asks for it.
    MessageSent {
        status: Status,
        kind: OutgoingMessageType,
        index_or_destination: u16,
        aps: ApsFrame,
        tag: u16,
        message: Vec<u8>,
    },
    /// `incomingMessageHandler`.
    IncomingMessage {
        kind: IncomingMessageType,
        aps: ApsFrame,
        info: RxPacketInfo,
        message: Vec<u8>,
    },
    /// `trustCenterJoinHandler` (EZSP 13) / `trustCenterPostJoinHandler`
    /// (EZSP ≥ 14).
    TrustCenterJoin {
        new_node_id: NodeId,
        new_node_eui64: Eui64,
        status: DeviceUpdate,
        policy_decision: JoinDecision,
        parent_of_new_node_id: NodeId,
    },
    /// `childJoinHandler`.
    ChildJoin {
        index: u8,
        joining: bool,
        child_id: NodeId,
        child_eui64: Eui64,
        child_type: NodeType,
    },
    /// `energyScanResultHandler`: one channel of an energy scan.
    EnergyScanResult { channel: u8, max_rssi: i8 },
    /// `networkFoundHandler`: one beacon of an active scan.
    NetworkFound {
        network: ZigbeeNetwork,
        last_hop_lqi: u8,
        last_hop_rssi: i8,
    },
    /// `scanCompleteHandler`: success ends the scan; otherwise `channel`
    /// is where it failed.
    ScanComplete { channel: u8, status: Status },
    /// `incomingRouteRecordHandler` (same layout in EZSP 13–18).
    IncomingRouteRecord {
        source: NodeId,
        source_eui64: Eui64,
        last_hop_lqi: u8,
        last_hop_rssi: i8,
        relays: Vec<NodeId>,
    },
    /// Any other callback.
    Unknown { frame_id: u16, params: Vec<u8> },
}

impl Callback {
    pub fn frame_id(&self) -> u16 {
        match self {
            Callback::StackStatus { .. } => id::STACK_STATUS_HANDLER,
            Callback::MessageSent { .. } => id::MESSAGE_SENT_HANDLER,
            Callback::IncomingMessage { .. } => id::INCOMING_MESSAGE_HANDLER,
            Callback::TrustCenterJoin { .. } => id::TRUST_CENTER_JOIN_HANDLER,
            Callback::ChildJoin { .. } => id::CHILD_JOIN_HANDLER,
            Callback::EnergyScanResult { .. } => id::ENERGY_SCAN_RESULT_HANDLER,
            Callback::NetworkFound { .. } => id::NETWORK_FOUND_HANDLER,
            Callback::ScanComplete { .. } => id::SCAN_COMPLETE_HANDLER,
            Callback::IncomingRouteRecord { .. } => id::INCOMING_ROUTE_RECORD_HANDLER,
            Callback::Unknown { frame_id, .. } => *frame_id,
        }
    }

    /// Host side: [`decode_with_trailing`](Self::decode_with_trailing)
    /// without the count of bytes past the known fields.
    pub fn decode(frame_id: u16, v: EzspVersion, params: &[u8]) -> Result<Callback, DecodeError> {
        Self::decode_with_trailing(frame_id, v, params).map(|(cb, _)| cb)
    }

    /// Host side, and how many bytes followed the fields this crate knows.
    /// They are ignored, as the SDK host's `command-functions.h` reads a
    /// callback's fields and never checks for more: SONOFF's EmberZNet
    /// 7.4.5 NCP (Dongle Max) appends a byte (0x02) to
    /// `incomingMessageHandler` for a device that joined in the NCP's
    /// session; refused, every frame of such a device was lost.
    pub fn decode_with_trailing(
        frame_id: u16,
        v: EzspVersion,
        params: &[u8],
    ) -> Result<(Callback, usize), DecodeError> {
        let mut r = Reader::new(params);
        let cb = match frame_id {
            id::STACK_STATUS_HANDLER => Callback::StackStatus {
                status: Status::read(&mut r, v, StatusFamily::Ember)?,
            },
            id::MESSAGE_SENT_HANDLER if v.uses_sl_status() => {
                let status = Status::read(&mut r, v, StatusFamily::Ember)?;
                Callback::MessageSent {
                    status,
                    kind: OutgoingMessageType(r.u8()?),
                    index_or_destination: r.u16()?,
                    aps: ApsFrame::read(&mut r)?,
                    tag: read_tag(&mut r, v)?,
                    message: r.len_prefixed()?,
                }
            }
            id::MESSAGE_SENT_HANDLER => {
                let kind = OutgoingMessageType(r.u8()?);
                let index_or_destination = r.u16()?;
                let aps = ApsFrame::read(&mut r)?;
                let tag = read_tag(&mut r, v)?;
                let status = Status::read(&mut r, v, StatusFamily::Ember)?;
                Callback::MessageSent {
                    status,
                    kind,
                    index_or_destination,
                    aps,
                    tag,
                    message: r.len_prefixed()?,
                }
            }
            id::INCOMING_MESSAGE_HANDLER => {
                let kind = IncomingMessageType(r.u8()?);
                let aps = ApsFrame::read(&mut r)?;
                let info = if v.alias_fields() {
                    RxPacketInfo {
                        sender: r.u16()?,
                        sender_eui64: Some(Eui64(r.array()?)),
                        binding_index: r.u8()?,
                        address_index: r.u8()?,
                        last_hop_lqi: r.u8()?,
                        last_hop_rssi: r.i8()?,
                        last_hop_timestamp: Some(r.u32()?),
                    }
                } else {
                    let last_hop_lqi = r.u8()?;
                    let last_hop_rssi = r.i8()?;
                    RxPacketInfo {
                        sender: r.u16()?,
                        sender_eui64: None,
                        binding_index: r.u8()?,
                        address_index: r.u8()?,
                        last_hop_lqi,
                        last_hop_rssi,
                        last_hop_timestamp: None,
                    }
                };
                Callback::IncomingMessage {
                    kind,
                    aps,
                    info,
                    message: r.len_prefixed()?,
                }
            }
            id::TRUST_CENTER_JOIN_HANDLER => Callback::TrustCenterJoin {
                new_node_id: r.u16()?,
                new_node_eui64: Eui64(r.array()?),
                status: DeviceUpdate(r.u8()?),
                policy_decision: JoinDecision(r.u8()?),
                parent_of_new_node_id: r.u16()?,
            },
            id::CHILD_JOIN_HANDLER => Callback::ChildJoin {
                index: r.u8()?,
                joining: r.bool()?,
                child_id: r.u16()?,
                child_eui64: Eui64(r.array()?),
                child_type: NodeType(r.u8()?),
            },
            id::ENERGY_SCAN_RESULT_HANDLER => Callback::EnergyScanResult {
                channel: r.u8()?,
                max_rssi: r.i8()?,
            },
            id::NETWORK_FOUND_HANDLER => Callback::NetworkFound {
                network: ZigbeeNetwork::read(&mut r)?,
                last_hop_lqi: r.u8()?,
                last_hop_rssi: r.i8()?,
            },
            id::SCAN_COMPLETE_HANDLER => Callback::ScanComplete {
                channel: r.u8()?,
                status: Status::read(&mut r, v, StatusFamily::Ember)?,
            },
            id::INCOMING_ROUTE_RECORD_HANDLER => {
                let source = r.u16()?;
                let source_eui64 = Eui64(r.array()?);
                let last_hop_lqi = r.u8()?;
                let last_hop_rssi = r.i8()?;
                let n = r.u8()?;
                let relays = (0..n).map(|_| r.u16()).collect::<Result<_, _>>()?;
                Callback::IncomingRouteRecord {
                    source,
                    source_eui64,
                    last_hop_lqi,
                    last_hop_rssi,
                    relays,
                }
            }
            _ => {
                return Ok((
                    Callback::Unknown {
                        frame_id,
                        params: params.to_vec(),
                    },
                    0,
                ));
            }
        };
        Ok((cb, r.remaining()))
    }

    /// NCP side.
    pub fn encode_params(&self, v: EzspVersion) -> Result<Vec<u8>, EncodeError> {
        let mut w = Writer::new();
        let status = |w: &mut Writer, s: &Status| {
            if !s.matches(v, StatusFamily::Ember) {
                return Err(EncodeError::StatusWidth);
            }
            s.write(w);
            Ok(())
        };
        match self {
            Callback::StackStatus { status: s } => status(&mut w, s)?,
            Callback::MessageSent {
                status: s,
                kind,
                index_or_destination,
                aps,
                tag: t,
                message,
            } => {
                if v.uses_sl_status() {
                    status(&mut w, s)?;
                }
                w.u8(kind.0);
                w.u16(*index_or_destination);
                aps.write(&mut w);
                tag(&mut w, v, *t)?;
                if !v.uses_sl_status() {
                    status(&mut w, s)?;
                }
                payload(&mut w, message)?;
            }
            Callback::IncomingMessage {
                kind,
                aps,
                info,
                message,
            } => {
                w.u8(kind.0);
                aps.write(&mut w);
                if v.alias_fields() {
                    let (Some(eui), Some(ts)) = (info.sender_eui64, info.last_hop_timestamp) else {
                        return Err(EncodeError::WrongLayout(
                            "incomingMessageHandler packet info",
                        ));
                    };
                    w.u16(info.sender);
                    w.bytes(&eui.0);
                    w.u8(info.binding_index);
                    w.u8(info.address_index);
                    w.u8(info.last_hop_lqi);
                    w.u8(info.last_hop_rssi as u8);
                    w.u32(ts);
                } else {
                    if info.sender_eui64.is_some() || info.last_hop_timestamp.is_some() {
                        return Err(EncodeError::WrongLayout(
                            "incomingMessageHandler packet info",
                        ));
                    }
                    w.u8(info.last_hop_lqi);
                    w.u8(info.last_hop_rssi as u8);
                    w.u16(info.sender);
                    w.u8(info.binding_index);
                    w.u8(info.address_index);
                }
                payload(&mut w, message)?;
            }
            Callback::TrustCenterJoin {
                new_node_id,
                new_node_eui64,
                status: s,
                policy_decision,
                parent_of_new_node_id,
            } => {
                w.u16(*new_node_id);
                w.bytes(&new_node_eui64.0);
                w.u8(s.0);
                w.u8(policy_decision.0);
                w.u16(*parent_of_new_node_id);
            }
            Callback::ChildJoin {
                index,
                joining,
                child_id,
                child_eui64,
                child_type,
            } => {
                w.u8(*index);
                w.bool(*joining);
                w.u16(*child_id);
                w.bytes(&child_eui64.0);
                w.u8(child_type.0);
            }
            Callback::EnergyScanResult { channel, max_rssi } => {
                w.u8(*channel);
                w.u8(*max_rssi as u8);
            }
            Callback::NetworkFound {
                network,
                last_hop_lqi,
                last_hop_rssi,
            } => {
                network.write(&mut w);
                w.u8(*last_hop_lqi);
                w.u8(*last_hop_rssi as u8);
            }
            Callback::ScanComplete { channel, status: s } => {
                w.u8(*channel);
                status(&mut w, s)?;
            }
            Callback::IncomingRouteRecord {
                source,
                source_eui64,
                last_hop_lqi,
                last_hop_rssi,
                relays,
            } => {
                let n =
                    u8::try_from(relays.len()).map_err(|_| EncodeError::TooLong(relays.len()))?;
                w.u16(*source);
                w.bytes(&source_eui64.0);
                w.u8(*last_hop_lqi);
                w.u8(*last_hop_rssi as u8);
                w.u8(n);
                for relay in relays {
                    w.u16(*relay);
                }
            }
            Callback::Unknown { params, .. } => w.bytes(params),
        }
        Ok(w.into_inner())
    }
}
