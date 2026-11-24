//! Network identity and ownership (design §2.5).

use sha2::{Digest, Sha256};

/// Domain prefix of [`NetworkIdentity::key_id_for`]; part of the persisted
/// ownership tag, never change it.
const KEY_ID_DOMAIN: &[u8] = b"zigbee-rs-hub/key-id/1";

/// Identity of a Zigbee network. Only the extended PAN ID and the key ID
/// identify it ([`Self::matches`]); the other fields are informative and
/// are rewritten when they change (PAN ID conflict resolution, channel
/// change, key rotation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkIdentity {
    /// Extended PAN ID (wire order).
    pub ext_pan_id: [u8; 8],
    /// [`Self::key_id_for`] of the network key; `None` when the key is not
    /// known (not exportable).
    pub key_id: Option<[u8; 8]>,
    /// PAN ID.
    pub pan_id: u16,
    /// Channel.
    pub channel: u8,
    /// Network key sequence number.
    pub key_seq: u8,
    /// Coordinator IEEE address (wire order).
    pub coordinator_ieee: [u8; 8],
}

impl NetworkIdentity {
    /// Key ID of a network key: the first 8 bytes of
    /// SHA-256(`"zigbee-rs-hub/key-id/1"` ‖ key). It identifies the key
    /// without revealing it.
    pub fn key_id_for(key: &[u8; 16]) -> [u8; 8] {
        let digest = Sha256::new()
            .chain_update(KEY_ID_DOMAIN)
            .chain_update(key)
            .finalize();
        let mut id = [0; 8];
        id.copy_from_slice(&digest[..8]);
        id
    }

    /// Same network: the extended PAN IDs are equal and, when both key IDs
    /// are known, so are the key IDs.
    pub fn matches(&self, other: &NetworkIdentity) -> bool {
        self.ext_pan_id == other.ext_pan_id
            && match (self.key_id, other.key_id) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
    }
}

/// Network ownership as the backend (ZRHP) or the hub (EZSP, ZNP) sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkOwnership {
    /// No session yet.
    Unknown,
    /// Neither the NCP nor the hub has a network (form one).
    NoNetwork,
    /// The NCP runs the hub's network.
    Owned(NetworkIdentity),
    /// The hub has a network but the NCP has none (NV wiped): restore from
    /// a backup.
    Lost(Option<NetworkIdentity>),
    /// The NCP runs a different network, or the hub's state belongs to
    /// another NCP; the hub does not touch it.
    Mismatch {
        /// The NCP's network, if it has one.
        found: Option<NetworkIdentity>,
        /// Why it does not match.
        why: String,
    },
    /// Observed only (read-only policy, hub state empty).
    Observed(NetworkIdentity),
}

impl NetworkOwnership {
    /// The `network/info` `ownership` string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::NoNetwork => "no_network",
            Self::Owned(_) => "owned",
            Self::Lost(_) => "lost",
            Self::Mismatch { .. } => "mismatch",
            Self::Observed(_) => "observed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(epid: u8, key_id: Option<[u8; 8]>) -> NetworkIdentity {
        NetworkIdentity {
            ext_pan_id: [epid; 8],
            key_id,
            pan_id: 0x1A62,
            channel: 15,
            key_seq: 0,
            coordinator_ieee: [0x11; 8],
        }
    }

    #[test]
    fn key_id_is_the_domain_separated_sha256_prefix() {
        // python3: hashlib.sha256(b"zigbee-rs-hub/key-id/1" + key).digest()[:8]
        let counting: [u8; 16] = core::array::from_fn(|i| i as u8);
        assert_eq!(
            NetworkIdentity::key_id_for(&counting),
            [0x75, 0x28, 0x9d, 0x14, 0x4d, 0x5c, 0x61, 0xde]
        );
        let z2m_default = [
            0x01, 0x03, 0x05, 0x07, 0x09, 0x0B, 0x0D, 0x0F, 0x00, 0x02, 0x04, 0x06, 0x08, 0x0A,
            0x0C, 0x0D,
        ];
        assert_eq!(
            NetworkIdentity::key_id_for(&z2m_default),
            [0xa4, 0x0c, 0xa8, 0xd1, 0xe9, 0x4f, 0x0b, 0x05]
        );
    }

    #[test]
    fn matching_needs_the_epid_and_known_key_ids() {
        let k1 = Some([1; 8]);
        let k2 = Some([2; 8]);
        let a = id(0xAA, k1);
        let moved = NetworkIdentity {
            pan_id: 0x2B73,
            channel: 25,
            key_seq: 3,
            coordinator_ieee: [0x22; 8],
            ..a
        };
        assert!(
            a.matches(&moved),
            "PAN ID, channel, key_seq and IEEE are informative"
        );
        assert!(!a.matches(&id(0xAA, k2)), "same EPID, another key");
        assert!(
            a.matches(&id(0xAA, None)) && id(0xAA, None).matches(&a),
            "key unknown on one side"
        );
        assert!(id(0xAA, None).matches(&id(0xAA, None)));
        assert!(!a.matches(&id(0xBB, k1)), "another EPID, same key");
        assert!(!id(0xAA, None).matches(&id(0xBB, None)));
    }
}
