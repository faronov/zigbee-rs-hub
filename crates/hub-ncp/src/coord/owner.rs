//! The ZRHP backend's side of the hub's ownership tag (design §2.7). The
//! host store is the authority here (`OwnershipModel::HostStore`): the
//! stored network decides whose the hub's network-scoped records are.
//!
//! - [`open`], at start, before the lifecycle runs and the application
//!   loads its records: the records' [`RegistryState`]; the first start
//!   of a tagging build derives the tag (migration): from the stored
//!   network, else `{zrhp, identity null}` when network-scoped records
//!   exist, else none until a network is formed or adopted;
//! - [`mirror`], on every `Owned` and when the stored network changes:
//!   the tag follows the stored network (PAN ID, channel, key sequence
//!   and coordinator are informative and rewritten; a tag of another
//!   network makes the records foreign, nothing is written);
//! - [`adopt`] and [`clear`]: the operator's `network/registry/adopt` and
//!   `network/registry/clear`.
//!
//! The tag is one KV record, written only when it changes.

use std::io;

use hub_coord::{BackendKind, NetworkIdentity, OwnerTag, OwnershipModel, RegistryState};
use hub_store::kv::{self, scope};
use hub_store::{Gate, HostState, OWNER_KEY, Scope, Store};
use log::warn;

/// The stored network as an identity (coordinator: the store's NCP). Its
/// key ID is unknown when the network was adopted without `KEY_EXPORT`
/// (an all-zero key).
#[inline(never)]
pub fn identity(s: &HostState) -> Option<NetworkIdentity> {
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

/// Access to the network-scoped records in `state`.
pub fn gate(state: RegistryState) -> Gate {
    match state {
        RegistryState::Foreign { .. } => Gate::Closed,
        RegistryState::Unverified => Gate::ReadOnly,
        RegistryState::Untagged | RegistryState::Owned => Gate::Open,
    }
}

#[inline(never)]
fn check(store: &Store, network: Option<&NetworkIdentity>) -> RegistryState {
    let state = RegistryState::check(store.kv_get(OWNER_KEY), BackendKind::Zrhp, OwnershipModel::HostStore, network);
    if let RegistryState::Foreign { backend, why } = state {
        warn!("hub: network records not ours: {backend}, {why}");
    }
    state
}

/// Writes the tag (the store skips an unchanged value).
#[inline(never)]
fn put(store: &mut Store, network: Option<NetworkIdentity>) -> io::Result<RegistryState> {
    store.kv_put(OWNER_KEY, OwnerTag::new(BackendKind::Zrhp, network).encode().as_bytes())?;
    Ok(RegistryState::Owned)
}

/// The records' state at start; `write` (not observing read-only) tags an
/// untagged store that has a network or network-scoped records.
pub fn open(store: &mut Store, write: bool) -> RegistryState {
    let network = identity(store.state());
    if store.kv_get(OWNER_KEY).is_some() {
        return check(store, network.as_ref());
    }
    if !write || (network.is_none() && !kv::has_network_records(store)) {
        return RegistryState::Untagged;
    }
    put(store, network).unwrap_or_default()
}

/// The tag after the stored network was owned or changed. `force` (the
/// operator formed or restored a network) rewrites a tag of another
/// network.
pub fn mirror(store: &mut Store, state: RegistryState, force: bool) -> RegistryState {
    let Some(network) = identity(store.state()) else { return state };
    if !force && let RegistryState::Foreign { .. } = state {
        return state;
    }
    if !force && let foreign @ RegistryState::Foreign { .. } = check(store, Some(&network)) {
        return foreign;
    }
    put(store, Some(network)).unwrap_or(state)
}

/// `network/registry/adopt`: the records become this backend's, of the
/// stored network (filled at the first `Owned` without one).
pub fn adopt(store: &mut Store) -> io::Result<RegistryState> {
    let network = identity(store.state());
    put(store, network)
}

/// `network/registry/clear`: every network-scoped record deleted, then
/// [`adopt`]; how many were deleted.
pub fn clear(store: &mut Store) -> io::Result<usize> {
    let keys: Vec<String> = store.state().kv.keys().filter(|k| scope(k) == Scope::Network).cloned().collect();
    for k in &keys {
        store.kv_delete(k)?;
    }
    adopt(store)?;
    Ok(keys.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hub_store::{Change, Meta, NetworkRecord};

    fn record(epid: u8, key: [u8; 16]) -> NetworkRecord {
        NetworkRecord {
            pan_id: 0x1A2B,
            ext_pan_id: [epid, 0x03, 0x02, 0x01, 0x00, 0x4B, 0x12, 0x00],
            channel: 15,
            channel_mask: 1 << 15,
            network_key: key,
            key_seq: 0,
            nwk_update_id: 0,
            tx_power: 8,
            nwk_manager: 0,
        }
    }

    fn store_with(network: Option<NetworkRecord>, kv: &[&str]) -> Store {
        let mut s = Store::volatile();
        s.commit(Change::Meta(Meta { ncp_ieee: [0x5A, 0x51, 0, 0, 0, 0, 1, 2], ..Meta::default() })).unwrap();
        if let Some(n) = network {
            s.commit(Change::Network(n)).unwrap();
        }
        for k in kv {
            s.kv_put(k, b"{}").unwrap();
        }
        s
    }

    fn tag(s: &Store) -> Option<OwnerTag> {
        s.kv_get(OWNER_KEY).map(|t| OwnerTag::decode(t).unwrap())
    }

    fn kv_records(s: &Store) -> usize {
        s.state().kv.len()
    }

    #[test]
    fn migration_from_the_stored_network() {
        let mut s = store_with(Some(record(0x04, [0x42; 16])), &["dev/zb:1"]);
        assert_eq!(open(&mut s, true), RegistryState::Owned);
        let t = tag(&s).unwrap();
        assert_eq!(t.backend, "zrhp");
        assert_eq!(t.identity, identity(s.state()));
        let n = t.identity.unwrap();
        assert_eq!((n.ext_pan_id[0], n.pan_id, n.channel, n.key_seq), (0x04, 0x1A2B, 15, 0));
        assert_eq!(n.key_id, Some(NetworkIdentity::key_id_for(&[0x42; 16])));
        assert_eq!(n.coordinator_ieee, [0x5A, 0x51, 0, 0, 0, 0, 1, 2]);
        // Started again: nothing more written.
        let stats = s.stats();
        assert_eq!(open(&mut s, true), RegistryState::Owned);
        assert_eq!(s.stats(), stats);

        // Adopted without KEY_EXPORT: the key is unknown.
        let mut s = store_with(Some(record(0x04, [0; 16])), &[]);
        assert_eq!(open(&mut s, true), RegistryState::Owned);
        assert_eq!(tag(&s).unwrap().identity.unwrap().key_id, None);
    }

    #[test]
    fn migration_without_a_network() {
        // Network-scoped records but no network: identity null, filled at
        // the first Owned.
        let mut s = store_with(None, &["st/zb:1", "udef/a"]);
        assert_eq!(open(&mut s, true), RegistryState::Owned);
        assert_eq!(tag(&s), Some(OwnerTag::new(BackendKind::Zrhp, None)));
        s.commit(Change::Network(record(0x04, [0x42; 16]))).unwrap();
        assert_eq!(mirror(&mut s, RegistryState::Owned, false), RegistryState::Owned);
        assert_eq!(tag(&s).unwrap().identity, identity(s.state()));

        // Only the hub's own records, or nothing: no tag until a network.
        for kv in [&["udef/a", "cfg/tz"][..], &[]] {
            let mut s = store_with(None, kv);
            assert_eq!(open(&mut s, true), RegistryState::Untagged);
            assert_eq!(kv_records(&s), kv.len());
            assert_eq!(mirror(&mut s, RegistryState::Untagged, false), RegistryState::Untagged);
            s.commit(Change::Network(record(0x04, [0x42; 16]))).unwrap();
            assert_eq!(mirror(&mut s, RegistryState::Untagged, false), RegistryState::Owned);
            assert!(tag(&s).unwrap().identity.is_some());
        }

        // Observing read-only: nothing written.
        let mut s = store_with(Some(record(0x04, [0x42; 16])), &["dev/zb:1"]);
        assert_eq!(open(&mut s, false), RegistryState::Untagged);
        assert_eq!(tag(&s), None);
    }

    #[test]
    fn upkeep_rewrites_informative_fields_only_when_they_change() {
        let mut s = store_with(Some(record(0x04, [0x42; 16])), &["dev/zb:1"]);
        let st = open(&mut s, true);
        let stats = s.stats();
        assert_eq!(mirror(&mut s, st, false), RegistryState::Owned);
        assert_eq!(s.stats(), stats, "unchanged: not written again");
        // PAN ID conflict resolution, channel change, key rotation (the
        // stored key stays: ZRHP stores the key sequence only).
        let mut n = record(0x04, [0x42; 16]);
        (n.pan_id, n.channel, n.key_seq) = (0x2B3C, 25, 1);
        s.commit(Change::Network(n)).unwrap();
        let stats = s.stats();
        assert_eq!(mirror(&mut s, st, false), RegistryState::Owned);
        let t = tag(&s).unwrap().identity.unwrap();
        assert_eq!((t.pan_id, t.channel, t.key_seq), (0x2B3C, 25, 1));
        assert_eq!(s.stats().appended, stats.appended + 1, "one record");
    }

    #[test]
    fn another_network_makes_the_records_foreign_unless_forced() {
        for (n, why) in [(record(0x05, [0x42; 16]), "extended PAN ID"), (record(0x04, [0x43; 16]), "network key")] {
            let mut s = store_with(Some(record(0x04, [0x42; 16])), &["dev/zb:1"]);
            let st = open(&mut s, true);
            let before = s.kv_get(OWNER_KEY).unwrap().to_vec();
            s.commit(Change::Network(n)).unwrap();
            let foreign = mirror(&mut s, st, false);
            assert!(matches!(foreign, RegistryState::Foreign { why: w, .. } if w.contains(why)), "{foreign:?}");
            assert_eq!(gate(foreign), Gate::Closed);
            assert_eq!(s.kv_get(OWNER_KEY), Some(&before[..]), "not written");
            assert_eq!(mirror(&mut s, foreign, false), foreign, "stays foreign");
            // A restart sees the same.
            assert_eq!(open(&mut s, true).as_str(), "foreign");
            // Forming or restoring (explicit) rewrites it.
            assert_eq!(mirror(&mut s, foreign, true), RegistryState::Owned);
            assert_eq!(tag(&s).unwrap().identity, identity(s.state()));
        }
        // Key unknown on one side: the EPID decides.
        let mut s = store_with(Some(record(0x04, [0; 16])), &["dev/zb:1"]);
        let st = open(&mut s, true);
        s.commit(Change::Network(record(0x04, [0x42; 16]))).unwrap();
        assert_eq!(mirror(&mut s, st, false), RegistryState::Owned);
    }

    #[test]
    fn another_backends_records_are_foreign() {
        let mut s = store_with(Some(record(0x04, [0x42; 16])), &["dev/zb:1"]);
        let ezsp = OwnerTag::new(BackendKind::Ezsp, identity(s.state())).encode();
        s.kv_put(OWNER_KEY, ezsp.as_bytes()).unwrap();
        let st = open(&mut s, true);
        assert!(matches!(st, RegistryState::Foreign { backend: "ezsp", .. }), "{st:?}");
        assert_eq!(mirror(&mut s, st, false), st);
        assert_eq!(s.kv_get(OWNER_KEY), Some(ezsp.as_bytes()));
    }

    #[test]
    fn adopt_and_clear() {
        let keys = ["dev/zb:1", "st/zb:1", "grp/0001", "bnd/zb:1", "auto/r", "ota/d/zb:1", "bkp/last", "udef/a", "usrc/a", "cfg/tz", "cfg/tx_power", "ota/cfg"];
        let mut s = store_with(Some(record(0x04, [0x42; 16])), &keys);
        s.kv_put(OWNER_KEY, OwnerTag::new(BackendKind::Ezsp, None).encode().as_bytes()).unwrap();
        assert_eq!(adopt(&mut s).unwrap(), RegistryState::Owned);
        assert_eq!(tag(&s).unwrap(), OwnerTag::new(BackendKind::Zrhp, identity(s.state())));
        assert_eq!(kv_records(&s), keys.len() + 1, "adopt deletes nothing");

        s.kv_put(OWNER_KEY, OwnerTag::new(BackendKind::Ezsp, None).encode().as_bytes()).unwrap();
        assert_eq!(clear(&mut s).unwrap(), 7);
        let left: Vec<&str> = s.state().kv.keys().map(String::as_str).collect();
        assert_eq!(left, ["cfg/tx_power", "cfg/tz", OWNER_KEY, "ota/cfg", "udef/a", "usrc/a"]);
        assert_eq!(check(&s, identity(s.state()).as_ref()), RegistryState::Owned);
    }
}
