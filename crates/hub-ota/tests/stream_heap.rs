//! Heap use of a streamed download: after the first chunk the hub's side
//! (server, streams) holds a fixed window and allocates nothing large per
//! chunk (an S3 without PSRAM ran out of heap with a `Value` copy and
//! decoded buffers per 4 KiB chunk). One test per binary: the counting
//! allocator sees this thread only in practice.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};

use hub_ota::file::build;
use hub_ota::images::Sources;
use hub_ota::server::{Action, Command, Device, Millis, Server, Settings};
use hub_ota::sim::SimClient;
use hub_ota::stream::{CHUNK, SLOTS, Streams};

struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);
static LARGEST: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let live = LIVE.fetch_add(l.size() as isize, Ordering::Relaxed) + l.size() as isize;
        PEAK.fetch_max(live, Ordering::Relaxed);
        LARGEST.fetch_max(l.size(), Ordering::Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn b64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(b.len() / 3 * 4 + 4);
    for c in b.chunks(3) {
        let n = u32::from(c[0]) << 16
            | u32::from(*c.get(1).unwrap_or(&0)) << 8
            | u32::from(*c.get(2).unwrap_or(&0));
        for k in 0..4 {
            s.push(if k <= c.len() {
                T[(n >> (18 - 6 * k) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    s
}

fn field(t: &str, k: &str) -> u32 {
    hub_ota::json::uint_field(t, k).unwrap() as u32
}

#[test]
fn a_streamed_download_holds_a_fixed_window() {
    const SID: u64 = 3;
    let body: Vec<u8> = (0..120_000u32).map(|i| (i * 31 + i / 7) as u8).collect();
    let file = build(0x6D6B, 1, 2, "c6", &body, None);
    // The client's base64 text per chunk (the WebSocket frames, which the
    // hub's network stack holds, not the OTA code).
    let texts: Vec<String> = file.chunks(CHUNK as usize).map(b64).collect();
    let mut srv = Server::new(Settings {
        blocks_per_s: 0,
        ..Settings::default()
    });
    let mut streams = Streams::default();
    let dev = Device {
        id: "zb:0000000000000001".into(),
        ieee: 1,
        sleepy: false,
    };
    let mut client = SimClient::new(0x6D6B, 1, 1);
    client.received.reserve(file.len() + 64);
    let id = streams
        .offer(SID, &file[..200], file.len() as u32, "c6.ota", None, None)
        .unwrap();
    let mut now: Millis = 1_795_230_000_000;
    let mut inflight = Vec::new();
    let q = client.query();
    inflight.push(q);
    let mut started = false;
    let mut base = None;
    let mut chunks = 0usize;
    while client.upgrades == 0 {
        now += 50;
        assert!(now < 1_795_230_000_000 + 3_600_000, "download stalled");
        for (sid, text) in std::mem::take(&mut streams.outbox) {
            assert_eq!(sid, SID);
            let off = field(&text, "offset");
            streams
                .data_base64(
                    SID,
                    field(&text, "session"),
                    off,
                    &texts[(off / CHUNK) as usize],
                )
                .unwrap();
            chunks += 1;
        }
        let mut frames = std::mem::take(&mut inflight);
        frames.extend(client.tick(now));
        let mut out: Vec<Action> = Vec::new();
        for (tsn, c, p) in frames {
            let mut src = Sources::new(&mut streams);
            out.extend(srv.on_command(
                &dev,
                &Command {
                    ep: 1,
                    tsn,
                    cmd: c,
                    payload: &p,
                    default_rsp: true,
                },
                &mut src,
                now,
            ));
        }
        if !started {
            let src = Sources::new(&mut streams);
            out.extend(
                srv.start(&dev, Some(1), Some(id), false, &src, now)
                    .unwrap(),
            );
            started = true;
        }
        {
            let src = Sources::new(&mut streams);
            out.extend(srv.poll(&src, &|_| Some(1), now));
        }
        for a in out {
            if let Action::Send { cmd, payload, .. } = a {
                inflight.extend(client.on_command(cmd, &payload, now));
            }
        }
        // Steady state: from 10 % on.
        if base.is_none() && client.received.len() > file.len() / 10 {
            base = Some(LIVE.load(Ordering::Relaxed));
            PEAK.store(base.unwrap(), Ordering::Relaxed);
            LARGEST.store(0, Ordering::Relaxed);
        }
    }
    let base = base.unwrap();
    let peak = PEAK.load(Ordering::Relaxed) - base;
    let largest = LARGEST.load(Ordering::Relaxed);
    let end = LIVE.load(Ordering::Relaxed) - base;
    println!(
        "chunks {chunks}, peak +{peak} B, largest allocation {largest} B, at the end {end:+} B"
    );
    assert_eq!(client.version, 2);
    assert!(
        chunks <= file.len() / CHUNK as usize + 3,
        "each chunk asked for about once: {chunks}"
    );
    assert_eq!(streams.memory(), 56 + SLOTS * CHUNK as usize);
    // Nothing chunk-sized is allocated per chunk, nothing grows.
    assert!(largest < 512, "largest allocation {largest} B");
    assert!(peak < 4096, "peak +{peak} B");
    assert!(end < 1024, "grew by {end} B");
}
