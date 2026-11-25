//! The hub's records for a backend whose NCP keeps the network in its own
//! NVM (`OwnershipModel::NcpNv`: EZSP, ZNP). The backend only reports the
//! network it finds ([`NetworkOwnership::Observed`]); whether the hub's
//! network-scoped records belong to it is decided here, against the
//! ownership tag `hub/owner` (design §2.5, §2.7):
//!
//! | Start | Records | Gate |
//! |---|---|---|
//! | no tag, no stored ZRHP network, no network-scoped records | `untagged`; the first network-scoped write tags them `{backend, epid: null}` | open |
//! | no tag, a ZRHP image's stored network (`NetworkRecord`) | tagged as that image's migration does (`{zrhp, epid of the record}`), `foreign` | closed |
//! | no tag, network-scoped records of unknown origin | `unverified` until `network/registry/adopt` or `clear` | closed |
//! | this backend's tag naming no formed network (`epid: null`, or a network being formed: channel 0) | `owned` (`network/form` may take them) | open |
//! | this backend's tag of a formed network | `unverified` | read-only |
//! | another backend's tag, unreadable tag | `foreign` | closed |
//!
//! | The NCP reports | Records | Device traffic |
//! |---|---|---|
//! | `Observed` matching the tag (EPID, and the key ID when both are known), or no tag yet | `owned`; the tag written or its informative fields updated | allowed |
//! | `Observed` of another network | `foreign` (nothing written) | refused |
//! | `NoNetwork`, `Unknown` | unchanged | refused until the next match |
//! | nothing yet (start), session down | unchanged | waits up to [`TRAFFIC_HOLD`] for the next report ([`Records::deciding`]), then refused |
//! | anything, while the records' origin is unknown | `unverified`, closed | refused until adopted or cleared |
//!
//! Reports count only for the session they were made in: an `Ownership`
//! event decides only while the backend's status still shows it, with that
//! session's epoch ([`Records::reported`]), and device traffic opens with
//! a compare-and-swap on the epoch the backend stored for its session
//! ([`NcpNvRecords::traffic`]), so a late report about an earlier session
//! opens nothing.
//!
//! While the operator replaces the network (`network/reset`, `form`,
//! `restore`), reports decide nothing; the new network then becomes the
//! records' (the tag is rewritten, as ZRHP's adapter does).

use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hub_coord::{
    BackendKind, CoordStatus, NetworkIdentity, NetworkOwnership, OwnerTag, OwnershipModel,
    RegistryState, SessionState,
};
use hub_store::kv::{self, scope};
use hub_store::{Gate, Guarded, HostState, JournalStats, Kv, KvStore, OWNER_KEY, Scope, Store};
use log::{info, warn};
use serde_json::{Value as J, json};

use super::{Records, Report};

/// What the records know about the NCP's network.
#[derive(Debug, Default)]
struct State {
    registry: RegistryState,
    /// The NCP's network, as last reported in this session.
    observed: Option<NetworkIdentity>,
    /// The session of the last report.
    epoch: Option<u32>,
    /// The operator is replacing the network.
    replacing: bool,
    /// Network-scoped records of unknown origin (no tag, no stored
    /// network): closed, and decided only by the operator (adopt, clear,
    /// or a network formed or restored over them).
    hold: bool,
    /// Waiting since then for the ownership report of the backend's next
    /// session (the app started, or the session ended).
    pending: Option<Instant>,
}

/// How long device traffic waits, after the app starts or the NCP's
/// session ends, for the next session's ownership report (instead of being
/// refused at once): the time an NCP takes to come back, and the app to
/// decide.
pub const TRAFFIC_HOLD: Duration = Duration::from_secs(3);

/// [`Records`] of an `NcpNv` backend in the hub's own store.
pub struct NcpNvRecords {
    backend: BackendKind,
    /// Locked before `state`.
    store: Mutex<Store>,
    state: Mutex<State>,
    /// `(epoch << 1) | open`: device traffic may go out in the backend's
    /// session `epoch` ([`Self::traffic`]).
    traffic: Arc<AtomicU32>,
    /// [`TRAFFIC_HOLD`].
    traffic_hold: Duration,
}

/// The [`NcpNvRecords::traffic`] value of session `epoch`, closed.
fn closed(epoch: u32) -> u32 {
    epoch.wrapping_shl(1)
}

/// This backend's tag that names no formed network: written before any
/// network (`epid: null`) or for one being formed (channel 0 until it is
/// up). `network/form` may take such records.
fn names_no_network(tag: &[u8], backend: BackendKind) -> bool {
    OwnerTag::decode(tag)
        .is_some_and(|t| t.backend == backend.as_str() && t.identity.is_none_or(|n| n.channel == 0))
}

/// The store while the records are untagged: the first network-scoped
/// write tags them as this backend's, with no network yet, so a restart
/// knows where they come from.
struct Tagging<'a> {
    store: &'a mut Store,
    backend: BackendKind,
    tagged: bool,
}

impl Kv for Tagging<'_> {
    fn get(&self, key: &str) -> Option<&[u8]> {
        self.store.kv_get(key)
    }
    fn prefix<'a>(&'a self, prefix: &'a str) -> Box<dyn Iterator<Item = (&'a str, &'a [u8])> + 'a> {
        Box::new(self.store.kv_prefix(prefix))
    }
    fn put(&mut self, key: &str, value: &[u8]) -> io::Result<()> {
        if !self.tagged && scope(key) == Scope::Network && self.store.kv_get(OWNER_KEY).is_none() {
            self.store.kv_put(OWNER_KEY, OwnerTag::new(self.backend, None).encode().as_bytes())?;
            self.tagged = true;
        }
        self.store.kv_put(key, value)
    }
    fn delete(&mut self, key: &str) -> io::Result<()> {
        self.store.kv_delete(key)
    }
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

/// A random extended PAN ID (the OS's random keys of `RandomState`, and
/// the time), neither all zeros nor all ones.
fn random_epid() -> [u8; 8] {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    let mut e = h.finish().to_le_bytes();
    if e == [0; 8] || e == [0xFF; 8] {
        e[0] = 0x5A;
    }
    e
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
            if names_no_network(tag, backend) {
                RegistryState::Owned
            } else {
                RegistryState::check(Some(tag), backend, OwnershipModel::NcpNv, None)
            }
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
        let st = State { registry, hold, pending: Some(Instant::now()), ..State::default() };
        Self {
            backend,
            store: Mutex::new(store),
            state: Mutex::new(st),
            traffic: Arc::default(),
            traffic_hold: TRAFFIC_HOLD,
        }
    }

    /// How long device traffic waits for a new session's decision
    /// ([`TRAFFIC_HOLD`]; `EzspOptions::traffic_hold`).
    pub fn traffic_hold(&self) -> Duration {
        self.traffic_hold
    }

    #[cfg(test)]
    fn with_traffic_hold(mut self, hold: Duration) -> Self {
        self.traffic_hold = hold;
        self
    }

    /// Whether device traffic may go out, shared with the backend
    /// (`EzspOptions::traffic`): `(epoch << 1) | open`. The backend stores
    /// `epoch << 1` when its session `epoch` starts (before it reports the
    /// session's network) and 0 when it ends; the records open it with a
    /// compare-and-swap for the session whose network matched, so neither
    /// a request queued earlier nor a late report reaches a network not
    /// decided yet.
    pub fn traffic(&self) -> Arc<AtomicU32> {
        self.traffic.clone()
    }

    /// Opens device traffic for session `epoch`, if that is still the
    /// backend's session.
    fn open_traffic(&self, epoch: Option<u32>) {
        let Some(e) = epoch else { return };
        // Already open for this session (a later report of it, e.g. after
        // a channel change): nothing to say.
        if let Err(now) = self.traffic.compare_exchange(closed(e), closed(e) | 1, Ordering::SeqCst, Ordering::SeqCst)
            && now != closed(e) | 1
        {
            info!("hub: the NCP's session {e} is over: its network decides nothing");
        }
    }

    /// Closes device traffic; the backend's session stays as it is.
    fn close_traffic(&self) {
        let mut v = self.traffic.load(Ordering::SeqCst);
        while v & 1 == 1 {
            match self.traffic.compare_exchange(v, v & !1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return,
                Err(now) => v = now,
            }
        }
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
    fn decide(&self, store: &mut Store, st: &mut State, network: NetworkIdentity, epoch: Option<u32>, force: bool) {
        st.observed = Some(network);
        st.epoch = epoch;
        if st.hold && !force {
            self.close_traffic();
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
                    self.open_traffic(epoch);
                }
                Err(e) => {
                    warn!("hub: writing the ownership tag: {e}");
                    self.close_traffic();
                }
            },
            foreign @ RegistryState::Foreign { backend, why } => {
                if st.registry != foreign {
                    warn!("hub: the NCP's network is not the records': {backend}, {why}");
                }
                st.registry = foreign;
                self.close_traffic();
            }
            RegistryState::Untagged | RegistryState::Unverified => self.close_traffic(),
        }
    }
}

impl KvStore for NcpNvRecords {
    /// Network-scoped records behind the gate of [`Records::registry`].
    fn with_kv(&self, f: &mut dyn FnMut(&mut dyn Kv)) {
        let mut store = self.store();
        // The state is not held while `f` runs.
        let (g, untagged) = {
            let st = self.state();
            (gate(&st), st.registry == RegistryState::Untagged)
        };
        if untagged {
            let mut t = Tagging { store: &mut store, backend: self.backend, tagged: false };
            f(&mut t);
            if t.tagged {
                info!("hub: network records tagged as {}'s (no network yet)", self.backend.as_str());
                self.state().registry = RegistryState::Owned;
            }
            return;
        }
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

    fn adopt(&self, now: Report<'_>) -> io::Result<RegistryState> {
        let mut store = self.store();
        let mut st = self.state();
        // What the backend reports now, not an earlier session's network.
        let (network, epoch) = match now {
            Some((NetworkOwnership::Observed(n) | NetworkOwnership::Owned(n), e)) => (Some(*n), Some(e)),
            Some((_, e)) => (None, Some(e)),
            None => (None, None),
        };
        self.put_tag(&mut store, network)?;
        (st.observed, st.epoch) = (network, epoch);
        st.registry = RegistryState::Owned;
        st.hold = false;
        st.pending = None;
        match network {
            Some(_) => self.open_traffic(epoch),
            None => self.close_traffic(),
        }
        Ok(st.registry)
    }

    fn clear(&self, now: Report<'_>) -> io::Result<usize> {
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
        self.adopt(now)?;
        Ok(keys.len())
    }

    fn observe(&self, report: Report<'_>) {
        let mut store = self.store();
        let mut st = self.state();
        // A report decides; the end of a session waits for the next one.
        st.pending = report.is_none().then(Instant::now);
        match report {
            Some((NetworkOwnership::Observed(n) | NetworkOwnership::Owned(n), e)) if !st.replacing => {
                self.decide(&mut store, &mut st, *n, Some(e), false)
            }
            Some((NetworkOwnership::Observed(n) | NetworkOwnership::Owned(n), e)) => {
                (st.observed, st.epoch) = (Some(*n), Some(e));
                self.close_traffic();
            }
            Some((_, e)) => {
                (st.observed, st.epoch) = (None, Some(e));
                self.close_traffic();
            }
            None => {
                (st.observed, st.epoch) = (None, None);
                self.close_traffic();
            }
        }
    }

    fn reported(&self, event: &NetworkOwnership, now: &CoordStatus) {
        match now.epoch {
            Some(e) if now.session == SessionState::Ready && now.ownership == *event => self.observe(Some((event, e))),
            _ => info!("hub: ownership report {} of an earlier session ignored", event.as_str()),
        }
    }

    fn verified(&self) -> bool {
        self.traffic.load(Ordering::SeqCst) & 1 == 1
    }

    fn deciding(&self) -> bool {
        let st = self.state();
        // Records the next report may verify: not another backend's or
        // network's, not of unknown origin, not being replaced.
        let may_match = !st.replacing && !st.hold && !matches!(st.registry, RegistryState::Foreign { .. });
        may_match && st.pending.is_some_and(|t| t.elapsed() < self.traffic_hold)
    }

    fn may_form(&self) -> bool {
        let store = self.store();
        let st = self.state();
        !st.hold && store.kv_get(OWNER_KEY).is_none_or(|t| names_no_network(t, self.backend))
    }

    fn ownership(&self, reported: &NetworkOwnership) -> Option<NetworkOwnership> {
        let NetworkOwnership::Observed(n) = reported else { return None };
        let st = self.state();
        match st.registry {
            _ if self.verified() => Some(NetworkOwnership::Owned(*n)),
            RegistryState::Foreign { why, .. } => Some(NetworkOwnership::Mismatch { found: Some(*n), why: why.into() }),
            _ => None,
        }
    }

    fn replacing(&self) {
        let mut st = self.state();
        st.replacing = true;
        self.close_traffic();
    }

    fn forming(&self, ext_pan_id: Option<[u8; 8]>) -> Option<[u8; 8]> {
        let epid = ext_pan_id.unwrap_or_else(random_epid);
        let mut store = self.store();
        let mut st = self.state();
        // The key ID is unknown until the network is up: the EPID alone
        // decides meanwhile; informative fields follow at the first match.
        let network = NetworkIdentity {
            ext_pan_id: epid,
            key_id: None,
            pan_id: 0,
            channel: 0,
            key_seq: 0,
            coordinator_ieee: st.observed.map_or([0; 8], |n| n.coordinator_ieee),
        };
        match store.kv_put(OWNER_KEY, OwnerTag::new(self.backend, Some(network)).encode().as_bytes()) {
            Ok(()) => {
                st.registry = RegistryState::Owned;
                st.hold = false;
            }
            Err(e) => warn!("hub: tagging the network being formed: {e}"),
        }
        Some(epid)
    }

    fn replaced(&self, now: Report<'_>) {
        let mut store = self.store();
        let mut st = self.state();
        st.replacing = false;
        match now {
            Some((NetworkOwnership::Observed(n) | NetworkOwnership::Owned(n), e)) => {
                self.decide(&mut store, &mut st, *n, Some(e), true)
            }
            Some((NetworkOwnership::NoNetwork, e)) => (st.observed, st.epoch) = (None, Some(e)),
            // Failed: what the NCP reported meanwhile decides as usual.
            _ => {
                if let Some(n) = st.observed {
                    let e = st.epoch;
                    self.decide(&mut store, &mut st, n, e, false);
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

    /// The records of `store`, the backend's session 1 started.
    fn open(store: Store) -> NcpNvRecords {
        let r = NcpNvRecords::open(BackendKind::Ezsp, store);
        r.traffic().store(closed(1), Ordering::SeqCst);
        r
    }

    /// The backend reports `o` in session 1.
    fn see(r: &NcpNvRecords, o: NetworkOwnership) {
        r.observe(Some((&o, 1)));
    }

    /// What the backend reports now: `o` in session 1.
    fn now(o: &NetworkOwnership) -> Report<'_> {
        Some((o, 1))
    }

    /// A copy of the records' store, as a restart finds it.
    fn reopen(r: &NcpNvRecords) -> NcpNvRecords {
        let mut s = Store::volatile();
        for (k, v) in &r.store().state().kv {
            s.kv_put(k, v).unwrap();
        }
        open(s)
    }

    #[test]
    fn a_fresh_store_is_tagged_at_the_first_network() {
        let r = open(Store::volatile());
        assert_eq!(r.registry(), RegistryState::Untagged);
        assert!(!r.verified());
        see(&r, NetworkOwnership::NoNetwork);
        assert_eq!((r.registry(), r.verified(), tag(&r)), (RegistryState::Untagged, false, None));
        assert!(r.may_form());
        // Rules made while the NCP has no network are kept, tagged as this
        // backend's (no network yet).
        put(&r, "auto/r1").unwrap();
        assert_eq!((r.registry(), tag(&r)), (RegistryState::Owned, Some(OwnerTag::new(BackendKind::Ezsp, None))));
        assert!(r.may_form());
        see(&r, observed(id(4, Some(1))));
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
        let r = open(s);
        assert_eq!(r.registry(), RegistryState::Unverified);
        assert_eq!(kv::with(&r, |s| (s.gate(), s.get("dev/zb:1").is_some())), (Gate::ReadOnly, true));
        assert!(put(&r, "dev/zb:2").is_err_and(|e| e.kind() == io::ErrorKind::PermissionDenied));
        // PAN ID, channel, key sequence and coordinator are informative;
        // the key is not exportable now: the EPID decides, the key ID stays.
        let moved = NetworkIdentity { pan_id: 0x2B3C, channel: 25, key_seq: 1, coordinator_ieee: [9; 8], key_id: None, ..id(4, Some(1)) };
        see(&r, observed(moved));
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
            let r = open(s);
            see(&r, observed(other));
            assert_eq!(r.registry().as_str(), "foreign");
            assert!(!r.verified());
            assert!(!kv::with(&r, |s| s.get("dev/zb:1").is_some()), "hidden");
            assert_eq!(r.store().kv_get(OWNER_KEY), Some(mine.as_bytes()), "nothing written");
            assert_eq!(r.adopt(now(&observed(other))).unwrap(), RegistryState::Owned);
            assert!(r.verified());
            assert_eq!(tag(&r).unwrap().identity, Some(other));
            assert!(kv::with(&r, |s| s.get("dev/zb:1").is_some()));
        }
        let mut s = Store::volatile();
        s.kv_put(OWNER_KEY, OwnerTag::new(BackendKind::Zrhp, Some(id(4, Some(1)))).encode().as_bytes()).unwrap();
        for k in ["dev/zb:1", "st/zb:1", "auto/r", "udef/a", "cfg/tz"] {
            s.kv_put(k, b"{}").unwrap();
        }
        let r = open(s);
        assert!(matches!(r.registry(), RegistryState::Foreign { backend: "zrhp", .. }));
        see(&r, observed(id(4, Some(1))));
        assert!(!r.verified() && r.registry().as_str() == "foreign", "another backend's records");
        assert_eq!(r.clear(now(&observed(id(4, Some(1))))).unwrap(), 3);
        assert_eq!((r.registry(), r.verified()), (RegistryState::Owned, true));
        let left: Vec<String> = r.store().state().kv.keys().cloned().collect();
        assert_eq!(left, ["cfg/tz", OWNER_KEY, "udef/a"]);
    }

    #[test]
    fn the_tx_power_is_kept_under_the_backends_key() {
        let r = open(Store::volatile());
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
            let r = open(zrhp_store(true, kv));
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
            see(&r, observed(n));
            assert!(!r.verified() && r.registry().as_str() == "foreign");
            assert_eq!(r.ownership(&observed(n)).map(|o| o.as_str()), Some("mismatch"));
            assert!(!r.may_form());
            assert_eq!(r.adopt(now(&observed(n))).unwrap(), RegistryState::Owned);
            assert!(r.verified());
            assert_eq!(tag(&r), Some(OwnerTag::new(BackendKind::Ezsp, Some(n))));
        }
    }

    /// (b) Network-scoped records without a tag or a stored network:
    /// unverified and closed whatever the NCP runs, until the operator
    /// decides; nothing written.
    #[test]
    fn untagged_records_of_unknown_origin_wait_for_the_operator() {
        let fresh = || open(zrhp_store(false, &["auto/r1", "udef/a"]));
        let r = fresh();
        assert_eq!(r.registry(), RegistryState::Unverified);
        assert_eq!(tag(&r), None);
        assert!(!r.may_form());
        let denied = |e: io::Error| e.kind() == io::ErrorKind::PermissionDenied;
        assert_eq!(kv::with(&r, |s| (s.gate(), s.get("auto/r1").is_some(), s.get("udef/a").is_some())), (Gate::Closed, false, true));
        assert!(put(&r, "auto/r2").is_err_and(denied));
        see(&r, observed(id(4, Some(1))));
        assert_eq!((r.registry(), r.verified(), tag(&r)), (RegistryState::Unverified, false, None));
        r.replacing();
        r.replaced(None);
        assert_eq!((r.registry(), r.verified()), (RegistryState::Unverified, false), "a failed replacement decides nothing");
        assert_eq!(r.adopt(now(&observed(id(4, Some(1))))).unwrap(), RegistryState::Owned);
        assert!(r.verified());
        assert_eq!(tag(&r).unwrap().identity, Some(id(4, Some(1))));
        assert!(kv::with(&r, |s| s.get("auto/r1").is_some()));

        // Cleared instead: the records go.
        let r = fresh();
        see(&r, observed(id(4, Some(1))));
        assert_eq!(r.clear(now(&observed(id(4, Some(1))))).unwrap(), 1);
        assert_eq!((r.registry(), r.verified()), (RegistryState::Owned, true));
        // A network formed over them takes them, as with ZRHP.
        let r = fresh();
        r.replacing();
        r.replaced(now(&observed(id(6, Some(3)))));
        assert_eq!((r.registry(), r.verified()), (RegistryState::Owned, true));
        assert!(kv::with(&r, |s| s.get("auto/r1").is_some()));
    }

    /// The network being formed is tagged before `form` (its EPID, the key
    /// ID unknown, channel 0): a form that times out with the network
    /// coming up later, or a reboot in between, leaves the records the new
    /// network's; one that failed leaves them to the next `network/form`.
    #[test]
    fn the_network_being_formed_is_tagged_before_it_comes_up() {
        let r = open(Store::volatile());
        see(&r, observed(id(4, Some(1))));
        r.replacing();
        see(&r, NetworkOwnership::NoNetwork);
        let epid = r.forming(None).unwrap();
        assert_ne!(epid, id(4, None).ext_pan_id);
        let pre = tag(&r).unwrap().identity.unwrap();
        assert_eq!((pre.ext_pan_id, pre.key_id), (epid, None));
        assert_eq!(r.forming(Some([7; 8])), Some([7; 8]), "the operator's EPID is kept");
        let epid = [7; 8];
        // The form job timed out.
        r.replaced(None);
        let new = NetworkIdentity { ext_pan_id: epid, ..id(0, Some(9)) };
        // The network comes up later.
        see(&r, observed(new));
        assert_eq!((r.registry(), r.verified()), (RegistryState::Owned, true));
        assert_eq!(tag(&r).unwrap().identity, Some(new), "filled at the match");

        // A reboot between forming and the network: the tag decides.
        let r2 = open(Store::volatile());
        r2.replacing();
        r2.forming(Some(epid));
        let mut s = Store::volatile();
        s.kv_put(OWNER_KEY, r2.store().kv_get(OWNER_KEY).unwrap()).unwrap();
        let r3 = open(s);
        assert_eq!(r3.registry(), RegistryState::Owned, "the hub's own, of no formed network");
        assert_eq!(kv::with(&r3, |s| s.gate()), Gate::Open);
        // The form failed (the NCP has no network): formed again.
        see(&r3, NetworkOwnership::NoNetwork);
        assert!(r3.may_form());
        see(&r3, observed(new));
        assert_eq!((r3.registry(), r3.verified()), (RegistryState::Owned, true));
        assert!(!r3.may_form(), "the network is up");
    }

    /// The backend shares the traffic flag and clears it itself.
    #[test]
    fn traffic_is_the_flag_the_backend_shares() {
        let r = open(Store::volatile());
        see(&r, observed(id(4, Some(1))));
        assert!(r.verified());
        r.traffic().store(0, Ordering::SeqCst);
        assert!(!r.verified());
    }

    /// The backend's status as the app handles an event: session `epoch`
    /// ready, reporting `o`.
    fn status(o: &NetworkOwnership, epoch: u32) -> CoordStatus {
        CoordStatus {
            session: SessionState::Ready,
            epoch: Some(epoch),
            ncp: None,
            caps: None,
            network_state: hub_coord::NetState::Up,
            network: None,
            ownership: o.clone(),
            permit_join_s: 0,
        }
    }

    /// A report counts for its session only: one the app handles late,
    /// after the NCP restarted (on whatever network), opens nothing.
    #[test]
    fn a_late_report_of_an_earlier_session_opens_nothing() {
        let r = open(Store::volatile());
        let (a, b) = (observed(id(4, Some(1))), observed(id(5, Some(1))));
        r.reported(&a, &status(&a, 1));
        assert!(r.verified());
        // The NCP restarts on network B: the backend ends session 1 and
        // starts session 2 before the app handles session 1's report.
        r.traffic().store(0, Ordering::SeqCst);
        r.traffic().store(closed(2), Ordering::SeqCst);
        r.reported(&a, &status(&b, 2));
        assert!(!r.verified(), "session 1's report, handled in session 2");
        assert_eq!(r.registry(), RegistryState::Owned);
        r.reported(&b, &status(&b, 2));
        assert_eq!(r.registry().as_str(), "foreign");
        // Session 3, network A again: session 2's reports decide nothing.
        r.traffic().store(closed(3), Ordering::SeqCst);
        r.observe(Some((&a, 2)));
        assert!(!r.verified(), "an earlier session's network opens nothing");
        assert_eq!(r.adopt(Some((&a, 2))).unwrap(), RegistryState::Owned);
        assert!(!r.verified(), "nor adopting it");
        r.reported(&a, &status(&a, 3));
        assert!(r.verified());
        assert_eq!(r.traffic().load(Ordering::SeqCst), closed(3) | 1);
    }

    /// Records the hub makes before its first network (rules, groups) are
    /// tagged as this backend's: after a restart they are still the hub's
    /// own, and `network/form` may take them.
    #[test]
    fn records_made_before_the_first_network_stay_the_hubs() {
        let r = open(Store::volatile());
        see(&r, NetworkOwnership::NoNetwork);
        put(&r, "auto/r1").unwrap();
        put(&r, "grp/1").unwrap();
        let r = reopen(&r);
        assert_eq!(r.registry(), RegistryState::Owned);
        assert_eq!(kv::with(&r, |s| (s.gate(), s.get("auto/r1").is_some())), (Gate::Open, true));
        see(&r, NetworkOwnership::NoNetwork);
        assert!(r.may_form());
        put(&r, "auto/r2").unwrap();
        // The first network becomes theirs.
        see(&r, observed(id(4, Some(1))));
        assert_eq!((r.registry(), r.verified()), (RegistryState::Owned, true));
        assert_eq!(tag(&r).unwrap().identity, Some(id(4, Some(1))));
    }

    /// `network/form` (no confirmation) takes only the hub's records of no
    /// formed network: a network the NCP lost needs `network/reset`.
    #[test]
    fn form_takes_only_records_of_no_formed_network() {
        let r = open(Store::volatile());
        see(&r, observed(id(4, Some(1))));
        see(&r, NetworkOwnership::NoNetwork);
        assert_eq!(r.registry(), RegistryState::Owned);
        assert!(!r.may_form(), "the NCP lost the network mid-run");
        let r = reopen(&r);
        see(&r, NetworkOwnership::NoNetwork);
        assert_eq!(r.registry(), RegistryState::Unverified);
        assert!(!r.may_form(), "after a restart");
        let mut s = Store::volatile();
        s.kv_put(OWNER_KEY, b"garbage").unwrap();
        assert!(!open(s).may_form(), "an unreadable tag");
        let mut s = Store::volatile();
        s.kv_put(OWNER_KEY, OwnerTag::new(BackendKind::Zrhp, None).encode().as_bytes()).unwrap();
        assert!(!open(s).may_form(), "another backend's");
    }

    /// After the start or the end of a session, the records wait a few
    /// seconds for the next session's report (device traffic waits instead
    /// of being refused); a report, or the hold running out, ends it.
    #[test]
    fn traffic_waits_for_the_next_sessions_report_for_a_while() {
        let tagged = || {
            let mut s = Store::volatile();
            s.kv_put(OWNER_KEY, OwnerTag::new(BackendKind::Ezsp, Some(id(4, Some(1)))).encode().as_bytes()).unwrap();
            s
        };
        for s in [Store::volatile(), tagged()] {
            let r = open(s);
            assert!(!r.verified() && r.deciding(), "before the first report");
        }
        let r = open(tagged());
        see(&r, observed(id(4, Some(1))));
        assert!(r.verified());
        r.observe(None);
        assert!(!r.verified() && r.deciding(), "the session ended");
        see(&r, NetworkOwnership::NoNetwork);
        assert!(!r.deciding(), "no network: decided");
        r.observe(None);
        r.replacing();
        assert!(!r.deciding(), "the operator replaces the network");
        r.replaced(None);
        see(&r, observed(id(5, Some(1))));
        assert_eq!(r.registry().as_str(), "foreign");
        r.observe(None);
        assert!(!r.deciding(), "foreign records do not wait");
        assert!(!open(zrhp_store(false, &["auto/r1"])).deciding(), "records of unknown origin");
        assert!(!open(zrhp_store(true, &[])).deciding(), "another backend's");
        let r = open(tagged()).with_traffic_hold(Duration::from_millis(20));
        assert!(r.deciding());
        std::thread::sleep(Duration::from_millis(40));
        assert!(!r.deciding(), "bounded");
    }

    #[test]
    fn a_replaced_network_becomes_the_records_network() {
        let r = open(Store::volatile());
        see(&r, observed(id(4, Some(1))));
        r.replacing();
        assert!(!r.verified());
        see(&r, NetworkOwnership::NoNetwork);
        // The new network's report may come before the job ends.
        see(&r, observed(id(6, Some(3))));
        assert_eq!(r.registry(), RegistryState::Owned, "undecided while replacing");
        assert!(!r.verified());
        r.replaced(now(&observed(id(6, Some(3)))));
        assert_eq!((r.registry(), r.verified()), (RegistryState::Owned, true));
        assert_eq!(tag(&r).unwrap().identity, Some(id(6, Some(3))));
        // A failed replacement: the reports decide as usual.
        r.replacing();
        see(&r, observed(id(7, Some(3))));
        r.replaced(None);
        assert_eq!(r.registry().as_str(), "foreign");
    }
}
