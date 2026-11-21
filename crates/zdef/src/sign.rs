//! COSE_Sign1 with Ed25519 (RFC 9052, alg -8) and the catalog manifest (spec 9.3).

use crate::cbor::{Head, Reader, Writer};
use crate::codec::{DecodeError, DecodeErrorKind};
use alloc::string::String;
use alloc::vec::Vec;

/// COSE algorithm EdDSA.
pub const ALG_EDDSA: i64 = -8;
/// CBOR tag of COSE_Sign1.
pub const TAG_SIGN1: u64 = 18;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignError {
    Malformed,
    Algorithm,
    /// No trusted key with this key ID.
    UnknownKey([u8; 8]),
    BadSignature,
    /// Detached signature without a payload, or a payload where detached was expected.
    Payload,
}

/// A trusted Ed25519 public key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrustedKey {
    pub kid: [u8; 8],
    pub public: [u8; 32],
}

impl TrustedKey {
    #[cfg(feature = "soft-sha256")]
    pub fn new(public: [u8; 32]) -> Self {
        TrustedKey {
            kid: key_id(&public),
            public,
        }
    }

    pub fn new_with<H: crate::hash::Sha256>(public: [u8; 32]) -> Self {
        TrustedKey {
            kid: key_id_with::<H>(&public),
            public,
        }
    }
}

/// Key ID: first 8 bytes of SHA-256 of the public key.
#[cfg(feature = "soft-sha256")]
pub fn key_id(public: &[u8; 32]) -> [u8; 8] {
    key_id_with::<crate::hash::SoftSha256>(public)
}

pub fn key_id_with<H: crate::hash::Sha256>(public: &[u8; 32]) -> [u8; 8] {
    let h = crate::hash::sha256_with::<H>(public);
    let mut k = [0; 8];
    k.copy_from_slice(&h[..8]);
    k
}

/// Protected header `{1: -8}` as a byte string's content.
pub fn protected_header() -> Vec<u8> {
    let mut w = Writer::new();
    w.map(1);
    w.uint(1);
    w.int(ALG_EDDSA);
    w.into_vec()
}

/// `Sig_structure` = ["Signature1", protected, h'', payload] (the signed bytes).
pub fn to_be_signed(protected: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut w = Writer::new();
    w.array(4);
    w.text("Signature1");
    w.bytes(protected);
    w.bytes(&[]);
    w.bytes(payload);
    w.into_vec()
}

/// Assembles a COSE_Sign1 from a signature. `embedded` = payload inside the
/// structure; `None` = detached (nil).
pub fn cose_sign1(kid: &[u8; 8], embedded: Option<&[u8]>, signature: &[u8; 64]) -> Vec<u8> {
    let mut w = Writer::new();
    w.tag(TAG_SIGN1);
    w.array(4);
    w.bytes(&protected_header());
    w.map(1);
    w.uint(4);
    w.bytes(kid);
    match embedded {
        Some(p) => w.bytes(p),
        None => w.null(),
    }
    w.bytes(signature);
    w.into_vec()
}

fn write_assets(w: &mut Writer, assets: &[ManifestAsset]) {
    w.array(assets.len());
    for b in assets {
        w.map(5);
        w.uint(0);
        w.text(&b.name);
        w.uint(1);
        w.bytes(&b.sha256);
        w.uint(2);
        w.uint(b.size);
        w.uint(3);
        w.uint(b.definitions as u64);
        w.uint(4);
        w.uint(b.generation as u64);
    }
}

/// Parsed COSE_Sign1.
pub struct Sign1<'a> {
    pub protected: &'a [u8],
    pub kid: [u8; 8],
    pub payload: Option<&'a [u8]>,
    pub signature: &'a [u8],
}

pub fn parse_cose_sign1(bytes: &[u8]) -> Result<Sign1<'_>, SignError> {
    let m = |_| SignError::Malformed;
    let mut r = Reader::new(bytes);
    if r.head().map_err(m)? != Head::Tag(TAG_SIGN1) || r.array().map_err(m)? != 4 {
        return Err(SignError::Malformed);
    }
    let protected = r.bytes().map_err(m)?;
    {
        let mut p = Reader::new(protected);
        if p.map().map_err(m)? != 1 || p.uint().map_err(m)? != 1 || p.int().map_err(m)? != ALG_EDDSA
        {
            return Err(SignError::Algorithm);
        }
        p.finish().map_err(m)?;
    }
    let n = r.map().map_err(m)?;
    let mut kid = None;
    for _ in 0..n {
        let k = r.int().map_err(m)?;
        if k == 4 {
            let b = r.bytes().map_err(m)?;
            kid = Some(<[u8; 8]>::try_from(b).map_err(|_| SignError::Malformed)?);
        } else {
            r.skip().map_err(m)?;
        }
    }
    let payload = match r.peek().map_err(m)? {
        Head::Null => {
            r.head().map_err(m)?;
            None
        }
        _ => Some(r.bytes().map_err(m)?),
    };
    let signature = r.bytes().map_err(m)?;
    r.finish().map_err(m)?;
    Ok(Sign1 {
        protected,
        kid: kid.ok_or(SignError::Malformed)?,
        payload,
        signature,
    })
}

/// Verifies a COSE_Sign1. `detached` supplies the payload when the structure
/// carries nil. Returns the payload.
#[cfg(feature = "sign")]
pub fn verify_cose_sign1<'a>(
    bytes: &'a [u8],
    detached: Option<&'a [u8]>,
    keys: &[TrustedKey],
) -> Result<&'a [u8], SignError> {
    let s = parse_cose_sign1(bytes)?;
    let payload = match (s.payload, detached) {
        (Some(p), None) => p,
        (None, Some(p)) => p,
        _ => return Err(SignError::Payload),
    };
    let key = keys
        .iter()
        .find(|k| k.kid == s.kid)
        .ok_or(SignError::UnknownKey(s.kid))?;
    let sig =
        ed25519_compact::Signature::from_slice(s.signature).map_err(|_| SignError::Malformed)?;
    let pk = ed25519_compact::PublicKey::new(key.public);
    pk.verify(to_be_signed(s.protected, payload), &sig)
        .map_err(|_| SignError::BadSignature)?;
    Ok(payload)
}

/// Bank asset entry of the manifest: `{0: name, 1: SHA-256 of the file as
/// published, 2: size, 3: definitions, 4: bank generation}`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ManifestAsset {
    pub name: String,
    pub sha256: [u8; 32],
    pub size: u64,
    pub definitions: u32,
    pub generation: u32,
}

/// Catalog manifest (payload of `manifest.cose`, spec 9.3).
///
/// CBOR map: `0` format (1), `1` catalog date (YYYYMMDD), `2` serial, `3`
/// minimum hub feature set, `4` banks (array of [`ManifestAsset`] maps), `5`
/// shards: 256 × (SHA-256 or null for an empty shard), `6` content digest
/// (SHA-256 of the ascending content hashes of every definition in the
/// shards; equal digests = same catalog content), `7` source
/// (text → text, informational, e.g. `{"commit": …, "zhc": …}`).
/// Keys 64..127 are ignored (optional extensions).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Manifest {
    pub catalog_date: u32,
    pub catalog_serial: u16,
    pub requires: Vec<u16>,
    pub banks: Vec<ManifestAsset>,
    pub shards: Vec<Option<[u8; 32]>>,
    pub content: Option<[u8; 32]>,
    pub source: Vec<(String, String)>,
    /// Key 64 (optional extension, ignored by older hubs): other assets of
    /// the release the hub may install, e.g. `zdefc-web.zwb` (the definition
    /// compiler for the web UI); same map as a bank entry, definitions and
    /// generation 0.
    pub tools: Vec<ManifestAsset>,
}

impl Manifest {
    /// Catalog version as one comparable number (anti-rollback).
    pub fn version(&self) -> u64 {
        ((self.catalog_date as u64) << 16) | self.catalog_serial as u64
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.map(
            6 + self.content.is_some() as usize
                + !self.source.is_empty() as usize
                + !self.tools.is_empty() as usize,
        );
        w.uint(0);
        w.uint(1);
        w.uint(1);
        w.uint(self.catalog_date as u64);
        w.uint(2);
        w.uint(self.catalog_serial as u64);
        w.uint(3);
        w.array(self.requires.len());
        for f in &self.requires {
            w.uint(*f as u64);
        }
        w.uint(4);
        write_assets(&mut w, &self.banks);
        w.uint(5);
        w.array(self.shards.len());
        for s in &self.shards {
            match s {
                Some(h) => w.bytes(h),
                None => w.null(),
            }
        }
        if let Some(c) = &self.content {
            w.uint(6);
            w.bytes(c);
        }
        if !self.source.is_empty() {
            // canonical: keys by encoded length, then bytewise
            let mut src: Vec<&(String, String)> = self.source.iter().collect();
            src.sort_by(|a, b| (a.0.len(), a.0.as_bytes()).cmp(&(b.0.len(), b.0.as_bytes())));
            w.uint(7);
            w.map(src.len());
            for (k, v) in src {
                w.text(k);
                w.text(v);
            }
        }
        if !self.tools.is_empty() {
            w.uint(64);
            write_assets(&mut w, &self.tools);
        }
        w.into_vec()
    }

    pub fn decode(bytes: &[u8]) -> Result<Manifest, DecodeError> {
        let inv = |r: &Reader<'_>, w: &'static str| DecodeError {
            offset: r.position(),
            kind: DecodeErrorKind::Invalid(w),
        };
        let hash = |r: &mut Reader<'_>, w: &'static str| -> Result<[u8; 32], DecodeError> {
            let b = r.bytes()?;
            <[u8; 32]>::try_from(b).map_err(|_| inv(r, w))
        };
        let mut r = Reader::new(bytes);
        let n = r.map()?;
        let mut m = Manifest::default();
        let mut prev = None;
        for _ in 0..n {
            let k = r.uint()?;
            if prev.is_some_and(|p| p >= k) {
                return Err(inv(&r, "manifest key order"));
            }
            prev = Some(k);
            match k {
                0 => {
                    if r.uint()? != 1 {
                        return Err(inv(&r, "manifest format"));
                    }
                }
                1 => m.catalog_date = r.uint()? as u32,
                2 => m.catalog_serial = r.uint()? as u16,
                3 => {
                    for _ in 0..r.array()? {
                        m.requires.push(r.uint()? as u16);
                    }
                }
                4 | 64 => {
                    let top = k;
                    for _ in 0..r.array()? {
                        let mut a = ManifestAsset::default();
                        let mut prev = None;
                        for _ in 0..r.map()? {
                            let k = r.uint()?;
                            if prev.is_some_and(|p| p >= k) {
                                return Err(inv(&r, "manifest bank key order"));
                            }
                            prev = Some(k);
                            match k {
                                0 => a.name = r.text()?.into(),
                                1 => a.sha256 = hash(&mut r, "manifest bank hash")?,
                                2 => a.size = r.uint()?,
                                3 => a.definitions = r.uint()? as u32,
                                4 => a.generation = r.uint()? as u32,
                                _ => {
                                    r.skip()?;
                                }
                            }
                        }
                        if a.name.is_empty() {
                            return Err(inv(&r, "manifest bank name"));
                        }
                        if top == 4 {
                            m.banks.push(a);
                        } else {
                            m.tools.push(a);
                        }
                    }
                }
                5 => {
                    for _ in 0..r.array()? {
                        if matches!(r.peek()?, Head::Null) {
                            r.head()?;
                            m.shards.push(None);
                        } else {
                            m.shards.push(Some(hash(&mut r, "manifest shard hash")?));
                        }
                    }
                }
                6 => m.content = Some(hash(&mut r, "manifest content digest")?),
                7 => {
                    for _ in 0..r.map()? {
                        let k = r.text()?.into();
                        m.source.push((k, r.text()?.into()));
                    }
                }
                _ => {
                    r.skip()?;
                }
            }
        }
        r.finish()?;
        Ok(m)
    }

    /// Verifies `manifest.cose` and decodes its payload.
    #[cfg(feature = "sign")]
    pub fn verify(cose: &[u8], keys: &[TrustedKey]) -> Result<Manifest, ManifestError> {
        let payload = verify_cose_sign1(cose, None, keys).map_err(ManifestError::Signature)?;
        Manifest::decode(payload).map_err(ManifestError::Decode)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestError {
    Signature(SignError),
    Decode(DecodeError),
}
