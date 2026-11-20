//! Host state kept in the store, and its journal records.

use std::collections::BTreeMap;

/// Store identity and table bookkeeping (spec 8.8, 12.1 `meta`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Meta {
    /// IEEE of the NCP this store belongs to.
    pub ncp_ieee: [u8; 8],
    /// Last seen NCP `table_epoch`.
    pub table_epoch: u32,
    /// Host database epoch (changes when the host store is recreated).
    pub host_db_epoch: u32,
    /// Highest table `rev` the host stored.
    pub host_max_rev: u32,
    /// Last NCP boot cookie (`EVT_NCP_READY.ncp_cookie`).
    pub last_ncp_cookie: u32,
}

/// The network the hub owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkRecord {
    /// PAN ID.
    pub pan_id: u16,
    /// Extended PAN ID.
    pub ext_pan_id: [u8; 8],
    /// Operating channel.
    pub channel: u8,
    /// Channel mask used for formation.
    pub channel_mask: u32,
    /// Network key.
    pub network_key: [u8; 16],
    /// Key sequence.
    pub key_seq: u8,
    /// NWK update ID.
    pub nwk_update_id: u8,
    /// TX power (dBm, antenna-referred).
    pub tx_power: i8,
    /// Network manager.
    pub nwk_manager: u16,
}

/// One logged frame-counter checkpoint (`EVT_FC_CHECKPOINT`, spec 8.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FcRecord {
    /// Key sequence.
    pub key_seq: u8,
    /// Durable limit.
    pub limit: u32,
    /// NCP journal generation.
    pub generation: u32,
}

/// Minimal device registry entry (spec 3.4: 24 B/device budget).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceRecord {
    /// IEEE.
    pub ieee: [u8; 8],
    /// Short address.
    pub nwk: u16,
    /// MAC capability.
    pub capability: u8,
    /// [`device_flags`].
    pub flags: u8,
    /// Hub time of the last event (seconds).
    pub last_seen: u32,
}

/// [`DeviceRecord::flags`] bits.
pub mod device_flags {
    /// The Trust Center delivered or verified a key.
    pub const AUTHORIZED: u8 = 0x01;
    /// The device left.
    pub const LEFT: u8 = 0x02;
}

/// An NCP firmware update started by the hub (spec 7.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FwPending {
    /// Version of the staged image.
    pub version: u32,
    /// SHA-256 of the staged image.
    pub sha256: [u8; 32],
    /// Target slot.
    pub slot: u8,
    /// `FW_ACTIVATE` was sent: the next session must confirm it.
    pub activated: bool,
}

/// A host endpoint the hub registers on every resume (`NWK_ADD_ENDPOINT`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointRecord {
    /// Endpoint (1..=240).
    pub ep: u8,
    /// Profile.
    pub profile: u16,
    /// Device ID.
    pub device_id: u16,
    /// Device version.
    pub version: u8,
    /// Input clusters.
    pub in_clusters: Vec<u16>,
    /// Output clusters.
    pub out_clusters: Vec<u16>,
}

/// Everything the store holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostState {
    /// Identity and table bookkeeping.
    pub meta: Meta,
    /// The owned network.
    pub network: Option<NetworkRecord>,
    /// Last logged checkpoint per domain (`[NWK, APS]`).
    pub fc: [Option<FcRecord>; 2],
    /// Device registry.
    pub devices: BTreeMap<[u8; 8], DeviceRecord>,
    /// Pending NCP update.
    pub fw_pending: Option<FwPending>,
    /// Host endpoints.
    pub endpoints: BTreeMap<u8, EndpointRecord>,
    /// Application key/value records (device registry, user definitions).
    pub kv: BTreeMap<String, Vec<u8>>,
    /// A multi-part value being received: `(key, parts, data so far, next part)`.
    pub kv_pending: Option<(String, u16, Vec<u8>, u16)>,
}

/// Largest data of one key/value record part (two parts fill a 4 KiB sector).
pub const KV_PART: usize = 2000;
/// Longest key.
pub const KV_KEY_MAX: usize = 96;

/// One state mutation = one journal record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Replace [`HostState::meta`].
    Meta(Meta),
    /// Set the owned network.
    Network(NetworkRecord),
    /// Forget the owned network.
    NetworkClear,
    /// Log a checkpoint; `domain` 1 NWK, 2 APS.
    Fc(u8, FcRecord),
    /// Insert or update a device.
    Device(DeviceRecord),
    /// Remove a device.
    DeviceDel([u8; 8]),
    /// Set the pending update.
    FwPending(FwPending),
    /// Clear the pending update.
    FwClear,
    /// Insert or replace a host endpoint.
    Endpoint(EndpointRecord),
    /// Remove a host endpoint.
    EndpointDel(u8),
    /// One part of a key/value record; the value replaces the old one when
    /// all `parts` parts arrived in order.
    KvPut {
        /// Key.
        key: String,
        /// Part index.
        part: u16,
        /// Number of parts.
        parts: u16,
        /// Data.
        data: Vec<u8>,
    },
    /// Remove a key.
    KvDel(String),
}

pub(crate) mod rec {
    pub const SNAP_BEGIN: u8 = 0x01;
    pub const SNAP_END: u8 = 0x02;
    pub const META: u8 = 0x10;
    pub const NETWORK: u8 = 0x11;
    pub const NETWORK_CLEAR: u8 = 0x12;
    pub const FC: u8 = 0x13;
    pub const DEVICE: u8 = 0x14;
    pub const DEVICE_DEL: u8 = 0x15;
    pub const FW_PENDING: u8 = 0x16;
    pub const FW_CLEAR: u8 = 0x17;
    pub const ENDPOINT: u8 = 0x18;
    pub const ENDPOINT_DEL: u8 = 0x19;
    pub const KV_PUT: u8 = 0x20;
    pub const KV_DEL: u8 = 0x21;
}

struct W(Vec<u8>);
impl W {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.0.extend_from_slice(v);
        self
    }
}

struct R<'a>(&'a [u8]);
impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn arr<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N).map(|b| b.try_into().unwrap())
    }
    fn list(&mut self) -> Option<Vec<u16>> {
        let n = self.u8()?;
        (0..n).map(|_| self.u16()).collect()
    }
}

impl Change {
    /// `(record type, payload)`.
    pub(crate) fn encode(&self) -> (u8, Vec<u8>) {
        let mut w = W(Vec::new());
        let t = match self {
            Self::Meta(m) => {
                w.bytes(&m.ncp_ieee).u32(m.table_epoch).u32(m.host_db_epoch).u32(m.host_max_rev).u32(m.last_ncp_cookie);
                rec::META
            }
            Self::Network(n) => {
                w.u16(n.pan_id)
                    .bytes(&n.ext_pan_id)
                    .u8(n.channel)
                    .u32(n.channel_mask)
                    .bytes(&n.network_key)
                    .u8(n.key_seq)
                    .u8(n.nwk_update_id)
                    .u8(n.tx_power as u8)
                    .u16(n.nwk_manager);
                rec::NETWORK
            }
            Self::NetworkClear => rec::NETWORK_CLEAR,
            Self::Fc(domain, f) => {
                w.u8(*domain).u8(f.key_seq).u32(f.limit).u32(f.generation);
                rec::FC
            }
            Self::Device(d) => {
                w.bytes(&d.ieee).u16(d.nwk).u8(d.capability).u8(d.flags).u32(d.last_seen);
                rec::DEVICE
            }
            Self::DeviceDel(ieee) => {
                w.bytes(ieee);
                rec::DEVICE_DEL
            }
            Self::FwPending(f) => {
                w.u32(f.version).bytes(&f.sha256).u8(f.slot).u8(f.activated as u8);
                rec::FW_PENDING
            }
            Self::FwClear => rec::FW_CLEAR,
            Self::Endpoint(e) => {
                w.u8(e.ep).u16(e.profile).u16(e.device_id).u8(e.version);
                w.u8(e.in_clusters.len() as u8);
                for c in &e.in_clusters {
                    w.u16(*c);
                }
                w.u8(e.out_clusters.len() as u8);
                for c in &e.out_clusters {
                    w.u16(*c);
                }
                rec::ENDPOINT
            }
            Self::EndpointDel(ep) => {
                w.u8(*ep);
                rec::ENDPOINT_DEL
            }
            Self::KvPut { key, part, parts, data } => {
                w.u8(key.len() as u8).bytes(key.as_bytes()).u16(*part).u16(*parts).bytes(data);
                rec::KV_PUT
            }
            Self::KvDel(key) => {
                w.u8(key.len() as u8).bytes(key.as_bytes());
                rec::KV_DEL
            }
        };
        (t, w.0)
    }

    /// Decode a record; unknown types are `None` (skipped, forward compatible).
    pub(crate) fn decode(t: u8, p: &[u8]) -> Option<Self> {
        let mut r = R(p);
        Some(match t {
            rec::META => Self::Meta(Meta {
                ncp_ieee: r.arr()?,
                table_epoch: r.u32()?,
                host_db_epoch: r.u32()?,
                host_max_rev: r.u32()?,
                last_ncp_cookie: r.u32()?,
            }),
            rec::NETWORK => Self::Network(NetworkRecord {
                pan_id: r.u16()?,
                ext_pan_id: r.arr()?,
                channel: r.u8()?,
                channel_mask: r.u32()?,
                network_key: r.arr()?,
                key_seq: r.u8()?,
                nwk_update_id: r.u8()?,
                tx_power: r.u8()? as i8,
                nwk_manager: r.u16()?,
            }),
            rec::NETWORK_CLEAR => Self::NetworkClear,
            rec::FC => {
                let domain = r.u8()?;
                Self::Fc(domain, FcRecord { key_seq: r.u8()?, limit: r.u32()?, generation: r.u32()? })
            }
            rec::DEVICE => Self::Device(DeviceRecord {
                ieee: r.arr()?,
                nwk: r.u16()?,
                capability: r.u8()?,
                flags: r.u8()?,
                last_seen: r.u32()?,
            }),
            rec::DEVICE_DEL => Self::DeviceDel(r.arr()?),
            rec::FW_PENDING => Self::FwPending(FwPending {
                version: r.u32()?,
                sha256: r.arr()?,
                slot: r.u8()?,
                activated: r.u8()? != 0,
            }),
            rec::FW_CLEAR => Self::FwClear,
            rec::ENDPOINT => Self::Endpoint(EndpointRecord {
                ep: r.u8()?,
                profile: r.u16()?,
                device_id: r.u16()?,
                version: r.u8()?,
                in_clusters: r.list()?,
                out_clusters: r.list()?,
            }),
            rec::ENDPOINT_DEL => Self::EndpointDel(r.u8()?),
            rec::KV_PUT => {
                let n = r.u8()?;
                let key = String::from_utf8(r.take(n.into())?.to_vec()).ok()?;
                let part = r.u16()?;
                let parts = r.u16()?;
                Self::KvPut { key, part, parts, data: r.0.to_vec() }
            }
            rec::KV_DEL => {
                let n = r.u8()?;
                Self::KvDel(String::from_utf8(r.take(n.into())?.to_vec()).ok()?)
            }
            _ => return None,
        })
    }
}

impl HostState {
    /// Apply one change.
    pub fn apply(&mut self, c: &Change) {
        match c {
            Change::Meta(m) => self.meta = *m,
            Change::Network(n) => self.network = Some(*n),
            Change::NetworkClear => self.network = None,
            Change::Fc(domain, f) => {
                if let Some(slot) = self.fc.get_mut(usize::from(domain.wrapping_sub(1))) {
                    *slot = Some(*f);
                }
            }
            Change::Device(d) => {
                self.devices.insert(d.ieee, *d);
            }
            Change::DeviceDel(ieee) => {
                self.devices.remove(ieee);
            }
            Change::FwPending(f) => self.fw_pending = Some(*f),
            Change::FwClear => self.fw_pending = None,
            Change::Endpoint(e) => {
                self.endpoints.insert(e.ep, e.clone());
            }
            Change::EndpointDel(ep) => {
                self.endpoints.remove(ep);
            }
            Change::KvPut { key, part, parts, data } => {
                let pending = if *part == 0 {
                    Some((key.clone(), *parts, data.clone(), 1))
                } else {
                    match self.kv_pending.take() {
                        Some((k, n, mut acc, next)) if k == *key && n == *parts && next == *part => {
                            acc.extend_from_slice(data);
                            Some((k, n, acc, next + 1))
                        }
                        // Out of sequence (a torn put): drop it, keep the old value.
                        _ => None,
                    }
                };
                match pending {
                    Some((k, n, acc, next)) if next >= n => {
                        self.kv.insert(k, acc);
                    }
                    p => self.kv_pending = p,
                }
            }
            Change::KvDel(key) => {
                self.kv.remove(key);
                if self.kv_pending.as_ref().is_some_and(|p| p.0 == *key) {
                    self.kv_pending = None;
                }
            }
        }
    }

    /// Whether applying `c` would change the state (cheap; no clone).
    pub fn would_change(&self, c: &Change) -> bool {
        match c {
            Change::Meta(m) => self.meta != *m,
            Change::Network(n) => self.network != Some(*n),
            Change::NetworkClear => self.network.is_some(),
            Change::Fc(domain, f) => self.fc.get(usize::from(domain.wrapping_sub(1))).is_some_and(|x| *x != Some(*f)),
            Change::Device(d) => self.devices.get(&d.ieee) != Some(d),
            Change::DeviceDel(ieee) => self.devices.contains_key(ieee),
            Change::FwPending(f) => self.fw_pending != Some(*f),
            Change::FwClear => self.fw_pending.is_some(),
            Change::Endpoint(e) => self.endpoints.get(&e.ep) != Some(e),
            Change::EndpointDel(ep) => self.endpoints.contains_key(ep),
            Change::KvPut { .. } => true,
            Change::KvDel(key) => self.kv.contains_key(key),
        }
    }

    /// The records that store `value` under `key`.
    pub fn kv_changes(key: &str, value: &[u8]) -> Vec<Change> {
        let chunks: Vec<&[u8]> = if value.is_empty() { vec![&[][..]] } else { value.chunks(KV_PART).collect() };
        let parts = chunks.len() as u16;
        chunks
            .into_iter()
            .enumerate()
            .map(|(i, data)| Change::KvPut { key: key.to_string(), part: i as u16, parts, data: data.to_vec() })
            .collect()
    }

    /// The changes that rebuild this state from empty (a snapshot).
    pub fn snapshot(&self) -> Vec<Change> {
        let mut out = vec![Change::Meta(self.meta)];
        out.extend(self.network.map(Change::Network));
        for (i, f) in self.fc.iter().enumerate() {
            if let Some(f) = f {
                out.push(Change::Fc(i as u8 + 1, *f));
            }
        }
        out.extend(self.devices.values().map(|d| Change::Device(*d)));
        out.extend(self.fw_pending.map(Change::FwPending));
        out.extend(self.endpoints.values().cloned().map(Change::Endpoint));
        for (k, v) in &self.kv {
            out.extend(Self::kv_changes(k, v));
        }
        out
    }
}
