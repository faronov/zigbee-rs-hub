//! SipHash-2-4 for index keys (spec 9.1).

/// Fixed public SipHash key of the index (spec 9.1).
pub const INDEX_KEY: [u8; 16] = *b"zdef-index-v1\0\0\0";

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
