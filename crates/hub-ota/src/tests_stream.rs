//! The server with streamed images (no store): a simulated client serves
//! `ota/block_needed` from the file it holds, a simulated device downloads.

use crate::file::build;
use crate::images::Sources;
use crate::server::{Action, Command, Device, Millis, Server, Settings, cmd, status};
use crate::sim::{ClientState, SimClient};
use crate::stream::{GRACE_MS, Streams};

const SESSION: u64 = 7;
const T0: Millis = 1_795_230_000_000;

struct Net {
    srv: Server,
    streams: Streams,
    dev: Device,
    client: SimClient,
    file: Vec<u8>,
    now: Millis,
    /// The API client is connected and answers.
    online: bool,
    waits_sent: u32,
    /// Device → hub frames, delivered at the next 50 ms tick (one round
    /// trip per tick).
    inflight: Vec<(u8, u8, Vec<u8>)>,
}

impl Net {
    fn new() -> Net {
        let file = build(
            0x6D6B,
            0x0001,
            0x0002_0000,
            "c6 2.0",
            &(0..20_000u32).map(|i| (i * 13) as u8).collect::<Vec<_>>(),
            None,
        );
        Net {
            srv: Server::new(Settings {
                blocks_per_s: 0,
                ..Settings::default()
            }),
            streams: Streams::default(),
            dev: Device {
                id: "zb:0000000000000001".into(),
                ieee: 1,
                sleepy: true,
            },
            client: SimClient::new(0x6D6B, 0x0001, 0x0001_0000),
            file,
            now: T0,
            online: true,
            waits_sent: 0,
            inflight: Vec::new(),
        }
    }

    fn offer(&mut self) -> u32 {
        let n = self.file.len() as u32;
        self.streams
            .offer(SESSION, &self.file[..200], n, "c6.ota", None, None)
            .unwrap()
    }

    fn hub_gets(&mut self, frames: Vec<(u8, u8, Vec<u8>)>) {
        for (tsn, c, p) in frames {
            let mut src = Sources {
                #[cfg(feature = "store")]
                store: None,
                streams: &mut self.streams,
            };
            let out = self.srv.on_command(
                &self.dev,
                &Command {
                    ep: 1,
                    tsn,
                    cmd: c,
                    payload: &p,
                    default_rsp: true,
                },
                &mut src,
                self.now,
            );
            self.device_gets(out);
        }
    }

    fn device_gets(&mut self, out: Vec<Action>) {
        for a in out {
            if let Action::Send {
                cmd: c, payload, ..
            } = a
            {
                if c == cmd::IMAGE_BLOCK_RSP && payload[0] == status::WAIT_FOR_DATA {
                    self.waits_sent += 1;
                }
                let frames = self.client.on_command(c, &payload, self.now);
                self.inflight.extend(frames);
            }
        }
    }

    /// The API client answers what it was asked for.
    fn serve(&mut self) {
        for (sid, text) in std::mem::take(&mut self.streams.outbox) {
            assert_eq!(sid, SESSION);
            if !self.online {
                continue;
            }
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(v["type"], "ota/block_needed");
            let (id, off, len) = (
                v["session"].as_u64().unwrap() as u32,
                v["offset"].as_u64().unwrap() as usize,
                v["len"].as_u64().unwrap() as usize,
            );
            self.streams
                .data(SESSION, id, off as u32, &self.file[off..off + len])
                .unwrap();
        }
    }

    fn run(&mut self, ms: u64) {
        let end = self.now + ms;
        while self.now < end {
            self.now += 50;
            self.serve();
            for gone in self.streams.poll(self.now) {
                let out = self.srv.image_gone(gone.ident(), "stream gone", self.now);
                self.device_gets(out);
            }
            let mut src = Sources {
                #[cfg(feature = "store")]
                store: None,
                streams: &mut self.streams,
            };
            let out = self.srv.poll(&src, &|_| Some(1), self.now);
            let _ = &mut src;
            self.device_gets(out);
            let mut f = std::mem::take(&mut self.inflight);
            f.extend(self.client.tick(self.now));
            self.hub_gets(f);
        }
    }

    fn state(&self) -> &'static str {
        self.srv.devices[&self.dev.id]
            .job
            .as_ref()
            .map_or("none", |j| j.state.name())
    }
}

#[test]
fn a_streamed_image_is_downloaded_through_a_small_window() {
    let mut n = Net::new();
    let id = n.offer();
    let q = n.client.query();
    n.hub_gets(vec![q]);
    n.run(100);
    let src = Sources {
        #[cfg(feature = "store")]
        store: None,
        streams: &mut n.streams,
    };
    let rec: serde_json::Value =
        serde_json::from_str(&n.srv.record(&n.dev.id, 1, &src, n.now, |_| {})).unwrap();
    assert_eq!(rec["available"]["image"], id);
    assert_eq!(rec["available"]["streamed"], true);
    let out = n
        .srv
        .start(&n.dev, None, Some(id), false, &src, n.now)
        .unwrap();
    n.device_gets(out);
    n.run(60_000);
    assert_eq!(n.client.upgrades, 1);
    assert_eq!(n.client.version, 0x0002_0000);
    assert_eq!(n.state(), "done");
    // The device waited for the client only at the start (prefetch).
    assert_eq!(n.waits_sent, 1);
    assert!(
        n.streams.list()[0].received <= n.file.len() as u64 + 2 * u64::from(crate::stream::CHUNK)
    );
    assert_eq!(
        n.streams.memory(),
        56 + crate::stream::SLOTS * crate::stream::CHUNK as usize
    );
}

#[test]
fn the_device_waits_while_the_client_is_away_and_continues_after_it_returns() {
    let mut n = Net::new();
    let id = n.offer();
    let q = n.client.query();
    n.hub_gets(vec![q]);
    n.run(100);
    let src = Sources {
        #[cfg(feature = "store")]
        store: None,
        streams: &mut n.streams,
    };
    let out = n
        .srv
        .start(&n.dev, None, Some(id), false, &src, n.now)
        .unwrap();
    n.device_gets(out);
    while n.client.received.len() < 9000 {
        n.run(50);
    }
    // The API client disconnects (Home Assistant restarting).
    n.online = false;
    n.streams.disconnected(SESSION, n.now);
    n.run(20_000);
    assert!(matches!(
        n.client.state,
        ClientState::Waiting(_) | ClientState::Downloading
    ));
    assert!(!n.client.aborted);
    // It offers the same image again: same session ID, the download goes on.
    n.online = true;
    assert_eq!(n.offer(), id);
    n.run(60_000);
    assert_eq!(n.client.upgrades, 1);
    assert_eq!(n.state(), "done");
}

#[test]
fn without_a_client_the_device_is_aborted_after_the_grace_time() {
    let mut n = Net::new();
    let id = n.offer();
    let q = n.client.query();
    n.hub_gets(vec![q]);
    n.run(100);
    let src = Sources {
        #[cfg(feature = "store")]
        store: None,
        streams: &mut n.streams,
    };
    let out = n
        .srv
        .start(&n.dev, None, Some(id), false, &src, n.now)
        .unwrap();
    n.device_gets(out);
    n.run(2_000);
    n.online = false;
    n.streams.disconnected(SESSION, n.now);
    n.run(GRACE_MS + 5_000);
    assert!(n.client.aborted, "ABORT once the stream is gone");
    assert_eq!(n.state(), "aborted", "the job ends with the stream");
    assert_eq!(n.client.version, 0x0001_0000);
}
