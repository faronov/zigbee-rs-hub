//! The hub's records for a backend whose NCP keeps the network in its own
//! NVM (`OwnershipModel::NcpNv`: EZSP, ZNP). The backend only reports the
//! network it finds ([`NetworkOwnership::Observed`]); whether the hub's
//! network-scoped records belong to it is decided here, against the
//! ownership tag `hub/owner` (design §2.5, §2.7):
//!
//! | Start | Records | Gate |
//! |---|---|---|
//! | no tag, no stored ZRHP network, no network-scoped records | `untagged` | open |
//! | no tag, a ZRHP image's stored network (`NetworkRecord`) | tagged as that image's migration does (`{zrhp, epid of the record}`), `foreign` | closed |
//! | no tag, network-scoped records of unknown origin | `unverified` until `network/registry/adopt` or `clear` | closed |
//! | this backend's tag | `unverified` | read-only |
//! | another backend's tag, unreadable tag | `foreign` | closed |
//!
//! | The NCP reports | Records | Device traffic |
//! |---|---|---|
//! | `Observed` matching the tag (EPID, and the key ID when both are known), or no tag yet | `owned`; the tag written or its informative fields updated | allowed |
//! | `Observed` of another network | `foreign` (nothing written) | refused |
//! | `NoNetwork`, `Unknown`, session down | unchanged | refused until the next match |
//! | anything, while the records' origin is unknown | `unverified`, closed | refused until adopted or cleared |
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
use hub_store::{Gate, Guarded, HostState, JournalStats, Kv, KvStore, OWNER_KEY, Scope, Store};
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
    /// Network-scoped records of unknown origin (no tag, no stored
    /// network): closed, and decided only by the operator (adopt, clear,
    /// or a network formed or restored over them).
    hold: bool,
}

/// [`Records`] of an `NcpNv` backend in the hub's own store.
pub struct NcpNvRecords {
    backend: BackendKind,
    /// Locked before `state`.
    store: Mutex<Store>,
    state: Mutex<State>,
}

/// Access to the network-scoped records in `st`.
fn gate(st: &State) -> Gate {
    match st.registry {
        RegistryState::Foreign { .. } => Gate::Closed,
        RegistryState::Unverified if st.hold => Gate::Closed,
        RegistryState::Unverified => Gate::ReadOnly,
        RegistryState::Untagged | RegistryState::Owned => Gate::Open,
    }
}

/// The network a ZRHP image stored (its host records), as the identity
/// its ownership tag names (`hub_ncp::coord::owner::identity`): the key ID
/// is unknown when the network was adopted without the key (all zero).
fn zrhp_network(s: &HostState) -> Option<NetworkIdentity> {
    let n = s.network?;
    Some(NetworkIdentity {
        ext_pan_id: n.ext_pan_id,
        key_id: (n.network_key != [0; 16]).then(|| NetworkIdentity::key_id_for(&n.network_key)),
        pan_id: n.pan_id,
        channel: n.channel,
        key_seq: n.key_seq,
        coordinator_ieee: s.meta.ncp_ieee,
    })
}

impl NcpNvRecords {
    /// The records of `backend` in `store`, before the app loads them. An
    /// untagged store is never taken silently: a ZRHP image's records are
    /// tagged as that image would (and are another backend's), records of
    /// unknown origin wait for the operator.
    pub fn open(backend: BackendKind, mut store: Store) -> Self {
        let mut hold = false;
        let registry = if let Some(tag) = store.kv_get(OWNER_KEY) {
            RegistryState::check(Some(tag), backend, OwnershipModel::NcpNv, None)
        } else if let Some(network) = zrhp_network(store.state()) {
            let tag = OwnerTag::new(BackendKind::Zrhp, Some(network)).encode();
            if let Err(e) = store.kv_put(OWNER_KEY, tag.as_bytes()) {
                warn!("hub: tagging the ZRHP network records: {e}");
            }
            RegistryState::check(Some(tag.as_bytes()), backend, OwnershipModel::NcpNv, None)
        } else if kv::has_network_records(&store) {
            warn!("hub: network records without an ownership tag: closed until adopted or cleared");
            hold = true;
            RegistryState::Unverified
        } else {
            RegistryState::Untagged
        };
        if let RegistryState::Foreign { backend, why } = registry {
            warn!("hub: network records not ours: {backend}, {why}");
        }
        let st = State { registry, hold, ..State::default() };
        Self { backend, store: Mutex::new(store), state: Mutex::new(st) }
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
        if st.hold && !force {
            st.verified = false;
            return;
        }
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
                    st.hold = false;
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
        // The state is not held while `f` runs.
        let g = gate(&self.state());
        match g {
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
        st.hold = false;
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

    fn zrhp_store(network: bool, kv: &[&str]) -> Store {
        use hub_store::{Change, Meta, NetworkRecord};
        let mut s = Store::volatile();
        s.commit(Change::Meta(Meta { ncp_ieee: [0x5A, 0x51, 0, 0, 0, 0, 1, 2], ..Meta::default() })).unwrap();
        if network {
            let n = NetworkRecord {
                pan_id: 0x1A2B,
                ext_pan_id: [4, 3, 2, 1, 0, 0x4B, 0x12, 0],
                channel: 15,
                channel_mask: 1 << 15,
                network_key: [0x42; 16],
                key_seq: 0,
                nwk_update_id: 0,
                tx_power: 8,
                nwk_manager: 0,
            };
            s.commit(Change::Network(n)).unwrap();
        }
        for k in kv {
            s.kv_put(k, b"{}").unwrap();
        }
        s
    }

    /// (a) A ZRHP image's store without the tag (written before #71): tagged
    /// as the ZRHP migration does, so another backend's records.
    #[test]
    fn a_zrhp_stored_network_makes_untagged_records_foreign() {
        for kv in [&["dev/zb:1", "auto/r1"][..], &[]] {
            let r = NcpNvRecords::open(BackendKind::Ezsp, zrhp_store(true, kv));
            // The tag ZRHP's own migration writes for the same store.
            let mut zrhp = zrhp_store(true, kv);
            assert_eq!(hub_ncp::coord::owner::open(&mut zrhp, true), RegistryState::Owned);
            assert_eq!(r.store().kv_get(OWNER_KEY), zrhp.kv_get(OWNER_KEY), "{kv:?}");
            assert_eq!(
                r.registry(),
                RegistryState::Foreign { backend: "zrhp", why: "another coordinator backend" }
            );
            assert_eq!(kv::with(&r, |s| (s.gate(), s.get("dev/zb:1").is_some())), (Gate::Closed, false));
            // The NCP runs the same network: still another backend's.
            let n = zrhp_network(r.store().state()).unwrap();
            r.observe(Some(&observed(n)));
            assert!(!r.verified() && r.registry().as_str() == "foreign");
            assert_eq!(r.ownership(&observed(n)).map(|o| o.as_str()), Some("mismatch"));
            assert_eq!(r.adopt().unwrap(), RegistryState::Owned);
            assert!(r.verified());
            assert_eq!(tag(&r), Some(OwnerTag::new(BackendKind::Ezsp, Some(n))));
        }
    }

    /// (b) Network-scoped records without a tag or a stored network:
    /// unverified and closed whatever the NCP runs, until the operator
    /// decides; nothing written.
    #[test]
    fn untagged_records_of_unknown_origin_wait_for_the_operator() {
        let open = || NcpNvRecords::open(BackendKind::Ezsp, zrhp_store(false, &["auto/r1", "udef/a"]));
        let r = open();
        assert_eq!(r.registry(), RegistryState::Unverified);
        assert_eq!(tag(&r), None);
        let denied = |e: io::Error| e.kind() == io::ErrorKind::PermissionDenied;
        assert_eq!(kv::with(&r, |s| (s.gate(), s.get("auto/r1").is_some(), s.get("udef/a").is_some())), (Gate::Closed, false, true));
        assert!(put(&r, "auto/r2").is_err_and(denied));
        r.observe(Some(&observed(id(4, Some(1)))));
        assert_eq!((r.registry(), r.verified(), tag(&r)), (RegistryState::Unverified, false, None));
        r.replacing();
        r.replaced(None);
        assert_eq!((r.registry(), r.verified()), (RegistryState::Unverified, false), "a failed replacement decides nothing");
        assert_eq!(r.adopt().unwrap(), RegistryState::Owned);
        assert!(r.verified());
        assert_eq!(tag(&r).unwrap().identity, Some(id(4, Some(1))));
        assert_eq!(kv::with(&r, |s| s.get("auto/r1").is_some()), true);

        // Cleared instead: the records go.
        let r = open();
        r.observe(Some(&observed(id(4, Some(1)))));
        assert_eq!(r.clear().unwrap(), 1);
        assert_eq!((r.registry(), r.verified()), (RegistryState::Owned, true));
        // A network formed over them takes them, as with ZRHP.
        let r = open();
        r.replacing();
        r.replaced(Some(&observed(id(6, Some(3)))));
        assert_eq!((r.registry(), r.verified()), (RegistryState::Owned, true));
        assert!(kv::with(&r, |s| s.get("auto/r1").is_some()));
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
