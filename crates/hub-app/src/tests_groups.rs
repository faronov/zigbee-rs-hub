//! Groups (device-support 14.9) on the host core and the simulated NCP:
//! the hub endpoint's `NWK_GROUP_MEMBERSHIP`, APS groupcasts, the host
//! store records, the replay in a new NCP session, the backup.

use std::time::{Duration, Instant};

use hub_ncp::store::{MemFlash, Store};
use serde_json::json;
use zigbee_zrhp::codec::{Reader, Wire};
use zigbee_zrhp::msg::data::{ApsDataRequest, ApsDst};
use zigbee_zrhp::msg::ids;

use crate::App;
use crate::tests::{T, call, cfg, form, ok, rig, session};

fn admin_app(r: &crate::tests::Rig) -> App {
    let app = App::start(r.ncp.clone(), cfg());
    assert!(app.wait(T, |s| s.endpoint_registered));
    app
}

/// `NWK_GROUP_MEMBERSHIP` requests the simulated NCP got: `(op, group, ep)`.
fn memberships(r: &crate::tests::Rig) -> Vec<(u8, u16, u8)> {
    r.sim
        .state()
        .log
        .iter()
        .filter(|(id, _)| *id == ids::NWK_GROUP_MEMBERSHIP)
        .map(|(_, p)| (p[0], u16::from_le_bytes([p[1], p[2]]), p[3]))
        .collect()
}

fn wait(f: impl Fn() -> bool) -> bool {
    let t = Instant::now();
    while !f() && t.elapsed() < T {
        std::thread::sleep(Duration::from_millis(20));
    }
    f()
}

#[test]
fn groups_hub_membership_groupcast_store_replay_and_backup() {
    let flash: Box<dyn hub_ncp::store::Flash> = Box::new(MemFlash::new(32, 4096));
    let r = rig(Store::open(flash).unwrap());
    let app = admin_app(&r);
    let (sid, rx) = session(&app);
    form(&app, sid, &rx, 17);
    let g = call(&app, sid, &rx, 1, json!({"type": "groups/create", "name": "Kitchen"}));
    assert_eq!(ok(&g)["id"], 1);
    assert!(wait(|| memberships(&r).contains(&(1, 1, 1))), "{:?}", memberships(&r));
    // A groupcast goes out as APS group addressing.
    let id = call(&app, sid, &rx, 2, json!({"type": "groups/identify", "group": 1, "time_s": 3}));
    ok(&id);
    let group_aps = || {
        r.sim.state().aps.iter().any(|p| {
            let req = ApsDataRequest::read(&mut Reader::new(p)).unwrap();
            req.dst == ApsDst::Group(1) && req.cluster == 0x0003 && req.tx_options & 1 == 0
        })
    };
    assert!(wait(group_aps), "groupcast Identify");
    // Stored within the flush delay.
    assert!(wait(|| r.ncp.with_store(|s| s.kv_get("grp/0001").is_some())));
    let b = call(&app, sid, &rx, 3, json!({"type": "network/backup"}));
    let backup = ok(&b).clone();
    assert_eq!(backup["metadata"]["internal"]["zigbee_rs"]["groups"][0]["name"], "Kitchen");
    app.shutdown();

    // A new app (a new NCP session as well): restored and replayed.
    let before = memberships(&r).len();
    let app = admin_app(&r);
    let (sid, rx) = session(&app);
    let l = call(&app, sid, &rx, 4, json!({"type": "groups/list"}));
    assert_eq!(ok(&l)[0]["name"], "Kitchen");
    assert!(wait(|| memberships(&r).len() > before), "membership replayed");
    let d = call(&app, sid, &rx, 5, json!({"type": "groups/delete", "group": 1}));
    ok(&d);
    assert!(wait(|| memberships(&r).contains(&(2, 1, 1))));
    assert!(wait(|| r.ncp.with_store(|s| s.kv_get("grp/0001").is_none())), "record deleted");

    // Restoring the backup brings the group back.
    let mut head = backup.clone();
    head["devices"] = json!([]);
    let res = call(&app, sid, &rx, 6, json!({"type": "network/restore", "backup": head, "force": true, "skip_backup": true}));
    ok(&res);
    assert!(wait(|| r.ncp.with_store(|s| s.kv_get("grp/0001").is_some())), "group restored");
    let l = call(&app, sid, &rx, 7, json!({"type": "groups/list"}));
    assert_eq!(ok(&l)[0]["name"], "Kitchen");
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}
