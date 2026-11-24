//! What the current NCP session can do (design §2.4). One value per
//! session: ZRHP renegotiates after every NCP restart or update, so the
//! values can change between sessions.

use crate::BackendKind;

/// Who holds the authoritative network state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipModel {
    /// The host store (ZRHP): the backend decides ownership.
    HostStore,
    /// The NCP's own non-volatile memory (EZSP, ZNP): the hub compares
    /// the NCP's identity with its ownership tag.
    NcpNv,
}

/// Table capacities of the NCP (`None`: not reported).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tables {
    /// Child table.
    pub children: Option<u16>,
    /// Address map cache.
    pub address_map: Option<u16>,
    /// Source route cache.
    pub source_routes: Option<u16>,
    /// Group (multicast) table.
    pub multicast: Option<u16>,
    /// Neighbor table.
    pub neighbors: Option<u16>,
    /// Route table.
    pub routes: Option<u16>,
    /// Binding table.
    pub bindings: Option<u16>,
    /// Link key cache.
    pub link_keys: Option<u16>,
}

/// Green Power support.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GpSupport {
    /// The NCP runs a GP sink.
    pub sink: bool,
    /// The NCP runs a GP proxy.
    pub proxy: bool,
    /// Sink table capacity.
    pub sink_table: Option<u16>,
}

/// Radio operations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RadioOps {
    /// Energy scan.
    pub energy_scan: bool,
    /// Active (beacon) scan.
    pub active_scan: bool,
    /// Channel change.
    pub channel: bool,
    /// TX power setting.
    pub tx_power: bool,
}

/// Backup and restore support.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackupSupport {
    /// Network backup export.
    pub export: bool,
    /// The export may include keys.
    pub export_keys: bool,
    /// Restore from a backup.
    pub restore: bool,
}

/// How the NCP firmware is updated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FwUpdateMethod {
    /// ZRHP `FW_*` streaming into the inactive A/B slot.
    ZrhpAb,
    /// Gecko bootloader, XMODEM transfer of a GBL file.
    GeckoXmodem,
    /// TI ROM serial bootloader.
    TiRomBsl,
}

/// Capabilities of one NCP session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// Protocol.
    pub backend: BackendKind,
    /// Where the network state lives.
    pub ownership: OwnershipModel,
    /// APS requests the NCP accepts at once.
    pub max_in_flight: u16,
    /// Largest unfragmented APS payload.
    pub max_asdu: u16,
    /// APS fragmentation.
    pub fragmentation: bool,
    /// Group membership of the hub's endpoints.
    pub group_membership: bool,
    /// Table capacities.
    pub tables: Tables,
    /// Green Power.
    pub gp: GpSupport,
    /// Wi-Fi coexistence (PTA).
    pub coex: bool,
    /// Radio operations.
    pub radio: RadioOps,
    /// Backup and restore.
    pub backup: BackupSupport,
    /// NCP firmware update.
    pub fw_update: Option<FwUpdateMethod>,
    /// NCP diagnostics (counters, tables).
    pub diagnostics: bool,
}
