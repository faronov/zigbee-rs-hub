//! Content hashing (spec 8.6) behind a SHA-256 trait.
//!
//! The hub can hash with the ESP32-S3 SHA accelerator or mbedTLS by
//! implementing [`Sha256`]; the software implementation (`sha2`) is the
//! `soft-sha256` feature (on by default).

/// A SHA-256 implementation.
pub trait Sha256: Default {
    fn update(&mut self, data: &[u8]);
    fn finish(self) -> [u8; 32];
}

/// SHA-256 of `data` with implementation `H`.
pub fn sha256_with<H: Sha256>(data: &[u8]) -> [u8; 32] {
    let mut h = H::default();
    h.update(data);
    h.finish()
}

/// Software SHA-256 (`sha2`).
#[cfg(feature = "soft-sha256")]
#[derive(Default)]
pub struct SoftSha256(sha2::Sha256);

#[cfg(feature = "soft-sha256")]
impl Sha256 for SoftSha256 {
    fn update(&mut self, data: &[u8]) {
        sha2::Digest::update(&mut self.0, data);
    }
    fn finish(self) -> [u8; 32] {
        sha2::Digest::finalize(self.0).into()
    }
}

/// SHA-256 with the software implementation.
#[cfg(feature = "soft-sha256")]
pub fn sha256(data: &[u8]) -> [u8; 32] {
    sha256_with::<SoftSha256>(data)
}

/// SHA-256 of the standalone canonical CBOR of a definition.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContentHash(pub [u8; 32]);

impl ContentHash {
    #[cfg(feature = "soft-sha256")]
    pub fn of(standalone_cbor: &[u8]) -> Self {
        ContentHash(sha256(standalone_cbor))
    }

    pub fn of_with<H: Sha256>(standalone_cbor: &[u8]) -> Self {
        ContentHash(sha256_with::<H>(standalone_cbor))
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
