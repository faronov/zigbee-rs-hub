//! `zdef`: zigbee-rs-hub device definitions (docs/device-support.md).
//!
//! `no_std` + `alloc`. One crate shared by the hub, the `zdefc` compiler and
//! (later) the zigbee-rs self-description build check:
//!
//! - [`model`]: the definition data model (spec 6);
//! - [`codec`]: canonical deterministic CBOR with integer keys (spec 8.1–8.4),
//!   [`keys`] and [`enums`] hold the v1 tables;
//! - [`hash`]: content hash (spec 8.6);
//! - [`expr`]: expression bytecode verifier and bounded VM (spec 7);
//! - [`validate`]: semantic checks and `requires` computation (spec 8.5);
//! - [`matching`]: fingerprints, match scoring, index keys (spec 3.2, 3.3, 13.3);
//! - [`resolve`]: layer precedence and merging (spec 3.3, 9.5);
//! - [`bank`]: catalog bank reader, SipHash index, inflate (spec 9.1);
//! - [`sign`]: COSE_Sign1 Ed25519 verification and the catalog manifest (spec 9.3).
//!
//! See `crates/zdef/README.md` for the API overview.

#![no_std]
#![forbid(unsafe_code)]
// Nested `if let` reads better than let-chains in the decoders.
#![allow(clippy::collapsible_if)]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

#[cfg(feature = "bank")]
pub mod bank;
pub mod cbor;
pub mod codec;
pub mod enums;
pub mod expr;
pub mod hash;
pub mod keys;
pub mod matching;
pub mod model;
pub mod num;
pub mod resolve;
pub mod sign;
pub mod siphash;
#[cfg(feature = "bank")]
pub mod webtools;

/// Insertion sort by key: the lists sorted on the hub are a few entries
/// long, and `core`'s sorts cost ~9 KiB of flash per element type.
pub(crate) fn sort_small_by_key<T, K: Ord>(v: &mut [T], key: impl Fn(&T) -> K) {
    for i in 1..v.len() {
        let mut j = i;
        while j > 0 && key(&v[j - 1]) > key(&v[j]) {
            v.swap(j - 1, j);
            j -= 1;
        }
    }
}
pub mod validate;

pub use codec::{DecodeError, DecodeErrorKind, EncodeError, decode, decode_canonical, encode};
pub use enums::{Feature, HubFeatures};
pub use hash::ContentHash;
pub use matching::{Fingerprint, GpFingerprint, Identity, Source};
pub use model::{Definition, Entity, Expr};

/// Decoding/encoding as traits (for generic hub code).
pub trait CborDecode: Sized {
    fn decode_cbor(bytes: &[u8]) -> Result<Self, DecodeError>;
}

pub trait CborEncode {
    fn encode_cbor(&self) -> Result<alloc::vec::Vec<u8>, EncodeError>;
}

impl CborDecode for Definition {
    fn decode_cbor(bytes: &[u8]) -> Result<Self, DecodeError> {
        codec::decode(bytes)
    }
}

impl CborEncode for Definition {
    fn encode_cbor(&self) -> Result<alloc::vec::Vec<u8>, EncodeError> {
        codec::encode(self)
    }
}

impl Definition {
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        codec::decode(bytes)
    }

    pub fn encode(&self) -> Result<alloc::vec::Vec<u8>, EncodeError> {
        codec::encode(self)
    }

    /// Content hash of the canonical standalone encoding.
    #[cfg(feature = "soft-sha256")]
    pub fn content_hash(&self) -> Result<ContentHash, EncodeError> {
        Ok(ContentHash::of(&self.encode()?))
    }

    pub fn validate(&self) -> Result<(), alloc::vec::Vec<validate::ValidationError>> {
        validate::validate(self)
    }

    /// Index of string constant `s`, appending it if absent.
    pub fn intern_str(&mut self, s: &str) -> usize {
        if let Some(i) = self
            .consts
            .iter()
            .position(|c| matches!(c, model::Const::Str(x) if x == s))
        {
            return i;
        }
        self.consts.push(model::Const::Str(s.into()));
        self.consts.len() - 1
    }
}

/// Decodes (strict deterministic CBOR, spec 8.1), validates (every
/// expression through the bytecode verifier) and checks `requires` against
/// the hub: what the hub runs on every definition load (spec 12.3 step 5).
///
/// Does not re-encode, so the encoder is not linked into a hub that only
/// loads; [`load_canonical`] additionally requires the canonical encoding.
pub fn load(bytes: &[u8], hub: &HubFeatures) -> Result<Definition, LoadError> {
    check_loaded(codec::decode(bytes).map_err(LoadError::Decode)?, hub)
}

/// [`load`] plus the re-encode check (input is the canonical encoding of
/// the result); used by `zdefc` and for user uploads where hash uniqueness matters.
pub fn load_canonical(bytes: &[u8], hub: &HubFeatures) -> Result<Definition, LoadError> {
    check_loaded(
        codec::decode_canonical(bytes).map_err(LoadError::Decode)?,
        hub,
    )
}

fn check_loaded(d: Definition, hub: &HubFeatures) -> Result<Definition, LoadError> {
    d.validate().map_err(LoadError::Invalid)?;
    if let Some(f) = hub.missing(&d.requires) {
        return Err(LoadError::Requires(f));
    }
    Ok(d)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadError {
    Decode(DecodeError),
    Invalid(alloc::vec::Vec<validate::ValidationError>),
    /// The hub lacks this feature ID.
    Requires(u16),
}

#[cfg(feature = "std")]
impl std::error::Error for DecodeError {}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "CBOR offset {}: {:?}", self.offset, self.kind)
    }
}

impl core::fmt::Display for LoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            LoadError::Decode(e) => write!(f, "{e}"),
            LoadError::Invalid(v) => {
                for e in v {
                    writeln!(f, "{}: {}", e.path, e.message)?;
                }
                Ok(())
            }
            LoadError::Requires(x) => match enums::Feature::from_code(*x as u64) {
                Some(n) => write!(f, "requires feature {x} ({})", n.as_str()),
                None => write!(f, "requires feature {x}"),
            },
        }
    }
}
