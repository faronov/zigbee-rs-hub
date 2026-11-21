//! Web tools bundle (`.zwb`): files the hub serves to its web UI from
//! `hubdata`, e.g. the definition compiler (`zdefc` WebAssembly and its
//! JavaScript glue). Every file is stored gzip-compressed and served as is
//! (`Content-Encoding: gzip`).
//!
//! ```text
//! 0  "ZWB1"
//! 4  u32 total length (header included)
//! 8  [32] SHA-256 of bytes 40..total
//! 40 u16 file count
//! 42 per file: u8 name length, name, u8 type length, content type,
//!              u32 offset (from the start), u32 length
//! .. gzip data
//! ```
//!
//! A hub writes everything after the first 40 bytes first and the header
//! last, so a bundle cut short by a power loss has no valid header.

use alloc::string::String;
use alloc::vec::Vec;

pub const MAGIC: &[u8; 4] = b"ZWB1";
pub const HEADER_LEN: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BundleError {
    Truncated,
    Magic,
    Layout,
    Hash,
}

/// One file of a bundle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
    pub name: &'a str,
    pub content_type: &'a str,
    /// Gzip bytes.
    pub data: &'a [u8],
}

/// A parsed bundle (borrowing its bytes).
#[derive(Clone, Copy, Debug)]
pub struct Bundle<'a> {
    bytes: &'a [u8],
    count: u16,
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

impl<'a> Bundle<'a> {
    /// Parses the layout (no hash check, see [`Bundle::verify`]); `bytes`
    /// may extend beyond the bundle.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, BundleError> {
        if bytes.len() < HEADER_LEN + 2 {
            return Err(BundleError::Truncated);
        }
        if &bytes[..4] != MAGIC {
            return Err(BundleError::Magic);
        }
        let total = le32(bytes, 4) as usize;
        if total > bytes.len() || total < HEADER_LEN + 2 {
            return Err(BundleError::Truncated);
        }
        let bytes = &bytes[..total];
        let b = Bundle {
            bytes,
            count: u16::from_le_bytes([bytes[40], bytes[41]]),
        };
        let mut n = 0;
        for e in b.iter() {
            e?;
            n += 1;
        }
        if n != b.count {
            return Err(BundleError::Layout);
        }
        Ok(b)
    }

    /// Bundle length.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The stored SHA-256 of bytes 40.. (the bundle's identity).
    pub fn digest(&self) -> [u8; 32] {
        let mut d = [0u8; 32];
        d.copy_from_slice(&self.bytes[8..40]);
        d
    }

    /// Checks the stored SHA-256.
    pub fn verify_with<H: crate::hash::Sha256>(&self) -> Result<(), BundleError> {
        if crate::hash::sha256_with::<H>(&self.bytes[HEADER_LEN..]) != self.digest() {
            return Err(BundleError::Hash);
        }
        Ok(())
    }

    /// The files.
    pub fn iter(&self) -> impl Iterator<Item = Result<Entry<'a>, BundleError>> + 'a {
        let bytes = self.bytes;
        let mut at = HEADER_LEN + 2;
        (0..self.count).map(move |_| {
            let field = |at: &mut usize| -> Result<&'a str, BundleError> {
                let n = *bytes.get(*at).ok_or(BundleError::Layout)? as usize;
                let s = bytes.get(*at + 1..*at + 1 + n).ok_or(BundleError::Layout)?;
                *at += 1 + n;
                core::str::from_utf8(s).map_err(|_| BundleError::Layout)
            };
            let name = field(&mut at)?;
            let content_type = field(&mut at)?;
            if at + 8 > bytes.len() {
                return Err(BundleError::Layout);
            }
            let (o, l) = (le32(bytes, at) as usize, le32(bytes, at + 4) as usize);
            at += 8;
            let data = bytes.get(o..o + l).ok_or(BundleError::Layout)?;
            Ok(Entry {
                name,
                content_type,
                data,
            })
        })
    }

    /// A file by name.
    pub fn find(&self, name: &str) -> Option<Entry<'a>> {
        self.iter().flatten().find(|e| e.name == name)
    }
}

/// Builds a bundle from `(name, content type, gzip bytes)`.
pub fn build(files: &[(String, String, Vec<u8>)]) -> Result<Vec<u8>, BundleError> {
    let mut dir = Vec::new();
    for (n, t, _) in files {
        if n.len() > 255 || t.len() > 255 {
            return Err(BundleError::Layout);
        }
        dir.push(n.len() as u8);
        dir.extend_from_slice(n.as_bytes());
        dir.push(t.len() as u8);
        dir.extend_from_slice(t.as_bytes());
        dir.extend_from_slice(&[0; 8]);
    }
    let data_start = HEADER_LEN + 2 + dir.len();
    let mut out = Vec::with_capacity(data_start);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&[0; 36]);
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    let mut at = data_start;
    let mut d = 0;
    for (n, t, data) in files {
        d += 2 + n.len() + t.len();
        dir[d..d + 4].copy_from_slice(&(at as u32).to_le_bytes());
        dir[d + 4..d + 8].copy_from_slice(&(data.len() as u32).to_le_bytes());
        d += 8;
        at += data.len();
    }
    out.extend_from_slice(&dir);
    for (_, _, data) in files {
        out.extend_from_slice(data);
    }
    let total = out.len() as u32;
    out[4..8].copy_from_slice(&total.to_le_bytes());
    #[cfg(feature = "soft-sha256")]
    {
        let h = crate::hash::sha256(&out[HEADER_LEN..]);
        out[8..40].copy_from_slice(&h);
    }
    Ok(out)
}
