//! Backup and restore against the simulated NCP: the export of a formed
//! network (keys, frame counters, child and key table), its restore onto
//! another NCP in every verified EZSP version, the frame counter margins,
//! the coordinator IEEE rules (a rewritable token, the one-time token
//! only when confirmed and never twice), refusals that change nothing,
//! and no key in any log line.

use std::sync::Mutex;

use hub_coord::{BackupItem, BackupSink, RestoreEncoder, RestoreOptions, restore_note};

use super::*;

/// Distinct bytes, so that a leak in any formatting is found.
const NKEY: [u8; 16] = [
    0xA7, 0x3C, 0x91, 0x20, 0x5E, 0xD4, 0x0B, 0x76, 0xE8, 0x13, 0xC9, 0x4F, 0x62, 0xB5, 0x08, 0xFA,
];
const ROOT: [u8; 16] = [
    0x1D, 0xE2, 0x57, 0x8C, 0x39, 0xA0, 0xF3, 0x64, 0xCB, 0x0E, 0x95, 0x7A, 0x26, 0xBF, 0x41, 0xD8,
];
const LINK: [u8; 16] = [
    0x6B, 0x92, 0x0D, 0xF5, 0x48, 0xB1, 0x3E, 0xC7, 0x75, 0x2A, 0xE9, 0x10, 0x8F, 0x54, 0xDC, 0x03,
];
const A_IEEE: u64 = 0x000B_57FF_FEAA_0001;
const B_IEEE: u64 = 0x000B_57FF_FEBB_0002;
const CHILD: u64 = 0x0015_8D00_017C_1A5B;
const KEYED: u64 = 0x0017_8801_00AA_BBCC;

static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Capture;

impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, r: &log::Record<'_>) {
        LINES
            .lock()
            .unwrap()
            .push(format!("{} {}", r.target(), r.args()));
    }
    fn flush(&self) {}
}

static CAPTURE: Capture = Capture;

/// Every log line of the test binary from now on.
fn capture_logs() {
    let _ = log::set_logger(&CAPTURE);
    log::set_max_level(log::LevelFilter::Trace);
}

/// Log lines that show any of `keys` (hex either case, decimal or hex
/// lists of its first bytes).
fn leaks(keys: &[[u8; 16]]) -> Vec<String> {
    let lines = LINES.lock().unwrap();
    let mut found = Vec::new();
    for k in keys {
        let lower: String = k.iter().map(|b| format!("{b:02x}")).collect();
        let patterns = [
            lower[..8].to_string(),
            lower[..8].to_uppercase(),
            format!("{:?}", &k[..3])
                .trim_matches(['[', ']'])
                .to_string(),
            format!("{:02X?}", &k[..3])
                .trim_matches(['[', ']'])
                .to_string(),
        ];
        found.extend(
            lines
                .iter()
                .filter(|l| patterns.iter().any(|p| l.contains(p.as_str())))
                .cloned(),
        );
    }
    found
}

#[derive(Default)]
struct Items(Vec<BackupItem>);

impl BackupSink for Items {
    fn item(&mut self, item: &BackupItem) -> Result<(), CoordError> {
        self.0.push(*item);
        Ok(())
    }
}

/// A formed network with a child, a key table entry, frame counters and
/// a NWK update ID.
fn source(version: u8) -> SimConfig {
    let mut c = sim(version);
    c.eui64 = Eui64::from_u64(A_IEEE);
    c.network_key = NKEY;
    c.network_key_sequence = 2;
    c.tc_link_key = ROOT;
    c.nwk_frame_counter = 50_000;
    c.aps_frame_counter = 700;
    c.network.nwk_update_id = 3;
    c.network.channels = (1 << 15) | (1 << 20);
    c.key_table_size = 2;
    c.key_table = vec![
        None,
        Some((
            Eui64::from_u64(KEYED),
            LINK,
            ApsKeyMetadata {
                bitmask: key_bitmask::IS_AUTHORIZED | key_bitmask::HAS_PARTNER_EUI64,
                outgoing_frame_counter: 33,
                incoming_frame_counter: 44,
                ttl_in_seconds: 0,
            },
        )),
    ];
    c.children = vec![ChildData {
        eui64: Eui64::from_u64(CHILD),
        node_type: NodeType::SLEEPY_END_DEVICE,
        id: 0x4D32,
        timeout: 8,
        ..ChildData::default()
    }];
    c
}

/// An NCP without a network, with its own IEEE.
fn target(version: u8) -> SimConfig {
    let mut c = sim(version);
    c.network_formed = false;
    c.eui64 = Eui64::from_u64(B_IEEE);
    c.key_table_size = 4;
    c
}

fn export(rig: &mut Rig, keys: bool) -> Vec<BackupItem> {
    let mut items = Items::default();
    rig.admin.backup_export(keys, &mut items).unwrap();
    items.0
}

fn blob(items: &[BackupItem]) -> hub_coord::RestoreBlob {
    let mut e = Box::<BlobEncoder>::default();
    for i in items {
        e.push(i).unwrap();
    }
    e.finish()
}

fn margins(margin: u32) -> RestoreOptions {
    RestoreOptions {
        nwk_margin: margin,
        aps_margin: margin,
        ..RestoreOptions::default()
    }
}

fn ups(log: &[CoordEvent]) -> usize {
    log.iter().filter(|e| up(e).is_some()).count()
}

fn devices(items: &[BackupItem]) -> Vec<hub_coord::BackupDevice> {
    items
        .iter()
        .filter_map(|i| match i {
            BackupItem::Device(d) => Some(*d),
            _ => None,
        })
        .collect()
}

#[test]
fn a_backup_restores_onto_another_ncp_in_every_verified_version() {
    capture_logs();
    for version in [13, 14, 16, 17, 18] {
        let mut a = Rig::new(source(version));
        a.next_up();
        let items = export(&mut a, true);
        assert_eq!(
            items[0],
            BackupItem::Node {
                ieee: A_IEEE.to_le_bytes(),
                ieee_override: false
            }
        );
        assert!(
            matches!(items[1], BackupItem::Network { pan_id: 0x1A62, ext_pan_id, channel: 15, channel_mask, nwk_update_id: 3, security_level: 5, .. } if channel_mask == (1 << 15 | 1 << 20) && ext_pan_id == [0xDD; 8]),
            "EZSP {version}: {:?}",
            items[1]
        );
        assert!(items.contains(&BackupItem::NetworkKey {
            key: NKEY,
            seq: 2,
            tx_counter: 50_000,
            active: true
        }));
        // bellows' form: the well-known key, the root as the seed.
        assert!(items.contains(&BackupItem::HashedTclkSeed(ROOT)));
        assert!(items.contains(&BackupItem::TcLinkKey {
            key: ZA09,
            tc_ieee: A_IEEE.to_le_bytes(),
            tx_counter: 700,
            hashed: true
        }));
        let devs = devices(&items);
        assert_eq!(devs.len(), 2, "EZSP {version}: {devs:?}");
        let child = devs.iter().find(|d| d.ieee == CHILD.to_le_bytes()).unwrap();
        assert!(child.is_child && child.nwk == Some(0x4D32) && child.link.is_none());
        assert_eq!(
            child.child.map(|c| (c.capability, c.timeout_exp)),
            Some((0x80, 8))
        );
        let keyed = devs.iter().find(|d| d.ieee == KEYED.to_le_bytes()).unwrap();
        let l = keyed.link.unwrap();
        assert_eq!(
            (l.key, l.attrs, l.tx_counter, l.rx_floor),
            (LINK, 2, 33, 44)
        );
        assert!(!keyed.is_child);
        // Without keys: no key, no key table.
        let bare = export(&mut a, false);
        assert!(!bare.iter().any(|i| matches!(
            i,
            BackupItem::NetworkKey { .. }
                | BackupItem::TcLinkKey { .. }
                | BackupItem::HashedTclkSeed(_)
        )));
        assert!(devices(&bare).iter().all(|d| d.link.is_none()));
        a.finish();

        // Onto a blank NCP with the token interface: the IEEE rewritten.
        let mut b = Rig::new(SimConfig {
            token_interface: true,
            ..target(version)
        });
        b.next_up();
        let before = ups(&b.log);
        let blob = blob(&items);
        b.admin.restore_check(&blob, margins(10_000)).unwrap();
        let report = b.admin.backup_restore(blob, margins(10_000)).unwrap();
        assert_eq!(
            (report.nwk_fc_start, report.aps_fc_start),
            (60_000, 10_700),
            "EZSP {version}: the margins"
        );
        assert_eq!((report.link_keys_on_ncp, report.children), (1, 1));
        assert_eq!(
            report.notes,
            [(0, restore_note::IEEE_REWRITTEN)],
            "EZSP {version}"
        );
        let n = b.sim.ncp(|n| n.config().clone());
        assert!(n.network_formed);
        assert_eq!(n.restored_eui64, Some(Eui64::from_u64(A_IEEE)));
        assert_eq!(n.custom_eui64, None, "the one-time token is untouched");
        assert_eq!(
            (
                n.network.pan_id,
                n.network.extended_pan_id,
                n.network.radio_channel
            ),
            (0x1A62, [0xDD; 8], 15)
        );
        assert_eq!(
            (n.network.channels, n.network.nwk_update_id),
            (1 << 15 | 1 << 20, 3)
        );
        assert_eq!((n.network_key, n.network_key_sequence), (NKEY, 2));
        assert_eq!((n.tc_link_key, n.tclk_hashed), (ROOT, true));
        assert_eq!((n.nwk_frame_counter, n.aps_frame_counter), (60_000, 10_700));
        assert_eq!(
            n.key_table[0].map(|(e, k, _)| (e, k)),
            Some((Eui64::from_u64(KEYED), LINK))
        );
        assert_eq!(
            n.children
                .iter()
                .map(|c| (c.eui64, c.id))
                .collect::<Vec<_>>(),
            [(Eui64::from_u64(CHILD), 0x4D32)]
        );
        // setChildData left the entry's flags 0x00 (the SDK bug bellows
        // works around): rewritten with 0x80 through the token interface.
        assert_eq!(n.child_table_flags, [0x80], "EZSP {version}");
        let sec = b.sim.ncp(|n| n.security_state()).unwrap();
        assert_eq!(
            sec.bitmask, 0x1B84,
            "no frame counter reset, hashed link keys"
        );
        assert_eq!(
            (sec.preconfigured_key, sec.network_key_sequence_number),
            (ROOT, 2)
        );
        // An NCP reset for the IEEE, another after the child table.
        b.until("the restored network after the last reset", |l| {
            ups(l) >= before + 2
                && l.iter()
                    .rev()
                    .find_map(|e| match e {
                        CoordEvent::Ownership(o) => {
                            Some(matches!(**o, NetworkOwnership::Observed(_)))
                        }
                        _ => None,
                    })
                    .unwrap_or(false)
        });
        match b.coord().status().ownership {
            NetworkOwnership::Observed(id) => {
                assert_eq!(id.ext_pan_id, [0xDD; 8]);
                assert_eq!(id.key_id, Some(NetworkIdentity::key_id_for(&NKEY)));
                assert_eq!(id.coordinator_ieee, A_IEEE.to_le_bytes());
            }
            o => panic!("EZSP {version}: {o:?}"),
        }
        // The restored network exports as the backup, its counters moved
        // on by the margins.
        let again = export(&mut b, true);
        assert_eq!(again[..2], items[..2], "EZSP {version}");
        assert!(again.contains(&BackupItem::NetworkKey {
            key: NKEY,
            seq: 2,
            tx_counter: 60_000,
            active: true
        }));
        assert!(again.contains(&BackupItem::HashedTclkSeed(ROOT)));
        b.finish();
    }
    let found = leaks(&[NKEY, ROOT, LINK]);
    assert!(found.is_empty(), "keys in the log: {found:#?}");
}

#[test]
fn the_one_time_ieee_token_is_written_only_when_confirmed_and_only_once() {
    let mut a = Rig::new(source(13));
    a.next_up();
    let items = export(&mut a, true);
    a.finish();
    // The NCP runs another network and has no token interface.
    let mut b = Rig::new(SimConfig {
        network_formed: true,
        network: NetworkParameters {
            extended_pan_id: [0xEE; 8],
            ..SimConfig::default().network
        },
        ..target(13)
    });
    b.next_up();
    let untouched = |b: &Rig, e: &CoordError, what: &str| {
        assert!(
            matches!(
                e.meaning(),
                CoordError::Backend {
                    kind: Kind::InvalidState,
                    ..
                }
            ),
            "{e:?}"
        );
        assert!(e.to_string().contains(what), "{e}");
        let cmds = b.commands();
        assert!(!cmds.contains(&id::LEAVE_NETWORK) && !cmds.contains(&id::SET_MFG_TOKEN));
        assert!(
            b.sim
                .ncp(|n| n.network_up() && n.config().network.extended_pan_id == [0xEE; 8])
        );
    };
    let e = b
        .admin
        .restore_check(&blob(&items), margins(0))
        .unwrap_err();
    untouched(&b, &e, "confirm_ieee_write_once");
    let e = b
        .admin
        .backup_restore(blob(&items), margins(0))
        .unwrap_err();
    untouched(&b, &e, "only once");
    assert_eq!(b.sim.ncp(|n| n.config().custom_eui64), None);

    // Confirmed: written, the network replaced.
    let once = RestoreOptions {
        ieee_write_once: true,
        ..margins(0)
    };
    let r = b.admin.backup_restore(blob(&items), once).unwrap();
    assert!(
        r.notes.contains(&(0, restore_note::IEEE_WRITTEN_ONCE)),
        "{:?}",
        r.notes
    );
    // No token interface: formNetwork's NWK update ID 0 stays.
    assert!(
        r.notes.contains(&(0, restore_note::UPDATE_ID_NOT_RESTORED)),
        "{:?}",
        r.notes
    );
    let n = b.sim.ncp(|n| n.config().clone());
    assert_eq!(n.custom_eui64, Some(Eui64::from_u64(A_IEEE)));
    assert_eq!(n.network.extended_pan_id, [0xDD; 8]);
    assert_eq!(
        b.coord().status().ncp.unwrap().ieee,
        Some(A_IEEE.to_le_bytes())
    );

    // A backup of another coordinator: the token is written already.
    let mut other = items.clone();
    other[0] = BackupItem::Node {
        ieee: 0x0012_4B00_1234_5678u64.to_le_bytes(),
        ieee_override: false,
    };
    let e = b.admin.backup_restore(blob(&other), once).unwrap_err();
    assert!(
        e.to_string().contains("already holds 000B57FFFEAA0001"),
        "{e}"
    );
    assert_eq!(
        b.sim.ncp(|n| n.config().custom_eui64),
        Some(Eui64::from_u64(A_IEEE))
    );
    // Keeping the NCP's IEEE is allowed.
    let keep = RestoreOptions {
        allow_ieee_change: false,
        ..margins(0)
    };
    let r = b.admin.backup_restore(blob(&other), keep).unwrap();
    assert!(r.notes.contains(&(0, restore_note::IEEE_KEPT)));
    assert_eq!(
        b.coord().status().ncp.unwrap().ieee,
        Some(A_IEEE.to_le_bytes())
    );
    b.finish();
}

#[test]
fn exhausted_frame_counters_and_incomplete_backups_change_nothing() {
    let mut a = Rig::new(source(13));
    a.next_up();
    let items = export(&mut a, true);
    let mut late = items.clone();
    for i in &mut late {
        if let BackupItem::NetworkKey { tx_counter, .. } = i {
            *tx_counter = u32::MAX - 100;
        }
    }
    let e = a
        .admin
        .backup_restore(blob(&late), margins(10_000))
        .unwrap_err();
    assert!(e.to_string().contains("frame counter"), "{e}");
    let bare = export(&mut a, false);
    let e = a.admin.backup_restore(blob(&bare), margins(0)).unwrap_err();
    assert!(e.to_string().contains("no network key"), "{e}");
    assert!(!a.commands().contains(&id::LEAVE_NETWORK));
    assert!(a.sim.ncp(|n| n.network_up()));
    a.finish();
}

#[test]
fn another_stacks_backup_gets_a_new_link_key_root_and_the_large_aps_margin() {
    let mut a = Rig::new(source(13));
    a.next_up();
    // A Z-Stack backup as zigpy-znp writes it: no seed, the devices' own
    // link keys; no TCLK counter (the app passes 2^20 as the margin).
    let items: Vec<BackupItem> = export(&mut a, true)
        .into_iter()
        .filter(|i| {
            !matches!(
                i,
                BackupItem::HashedTclkSeed(_) | BackupItem::TcLinkKey { .. }
            )
        })
        .collect();
    a.finish();
    let mut b = Rig::new(target(13));
    b.next_up();
    let o = RestoreOptions {
        allow_ieee_change: false,
        aps_margin: 1 << 20,
        ..margins(10_000)
    };
    let r = b.admin.backup_restore(blob(&items), o).unwrap();
    assert!(r.notes.contains(&(0, restore_note::TCLK_ROOT_GENERATED)));
    assert!(r.notes.contains(&(0, restore_note::IEEE_KEPT)));
    assert_eq!(r.aps_fc_start, 1 << 20);
    let n = b.sim.ncp(|n| n.config().clone());
    assert_ne!(n.tc_link_key, ROOT);
    assert_ne!(n.tc_link_key, [0; 16]);
    assert_eq!(n.aps_frame_counter, 1 << 20);
    assert_eq!(
        n.key_table[0].map(|(_, k, _)| k),
        Some(LINK),
        "the device's own key"
    );
    b.finish();
}

#[test]
fn a_tx_power_the_ncp_refuses_forms_at_the_default() {
    let mut a = Rig::new(source(13));
    a.next_up();
    let mut items = export(&mut a, true);
    a.finish();
    for i in &mut items {
        if let BackupItem::Network { tx_power, .. } = i {
            *tx_power = Some(20);
        }
    }
    for version in [13, 18] {
        let mut b = Rig::new(SimConfig {
            max_tx_power: Some(8),
            ..target(version)
        });
        b.next_up();
        let keep = RestoreOptions {
            allow_ieee_change: false,
            ..margins(0)
        };
        b.admin.backup_restore(blob(&items), keep).unwrap();
        let forms = b
            .commands()
            .iter()
            .filter(|c| **c == id::FORM_NETWORK)
            .count();
        assert_eq!(forms, 2, "EZSP {version}: refused at 20 dBm, then 8");
        assert_eq!(b.sim.ncp(|n| n.config().network.radio_tx_power), 8);
        assert!(b.sim.ncp(|n| n.network_up()));
        b.finish();
    }
}
