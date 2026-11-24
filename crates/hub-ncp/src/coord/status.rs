//! The host core's state as the neutral [`CoordStatus`]: session, NCP
//! facts and [`Capabilities`] from the negotiation, network and ownership.

use std::sync::{Arc, Mutex};

use hub_coord::{
    BackendKind, BackupSupport, Capabilities, CoordStatus, FwUpdateMethod, GpSupport, NcpInfo,
    NetState, NetworkIdentity, NetworkOwnership, NetworkParams, OwnershipModel, RadioOps,
    SessionState, Tables,
};
use zigbee_zrhp::msg::system::{capacity, caps};

use crate::Hub;
use crate::driver::{self, NcpSession, Negotiated};
use crate::lifecycle::{HubStatus, NetworkInfo, Ownership};

/// Largest unfragmented APS payload when the NCP reports none.
pub const DEFAULT_ASDU: u16 = 82;

/// What one session's facts are: the session they were made for, the NCP
/// and its capabilities.
type SessionFacts = (Arc<NcpSession>, Arc<NcpInfo>, Arc<Capabilities>);

/// The NCP facts and capabilities of the current session, made once.
#[derive(Default)]
pub(crate) struct Facts(Mutex<Option<SessionFacts>>);

impl Facts {
    /// The facts of session `s`.
    pub(crate) fn of(&self, s: &Arc<NcpSession>) -> (Arc<NcpInfo>, Arc<Capabilities>) {
        let mut g = self.0.lock().unwrap();
        match &*g {
            Some((k, info, caps)) if Arc::ptr_eq(k, s) => (info.clone(), caps.clone()),
            _ => {
                let info = Arc::new(ncp_info(s));
                let caps = Arc::new(capabilities(&s.negotiated));
                *g = Some((s.clone(), info.clone(), caps.clone()));
                (info, caps)
            }
        }
    }
}

/// [`NcpInfo`] of a session.
pub fn ncp_info(s: &NcpSession) -> NcpInfo {
    let n = &s.negotiated;
    let info = s.info.as_ref();
    NcpInfo {
        fw: n.fw.clone(),
        protocol: (n.major, n.minor),
        build_id: n.build_id,
        chip: n.chip,
        ieee: info.map(|i| i.ieee_active),
        flash_size: info.map_or(0, |i| i.flash_size),
        board: info.map(|i| i.board.clone()).unwrap_or_default(),
        product: info.map(|i| i.product.clone()).unwrap_or_default(),
        baud: s.line.baud,
        flow_control: s.line.flow,
    }
}

/// [`Capabilities`] from `SYS_NEGOTIATE` (design §4).
pub fn capabilities(n: &Negotiated) -> Capabilities {
    let cap = |id| n.capacity(id).map(|v| v.min(u32::from(u16::MAX)) as u16);
    Capabilities {
        backend: BackendKind::Zrhp,
        ownership: OwnershipModel::HostStore,
        max_in_flight: cap(capacity::MAX_APS_INFLIGHT).unwrap_or(1),
        max_asdu: cap(capacity::MAX_ASDU).unwrap_or(DEFAULT_ASDU),
        fragmentation: n.has(caps::APS_FRAGMENTATION),
        group_membership: n.has(caps::GROUP_MEMBERSHIP),
        tables: Tables {
            children: cap(capacity::MAX_CHILDREN),
            address_map: cap(capacity::ADDRESS_MAP_CACHE),
            source_routes: cap(capacity::SOURCE_ROUTE_CACHE),
            multicast: cap(capacity::GROUP_TABLE),
            neighbors: cap(capacity::NEIGHBOR_TABLE),
            routes: cap(capacity::ROUTE_TABLE),
            bindings: cap(capacity::BINDING_TABLE),
            link_keys: cap(capacity::LINK_KEY_CACHE),
        },
        gp: GpSupport {
            sink: n.has(caps::GREEN_POWER_SINK),
            proxy: n.has(caps::GREEN_POWER_PROXY),
            sink_table: cap(capacity::GP_SINK_TABLE),
        },
        coex: n.has(caps::PTA_COEX),
        radio: RadioOps {
            energy_scan: n.has(caps::ENERGY_SCAN),
            active_scan: true,
            channel: true,
            tx_power: n.has(caps::TX_POWER),
        },
        backup: BackupSupport {
            export: n.has(caps::BACKUP_RESTORE),
            export_keys: n.has(caps::KEY_EXPORT),
            restore: n.has(caps::BACKUP_RESTORE),
        },
        fw_update: n.has(caps::NCP_FW_UPDATE).then_some(FwUpdateMethod::ZrhpAb),
        diagnostics: true,
    }
}

fn session_state(s: driver::SessionState) -> SessionState {
    match s {
        driver::SessionState::Down => SessionState::Down,
        driver::SessionState::Negotiating => SessionState::Negotiating,
        driver::SessionState::Resuming => SessionState::Resuming,
        driver::SessionState::Ready => SessionState::Ready,
        driver::SessionState::Incompatible => SessionState::Incompatible,
        driver::SessionState::Stopped => SessionState::Stopped,
    }
}

fn network_params(n: &NetworkInfo) -> NetworkParams {
    NetworkParams {
        pan_id: n.pan_id,
        ext_pan_id: n.ext_pan_id,
        channel: n.channel,
        nwk_update_id: n.nwk_update_id,
        key_seq: n.key_seq,
        // 0x7F: unknown (spec 9.3, NWK_GET_INFO before the power is set).
        tx_power: (n.tx_power != 0x7F).then_some(n.tx_power),
    }
}

/// The network as far as the NCP reported it; the key is not known here
/// (the ownership tag of design §2.7 fills it).
fn identity(st: &HubStatus, ieee: Option<[u8; 8]>) -> NetworkIdentity {
    let n = st.network.unwrap_or(NetworkInfo {
        state: 0,
        pan_id: 0,
        ext_pan_id: [0; 8],
        channel: 0,
        short: 0,
        nwk_update_id: 0,
        key_seq: 0,
        tx_power: 0,
    });
    NetworkIdentity {
        ext_pan_id: n.ext_pan_id,
        key_id: None,
        pan_id: n.pan_id,
        channel: n.channel,
        key_seq: n.key_seq,
        coordinator_ieee: ieee.unwrap_or_default(),
    }
}

fn ownership(st: &HubStatus, ieee: Option<[u8; 8]>) -> NetworkOwnership {
    match &st.ownership {
        Ownership::Unknown => NetworkOwnership::Unknown,
        Ownership::NoNetwork => NetworkOwnership::NoNetwork,
        Ownership::Owned => NetworkOwnership::Owned(identity(st, ieee)),
        Ownership::Lost => NetworkOwnership::Lost(None),
        Ownership::Mismatch(why) => NetworkOwnership::Mismatch {
            found: st.network.map(|_| identity(st, ieee)),
            why: why.clone(),
        },
        Ownership::Observed => NetworkOwnership::Observed(identity(st, ieee)),
    }
}

/// The [`CoordStatus`] of `hub`: the same `status()` and session calls as
/// before, no I/O.
pub(crate) fn status(hub: &Hub, facts: &Facts) -> CoordStatus {
    let st = hub.status();
    let (ncp, caps) = st.ncp.as_ref().map(|s| facts.of(s)).unzip();
    let ieee = ncp.as_ref().and_then(|i| i.ieee);
    CoordStatus {
        session: session_state(st.session),
        epoch: st.ncp.as_ref().map(|s| s.link.peer_cookie),
        ncp,
        caps,
        network_state: match st.network_state {
            2 => NetState::Up,
            1 => NetState::FormedDown,
            _ => NetState::None,
        },
        network: st.network.as_ref().map(network_params),
        ownership: ownership(&st, ieee),
        permit_join_s: hub.permit_join_remaining(),
    }
}
