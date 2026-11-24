//! Key/value access to the hub's own records (device registry, last
//! values, groups, automations, settings), independent of the coordinator
//! backend that owns the journal.
//!
//! A ZRHP image shares one journal and one lock with the NCP lifecycle
//! (`hub_ncp::Hub` implements [`KvStore`]); other backends use
//! `Mutex<Store>`.

use std::io;
use std::sync::Mutex;

use crate::{JournalStats, Store};

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
}
