//! The hub's records for a backend whose NCP keeps the network in its own
//! NVM (`OwnershipModel::NcpNv`: EZSP, ZNP). The backend only reports the
//! network it finds ([`NetworkOwnership::Observed`]); whether the hub's
//! network-scoped records belong to it is decided here, against the
//! ownership tag `hub/owner` (design §2.5, §2.7):
//!
//! | Start | Records | Gate |
//! |---|---|---|
//! | no tag, no network-scoped records | `untagged` | open |
//! | no tag, network-scoped records (migration) | tagged `{backend, epid: null}`, `unverified` | read-only |
//! | this backend's tag | `unverified` | read-only |
//! | another backend's tag, unreadable tag | `foreign` | closed |
//!
//! | The NCP reports | Records | Device traffic |
//! |---|---|---|
//! | `Observed` matching the tag (EPID, and the key ID when both are known), or no tag yet | `owned`; the tag written or its informative fields updated | allowed |
//! | `Observed` of another network | `foreign` (nothing written) | refused |
//! | `NoNetwork`, `Unknown`, session down | unchanged | refused until the next match |
//!
//! While the operator replaces the network (`network/reset`, `form`,
//! `restore`), reports decide nothing; the new network then becomes the
//! records' (the tag is rewritten, as ZRHP's adapter does).

use std::io;
use std::sync::Mutex;

use hub_coord::{
    BackendKind, CoordStatus, NetworkIdentity, NetworkOwnership, OwnerTag, OwnershipModel,
    RegistryState,
};
use hub_store::kv::{self, scope};
use hub_store::{Gate, Guarded, JournalStats, Kv, KvStore, OWNER_KEY, Scope, Store};
use log::{info, warn};
use serde_json::{Value as J, json};

use super::Records;

/// What the records know about the NCP's network.
#[derive(Debug, Default)]
struct State {
    registry: RegistryState,
    /// The NCP's network, as last reported in this session.
    observed: Option<NetworkIdentity>,
    /// That network matches the tag.
    verified: bool,
    /// The operator is replacing the network.
    replacing: bool,
}

/// [`Records`] of an `NcpNv` backend in the hub's own store.
pub struct NcpNvRecords {
    backend: BackendKind,
    /// Locked before `state`.
    store: Mutex<Store>,
    state: Mutex<State>,
}

/// Access to the network-scoped records in `state`.
fn gate(state: RegistryState) -> Gate {
    match state {
        RegistryState::Foreign { .. } => Gate::Closed,
        RegistryState::Unverified => Gate::ReadOnly,
        RegistryState::Untagged | RegistryState::Owned => Gate::Open,
    }
}

impl NcpNvRecords {
    /// The records of `backend` in `store`, before the app loads them.
    pub fn open(backend: BackendKind, mut store: Store) -> Self {
        let registry = if store.kv_get(OWNER_KEY).is_some() {
            RegistryState::check(store.kv_get(OWNER_KEY), backend, OwnershipModel::NcpNv, None)
        } else if kv::has_network_records(&store) {
            // Written by this backend before a network was known (rules
            // while the NCP had none), or by an image without the tag.
            if let Err(e) = store.kv_put(OWNER_KEY, OwnerTag::new(backend, None).encode().as_bytes()) {
                warn!("hub: tagging the network records: {e}");
            }
            RegistryState::Unverified
        } else {
            RegistryState::Untagged
        };
        if let RegistryState::Foreign { backend, why } = registry {
            warn!("hub: network records not ours: {backend}, {why}");
        }
        Self { backend, store: Mutex::new(store), state: Mutex::new(State { registry, ..State::default() }) }
    }

    /// The backend-scoped key of the operator's TX power.
    fn tx_power_key(&self) -> String {
        format!("{}/tx_power", self.backend.as_str())
    }

    /// The TX power the operator set ([`Records::tx_power_set`]).
    pub fn tx_power(&self) -> Option<i8> {
        let key = self.tx_power_key();
        self.store().kv_get(&key).and_then(|v| std::str::from_utf8(v).ok()?.parse().ok())
    }

    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The tag of `network`, keeping a key ID the tag had when the NCP
    /// does not export its key now.
    fn put_tag(&self, store: &mut Store, network: Option<NetworkIdentity>) -> io::Result<()> {
        let mut network = network;
        if let Some(n) = &mut network
            && n.key_id.is_none()
            && let Some(OwnerTag { identity: Some(t), .. }) = store.kv_get(OWNER_KEY).and_then(OwnerTag::decode)
            && t.ext_pan_id == n.ext_pan_id
        {
            n.key_id = t.key_id;
        }
        store.kv_put(OWNER_KEY, OwnerTag::new(self.backend, network).encode().as_bytes())
    }

    /// The NCP runs `network`: the records' state against the tag (`force`:
    /// the operator made it, it becomes the records' network).
    fn decide(&self, store: &mut Store, st: &mut State, network: NetworkIdentity, force: bool) {
        st.observed = Some(network);
        let tag = store.kv_get(OWNER_KEY);
        let state = match tag {
            _ if force => RegistryState::Owned,
            None => RegistryState::Owned,
            Some(t) => RegistryState::check(Some(t), self.backend, OwnershipModel::NcpNv, Some(&network)),
        };
        match state {
            RegistryState::Owned => match self.put_tag(store, Some(network)) {
                Ok(()) => {
                    if st.registry != RegistryState::Owned {
                        info!("hub: network records owned (EPID {:02X?})", network.ext_pan_id);
                    }
                    st.registry = RegistryState::Owned;
                    st.verified = true;
                }
                Err(e) => {
                    warn!("hub: writing the ownership tag: {e}");
                    st.verified = false;
                }
            },
            foreign @ RegistryState::Foreign { backend, why } => {
                if st.registry != foreign {
                    warn!("hub: the NCP's network is not the records': {backend}, {why}");
                }
                st.registry = foreign;
                st.verified = false;
            }
            RegistryState::Untagged | RegistryState::Unverified => st.verified = false,
        }
    }
}

impl KvStore for NcpNvRecords {
    /// Network-scoped records behind the gate of [`Records::registry`].
    fn with_kv(&self, f: &mut dyn FnMut(&mut dyn Kv)) {
        let mut store = self.store();
        match gate(self.state().registry) {
            Gate::Open => f(&mut *store),
            g => f(&mut Guarded::new(&mut *store, g)),
        }
    }

    fn stats(&self) -> JournalStats {
        self.store().stats()
    }
}

impl Records for NcpNvRecords {
    fn registry(&self) -> RegistryState {
        self.state().registry
    }

    fn adopt(&self) -> io::Result<RegistryState> {
        let mut store = self.store();
        let mut st = self.state();
        self.put_tag(&mut store, st.observed)?;
        st.registry = RegistryState::Owned;
        st.verified = st.observed.is_some();
        Ok(st.registry)
    }

    fn clear(&self) -> io::Result<usize> {
        let keys: Vec<String> = {
            let store = self.store();
            store.state().kv.keys().filter(|k| scope(k) == Scope::Network).cloned().collect()
        };
        {
            let mut store = self.store();
            for k in &keys {
                store.kv_delete(k)?;
            }
        }
        self.adopt()?;
        Ok(keys.len())
    }

    fn observe(&self, ownership: Option<&NetworkOwnership>) {
        let mut store = self.store();
        let mut st = self.state();
        let network = match ownership {
            Some(NetworkOwnership::Observed(n) | NetworkOwnership::Owned(n)) => Some(*n),
            _ => None,
        };
        match network {
            Some(n) if !st.replacing => self.decide(&mut store, &mut st, n, false),
            Some(n) => {
                st.observed = Some(n);
                st.verified = false;
            }
            None => {
                st.observed = None;
                st.verified = false;
            }
        }
    }

    fn verified(&self) -> bool {
        self.state().verified
    }

    fn ownership(&self, reported: &NetworkOwnership) -> Option<NetworkOwnership> {
        let NetworkOwnership::Observed(n) = reported else { return None };
        let st = self.state();
        match st.registry {
            _ if st.verified => Some(NetworkOwnership::Owned(*n)),
            RegistryState::Foreign { why, .. } => Some(NetworkOwnership::Mismatch { found: Some(*n), why: why.into() }),
            _ => None,
        }
    }

    fn replacing(&self) {
        let mut st = self.state();
        st.replacing = true;
        st.verified = false;
    }

    fn replaced(&self, now: Option<&NetworkOwnership>) {
        let mut store = self.store();
        let mut st = self.state();
        st.replacing = false;
        match now {
            Some(NetworkOwnership::Observed(n) | NetworkOwnership::Owned(n)) => {
                self.decide(&mut store, &mut st, *n, true)
            }
            Some(NetworkOwnership::NoNetwork) => st.observed = None,
            // Failed: what the NCP reported meanwhile decides as usual.
            _ => {
                if let Some(n) = st.observed {
                    self.decide(&mut store, &mut st, n, false);
                }
            }
        }
    }

    fn tx_power_set(&self, dbm: i8) {
        let key = self.tx_power_key();
        if let Err(e) = self.store().kv_put(&key, dbm.to_string().as_bytes()) {
            warn!("hub: {key}: {e}");
        }
    }

    fn system(&self, st: &CoordStatus) -> (J, J) {
        let (volatile, j) = {
            let s = self.store();
            (s.is_volatile(), s.stats())
        };
        let store = json!({
            "volatile": volatile,
            "journal_sectors": j.sectors,
            "journal_live_sectors": j.live_sectors,
            "journal_generation": j.generation,
        });
        let ncp = json!({
            "backend": self.backend.as_str(),
            "fw": st.ncp.as_ref().map(|n| n.fw.clone()),
        });
        (store, ncp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(epid: u8, key: Option<u8>) -> NetworkIdentity {
        NetworkIdentity {
            ext_pan_id: [epid, 2, 3, 4, 5, 6, 7, 8],
            key_id: key.map(|k| NetworkIdentity::key_id_for(&[k; 16])),
            pan_id: 0x1A62,
            channel: 15,
            key_seq: 0,
            coordinator_ieee: [0x11; 8],
        }
    }

    fn observed(n: NetworkIdentity) -> NetworkOwnership {
        NetworkOwnership::Observed(n)
    }

    fn tag(r: &NcpNvRecords) -> Option<OwnerTag> {
        r.store().kv_get(OWNER_KEY).and_then(OwnerTag::decode)
    }

    fn put(r: &NcpNvRecords, k: &str) -> io::Result<()> {
        kv::with(r, |s| s.put(k, b"{}"))
    }

    #[test]
    fn a_fresh_store_is_tagged_at_the_first_network() {
        let r = NcpNvRecords::open(BackendKind::Ezsp, Store::volatile());
        assert_eq!(r.registry(), RegistryState::Untagged);
        assert!(!r.verified());
        r.observe(Some(&NetworkOwnership::NoNetwork));
        assert_eq!((r.registry(), r.verified(), tag(&r)), (RegistryState::Untagged, false, None));
        // Rules made while the NCP has no network are kept.
        put(&r, "auto/r1").unwrap();
        r.observe(Some(&observed(id(4, Some(1)))));
        assert_eq!((r.registry(), r.verified()), (RegistryState::Owned, true));
        assert_eq!(tag(&r), Some(OwnerTag::new(BackendKind::Ezsp, Some(id(4, Some(1))))));
        put(&r, "dev/zb:1").unwrap();
        // The session ends: traffic waits for the next match.
        r.observe(None);
        assert!(!r.verified());
        assert_eq!(r.registry(), RegistryState::Owned);
    }

    #[test]
    fn a_restart_reads_the_records_once_the_ncp_matches_the_tag() {
        let mut s = Store::volatile();
        s.kv_put(OWNER_KEY, OwnerTag::new(BackendKind::Ezsp, Some(id(4, Some(1)))).encode().as_bytes()).unwrap();
        s.kv_put("dev/zb:1", b"{}").unwrap();
        let r = NcpNvRecords::open(BackendKind::Ezsp, s);
        assert_eq!(r.registry(), RegistryState::Unverified);
        assert_eq!(kv::with(&r, |s| (s.gate(), s.get("dev/zb:1").is_some())), (Gate::ReadOnly, true));
        assert!(put(&r, "dev/zb:2").is_err_and(|e| e.kind() == io::ErrorKind::PermissionDenied));
        // PAN ID, channel, key sequence and coordinator are informative;
        // the key is not exportable now: the EPID decides, the key ID stays.
        let moved = NetworkIdentity { pan_id: 0x2B3C, channel: 25, key_seq: 1, coordinator_ieee: [9; 8], key_id: None, ..id(4, Some(1)) };
        r.observe(Some(&observed(moved)));
        assert_eq!((r.registry(), r.verified()), (RegistryState::Owned, true));
        let t = tag(&r).unwrap().identity.unwrap();
        assert_eq!((t.pan_id, t.channel, t.key_seq, t.key_id), (0x2B3C, 25, 1, id(4, Some(1)).key_id));
        put(&r, "dev/zb:2").unwrap();
    }

    #[test]
    fn another_network_or_backend_is_foreign_until_adopted_or_cleared() {
        for other in [id(5, Some(1)), id(4, Some(2))] {
            let mut s = Store::volatile();
            let mine = OwnerTag::new(BackendKind::Ezsp, Some(id(4, Some(1)))).encode();
            s.kv_put(OWNER_KEY, mine.as_bytes()).unwrap();
            s.kv_put("dev/zb:1", b"{}").unwrap();
            let r = NcpNvRecords::open(BackendKind::Ezsp, s);
            r.observe(Some(&observed(other)));
            assert_eq!(r.registry().as_str(), "foreign");
            assert!(!r.verified());
            assert_eq!(kv::with(&r, |s| s.get("dev/zb:1").is_some()), false, "hidden");
            assert_eq!(r.store().kv_get(OWNER_KEY), Some(mine.as_bytes()), "nothing written");
            assert_eq!(r.adopt().unwrap(), RegistryState::Owned);
            assert!(r.verified());
            assert_eq!(tag(&r).unwrap().identity, Some(other));
            assert!(kv::with(&r, |s| s.get("dev/zb:1").is_some()));
        }
        let mut s = Store::volatile();
        s.kv_put(OWNER_KEY, OwnerTag::new(BackendKind::Zrhp, Some(id(4, Some(1)))).encode().as_bytes()).unwrap();
        for k in ["dev/zb:1", "st/zb:1", "auto/r", "udef/a", "cfg/tz"] {
            s.kv_put(k, b"{}").unwrap();
        }
        let r = NcpNvRecords::open(BackendKind::Ezsp, s);
        assert!(matches!(r.registry(), RegistryState::Foreign { backend: "zrhp", .. }));
        r.observe(Some(&observed(id(4, Some(1)))));
        assert!(!r.verified() && r.registry().as_str() == "foreign", "another backend's records");
        assert_eq!(r.clear().unwrap(), 3);
        assert_eq!((r.registry(), r.verified()), (RegistryState::Owned, true));
        let left: Vec<String> = r.store().state().kv.keys().cloned().collect();
        assert_eq!(left, ["cfg/tz", OWNER_KEY, "udef/a"]);
    }

    #[test]
    fn the_tx_power_is_kept_under_the_backends_key() {
        let r = NcpNvRecords::open(BackendKind::Ezsp, Store::volatile());
        assert_eq!(r.tx_power(), None);
        r.tx_power_set(-3);
        assert_eq!(r.tx_power(), Some(-3));
        assert_eq!(r.store().kv_get("ezsp/tx_power"), Some(&b"-3"[..]));
    }

    #[test]
    fn untagged_network_records_are_tagged_at_open() {
        let mut s = Store::volatile();
        s.kv_put("auto/r1", b"{}").unwrap();
        let r = NcpNvRecords::open(BackendKind::Ezsp, s);
        assert_eq!(r.registry(), RegistryState::Unverified);
        assert_eq!(tag(&r), Some(OwnerTag::new(BackendKind::Ezsp, None)));
        r.observe(Some(&observed(id(4, None))));
        assert_eq!(r.registry(), RegistryState::Owned);
        assert_eq!(tag(&r).unwrap().identity, Some(id(4, None)));
    }

    #[test]
    fn a_replaced_network_becomes_the_records_network() {
        let r = NcpNvRecords::open(BackendKind::Ezsp, Store::volatile());
        r.observe(Some(&observed(id(4, Some(1)))));
        r.replacing();
        assert!(!r.verified());
        r.observe(Some(&NetworkOwnership::NoNetwork));
        // The new network's report may come before the job ends.
        r.observe(Some(&observed(id(6, Some(3)))));
        assert_eq!(r.registry(), RegistryState::Owned, "undecided while replacing");
        assert!(!r.verified());
        r.replaced(Some(&observed(id(6, Some(3)))));
        assert_eq!((r.registry(), r.verified()), (RegistryState::Owned, true));
        assert_eq!(tag(&r).unwrap().identity, Some(id(6, Some(3))));
        // A failed replacement: the reports decide as usual.
        r.replacing();
        r.observe(Some(&observed(id(7, Some(3)))));
        r.replaced(None);
        assert_eq!(r.registry().as_str(), "foreign");
    }
}
