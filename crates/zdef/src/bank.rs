//! Catalog banks (spec 9.1).
//!
//! ```text
//! Header (64 B, little-endian):
//!   0  magic "ZDB1"          4  format u16 (1)      6  signature length u16
//!   8  generation u32       12  catalog date u32 (YYYYMMDD)
//!  16  catalog serial u16   18  blocks u16         20  definitions u16
//!  22  strings u16          24  index keys u32     28  string section length u32
//!  32  SHA-256 of the body (everything after the signature)
//! Signature: COSE_Sign1, EdDSA (alg -8), detached payload = the 64-byte header
//! Body:
//!   string section: u32 inflated length + raw deflate of a CBOR array of text strings
//!   index: keys x 12 B {u64 key hash, u16 block, u8 slot, u8 flags}, sorted by (hash, block, slot)
//!   block table: (blocks + 1) x u32 offsets into the block area (last = its length)
//!   block area: raw deflate blocks, each <= 4 KiB inflated:
//!     sequence of {u16 length, definition in catalog form}
//! ```

use crate::codec::{self, DecodeError, DecodeOptions, StringTable};
use crate::model::Definition;
use crate::sign::{self, SignError, TrustedKey};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

pub const MAGIC: &[u8; 4] = b"ZDB1";
pub const FORMAT: u16 = 1;
pub const HEADER_LEN: usize = 64;
/// Largest inflated block.
pub const BLOCK_MAX: usize = 4096;
pub const INDEX_ENTRY_LEN: usize = 12;
/// Fixed public SipHash key of the index (spec 9.1).
pub const INDEX_KEY: [u8; 16] = *b"zdef-index-v1\0\0\0";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BankError {
    Truncated,
    Magic,
    Format(u16),
    Layout,
    BodyHash,
    Signature(SignError),
    Inflate,
    BlockRange,
    Slot,
    Strings,
    Decode(DecodeError),
}

impl From<DecodeError> for BankError {
    fn from(e: DecodeError) -> Self {
        BankError::Decode(e)
    }
}

impl From<SignError> for BankError {
    fn from(e: SignError) -> Self {
        BankError::Signature(e)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Header {
    pub format: u16,
    pub sig_len: u16,
    pub generation: u32,
    pub catalog_date: u32,
    pub catalog_serial: u16,
    pub blocks: u16,
    pub definitions: u16,
    pub strings: u16,
    pub keys: u32,
    pub strings_len: u32,
    pub body_sha256: [u8; 32],
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

impl Header {
    pub fn parse(b: &[u8]) -> Result<Header, BankError> {
        if b.len() < HEADER_LEN {
            return Err(BankError::Truncated);
        }
        if &b[..4] != MAGIC {
            return Err(BankError::Magic);
        }
        let format = le16(b, 4);
        if format != FORMAT {
            return Err(BankError::Format(format));
        }
        let mut body_sha256 = [0u8; 32];
        body_sha256.copy_from_slice(&b[32..64]);
        Ok(Header {
            format,
            sig_len: le16(b, 6),
            generation: le32(b, 8),
            catalog_date: le32(b, 12),
            catalog_serial: le16(b, 16),
            blocks: le16(b, 18),
            definitions: le16(b, 20),
            strings: le16(b, 22),
            keys: le32(b, 24),
            strings_len: le32(b, 28),
            body_sha256,
        })
    }

    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[..4].copy_from_slice(MAGIC);
        b[4..6].copy_from_slice(&self.format.to_le_bytes());
        b[6..8].copy_from_slice(&self.sig_len.to_le_bytes());
        b[8..12].copy_from_slice(&self.generation.to_le_bytes());
        b[12..16].copy_from_slice(&self.catalog_date.to_le_bytes());
        b[16..18].copy_from_slice(&self.catalog_serial.to_le_bytes());
        b[18..20].copy_from_slice(&self.blocks.to_le_bytes());
        b[20..22].copy_from_slice(&self.definitions.to_le_bytes());
        b[22..24].copy_from_slice(&self.strings.to_le_bytes());
        b[24..28].copy_from_slice(&self.keys.to_le_bytes());
        b[28..32].copy_from_slice(&self.strings_len.to_le_bytes());
        b[32..64].copy_from_slice(&self.body_sha256);
        b
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct IndexEntry {
    pub key_hash: u64,
    pub block: u16,
    pub slot: u8,
    pub flags: u8,
}

impl IndexEntry {
    pub fn parse(b: &[u8]) -> IndexEntry {
        let mut h = [0u8; 8];
        h.copy_from_slice(&b[..8]);
        IndexEntry {
            key_hash: u64::from_le_bytes(h),
            block: le16(b, 8),
            slot: b[10],
            flags: b[11],
        }
    }

    pub fn to_bytes(&self) -> [u8; INDEX_ENTRY_LEN] {
        let mut b = [0u8; INDEX_ENTRY_LEN];
        b[..8].copy_from_slice(&self.key_hash.to_le_bytes());
        b[8..10].copy_from_slice(&self.block.to_le_bytes());
        b[10] = self.slot;
        b[11] = self.flags;
        b
    }
}

/// Raw-deflate decoder. The default is `miniz_oxide`; the S3 hub can wrap
/// the ROM `tinfl` (same algorithm, non-wrapping output buffer).
pub trait Inflate {
    /// Inflates all of `input` into `out`; returns the output length.
    fn inflate(&mut self, input: &[u8], out: &mut [u8]) -> Result<usize, BankError>;
}

/// `miniz_oxide` inflater (~11 KiB of state, boxed).
pub struct MinizInflate {
    state: Box<miniz_oxide::inflate::core::DecompressorOxide>,
}

impl Default for MinizInflate {
    fn default() -> Self {
        Self::new()
    }
}

impl MinizInflate {
    pub fn new() -> Self {
        MinizInflate {
            state: Box::default(),
        }
    }
}

impl Inflate for MinizInflate {
    fn inflate(&mut self, input: &[u8], out: &mut [u8]) -> Result<usize, BankError> {
        use miniz_oxide::inflate::TINFLStatus;
        use miniz_oxide::inflate::core::{
            decompress, inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
        };
        self.state.init();
        let (status, used, written) = decompress(
            &mut self.state,
            input,
            out,
            0,
            TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
        );
        if status != TINFLStatus::Done || used != input.len() {
            return Err(BankError::Inflate);
        }
        Ok(written)
    }
}

/// Inflated bank string table (catalog form, spec 8.4).
pub struct BankStrings {
    data: Vec<u8>,
    spans: Vec<(u32, u32)>,
}

impl BankStrings {
    /// Parses an inflated CBOR array of text strings.
    pub fn from_cbor(data: Vec<u8>) -> Result<Self, BankError> {
        let mut r = crate::cbor::Reader::new(&data);
        let n = r.array().map_err(|_| BankError::Strings)?;
        let mut spans = Vec::with_capacity(n);
        for _ in 0..n {
            let before = r.position();
            let s = r.text().map_err(|_| BankError::Strings)?;
            let start = r.position() - s.len();
            let _ = before;
            spans.push((start as u32, s.len() as u32));
        }
        r.finish().map_err(|_| BankError::Strings)?;
        Ok(BankStrings { data, spans })
    }

    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Bytes held (inflated table + spans).
    pub fn ram_bytes(&self) -> usize {
        self.data.capacity() + self.spans.capacity() * 8
    }
}

impl StringTable for BankStrings {
    fn get(&self, i: u64) -> Option<&str> {
        let &(o, n) = self.spans.get(i as usize)?;
        core::str::from_utf8(&self.data[o as usize..(o + n) as usize]).ok()
    }
}

/// A bank held in memory (memory-mapped flash, PSRAM or a host file).
pub struct Bank<'a> {
    pub header: Header,
    header_bytes: &'a [u8],
    signature: &'a [u8],
    body: &'a [u8],
    strings: &'a [u8],
    index: &'a [u8],
    block_table: &'a [u8],
    blocks: &'a [u8],
}

impl<'a> Bank<'a> {
    /// Parses the layout (no hash or signature check; see [`Bank::verify`]).
    pub fn parse(bytes: &'a [u8]) -> Result<Self, BankError> {
        let header = Header::parse(bytes)?;
        let sig_end = HEADER_LEN + header.sig_len as usize;
        if bytes.len() < sig_end {
            return Err(BankError::Truncated);
        }
        let body = &bytes[sig_end..];
        let sl = header.strings_len as usize;
        let il = header.keys as usize * INDEX_ENTRY_LEN;
        let tl = (header.blocks as usize + 1) * 4;
        if body.len() < sl + il + tl || sl < 4 {
            return Err(BankError::Layout);
        }
        let strings = &body[..sl];
        let index = &body[sl..sl + il];
        let block_table = &body[sl + il..sl + il + tl];
        let blocks = &body[sl + il + tl..];
        let total = le32(block_table, header.blocks as usize * 4) as usize;
        if total != blocks.len() {
            return Err(BankError::Layout);
        }
        let mut prev = 0;
        for i in 0..=header.blocks as usize {
            let o = le32(block_table, i * 4) as usize;
            if o < prev || o > total {
                return Err(BankError::Layout);
            }
            prev = o;
        }
        Ok(Bank {
            header,
            header_bytes: &bytes[..HEADER_LEN],
            signature: &bytes[HEADER_LEN..sig_end],
            body,
            strings,
            index,
            block_table,
            blocks,
        })
    }

    /// Checks the body hash and the header signature against trusted keys.
    pub fn verify(&self, keys: &[TrustedKey]) -> Result<(), BankError> {
        if crate::hash::sha256(self.body) != self.header.body_sha256 {
            return Err(BankError::BodyHash);
        }
        sign::verify_cose_sign1(self.signature, Some(self.header_bytes), keys)?;
        Ok(())
    }

    pub fn index_len(&self) -> usize {
        self.header.keys as usize
    }

    pub fn entry(&self, i: usize) -> IndexEntry {
        IndexEntry::parse(&self.index[i * INDEX_ENTRY_LEN..(i + 1) * INDEX_ENTRY_LEN])
    }

    /// Index entries with `key_hash` (binary search; several on collisions
    /// or when several definitions share a key).
    pub fn lookup(&self, key_hash: u64) -> impl Iterator<Item = IndexEntry> + '_ {
        let n = self.index_len();
        let (mut lo, mut hi) = (0, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.entry(mid).key_hash < key_hash {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        (lo..n)
            .map(move |i| self.entry(i))
            .take_while(move |e| e.key_hash == key_hash)
    }

    /// Compressed bytes of block `n`.
    pub fn block(&self, n: u16) -> Result<&'a [u8], BankError> {
        if n >= self.header.blocks {
            return Err(BankError::BlockRange);
        }
        let a = le32(self.block_table, n as usize * 4) as usize;
        let b = le32(self.block_table, n as usize * 4 + 4) as usize;
        Ok(&self.blocks[a..b])
    }

    /// Inflates the string table.
    pub fn load_strings(&self, inf: &mut dyn Inflate) -> Result<BankStrings, BankError> {
        let raw_len = le32(self.strings, 0) as usize;
        if raw_len > 1 << 20 {
            return Err(BankError::Strings);
        }
        let mut data = alloc::vec![0u8; raw_len];
        let n = inf.inflate(&self.strings[4..], &mut data)?;
        if n != raw_len {
            return Err(BankError::Strings);
        }
        let t = BankStrings::from_cbor(data)?;
        if t.len() != self.header.strings as usize {
            return Err(BankError::Strings);
        }
        Ok(t)
    }

    /// Inflates block `n` into `buf` and returns the inflated length.
    pub fn inflate_block(
        &self,
        inf: &mut dyn Inflate,
        n: u16,
        buf: &mut [u8; BLOCK_MAX],
    ) -> Result<usize, BankError> {
        inf.inflate(self.block(n)?, buf)
    }

    /// Loads and decodes one definition (catalog form) by index entry.
    pub fn load(
        &self,
        inf: &mut dyn Inflate,
        strings: &BankStrings,
        e: &IndexEntry,
        buf: &mut [u8; BLOCK_MAX],
    ) -> Result<Definition, BankError> {
        let n = self.inflate_block(inf, e.block, buf)?;
        let item = block_slot(&buf[..n], e.slot)?;
        Ok(codec::decode_with(
            item,
            &DecodeOptions {
                strings: Some(strings),
            },
        )?)
    }

    /// Bytes of the body (for re-hashing by a streaming writer).
    pub fn body(&self) -> &'a [u8] {
        self.body
    }
}

/// Slot `slot` of an inflated block.
pub fn block_slot(block: &[u8], slot: u8) -> Result<&[u8], BankError> {
    let mut o = 0usize;
    let mut i = 0u8;
    while o + 2 <= block.len() {
        let len = le16(block, o) as usize;
        let start = o + 2;
        if start + len > block.len() {
            return Err(BankError::Slot);
        }
        if i == slot {
            return Ok(&block[start..start + len]);
        }
        o = start + len;
        i = i.checked_add(1).ok_or(BankError::Slot)?;
    }
    Err(BankError::Slot)
}

/// Number of definitions in an inflated block.
pub fn block_slots(block: &[u8]) -> Result<usize, BankError> {
    let mut o = 0usize;
    let mut n = 0;
    while o < block.len() {
        if o + 2 > block.len() {
            return Err(BankError::Slot);
        }
        o += 2 + le16(block, o) as usize;
        n += 1;
    }
    if o != block.len() {
        return Err(BankError::Slot);
    }
    Ok(n)
}

/// SipHash-2-4 (Aumasson & Bernstein), 64-bit output.
pub fn siphash24(key: &[u8; 16], data: &[u8]) -> u64 {
    let k0 = u64::from_le_bytes(key[..8].try_into().unwrap());
    let k1 = u64::from_le_bytes(key[8..].try_into().unwrap());
    let mut v0 = 0x736f6d6570736575u64 ^ k0;
    let mut v1 = 0x646f72616e646f6du64 ^ k1;
    let mut v2 = 0x6c7967656e657261u64 ^ k0;
    let mut v3 = 0x7465646279746573u64 ^ k1;
    macro_rules! round {
        () => {
            v0 = v0.wrapping_add(v1);
            v1 = v1.rotate_left(13);
            v1 ^= v0;
            v0 = v0.rotate_left(32);
            v2 = v2.wrapping_add(v3);
            v3 = v3.rotate_left(16);
            v3 ^= v2;
            v0 = v0.wrapping_add(v3);
            v3 = v3.rotate_left(21);
            v3 ^= v0;
            v2 = v2.wrapping_add(v1);
            v1 = v1.rotate_left(17);
            v1 ^= v2;
            v2 = v2.rotate_left(32);
        };
    }
    let (chunks, rem) = data.as_chunks::<8>();
    for c in chunks {
        let m = u64::from_le_bytes(*c);
        v3 ^= m;
        round!();
        round!();
        v0 ^= m;
    }
    let mut last = (data.len() as u64 & 0xff) << 56;
    for (i, b) in rem.iter().enumerate() {
        last |= (*b as u64) << (8 * i);
    }
    v3 ^= last;
    round!();
    round!();
    v0 ^= last;
    v2 ^= 0xff;
    round!();
    round!();
    round!();
    round!();
    v0 ^ v1 ^ v2 ^ v3
}

/// Shard number (first byte of the key hash, big-endian view) for on-demand
/// fetch (spec 9.3).
pub fn shard_of(key_hash: u64) -> u8 {
    (key_hash >> 56) as u8
}

/// One record of `shard-{hh}.cbor`: `[key hash, requires, standalone definition]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShardRecord {
    pub key_hash: u64,
    pub requires: Vec<u16>,
    pub definition: Vec<u8>,
}

pub fn decode_shard(bytes: &[u8]) -> Result<Vec<ShardRecord>, DecodeError> {
    let mut r = crate::cbor::Reader::new(bytes);
    let n = r.array()?;
    let mut out = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        if r.array()? != 3 {
            return Err(DecodeError {
                offset: r.position(),
                kind: codec::DecodeErrorKind::Invalid("shard record"),
            });
        }
        let key_hash = r.uint()?;
        let k = r.array()?;
        let mut requires = Vec::with_capacity(k);
        for _ in 0..k {
            requires.push(r.uint()? as u16);
        }
        out.push(ShardRecord {
            key_hash,
            requires,
            definition: r.bytes()?.to_vec(),
        });
    }
    r.finish()?;
    Ok(out)
}

pub fn encode_shard(records: &[ShardRecord]) -> Vec<u8> {
    let mut w = crate::cbor::Writer::new();
    w.array(records.len());
    for rec in records {
        w.array(3);
        w.uint(rec.key_hash);
        w.array(rec.requires.len());
        for f in &rec.requires {
            w.uint(*f as u64);
        }
        w.bytes(&rec.definition);
    }
    w.into_vec()
}

/// Display helper for bank sizes in diagnostics.
pub fn describe(h: &Header) -> String {
    alloc::format!(
        "generation {} catalog {}.{} defs {} keys {} blocks {} strings {}",
        h.generation,
        h.catalog_date,
        h.catalog_serial,
        h.definitions,
        h.keys,
        h.blocks,
        h.strings
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn siphash_reference_vector() {
        // Reference vector from the SipHash paper (key 00..0f, message 00..0e).
        let key: [u8; 16] = core::array::from_fn(|i| i as u8);
        let msg: alloc::vec::Vec<u8> = (0u8..15).collect();
        assert_eq!(siphash24(&key, &msg), 0xa129ca6149be45e5);
        assert_eq!(siphash24(&key, &[]), 0x726fdb47dd0e0e31);
    }
}
