//! Local definitions in `hubdata` (device-support 9.2): definitions fetched
//! on demand (9.3), stored as standalone CBOR with the index-key hashes
//! they were found under, and read in place.
//!
//! The area is two halves written alternately. A half starts with a
//! 16-byte header `"ZLD1" | u32 generation | u32 crc32c(magic, generation)
//! | 0xFFFFFFFF`; the valid half with the higher generation is live.
//! Records follow, 4-byte aligned:
//!
//! ```text
//! u16 length (whole record) | u8 key count | u8 kind (1 = definition)
//! u32 crc32c(first 4 bytes, everything after the crc) | n x u64 key hashes
//! | standalone CBOR | 0xFF padding to 4
//! ```
//!
//! Erased bytes (`length` 0xFFFF) end the log. A record that fails its CRC
//! (power lost while appending) ends it too, and the next append first
//! compacts into the other half: the kept records are copied, then the new
//! header is written last, so a power cut during compaction leaves the old
//! half live. Fetched definitions are a cache: what does not fit after
//! dropping the unused ones is refused (`no_space`), never written over
//! data.

use std::io;

use hub_devices::bankcat::{Fetched, FetchedCatalog};
use hub_store::crc::crc32c;

use super::data::{DataFlash, SECTOR};

const MAGIC: &[u8; 4] = b"ZLD1";
const HEADER: u32 = 16;
const KIND_DEF: u8 = 1;

fn align4(n: u32) -> u32 {
    (n + 3) & !3
}

fn header(generation: u32) -> [u8; 16] {
    let mut h = [0xFFu8; 16];
    h[..4].copy_from_slice(MAGIC);
    h[4..8].copy_from_slice(&generation.to_le_bytes());
    let c = crc32c(&h[..8]);
    h[8..12].copy_from_slice(&c.to_le_bytes());
    h
}

fn parse_header(b: &[u8]) -> Option<u32> {
    (b.len() >= 16 && &b[..4] == MAGIC && crc32c(&b[..8]).to_le_bytes() == b[8..12])
        .then(|| u32::from_le_bytes(b[4..8].try_into().unwrap()))
}

fn record(keys: &[u64], cbor: &[u8]) -> io::Result<Vec<u8>> {
    let len = 8 + keys.len() * 8 + cbor.len();
    if keys.is_empty() || keys.len() > 255 || len > 0xFFF0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "local definition record size"));
    }
    let mut r = Vec::with_capacity(align4(len as u32) as usize);
    r.extend_from_slice(&(len as u16).to_le_bytes());
    r.push(keys.len() as u8);
    r.push(KIND_DEF);
    r.extend_from_slice(&[0; 4]);
    for k in keys {
        r.extend_from_slice(&k.to_le_bytes());
    }
    r.extend_from_slice(cbor);
    let mut c = r[..4].to_vec();
    c.extend_from_slice(&r[8..]);
    r[4..8].copy_from_slice(&crc32c(&c).to_le_bytes());
    r.resize(align4(len as u32) as usize, 0xFF);
    Ok(r)
}

/// Records of a half: `(keys, cbor, end offset)`; `clean` is false when a
/// torn record ended the log.
struct Scan {
    defs: Vec<Fetched>,
    end: u32,
    clean: bool,
}

#[inline(never)]
fn scan(half: &'static [u8]) -> Scan {
    let mut at = HEADER as usize;
    let mut defs = Vec::new();
    loop {
        if at + 8 > half.len() {
            return Scan { defs, end: at as u32, clean: true };
        }
        let len = u16::from_le_bytes([half[at], half[at + 1]]) as usize;
        if len == 0xFFFF {
            // Erased: the end, if everything up to the next 8 bytes is too.
            let clean = half[at..at + 8].iter().all(|b| *b == 0xFF);
            return Scan { defs, end: at as u32, clean };
        }
        let n = half[at + 2] as usize;
        if len < 8 + n * 8 || at + len > half.len() || n == 0 {
            return Scan { defs, end: at as u32, clean: false };
        }
        let r = &half[at..at + len];
        let mut c = r[..4].to_vec();
        c.extend_from_slice(&r[8..]);
        if crc32c(&c).to_le_bytes() != r[4..8] {
            return Scan { defs, end: at as u32, clean: false };
        }
        if r[3] == KIND_DEF {
            let keys = (0..n).map(|i| u64::from_le_bytes(r[8 + i * 8..16 + i * 8].try_into().unwrap())).collect();
            defs.push(Fetched { keys, cbor: &r[8 + n * 8..] });
        }
        at += align4(len as u32) as usize;
    }
}

/// The local definitions area.
pub struct LocalDefs {
    offset: u32,
    half: u32,
    live: u8,
    generation: u32,
    end: u32,
    clean: bool,
    defs: Vec<Fetched>,
}

impl LocalDefs {
    /// Opens the area `[offset, offset + size)` (`size`: two halves of
    /// whole sectors). An area without a valid half is formatted.
    #[inline(never)]
    pub fn open(f: &mut dyn DataFlash, offset: u32, size: u32) -> io::Result<Self> {
        let half = (size / 2) / SECTOR * SECTOR;
        if half < 2 * SECTOR {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "local area too small"));
        }
        let gens: Vec<Option<u32>> = (0..2).map(|i| parse_header(f.map(offset + i * half, HEADER))).collect();
        let live = match (gens[0], gens[1]) {
            (Some(a), Some(b)) => u8::from(b > a),
            (Some(_), None) => 0,
            (None, Some(_)) => 1,
            (None, None) => {
                f.erase(offset, half)?;
                f.write(offset, &header(1))?;
                0
            }
        };
        let generation = gens[live as usize].unwrap_or(1);
        let s = scan(f.map(offset + live as u32 * half, half));
        Ok(Self { offset, half, live, generation, end: s.end, clean: s.clean, defs: s.defs })
    }

    /// The live records as a catalog (borrowing the mapped area).
    pub fn catalog(&self) -> FetchedCatalog {
        FetchedCatalog { defs: self.defs.clone() }
    }

    /// Records.
    pub fn defs(&self) -> &[Fetched] {
        &self.defs
    }

    /// Bytes used in the live half, and its size.
    pub fn usage(&self) -> (u32, u32) {
        (self.end, self.half)
    }

    fn base(&self, half: u8) -> u32 {
        self.offset + half as u32 * self.half
    }

    /// Copies the records `keep` accepts into the other half and makes it
    /// live.
    #[inline(never)]
    pub fn compact(&mut self, f: &mut dyn DataFlash, keep: &dyn Fn(&Fetched) -> bool) -> io::Result<()> {
        let to = 1 - self.live;
        let base = self.base(to);
        f.erase(base, self.half)?;
        let mut at = HEADER;
        for d in self.defs.iter().filter(|d| keep(d)) {
            let r = record(&d.keys, d.cbor)?;
            f.write(base + at, &r)?;
            at += r.len() as u32;
        }
        let generation = self.generation.wrapping_add(1).max(1);
        f.write(base, &header(generation))?;
        self.live = to;
        self.generation = generation;
        let s = scan(f.map(base, self.half));
        self.end = s.end;
        self.clean = s.clean;
        self.defs = s.defs;
        Ok(())
    }

    /// Appends a definition. Compacts first after a torn record, or when it
    /// does not fit (dropping what `keep` refuses); `no_space` (OutOfMemory)
    /// if it still does not fit.
    #[inline(never)]
    pub fn append(
        &mut self,
        f: &mut dyn DataFlash,
        keys: &[u64],
        cbor: &[u8],
        keep: &dyn Fn(&Fetched) -> bool,
    ) -> io::Result<()> {
        let r = record(keys, cbor)?;
        if !self.clean || self.end + r.len() as u32 > self.half {
            self.compact(f, keep)?;
        }
        if self.end + r.len() as u32 > self.half {
            return Err(io::Error::new(io::ErrorKind::OutOfMemory, "local definitions area full"));
        }
        let at = self.base(self.live) + self.end;
        f.write(at, &r)?;
        let s = scan(f.map(self.base(self.live), self.half));
        self.end = s.end;
        self.clean = s.clean;
        self.defs = s.defs;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::data::MemData;
    use super::*;

    const OFF: u32 = 8192;
    const SIZE: u32 = 4 * SECTOR;

    fn keep_all(_: &Fetched) -> bool {
        true
    }

    #[test]
    fn append_reopen_compact_and_torn_records() {
        let mut f = MemData::new(OFF + SIZE);
        let mut l = LocalDefs::open(&mut f, OFF, SIZE).unwrap();
        assert!(l.defs().is_empty());
        l.append(&mut f, &[1, 2], b"\xa1\x01\x61x", &keep_all).unwrap();
        l.append(&mut f, &[3], &[0x42; 1000], &keep_all).unwrap();
        let l2 = LocalDefs::open(&mut f, OFF, SIZE).unwrap();
        assert_eq!(l2.defs().len(), 2);
        assert_eq!(l2.defs()[0].keys, [1, 2]);
        assert_eq!(l2.defs()[1].cbor, &[0x42; 1000][..]);

        // A torn append (power cut after the length field): the log ends
        // before it, and the next append compacts into the other half.
        let end = l.usage().0;
        f.write(OFF + end, &[0x20, 0x00, 0x01, 0x01, 0x00]).unwrap();
        let mut l = LocalDefs::open(&mut f, OFF, SIZE).unwrap();
        assert_eq!(l.defs().len(), 2);
        assert!(!l.clean);
        l.append(&mut f, &[4], b"abc", &keep_all).unwrap();
        assert_eq!(l.live, 1);
        let l = LocalDefs::open(&mut f, OFF, SIZE).unwrap();
        assert_eq!((l.live, l.defs().len()), (1, 3));

        // Full: unused records are dropped; too large is refused.
        let mut l = l;
        let big = vec![7u8; 7200];
        l.append(&mut f, &[5], &big, &|d| d.keys != [3]).unwrap();
        assert!(l.defs().iter().all(|d| d.keys != [3]), "dropped when compacting");
        let err = l.append(&mut f, &[6], &vec![1u8; 9000], &keep_all).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::OutOfMemory);
        let l = LocalDefs::open(&mut f, OFF, SIZE).unwrap();
        assert!(l.defs().iter().any(|d| d.keys == [5]));
    }

    #[test]
    fn compaction_interrupted_before_its_header_keeps_the_old_half() {
        let mut f = MemData::new(OFF + SIZE);
        let mut l = LocalDefs::open(&mut f, OFF, SIZE).unwrap();
        l.append(&mut f, &[9], b"old", &keep_all).unwrap();
        // The other half got records but no header (power cut).
        let other = OFF + SIZE / 2;
        f.erase(other, SIZE / 2).unwrap();
        f.write(other + HEADER, &record(&[8], b"half-done").unwrap()).unwrap();
        let l = LocalDefs::open(&mut f, OFF, SIZE).unwrap();
        assert_eq!(l.live, 0);
        assert_eq!(l.defs()[0].cbor, b"old");
    }
}
