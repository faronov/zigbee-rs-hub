//! Catalog verifier: the trusted keys of the definitions catalog
//! (`faronov/zigbee-rs-definitions`, spec device-support 9.3) and the
//! checks of its signed assets.
//!
//! Two key slots are built into the firmware. Slot 0 holds the catalog
//! signing key (secret `ZDEF_SIGNING_KEY` of the definitions repository;
//! public key `keys/zdef-catalog-ed25519.pub.pem`). Slot 1 is reserved for
//! a rotation: a successor key is added there in a firmware release first,
//! the catalog is then signed with it, and slot 0 is retired in a later
//! release. A signature verifies against any filled slot (the COSE `kid` —
//! the first 8 bytes of SHA-256 of the public key — selects it).

use zdef::bank::{Bank, BankError};
use zdef::sign::{Manifest, ManifestError, TrustedKey, key_id};

/// Raw Ed25519 public key of slot 0 (2026-11-20).
pub const SLOT_0: [u8; 32] = [
    0xff, 0x93, 0x17, 0x67, 0xdf, 0x34, 0x7a, 0x7b, 0xcc, 0xd1, 0x4b, 0x69, 0x60, 0x2d, 0x4e, 0xb7, 0x84, 0x1e, 0xf8,
    0x50, 0x3f, 0x77, 0x12, 0x48, 0xb7, 0x17, 0xf4, 0x55, 0x4f, 0x63, 0xc6, 0xa2,
];

/// Key slots: 0 current, 1 reserved for rotation.
pub const KEY_SLOTS: [Option<[u8; 32]>; 2] = [Some(SLOT_0), None];

/// The filled slots as trusted keys.
pub fn trusted_keys() -> Vec<TrustedKey> {
    KEY_SLOTS.iter().flatten().map(|k| TrustedKey::new(*k)).collect()
}

/// `(slot, kid)` of the filled slots, for status output.
pub fn key_ids() -> Vec<(usize, [u8; 8])> {
    KEY_SLOTS.iter().enumerate().filter_map(|(i, k)| k.map(|k| (i, key_id(&k)))).collect()
}

/// Verifies `manifest.cose` against the built-in keys and decodes it.
pub fn verify_manifest(cose: &[u8]) -> Result<Manifest, ManifestError> {
    Manifest::verify(cose, &trusted_keys())
}

/// Parses a bank and checks its body hash and header signature against the
/// built-in keys.
pub fn verify_bank(bytes: &[u8]) -> Result<Bank<'_>, BankError> {
    let bank = Bank::parse(bytes)?;
    bank.verify(&trusted_keys())?;
    Ok(bank)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use zdef::sign::{SignError, cose_sign1, protected_header, to_be_signed};

    #[test]
    fn slot_0_is_the_committed_public_key() {
        let pem = include_str!("../../../keys/zdef-catalog-ed25519.pub.pem");
        let b64: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
        let der = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        // SubjectPublicKeyInfo for Ed25519: 12-byte prefix, then the key.
        assert_eq!(der[..12], [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00]);
        assert_eq!(der[12..], SLOT_0);
        assert_eq!(KEY_SLOTS[1], None, "slot 1 is reserved");
        assert_eq!(trusted_keys().len(), 1);
    }

    #[test]
    fn manifests_from_other_keys_are_refused_by_kid() {
        let kp = ed25519_compact::KeyPair::from_seed(ed25519_compact::Seed::new([9; 32]));
        let public: [u8; 32] = *kp.pk;
        let manifest = Manifest { catalog_date: 20261120, catalog_serial: 1, requires: vec![], banks: vec![], shards: vec![None; 256], ..Default::default() };
        let payload = manifest.encode();
        let sig = kp.sk.sign(to_be_signed(&protected_header(), &payload), None);
        let cose = cose_sign1(&key_id(&public), Some(&payload), &sig);
        // With the signer's key trusted the manifest verifies …
        assert_eq!(Manifest::verify(&cose, &[TrustedKey::new(public)]).unwrap(), manifest);
        // … the built-in slots do not know it.
        assert_eq!(verify_manifest(&cose), Err(ManifestError::Signature(SignError::UnknownKey(key_id(&public)))));
    }
}
