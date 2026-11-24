//! CRC-32C (Castagnoli) for the journal and the catalog areas.
//!
//! Reflected polynomial `0x82F63B78`, init and xorout `0xFFFF_FFFF`: the
//! same values as ZRHP frames, so existing `hubdata` stays readable. A
//! 16-entry table keeps a ZRHP image, which also links the ZRHP crate's
//! 1 KiB table, a few dozen bytes larger instead of a kilobyte.

const POLY: u32 = 0x82F6_3B78;

const TABLE: [u32; 16] = {
    let mut table = [0u32; 16];
    let mut i = 0;
    while i < 16 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 4 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ POLY } else { crc >> 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// Incremental CRC-32C state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crc32c {
    state: u32,
}

impl Crc32c {
    /// A fresh CRC.
    pub const fn new() -> Self {
        Self { state: 0xFFFF_FFFF }
    }

    /// Feed bytes.
    pub fn update(&mut self, data: &[u8]) {
        for &b in data {
            self.update_byte(b);
        }
    }

    /// Feed one byte.
    #[inline]
    pub fn update_byte(&mut self, byte: u8) {
        let mut c = self.state ^ byte as u32;
        c = TABLE[(c & 0xF) as usize] ^ (c >> 4);
        c = TABLE[(c & 0xF) as usize] ^ (c >> 4);
        self.state = c;
    }

    /// The final value; the state is not consumed.
    pub const fn finish(&self) -> u32 {
        self.state ^ 0xFFFF_FFFF
    }
}

impl Default for Crc32c {
    fn default() -> Self {
        Self::new()
    }
}

/// One-shot CRC-32C.
pub fn crc32c(data: &[u8]) -> u32 {
    let mut c = Crc32c::new();
    c.update(data);
    c.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3720_vectors() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
        assert_eq!(crc32c(&[0xFFu8; 32]), 0x62A8_AB43);
        let inc: Vec<u8> = (0..32).collect();
        assert_eq!(crc32c(&inc), 0x46DD_794E);
        let dec: Vec<u8> = (0..32).rev().collect();
        assert_eq!(crc32c(&dec), 0x113F_DB5C);
    }

    #[test]
    fn matches_zrhp() {
        let data: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        for n in [0, 1, 3, 12, 64, 255, 4096] {
            assert_eq!(crc32c(&data[..n]), zigbee_zrhp::crc::crc32c(&data[..n]), "len {n}");
        }
        let mut a = Crc32c::new();
        let mut b = zigbee_zrhp::crc::Crc32c::new();
        for chunk in data.chunks(37) {
            a.update(&chunk[1..]);
            a.update_byte(chunk[0]);
            b.update(&chunk[1..]);
            b.update_byte(chunk[0]);
        }
        assert_eq!(a.finish(), b.finish());
    }
}
