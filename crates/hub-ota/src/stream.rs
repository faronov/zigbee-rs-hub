//! Images streamed on demand from an API client: the hub stores nothing.
//!
//! A client (the Home Assistant integration, a browser tab) that holds an
//! OTA file offers it with its first bytes (`ota/stream/offer`): the hub
//! checks the header and offers the image to devices like a stored one.
//! When a device asks for bytes the hub does not have, it answers
//! WAIT_FOR_DATA and sends the owning client `ota/block_needed {session,
//! offset, len}`; the client answers with `ota/block_data`. The hub keeps
//! a prefetch window of [`KEEP`] × [`CHUNK`] bytes per stream and asks for
//! the next chunk as soon as the device reads the newest one.
//!
//! When the client disappears its streams wait [`GRACE_MS`] for a client
//! to offer the same image again (Home Assistant reconnecting after a hub
//! restart); devices get WAIT_FOR_DATA meanwhile, then ABORT (a device
//! keeps what it has and continues when the image is offered again).
//! The file's sub-elements and SHA are checked by the client.

use crate::file::Header;
use crate::images::{Fetch, Offer};
use crate::json::Obj;
use crate::server::Millis;

/// The first stream ID (store images use 1..=0xFFFF).
pub const FIRST_ID: u32 = 0x1_0000;
/// Bytes a client sends per `ota/block_data`.
pub const CHUNK: u32 = 4096;
/// Chunks kept per stream (the prefetch window).
pub const KEEP: usize = 2;
/// Streams at once.
pub const MAX_STREAMS: usize = 4;
/// A chunk asked for and not delivered is asked for again after this.
const REASK_MS: Millis = 5_000;
/// A stream without a client waits this long for one.
pub const GRACE_MS: Millis = 120_000;

/// One streamed image.
#[derive(Debug, Clone)]
pub struct Stream {
    /// What is offered.
    pub offer: Offer,
    /// The API session serving it (`None`: gone, in its grace time).
    pub owner: Option<u64>,
    /// The header bytes the client offered (checked against chunk 0).
    head: Vec<u8>,
    chunks: Vec<(u32, Vec<u8>)>,
    pending: Option<(u32, Millis)>,
    orphaned: Option<Millis>,
    last_read: u32,
    /// Bytes received from the client.
    pub received: u64,
}

/// The streams.
#[derive(Debug, Default)]
pub struct Streams {
    list: Vec<Stream>,
    next: u32,
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
            chunks: Vec::new(),
            pending: None,
            orphaned: None,
            last_read: 0,
            received: 0,
        });
        Ok(id)
    }

    /// `ota/block_data`: a chunk the client was asked for.
    pub fn data(&mut self, owner: u64, id: u32, offset: u32, bytes: &[u8]) -> Result<(), String> {
        let s = self
            .list
            .iter_mut()
            .find(|s| s.offer.id == id && s.owner == Some(owner))
            .ok_or("no such stream of this session")?;
        let want = CHUNK.min(s.offer.size.saturating_sub(offset)) as usize;
        if !offset.is_multiple_of(CHUNK) || offset >= s.offer.size || bytes.len() != want {
            return Err(format!(
                "chunks are {CHUNK} B at multiples of {CHUNK} (the last one shorter)"
            ));
        }
        if offset == 0 && !bytes.starts_with(&s.head) {
            let id = s.offer.id;
            self.list.retain(|s| s.offer.id != id);
            return Err("the file does not start with the offered header; stream closed".into());
        }
        if s.pending.is_some_and(|p| p.0 == offset) {
            s.pending = None;
        }
        s.received += bytes.len() as u64;
        s.chunks.retain(|c| c.0 != offset);
        s.chunks.push((offset, bytes.to_vec()));
        // Keep the window where the device reads: first drop chunks wholly
        // behind it, then the farthest ahead (never the one just received).
        while s.chunks.len() > KEEP {
            let r = s.last_read;
            let behind = s
                .chunks
                .iter()
                .position(|c| c.0 != offset && c.0 + c.1.len() as u32 <= r);
            let drop = behind.unwrap_or_else(|| {
                (0..s.chunks.len())
                    .filter(|&i| s.chunks[i].0 != offset)
                    .max_by_key(|&i| s.chunks[i].0)
                    .unwrap()
            });
            s.chunks.remove(drop);
        }
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

    /// Drops streams whose grace time ended; returns their IDs.
    pub fn poll(&mut self, now: Millis) -> Vec<u32> {
        let gone: Vec<u32> = self
            .list
            .iter()
            .filter(|s| {
                s.orphaned
                    .is_some_and(|t| now.saturating_sub(t) >= GRACE_MS)
            })
            .map(|s| s.offer.id)
            .collect();
        self.list.retain(|s| !gone.contains(&s.offer.id));
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

    fn ask(&mut self, i: usize, start: u32, needed: bool, now: Millis) {
        let s = &mut self.list[i];
        let Some(owner) = s.owner else { return };
        match s.pending {
            Some((p, t)) if p == start && now.saturating_sub(t) < REASK_MS => return,
            Some((_, t)) if !needed && now.saturating_sub(t) < REASK_MS => return,
            _ => {}
        }
        s.pending = Some((start, now));
        let len = CHUNK.min(s.offer.size - start);
        let mut o = Obj::new();
        o.str("type", "ota/block_needed")
            .num("session", s.offer.id)
            .num("offset", start)
            .num("len", len);
        self.outbox.push((owner, o.end()));
    }

    /// Reads bytes of stream `id`; asks the client for what is missing and
    /// for the next chunk ahead.
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
            let Some((cs, data)) = s
                .chunks
                .iter()
                .find(|(cs, d)| *cs <= at && at < cs + d.len() as u32)
            else {
                let start = at / CHUNK * CHUNK;
                self.ask(i, start, true, now);
                return Fetch::Wait;
            };
            let from = (at - cs) as usize;
            let n = (data.len() - from).min((end - at) as usize);
            let dst = (at - offset) as usize;
            buf[dst..dst + n].copy_from_slice(&data[from..from + n]);
            at += n as u32;
        }
        let next = (offset / CHUNK + 1) * CHUNK;
        if next < s.offer.size && !s.chunks.iter().any(|c| c.0 == next) {
            self.ask(i, next, false, now);
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

    #[test]
    fn serves_on_demand_with_a_prefetch_window() {
        let f = file();
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
        let mut b = [0u8; 48];
        assert_eq!(s.read(id, 0, &mut b, 0), Fetch::Wait);
        assert_eq!(s.outbox.len(), 1);
        assert!(s.outbox[0].1.contains("\"offset\":0") && s.outbox[0].1.contains("\"len\":4096"));
        // Asked once until the re-ask time.
        assert_eq!(s.read(id, 0, &mut b, 100), Fetch::Wait);
        assert_eq!(s.outbox.len(), 1);
        s.data(9, id, 0, &f[..4096]).unwrap();
        assert_eq!(s.read(id, 0, &mut b, 200), Fetch::Ready);
        assert_eq!(&b[..], &f[..48]);
        // Prefetch of the next chunk.
        assert!(s.outbox[1].1.contains("\"offset\":4096"));
        s.data(9, id, 4096, &f[4096..8192]).unwrap();
        // A block across two chunks.
        assert_eq!(s.read(id, 4090, &mut b, 300), Fetch::Ready);
        assert_eq!(&b[..], &f[4090..4138]);
        // Reading in the newest chunk asks for the next one: the last,
        // shorter chunk; the chunk behind is dropped.
        assert_eq!(s.read(id, 5000, &mut b, 350), Fetch::Ready);
        assert!(s.outbox.last().unwrap().1.contains("\"offset\":8192"));
        s.data(9, id, 8192, &f[8192..]).unwrap();
        assert!(s.list()[0].chunks.iter().all(|c| c.0 != 0));
        assert_eq!(s.list()[0].chunks.len(), KEEP);
        let mut t = vec![0u8; f.len() - 8200];
        assert_eq!(s.read(id, 8200, &mut t, 400), Fetch::Ready);
        assert_eq!(&t[..], &f[8200..]);
        // Wrong chunks are refused.
        assert!(s.data(9, id, 100, &f[100..4196]).is_err());
        assert!(s.data(8, id, 0, &f[..4096]).is_err(), "another session");
        assert_eq!(s.read(id, f.len() as u32, &mut b, 0), Fetch::Gone);
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
        assert_eq!(s.poll(4000 + GRACE_MS), vec![id]);
        assert_eq!(s.read(id, 0, &mut b, 0), Fetch::Gone);
    }

    #[test]
    fn a_file_that_does_not_match_its_header_is_dropped() {
        let f = file();
        let mut s = Streams::default();
        let id = s
            .offer(1, &f[..100], f.len() as u32, "x", None, None)
            .unwrap();
        let mut other = f[..4096].to_vec();
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
