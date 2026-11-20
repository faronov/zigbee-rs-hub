//! Host store (spec 3.4, 8.8): a raw-flash journal in the `hubdata`
//! partition (S3), a file (desktop) or RAM (no `hubdata` partition).
//!
//! Phase 1 keeps what the hub needs to own the network: the store identity
//! and table bookkeeping, the owned network, the frame-counter checkpoint
//! log (floors for `NWK_INIT`), a minimal device registry, the pending NCP
//! update and the host endpoints. Link keys and host-backed tables
//! (`HOST_TABLES`) are phase 2.

pub mod flash;
pub mod journal;
pub mod state;

use std::io;

pub use flash::{FileFlash, Flash, MemFlash};
pub use journal::{Journal, JournalStats};
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
        let mut next = self.state().clone();
        next.apply(&change);
        if &next != self.state() {
            self.commit(change)?;
        }
        Ok(())
    }

    /// Journal occupancy.
    pub fn stats(&self) -> JournalStats {
        self.journal.stats()
    }
}
