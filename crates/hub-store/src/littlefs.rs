//! The blocks a LittleFS v2 image uses, read only: the walk of
//! `lfs_fs_traverse` (with orphans) over the on-disk format of littlefs's
//! `SPEC.md`, for the claim of the Dongle-M's `files` partition
//! ([`crate::files`]): only the blocks SONOFF's file system needs are
//! copied, not every block it ever wrote (LittleFS does not erase the
//! blocks it frees).
//!
//! From the superblock pair {0, 1} the metadata pairs are followed through
//! their tails (the threaded list every directory is on) and through the
//! directory structs (orphans included): after a power cut while littlefs
//! relocates a directory, the parent already points at the new pair while
//! the tails still list the old one, and littlefs reads the new pair. Of
//! each pair the block with the newer revision that has a valid commit
//! counts, its entries replayed over every valid commit (create and delete
//! splices, the last struct of each id), as `lfs_dir_fetch`: a torn commit
//! after the last valid one is ignored, whatever its tags. The CTZ
//! skip-lists of the files are walked; both blocks of every pair count. So
//! the walk finds what `lfs_fs_traverse` finds, and on such an interrupted
//! relocation also the new pair's files. Anything that does not add up (a
//! valid commit with a splice of an id that does not exist, a CRC that
//! never matches, a pointer beyond the file system, a cycle, an unknown
//! superblock) is [`LfsError::Corrupt`]: the caller then counts every
//! block that is not erased instead.

use std::collections::{BTreeSet, VecDeque};
use std::io;

/// Why no walk.
#[derive(Debug)]
pub enum LfsError {
    /// Flash access.
    Io(io::Error),
    /// Not a LittleFS v2 image this walk understands.
    Corrupt(&'static str),
}

impl From<io::Error> for LfsError {
    fn from(e: io::Error) -> Self {
        LfsError::Io(e)
    }
}

impl std::fmt::Display for LfsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LfsError::Io(e) => write!(f, "flash: {e}"),
            LfsError::Corrupt(why) => write!(f, "not a LittleFS image this walk understands: {why}"),
        }
    }
}

/// Reads `buf.len()` bytes at byte `offset` of block `block`.
pub type Read<'a> = dyn FnMut(u32, u32, &mut [u8]) -> io::Result<()> + 'a;

const NULL: u32 = 0xFFFF_FFFF;
const TYPE_CREATE: u32 = 0x401;
const TYPE_DELETE: u32 = 0x4FF;
const TYPE_DIRSTRUCT: u32 = 0x200;
const TYPE_INLINESTRUCT: u32 = 0x201;
const TYPE_CTZSTRUCT: u32 = 0x202;
const TYPE_SUPERBLOCK: u32 = 0x0FF;

/// littlefs's CRC-32 (reflected 0x04C11DB7, no final XOR).
fn crc(mut crc: u32, data: &[u8]) -> u32 {
    const T: [u32; 16] = [
        0x0000_0000, 0x1DB7_1064, 0x3B6E_20C8, 0x26D9_30AC, 0x76DC_4190, 0x6B6B_51F4, 0x4DB2_6158, 0x5005_713C,
        0xEDB8_8320, 0xF00F_9344, 0xD6D6_A3E8, 0xCB61_B38C, 0x9B64_C2B0, 0x86D3_D2D4, 0xA00A_E278, 0xBDBD_F21C,
    ];
    for &b in data {
        crc = (crc >> 4) ^ T[((crc ^ u32::from(b)) & 0xF) as usize];
        crc = (crc >> 4) ^ T[((crc ^ (u32::from(b) >> 4)) & 0xF) as usize];
    }
    crc
}

fn valid(tag: u32) -> bool {
    tag & 0x8000_0000 == 0
}
fn type1(tag: u32) -> u32 {
    (tag & 0x7000_0000) >> 20
}
fn type2(tag: u32) -> u32 {
    (tag & 0x7800_0000) >> 20
}
fn type3(tag: u32) -> u32 {
    (tag & 0x7FF0_0000) >> 20
}
fn chunk(tag: u32) -> u32 {
    (tag & 0x0FF0_0000) >> 20
}
fn id(tag: u32) -> usize {
    ((tag & 0x000F_FC00) >> 10) as usize
}
fn deleted(tag: u32) -> bool {
    tag & 0x3FF == 0x3FF
}
/// Tag and data on disk.
fn dsize(tag: u32) -> u32 {
    4 + (tag.wrapping_add(u32::from(deleted(tag))) & 0x3FF)
}
fn le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// The last state of one metadata block.
#[derive(Debug, Clone, Default)]
struct Dir {
    /// Struct type and data of each id (`None`: no struct).
    entries: Vec<Option<(u32, Vec<u8>)>>,
    tail: Option<[u32; 2]>,
    /// The superblock's name tag (`littlefs`) seen for id 0.
    superblock: bool,
    /// Where a next commit would start: its offset and the tag its first
    /// tag is XORed with.
    end: (u32, u32),
}

/// Replays the valid commits of `block`; `None` without one. A tag that
/// does not add up only counts once its commit's CRC matches (a torn
/// commit's tags are anything).
fn fetch_block(read: &mut Read<'_>, block: u32, block_size: u32) -> Result<Option<Dir>, LfsError> {
    let mut b = vec![0u8; block_size as usize];
    read(block, 0, &mut b)?;
    let mut sum = crc(0xFFFF_FFFF, &b[..4]);
    let mut off = 0u32;
    let mut ptag = NULL;
    let mut temp = Dir::default();
    let mut committed = None;
    let mut bad = None;
    loop {
        off += dsize(ptag);
        if off + 4 > block_size {
            break;
        }
        let at = off as usize;
        sum = crc(sum, &b[at..at + 4]);
        let tag = u32::from_be_bytes(b[at..at + 4].try_into().unwrap()) ^ ptag;
        if !valid(tag) || off + dsize(tag) > block_size {
            break;
        }
        ptag = tag;
        let data = &b[at + 4..at + dsize(tag) as usize];
        if type2(tag) == 0x500 {
            // A commit's CRC: what came before it counts.
            if data.len() < 4 || le(data, 0) != sum {
                break;
            }
            if let Some(why) = bad {
                return Err(LfsError::Corrupt(why));
            }
            ptag ^= (chunk(tag) & 1) << 31;
            temp.end = (off + dsize(tag), ptag);
            committed = Some(temp.clone());
            sum = 0xFFFF_FFFF;
            continue;
        }
        sum = crc(sum, data);
        let i = id(tag);
        match type1(tag) {
            0x000 => {
                if temp.entries.len() <= i {
                    temp.entries.resize(i + 1, None);
                }
                if i == 0 && type3(tag) == TYPE_SUPERBLOCK && data == b"littlefs" {
                    temp.superblock = true;
                }
            }
            0x400 => match type3(tag) {
                TYPE_CREATE => temp.entries.insert(i.min(temp.entries.len()), None),
                TYPE_DELETE if i < temp.entries.len() => {
                    temp.entries.remove(i);
                }
                _ => {
                    bad.get_or_insert("splice of an id that does not exist");
                }
            },
            0x200 => {
                if temp.entries.len() <= i {
                    temp.entries.resize(i + 1, None);
                }
                temp.entries[i] = (!deleted(tag)).then(|| (type3(tag), data.to_vec()));
            }
            0x600 => {
                if data.len() < 8 {
                    bad.get_or_insert("short tail");
                } else {
                    temp.tail = Some([le(data, 0), le(data, 4)]);
                }
            }
            // Names of types, user attributes, global state, CRCs.
            _ => {}
        }
    }
    Ok(committed)
}

/// The pair's state: its newer block with a valid commit (littlefs's
/// `lfs_dir_fetch`), and that block.
fn fetch(read: &mut Read<'_>, pair: [u32; 2], block_size: u32) -> Result<(u32, Dir), LfsError> {
    let mut revs = [0u8; 4];
    read(pair[0], 0, &mut revs)?;
    let r0 = u32::from_le_bytes(revs);
    read(pair[1], 0, &mut revs)?;
    let r1 = u32::from_le_bytes(revs);
    // Sequence comparison of the revision counts.
    let order = if (r1.wrapping_sub(r0) as i32) > 0 { [pair[1], pair[0]] } else { pair };
    for block in order {
        if let Some(dir) = fetch_block(read, block, block_size)? {
            return Ok((block, dir));
        }
    }
    Err(LfsError::Corrupt("a metadata pair without a valid commit"))
}

/// The block of `pair` littlefs reads, where its next commit would start
/// and the tag that commit's first tag is XORed with.
#[cfg(test)]
pub(crate) fn commit_end(read: &mut Read<'_>, pair: [u32; 2], block_size: u32) -> Result<(u32, u32, u32), LfsError> {
    let (block, dir) = fetch(read, pair, block_size)?;
    Ok((block, dir.end.0, dir.end.1))
}

/// littlefs's `lfs_ctz_index`: the index of the block holding byte `off`.
fn ctz_index(block_size: u32, off: u32) -> u32 {
    let b = block_size - 8;
    let i = off / b;
    if i == 0 {
        return 0;
    }
    (off - 4 * ((i - 1).count_ones() + 2)) / b
}

/// Every block of a file's CTZ skip-list (`lfs_ctz_traverse`).
fn ctz(read: &mut Read<'_>, mut head: u32, size: u32, block_size: u32, count: u32, used: &mut BTreeSet<u32>) -> Result<(), LfsError> {
    if size == 0 {
        return Ok(());
    }
    let mut index = ctz_index(block_size, size - 1);
    loop {
        if head >= count {
            return Err(LfsError::Corrupt("file block beyond the file system"));
        }
        used.insert(head);
        if index == 0 {
            return Ok(());
        }
        let n = 2 - (index & 1);
        let mut heads = [0u8; 8];
        read(head, 0, &mut heads[..4 * n as usize])?;
        let first = le(&heads, 0);
        if n == 2 {
            if first >= count {
                return Err(LfsError::Corrupt("file block beyond the file system"));
            }
            used.insert(first);
        }
        head = le(&heads, 4 * (n as usize - 1));
        index -= n;
    }
}

/// The blocks of the LittleFS v2 image `read` reads (block size 4096, at
/// most `max_blocks` blocks).
pub fn blocks_in_use(read: &mut Read<'_>, block_size: u32, max_blocks: u32) -> Result<BTreeSet<u32>, LfsError> {
    let (_, root) = fetch(read, [0, 1], block_size)?;
    let sb = match root.entries.first() {
        Some(Some((TYPE_INLINESTRUCT, d))) if root.superblock && d.len() >= 12 => d,
        _ => return Err(LfsError::Corrupt("no superblock")),
    };
    let (version, bsize, count) = (le(sb, 0), le(sb, 4), le(sb, 8));
    if version >> 16 != 2 {
        return Err(LfsError::Corrupt("not LittleFS v2"));
    }
    if bsize != block_size || count == 0 || count > max_blocks {
        return Err(LfsError::Corrupt("block size or count"));
    }
    let mut used = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::from([[0u32, 1u32]]);
    while let Some(pair) = queue.pop_front() {
        if pair.contains(&NULL) {
            continue;
        }
        if pair[0] >= count || pair[1] >= count {
            return Err(LfsError::Corrupt("metadata pair beyond the file system"));
        }
        if !seen.insert((pair[0].min(pair[1]), pair[0].max(pair[1]))) {
            continue;
        }
        if seen.len() > count as usize {
            return Err(LfsError::Corrupt("metadata cycle"));
        }
        used.extend(pair);
        let dir = if pair == [0, 1] { root.clone() } else { fetch(read, pair, block_size)?.1 };
        for (t, d) in dir.entries.iter().flatten() {
            match *t {
                TYPE_CTZSTRUCT if d.len() >= 8 => ctz(read, le(d, 0), le(d, 4), block_size, count, &mut used)?,
                TYPE_DIRSTRUCT if d.len() >= 8 => queue.push_back([le(d, 0), le(d, 4)]),
                TYPE_CTZSTRUCT | TYPE_DIRSTRUCT => return Err(LfsError::Corrupt("short struct")),
                _ => {}
            }
        }
        if let Some(t) = dir.tail {
            queue.push_back(t);
        }
    }
    Ok(used)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_is_littlefs_s() {
        // lfs_crc(0xffffffff, "123456789") without the final XOR of CRC-32.
        assert_eq!(!crc(0xFFFF_FFFF, b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn ctz_index_matches_littlefs() {
        // Block 0 holds 4096 B (no pointer), block 1 4092 (one), block 2
        // 4088 (two).
        assert_eq!(ctz_index(4096, 0), 0);
        assert_eq!(ctz_index(4096, 4095), 0);
        assert_eq!(ctz_index(4096, 4096), 1);
        assert_eq!(ctz_index(4096, 4096 + 4091), 1);
        assert_eq!(ctz_index(4096, 4096 + 4092), 2);
    }
}
