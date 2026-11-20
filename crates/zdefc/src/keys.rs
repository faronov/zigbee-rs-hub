//! Ed25519 key files for catalog signing (spec 9.4).
//!
//! Accepted private keys (auto-detected, `#` comment lines ignored):
//! - PEM `PRIVATE KEY` (PKCS#8 / RFC 8410 `OneAsymmetricKey` v1 or v2), as
//!   written by `openssl genpkey -algorithm ed25519` and Python
//!   `cryptography`; this is the format of the `ZDEF_SIGNING_KEY` secret;
//! - 64 hex digits: the raw 32-byte seed (`zdefc bank keygen`).
//!
//! Accepted public keys: PEM `PUBLIC KEY` (SubjectPublicKeyInfo, RFC 8410,
//! e.g. `keys/zdef-catalog-ed25519.pub.pem`) or 64 hex digits.
//!
//! Encrypted PEM keys are rejected: CI passes the key unencrypted through a
//! secret.

/// `id-Ed25519` (1.3.101.112) as DER OID content.
const OID_ED25519: [u8; 3] = [0x2b, 0x65, 0x70];
/// `AlgorithmIdentifier { id-Ed25519 }` (parameters absent, RFC 8410 §3).
const ALG_ED25519: [u8; 7] = [0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70];

/// Private key material read from a key file.
#[derive(Debug, PartialEq, Eq)]
pub struct PrivateKey {
    pub seed: [u8; 32],
    /// Public key embedded in a PKCS#8 v2 key, checked by the caller.
    pub public: Option<[u8; 32]>,
}

fn strip_comments(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Body of the PEM block with `label`, base64-decoded.
fn pem_block(text: &str, label: &str) -> Result<Option<Vec<u8>>, String> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let Some(b) = text.find(&begin) else {
        return Ok(None);
    };
    let rest = &text[b + begin.len()..];
    let e = rest
        .find(&end)
        .ok_or_else(|| format!("PEM: missing {end}"))?;
    let body: String = rest[..e]
        .lines()
        .filter(|l| !l.contains(':'))
        .flat_map(|l| l.split_whitespace())
        .collect();
    base64_decode(&body)
        .map(Some)
        .ok_or_else(|| "PEM: invalid base64".into())
}

fn base64_val(c: u8) -> Option<u32> {
    Some(match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    } as u32)
}

pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=').as_bytes();
    if s.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.chunks(4) {
        let mut acc = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            acc |= base64_val(c)? << (18 - 6 * i);
        }
        let n = chunk.len() - 1;
        out.extend_from_slice(&acc.to_be_bytes()[1..1 + n]);
    }
    Some(out)
}

pub fn base64_encode(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in b.chunks(3) {
        let acc = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                s.push(T[(acc >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

fn pem(label: &str, der: &[u8]) -> String {
    let b = base64_encode(der);
    let mut s = format!("-----BEGIN {label}-----\n");
    for l in b.as_bytes().chunks(64) {
        s.push_str(std::str::from_utf8(l).unwrap());
        s.push('\n');
    }
    s.push_str(&format!("-----END {label}-----\n"));
    s
}

/// Minimal DER reader: one TLV with a definite length.
fn tlv(b: &[u8]) -> Result<(u8, &[u8], &[u8]), String> {
    let bad = || "DER: truncated".to_string();
    let (&tag, rest) = b.split_first().ok_or_else(bad)?;
    let (&l0, mut rest) = rest.split_first().ok_or_else(bad)?;
    let len = if l0 < 0x80 {
        l0 as usize
    } else {
        let n = (l0 & 0x7f) as usize;
        if n == 0 || n > 2 || rest.len() < n {
            return Err("DER: unsupported length".into());
        }
        let v = rest[..n].iter().fold(0usize, |a, &x| a << 8 | x as usize);
        rest = &rest[n..];
        v
    };
    if rest.len() < len {
        return Err(bad());
    }
    Ok((tag, &rest[..len], &rest[len..]))
}

fn expect_alg(alg: &[u8]) -> Result<(), String> {
    let (tag, body, _) = tlv(alg)?;
    let (otag, oid, params) = tlv(body)?;
    if tag != 0x30 || otag != 0x06 || oid != OID_ED25519 || !params.is_empty() {
        return Err("not an Ed25519 key (algorithm must be id-Ed25519, 1.3.101.112)".into());
    }
    Ok(())
}

/// PKCS#8 `OneAsymmetricKey` (RFC 5958 / RFC 8410 §7).
pub fn parse_pkcs8(der: &[u8]) -> Result<PrivateKey, String> {
    let (tag, body, trail) = tlv(der)?;
    if tag != 0x30 || !trail.is_empty() {
        return Err("PKCS#8: expected one SEQUENCE".into());
    }
    let (vt, ver, rest) = tlv(body)?;
    if vt != 0x02 || !(ver == [0] || ver == [1]) {
        return Err("PKCS#8: unsupported version".into());
    }
    let (_, _, after_alg) = tlv(rest)?;
    expect_alg(&rest[..rest.len() - after_alg.len()])?;
    let (pt, octets, mut more) = tlv(after_alg)?;
    if pt != 0x04 {
        return Err("PKCS#8: expected privateKey OCTET STRING".into());
    }
    // CurvePrivateKey ::= OCTET STRING (32 bytes)
    let (it, seed, t2) = tlv(octets)?;
    if it != 0x04 || seed.len() != 32 || !t2.is_empty() {
        return Err("PKCS#8: Ed25519 private key must be a 32-byte OCTET STRING".into());
    }
    let mut public = None;
    while !more.is_empty() {
        let (t, v, r) = tlv(more)?;
        match t {
            0xa0 => {} // attributes
            0x81 => {
                // [1] IMPLICIT BIT STRING: unused-bits byte + 32 bytes
                if v.len() != 33 || v[0] != 0 {
                    return Err("PKCS#8: bad publicKey".into());
                }
                public = Some(v[1..].try_into().unwrap());
            }
            _ => return Err("PKCS#8: unexpected field".into()),
        }
        more = r;
    }
    Ok(PrivateKey {
        seed: seed.try_into().unwrap(),
        public,
    })
}

/// SubjectPublicKeyInfo (RFC 8410 §4).
pub fn parse_spki(der: &[u8]) -> Result<[u8; 32], String> {
    let (tag, body, trail) = tlv(der)?;
    if tag != 0x30 || !trail.is_empty() {
        return Err("SPKI: expected one SEQUENCE".into());
    }
    let (_, _, after_alg) = tlv(body)?;
    expect_alg(&body[..body.len() - after_alg.len()])?;
    let (bt, bits, rest) = tlv(after_alg)?;
    if bt != 0x03 || bits.len() != 33 || bits[0] != 0 || !rest.is_empty() {
        return Err("SPKI: Ed25519 public key must be a 32-byte BIT STRING".into());
    }
    Ok(bits[1..].try_into().unwrap())
}

fn hex32(text: &str) -> Option<[u8; 32]> {
    let h: String = text.split_whitespace().collect();
    crate::bank::unhex(&h).and_then(|b| b.try_into().ok())
}

/// Reads a private key: PKCS#8 PEM or a 64-hex-digit seed.
pub fn parse_private(text: &str) -> Result<PrivateKey, String> {
    let text = strip_comments(text);
    if text.contains("ENCRYPTED PRIVATE KEY") {
        return Err(
            "signing key: encrypted PEM is not supported (use an unencrypted PKCS#8 key)".into(),
        );
    }
    if let Some(der) = pem_block(&text, "PRIVATE KEY")? {
        return parse_pkcs8(&der).map_err(|e| format!("signing key: {e}"));
    }
    if text.contains("-----BEGIN") {
        return Err("signing key: expected a PEM PRIVATE KEY (PKCS#8) block".into());
    }
    hex32(&text)
        .map(|seed| PrivateKey { seed, public: None })
        .ok_or_else(|| {
            "signing key: expected a PKCS#8 PEM (openssl genpkey -algorithm ed25519) or 64 hex digits (seed)"
                .into()
        })
}

/// Reads a public key: SPKI PEM or 64 hex digits.
pub fn parse_public(text: &str) -> Result<[u8; 32], String> {
    let text = strip_comments(text);
    if let Some(der) = pem_block(&text, "PUBLIC KEY")? {
        return parse_spki(&der).map_err(|e| format!("public key: {e}"));
    }
    if text.contains("-----BEGIN") {
        return Err("public key: expected a PEM PUBLIC KEY block".into());
    }
    hex32(&text).ok_or_else(|| "public key: expected a PEM PUBLIC KEY or 64 hex digits".into())
}

/// PKCS#8 v1 PEM of a seed (what `openssl genpkey` writes).
pub fn private_pem(seed: &[u8; 32]) -> String {
    let mut der = vec![0x30, 0x2e, 0x02, 0x01, 0x00];
    der.extend_from_slice(&ALG_ED25519);
    der.extend_from_slice(&[0x04, 0x22, 0x04, 0x20]);
    der.extend_from_slice(seed);
    pem("PRIVATE KEY", &der)
}

/// SPKI PEM of a public key.
pub fn public_pem(public: &[u8; 32]) -> String {
    let mut der = vec![0x30, 0x2a];
    der.extend_from_slice(&ALG_ED25519);
    der.extend_from_slice(&[0x03, 0x21, 0x00]);
    der.extend_from_slice(public);
    pem("PUBLIC KEY", &der)
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 8410 §10.1 / §10.3 example key pair (public test vector).
    const RFC_PRIV_V1: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEINTuctv5E1hK1bbY8fdp+K06/nwoy/HU++CXqI9EdVhC\n-----END PRIVATE KEY-----\n";
    const RFC_PRIV_V2: &str = "-----BEGIN PRIVATE KEY-----\nMHICAQEwBQYDK2VwBCIEINTuctv5E1hK1bbY8fdp+K06/nwoy/HU++CXqI9EdVhC\noB8wHQYKKoZIhvcNAQkJFDEPDA1DdXJkbGUgQ2hhaXJzgSEAGb9ECWmEzf6FQbrB\nZ9w7lshQhqowtrbLDFw4rXAxZuE=\n-----END PRIVATE KEY-----\n";
    const RFC_PUB: &str = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAGb9ECWmEzf6FQbrBZ9w7lshQhqowtrbLDFw4rXAxZuE=\n-----END PUBLIC KEY-----\n";
    const RFC_SEED: &str = "d4ee72dbf913584ad5b6d8f1f769f8ad3afe7c28cbf1d4fbe097a88f44755842";
    const RFC_PUB_HEX: &str = "19bf44096984cdfe8541bac167dc3b96c85086aa30b6b6cb0c5c38ad703166e1";

    #[test]
    fn rfc8410_vectors() {
        let v1 = parse_private(RFC_PRIV_V1).unwrap();
        assert_eq!(crate::hex(&v1.seed), RFC_SEED);
        assert_eq!(v1.public, None);
        let v2 = parse_private(RFC_PRIV_V2).unwrap();
        assert_eq!(v2.seed, v1.seed);
        assert_eq!(crate::hex(&v2.public.unwrap()), RFC_PUB_HEX);
        assert_eq!(crate::hex(&parse_public(RFC_PUB).unwrap()), RFC_PUB_HEX);
        assert_eq!(private_pem(&v1.seed), RFC_PRIV_V1);
        assert_eq!(public_pem(&parse_public(RFC_PUB_HEX).unwrap()), RFC_PUB);
        let hex = parse_private(&format!("# comment\n{RFC_SEED}\n")).unwrap();
        assert_eq!(hex.seed, v1.seed);
    }

    #[test]
    fn rejects_other_keys() {
        assert!(
            parse_private(
                "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAA==\n-----END ENCRYPTED PRIVATE KEY-----"
            )
            .is_err()
        );
        // X25519 (1.3.101.110) PKCS#8
        let x = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VuBCIEINTuctv5E1hK1bbY8fdp+K06/nwoy/HU++CXqI9EdVhC\n-----END PRIVATE KEY-----\n";
        assert!(parse_private(x).unwrap_err().contains("Ed25519"));
        assert!(parse_private("abcd").is_err());
        assert!(parse_public(RFC_PRIV_V1).is_err());
    }

    #[test]
    fn base64_round_trip() {
        for n in 0..10 {
            let b: Vec<u8> = (0..n as u8).map(|x| x.wrapping_mul(37)).collect();
            assert_eq!(base64_decode(&base64_encode(&b)).unwrap(), b);
        }
    }
}
