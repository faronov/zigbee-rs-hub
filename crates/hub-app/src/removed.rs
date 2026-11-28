//! Devices the operator removed (`devices/remove`), remembered so that the
//! import of the NCP's devices (`Request::DeviceInventory`) does not bring
//! them back while the coordinator's tables still list them: a child entry
//! until the child ages out, a router neighbor that ignored its leave, the
//! tables of an NCP that could not purge them (a removal while the session
//! was down, an NCP without the purge commands).
//!
//! A removed device that still holds the network key comes back on its
//! next secured rejoin (EmberZNet and the zigbee-rs NCP since its PR #22
//! admit it): heard again while joining is closed (a join, a device
//! announcement, a frame naming its IEEE) it is asked to leave again
//! ([`Removed::ask_again`], at most every [`ASK_AGAIN_MS`]) and stays
//! removed. A join or an announcement while joining is open is the
//! operator pairing it again and ends the entry, as does a device created
//! any other way (a migration import, the registry). Only a new network
//! key keeps a device that holds the current one out for good. Forming,
//! resetting or restoring the network drops them all (the operator
//! replaced the network's state), as `network/registry/clear` does with
//! the record.
//!
//! One network-scoped record ([`KEY`], `hub_store::kv`), written at once
//! when it changes, while the network records are this hub's: format byte
//! 1, then the IEEE addresses (8 bytes each, little-endian), oldest first,
//! at most [`MAX`] (the oldest goes first). Changes made while the records
//! are hidden (foreign, before the operator adopts them) apply on top of
//! the stored list once it is read.

use hub_devices::device::Millis;
use hub_store::{Gate, KvStore, kv};
use log::warn;

/// The record (network-scoped).
pub const KEY: &str = "removed";
/// Most removed devices remembered.
pub const MAX: usize = 64;
const FORMAT: u8 = 1;
/// Delay before a failed write is tried again.
const RETRY_MS: Millis = 30_000;
/// Least time between two leave requests to a removed device heard again.
pub const ASK_AGAIN_MS: Millis = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Add(u64),
    Clear(u64),
    ClearAll,
}

/// The removed devices.
#[derive(Debug, Default)]
pub struct Removed {
    /// Oldest first.
    list: Vec<u64>,
    /// Changes made before the stored list was read, in order.
    pending: Vec<Op>,
    /// The stored list was read (the gate showed the network records).
    pub loaded: bool,
    dirty: bool,
    retry_at: Millis,
    /// Store write failures.
    pub errors: u64,
    /// Removed devices heard again: when each was last asked to leave (not
    /// stored).
    asked: Vec<(u64, Millis)>,
}

impl Removed {
    /// Whether `ieee` was removed.
    pub fn contains(&self, ieee: u64) -> bool {
        self.list.contains(&ieee)
    }

    /// The removed devices, oldest first.
    pub fn list(&self) -> &[u64] {
        &self.list
    }

    /// `ieee` was removed.
    pub fn add(&mut self, ieee: u64) {
        self.apply(Op::Add(ieee));
    }

    /// `ieee` is back (it joined or was added): whether it was removed.
    pub fn clear(&mut self, ieee: u64) -> bool {
        let was = self.contains(ieee);
        self.apply(Op::Clear(ieee));
        was
    }

    /// Every entry dropped.
    pub fn clear_all(&mut self) {
        self.apply(Op::ClearAll);
    }

    /// The removed device `ieee` was heard again: whether to ask it to
    /// leave now (not more often than every [`ASK_AGAIN_MS`]).
    pub fn ask_again(&mut self, ieee: u64, now: Millis) -> bool {
        let list = &self.list;
        self.asked.retain(|(i, _)| list.contains(i));
        if !self.contains(ieee) {
            return false;
        }
        match self.asked.iter_mut().find(|(i, _)| *i == ieee) {
            Some((_, t)) if now.saturating_sub(*t) < ASK_AGAIN_MS => false,
            Some((_, t)) => {
                *t = now;
                true
            }
            None => {
                self.asked.push((ieee, now));
                true
            }
        }
    }

    fn apply(&mut self, op: Op) {
        if !self.loaded {
            self.pending.push(op);
        }
        let n = self.list.len();
        match op {
            Op::Add(ieee) => {
                self.list.retain(|x| *x != ieee);
                self.list.push(ieee);
                let over = self.list.len().saturating_sub(MAX);
                self.list.drain(..over);
                self.dirty = true;
            }
            Op::Clear(ieee) => self.list.retain(|x| *x != ieee),
            Op::ClearAll => self.list.clear(),
        }
        self.dirty |= n != self.list.len();
    }

    /// Reads the stored list once the gate shows the network records, then
    /// applies what changed meanwhile. Whether it was read now.
    pub fn load(&mut self, kv: &dyn KvStore) -> bool {
        if self.loaded {
            return false;
        }
        let (gate, stored) = kv::with(kv, |s| (s.gate(), s.get(KEY).map(<[u8]>::to_vec)));
        if gate == Gate::Closed {
            return false;
        }
        self.loaded = true;
        self.list = stored.map(|b| decode(&b)).unwrap_or_default();
        let pending = std::mem::take(&mut self.pending);
        self.dirty = false;
        for op in pending {
            self.apply(op);
        }
        true
    }

    /// Writes the list when it changed, once it was read and while the
    /// network records are this hub's.
    pub fn flush(&mut self, kv: &dyn KvStore, now: Millis) {
        if !self.dirty || !self.loaded || now < self.retry_at {
            return;
        }
        let r = kv::with(kv, |s| {
            if s.gate() != Gate::Open {
                return None;
            }
            Some(if self.list.is_empty() { s.delete(KEY) } else { s.put(KEY, &encode(&self.list)) })
        });
        match r {
            None => {}
            Some(Ok(())) => self.dirty = false,
            Some(Err(e)) => {
                self.errors += 1;
                self.retry_at = now + RETRY_MS;
                warn!("removed devices: {e}");
            }
        }
    }
}

fn encode(list: &[u64]) -> Vec<u8> {
    let mut b = Vec::with_capacity(1 + 8 * list.len());
    b.push(FORMAT);
    for ieee in list {
        b.extend_from_slice(&ieee.to_le_bytes());
    }
    b
}

/// The stored list; the newest [`MAX`] of a longer one.
fn decode(b: &[u8]) -> Vec<u64> {
    match b.split_first() {
        Some((&FORMAT, rest)) => {
            let all: Vec<u64> = rest.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect();
            all[all.len().saturating_sub(MAX)..].to_vec()
        }
        _ => {
            warn!("removed devices: unknown record format, ignored");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use hub_store::Store;

    use super::*;

    fn stored(kv: &Mutex<Store>) -> Option<Vec<u8>> {
        kv::with(kv, |s| s.get(KEY).map(<[u8]>::to_vec))
    }

    #[test]
    fn bounded_and_kept_across_restarts() {
        let kv = Mutex::new(Store::volatile());
        let mut r = Removed::default();
        assert!(r.load(&kv));
        for ieee in 1..=(MAX as u64 + 2) {
            r.add(ieee);
        }
        // Removed again: the newest.
        r.add(5);
        assert_eq!(r.list().len(), MAX);
        assert!(!r.contains(1) && !r.contains(2) && r.contains(3), "the oldest go first");
        assert_eq!(r.list().last(), Some(&5));
        r.flush(&kv, 0);
        let mut again = Removed::default();
        again.load(&kv);
        assert_eq!(again.list(), r.list());
        // A join ends the entry; the last one deletes the record.
        assert!(again.clear(5));
        assert!(!again.clear(5));
        again.clear_all();
        again.flush(&kv, 0);
        assert_eq!(stored(&kv), None);
    }

    #[test]
    fn a_removed_device_heard_again_is_asked_at_most_every_minute() {
        let mut r = Removed::default();
        r.add(1);
        assert!(!r.ask_again(2, 0), "not removed");
        assert!(r.ask_again(1, 1_000));
        assert!(!r.ask_again(1, 1_000 + ASK_AGAIN_MS - 1));
        assert!(r.ask_again(1, 1_000 + ASK_AGAIN_MS));
        r.clear(1);
        assert!(!r.ask_again(1, 10 * ASK_AGAIN_MS));
        r.add(1);
        assert!(r.ask_again(1, 10 * ASK_AGAIN_MS), "removed again: asked at once");
    }

    #[test]
    fn changes_before_the_records_are_read_apply_on_top() {
        let kv = Mutex::new(Store::volatile());
        let mut first = Removed::default();
        first.load(&kv);
        first.add(1);
        first.add(2);
        first.flush(&kv, 0);
        let mut r = Removed::default();
        r.add(3);
        r.clear(1);
        r.flush(&kv, 0);
        assert_eq!(stored(&kv).unwrap().len(), 1 + 16, "nothing written before the list was read");
        r.load(&kv);
        assert_eq!(r.list(), [2, 3]);
        r.flush(&kv, 0);
        assert_eq!(decode(&stored(&kv).unwrap()), [2, 3]);
    }
}
