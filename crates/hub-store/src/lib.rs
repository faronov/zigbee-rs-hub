//! Host store (spec 3.4, 8.8): a raw-flash journal in the `hubdata`
//! partition (S3), a file (desktop) or RAM (no `hubdata` partition).
//!
//! It keeps what the hub needs to own the network: the store identity and
//! table bookkeeping, the owned network, the frame-counter checkpoint log
//! (floors for `NWK_INIT`), a minimal ZRHP device registry, the pending NCP
//! update and the host endpoints; plus application key/value records (the
//! engine's device registry and user definitions, spec device-support
//! 16.1: everything shares the 128 KiB journal). Link keys and host-backed
//! tables (`HOST_TABLES`) come later.

pub mod crc;
pub mod files;
pub mod flash;
pub mod journal;
pub mod kv;
pub mod state;

use std::io;

pub use flash::{FileFlash, Flash, MemFlash};
pub use journal::{Journal, JournalStats};
pub use kv::{Gate, Guarded, Kv, KvStore, OWNER_KEY, Scope};
pub use state::{Change, DeviceRecord, EndpointRecord, FcRecord, FwPending, HostState, Meta, NetworkRecord};

/// Journal size in `hubdata` (spec 3.4: 32 × 4 KiB).
pub const JOURNAL_BYTES: u32 = 128 * 1024;
/// Flash sector size.
pub const SECTOR: u32 = 4096;

/// The store.
pub struct Store {
    journal: Journal<Box<dyn Flash>>,
    volatile: bool,
}

impl Store {
    /// Open the journal in `flash`.
    pub fn open(flash: Box<dyn Flash>) -> io::Result<Self> {
        Ok(Self { journal: Journal::open(flash)?, volatile: false })
    }

    /// A RAM-only store (lost at reset; used when no `hubdata` exists).
    pub fn volatile() -> Self {
        let flash: Box<dyn Flash> = Box::new(MemFlash::new(JOURNAL_BYTES / SECTOR, SECTOR));
        Self { journal: Journal::open(flash).expect("RAM journal"), volatile: true }
    }

    /// Whether nothing survives a reset.
    pub fn is_volatile(&self) -> bool {
        self.volatile
    }

    /// Current state.
    pub fn state(&self) -> &HostState {
        self.journal.state()
    }

    /// Durably apply one change.
    pub fn commit(&mut self, change: Change) -> io::Result<()> {
        self.journal.commit(change)
    }

    /// Commit only if the change alters the state.
    pub fn commit_if_changed(&mut self, change: Change) -> io::Result<()> {
        if self.state().would_change(&change) {
            self.commit(change)?;
        }
        Ok(())
    }

    /// Store `value` under `key` (skipped when identical). Values larger
    /// than one record are written as parts; a power cut in the middle
    /// keeps the previous value.
    pub fn kv_put(&mut self, key: &str, value: &[u8]) -> io::Result<()> {
        if key.is_empty() || key.len() > state::KV_KEY_MAX {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "key length"));
        }
        if self.state().kv.get(key).map(Vec::as_slice) == Some(value) {
            return Ok(());
        }
        self.journal.commit_many(&HostState::kv_changes(key, value))
    }

    /// Remove `key`.
    pub fn kv_delete(&mut self, key: &str) -> io::Result<()> {
        self.commit_if_changed(Change::KvDel(key.to_string()))
    }

    /// The value of `key`.
    pub fn kv_get(&self, key: &str) -> Option<&[u8]> {
        self.state().kv.get(key).map(Vec::as_slice)
    }

    /// Keys starting with `prefix`, with their values.
    pub fn kv_prefix<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = (&'a str, &'a [u8])> + 'a {
        self.state()
            .kv
            .range(prefix.to_string()..)
            .take_while(move |(k, _)| k.starts_with(prefix))
            .map(|(k, v)| (k.as_str(), v.as_slice()))
    }

    /// Journal occupancy.
    pub fn stats(&self) -> JournalStats {
        self.journal.stats()
    }
}
