//! The ownership tag of the hub's network-scoped records and what it says
//! about them (design §2.7).
//!
//! The tag is the hub's KV record `hub/owner`:
//! `{"v":1,"backend":"zrhp","epid":…,"key_id":…|null,"pan_id":…,"channel":…,"key_seq":…,"coordinator":…}`
//! (EPID and coordinator IEEE as big-endian hex, as the API shows EUI-64s;
//! the key ID as hex in digest order), or `{"v":1,"backend":"zrhp","epid":null}`
//! while the backend has not owned a network yet.

use crate::{BackendKind, NetworkIdentity, OwnershipModel};

/// Which backend and network the hub's network-scoped records belong to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerTag {
    /// The backend that keeps them ([`BackendKind::as_str`]; another
    /// image's backend may be one this build does not know).
    pub backend: String,
    /// Their network; `None` until the backend first owns one.
    pub identity: Option<NetworkIdentity>,
}

/// Lower-case hex of 8 bytes (`rev`: last byte first, as EUI-64s are shown).
fn hex8(b: &[u8; 8], rev: bool) -> [u8; 16] {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0; 16];
    for (i, x) in b.iter().enumerate() {
        let at = 2 * if rev { 7 - i } else { i };
        out[at] = DIGITS[usize::from(x >> 4)];
        out[at + 1] = DIGITS[usize::from(x & 15)];
    }
    out
}

fn push_hex8(s: &mut String, b: &[u8; 8], rev: bool) {
    s.push('"');
    s.extend(hex8(b, rev).map(char::from));
    s.push('"');
}

/// The raw value of `key` in the flat JSON object [`OwnerTag::encode`]
/// writes (no whitespace): a string's text, or a literal (number, `null`).
fn field<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let b = json.as_bytes();
    let k = key.as_bytes();
    let at = (0..b.len()).find(|&i| {
        b[i] == b'"'
            && b.get(i + 1..i + 1 + k.len()) == Some(k)
            && b.get(i + 1 + k.len()..i + 3 + k.len()) == Some(b"\":")
    })? + k.len()
        + 3;
    let (from, stops): (usize, &[u8]) = if b.get(at) == Some(&b'"') {
        (at + 1, b"\"")
    } else {
        (at, b",}")
    };
    let len = b.get(from..)?.iter().position(|c| stops.contains(c))?;
    json.get(from..from + len)
}

/// The tag's value of `key` is the hex of `b` (case-insensitive).
fn same_hex(tag: &str, key: &str, b: &[u8; 8], rev: bool) -> bool {
    field(tag, key).is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(&hex8(b, rev)))
}

impl OwnerTag {
    /// The tag of `backend`'s records of `identity`.
    pub fn new(backend: BackendKind, identity: Option<NetworkIdentity>) -> Self {
        Self {
            backend: backend.as_str().into(),
            identity,
        }
    }

    /// The record's value.
    pub fn encode(&self) -> String {
        let mut s = String::with_capacity(160);
        s.push_str(r#"{"v":1,"backend":""#);
        s.push_str(&self.backend);
        s.push_str(r#"","epid":"#);
        let Some(n) = &self.identity else {
            s.push_str("null}");
            return s;
        };
        push_hex8(&mut s, &n.ext_pan_id, true);
        s.push_str(r#","key_id":"#);
        match &n.key_id {
            Some(k) => push_hex8(&mut s, k, false),
            None => s.push_str("null"),
        }
        let nums = [
            (r#","pan_id":"#, n.pan_id),
            (r#","channel":"#, n.channel.into()),
            (r#","key_seq":"#, n.key_seq.into()),
        ];
        for (k, mut v) in nums {
            s.push_str(k);
            let mut digits = [0; 5];
            let mut n = 0;
            loop {
                digits[n] = b'0' + (v % 10) as u8;
                n += 1;
                v /= 10;
                if v == 0 {
                    break;
                }
            }
            s.extend(digits[..n].iter().rev().map(|d| char::from(*d)));
        }
        s.push_str(r#","coordinator":"#);
        push_hex8(&mut s, &n.coordinator_ieee, true);
        s.push('}');
        s
    }

    /// The tag in a record's value (`None`: not a tag of this format).
    pub fn decode(value: &[u8]) -> Option<Self> {
        let t = std::str::from_utf8(value).ok()?;
        if field(t, "v")? != "1" {
            return None;
        }
        let bytes8 = |key: &str, rev: bool| -> Option<[u8; 8]> {
            let h = field(t, key).filter(|h| h.len() == 16)?;
            let mut out = [0; 8];
            for (i, o) in out.iter_mut().enumerate() {
                *o = u8::from_str_radix(h.get(2 * i..2 * i + 2)?, 16).ok()?;
            }
            if rev {
                out.reverse();
            }
            Some(out)
        };
        let identity = match field(t, "epid")? {
            "null" => None,
            _ => Some(NetworkIdentity {
                ext_pan_id: bytes8("epid", true)?,
                key_id: match field(t, "key_id")? {
                    "null" => None,
                    _ => Some(bytes8("key_id", false)?),
                },
                pan_id: field(t, "pan_id")?.parse().ok()?,
                channel: field(t, "channel")?.parse().ok()?,
                key_seq: field(t, "key_seq")?.parse().ok()?,
                coordinator_ieee: bytes8("coordinator", true)?,
            }),
        };
        Some(Self {
            backend: field(t, "backend")?.into(),
            identity,
        })
    }
}

/// The state of the hub's network-scoped records (`network/info.registry`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RegistryState {
    /// No tag: nothing network-scoped was stored yet.
    #[default]
    Untagged,
    /// They are this backend's and belong to its network.
    Owned,
    /// An NCP-held network ([`OwnershipModel::NcpNv`]) not compared with
    /// the tag yet: loaded, not writable.
    Unverified,
    /// They are another backend's or another network's: neither loaded
    /// nor writable until the operator adopts or clears them.
    Foreign {
        /// The tag's backend ([`BackendKind::as_str`], `unknown`).
        backend: &'static str,
        /// Why they are not this backend's.
        why: &'static str,
    },
}

impl RegistryState {
    /// The `network/info.registry.state` string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Untagged => "untagged",
            Self::Owned => "owned",
            Self::Unverified => "unverified",
            Self::Foreign { .. } => "foreign",
        }
    }

    /// What the stored `tag` says about the records for `backend`, whose
    /// network is `network` (`None`: none known yet). Matching follows
    /// [`NetworkIdentity::matches`]: the EPID, and the key ID when both
    /// are known.
    pub fn check(
        tag: Option<&[u8]>,
        backend: BackendKind,
        model: OwnershipModel,
        network: Option<&NetworkIdentity>,
    ) -> Self {
        let Some(tag) = tag else {
            return Self::Untagged;
        };
        let foreign = |backend, why| Self::Foreign { backend, why };
        let Some(t) = std::str::from_utf8(tag)
            .ok()
            .filter(|t| field(t, "v") == Some("1"))
        else {
            return foreign("unknown", "unreadable tag");
        };
        let tagged = field(t, "backend").unwrap_or("");
        if tagged != backend.as_str() {
            let other = [BackendKind::Zrhp, BackendKind::Ezsp, BackendKind::Znp]
                .into_iter()
                .map(BackendKind::as_str)
                .find(|b| *b == tagged);
            return foreign(other.unwrap_or("unknown"), "another coordinator backend");
        }
        let backend = backend.as_str();
        match (field(t, "epid").filter(|e| *e != "null"), network) {
            (Some(_), Some(n)) if !same_hex(t, "epid", &n.ext_pan_id, true) => {
                foreign(backend, "another extended PAN ID")
            }
            (Some(_), Some(n))
                if n.key_id.is_some_and(|k| {
                    field(t, "key_id").is_some_and(|v| v != "null")
                        && !same_hex(t, "key_id", &k, false)
                }) =>
            {
                foreign(backend, "another network key")
            }
            (_, None) if model == OwnershipModel::NcpNv => Self::Unverified,
            _ => Self::Owned,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(epid: u8, key: Option<u8>) -> NetworkIdentity {
        NetworkIdentity {
            ext_pan_id: [epid, 2, 3, 4, 5, 6, 7, 8],
            key_id: key.map(|k| NetworkIdentity::key_id_for(&[k; 16])),
            pan_id: 0x1A2B,
            channel: 15,
            key_seq: 0,
            coordinator_ieee: [0x5A, 0x51, 0, 0, 0, 0, 1, 2],
        }
    }

    fn tag(backend: BackendKind, identity: Option<NetworkIdentity>) -> Vec<u8> {
        OwnerTag::new(backend, identity).encode().into_bytes()
    }

    #[test]
    fn the_tag_round_trips_in_the_documented_form() {
        let n = id(0x04, Some(0x42));
        let text = OwnerTag::new(BackendKind::Zrhp, Some(n)).encode();
        assert_eq!(
            text,
            format!(
                r#"{{"v":1,"backend":"zrhp","epid":"0807060504030204","key_id":"{}","pan_id":6699,"channel":15,"key_seq":0,"coordinator":"020100000000515a"}}"#,
                String::from_utf8(hex8(&n.key_id.unwrap(), false).to_vec()).unwrap()
            )
        );
        assert_eq!(
            OwnerTag::decode(text.as_bytes()).unwrap(),
            OwnerTag::new(BackendKind::Zrhp, Some(n))
        );
        let unknown_key = OwnerTag::new(BackendKind::Zrhp, Some(id(0x04, None)));
        assert!(unknown_key.encode().contains(r#""key_id":null"#));
        assert_eq!(
            OwnerTag::decode(unknown_key.encode().as_bytes()).unwrap(),
            unknown_key
        );
        let pending = OwnerTag::new(BackendKind::Ezsp, None);
        assert_eq!(pending.encode(), r#"{"v":1,"backend":"ezsp","epid":null}"#);
        assert_eq!(
            OwnerTag::decode(pending.encode().as_bytes()).unwrap(),
            pending
        );
        // Field order is free.
        let reordered = br#"{"epid":null,"backend":"ezsp","v":1}"#;
        assert_eq!(OwnerTag::decode(reordered), Some(pending));
        for bad in [
            &br#"{"v":2,"backend":"zrhp","epid":null}"#[..],
            br#"{"v":1,"epid":null}"#,
            br#"{"v":1,"backend":"zrhp","epid":"0102"}"#,
            br#"{"v":1,"backend":"zrhp","epid":"0807060504030204","key_id":null}"#,
            br#"not json"#,
            b"\xff",
        ] {
            assert!(
                OwnerTag::decode(bad).is_none(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    /// The identity matrix of design §7 row I.
    #[test]
    fn identity_matrix() {
        use BackendKind::{Ezsp, Zrhp};
        use OwnershipModel::{HostStore, NcpNv};
        let stored = id(0x04, Some(1));
        let check = |t: &[u8], b, m, n: Option<NetworkIdentity>| {
            RegistryState::check(Some(t), b, m, n.as_ref())
        };
        let zrhp = tag(Zrhp, Some(stored));
        assert_eq!(
            RegistryState::check(None, Zrhp, HostStore, Some(&stored)),
            RegistryState::Untagged
        );
        assert_eq!(
            check(&zrhp, Zrhp, HostStore, Some(stored)),
            RegistryState::Owned
        );
        let moved = NetworkIdentity {
            pan_id: 0x2B3C,
            channel: 25,
            key_seq: 1,
            coordinator_ieee: [9; 8],
            ..stored
        };
        assert_eq!(
            check(&zrhp, Zrhp, HostStore, Some(moved)),
            RegistryState::Owned,
            "PAN ID, channel, key_seq, IEEE"
        );
        assert_eq!(
            check(&zrhp, Zrhp, HostStore, Some(id(0x04, None))),
            RegistryState::Owned,
            "key unknown: EPID only"
        );
        assert_eq!(
            check(
                &tag(Zrhp, Some(id(0x04, None))),
                Zrhp,
                HostStore,
                Some(stored)
            ),
            RegistryState::Owned
        );
        let rotated = check(&zrhp, Zrhp, HostStore, Some(id(0x04, Some(2))));
        assert_eq!(rotated.as_str(), "foreign", "same EPID, another key");
        assert!(
            matches!(rotated, RegistryState::Foreign { why, .. } if why.contains("network key")),
            "{rotated:?}"
        );
        let other = check(&zrhp, Zrhp, HostStore, Some(id(0x05, Some(1))));
        assert_eq!(
            other,
            RegistryState::Foreign {
                backend: "zrhp",
                why: "another extended PAN ID"
            }
        );
        assert_eq!(
            check(&zrhp, Zrhp, HostStore, None),
            RegistryState::Owned,
            "no network to compare"
        );
        assert_eq!(
            check(&tag(Zrhp, None), Zrhp, HostStore, Some(stored)),
            RegistryState::Owned,
            "filled at the first Owned"
        );

        // Another backend's records, whatever the network.
        let ezsp = tag(Ezsp, Some(stored));
        let foreign = check(&ezsp, Zrhp, HostStore, Some(stored));
        assert!(
            matches!(
                foreign,
                RegistryState::Foreign {
                    backend: "ezsp",
                    ..
                }
            ),
            "{foreign:?}"
        );
        assert_eq!(
            check(
                br#"{"v":1,"backend":"znp-x","epid":null}"#,
                Zrhp,
                HostStore,
                None
            ),
            RegistryState::Foreign {
                backend: "unknown",
                why: "another coordinator backend"
            }
        );
        assert_eq!(
            check(b"\xff", Zrhp, HostStore, None).as_str(),
            "foreign",
            "unreadable"
        );

        // NCP-held networks: unverified until the NCP's identity is known.
        assert_eq!(check(&ezsp, Ezsp, NcpNv, None), RegistryState::Unverified);
        assert_eq!(
            check(&tag(Ezsp, None), Ezsp, NcpNv, None),
            RegistryState::Unverified
        );
        assert_eq!(check(&ezsp, Ezsp, NcpNv, Some(moved)), RegistryState::Owned);
        assert_eq!(
            check(&ezsp, Ezsp, NcpNv, Some(id(0x05, Some(1)))).as_str(),
            "foreign"
        );
    }
}
