//! The SONOFF Dongle-M's `files` partition as the hub's data region
//! ([`hub_core::layout::dongle_files`]): what it holds, the **claim** that
//! makes it the hub's, and the **return to stock** that gives SONOFF's
//! data back. Every step is idempotent and survives a power cut at any
//! point: running the same operation again finishes it.
//!
//! Until the claim the region is SONOFF's LittleFS (or erased) and the hub
//! never writes it. The claim:
//!
//! 1. **Scan** ([`FilesRegion::scan`]): SONOFF's blocks are those its
//!    LittleFS uses ([`crate::littlefs`]: the metadata pairs and the files'
//!    blocks, walked read-only as littlefs's own traversal does, also
//!    through the directory entries, for a relocation cut short); when the
//!    region cannot be walked, every 4 KiB block that is not erased counts
//!    (LittleFS does not erase the blocks it frees, so that may count many
//!    more). Refused, writing nothing, when more than [`SLOTS`] blocks are
//!    in use outside `stock_copy`, when SONOFF's file system uses a block of
//!    `stock_copy` (without the walk: any written block there), or when the
//!    region holds neither LittleFS nor erased flash.
//! 2. **Copy**: written blocks of `stock_copy` that SONOFF's file system
//!    freed are erased; the index (`ZRHS`: block numbers and CRC-32C of
//!    each block, whether they came from the walk; its body first, the
//!    magic last) at the first block of `stock_copy`, the blocks into the
//!    slots after it, each read back, then the index's `DONE` word.
//!    SONOFF's file system is still intact: a power cut here leaves the
//!    stock firmware working, and the claim starts over.
//! 3. **Commit**: the claim record (`ZRHF`, state `erasing`) over block 0
//!    (LittleFS superblock), from here on the region is the hub's; block 1
//!    and the hub's areas (`hubdata`, `ota_store`, `radio`) are erased, and
//!    a second record (state `ready`) goes to block 1. A power cut after
//!    the first record leaves a claimed region whose erase is finished at
//!    the next boot ([`FilesRegion::claim`] again).
//!
//! Claim records are written alternately to blocks 0 and 1 (the newer
//! sequence number counts), so a torn record leaves the previous one.
//!
//! The return to stock ([`FilesRegion::restore`]): a record `restoring`,
//! every block outside `stock_copy` erased and the copied blocks written
//! back (blocks 0 and 1 last, the one holding the newest record at the
//! very end), then `stock_copy` erased (its index last). Blocks SONOFF's
//! file system had freed come back erased. A claim that never committed is
//! only undone (its copy erased); SONOFF's blocks were never touched.

use std::io;

use hub_core::layout::dongle_files::{self as layout, Content};

use crate::crc::crc32c;
use crate::flash::Flash;
use crate::littlefs::{self, LfsError};

/// Block (sector) size.
pub const BLOCK: u32 = 4096;
/// Blocks in the region.
pub const BLOCKS: u32 = layout::SIZE / BLOCK;
/// First block of `stock_copy`: the index of the copy.
pub const INDEX_BLOCK: u32 = layout::STOCK_COPY.offset / BLOCK;
/// Copy slots after the index.
pub const SLOTS: usize = (layout::STOCK_COPY.size / BLOCK - 1) as usize;
/// The hub's areas, erased by the claim: `hubdata`, `ota_store`, `radio`.
const HUB_AREAS: core::ops::Range<u32> = layout::HUBDATA.offset / BLOCK..INDEX_BLOCK;

const RECORD_MAGIC: [u8; 4] = layout::CLAIM_MAGIC;
const RECORD_VERSION: u32 = 1;
const RECORD_LEN: usize = 20;
const INDEX_MAGIC: [u8; 4] = *b"ZRHS";
const INDEX_DONE: [u8; 4] = *b"DONE";
const INDEX_VERSION: u32 = 1;
/// Index header: magic, done word, version, count, flags.
const INDEX_HEAD: usize = 20;
/// Index flag: the copy holds the blocks the LittleFS walk found (the
/// others may be written, free in SONOFF's file system).
const WALKED: u32 = 1;

/// What a claim record says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimState {
    /// Committed; the hub's areas are being erased.
    Erasing,
    /// The hub's region.
    Ready,
    /// The return to stock is under way.
    Restoring,
}

impl ClaimState {
    fn code(self) -> u8 {
        match self {
            ClaimState::Erasing => 1,
            ClaimState::Ready => 2,
            ClaimState::Restoring => 3,
        }
    }

    fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            1 => ClaimState::Erasing,
            2 => ClaimState::Ready,
            3 => ClaimState::Restoring,
            _ => return None,
        })
    }

    /// `erasing`, `ready`, `restoring`.
    pub fn as_str(self) -> &'static str {
        match self {
            ClaimState::Erasing => "erasing",
            ClaimState::Ready => "ready",
            ClaimState::Restoring => "restoring",
        }
    }
}

/// A claim record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    /// Sequence number (the newer record counts).
    pub seq: u32,
    /// State.
    pub state: ClaimState,
}

impl Record {
    fn encode(&self) -> [u8; RECORD_LEN] {
        let mut b = [0u8; RECORD_LEN];
        b[..4].copy_from_slice(&RECORD_MAGIC);
        b[4..8].copy_from_slice(&RECORD_VERSION.to_le_bytes());
        b[8..12].copy_from_slice(&self.seq.to_le_bytes());
        b[12] = self.state.code();
        let crc = crc32c(&b[..16]);
        b[16..20].copy_from_slice(&crc.to_le_bytes());
        b
    }

    fn decode(b: &[u8]) -> Option<Record> {
        let b = b.get(..RECORD_LEN)?;
        let word = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        if b[..4] != RECORD_MAGIC || word(4) != RECORD_VERSION || crc32c(&b[..16]) != word(16) {
            return None;
        }
        Some(Record { seq: word(8), state: ClaimState::from_code(b[12])? })
    }
}

/// The copy of SONOFF's blocks: block numbers and their CRC-32C.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    /// `(block, crc32c)` in slot order.
    pub blocks: Vec<(u32, u32)>,
    /// Every slot was written and read back.
    pub done: bool,
    /// The blocks are those the LittleFS walk found, not every written one.
    pub walked: bool,
}

impl Index {
    /// The index block as written, `DONE` included.
    fn image(blocks: &[(u32, u32)], walked: bool) -> Vec<u8> {
        let mut b = vec![0xFF; BLOCK as usize];
        b[..4].copy_from_slice(&INDEX_MAGIC);
        b[4..8].copy_from_slice(&INDEX_DONE);
        let mut body = Vec::with_capacity(12 + 8 * blocks.len());
        body.extend_from_slice(&INDEX_VERSION.to_le_bytes());
        body.extend_from_slice(&(blocks.len() as u32).to_le_bytes());
        body.extend_from_slice(&(if walked { WALKED } else { 0 }).to_le_bytes());
        for (block, crc) in blocks {
            body.extend_from_slice(&block.to_le_bytes());
            body.extend_from_slice(&crc.to_le_bytes());
        }
        let crc = crc32c(&body);
        body.extend_from_slice(&crc.to_le_bytes());
        b[8..8 + body.len()].copy_from_slice(&body);
        b
    }

    fn decode(b: &[u8]) -> Option<Index> {
        if b.get(..4)? != INDEX_MAGIC {
            return None;
        }
        let word = |i: usize| -> Option<u32> { Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?)) };
        let n = word(12)? as usize;
        if word(8)? != INDEX_VERSION || n > SLOTS {
            return None;
        }
        let end = INDEX_HEAD + 8 * n;
        if crc32c(b.get(8..end)?) != word(end)? {
            return None;
        }
        let blocks = (0..n).map(|i| Some((word(INDEX_HEAD + 8 * i)?, word(INDEX_HEAD + 8 * i + 4)?))).collect::<Option<Vec<_>>>()?;
        if blocks.iter().any(|(b, _)| *b >= INDEX_BLOCK) {
            return None;
        }
        Some(Index { blocks, done: b.get(4..8)? == INDEX_DONE, walked: word(16)? & WALKED != 0 })
    }

    fn crc_of(&self, block: u32) -> Option<u32> {
        self.blocks.iter().find(|(b, _)| *b == block).map(|(_, c)| *c)
    }
}

/// What the index block holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyState {
    /// Erased: no copy.
    None,
    /// A copy made by the hub (complete when `done`).
    Hub(Index),
    /// Data that is no valid index (a torn first write of a claim, or
    /// SONOFF's file system using the block).
    Other,
}

/// What the region holds ([`FilesRegion::status`], quick: a few hundred
/// bytes read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// The first blocks as [`layout::classify`] sees them.
    pub content: Content,
    /// The newest valid claim record: the region is the hub's.
    pub record: Option<Record>,
    /// The copy of SONOFF's blocks.
    pub copy: CopyState,
}

impl Status {
    /// The region is the hub's, ready for use.
    pub fn ready(&self) -> bool {
        self.record.is_some_and(|r| r.state == ClaimState::Ready)
    }

    /// One line for the console and the log.
    pub fn describe(&self) -> String {
        let copy = match &self.copy {
            CopyState::Hub(ix) if ix.done => format!("copy of SONOFF's data: {} blocks", ix.blocks.len()),
            CopyState::Hub(ix) => format!("an interrupted copy of {} blocks", ix.blocks.len()),
            CopyState::Other | CopyState::None => String::new(),
        };
        let what = match (self.record, self.content) {
            (Some(r), _) => match r.state {
                ClaimState::Ready => "claimed by the hub".to_string(),
                ClaimState::Erasing => "claimed by the hub, its areas being erased (finished at boot)".to_string(),
                ClaimState::Restoring => "claimed, an interrupted return to stock (`stock confirm` resumes it)".to_string(),
            },
            (None, Content::LittleFs) => "SONOFF's LittleFS, not claimed".to_string(),
            (None, Content::Blank) => "erased, not claimed".to_string(),
            (None, Content::Claimed) => "an interrupted claim (`files claim confirm` finishes it)".to_string(),
            (None, Content::Unknown) => "unknown content, not claimed".to_string(),
        };
        if copy.is_empty() { what } else { format!("{what}; {copy}") }
    }
}

/// How [`FilesRegion::scan`] found SONOFF's blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanMethod {
    /// The LittleFS walk ([`crate::littlefs`]): the blocks the file
    /// system uses.
    LittleFs,
    /// Every block that is not erased (no LittleFS the walk understands).
    NonErased,
}

/// What the claim found ([`FilesRegion::scan`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    /// The first blocks.
    pub content: Content,
    /// SONOFF's blocks outside `stock_copy`: what the claim copies.
    pub used: Vec<u32>,
    /// SONOFF's blocks inside `stock_copy`: the claim is refused.
    pub copy_area_used: Vec<u32>,
    /// Blocks of `stock_copy` that are not erased (SONOFF's or free).
    pub copy_area_written: Vec<u32>,
    /// Blocks of the region that are not erased.
    pub written: usize,
    /// How `used` was found.
    pub method: ScanMethod,
    /// Why the LittleFS walk did not count (LittleFS content only).
    pub fallback: Option<String>,
}

/// Why a claim or a return to stock is refused or failed.
#[derive(Debug)]
pub enum FilesError {
    /// Flash access.
    Io(io::Error),
    /// The region holds neither SONOFF's LittleFS nor erased flash.
    UnknownContent,
    /// More blocks in use than the copy holds.
    TooManyBlocks {
        /// Blocks in use.
        used: usize,
    },
    /// `stock_copy` holds data that is not the hub's: SONOFF's file system
    /// may use those blocks.
    CopyAreaInUse {
        /// The blocks.
        blocks: Vec<u32>,
    },
    /// The copy of SONOFF's blocks does not verify: no way back through
    /// the hub (USB: SONOFF Dongle Flasher or a backup).
    CopyDamaged,
    /// SONOFF's blocks changed after the claim began, not through the hub.
    Inconsistent,
    /// A block read back differs from what was written.
    Verify {
        /// The block.
        block: u32,
    },
}

impl std::fmt::Display for FilesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FilesError::Io(e) => write!(f, "flash: {e}"),
            FilesError::UnknownContent => f.write_str("the files partition holds neither SONOFF's LittleFS nor erased flash"),
            FilesError::TooManyBlocks { used } => write!(
                f,
                "SONOFF's file system has {used} blocks in use, the copy holds {SLOTS}: reset SONOFF's settings (a staged MG24 update takes many blocks) or take a USB backup"
            ),
            FilesError::CopyAreaInUse { blocks } => {
                write!(f, "the stock_copy area is in use by SONOFF's file system (blocks {blocks:?})")
            }
            FilesError::CopyDamaged => {
                f.write_str("the copy of SONOFF's data does not verify; the way back is USB (SONOFF Dongle Flasher or a backup)")
            }
            FilesError::Inconsistent => f.write_str("SONOFF's data changed after the claim began, not through the hub"),
            FilesError::Verify { block } => write!(f, "block {block} reads back different from what was written"),
        }
    }
}

impl std::error::Error for FilesError {}

impl From<io::Error> for FilesError {
    fn from(e: io::Error) -> Self {
        FilesError::Io(e)
    }
}

/// What [`FilesRegion::claim`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claimed {
    /// The region was claimed now (or a committed claim finished).
    Now {
        /// SONOFF's blocks copied.
        copied: usize,
    },
    /// It was the hub's already.
    Already,
}

/// What [`FilesRegion::restore`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restored {
    /// Nothing to give back: never claimed, no copy of the hub's.
    Nothing,
    /// A claim that had not committed was undone (its copy erased).
    Undone,
    /// SONOFF's blocks were written back.
    Restored {
        /// Blocks written back.
        blocks: usize,
    },
}

fn is_blank(b: &[u8]) -> bool {
    b.iter().all(|&x| x == 0xFF)
}

/// `actual` could be a write of `intended` cut short: it clears only bits
/// that `intended` clears.
fn partial_write_of(actual: &[u8], intended: &[u8]) -> bool {
    actual.len() == intended.len() && actual.iter().zip(intended).all(|(a, t)| !a & t == 0)
}

/// The `files` partition (the whole of it, [`BLOCKS`] blocks).
pub struct FilesRegion<F: Flash> {
    flash: F,
}

impl<F: Flash> FilesRegion<F> {
    /// The region on `flash`.
    pub fn new(flash: F) -> Self {
        Self { flash }
    }

    /// The flash back.
    pub fn into_inner(self) -> F {
        self.flash
    }

    fn read_block(&mut self, block: u32) -> io::Result<Vec<u8>> {
        let mut b = vec![0u8; BLOCK as usize];
        self.flash.read(block * BLOCK, &mut b)?;
        Ok(b)
    }

    fn block_blank(&mut self, block: u32) -> io::Result<bool> {
        let mut buf = [0u8; 512];
        for at in (0..BLOCK).step_by(buf.len()) {
            self.flash.read(block * BLOCK + at, &mut buf)?;
            if !is_blank(&buf) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn erase(&mut self, block: u32) -> io::Result<()> {
        self.flash.erase(block)
    }

    /// Writes `data` into the erased `block` and reads it back.
    fn program(&mut self, block: u32, data: &[u8]) -> Result<(), FilesError> {
        self.flash.write(block * BLOCK, data)?;
        if self.read_block(block)? != data {
            return Err(FilesError::Verify { block });
        }
        Ok(())
    }

    fn content(&mut self) -> io::Result<Content> {
        let mut b0 = [0u8; 64];
        let mut b1 = [0u8; 64];
        self.flash.read(0, &mut b0)?;
        self.flash.read(BLOCK, &mut b1)?;
        Ok(layout::classify(&b0, &b1))
    }

    /// The newest valid claim record and the block holding it.
    fn record(&mut self) -> io::Result<Option<(u32, Record)>> {
        let mut best: Option<(u32, Record)> = None;
        for block in 0..2 {
            let mut b = [0u8; RECORD_LEN];
            self.flash.read(block * BLOCK, &mut b)?;
            if let Some(r) = Record::decode(&b)
                && best.is_none_or(|(_, x)| r.seq > x.seq)
            {
                best = Some((block, r));
            }
        }
        Ok(best)
    }

    /// Writes a record `state` into the claim block not holding the newest
    /// record (block 0 when there is none).
    fn write_record(&mut self, state: ClaimState) -> Result<(), FilesError> {
        let (block, seq) = match self.record()? {
            Some((b, r)) => (1 - b, r.seq.wrapping_add(1)),
            None => (0, 1),
        };
        self.erase(block)?;
        let rec = Record { seq, state }.encode();
        self.flash.write(block * BLOCK, &rec)?;
        let mut back = [0u8; RECORD_LEN];
        self.flash.read(block * BLOCK, &mut back)?;
        if back != rec {
            return Err(FilesError::Verify { block });
        }
        Ok(())
    }

    fn copy_state(&mut self) -> io::Result<(CopyState, Vec<u8>)> {
        let b = self.read_block(INDEX_BLOCK)?;
        let state = if is_blank(&b) {
            CopyState::None
        } else {
            Index::decode(&b).map_or(CopyState::Other, CopyState::Hub)
        };
        Ok((state, b))
    }

    /// What the region holds (reads the claim blocks and the index).
    pub fn status(&mut self) -> io::Result<Status> {
        Ok(Status { content: self.content()?, record: self.record()?.map(|(_, r)| r), copy: self.copy_state()?.0 })
    }

    /// SONOFF's blocks (reads the whole region): those its LittleFS uses
    /// when the walk understands it, else every block that is not erased.
    /// Deterministic for the same flash contents.
    pub fn scan(&mut self) -> io::Result<Scan> {
        let content = self.content()?;
        let mut written = Vec::new();
        for block in 0..BLOCKS {
            if !self.block_blank(block)? {
                written.push(block);
            }
        }
        let (walked, fallback) = if content == Content::LittleFs {
            let flash = &mut self.flash;
            match littlefs::blocks_in_use(&mut |b, off, buf| flash.read(b * BLOCK + off, buf), BLOCK, BLOCKS) {
                Ok(set) => (Some(set), None),
                Err(LfsError::Io(e)) => return Err(e),
                Err(e @ LfsError::Corrupt(_)) => (None, Some(e.to_string())),
            }
        } else {
            (None, None)
        };
        let sonoffs = |b: &u32| walked.as_ref().is_none_or(|set| set.contains(b));
        let used = written.iter().filter(|&&b| b < INDEX_BLOCK && sonoffs(&b)).copied().collect();
        let copy_area_written: Vec<u32> = written.iter().filter(|&&b| b >= INDEX_BLOCK).copied().collect();
        let copy_area_used = copy_area_written.iter().filter(|b| sonoffs(b)).copied().collect();
        let method = if walked.is_some() { ScanMethod::LittleFs } else { ScanMethod::NonErased };
        Ok(Scan { content, used, copy_area_used, copy_area_written, written: written.len(), method, fallback })
    }

    fn crcs(&mut self, blocks: &[u32]) -> io::Result<Vec<(u32, u32)>> {
        blocks.iter().map(|&b| Ok((b, crc32c(&self.read_block(b)?)))).collect()
    }

    /// Every slot holds the block the index names.
    fn slots_verify(&mut self, ix: &Index) -> io::Result<bool> {
        for (i, (_, crc)) in ix.blocks.iter().enumerate() {
            if crc32c(&self.read_block(INDEX_BLOCK + 1 + i as u32)?) != *crc {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The blocks outside `stock_copy` are SONOFF's as copied (blocks in
    /// `skip` not looked at): the copied ones hold what was copied, and,
    /// unless the copy came from the LittleFS walk, the others are erased.
    /// (After a walk the others may be free blocks of SONOFF's; any change
    /// of its file system rewrites a metadata block, which is copied.)
    fn matches(&mut self, ix: &Index, skip: &[u32]) -> io::Result<bool> {
        for block in (0..INDEX_BLOCK).filter(|b| !skip.contains(b)) {
            let ok = match ix.crc_of(block) {
                Some(crc) => crc32c(&self.read_block(block)?) == crc,
                None => ix.walked || self.block_blank(block)?,
            };
            if !ok {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The claim blocks (0, 1) still hold what was copied: the commit did
    /// not begin.
    fn claim_blocks_intact(&mut self, ix: &Index) -> io::Result<bool> {
        for block in 0..2 {
            let ok = match ix.crc_of(block) {
                Some(crc) => crc32c(&self.read_block(block)?) == crc,
                None => self.block_blank(block)?,
            };
            if !ok {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Erases `stock_copy`, its index last.
    fn erase_copy(&mut self) -> io::Result<()> {
        for block in (INDEX_BLOCK..BLOCKS).rev() {
            if !self.block_blank(block)? {
                self.erase(block)?;
            }
        }
        Ok(())
    }

    /// Erases the hub's areas (blocks that are erased already are only
    /// read: most of the region usually is).
    fn erase_hub_areas(&mut self) -> io::Result<()> {
        for block in HUB_AREAS {
            if !self.block_blank(block)? {
                self.erase(block)?;
            }
        }
        Ok(())
    }

    /// Makes the region the hub's (see the module docs). Idempotent: run
    /// it again after a power cut, or at boot when [`Status::record`] says
    /// [`ClaimState::Erasing`]; it refuses (writing nothing) when SONOFF's
    /// data cannot be kept.
    pub fn claim(&mut self) -> Result<Claimed, FilesError> {
        if let Some((_, rec)) = self.record()? {
            if rec.state == ClaimState::Ready {
                return Ok(Claimed::Already);
            }
            // Committed (or a return to stock is abandoned for a new claim:
            // the copy stays until the region is restored).
            let copied = match self.copy_state()?.0 {
                CopyState::Hub(ix) => ix.blocks.len(),
                _ => 0,
            };
            if rec.state == ClaimState::Restoring {
                self.write_record(ClaimState::Erasing)?;
            }
            self.erase_hub_areas()?;
            self.write_record(ClaimState::Ready)?;
            return Ok(Claimed::Now { copied });
        }
        let (copy, raw) = self.copy_state()?;
        match copy {
            CopyState::Hub(ix) if ix.done && self.slots_verify(&ix)? => {
                if self.matches(&ix, &[0, 1])? {
                    // Not begun, or the commit began (block 0 is being made
                    // the record): commit.
                    return self.commit(&ix);
                }
                if !self.claim_blocks_intact(&ix)? {
                    return Err(FilesError::Inconsistent);
                }
                // SONOFF's file system changed since the copy (its
                // firmware ran in between): copy again.
                self.erase_copy()?;
            }
            CopyState::Hub(ix) => {
                // An interrupted copy (the commit follows `DONE`: it never
                // began), or a complete one damaged before the commit.
                if ix.done && !self.claim_blocks_intact(&ix)? {
                    return Err(FilesError::Inconsistent);
                }
                self.erase_copy()?;
            }
            CopyState::Other => {
                // A block SONOFF's file system does not use (the LittleFS
                // walk), the claim's first write (the index) cut short, or
                // SONOFF's.
                let scan = self.scan()?;
                let erasable = scan.copy_area_used.is_empty() || self.torn_index(&scan, &raw)?;
                if !erasable {
                    return Err(FilesError::CopyAreaInUse { blocks: scan.copy_area_used });
                }
                self.erase(INDEX_BLOCK)?;
            }
            CopyState::None => {}
        }
        self.copy_and_commit()
    }

    fn copy_and_commit(&mut self) -> Result<Claimed, FilesError> {
        let scan = self.scan()?;
        if !matches!(scan.content, Content::LittleFs | Content::Blank) {
            return Err(FilesError::UnknownContent);
        }
        if !scan.copy_area_used.is_empty() {
            return Err(FilesError::CopyAreaInUse { blocks: scan.copy_area_used });
        }
        if scan.used.len() > SLOTS {
            return Err(FilesError::TooManyBlocks { used: scan.used.len() });
        }
        if !scan.copy_area_written.is_empty() {
            // Blocks SONOFF's file system freed (the LittleFS walk).
            self.erase_copy()?;
        }
        let blocks = self.crcs(&scan.used)?;
        let walked = scan.method == ScanMethod::LittleFs;
        let image = Index::image(&blocks, walked);
        let head = INDEX_HEAD + 8 * blocks.len() + 4;
        // The body, then the magic: a valid magic means a whole index.
        self.flash.write(INDEX_BLOCK * BLOCK + 8, &image[8..head])?;
        self.flash.write(INDEX_BLOCK * BLOCK, &image[..4])?;
        for (i, &(block, crc)) in blocks.iter().enumerate() {
            let data = self.read_block(block)?;
            if crc32c(&data) != crc {
                return Err(FilesError::Inconsistent);
            }
            self.program(INDEX_BLOCK + 1 + i as u32, &data)?;
        }
        self.flash.write(INDEX_BLOCK * BLOCK + 4, &image[4..8])?;
        let ix = Index { blocks, done: true, walked };
        self.commit(&ix)
    }

    /// `raw` (the index block, all of `stock_copy` that is written) is the
    /// claim's first write cut short: a partial write of the index the
    /// claim makes of this region.
    fn torn_index(&mut self, scan: &Scan, raw: &[u8]) -> io::Result<bool> {
        Ok(scan.copy_area_written == [INDEX_BLOCK]
            && scan.used.len() <= SLOTS
            && partial_write_of(raw, &Index::image(&self.crcs(&scan.used)?, scan.method == ScanMethod::LittleFs)))
    }

    fn commit(&mut self, ix: &Index) -> Result<Claimed, FilesError> {
        // Block 0 (LittleFS superblock) becomes the claim record: the
        // region is the hub's from here.
        self.write_record(ClaimState::Erasing)?;
        self.erase_hub_areas()?;
        self.write_record(ClaimState::Ready)?;
        Ok(Claimed::Now { copied: ix.blocks.len() })
    }

    /// Gives SONOFF's data back (see the module docs). Idempotent; the hub
    /// must not use the region meanwhile.
    pub fn restore(&mut self) -> Result<Restored, FilesError> {
        let record = self.record()?;
        let (copy, raw) = self.copy_state()?;
        let ix = match (record, copy) {
            (None, CopyState::None) => return Ok(Restored::Nothing),
            (None, CopyState::Other) => {
                // A claim cut short at its first write (the index), whose
                // blocks are SONOFF's untouched; anything else is not ours.
                let scan = self.scan()?;
                if self.torn_index(&scan, &raw)? {
                    self.erase(INDEX_BLOCK)?;
                    return Ok(Restored::Undone);
                }
                return Ok(Restored::Nothing);
            }
            (None, CopyState::Hub(ix)) => {
                // Never committed (the commit begins with block 0, a return
                // writes the claim blocks back last), written back already,
                // or SONOFF's file system changed since the copy: SONOFF's
                // blocks are its own, only the copy goes.
                if !ix.done || self.claim_blocks_intact(&ix)? {
                    self.erase_copy()?;
                    return Ok(Restored::Undone);
                }
                // A return cut short while writing the claim blocks back.
                if !(ix.done && self.slots_verify(&ix)?) {
                    return Err(FilesError::CopyDamaged);
                }
                ix
            }
            (Some((_, rec)), copy) => {
                let ix = match copy {
                    CopyState::Hub(ix) if ix.done => ix,
                    _ => return Err(FilesError::CopyDamaged),
                };
                if !self.slots_verify(&ix)? {
                    return Err(FilesError::CopyDamaged);
                }
                if rec.state != ClaimState::Restoring {
                    self.write_record(ClaimState::Restoring)?;
                }
                ix
            }
        };
        self.write_back(&ix)?;
        self.erase_copy()?;
        Ok(Restored::Restored { blocks: ix.blocks.len() })
    }

    fn slot_of(ix: &Index, block: u32) -> Option<u32> {
        ix.blocks.iter().position(|(b, _)| *b == block).map(|i| INDEX_BLOCK + 1 + i as u32)
    }

    /// `block` as copied (erased when it is not in the copy).
    fn put_back(&mut self, ix: &Index, block: u32) -> Result<(), FilesError> {
        match Self::slot_of(ix, block) {
            Some(slot) => {
                let data = self.read_block(slot)?;
                if self.read_block(block)? != data {
                    self.erase(block)?;
                    self.program(block, &data)?;
                }
            }
            None if !self.block_blank(block)? => self.erase(block)?,
            None => {}
        }
        Ok(())
    }

    fn write_back(&mut self, ix: &Index) -> Result<(), FilesError> {
        for block in HUB_AREAS {
            self.put_back(ix, block)?;
        }
        // The claim blocks last, the newest record's at the very end.
        let last = self.record()?.map_or(1, |(b, _)| b);
        self.put_back(ix, 1 - last)?;
        self.put_back(ix, last)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testutil::{CutFlash, cut_points};

    fn noise(seed: u32, n: usize) -> Vec<u8> {
        let mut x = seed.wrapping_mul(2_654_435_761) | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    /// SONOFF's LittleFS as on the lab unit: the superblock pair in blocks
    /// 0 and 1 and a file block inside `hubdata` (411), plus `extra` more
    /// blocks in use; erased elsewhere.
    fn sonoff(extra: &[u32]) -> Vec<u8> {
        let mut d = vec![0xFF; layout::SIZE as usize];
        for (i, &block) in [0u32, 1, 411].iter().chain(extra).enumerate() {
            let s = (block * BLOCK) as usize;
            let len = if block < 2 { 600 } else { 3000 };
            d[s..s + len].copy_from_slice(&noise(block + 7 * i as u32, len));
            if block < 2 {
                d[s..s + 8].copy_from_slice(&[(block + 1) as u8, 0, 0, 0, 0xF0, 0x0F, 0xFF, 0xF7]);
                d[s + 8..s + 16].copy_from_slice(b"littlefs");
            }
        }
        d
    }

    fn region(data: Vec<u8>) -> FilesRegion<CutFlash> {
        FilesRegion::new(CutFlash::new(data))
    }

    fn bytes(r: &FilesRegion<CutFlash>) -> &[u8] {
        &r.flash.data
    }

    /// The hub's view of a claimed region: records, the copy and, after a
    /// fresh claim (`erased`), nothing else.
    fn assert_claimed(r: &mut FilesRegion<CutFlash>, original: &[u8], copied: usize, erased: bool) {
        let st = r.status().unwrap();
        assert_eq!(st.content, Content::Claimed);
        assert!(st.ready(), "{st:?}");
        let CopyState::Hub(ix) = st.copy else { panic!("no copy: {st:?}") };
        assert!(ix.done);
        assert_eq!(ix.blocks.len(), copied);
        for (i, (block, crc)) in ix.blocks.iter().enumerate() {
            let s = (block * BLOCK) as usize;
            assert_eq!(crc32c(&original[s..s + BLOCK as usize]), *crc, "block {block}");
            let slot = ((INDEX_BLOCK + 1 + i as u32) * BLOCK) as usize;
            assert_eq!(&bytes(r)[slot..slot + BLOCK as usize], &original[s..s + BLOCK as usize]);
        }
        let hub = (HUB_AREAS.start * BLOCK) as usize..(HUB_AREAS.end * BLOCK) as usize;
        assert!(!erased || is_blank(&bytes(r)[hub]), "the hub's areas are erased");
    }

    /// Writes some hub data into `hubdata`, `ota_store` and `radio`.
    fn use_region(r: &mut FilesRegion<CutFlash>) {
        for block in [2u32, 3, 411, 600, 897] {
            r.flash.write(block * BLOCK, &noise(block, 100)).unwrap();
        }
    }

    #[test]
    fn claim_copies_sonoffs_blocks_and_restore_gives_them_back() {
        let original = sonoff(&[]);
        let mut r = region(original.clone());
        let st = r.status().unwrap();
        assert_eq!((st.content, st.record, st.copy.clone()), (Content::LittleFs, None, CopyState::None));
        assert!(st.describe().contains("SONOFF's LittleFS"));
        let scan = r.scan().unwrap();
        assert_eq!(scan.used, [0, 1, 411]);
        assert!(scan.copy_area_used.is_empty());

        assert_eq!(r.claim().unwrap(), Claimed::Now { copied: 3 });
        assert_claimed(&mut r, &original, 3, true);
        assert_eq!(r.claim().unwrap(), Claimed::Already, "idempotent");
        assert!(r.status().unwrap().describe().starts_with("claimed by the hub"));

        use_region(&mut r);
        assert_eq!(r.restore().unwrap(), Restored::Restored { blocks: 3 });
        assert!(bytes(&r) == original, "the region is SONOFF's again, byte for byte");
        assert_eq!(r.restore().unwrap(), Restored::Nothing);
        assert_eq!(r.status().unwrap().content, Content::LittleFs);
    }

    #[test]
    fn an_erased_region_is_claimed_without_a_copy() {
        let original = vec![0xFF; layout::SIZE as usize];
        let mut r = region(original.clone());
        assert_eq!(r.status().unwrap().content, Content::Blank);
        assert_eq!(r.claim().unwrap(), Claimed::Now { copied: 0 });
        assert_claimed(&mut r, &original, 0, true);
        use_region(&mut r);
        assert_eq!(r.restore().unwrap(), Restored::Restored { blocks: 0 });
        assert!(bytes(&r) == original);
    }

    #[test]
    fn refusals_write_nothing() {
        // More blocks in use than the copy holds.
        let extra: Vec<u32> = (100..100 + SLOTS as u32 - 2).collect();
        let full = sonoff(&extra);
        let mut r = region(full.clone());
        match r.claim() {
            Err(FilesError::TooManyBlocks { used }) => assert_eq!(used, SLOTS + 1),
            other => panic!("{other:?}"),
        }
        assert!(bytes(&r) == full);
        // Exactly as many fit.
        let fits = sonoff(&extra[1..]);
        let mut r = region(fits.clone());
        assert_eq!(r.claim().unwrap(), Claimed::Now { copied: SLOTS });
        assert_claimed(&mut r, &fits, SLOTS, true);
        assert_eq!(r.restore().unwrap(), Restored::Restored { blocks: SLOTS });
        assert!(bytes(&r) == fits);

        // SONOFF's file system uses a block of stock_copy.
        let taken = sonoff(&[INDEX_BLOCK + 5]);
        let mut r = region(taken.clone());
        match r.claim() {
            Err(FilesError::CopyAreaInUse { blocks }) => assert_eq!(blocks, [INDEX_BLOCK + 5]),
            other => panic!("{other:?}"),
        }
        assert!(bytes(&r) == taken);
        // ... or its first block, which is no index of the hub's.
        let taken = sonoff(&[INDEX_BLOCK]);
        let mut r = region(taken.clone());
        assert!(matches!(r.claim(), Err(FilesError::CopyAreaInUse { .. })));
        assert_eq!(r.restore().unwrap(), Restored::Nothing, "not the hub's to undo");
        assert!(bytes(&r) == taken);

        // Neither LittleFS nor erased.
        let mut unknown = vec![0xFF; layout::SIZE as usize];
        unknown[..64].fill(0);
        let mut r = region(unknown.clone());
        assert_eq!(r.status().unwrap().content, Content::Unknown);
        assert!(matches!(r.claim(), Err(FilesError::UnknownContent)));
        assert!(bytes(&r) == unknown);
    }

    /// Operations of an uninterrupted run of `f`.
    fn ops(data: &[u8], f: impl Fn(&mut FilesRegion<CutFlash>)) -> usize {
        let mut r = region(data.to_vec());
        f(&mut r);
        r.flash.ops
    }

    #[test]
    fn a_power_cut_anywhere_in_the_claim_then_claim_again_or_return_to_stock() {
        let original = sonoff(&[3, 700]);
        let total = ops(&original, |r| {
            r.claim().unwrap();
        });
        for k in cut_points(total) {
            for torn in [0, 9, 2048] {
                let mut r = FilesRegion::new(CutFlash::new(original.clone()).cut_at(k, torn));
                assert!(r.claim().is_err(), "cut at {k}");
                r.flash.power_on();
                let mut back = FilesRegion::new(r.flash.clone());
                // Claim again: the region becomes the hub's with the copy
                // (`Already` when the cut came after the last write).
                assert!(r.claim().is_ok(), "claim after a cut at {k}/{torn}: {:?}", r.status());
                assert_claimed(&mut r, &original, 5, true);
                r.restore().unwrap();
                assert!(bytes(&r) == original, "restore after the claim, cut at {k}/{torn}");
                // Or go back to stock right away.
                back.restore().unwrap_or_else(|e| panic!("restore after a cut at {k}/{torn}: {e}"));
                assert!(bytes(&back) == original, "restore after a cut at {k}/{torn}");
            }
        }
    }

    #[test]
    fn a_power_cut_anywhere_in_the_return_to_stock_then_again_or_claim_again() {
        let original = sonoff(&[3, 700]);
        let mut claimed = region(original.clone());
        claimed.claim().unwrap();
        use_region(&mut claimed);
        let claimed = claimed.into_inner().data;
        let total = ops(&claimed, |r| {
            r.restore().unwrap();
        });
        for k in cut_points(total) {
            for torn in [0, 9, 2048] {
                let mut r = FilesRegion::new(CutFlash::new(claimed.clone()).cut_at(k, torn));
                assert!(r.restore().is_err(), "cut at {k}");
                r.flash.power_on();
                let mut again = FilesRegion::new(r.flash.clone());
                r.restore().unwrap_or_else(|e| panic!("restore after a cut at {k}/{torn}: {e}"));
                assert!(bytes(&r) == original, "restore after a cut at {k}/{torn}");
                // Or claim again (the copy is kept until the restore ends).
                again.claim().unwrap_or_else(|e| panic!("claim after a cut at {k}/{torn}: {e}"));
                // The hub's data stay when the cut came before the restore
                // erased anything.
                assert_claimed(&mut again, &original, 5, false);
                again.restore().unwrap();
                assert!(bytes(&again) == original);
            }
        }
    }

    #[test]
    fn a_committed_claim_cut_short_is_finished_by_claim() {
        let original = sonoff(&[]);
        let mut r = region(original.clone());
        r.claim().unwrap();
        // As if the power failed while erasing the hub's areas: the first
        // record only, a block not erased yet.
        r.flash.erase(1).unwrap();
        r.flash.write(5 * BLOCK, &[0x00; 16]).unwrap();
        let st = r.status().unwrap();
        assert_eq!(st.record.map(|r| r.state), Some(ClaimState::Erasing));
        assert!(!st.ready());
        assert!(st.describe().contains("finished at boot"));
        assert_eq!(r.claim().unwrap(), Claimed::Now { copied: 3 });
        assert_claimed(&mut r, &original, 3, true);
    }

    #[test]
    fn sonoffs_superblock_changed_after_an_interrupted_copy() {
        let original = sonoff(&[]);
        // Cut while the second slot is written: the copy is not done ...
        let mut r = FilesRegion::new(CutFlash::new(original.clone()).cut_at(3, 100));
        assert!(r.claim().is_err());
        r.flash.power_on();
        assert!(matches!(r.status().unwrap().copy, CopyState::Hub(Index { done: false, .. })));
        let mut cut = r.into_inner();
        // ... then SONOFF's firmware ran and rewrote its superblock.
        cut.write(64, &[0u8; 8]).unwrap();
        let mut after = original.clone();
        after[64..72].fill(0);
        let mut back = FilesRegion::new(cut.clone());
        assert_eq!(back.restore().unwrap(), Restored::Undone);
        assert!(bytes(&back) == after);
        let mut r = FilesRegion::new(cut);
        assert_eq!(r.claim().unwrap(), Claimed::Now { copied: 3 });
        assert_claimed(&mut r, &after, 3, true);
    }

    #[test]
    fn sonoffs_file_system_changed_after_an_interrupted_copy_is_copied_again() {
        let original = sonoff(&[]);
        // The copy is written (index body, magic, 3 slots, DONE), the
        // commit not begun ...
        let mut r = FilesRegion::new(CutFlash::new(original.clone()).cut_at(6, 0));
        assert!(r.claim().is_err());
        r.flash.power_on();
        assert!(matches!(r.status().unwrap().copy, CopyState::Hub(Index { done: true, .. })));
        // ... then SONOFF's firmware ran and wrote another block.
        let mut changed = r.into_inner();
        changed.write(800 * BLOCK, &noise(1, 64)).unwrap();
        let mut after = original.clone();
        let at = (800 * BLOCK) as usize;
        after[at..at + 64].copy_from_slice(&noise(1, 64));
        let mut r = FilesRegion::new(changed);
        // Back to stock now: SONOFF's newer block stays, the copy goes.
        let mut back = FilesRegion::new(r.flash.clone());
        assert_eq!(back.restore().unwrap(), Restored::Undone);
        assert!(bytes(&back) == after);
        // Or claim: the copy is made again.
        assert_eq!(r.claim().unwrap(), Claimed::Now { copied: 4 });
        assert_claimed(&mut r, &after, 4, true);
        r.restore().unwrap();
        assert!(bytes(&r) == after);
    }
}
