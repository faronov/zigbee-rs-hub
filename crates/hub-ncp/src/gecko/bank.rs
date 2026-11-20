//! Radio image banks in `hubdata`: two banks, each a 4 KiB header sector
//! followed by one `.gbl` image. One bank holds the last-known-good image
//! (the image the NCP ran with a verified ZRHP session), the other receives
//! uploads. A failed update is rolled back by installing the
//! last-known-good image through the Gecko bootloader again.
//!
//! The header sector is an append-only log of 36-byte records
//! `"ZGBL", u8 format, u8 state, u16 0, u32 seq, u32 len, u32 crc32,
//! u32 app_version, [8] build_id, u32 crc32(record[..32])`; the last valid
//! record wins, so a state change never erases the image's description
//! (the sector is erased only when it is full, or when a new upload starts).

use std::io;

use super::gbl::{self, Checker, GblError, GblInfo};
use super::ImageSource;
use crate::store::Flash;

const MAGIC: [u8; 4] = *b"ZGBL";
const FORMAT: u8 = 1;
const RECORD: usize = 36;

/// State of a bank's image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BankState {
    /// Uploaded and checked, never verified on the NCP.
    Staged,
    /// Ran on the NCP with a verified session.
    Good,
    /// Failed on the NCP (or could not be installed).
    Bad,
}

impl BankState {
    fn code(self) -> u8 {
        match self {
            BankState::Staged => 1,
            BankState::Good => 2,
            BankState::Bad => 3,
        }
    }
    fn from_code(c: u8) -> Option<Self> {
        match c {
            1 => Some(BankState::Staged),
            2 => Some(BankState::Good),
            3 => Some(BankState::Bad),
            _ => None,
        }
    }
}

/// One bank's description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// State.
    pub state: BankState,
    /// Generation; the good bank with the highest `seq` is the
    /// last-known-good one.
    pub seq: u32,
    /// Image length.
    pub len: u32,
    /// The image's end-tag CRC-32.
    pub crc32: u32,
    /// GBL application version.
    pub app_version: Option<u32>,
    /// zigbee-rs build ID from the GBL metadata.
    pub build_id: Option<[u8; 8]>,
}

impl Slot {
    fn encode(&self) -> [u8; RECORD] {
        let mut r = [0u8; RECORD];
        r[..4].copy_from_slice(&MAGIC);
        r[4] = FORMAT;
        r[5] = self.state.code();
        r[8..12].copy_from_slice(&self.seq.to_le_bytes());
        r[12..16].copy_from_slice(&self.len.to_le_bytes());
        r[16..20].copy_from_slice(&self.crc32.to_le_bytes());
        r[20..24].copy_from_slice(&self.app_version.unwrap_or(u32::MAX).to_le_bytes());
        r[24..32].copy_from_slice(&self.build_id.unwrap_or([0xFF; 8]));
        let crc = gbl::Crc32::of(&r[..32]);
        r[32..].copy_from_slice(&crc.to_le_bytes());
        r
    }

    fn decode(r: &[u8]) -> Option<Self> {
        let u = |o: usize| u32::from_le_bytes([r[o], r[o + 1], r[o + 2], r[o + 3]]);
        if r[..4] != MAGIC || r[4] != FORMAT || gbl::Crc32::of(&r[..32]) != u(32) {
            return None;
        }
        let build_id: [u8; 8] = r[24..32].try_into().ok()?;
        Some(Self {
            state: BankState::from_code(r[5])?,
            seq: u(8),
            len: u(12),
            crc32: u(16),
            app_version: Some(u(20)).filter(|&v| v != u32::MAX),
            build_id: Some(build_id).filter(|b| *b != [0xFF; 8]),
        })
    }

    fn from_info(info: &GblInfo, state: BankState, seq: u32) -> Self {
        Self {
            state,
            seq,
            len: info.size,
            crc32: info.crc32,
            app_version: info.app.map(|a| a.version),
            build_id: info.zrhp.as_ref().map(|m| m.build_id),
        }
    }
}

/// Bank errors.
#[derive(Debug)]
pub enum BankError {
    /// Flash error.
    Io(io::Error),
    /// The image does not fit a bank.
    TooLarge {
        /// Image size.
        size: u32,
        /// Bank capacity.
        capacity: u32,
    },
    /// Fewer or more bytes than announced.
    Length {
        /// Announced.
        expected: u32,
        /// Written.
        written: u32,
    },
    /// Not a valid GBL.
    Image(GblError),
    /// The read-back differs from what was written.
    Verify,
    /// The bank is empty.
    Empty(u8),
}

impl From<io::Error> for BankError {
    fn from(e: io::Error) -> Self {
        BankError::Io(e)
    }
}

impl std::fmt::Display for BankError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BankError::Io(e) => write!(f, "radio bank flash: {e}"),
            BankError::TooLarge { size, capacity } => write!(f, "image of {size} B exceeds the {capacity} B radio bank"),
            BankError::Length { expected, written } => write!(f, "expected {expected} B, got {written} B"),
            BankError::Image(e) => write!(f, "{e}"),
            BankError::Verify => write!(f, "radio bank read-back mismatch"),
            BankError::Empty(i) => write!(f, "radio bank {} is empty", bank_name(*i)),
        }
    }
}

/// `A` or `B`.
pub fn bank_name(i: u8) -> char {
    if i == 0 { 'A' } else { 'B' }
}

/// The two banks on a flash region (`hubdata16::RADIO`).
pub struct Banks<F: Flash> {
    flash: F,
    bank: u32,
    sector: u32,
}

impl<F: Flash> Banks<F> {
    /// Banks on all of `flash` (split in two halves).
    pub fn new(flash: F) -> Self {
        let sector = flash.sector_size();
        let bank = flash.sectors() / 2 * sector;
        Self { flash, bank, sector }
    }

    /// Largest image.
    pub fn capacity(&self) -> u32 {
        self.bank - self.sector
    }

    fn base(&self, i: u8) -> u32 {
        u32::from(i) * self.bank
    }

    /// Scan a header sector: the last valid record and the next free offset.
    fn scan(&mut self, i: u8) -> io::Result<(Option<Slot>, Option<u32>)> {
        let mut sector = vec![0u8; self.sector as usize];
        self.flash.read(self.base(i), &mut sector)?;
        let mut last = None;
        for (n, r) in sector.chunks_exact(RECORD).enumerate() {
            if r.iter().all(|&b| b == 0xFF) {
                return Ok((last, Some((n * RECORD) as u32)));
            }
            if let Some(s) = Slot::decode(r) {
                last = Some(s);
            }
        }
        Ok((last, None))
    }

    /// One bank.
    pub fn slot(&mut self, i: u8) -> io::Result<Option<Slot>> {
        Ok(self.scan(i)?.0)
    }

    /// Both banks.
    pub fn slots(&mut self) -> io::Result<[Option<Slot>; 2]> {
        Ok([self.slot(0)?, self.slot(1)?])
    }

    /// The good bank with the highest generation.
    pub fn last_known_good(&mut self) -> io::Result<Option<(u8, Slot)>> {
        let s = self.slots()?;
        Ok((0..2u8)
            .filter_map(|i| s[i as usize].filter(|s| s.state == BankState::Good).map(|s| (i, s)))
            .max_by_key(|(_, s)| s.seq))
    }

    /// The bank an upload goes to: never the last-known-good one.
    pub fn upload_target(&mut self) -> io::Result<u8> {
        Ok(match self.last_known_good()? {
            Some((i, _)) => 1 - i,
            None => 0,
        })
    }

    fn append(&mut self, i: u8, slot: Slot) -> io::Result<()> {
        let off = match self.scan(i)?.1 {
            Some(off) => off,
            None => {
                self.flash.erase(self.base(i) / self.sector)?;
                0
            }
        };
        self.flash.write(self.base(i) + off, &slot.encode())
    }

    /// Record a new state for bank `i`.
    pub fn set_state(&mut self, i: u8, state: BankState) -> Result<Slot, BankError> {
        let mut slot = self.slot(i)?.ok_or(BankError::Empty(i))?;
        slot.state = state;
        self.append(i, slot)?;
        Ok(slot)
    }

    /// Mark bank `i` good with the next generation: it becomes the
    /// last-known-good image.
    pub fn promote(&mut self, i: u8) -> Result<Slot, BankError> {
        let other = self.slot(1 - i)?.map_or(0, |s| s.seq);
        let mut slot = self.slot(i)?.ok_or(BankError::Empty(i))?;
        slot.state = BankState::Good;
        slot.seq = slot.seq.max(other) + 1;
        self.append(i, slot)?;
        Ok(slot)
    }

    /// Start an upload of `len` bytes into [`Banks::upload_target`]. The
    /// target's header sector is erased first, so an interrupted upload
    /// leaves an empty bank.
    pub fn writer(&mut self, len: u32) -> Result<Writer<'_, F>, BankError> {
        if len > self.capacity() {
            return Err(BankError::TooLarge { size: len, capacity: self.capacity() });
        }
        let i = self.upload_target()?;
        self.flash.erase(self.base(i) / self.sector)?;
        let first = (self.base(i) + self.sector) / self.sector;
        Ok(Writer { banks: self, bank: i, expected: len, written: 0, erased_to: first, checker: Checker::new() })
    }

    /// Read access to bank `i`'s image.
    pub fn image(&mut self, i: u8) -> Result<BankImage<'_, F>, BankError> {
        let slot = self.slot(i)?.ok_or(BankError::Empty(i))?;
        let base = self.base(i) + self.sector;
        Ok(BankImage { flash: &mut self.flash, base, len: slot.len })
    }

    /// Check bank `i`'s image against its record (CRC of the whole file).
    pub fn verify(&mut self, i: u8) -> Result<GblInfo, BankError> {
        let slot = self.slot(i)?.ok_or(BankError::Empty(i))?;
        let info = check_source(&mut self.image(i)?)?;
        if info.crc32 != slot.crc32 || info.size != slot.len {
            return Err(BankError::Verify);
        }
        Ok(info)
    }
}

/// Stream an image source through the GBL checker.
pub fn check_source(src: &mut dyn ImageSource) -> Result<GblInfo, BankError> {
    let mut c = Checker::new();
    let mut buf = [0u8; 512];
    let len = src.size();
    let mut off = 0;
    while off < len {
        let n = (len - off).min(buf.len() as u32) as usize;
        src.read_at(off, &mut buf[..n])?;
        c.feed(&buf[..n]);
        off += n as u32;
    }
    c.finish().map_err(BankError::Image)
}

/// Sequential upload into one bank.
pub struct Writer<'a, F: Flash> {
    banks: &'a mut Banks<F>,
    bank: u8,
    expected: u32,
    written: u32,
    erased_to: u32,
    checker: Checker,
}

impl<F: Flash> Writer<'_, F> {
    /// The bank being written.
    pub fn bank(&self) -> u8 {
        self.bank
    }

    /// Append bytes.
    pub fn write(&mut self, data: &[u8]) -> Result<(), BankError> {
        let end = self.written + data.len() as u32;
        if end > self.expected {
            return Err(BankError::Length { expected: self.expected, written: end });
        }
        let b = &mut *self.banks;
        let start = b.base(self.bank) + b.sector + self.written;
        while self.erased_to * b.sector < start + data.len() as u32 {
            b.flash.erase(self.erased_to)?;
            self.erased_to += 1;
        }
        b.flash.write(start, data)?;
        self.checker.feed(data);
        self.written = end;
        Ok(())
    }

    /// Check the image, verify the flash contents and record it as
    /// [`BankState::Staged`].
    pub fn finish(self) -> Result<(u8, Slot, GblInfo), BankError> {
        if self.written != self.expected {
            return Err(BankError::Length { expected: self.expected, written: self.written });
        }
        let info = self.checker.finish().map_err(BankError::Image)?;
        let i = self.bank;
        let b = self.banks;
        let base = b.base(i) + b.sector;
        let back = check_source(&mut BankImage { flash: &mut b.flash, base, len: info.size })?;
        if back.crc32 != info.crc32 {
            return Err(BankError::Verify);
        }
        let slot = Slot::from_info(&info, BankState::Staged, 0);
        b.append(i, slot)?;
        Ok((i, slot, info))
    }
}

/// One bank's image as an [`ImageSource`].
pub struct BankImage<'a, F: Flash> {
    flash: &'a mut F,
    base: u32,
    len: u32,
}

impl<F: Flash> ImageSource for BankImage<'_, F> {
    fn size(&self) -> u32 {
        self.len
    }
    fn read_at(&mut self, offset: u32, buf: &mut [u8]) -> io::Result<()> {
        if offset + buf.len() as u32 > self.len {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        self.flash.read(self.base + offset, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemFlash;

    fn banks() -> Banks<MemFlash> {
        // 2 × 64 KiB banks.
        Banks::new(MemFlash::new(32, 4096))
    }

    fn upload(b: &mut Banks<MemFlash>, image: &[u8]) -> Result<(u8, Slot, GblInfo), BankError> {
        let mut w = b.writer(image.len() as u32)?;
        for part in image.chunks(1000) {
            w.write(part)?;
        }
        w.finish()
    }

    #[test]
    fn uploads_alternate_around_the_last_known_good_bank() {
        let mut b = banks();
        let v1 = gbl::sample(1, *b"build-v1", 20_000);
        let (i, slot, _) = upload(&mut b, &v1).unwrap();
        assert_eq!((i, slot.state, slot.app_version, slot.build_id), (0, BankState::Staged, Some(1), Some(*b"build-v1")));
        assert_eq!(b.last_known_good().unwrap(), None);
        // A second upload before verification replaces the staged image.
        let (i, ..) = upload(&mut b, &v1).unwrap();
        assert_eq!(i, 0);
        b.promote(0).unwrap();
        assert_eq!(b.last_known_good().unwrap().map(|(i, s)| (i, s.seq)), Some((0, 1)));

        let v2 = gbl::sample(2, *b"build-v2", 30_000);
        let (i, ..) = upload(&mut b, &v2).unwrap();
        assert_eq!(i, 1);
        // Failure: v2 bad, v1 stays the rollback image.
        b.set_state(1, BankState::Bad).unwrap();
        assert_eq!(b.last_known_good().unwrap().map(|(i, _)| i), Some(0));
        let mut img = Vec::new();
        let mut src = b.image(0).unwrap();
        img.resize(src.size() as usize, 0);
        src.read_at(0, &mut img).unwrap();
        assert_eq!(img, v1);
        assert_eq!(b.verify(0).unwrap().app.map(|a| a.version), Some(1));

        // Success of a later upload: it becomes last-known-good, the older
        // bank is the next upload target.
        let (i, ..) = upload(&mut b, &v2).unwrap();
        assert_eq!(i, 1);
        b.promote(1).unwrap();
        assert_eq!(b.last_known_good().unwrap().map(|(i, s)| (i, s.seq)), Some((1, 2)));
        assert_eq!(b.upload_target().unwrap(), 0);
    }

    #[test]
    fn rejects_bad_uploads_and_survives_many_state_changes() {
        let mut b = banks();
        let v1 = gbl::sample(1, [1; 8], 1000);
        let mut bad = v1.clone();
        bad[50] ^= 0x40;
        assert!(matches!(upload(&mut b, &bad), Err(BankError::Image(GblError::Crc { .. }))));
        assert_eq!(b.slot(0).unwrap(), None);
        assert!(matches!(b.writer(64 * 1024), Err(BankError::TooLarge { .. })));
        let mut w = b.writer(v1.len() as u32 + 1).unwrap();
        w.write(&v1).unwrap();
        assert!(matches!(w.finish(), Err(BankError::Length { .. })));

        upload(&mut b, &v1).unwrap();
        // More records than fit one header sector.
        for n in 0..300 {
            b.set_state(0, if n % 2 == 0 { BankState::Bad } else { BankState::Staged }).unwrap();
        }
        b.promote(0).unwrap();
        assert_eq!(b.slot(0).unwrap().map(|s| s.state), Some(BankState::Good));
        assert_eq!(b.verify(0).unwrap().size, v1.len() as u32);
    }
}
