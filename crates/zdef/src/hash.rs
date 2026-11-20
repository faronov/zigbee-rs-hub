//! Content hashing (spec 8.6).

use sha2::{Digest, Sha256};

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

/// SHA-256 of the standalone canonical CBOR of a definition.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContentHash(pub [u8; 32]);

impl ContentHash {
    pub fn of(standalone_cbor: &[u8]) -> Self {
        ContentHash(sha256(standalone_cbor))
    }

    /// First 8 bytes, the UI short form (16 hex digits).
    pub fn short(&self) -> [u8; 8] {
        let mut o = [0; 8];
        o.copy_from_slice(&self.0[..8]);
        o
    }
}

impl core::fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(self, f)
    }
}

impl core::fmt::Display for ContentHash {
    /// Full hex; `{:#}` prints the 8-byte short form.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let n = if f.alternate() { 8 } else { 32 };
        for b in &self.0[..n] {
            write!(f, "{:02x}", b)?;
        }
        Ok(())
    }
}
