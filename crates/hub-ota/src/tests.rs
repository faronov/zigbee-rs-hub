//! The server against simulated clients, without a hub: frames go straight
//! between [`Server`] and [`SimClient`]s, time is simulated.

use std::collections::VecDeque;

use hub_ncp::store::MemFlash;

use crate::file::build;
use crate::server::{Action, Command, Device, Millis, Policy, Server, Settings, cmd, status};
use crate::sim::SimClient;
use crate::store::{ImageStore, UploadMeta};

const MFR: u16 = 0x6D6B;
const TYPE: u16 = 0x0102;
/// A synchronised wall clock: 2026-11-21 03:00 UTC.
const T0: Millis = 1_795_230_000_000;

struct Net {
    srv: Server,
    store: ImageStore,
    clients: Vec<(Device, SimClient)>,
    now: Millis,
    /// Every action the server produced, in order.
    log: Vec<Action>,
}

fn dev(n: u64) -> Device {
    Device {
        id: format!("zb:{n:016x}"),
        ieee: n,
        sleepy: false,
    }
}

fn image(version: u32, len: usize) -> Vec<u8> {
    build(
        MFR,
        TYPE,
        version,
        "sim image",
        &(0..len).map(|i| (i * 31 + 7) as u8).collect::<Vec<_>>(),
        None,
    )
}

impl Net {
    fn new(settings: Settings, n: usize) -> Net {
        let store = ImageStore::open(Box::new(MemFlash::new(64, 4096))).unwrap();
        let clients = (1..=n as u64)
            .map(|i| (dev(i), SimClient::new(MFR, TYPE, 0x0001_0000)))
            .collect();
        Net {
            srv: Server::new(settings),
            store,
            clients,
            now: T0,
            log: Vec::new(),
        }
    }

    fn upload(&mut self, f: &[u8]) -> u16 {
        let (id, _) = self
            .store
            .begin(
                f.len() as u32,
                UploadMeta {
                    name: "f.ota".into(),
                    ..Default::default()
                },
                0,
            )
            .unwrap();
        for (i, c) in f.chunks(4096).enumerate() {
            self.store.write(id, (i * 4096) as u32, c).unwrap();
        }
        self.store.commit(id, false).unwrap();
        id
    }

    fn idx(&self, id: &str) -> usize {
        self.clients.iter().position(|(d, _)| d.id == id).unwrap()
    }

    /// Routes server actions to clients and their answers back until quiet.
    fn deliver(&mut self, actions: Vec<Action>) {
        let mut q: VecDeque<Action> = actions.into();
        while let Some(a) = q.pop_front() {
            self.log.push(a.clone());
            let Action::Send {
                device,
                cmd: c,
                payload,
                ..
            } = a
            else {
                if let Action::ReadVersion { device, .. } = &a {
                    let i = self.idx(device);
                    let v = self.clients[i].1.version;
                    q.extend(self.srv.on_version(device, v, self.now));
                }
                continue;
            };
            let i = self.idx(&device);
            let now = self.now;
            let frames = self.clients[i].1.on_command(c, &payload, now);
            for (tsn, c, p) in frames {
                let d = self.clients[i].0.clone();
                let out = self.srv.on_command(
                    &d,
                    &Command {
                        ep: 1,
                        tsn,
                        cmd: c,
                        payload: &p,
                        default_rsp: true,
                    },
                    &mut self.store,
                    now,
                );
                q.extend(out);
            }
        }
    }

    fn query(&mut self, i: usize) {
        let (tsn, c, p) = self.clients[i].1.query();
        let d = self.clients[i].0.clone();
        let out = self.srv.on_command(
            &d,
            &Command {
                ep: 1,
                tsn,
                cmd: c,
                payload: &p,
                default_rsp: true,
            },
            &mut self.store,
            self.now,
        );
        self.deliver(out);
    }

    /// Advance time in 10 ms steps.
    fn run(&mut self, ms: u64) {
        let end = self.now + ms;
        while self.now < end {
            self.now += 10;
            let ids: Vec<(String, u64)> = self
                .clients
                .iter()
                .map(|(d, _)| (d.id.clone(), d.ieee))
                .collect();
            let out = self.srv.poll(
                &self.store,
                &|id| ids.iter().find(|x| x.0 == id).map(|x| x.1),
                self.now,
            );
            self.deliver(out);
            for i in 0..self.clients.len() {
                let now = self.now;
                for (tsn, c, p) in self.clients[i].1.tick(now) {
                    let d = self.clients[i].0.clone();
                    let out = self.srv.on_command(
                        &d,
                        &Command {
                            ep: 1,
                            tsn,
                            cmd: c,
                            payload: &p,
                            default_rsp: true,
                        },
                        &mut self.store,
                        now,
                    );
                    self.deliver(out);
                }
            }
        }
    }

    fn job_state(&self, i: usize) -> Option<&'static str> {
        self.srv
            .devices
            .get(&self.clients[i].0.id)?
            .job
            .as_ref()
            .map(|j| j.state.name())
    }

    fn record(&self, i: usize) -> serde_json::Value {
        let d = &self.clients[i].0;
        serde_json::from_str(
            &self
                .srv
                .record(&d.id, d.ieee, &self.store, self.now, |_| {}),
        )
        .unwrap()
    }
}

fn fast() -> Settings {
    Settings {
        blocks_per_s: 0,
        ..Settings::default()
    }
}

#[test]
fn manual_policy_offers_only_after_start_and_confirms_the_new_version() {
    let mut n = Net::new(fast(), 1);
    let f = image(0x0001_0100, 3000);
    let img = n.upload(&f);
    // The device's own query: recorded, the update is available, not offered.
    n.query(0);
    let rsp = n.log.iter().find_map(|a| match a {
        Action::Send {
            cmd: cmd::QUERY_NEXT_IMAGE_RSP,
            payload,
            ..
        } => Some(payload.clone()),
        _ => None,
    });
    assert_eq!(rsp, Some(vec![status::NO_IMAGE_AVAILABLE]));
    let r = n.record(0);
    assert_eq!(r["current_version"], 0x0001_0000);
    assert_eq!(r["available"]["image"], img);
    assert_eq!(r["effective_policy"], "manual");
    assert!(r["job"].is_null());
    // ota/start: Image Notify (type 3), the client queries and downloads.
    let d = n.clients[0].0.clone();
    let out = n
        .srv
        .start(&d, Some(1), None, false, &n.store, n.now)
        .unwrap();
    assert!(
        matches!(&out[0], Action::Send { cmd: cmd::IMAGE_NOTIFY, payload, .. } if payload[0] == 3 && payload.len() == 10)
    );
    n.deliver(out);
    let c = &n.clients[0].1;
    assert_eq!(c.version, 0x0001_0100, "upgraded");
    assert_eq!(c.upgrades, 1);
    assert_eq!(c.largest_block, 48, "the client's MaxDataSize");
    assert_eq!(n.srv.block_cap(), 48, "the default cap");
    // After the "reboot" the client queried with the new version: done.
    assert_eq!(n.job_state(0), Some("done"));
    assert_eq!(n.record(0)["current_version"], 0x0001_0100);
    assert!(n.record(0)["available"].is_null());
    // The Upgrade End Response said "now".
    let end = n.log.iter().find_map(|a| match a {
        Action::Send {
            cmd: cmd::UPGRADE_END_RSP,
            payload,
            ..
        } => Some(payload.clone()),
        _ => None,
    });
    let end = end.unwrap();
    assert_eq!(&end[8..16], &[0; 8]);
}

#[test]
fn blocks_fit_the_ncp_asdu() {
    for (asdu, want) in [(82usize, 65usize), (60, 43)] {
        let mut n = Net::new(
            Settings {
                block_max: 200,
                ..fast()
            },
            1,
        );
        n.srv.asdu_max = asdu;
        n.clients[0].1.block_size = 100;
        n.upload(&image(2 << 16, 5000));
        let d = n.clients[0].0.clone();
        n.query(0);
        let out = n.srv.start(&d, None, None, false, &n.store, n.now).unwrap();
        n.deliver(out);
        assert_eq!(n.clients[0].1.upgrades, 1);
        assert_eq!(n.clients[0].1.largest_block, want, "asdu {asdu}");
        let biggest = n
            .log
            .iter()
            .filter_map(|a| match a {
                Action::Send {
                    cmd: cmd::IMAGE_BLOCK_RSP,
                    payload,
                    ..
                } => Some(payload.len() + 3),
                _ => None,
            })
            .max()
            .unwrap();
        assert!(biggest <= asdu, "{biggest} > {asdu}");
    }
}

#[test]
fn concurrency_queues_and_notifies_when_a_slot_frees() {
    let mut n = Net::new(
        Settings {
            max_concurrent: 1,
            ..fast()
        },
        2,
    );
    n.upload(&image(0x0001_0100, 20_000));
    for i in 0..2 {
        n.query(i);
    }
    // Start both; deliver only the notifies, then let the first one block.
    let d0 = n.clients[0].0.clone();
    let d1 = n.clients[1].0.clone();
    let a0 = n
        .srv
        .start(&d0, None, None, false, &n.store, n.now)
        .unwrap();
    let a1 = n
        .srv
        .start(&d1, None, None, false, &n.store, n.now)
        .unwrap();
    // The first device's notify starts its download; hold its blocks by
    // delivering the second's notify first with the first mid-download.
    let notify0: Vec<_> = a0
        .into_iter()
        .filter(|a| matches!(a, Action::Send { .. }))
        .collect();
    let (tsn, c, p) = n.clients[0]
        .1
        .on_command(
            cmd::IMAGE_NOTIFY,
            &match &notify0[0] {
                Action::Send { payload, .. } => payload.clone(),
                _ => unreachable!(),
            },
            n.now,
        )
        .remove(0);
    let qr = n.srv.on_command(
        &d0,
        &Command {
            ep: 1,
            tsn,
            cmd: c,
            payload: &p,
            default_rsp: true,
        },
        &mut n.store,
        n.now,
    );
    // Device 0 got an offer; it is downloading now.
    assert_eq!(n.job_state(0), Some("downloading"));
    n.deliver(a1);
    assert_eq!(
        n.job_state(1),
        Some("queued"),
        "only one download at a time"
    );
    assert_eq!(n.clients[1].1.state, crate::sim::ClientState::Idle);
    // Device 0 completes; device 1 is notified (after 30 s since its last poke).
    n.deliver(qr);
    assert_eq!(n.job_state(0), Some("done"));
    n.run(31_000);
    assert_eq!(n.job_state(1), Some("done"));
    assert_eq!(n.clients[1].1.version, 0x0001_0100);
}

#[test]
fn abort_ends_the_download_with_abort() {
    let mut n = Net::new(fast(), 1);
    n.upload(&image(0x0001_0100, 4000));
    n.query(0);
    let d = n.clients[0].0.clone();
    let out = n.srv.start(&d, None, None, false, &n.store, n.now).unwrap();
    // Deliver the notify and the first few exchanges by hand.
    let notify = match &out[0] {
        Action::Send { payload, .. } => payload.clone(),
        _ => unreachable!(),
    };
    let (tsn, c, p) = n.clients[0]
        .1
        .on_command(cmd::IMAGE_NOTIFY, &notify, n.now)
        .remove(0);
    let qr = n.srv.on_command(
        &d,
        &Command {
            ep: 1,
            tsn,
            cmd: c,
            payload: &p,
            default_rsp: true,
        },
        &mut n.store,
        n.now,
    );
    let rsp = qr.iter().find_map(|a| match a {
        Action::Send {
            cmd: cmd::QUERY_NEXT_IMAGE_RSP,
            payload,
            ..
        } => Some(payload.clone()),
        _ => None,
    });
    let (tsn, c, p) = n.clients[0]
        .1
        .on_command(cmd::QUERY_NEXT_IMAGE_RSP, &rsp.unwrap(), n.now)
        .remove(0);
    n.srv.abort(&d.id, n.now).unwrap();
    let out = n.srv.on_command(
        &d,
        &Command {
            ep: 1,
            tsn,
            cmd: c,
            payload: &p,
            default_rsp: true,
        },
        &mut n.store,
        n.now,
    );
    n.deliver(out);
    assert!(n.clients[0].1.aborted);
    assert_eq!(n.job_state(0), Some("aborted"));
    assert_eq!(n.clients[0].1.version, 0x0001_0000);
}

#[test]
fn page_requests() {
    let mut n = Net::new(
        Settings {
            blocks_per_s: 50,
            ..Settings::default()
        },
        1,
    );
    n.clients[0].1.page_size = 256;
    n.upload(&image(0x0001_0100, 3000));
    n.query(0);
    let d = n.clients[0].0.clone();
    let out = n.srv.start(&d, None, None, false, &n.store, n.now).unwrap();
    n.deliver(out);
    n.run(20_000);
    assert_eq!(n.clients[0].1.upgrades, 1);
    assert_eq!(n.job_state(0), Some("done"));
}

#[test]
fn pacing_spaces_blocks_and_tells_the_minimum_block_period() {
    let mut n = Net::new(
        Settings {
            blocks_per_s: 10,
            min_block_period_ms: 250,
            ..Settings::default()
        },
        1,
    );
    n.upload(&image(0x0001_0100, 48 * 10));
    n.query(0);
    let d = n.clients[0].0.clone();
    let t0 = n.now;
    let out = n.srv.start(&d, None, None, false, &n.store, n.now).unwrap();
    n.deliver(out);
    n.run(10_000);
    let c = &n.clients[0].1;
    assert_eq!(c.upgrades, 1);
    assert_eq!(c.min_block_period, 250, "told with WAIT_FOR_DATA");
    assert!(c.waits >= 1);
    // 11 blocks, at least 250 ms apart.
    assert!(n.srv.devices[&d.id].job.as_ref().unwrap().updated - t0 >= 10 * 250);
}

#[test]
fn a_download_continues_after_a_hub_restart() {
    let mut n = Net::new(fast(), 1);
    let f = image(0x0001_0100, 6000);
    n.upload(&f);
    n.query(0);
    let d = n.clients[0].0.clone();
    let out = n.srv.start(&d, None, None, false, &n.store, n.now).unwrap();
    let notify = match &out[0] {
        Action::Send { payload, .. } => payload.clone(),
        _ => unreachable!(),
    };
    // Exchange until half of the image arrived.
    let mut frames = n.clients[0].1.on_command(cmd::IMAGE_NOTIFY, &notify, n.now);
    while n.clients[0].1.received.len() < 3000 {
        let (tsn, c, p) = frames.remove(0);
        let out = n.srv.on_command(
            &d,
            &Command {
                ep: 1,
                tsn,
                cmd: c,
                payload: &p,
                default_rsp: true,
            },
            &mut n.store,
            n.now,
        );
        for a in out {
            if let Action::Send {
                cmd: c, payload, ..
            } = a
            {
                frames.extend(n.clients[0].1.on_command(c, &payload, n.now));
            }
        }
    }
    // The hub restarts: only the persisted device state survives.
    let saved = n.srv.devices[&d.id].to_bytes();
    let mut srv = Server::new(fast());
    srv.devices.insert(
        d.id.clone(),
        crate::server::DevState::from_bytes(&saved, n.now).unwrap(),
    );
    n.srv = srv;
    assert_eq!(n.job_state(0), Some("downloading"));
    // The client simply carries on with its next block request.
    let (tsn, c, p) = frames.remove(0);
    let out = n.srv.on_command(
        &d,
        &Command {
            ep: 1,
            tsn,
            cmd: c,
            payload: &p,
            default_rsp: true,
        },
        &mut n.store,
        n.now,
    );
    n.deliver(out);
    assert_eq!(n.clients[0].1.upgrades, 1);
    assert_eq!(n.job_state(0), Some("done"));
}

#[test]
fn a_rejected_image_fails_the_job() {
    let mut n = Net::new(fast(), 1);
    n.clients[0].1.reject = true;
    n.upload(&image(0x0001_0100, 1000));
    n.query(0);
    let d = n.clients[0].0.clone();
    let out = n.srv.start(&d, None, None, false, &n.store, n.now).unwrap();
    n.deliver(out);
    assert_eq!(n.job_state(0), Some("failed"));
    assert!(
        n.record(0)["job"]["error"]
            .as_str()
            .unwrap()
            .contains("INVALID_IMAGE")
    );
    assert!(n.log.iter().any(|a| matches!(
        a,
        Action::DefaultRsp {
            status: 0,
            cmd: cmd::UPGRADE_END_REQ,
            ..
        }
    )));
}

#[test]
fn auto_policy_follows_the_window_and_needs_a_clock() {
    // 03:00 UTC, window 02:00–04:00 local (UTC).
    let w = Settings {
        policy: Policy::Auto,
        window: Some((120, 240)),
        ..fast()
    };
    let mut n = Net::new(w.clone(), 1);
    n.upload(&image(0x0001_0100, 1000));
    n.query(0);
    assert_eq!(n.clients[0].1.upgrades, 1, "offered inside the window");
    // Outside the window (tz +3 h → 06:00 local): not offered.
    let mut n = Net::new(
        Settings {
            tz_offset_s: 3 * 3600,
            ..w.clone()
        },
        1,
    );
    n.upload(&image(0x0001_0100, 1000));
    n.query(0);
    assert_eq!(n.clients[0].1.upgrades, 0);
    assert!(n.record(0)["available"].is_object());
    // No clock: the window cannot be evaluated, nothing is offered.
    let mut n = Net::new(w, 1);
    n.now = 50_000;
    n.upload(&image(0x0001_0100, 1000));
    n.query(0);
    assert_eq!(n.clients[0].1.upgrades, 0);
    // Without a window `auto` notifies on its own once a minute.
    let mut n = Net::new(
        Settings {
            policy: Policy::Auto,
            ..fast()
        },
        1,
    );
    n.query(0);
    n.upload(&image(0x0001_0100, 1000));
    n.run(61_000);
    assert_eq!(n.clients[0].1.upgrades, 1);
}

#[test]
fn hardware_range_and_file_version_limits() {
    let mut n = Net::new(fast(), 1);
    n.clients[0].1.hw = Some(5);
    let f = build(MFR, TYPE, 0x0001_0100, "", &[0; 100], Some((1, 3)));
    n.upload(&f);
    n.query(0);
    assert!(
        n.record(0)["available"].is_null(),
        "hardware 5 is outside 1..=3"
    );
    n.clients[0].1.hw = Some(2);
    n.query(0);
    assert!(n.record(0)["available"].is_object());
    // min_file_version above the device's version.
    let mut n = Net::new(fast(), 1);
    let f = image(0x0001_0100, 100);
    let (id, _) = n
        .store
        .begin(
            f.len() as u32,
            UploadMeta {
                min_file_version: Some(0x0001_0050),
                ..Default::default()
            },
            0,
        )
        .unwrap();
    n.store.write(id, 0, &f).unwrap();
    n.store.commit(id, false).unwrap();
    n.query(0);
    assert!(n.record(0)["available"].is_null());
    // Starting it anyway is refused, so is an unknown image.
    let d = n.clients[0].0.clone();
    assert_eq!(
        n.srv
            .start(&d, None, None, false, &n.store, n.now)
            .unwrap_err()
            .code,
        "not_found"
    );
    assert_eq!(
        n.srv
            .start(&d, None, Some(99), false, &n.store, n.now)
            .unwrap_err()
            .code,
        "not_found"
    );
}

#[test]
fn unknown_images_and_commands() {
    let mut n = Net::new(fast(), 1);
    let d = n.clients[0].0.clone();
    // A block request for an image the hub does not have: ABORT.
    let mut p = vec![0u8];
    p.extend(MFR.to_le_bytes());
    p.extend(TYPE.to_le_bytes());
    p.extend(9u32.to_le_bytes());
    p.extend(0u32.to_le_bytes());
    p.push(48);
    let out = n.srv.on_command(
        &d,
        &Command {
            ep: 1,
            tsn: 5,
            cmd: cmd::IMAGE_BLOCK_REQ,
            payload: &p,
            default_rsp: true,
        },
        &mut n.store,
        n.now,
    );
    assert!(out.iter().any(|a| matches!(a, Action::Send { cmd: cmd::IMAGE_BLOCK_RSP, payload, tsn: Some(5), .. } if payload == &[status::ABORT])));
    // Query Device Specific File: none.
    let out = n.srv.on_command(
        &d,
        &Command {
            ep: 1,
            tsn: 6,
            cmd: cmd::QUERY_SPECIFIC_FILE_REQ,
            payload: &[0; 18],
            default_rsp: true,
        },
        &mut n.store,
        n.now,
    );
    assert!(out.iter().any(|a| matches!(a, Action::Send { cmd: cmd::QUERY_SPECIFIC_FILE_RSP, payload, .. } if payload == &[status::NO_IMAGE_AVAILABLE])));
    // Unknown command: UNSUP_CLUSTER_COMMAND; truncated query: MALFORMED.
    let out = n.srv.on_command(
        &d,
        &Command {
            ep: 1,
            tsn: 7,
            cmd: 0x42,
            payload: &[],
            default_rsp: true,
        },
        &mut n.store,
        n.now,
    );
    assert!(out.iter().any(|a| matches!(
        a,
        Action::DefaultRsp {
            status: status::UNSUP_CLUSTER_COMMAND,
            ..
        }
    )));
    let out = n.srv.on_command(
        &d,
        &Command {
            ep: 1,
            tsn: 8,
            cmd: cmd::QUERY_NEXT_IMAGE_REQ,
            payload: &[0, 1],
            default_rsp: true,
        },
        &mut n.store,
        n.now,
    );
    assert!(out.iter().any(|a| matches!(
        a,
        Action::DefaultRsp {
            status: status::MALFORMED_COMMAND,
            ..
        }
    )));
    // Without an OTA area: queries are recorded and answered NO_IMAGE.
    let (tsn, c, p) = n.clients[0].1.query();
    let out = n.srv.on_command(
        &d,
        &Command {
            ep: 1,
            tsn,
            cmd: c,
            payload: &p,
            default_rsp: true,
        },
        &mut crate::images::NoImages,
        n.now,
    );
    assert!(out.iter().any(
        |a| matches!(a, Action::Send { payload, .. } if payload == &[status::NO_IMAGE_AVAILABLE])
    ));
}

#[test]
fn settings_round_trip_and_validation() {
    let mut s = Settings::default();
    s.apply(&serde_json::json!({"policy": "auto", "window": {"start": "23:30", "end": "05:00"}, "max_concurrent": 3})).unwrap();
    assert_eq!(s.window, Some((1410, 300)));
    let t = Settings::from_bytes(&s.to_bytes());
    assert_eq!(t, s);
    assert!(
        s.apply(&serde_json::json!({"policy": "sometimes"}))
            .is_err()
    );
    assert!(
        s.apply(&serde_json::json!({"window": {"start": "25:00", "end": "01:00"}}))
            .is_err()
    );
    assert!(s.apply(&serde_json::json!({"block_max": 5})).is_err());
    assert!(
        s.apply(&serde_json::json!({"index_url": "ftp://x"}))
            .is_err()
    );
    let mut u = s.clone();
    u.apply(&serde_json::json!({"index_url": "https://example.org/ota/index.json"}))
        .unwrap();
    assert_eq!(
        Settings::from_bytes(&u.to_bytes()).index_url,
        "https://example.org/ota/index.json"
    );
    assert_eq!(
        Settings::from_bytes(&s.to_bytes()),
        t,
        "failed applies change nothing"
    );
}
