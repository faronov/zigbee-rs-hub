//! Log-structured host store journal (spec 3.4, 8.8).
//!
//! The region is a ring of sectors (128 KiB = 32 × 4 KiB in `hubdata`).
//! Every sector starts with a header `{magic "ZHJ1", u32 seq, u32 reserved,
//! u32 crc32c}`; records follow back to back:
//!
//! ```text
//! u16 len | u8 type | u8 commit | payload[len] | u32 crc32c(len, type, payload) | pad to 4
//! ```
//!
//! A record is programmed with `commit = 0xFF`, read back and checked, and
//! only then committed by clearing the commit byte to `0x00` (a NOR write
//! can only clear bits). A power cut leaves either no record, an
//! uncommitted one or one with a bad CRC; recovery ignores it and the next
//! append opens a fresh sector.
//!
//! State = the newest complete snapshot (`SNAP_BEGIN{snap_gen}` … `SNAP_END{snap_gen,
//! count}`) plus the delta records after it. When more than half of the ring
//! is live, a new snapshot is written into fresh sectors; the previous one
//! stays valid until the new `SNAP_END` is committed, and only sectors older
//! than the newest complete snapshot are erased (lazily, when reused). This
//! is the two-generation snapshot + append discipline of the NCP journals.

use std::io;

use zigbee_zrhp::crc::crc32c;

use super::flash::Flash;
use super::state::{Change, HostState, rec};

const MAGIC: u32 = u32::from_le_bytes(*b"ZHJ1");
const SECTOR_HEADER: u32 = 16;
const RECORD_OVERHEAD: u32 = 8;
const COMMITTED: u8 = 0x00;

impl Flash for Box<dyn Flash> {
    fn sector_size(&self) -> u32 {
        (**self).sector_size()
    }
    fn sectors(&self) -> u32 {
        (**self).sectors()
    }
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> io::Result<()> {
        (**self).read(offset, buf)
    }
    fn write(&mut self, offset: u32, data: &[u8]) -> io::Result<()> {
        (**self).write(offset, data)
    }
    fn erase(&mut self, sector: u32) -> io::Result<()> {
        (**self).erase(sector)
    }
}

/// Journal occupancy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalStats {
    /// Sectors in the ring.
    pub sectors: u32,
    /// Sectors from the newest complete snapshot to the head.
    pub live_sectors: u32,
    /// Snapshot generation.
    pub generation: u32,
    /// Records appended since open.
    pub appended: u32,
    /// Torn or corrupt records skipped at open.
    pub torn: u32,
}

/// The journal and the state it holds.
pub struct Journal<F: Flash> {
    flash: F,
    n: u32,
    size: u32,
    seqs: Vec<Option<u32>>,
    head: Option<(u32, u32)>,
    head_seq: u32,
    live_from: u32,
    generation: u32,
    state: HostState,
    appended: u32,
    torn: u32,
}

fn pad4(n: u32) -> u32 {
    (n + 3) & !3
}

fn header_bytes(seq: u32) -> [u8; 16] {
    let mut h = [0xFFu8; 16];
    h[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    h[4..8].copy_from_slice(&seq.to_le_bytes());
    let crc = crc32c(&h[0..12]);
    h[12..16].copy_from_slice(&crc.to_le_bytes());
    h
}

fn record_crc(len: u16, t: u8, payload: &[u8]) -> u32 {
    let mut c = zigbee_zrhp::crc::Crc32c::new();
    c.update(&len.to_le_bytes());
    c.update_byte(t);
    c.update(payload);
    c.finish()
}

impl<F: Flash> Journal<F> {
    /// Open (and recover) the journal in `flash`.
    pub fn open(mut flash: F) -> io::Result<Self> {
        let n = flash.sectors();
        let size = flash.sector_size();
        if n < 4 || size < 256 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "journal needs ≥ 4 sectors of ≥ 256 B"));
        }
        let mut seqs = vec![None; n as usize];
        for (i, slot) in seqs.iter_mut().enumerate() {
            let mut h = [0u8; 16];
            flash.read(i as u32 * size, &mut h)?;
            let magic = u32::from_le_bytes(h[0..4].try_into().unwrap());
            let crc = u32::from_le_bytes(h[12..16].try_into().unwrap());
            if magic == MAGIC && crc == crc32c(&h[0..12]) {
                *slot = Some(u32::from_le_bytes(h[4..8].try_into().unwrap()));
            }
        }
        let mut j = Self {
            flash,
            n,
            size,
            seqs,
            head: None,
            head_seq: 0,
            live_from: 0,
            generation: 0,
            state: HostState::default(),
            appended: 0,
            torn: 0,
        };
        j.recover()?;
        Ok(j)
    }

    fn recover(&mut self) -> io::Result<()> {
        let mut order: Vec<(u32, u32)> =
            self.seqs.iter().enumerate().filter_map(|(i, s)| s.map(|s| (s, i as u32))).collect();
        order.sort_unstable();
        // A ring without a snapshot is valid only from its oldest sector on.
        self.live_from = order.first().map_or(0, |(s, _)| *s);
        let mut current = HostState::default();
        let mut building: Option<(u32, HostState, u32, u32)> = None;
        for (seq, sector) in order.iter().copied() {
            let base = sector * self.size;
            let mut off = SECTOR_HEADER;
            let mut open_at = None;
            loop {
                if off + RECORD_OVERHEAD > self.size {
                    break;
                }
                let mut h = [0u8; 4];
                self.flash.read(base + off, &mut h)?;
                let len = u16::from_le_bytes([h[0], h[1]]);
                if len == 0xFFFF && h[2] == 0xFF && h[3] == 0xFF {
                    open_at = Some(off);
                    break;
                }
                let total = pad4(RECORD_OVERHEAD + u32::from(len));
                if off + total > self.size {
                    self.torn += 1;
                    break;
                }
                let mut body = vec![0u8; len as usize + 4];
                self.flash.read(base + off + 4, &mut body)?;
                let (payload, crc) = body.split_at(len as usize);
                let crc = u32::from_le_bytes(crc.try_into().unwrap());
                if h[3] != COMMITTED || crc != record_crc(len, h[2], payload) {
                    self.torn += 1;
                    break;
                }
                let mut r = payload;
                let u32_at = |r: &mut &[u8]| -> Option<u32> {
                    let (a, b) = r.split_at_checked(4)?;
                    *r = b;
                    Some(u32::from_le_bytes(a.try_into().unwrap()))
                };
                match h[2] {
                    rec::SNAP_BEGIN => {
                        if let Some(snap_gen) = u32_at(&mut r) {
                            building = Some((snap_gen, HostState::default(), 0, seq));
                        }
                    }
                    rec::SNAP_END => {
                        let snap_gen = u32_at(&mut r);
                        let count = u32_at(&mut r);
                        if let Some((bgen, state, bcount, bseq)) = building.take()
                            && Some(bgen) == snap_gen && Some(bcount) == count {
                                current = state;
                                self.live_from = bseq;
                                self.generation = bgen;
                            }
                    }
                    t => {
                        if let Some(change) = Change::decode(t, payload) {
                            match building.as_mut() {
                                Some((_, state, count, _)) => {
                                    state.apply(&change);
                                    *count += 1;
                                }
                                None => current.apply(&change),
                            }
                        }
                    }
                }
                off += total;
            }
            self.head_seq = seq;
            self.head = open_at.map(|o| (sector, o));
        }
        self.state = current;
        if order.is_empty() {
            self.open_sector(0, 1)?;
            self.live_from = 1;
        }
        Ok(())
    }

    fn open_sector(&mut self, sector: u32, seq: u32) -> io::Result<()> {
        self.flash.erase(sector)?;
        self.flash.write(sector * self.size, &header_bytes(seq))?;
        self.seqs[sector as usize] = Some(seq);
        self.head_seq = seq;
        self.head = Some((sector, SECTOR_HEADER));
        Ok(())
    }

    fn next_sector(&mut self) -> io::Result<()> {
        let cur = self
            .seqs
            .iter()
            .position(|s| *s == Some(self.head_seq))
            .map_or(0, |p| p as u32);
        let next = (cur + 1) % self.n;
        if let Some(s) = self.seqs[next as usize]
            && s >= self.live_from {
                return Err(io::Error::other("host store journal full"));
            }
        self.open_sector(next, self.head_seq + 1)
    }

    fn write_record(&mut self, t: u8, payload: &[u8]) -> io::Result<()> {
        let len = u16::try_from(payload.len()).map_err(|_| io::ErrorKind::InvalidInput)?;
        let total = pad4(RECORD_OVERHEAD + u32::from(len));
        if total > self.size - SECTOR_HEADER {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "journal record larger than a sector"));
        }
        match self.head {
            Some((_, off)) if off + total <= self.size => {}
            _ => self.next_sector()?,
        }
        let (sector, off) = self.head.unwrap();
        let at = sector * self.size + off;
        let mut bytes = Vec::with_capacity(total as usize);
        bytes.extend_from_slice(&len.to_le_bytes());
        bytes.push(t);
        bytes.push(0xFF);
        bytes.extend_from_slice(payload);
        bytes.extend_from_slice(&record_crc(len, t, payload).to_le_bytes());
        bytes.resize(total as usize, 0xFF);
        // Whatever happens below, the space is used: never write it twice.
        self.head = if off + total + RECORD_OVERHEAD <= self.size { Some((sector, off + total)) } else { None };
        let result = (|| {
            self.flash.write(at, &bytes)?;
            let mut back = vec![0u8; bytes.len()];
            self.flash.read(at, &mut back)?;
            if back != bytes {
                return Err(io::Error::other("journal read-back mismatch"));
            }
            self.flash.write(at + 3, &[COMMITTED])?;
            let mut c = [0u8; 1];
            self.flash.read(at + 3, &mut c)?;
            if c[0] != COMMITTED {
                return Err(io::Error::other("journal commit marker not written"));
            }
            Ok(())
        })();
        if result.is_err() {
            self.head = None;
        }
        result
    }

    /// Durably append one change, then apply it.
    pub fn commit(&mut self, change: Change) -> io::Result<()> {
        let (t, payload) = change.encode();
        self.write_record(t, &payload)?;
        self.state.apply(&change);
        self.appended += 1;
        if self.head_seq + 1 - self.live_from > self.n / 2 {
            self.compact()?;
        }
        Ok(())
    }

    /// Write a new snapshot generation into fresh sectors.
    pub fn compact(&mut self) -> io::Result<()> {
        let snap_gen = self.generation.wrapping_add(1);
        self.next_sector()?;
        let begin = self.head_seq;
        self.write_record(rec::SNAP_BEGIN, &snap_gen.to_le_bytes())?;
        let records = self.state.snapshot();
        for change in &records {
            let (t, payload) = change.encode();
            self.write_record(t, &payload)?;
        }
        let mut end = snap_gen.to_le_bytes().to_vec();
        end.extend_from_slice(&(records.len() as u32).to_le_bytes());
        self.write_record(rec::SNAP_END, &end)?;
        self.live_from = begin;
        self.generation = snap_gen;
        Ok(())
    }

    /// Current state.
    pub fn state(&self) -> &HostState {
        &self.state
    }

    /// Occupancy.
    pub fn stats(&self) -> JournalStats {
        JournalStats {
            sectors: self.n,
            live_sectors: self.head_seq + 1 - self.live_from,
            generation: self.generation,
            appended: self.appended,
            torn: self.torn,
        }
    }

    /// The underlying flash.
    pub fn into_flash(self) -> F {
        self.flash
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::flash::MemFlash;
    use crate::store::state::{DeviceRecord, FcRecord, Meta};

    fn dev(i: u8) -> DeviceRecord {
        DeviceRecord { ieee: [i; 8], nwk: u16::from(i) << 8, capability: 0x8E, flags: 0, last_seen: u32::from(i) }
    }

    #[test]
    fn survives_reopen_and_compaction() {
        let mut j = Journal::open(MemFlash::new(8, 512)).unwrap();
        for round in 0..120u32 {
            j.commit(Change::Fc(1, FcRecord { key_seq: 0, limit: round * 1024, generation: round })).unwrap();
            j.commit(Change::Device(dev(round as u8 % 7))).unwrap();
        }
        assert!(j.stats().generation > 1, "compacted: {:?}", j.stats());
        let want = j.state().clone();
        let j = Journal::open(j.into_flash()).unwrap();
        assert_eq!(j.state(), &want);
        assert_eq!(j.state().fc[0].unwrap().limit, 119 * 1024);
        assert_eq!(j.state().devices.len(), 7);
    }

    #[test]
    fn power_cut_at_every_byte_keeps_a_prefix() {
        // Reference: the state after each successful commit.
        let changes: Vec<Change> = (0..30u32)
            .map(|i| match i % 3 {
                0 => Change::Device(dev(i as u8)),
                1 => Change::Fc(2, FcRecord { key_seq: 1, limit: i, generation: i }),
                _ => Change::Meta(Meta { table_epoch: i, ..Meta::default() }),
            })
            .collect();
        let mut states = vec![HostState::default()];
        for c in &changes {
            let mut s = states.last().unwrap().clone();
            s.apply(c);
            states.push(s);
        }
        let mut probe = MemFlash::new(6, 256);
        probe.budget = Some(usize::MAX);
        for budget in (0..4000).step_by(7) {
            let mut flash = MemFlash::new(6, 256);
            flash.budget = Some(budget);
            let mut done = 0;
            if let Ok(mut j) = Journal::open(flash.clone()) {
                for c in &changes {
                    if j.commit(c.clone()).is_err() {
                        break;
                    }
                    done += 1;
                }
                flash = j.into_flash();
            }
            flash.budget = None;
            let j = Journal::open(flash).unwrap();
            // Either the last commit landed (marker written just before the
            // cut was noticed) or it did not: never anything else.
            let got = j.state();
            assert!(
                got == &states[done] || got == &states[(done + 1).min(changes.len())],
                "budget {budget}: {done} commits"
            );
            if done == changes.len() {
                break;
            }
        }
    }

    #[test]
    fn appends_continue_after_a_torn_record() {
        let mut flash = MemFlash::new(8, 512);
        flash.budget = Some(16 + 20);
        let mut j = Journal::open(flash).unwrap();
        assert!(j.commit(Change::Device(dev(1))).is_err());
        let mut flash = j.into_flash();
        flash.budget = None;
        let mut j = Journal::open(flash).unwrap();
        assert!(j.state().devices.is_empty());
        assert_eq!(j.stats().torn, 1);
        j.commit(Change::Device(dev(2))).unwrap();
        let j = Journal::open(j.into_flash()).unwrap();
        assert_eq!(j.state().devices.keys().copied().collect::<Vec<_>>(), vec![[2; 8]]);
    }
}
