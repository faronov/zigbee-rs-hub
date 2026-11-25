//! Key/value access to the hub's own records (device registry, last
//! values, groups, automations, settings), independent of the coordinator
//! backend that owns the journal.
//!
//! A ZRHP image shares one journal and one lock with the NCP lifecycle
//! (`hub_ncp::Hub` implements [`KvStore`]); other backends use
//! `Mutex<Store>`.
//!
//! Records are classified by key ([`scope`], `docs/coordinator-boundary.md`
//! §2.7). Network-scoped records belong to the network named by the
//! ownership tag ([`OWNER_KEY`]); while they belong to another backend or
//! network, the store's [`Gate`] hides them and refuses their writes
//! ([`Guarded`]), so nothing (a registry sweep included) deletes them.

use std::io;
use std::sync::Mutex;

use crate::{JournalStats, Store};

/// Key of the ownership tag: which backend and network the network-scoped
/// records belong to.
pub const OWNER_KEY: &str = "hub/owner";

/// What a record belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The network of the ownership tag: `dev/`, `st/`, `grp/`, `bnd/`,
    /// `auto/` (rules name device IDs), `ota/d/`, `bkp/last`.
    Network,
    /// The hub, whatever the network: everything else (`udef/`, `usrc/`,
    /// `cfg/tz`, `cfg/ntp`, `cfg/catalog`, `cfg/report`, `ota/cfg`,
    /// [`OWNER_KEY`], `bkp/replaced/` and `bkp/replaced`: the backups of
    /// networks the operator replaced and their order, written whatever
    /// the gate).
    Hub,
    /// The coordinator backend: `cfg/tx_power`, `zrhp/`, `ezsp/`, `znp/`.
    Backend,
}

/// Network-scoped prefixes (a trailing `/`) and keys.
const NETWORK: &[&str] = &["dev/", "st/", "grp/", "bnd/", "auto/", "ota/d/", "bkp/last"];
const BACKEND: &[&str] = &["cfg/tx_power", "zrhp/", "ezsp/", "znp/"];

fn listed(key: &str, list: &[&str]) -> bool {
    list.iter().any(|p| if p.ends_with('/') { key.starts_with(p) } else { key == *p })
}

/// The scope of `key`.
pub fn scope(key: &str) -> Scope {
    if listed(key, NETWORK) {
        Scope::Network
    } else if listed(key, BACKEND) {
        Scope::Backend
    } else {
        Scope::Hub
    }
}

/// Whether `kv` holds network-scoped records.
pub fn has_network_records(kv: &dyn Kv) -> bool {
    NETWORK.iter().any(|p| kv.prefix(p).next().is_some_and(|(k, _)| scope(k) == Scope::Network))
}

/// Access to the network-scoped records.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Gate {
    /// Readable and writable.
    #[default]
    Open,
    /// Readable, not writable (an NCP-held network not verified yet).
    ReadOnly,
    /// Neither visible nor writable (another backend's or network's).
    Closed,
}

/// Records under one lock.
pub trait Kv {
    /// The value of `key`.
    fn get(&self, key: &str) -> Option<&[u8]>;
    /// Keys starting with `prefix`, ascending, with their values.
    fn prefix<'a>(&'a self, prefix: &'a str) -> Box<dyn Iterator<Item = (&'a str, &'a [u8])> + 'a>;
    /// Store `value` under `key` (skipped when identical).
    fn put(&mut self, key: &str, value: &[u8]) -> io::Result<()>;
    /// Remove `key`.
    fn delete(&mut self, key: &str) -> io::Result<()>;
    /// Access to the network-scoped records.
    fn gate(&self) -> Gate {
        Gate::Open
    }
}

/// `kv` behind a [`Gate`]: network-scoped records are hidden when it is
/// closed, and their writes and deletes fail unless it is open.
pub struct Guarded<'a> {
    kv: &'a mut dyn Kv,
    gate: Gate,
}

impl<'a> Guarded<'a> {
    /// `kv` behind `gate`.
    pub fn new(kv: &'a mut dyn Kv, gate: Gate) -> Self {
        Self { kv, gate }
    }

    fn hidden(&self, key: &str) -> bool {
        self.gate == Gate::Closed && scope(key) == Scope::Network
    }

    fn writable(&self, key: &str) -> io::Result<()> {
        if self.gate != Gate::Open && scope(key) == Scope::Network {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "read-only network record"));
        }
        Ok(())
    }
}

impl Kv for Guarded<'_> {
    fn get(&self, key: &str) -> Option<&[u8]> {
        if self.hidden(key) { None } else { self.kv.get(key) }
    }
    fn prefix<'a>(&'a self, prefix: &'a str) -> Box<dyn Iterator<Item = (&'a str, &'a [u8])> + 'a> {
        if self.gate != Gate::Closed {
            return self.kv.prefix(prefix);
        }
        Box::new(self.kv.prefix(prefix).filter(|(k, _)| scope(k) != Scope::Network))
    }
    fn put(&mut self, key: &str, value: &[u8]) -> io::Result<()> {
        self.writable(key)?;
        self.kv.put(key, value)
    }
    fn delete(&mut self, key: &str) -> io::Result<()> {
        self.writable(key)?;
        self.kv.delete(key)
    }
    fn gate(&self) -> Gate {
        self.gate
    }
}

/// A shared store: every call holds its lock once.
pub trait KvStore: Send + Sync {
    /// Run `f` with the records locked.
    fn with_kv(&self, f: &mut dyn FnMut(&mut dyn Kv));
    /// Journal occupancy.
    fn stats(&self) -> JournalStats;
}

/// [`KvStore::with_kv`] returning a value.
pub fn with<S: KvStore + ?Sized, R>(store: &S, f: impl FnOnce(&mut dyn Kv) -> R) -> R {
    let mut f = Some(f);
    let mut out = None;
    store.with_kv(&mut |kv| {
        if let Some(f) = f.take() {
            out = Some(f(kv));
        }
    });
    out.expect("KvStore::with_kv must call its closure")
}

impl Kv for Store {
    fn get(&self, key: &str) -> Option<&[u8]> {
        self.kv_get(key)
    }
    fn prefix<'a>(&'a self, prefix: &'a str) -> Box<dyn Iterator<Item = (&'a str, &'a [u8])> + 'a> {
        Box::new(self.kv_prefix(prefix))
    }
    fn put(&mut self, key: &str, value: &[u8]) -> io::Result<()> {
        self.kv_put(key, value)
    }
    fn delete(&mut self, key: &str) -> io::Result<()> {
        self.kv_delete(key)
    }
}

impl KvStore for Mutex<Store> {
    fn with_kv(&self, f: &mut dyn FnMut(&mut dyn Kv)) {
        f(&mut *self.lock().unwrap_or_else(|p| p.into_inner()));
    }
    fn stats(&self) -> JournalStats {
        self.lock().unwrap_or_else(|p| p.into_inner()).stats()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutex_store_round_trip() {
        let store = Mutex::new(Store::volatile());
        with(&store, |kv| {
            kv.put("a/1", b"x").unwrap();
            kv.put("a/2", b"y").unwrap();
            kv.put("b/1", b"z").unwrap();
            kv.delete("a/1").unwrap();
        });
        let dyn_store: &dyn KvStore = &store;
        let keys: Vec<String> = with(dyn_store, |kv| kv.prefix("a/").map(|(k, _)| k.to_string()).collect());
        assert_eq!(keys, ["a/2"]);
        assert_eq!(with(&store, |kv| kv.get("b/1").map(<[u8]>::to_vec)), Some(b"z".to_vec()));
    }

    #[test]
    fn key_scopes_follow_the_design_table() {
        for k in ["dev/zb:1", "st/zb:1", "grp/0001", "bnd/zb:1", "auto/r1", "ota/d/zb:1", "bkp/last"] {
            assert_eq!(scope(k), Scope::Network, "{k}");
        }
        for k in ["udef/a", "usrc/a", "cfg/tz", "cfg/ntp", "cfg/catalog", "cfg/report", "ota/cfg", OWNER_KEY, "bkp/lastx", "bkp/replaced", "bkp/replaced/eeeeeeeeeeeeeeee", "devx"] {
            assert_eq!(scope(k), Scope::Hub, "{k}");
        }
        for k in ["cfg/tx_power", "zrhp/x", "ezsp/x", "znp/x"] {
            assert_eq!(scope(k), Scope::Backend, "{k}");
        }
    }

    #[test]
    fn the_gate_hides_and_protects_network_records() {
        let mut store = Store::volatile();
        for k in ["bkp/lastx", "udef/a", "cfg/tx_power"] {
            store.kv_put(k, b"v").unwrap();
        }
        assert!(!has_network_records(&store));
        store.kv_delete("bkp/lastx").unwrap();
        for k in ["dev/zb:1", "st/zb:1"] {
            store.kv_put(k, b"v").unwrap();
            assert!(has_network_records(&store));
        }
        let before = store.state().clone();
        let keys = |kv: &dyn Kv, p: &str| kv.prefix(p).map(|(k, _)| k.to_string()).collect::<Vec<_>>();
        let denied = |r: io::Result<()>| r.is_err_and(|e| e.kind() == io::ErrorKind::PermissionDenied);

        let mut closed = Guarded::new(&mut store, Gate::Closed);
        assert_eq!(closed.gate(), Gate::Closed);
        assert_eq!(closed.get("dev/zb:1"), None);
        assert_eq!(keys(&closed, ""), ["cfg/tx_power", "udef/a"]);
        assert_eq!(keys(&closed, "dev/"), Vec::<String>::new());
        assert!(denied(closed.put("dev/zb:2", b"v")) && denied(closed.delete("st/zb:1")));
        closed.put(OWNER_KEY, b"x").unwrap();
        closed.delete(OWNER_KEY).unwrap();
        assert_eq!(store.state(), &before, "behind a closed gate only the hub's own records change");

        let mut ro = Guarded::new(&mut store, Gate::ReadOnly);
        assert_eq!(ro.get("dev/zb:1"), Some(&b"v"[..]));
        assert_eq!(keys(&ro, "dev/"), ["dev/zb:1"]);
        assert!(denied(ro.put("dev/zb:1", b"w")) && denied(ro.delete("dev/zb:1")));
        ro.put("udef/b", b"v").unwrap();

        let mut open = Guarded::new(&mut store, Gate::Open);
        open.delete("dev/zb:1").unwrap();
        assert_eq!(store.kv_get("dev/zb:1"), None);
        assert_eq!(store.kv_get("udef/b"), Some(&b"v"[..]));
    }
}
