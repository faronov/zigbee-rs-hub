//! Owned mirrors of the ZRHP 7.11 Green Power records, events and remote
//! provisioning requests (zigbee-zrhp `msg::gp`, master 0580bc1). The
//! phase-1 host core converts its borrowed wire structs into these; the
//! field names and meanings are kept 1:1.

use std::fmt;

/// ZRHP message IDs of the GP group (zigbee-zrhp `msg::ids`).
#[allow(missing_docs)]
pub mod ids {
    pub const GP_SINK_CONFIG: u16 = 0x0A01;
    pub const GP_SINK_COMMISSIONING: u16 = 0x0A03;
    pub const GP_SINK_TABLE_READ: u16 = 0x0A04;
    pub const GP_SINK_TABLE_PUT: u16 = 0x0A05;
    pub const GP_SINK_REMOVE: u16 = 0x0A06;
    pub const GP_PAIRING_CONFIGURATION: u16 = 0x0A08;
    pub const GP_SEND_PAIRING: u16 = 0x0A09;
    pub const GP_TRANSLATION_UPDATE: u16 = 0x0A0A;
    pub const GP_TRANSLATION_REQUEST: u16 = 0x0A0B;
    pub const GP_REMOTE_SINK_TABLE_REQUEST: u16 = 0x0A0C;
    pub const GP_REMOTE_PROXY_TABLE_REQUEST: u16 = 0x0A0D;
    pub const EVT_GP_FRAME: u16 = 0x0A80;
    pub const EVT_GP_COMMISSIONING: u16 = 0x0A81;
    pub const EVT_GP_COMMISSIONED: u16 = 0x0A82;
    pub const EVT_GP_COMMISSIONING_FAILED: u16 = 0x0A83;
    pub const EVT_GP_DECOMMISSIONED: u16 = 0x0A84;
    pub const EVT_GP_REMOTE_STATUS: u16 = 0x0A85;
    pub const EVT_GP_REMOTE_SINK_TABLE: u16 = 0x0A86;
    pub const EVT_GP_REMOTE_PROXY_TABLE: u16 = 0x0A87;
    pub const EVT_GP_REMOTE_TRANSLATION_TABLE: u16 = 0x0A88;
}

/// GPD identifier (`app_id` 0 or 2).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum GpdId {
    /// Application ID 0.
    SrcId(u32),
    /// Application ID 2.
    Ieee {
        /// IEEE.
        ieee: u64,
        /// Endpoint.
        endpoint: u8,
    },
}

impl GpdId {
    /// Device record ID (4.1): `gp:<srcid 8 hex>` or `gp:<ieee>:<ep>`.
    pub fn device_id(&self) -> String {
        match self {
            Self::SrcId(s) => format!("gp:{s:08x}"),
            Self::Ieee { ieee, endpoint } => format!("gp:{ieee:016x}:{endpoint}"),
        }
    }
    /// SrcID for app 0.
    pub fn src_id(&self) -> Option<u32> {
        match self {
            Self::SrcId(s) => Some(*s),
            Self::Ieee { .. } => None,
        }
    }
}

impl fmt::Display for GpdId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.device_id())
    }
}

/// `R_GP_SINK` record.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GpSinkRecord {
    /// GPD.
    pub gpd_id: GpdId,
    /// DeviceID.
    pub device_id: u8,
    /// Communication mode.
    pub communication_mode: u8,
    /// Group (`FFFF` none).
    pub group: u16,
    /// Assigned alias (`FFFF` derived).
    pub assigned_alias: u16,
    /// Sink flags.
    pub flags: u8,
    /// Security level.
    pub security_level: u8,
    /// Key type.
    pub key_type: u8,
    /// Key, if exported.
    pub key: Option<[u8; 16]>,
    /// Frame counter.
    pub frame_counter: u32,
    /// Groupcast radius.
    pub groupcast_radius: u8,
}

/// `EVT_GP_FRAME.result` values.
#[allow(missing_docs)]
pub mod frame_result {
    pub const TRANSLATED: u8 = 0;
    pub const PASSED_THROUGH: u8 = 1;
    pub const COMMISSIONED: u8 = 2;
    pub const COMMISSIONING_REPLY_SENT: u8 = 3;
    pub const DECOMMISSIONED: u8 = 4;
    pub const COMMISSIONING_STEP: u8 = 5;
}

/// `EVT_GP_FRAME`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GpFrame {
    /// 1 direct, 2 notification, 3 commissioning notification.
    pub origin: u8,
    /// [`frame_result`].
    pub result: u8,
    /// GPD.
    pub gpd_id: GpdId,
    /// DeviceID.
    pub device_id: u8,
    /// GPD command.
    pub command_id: u8,
    /// Frame counter.
    pub frame_counter: u32,
    /// Security level.
    pub security_level: u8,
    /// Key type.
    pub key_type: u8,
    /// Flags.
    pub flags: u8,
    /// Proxy (`FFFF` direct).
    pub proxy_nwk: u16,
    /// GPP-GPD link.
    pub gpp_gpd_link: u8,
    /// RSSI.
    pub rssi: i8,
    /// LQI.
    pub lqi: u8,
    /// Command payload.
    pub payload: Vec<u8>,
}

/// GP events from the NCP (ZRHP 7.11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpEvent {
    /// `EVT_GP_FRAME`.
    Frame(GpFrame),
    /// `EVT_GP_COMMISSIONING`.
    Commissioning {
        /// 1 entered, 0 left.
        state: u8,
        /// Remaining window.
        window_s: u16,
        /// 1 explicit, 2 first pairing, 3 timeout.
        exit_reason: u8,
    },
    /// `EVT_GP_COMMISSIONED`.
    Commissioned {
        /// GPD.
        gpd_id: GpdId,
        /// DeviceID.
        device_id: u8,
        /// Bit0 bidirectional, bit1 key present.
        flags: u8,
        /// Sink record.
        record: GpSinkRecord,
    },
    /// `EVT_GP_COMMISSIONING_FAILED`.
    CommissioningFailed {
        /// GPD.
        gpd_id: GpdId,
        /// `SinkDrop` reason.
        reason: u8,
    },
    /// `EVT_GP_DECOMMISSIONED`.
    Decommissioned {
        /// GPD.
        gpd_id: GpdId,
    },
    /// `EVT_GP_REMOTE_STATUS`.
    RemoteStatus {
        /// Responding node.
        src_nwk: u16,
        /// TSN of the request.
        zcl_tsn: u8,
        /// GP command answered.
        command_id: u8,
        /// ZCL status.
        zcl_status: u8,
    },
    /// `EVT_GP_REMOTE_SINK_TABLE`.
    RemoteSinkTable {
        /// Sink.
        src_nwk: u16,
        /// TSN.
        zcl_tsn: u8,
        /// Status.
        status: u8,
        /// Records.
        records: Vec<GpSinkRecord>,
    },
    /// `EVT_GP_REMOTE_TRANSLATION_TABLE`.
    RemoteTranslationTable {
        /// Sink.
        src_nwk: u16,
        /// TSN.
        zcl_tsn: u8,
        /// Status.
        status: u8,
        /// Records.
        records: Vec<(GpdId, GpTranslation)>,
    },
}

/// Payload of one translation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode", content = "bytes")]
pub enum TranslationPayload {
    /// Fixed bytes (≤ 8 for a binding).
    Fixed(Vec<u8>),
    /// Derived from the GPD command (`0xFE`).
    Derived,
    /// Copied from the GPD payload (`0xFF`).
    Copy,
}

impl TranslationPayload {
    /// The wire `payload_len` value.
    pub fn payload_len(&self) -> u8 {
        match self {
            Self::Fixed(b) => b.len() as u8,
            Self::Derived => 0xFE,
            Self::Copy => 0xFF,
        }
    }
}

/// One GPD command → ZCL command translation (`GpTranslation`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct GpTranslation {
    /// GPD command.
    pub gpd_command: u8,
    /// Endpoint on the sink.
    pub endpoint: u8,
    /// Profile.
    pub profile: u16,
    /// Cluster.
    pub cluster: u16,
    /// ZCL command.
    pub zcl_command: u8,
    /// Payload.
    pub payload: TranslationPayload,
}

/// `GP_PAIRING_CONFIGURATION.actions` bits.
#[allow(missing_docs)]
pub mod pairing_actions {
    pub const EXTEND: u8 = 1;
    pub const REPLACE: u8 = 2;
    pub const REMOVE_PAIRING: u8 = 3;
    pub const REMOVE_GPD: u8 = 4;
    pub const SEND_GP_PAIRING: u8 = 0x08;
}

/// `GP_PAIRING_CONFIGURATION` (0x0A08).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PairingConfiguration {
    /// Target NWK.
    pub dst: u16,
    /// [`pairing_actions`].
    pub actions: u8,
    /// GPD.
    pub gpd_id: GpdId,
    /// DeviceID.
    pub device_id: u8,
    /// Communication mode.
    pub communication_mode: u8,
    /// Flags (MAC seq, RxOn, fixed, security, alias).
    pub flags: u8,
    /// Assigned alias.
    pub assigned_alias: u16,
    /// Groupcast radius.
    pub groupcast_radius: u8,
    /// Security level.
    pub security_level: u8,
    /// Key type.
    pub key_type: u8,
    /// Key.
    pub key: [u8; 16],
    /// Frame counter.
    pub frame_counter: u32,
    /// Groups `(group, alias)`.
    pub groups: Vec<(u16, u16)>,
    /// `NumberOfPairedEndpoints` (0x00 none).
    pub paired: u8,
    /// Paired endpoints.
    pub endpoints: Vec<u8>,
    /// Application information bits (0 none).
    pub app_info: u8,
    /// Manufacturer ID.
    pub manufacturer_id: u16,
    /// Model ID.
    pub model_id: u16,
    /// GPD commands.
    pub gpd_commands: Vec<u8>,
    /// Server clusters.
    pub server_clusters: Vec<u16>,
    /// Client clusters.
    pub client_clusters: Vec<u16>,
}

/// `GP_PAIRING_CONFIGURATION.flags`.
#[allow(missing_docs)]
pub mod pairing_config_flags {
    pub const MAC_SEQ_CAPABLE: u8 = 0x01;
    pub const RX_ON_CAPABLE: u8 = 0x02;
    pub const FIXED_LOCATION: u8 = 0x04;
    pub const SECURITY: u8 = 0x08;
    pub const ASSIGNED_ALIAS: u8 = 0x10;
}

/// `GP_TRANSLATION_UPDATE.action`.
#[allow(missing_docs)]
pub mod translation_action {
    pub const ADD: u8 = 0;
    pub const REPLACE: u8 = 1;
    pub const REMOVE: u8 = 2;
}

/// GP requests the engine issues through the transport (ZRHP 0x0A0x).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GpRequest {
    /// `GP_SINK_CONFIG` (0x0A01).
    SinkConfig {
        /// 1 NCP sink.
        mode: u8,
        /// Communication mode.
        communication_mode: u8,
        /// Group for mode 2.
        group: u16,
        /// Exit mode bits.
        exit_mode: u8,
        /// Window.
        window_s: u16,
        /// `gpsSecurityLevel` (0x00, D14).
        security_level: u8,
        /// Groupcast radius.
        groupcast_radius: u8,
    },
    /// `GP_SINK_COMMISSIONING` (0x0A03).
    SinkCommissioning {
        /// 1 enter, 0 exit.
        action: u8,
        /// Bit0 involve proxies, bit1 exit on first pairing.
        flags: u8,
        /// Window.
        window_s: u16,
    },
    /// `GP_SINK_REMOVE` (0x0A06).
    SinkRemove {
        /// GPD.
        gpd_id: GpdId,
        /// Bit0 send GP Pairing RemoveGPD.
        flags: u8,
    },
    /// `GP_PAIRING_CONFIGURATION` (0x0A08).
    PairingConfiguration(PairingConfiguration),
    /// `GP_TRANSLATION_UPDATE` (0x0A0A).
    TranslationUpdate {
        /// Target.
        dst: u16,
        /// [`translation_action`].
        action: u8,
        /// GPD.
        gpd_id: GpdId,
        /// ≤ 7 translations.
        translations: Vec<GpTranslation>,
    },
    /// `GP_TRANSLATION_REQUEST` (0x0A0B).
    TranslationRequest {
        /// Target.
        dst: u16,
        /// First entry.
        start_index: u8,
    },
    /// `GP_REMOTE_SINK_TABLE_REQUEST` (0x0A0C).
    RemoteSinkTableRequest {
        /// Target.
        dst: u16,
        /// `FF` = by GPD ID.
        index: u8,
        /// GPD.
        gpd_id: GpdId,
    },
    /// `GP_REMOTE_PROXY_TABLE_REQUEST` (0x0A0D).
    RemoteProxyTableRequest {
        /// Target.
        dst: u16,
        /// `FF` = by GPD ID.
        index: u8,
        /// GPD.
        gpd_id: GpdId,
    },
}

impl GpRequest {
    /// ZRHP message ID.
    pub fn zrhp_id(&self) -> u16 {
        match self {
            Self::SinkConfig { .. } => ids::GP_SINK_CONFIG,
            Self::SinkCommissioning { .. } => ids::GP_SINK_COMMISSIONING,
            Self::SinkRemove { .. } => ids::GP_SINK_REMOVE,
            Self::PairingConfiguration(_) => ids::GP_PAIRING_CONFIGURATION,
            Self::TranslationUpdate { .. } => ids::GP_TRANSLATION_UPDATE,
            Self::TranslationRequest { .. } => ids::GP_TRANSLATION_REQUEST,
            Self::RemoteSinkTableRequest { .. } => ids::GP_REMOTE_SINK_TABLE_REQUEST,
            Self::RemoteProxyTableRequest { .. } => ids::GP_REMOTE_PROXY_TABLE_REQUEST,
        }
    }

    /// `true` for remote provisioning requests answered by an
    /// `EVT_GP_REMOTE_*` with the request's ZCL TSN.
    pub fn is_remote(&self) -> bool {
        (ids::GP_PAIRING_CONFIGURATION..=ids::GP_REMOTE_PROXY_TABLE_REQUEST)
            .contains(&self.zrhp_id())
    }
}

/// GP cluster commands answered by `EVT_GP_REMOTE_STATUS.command_id`
/// (GP 1.1 client → server: Pairing Configuration 0x09, Translation Table
/// Update 0x07; Sink Table Request 0x0A).
#[allow(missing_docs)]
pub mod gp_cluster_cmd {
    pub const TRANSLATION_TABLE_UPDATE: u8 = 0x07;
    pub const TRANSLATION_TABLE_REQUEST: u8 = 0x08;
    pub const PAIRING_CONFIGURATION: u8 = 0x09;
    pub const SINK_TABLE_REQUEST: u8 = 0x0A;
}
