//! Devices the NCP already knows: its `DEVICE` table (`TBL_READ`, spec 8.5),
//! its children (`DIAG_GET_CHILDREN`) and neighbors (`DIAG_GET_NEIGHBORS`),
//! plus the host store's device records. The hub imports them when it
//! adopted a network whose devices joined before it (no join event).

use std::collections::BTreeMap;
use std::time::Duration;

use zigbee_zrhp::types::Status;
use zigbee_zrhp::msg::diag::{DiagGetChildren, DiagGetChildrenRsp, DiagGetNeighbors, DiagGetNeighborsRsp};
use zigbee_zrhp::msg::ids;
use zigbee_zrhp::msg::tables::{TblRead, TblReadRsp};
use zigbee_zrhp::records::{Record, device_flags, record_type};

use crate::driver::{CallOpts, Client, Error};
use crate::lifecycle::Hub;
use crate::store::state::device_flags as store_flags;

/// [`NcpDevice::sources`] bits.
pub mod source {
    /// The NCP's `DEVICE` table.
    pub const TABLE: u8 = 0x01;
    /// A child of the coordinator.
    pub const CHILD: u8 = 0x02;
    /// A neighbor of the coordinator.
    pub const NEIGHBOR: u8 = 0x04;
    /// A device record of the host store.
    pub const STORE: u8 = 0x08;
}

/// Neighbor relationship "previous child" (the device has a new parent or
/// left): not evidence that it is on the network.
const PREVIOUS_CHILD: u8 = 4;
const NO_NWK: u16 = 0xFFFF;
/// Bound on pages per source (255 entries per page at most).
const MAX_PAGES: usize = 64;

/// One device of the inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NcpDevice {
    /// IEEE address (little-endian bytes, as on the wire).
    pub ieee: [u8; 8],
    /// Short address (`FFFF` unknown).
    pub nwk: u16,
    /// MAC capability, if a source knows it.
    pub capability: Option<u8>,
    /// Neighbor-table device type (0 coordinator, 1 router, 2 end device).
    pub device_type: Option<u8>,
    /// [`source`] bits.
    pub sources: u8,
}

/// Result of [`Hub::ncp_devices`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    /// Devices by IEEE, the coordinator excluded.
    pub devices: Vec<NcpDevice>,
    /// Requests the NCP answered `UNSUPPORTED_COMMAND`.
    pub unsupported: Vec<u16>,
}

fn add(map: &mut BTreeMap<[u8; 8], NcpDevice>, ieee: [u8; 8], nwk: u16, src: u8) -> &mut NcpDevice {
    let d = map.entry(ieee).or_insert(NcpDevice { ieee, nwk: NO_NWK, capability: None, device_type: None, sources: 0 });
    // Children and neighbors are live; they win over a table's address.
    if nwk < 0xFFF8 && (d.nwk == NO_NWK || src & (source::CHILD | source::NEIGHBOR) != 0) {
        d.nwk = nwk;
    }
    d.sources |= src;
    d
}

/// `Ok(false)` when the NCP does not implement the request.
fn supported<T>(r: Result<T, Error>, id: u16, out: &mut Inventory) -> Result<Option<T>, Error> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(Error::Status(Status::UnsupportedCommand)) => {
            out.unsupported.push(id);
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Reads the three NCP sources.
pub fn read(client: &Client, opts: CallOpts) -> Result<Inventory, Error> {
    let mut out = Inventory::default();
    let mut map = BTreeMap::new();

    let mut cursor = 0u16;
    for _ in 0..MAX_PAGES {
        let req = TblRead { table: record_type::DEVICE, cursor, max: 16 };
        let Some(rsp) = supported(client.call(&req, opts), ids::TBL_READ, &mut out)? else { break };
        let body: TblReadRsp = rsp.body()?;
        for e in body.entries.iter() {
            if let Record::Device(d) = e.record {
                let nwk = if d.flags & device_flags::NWK_VALID != 0 { d.nwk } else { NO_NWK };
                let dev = add(&mut map, d.ieee.0, nwk, source::TABLE);
                if let Some(c) = d.child {
                    dev.capability = Some(c.capability);
                }
            }
        }
        if body.next_cursor == 0xFFFF || body.entries.is_empty() {
            break;
        }
        cursor = body.next_cursor;
    }

    let mut start = 0u8;
    for _ in 0..MAX_PAGES {
        let r = client.call(&DiagGetChildren { start }, opts);
        let Some(rsp) = supported(r, ids::DIAG_GET_CHILDREN, &mut out)? else { break };
        let body: DiagGetChildrenRsp = rsp.body()?;
        let n = body.entries.len();
        for c in body.entries.iter() {
            add(&mut map, c.ieee.0, c.nwk, source::CHILD).capability = Some(c.capability);
        }
        start = start.saturating_add(n as u8);
        if n == 0 || start >= body.total {
            break;
        }
    }

    let mut start = 0u8;
    for _ in 0..MAX_PAGES {
        let r = client.call(&DiagGetNeighbors { start }, opts);
        let Some(rsp) = supported(r, ids::DIAG_GET_NEIGHBORS, &mut out)? else { break };
        let body: DiagGetNeighborsRsp = rsp.body()?;
        let n = body.entries.len();
        for nb in body.entries.iter().filter(|nb| nb.relationship != PREVIOUS_CHILD) {
            add(&mut map, nb.ieee.0, nb.nwk, source::NEIGHBOR).device_type = Some(nb.device_type);
        }
        start = start.saturating_add(n as u8);
        if n == 0 || start >= body.total {
            break;
        }
    }

    out.devices = map.into_values().collect();
    Ok(out)
}

impl Hub {
    /// Every device the NCP and the host store know, except the
    /// coordinator and devices without a usable IEEE address. Sources the
    /// NCP does not implement are listed in [`Inventory::unsupported`].
    pub fn ncp_devices(&self) -> Result<Inventory, Error> {
        let opts = CallOpts::timeout(Duration::from_secs(5));
        let mut inv = read(self.client(), opts)?;
        let mut map: BTreeMap<[u8; 8], NcpDevice> = inv.devices.drain(..).map(|d| (d.ieee, d)).collect();
        for r in self.store_state().devices.values().filter(|r| r.flags & store_flags::LEFT == 0) {
            let nwk = r.nwk;
            add(&mut map, r.ieee, nwk, source::STORE).capability.get_or_insert(r.capability);
        }
        let own = self.client().session().and_then(|s| s.info.as_ref().map(|i| i.ieee_active));
        inv.devices = map
            .into_values()
            .filter(|d| Some(d.ieee) != own && d.ieee != [0; 8] && d.ieee != [0xFF; 8])
            .collect();
        Ok(inv)
    }
}
