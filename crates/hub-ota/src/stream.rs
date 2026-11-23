//! Images streamed on demand from an API client: the hub stores nothing.
//!
//! A client (the Home Assistant integration, a browser tab) that holds an
//! OTA file offers it with its first bytes (`ota/stream/offer`): the hub
//! checks the header and offers the image to devices like a stored one.
//! When a device asks for bytes the hub does not have, it answers
//! WAIT_FOR_DATA and sends the owning client `ota/block_needed {session,
//! offset, len}`; the client answers with `ota/block_data`.
//!
//! Memory is fixed per stream: a ring of [`SLOTS`] × [`CHUNK`] bytes,
//! allocated when the first chunk arrives, into which `ota/block_data` is
//! decoded straight from the request text ([`Streams::data_base64`]); at
//! most one chunk is asked for at a time (plus one the device needs while
//! a prefetch is on its way), [`AHEAD`] chunks ahead of the device. While
//! the heap is low ([`Streams::set_low_heap`]) nothing is asked for and
//! devices get WAIT_FOR_DATA.
//!
//! When the client disappears its streams wait [`GRACE_MS`] for a client
//! to offer the same image again (Home Assistant reconnecting after a hub
//! restart); devices get WAIT_FOR_DATA meanwhile, then ABORT (a device
//! keeps what it has and continues when the image is offered again).
//! The file's sub-elements and SHA are checked by the client.

use crate::file::Header;
use crate::images::{Fetch, Offer};
use crate::json::Obj;
use crate::server::{Ident, Millis};

/// The first stream ID (store images use 1..=0xFFFF).
pub const FIRST_ID: u32 = 0x1_0000;
/// Bytes a client sends per `ota/block_data` (about 1.4 KB of request
/// text: small next to the free heap of an S3 without PSRAM).
pub const CHUNK: u32 = 1024;
/// Chunks a stream's ring holds.
pub const SLOTS: usize = 4;
/// Chunks fetched ahead of the one the device reads.
pub const AHEAD: u32 = 2;
/// Streams at once.
pub const MAX_STREAMS: usize = 4;
/// A chunk asked for and not delivered is asked for again after this.
const REASK_MS: Millis = 5_000;
/// A stream without a client waits this long for one.
pub const GRACE_MS: Millis = 120_000;

/// One streamed image.
#[derive(Debug)]
pub struct Stream {
    /// What is offered.
    pub offer: Offer,
    /// The API session serving it (`None`: gone, in its grace time).
    pub owner: Option<u64>,
    /// The header bytes the client offered (checked against chunk 0).
    head: Vec<u8>,
    /// `SLOTS × CHUNK` bytes once the first chunk came.
    ring: Option<Box<[u8]>>,
    /// The chunk offset each ring slot holds.
    slots: [Option<u32>; SLOTS],
    pending: Option<(u32, Millis)>,
    orphaned: Option<Millis>,
    last_read: u32,
    /// Bytes received from the client.
    pub received: u64,
    /// Chunks asked for.
    pub asks: u32,
    /// Block reads answered with WAIT_FOR_DATA.
    pub waits: u32,
}

impl Stream {
    fn len_at(&self, offset: u32) -> u32 {
        CHUNK.min(self.offer.size.saturating_sub(offset))
    }

    fn slot_of(&self, chunk: u32) -> Option<usize> {
        self.slots.iter().position(|s| *s == Some(chunk))
    }

    /// The slot a chunk at `offset` goes to: its own, a free one, one
    /// wholly behind the device, else the farthest ahead.
    fn slot_for(&self, offset: u32) -> usize {
        if let Some(i) = self.slot_of(offset) {
            return i;
        }
        if let Some(i) = self.slots.iter().position(Option::is_none) {
            return i;
        }
        let r = self.last_read / CHUNK * CHUNK;
        if let Some(i) = self.slots.iter().position(|s| s.is_some_and(|c| c < r)) {
            return i;
        }
        (0..SLOTS).max_by_key(|&i| self.slots[i]).unwrap_or(0)
    }

    /// Bytes this stream holds.
    pub fn memory(&self) -> usize {
        self.ring.as_ref().map_or(0, |r| r.len()) + self.head.len()
    }
}

/// The streams.
#[derive(Debug, Default)]
pub struct Streams {
    list: Vec<Stream>,
    next: u32,
    low_heap: bool,
    /// Frames for API sessions: `(session, text)`.
    pub outbox: Vec<(u64, String)>,
}

impl Streams {
    /// An offer by session `owner`: the file's first bytes (`head`, at
    /// least its header), its size and name. The same image offered again
    /// (a client reconnecting) keeps its ID.
    #[allow(clippy::too_many_arguments)]
    pub fn offer(
        &mut self,
        owner: u64,
        head: &[u8],
        size: u32,
        name: &str,
        min_file_version: Option<u32>,
        max_file_version: Option<u32>,
    ) -> Result<u32, String> {
        let header = Header::parse(head).map_err(|e| e.to_string())?;
        if header.total_size != size {
            return Err(format!(
                "the header's total image size is {} B, the offer {size} B",
                header.total_size
            ));
        }
        let hl = usize::from(header.header_len).min(head.len());
        let ident = (header.manufacturer, header.image_type, header.file_version);
        if let Some(s) = self
            .list
            .iter_mut()
            .find(|s| s.offer.ident() == ident && s.offer.size == size)
        {
            s.owner = Some(owner);
            s.orphaned = None;
            s.pending = None;
            return Ok(s.offer.id);
        }
        if self.list.len() >= MAX_STREAMS {
            return Err(format!("at most {MAX_STREAMS} streamed images at once"));
        }
        let id = FIRST_ID + self.next % 0xFFFF;
        self.next = self.next.wrapping_add(1);
        self.list.push(Stream {
            offer: Offer {
                id,
                header,
                size,
                min_file_version,
                max_file_version,
                name: name.chars().take(48).collect(),
                streamed: true,
            },
            owner: Some(owner),
            head: head[..hl].to_vec(),
            ring: None,
            slots: [None; SLOTS],
            pending: None,
            orphaned: None,
            last_read: 0,
            received: 0,
            asks: 0,
            waits: 0,
        });
        Ok(id)
    }

    /// While set, no chunk is asked for (devices get WAIT_FOR_DATA).
    pub fn set_low_heap(&mut self, low: bool) {
        self.low_heap = low;
    }

    /// Whether asking is paused for the heap.
    pub fn low_heap(&self) -> bool {
        self.low_heap
    }

    /// `ota/block_data` with the bytes.
    pub fn data(&mut self, owner: u64, id: u32, offset: u32, bytes: &[u8]) -> Result<(), String> {
        self.accept(owner, id, offset, bytes.len(), |dst| {
            dst.copy_from_slice(bytes);
            Ok(())
        })
    }

    /// `ota/block_data` with the base64 text of the request: decoded into
    /// the ring, nothing allocated (but the ring the first time).
    pub fn data_base64(
        &mut self,
        owner: u64,
        id: u32,
        offset: u32,
        b64: &str,
    ) -> Result<(), String> {
        let n = crate::json::base64_len(b64)?;
        self.accept(owner, id, offset, n, |dst| {
            crate::json::base64_decode_into(b64, dst)
                .map_err(String::from)
                .and_then(|k| {
                    (k == dst.len())
                        .then_some(())
                        .ok_or_else(|| "base64: wrong length".into())
                })
        })
    }

    fn accept(
        &mut self,
        owner: u64,
        id: u32,
        offset: u32,
        n: usize,
        fill: impl FnOnce(&mut [u8]) -> Result<(), String>,
    ) -> Result<(), String> {
        let i = self
            .list
            .iter()
            .position(|s| s.offer.id == id && s.owner == Some(owner))
            .ok_or("no such stream of this session")?;
        let s = &mut self.list[i];
        if !offset.is_multiple_of(CHUNK) || offset >= s.offer.size || n != s.len_at(offset) as usize
        {
            return Err(format!(
                "chunks are {CHUNK} B at multiples of {CHUNK} (the last one shorter)"
            ));
        }
        if s.ring.is_none() {
            let mut v = Vec::new();
            v.try_reserve_exact(SLOTS * CHUNK as usize)
                .map_err(|_| "no memory for the stream window")?;
            v.resize(SLOTS * CHUNK as usize, 0);
            s.ring = Some(v.into_boxed_slice());
        }
        let k = s.slot_for(offset);
        s.slots[k] = None;
        let at = k * CHUNK as usize;
        let ring = s.ring.as_mut().unwrap();
        fill(&mut ring[at..at + n])?;
        if offset == 0 && !ring[at..at + n].starts_with(&s.head) {
            self.list.remove(i);
            return Err("the file does not start with the offered header; stream closed".into());
        }
        s.slots[k] = Some(offset);
        if s.pending.is_some_and(|p| p.0 == offset) {
            s.pending = None;
        }
        s.received += n as u64;
        Ok(())
    }

    /// `ota/stream/close`.
    pub fn close(&mut self, owner: u64, id: u32) -> bool {
        let n = self.list.len();
        self.list
            .retain(|s| !(s.offer.id == id && s.owner == Some(owner)));
        self.list.len() != n
    }

    /// Session `owner` went away: its streams wait for another client.
    pub fn disconnected(&mut self, owner: u64, now: Millis) {
        for s in self.list.iter_mut().filter(|s| s.owner == Some(owner)) {
            s.owner = None;
            s.orphaned = Some(now);
            s.pending = None;
        }
    }

    /// Drops streams whose grace time ended; returns their offers.
    pub fn poll(&mut self, now: Millis) -> Vec<Offer> {
        let mut gone = Vec::new();
        self.list.retain(|s| {
            let keep = s.orphaned.is_none_or(|t| now.saturating_sub(t) < GRACE_MS);
            if !keep {
                gone.push(s.offer.clone());
            }
            keep
        });
        gone
    }

    /// The streams.
    pub fn list(&self) -> &[Stream] {
        &self.list
    }

    /// The offers.
    pub fn offers(&self) -> Vec<Offer> {
        self.list.iter().map(|s| s.offer.clone()).collect()
    }

    /// The offer with this identity (no list built).
    pub fn find(&self, ident: Ident) -> Option<Offer> {
        self.list
            .iter()
            .find(|s| s.offer.ident() == ident)
            .map(|s| s.offer.clone())
    }

    /// Bytes the streams hold.
    pub fn memory(&self) -> usize {
        self.list.iter().map(Stream::memory).sum()
    }

    fn ask(&mut self, i: usize, start: u32, needed: bool, now: Millis) {
        let low = self.low_heap;
        let s = &mut self.list[i];
        let Some(owner) = s.owner else { return };
        if low {
            return;
        }
        match s.pending {
            Some((p, t)) if p == start && now.saturating_sub(t) < REASK_MS => return,
            Some((_, t)) if !needed && now.saturating_sub(t) < REASK_MS => return,
            _ => {}
        }
        s.pending = Some((start, now));
        s.asks += 1;
        let len = s.len_at(start);
        let mut o = Obj::new();
        o.str("type", "ota/block_needed")
            .num("session", s.offer.id)
            .num("offset", start)
            .num("len", len);
        self.outbox.push((owner, o.end()));
    }

    /// Reads bytes of stream `id`; asks the client for what is missing and
    /// for the chunks ahead.
    pub fn read(&mut self, id: u32, offset: u32, buf: &mut [u8], now: Millis) -> Fetch {
        let Some(i) = self.list.iter().position(|s| s.offer.id == id) else {
            return Fetch::Gone;
        };
        let s = &mut self.list[i];
        if u64::from(offset) + buf.len() as u64 > u64::from(s.offer.size) {
            return Fetch::Gone;
        }
        s.last_read = offset;
        let mut at = offset;
        let end = offset + buf.len() as u32;
        while at < end {
            let chunk = at / CHUNK * CHUNK;
            let (Some(k), Some(ring)) = (s.slot_of(chunk), s.ring.as_ref()) else {
                s.waits += 1;
                self.ask(i, chunk, true, now);
                return Fetch::Wait;
            };
            let from = k * CHUNK as usize + (at - chunk) as usize;
            let n = (chunk + s.len_at(chunk) - at).min(end - at) as usize;
            let dst = (at - offset) as usize;
            buf[dst..dst + n].copy_from_slice(&ring[from..from + n]);
            at += n as u32;
        }
        // One chunk ahead at a time: the nearest missing one.
        let cur = offset / CHUNK * CHUNK;
        let next = (1..=AHEAD)
            .map(|k| cur + k * CHUNK)
            .take_while(|&c| c < s.offer.size)
            .find(|&c| s.slot_of(c).is_none());
        if let Some(c) = next {
            self.ask(i, c, false, now);
        }
        Fetch::Ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::build;

    fn file() -> Vec<u8> {
        build(
            1,
            2,
            3,
            "s",
            &(0..10_000u32).map(|i| (i * 7) as u8).collect::<Vec<_>>(),
            None,
        )
    }

    fn b64(b: &[u8]) -> String {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for c in b.chunks(3) {
            let n = u32::from(c[0]) << 16
                | u32::from(*c.get(1).unwrap_or(&0)) << 8
                | u32::from(*c.get(2).unwrap_or(&0));
            for k in 0..4 {
                s.push(if k <= c.len() {
                    A[(n >> (18 - 6 * k) & 63) as usize] as char
                } else {
                    '='
                });
            }
        }
        s
    }

    #[test]
    fn serves_on_demand_through_a_fixed_ring() {
        let f = file();
        let c = CHUNK as usize;
        let mut s = Streams::default();
        assert!(
            s.offer(9, &f[..20], f.len() as u32, "x", None, None)
                .is_err(),
            "header incomplete"
        );
        let id = s
            .offer(9, &f[..100], f.len() as u32, "x", None, None)
            .unwrap();
        assert_eq!(id, FIRST_ID);
        assert_eq!(s.memory(), 56, "no window before the first chunk");
        let mut b = [0u8; 48];
        assert_eq!(s.read(id, 0, &mut b, 0), Fetch::Wait);
        assert_eq!(s.outbox.len(), 1);
        assert!(s.outbox[0].1.contains("\"offset\":0") && s.outbox[0].1.contains("\"len\":1024"));
        // Asked once until the re-ask time.
        assert_eq!(s.read(id, 0, &mut b, 100), Fetch::Wait);
        assert_eq!(s.outbox.len(), 1);
        s.data_base64(9, id, 0, &b64(&f[..c])).unwrap();
        assert_eq!(s.memory(), 56 + SLOTS * c);
        assert_eq!(s.read(id, 0, &mut b, 200), Fetch::Ready);
        assert_eq!(&b[..], &f[..48]);
        // Prefetch of the next chunk, one at a time.
        assert!(s.outbox[1].1.contains("\"offset\":1024"));
        assert_eq!(s.read(id, 48, &mut b, 250), Fetch::Ready);
        assert_eq!(s.outbox.len(), 2);
        s.data(9, id, CHUNK, &f[c..2 * c]).unwrap();
        // A block across two chunks; then the one after is asked for.
        assert_eq!(s.read(id, CHUNK - 6, &mut b, 300), Fetch::Ready);
        assert_eq!(&b[..], &f[c - 6..c + 42]);
        assert!(s.outbox.last().unwrap().1.contains("\"offset\":2048"));
        // The device far ahead: the ring keeps its size, chunks behind go.
        for k in 2..9u32 {
            let o = k * CHUNK;
            s.data(9, id, o, &f[o as usize..(o as usize + c).min(f.len())])
                .unwrap();
            assert_eq!(s.read(id, o, &mut b, 400), Fetch::Ready);
            assert_eq!(&b[..], &f[o as usize..o as usize + 48]);
        }
        assert_eq!(s.memory(), 56 + SLOTS * c);
        assert_eq!(s.read(id, 0, &mut b, 500), Fetch::Wait, "dropped behind");
        // The last, shorter chunk.
        let last = (f.len() / c * c) as u32;
        s.data(9, id, last, &f[last as usize..]).unwrap();
        let mut t = vec![0u8; f.len() - last as usize - 10];
        assert_eq!(s.read(id, last + 10, &mut t, 600), Fetch::Ready);
        assert_eq!(&t[..], &f[last as usize + 10..]);
        // Wrong chunks are refused.
        assert!(s.data(9, id, 100, &f[100..100 + c]).is_err());
        assert!(s.data(9, id, CHUNK, &f[c..2 * c - 1]).is_err(), "short");
        assert!(s.data_base64(9, id, CHUNK, "Zm9v").is_err());
        assert!(s.data(8, id, 0, &f[..c]).is_err(), "another session");
        assert_eq!(s.read(id, f.len() as u32, &mut b, 0), Fetch::Gone);
    }

    #[test]
    fn nothing_is_asked_for_while_the_heap_is_low() {
        let f = file();
        let mut s = Streams::default();
        let id = s
            .offer(9, &f[..100], f.len() as u32, "x", None, None)
            .unwrap();
        s.set_low_heap(true);
        let mut b = [0u8; 48];
        assert_eq!(s.read(id, 0, &mut b, 0), Fetch::Wait);
        assert!(s.outbox.is_empty());
        s.set_low_heap(false);
        assert_eq!(s.read(id, 0, &mut b, 10), Fetch::Wait);
        assert_eq!(s.outbox.len(), 1);
        assert_eq!(s.list()[0].waits, 2);
    }

    #[test]
    fn a_client_that_goes_away_has_a_grace_time() {
        let f = file();
        let mut s = Streams::default();
        let id = s
            .offer(1, &f[..100], f.len() as u32, "x", None, None)
            .unwrap();
        s.disconnected(1, 1000);
        let mut b = [0u8; 10];
        assert_eq!(s.read(id, 0, &mut b, 2000), Fetch::Wait);
        assert!(s.outbox.is_empty(), "nobody to ask");
        // Another session offers the same image: same ID, asked again.
        assert_eq!(
            s.offer(2, &f[..100], f.len() as u32, "y", None, None),
            Ok(id)
        );
        assert_eq!(s.read(id, 0, &mut b, 3000), Fetch::Wait);
        assert_eq!(s.outbox[0].0, 2);
        s.disconnected(2, 4000);
        assert!(s.poll(4000 + GRACE_MS - 1).is_empty());
        assert_eq!(s.poll(4000 + GRACE_MS)[0].id, id);
        assert_eq!(s.read(id, 0, &mut b, 0), Fetch::Gone);
    }

    #[test]
    fn a_file_that_does_not_match_its_header_is_dropped() {
        let f = file();
        let mut s = Streams::default();
        let id = s
            .offer(1, &f[..100], f.len() as u32, "x", None, None)
            .unwrap();
        let mut other = f[..CHUNK as usize].to_vec();
        other[14] ^= 1; // file version
        assert!(s.data(1, id, 0, &other).is_err());
        assert!(s.list().is_empty());
        assert!(
            s.offer(1, &f[..100], f.len() as u32 + 1, "x", None, None)
                .is_err(),
            "size"
        );
    }
}
