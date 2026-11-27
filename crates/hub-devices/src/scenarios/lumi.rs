//! Xiaomi (`lumi.*`) devices and sleepy-device interviews: the quirk that
//! completes a stuck interview from the model, bounded sleepy retries,
//! identity from unsolicited Basic frames, Xiaomi's malformed Basic
//! structs and On/Off client-cluster reports.

use serde_json::json;

use super::*;
use crate::device::InterviewStep;
use crate::testutil::sim::{Sim, SimDevice};
use crate::testutil::yaml::load_definition;
use crate::transport::ApsIndication;
use crate::zcl::frame::global;
use crate::zcl::{FrameType, TypedValue, ZclType, ZclValue};
use crate::zdo::LogicalType;
use crate::zdo::cluster::{ACTIVE_EP_REQ, BIND_REQ, NODE_DESC_REQ};

const SWITCH: u64 = 0x00158d0000aa0001;
const SWITCH_ID: &str = "zb:00158d0000aa0001";
const WXKG01LM_IN: [u16; 4] = [0x0000, 0x0003, 0x0019, 0xFFFF];
const WXKG01LM_OUT: [u16; 7] = [0x0000, 0x0004, 0x0003, 0x0006, 0x0008, 0x0005, 0x0019];

/// A WXKG01LM (CR2032 sleepy end device).
fn wxkg01lm() -> SimDevice {
    SimDevice::new(SWITCH, 0x6001, LogicalType::EndDevice, false, 0x1037)
        .endpoint(1, 0x0104, 0x0104, &WXKG01LM_IN, &WXKG01LM_OUT)
        .basic("LUMI", "lumi.sensor_switch", 0x03)
}

fn text(s: &str) -> TypedValue {
    TypedValue::new(ZclType::CHAR_STR, ZclValue::String(s.into()))
}

/// Its Device_annce (the moment it joined: asleep when idle).
fn annce(s: &SimDevice) -> ApsIndication {
    let p = crate::zdo::device_annce(1, s.nwk, s.ieee, s.node.mac_capabilities);
    s.ind(0, 0, 0x0013, p)
}

/// The interview requests the hub sent: ZDO by cluster, and whether a
/// bind or a Configure Reporting went out.
fn sent_zdo(sim: &Sim, cluster: u16) -> usize {
    sim.aps()
        .iter()
        .filter(|a| a.profile == 0 && a.cluster == cluster)
        .count()
}

fn configured(sim: &Sim) -> bool {
    sim.aps().iter().any(|a| {
        (a.profile == 0 && a.cluster == BIND_REQ)
            || (a.profile != 0 && a.payload.get(2) == Some(&global::CONFIGURE_REPORTING))
    })
}

/// A WXKG01LM that answers no request but reports its ModelIdentifier
/// (after joining, or when its button is held): its interview completes
/// from the model with the endpoint layout of zhaquirks' signature, as a
/// sleepy LUMI end device on battery, partial and tagged; nothing is
/// configured. The report may come before the Node descriptor request
/// times out or after.
#[test]
fn a_lumi_switch_that_never_answers_completes_from_its_model() {
    for report_first in [false, true] {
        let mut hub = hub_with(&[]);
        let mut sim = Sim::default();
        let mut s = wxkg01lm();
        s.responsive = false;
        let model = s.report(1, 0x0000, None, &[(0x0005, text("lumi.sensor_switch\0"))]);
        let a = annce(&s);
        sim.devices.push(s);
        hub.on_device_joined(SWITCH, 0x6001, T0);
        hub.on_aps_indication(a, T0 + 100);
        sim.run(&mut hub, T0 + 100);
        if report_first {
            hub.on_aps_indication(model.clone(), T0 + 1000);
            assert_eq!(
                hub.device(SWITCH_ID).unwrap().interview,
                InterviewStep::NodeDesc
            );
        }
        // The Node descriptor request times out: it waits for the device.
        hub.tick(T0 + 25_000);
        if !report_first {
            assert_eq!(
                hub.device(SWITCH_ID).unwrap().interview,
                InterviewStep::NodeDesc
            );
            hub.on_aps_indication(model, T0 + 30_000);
        }
        sim.run(&mut hub, T0 + 30_000);

        let d = hub.device(SWITCH_ID).unwrap();
        assert_eq!(
            d.interview,
            InterviewStep::Done,
            "report first: {report_first}"
        );
        assert_eq!(d.record()["interview"], "partial");
        assert_eq!(
            hub.diagnostics(SWITCH_ID).unwrap()["interview_quirk"],
            "lumi"
        );
        assert_eq!(d.fp.model_id.as_deref(), Some("lumi.sensor_switch"));
        assert_eq!(d.fp.manufacturer_name.as_deref(), Some("LUMI"));
        assert_eq!(d.fp.mfr_code, Some(0x1037));
        assert!(d.sleepy());
        assert_eq!(d.record()["power"], "battery");
        assert_eq!(d.fp.endpoints.len(), 1);
        let e = &d.fp.endpoints[0];
        assert_eq!((e.ep, e.profile, e.device_id), (1, 0x0104, 0x0104));
        assert_eq!(
            (&e.in_clusters[..], &e.out_clusters[..]),
            (&WXKG01LM_IN[..], &WXKG01LM_OUT[..])
        );
        assert!(d.entity(1, "action").is_some());
        assert!(!configured(&sim), "no binds or reporting for lumi");
        // Nothing of the interview is left to send.
        let n = sim.aps().len();
        hub.tick(T0 + 60_000);
        hub.on_aps_indication(sim.dev(SWITCH).report(1, 0, None, &[]), T0 + 61_000);
        sim.run(&mut hub, T0 + 61_000);
        assert_eq!(sim.aps().len(), n);
    }
}

/// Not when the definition it would resolve to asks for binds: the
/// interview goes on, the fingerprint as it was.
#[test]
fn the_lumi_quirk_is_skipped_when_the_definition_needs_binds() {
    let mut hub = hub_with(&[]);
    let def = load_definition(
        "zdef: 1\nid: test/bound\nrevision: 1\nvendor: Xiaomi\nmodel: X\n\
         match: [{model: lumi.sensor_switch}]\nconfigure:\n  - bind: {ep: 1, clusters: [0x0006]}\n",
        crate::model::LayerSource::User,
    )
    .unwrap();
    hub.upload_user_definition(def, T0).unwrap();
    let mut sim = Sim::default();
    let mut s = wxkg01lm();
    s.responsive = false;
    let model = s.report(1, 0x0000, None, &[(0x0005, text("lumi.sensor_switch"))]);
    let a = annce(&s);
    sim.devices.push(s);
    hub.on_device_joined(SWITCH, 0x6001, T0);
    hub.on_aps_indication(a, T0 + 100);
    hub.on_aps_indication(model, T0 + 1000);
    hub.tick(T0 + 25_000);
    sim.run(&mut hub, T0 + 25_000);
    let d = hub.device(SWITCH_ID).unwrap();
    assert_eq!(d.interview, InterviewStep::NodeDesc);
    assert!(d.fp.endpoints.iter().all(crate::hub::is_stub));
    assert_eq!(d.fp.manufacturer_name, None);
    assert!(hub.diagnostics(SWITCH_ID).unwrap()["interview_quirk"].is_null());
}

/// A Xiaomi button resolves to the generic tier (zigbee-herdsman-converters'
/// definition needs the `lumi` template): its presses, reports of its On/Off
/// client cluster (0 pressed, 1 released), fire the remote's `off` and `on`
/// events; its battery struct (Basic 0xFF02 with a bogus manufacturer code
/// and a trailing byte) reaches a definition that binds it.
#[test]
fn a_lumi_switch_s_reports_reach_its_entities() {
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    sim.devices.push(wxkg01lm());
    sim.join(&mut hub, SWITCH, T0);
    let d = hub.device(SWITCH_ID).unwrap();
    assert_eq!(d.record()["interview"], "complete");
    assert!(!configured(&sim), "no binds for its events");
    let uid = format!("{SWITCH_ID}:1:action");
    hub.drain_events();
    let press = |v: bool| {
        let tv = TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(v)));
        let mut i = sim.devices[0].report(1, 0x0006, None, &[(0x0000, tv)]);
        // Each frame its own TSN.
        i.payload[1] = u8::from(v);
        i
    };
    hub.on_aps_indication(press(false), T0 + 10_000);
    hub.on_aps_indication(press(true), T0 + 10_200);
    let ev: Vec<String> = fired(&hub.drain_events(), &uid)
        .into_iter()
        .map(|e| e.0)
        .collect();
    assert_eq!(ev, ["off", "on"]);

    let def = load_definition(
        "zdef: 1\nid: test/wxkg01lm\nrevision: 1\nvendor: Xiaomi\nmodel: WXKG01LM\n\
         match: [{model: lumi.sensor_switch}]\nentities:\n  - key: voltage\n    platform: sensor\n    \
         device_class: voltage\n    unit: mV\n    struct: {cluster: 0x0000, attr: 0xFF02, codec: zcl_tagged, tag: 1}\n",
        crate::model::LayerSource::User,
    )
    .unwrap();
    hub.upload_user_definition(def, T0 + 20_000).unwrap();
    // 0xFF02: struct of 6 elements (bool, mV, u16, u40, u16, u8), then a
    // stray byte; manufacturer-specific header with code 0x1234.
    let mut p = vec![0x02, 0xFF, 0x4C, 0x06, 0x00, 0x10, 0x01, 0x21];
    p.extend_from_slice(&3005u16.to_le_bytes());
    p.extend_from_slice(&[
        0x21, 0xA8, 0x13, 0x24, 1, 0, 0, 0, 0, 0x21, 0x6C, 0x00, 0x20, 0x00,
    ]);
    p.push(0x00);
    let ind = sim.devices[0].zcl(
        1,
        0x0000,
        FrameType::Global,
        Some(0x1234),
        0x33,
        global::REPORT_ATTRIBUTES,
        p,
    );
    hub.on_aps_indication(ind, T0 + 30_000);
    assert_eq!(state(&hub, &format!("{SWITCH_ID}:1:voltage")), json!(3005));
    let d = hub.device(SWITCH_ID).unwrap();
    assert!(d.attrs.map.contains_key(&(1, 0x0000, 0xFF02, None)));
}

/// A device that reports its identity unasked (its interview could not read
/// Basic: no Basic in its descriptor) gets it in the fingerprint and
/// resolves to its definition; a later report does not replace what is
/// known, and trailing NULs are dropped.
#[test]
fn an_unsolicited_basic_report_fills_the_identity() {
    const PLUG: u64 = 0x00158d0000bb0001;
    let id = "zb:00158d0000bb0001";
    let mut hub = hub_with(&["innr-sp120"]);
    let mut sim = Sim::default();
    sim.devices.push(
        SimDevice::new(PLUG, 0x7001, LogicalType::Router, true, 0x1166).endpoint(
            1,
            0x0104,
            0x0051,
            &[0x0003, 0x0006],
            &[],
        ),
    );
    sim.join(&mut hub, PLUG, T0);
    let d = hub.device(id).unwrap();
    assert_eq!(d.fp.model_id, None);
    assert!(!d.resolution.layers.iter().any(|l| l.id == "innr/sp-120"));

    let s = sim.dev(PLUG).clone();
    let ind = s.report(
        1,
        0x0000,
        None,
        &[
            (0x0005, text("SP 120\0\0")),
            (0x0004, text("innr")),
            (0x0001, TypedValue::uint(ZclType::U8, 7)),
        ],
    );
    hub.on_aps_indication(ind, T0 + 10_000);
    let d = hub.device(id).unwrap();
    assert_eq!(d.fp.model_id.as_deref(), Some("SP 120"));
    assert_eq!(d.fp.manufacturer_name.as_deref(), Some("innr"));
    assert_eq!(d.fp.app_version, Some(7));
    assert!(d.resolution.layers.iter().any(|l| l.id == "innr/sp-120"));
    assert_eq!(d.record()["model"], "SP 120");

    hub.on_aps_indication(
        s.report(1, 0x0000, None, &[(0x0005, text("x\u{1}"))]),
        T0 + 20_000,
    );
    assert_eq!(
        hub.device(id).unwrap().fp.model_id.as_deref(),
        Some("SP 120")
    );
}

/// A sleepy device that never answers a step: the request is sent again
/// when the device is next heard from, twice in all for the endpoint list,
/// then the interview goes on and ends partial (before, every frame from
/// the device sent it again, forever). Its next Device_annce resumes the
/// interview there with fresh tries (#101).
#[test]
fn a_sleepy_device_that_keeps_failing_a_step_ends_partial() {
    const SENSOR: u64 = 0x00158d0000cc0001;
    let id = "zb:00158d0000cc0001";
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    let mut s = SimDevice::new(SENSOR, 0x7101, LogicalType::EndDevice, false, 0x1234)
        .endpoint(1, 0x0104, 0x0302, &[0x0000, 0x0001, 0x0402], &[])
        .basic("ACME", "TH-2", 0x03)
        .attr(1, 0x0402, 0x0000, TypedValue::int(ZclType::I16, 2137));
    s.zdo_silent.insert(ACTIVE_EP_REQ);
    sim.devices.push(s);
    sim.join(&mut hub, SENSOR, T0);
    let report = sim.dev(SENSOR).report(
        1,
        0x0402,
        None,
        &[(0x0000, TypedValue::int(ZclType::I16, 2200))],
    );
    for k in 1..=6u64 {
        let t = T0 + k * 60_000;
        hub.tick(t);
        hub.on_aps_indication(report.clone(), t + 1000);
        sim.run(&mut hub, t + 1000);
    }
    assert_eq!(sent_zdo(&sim, ACTIVE_EP_REQ), 2);
    let d = hub.device(id).unwrap();
    assert_eq!(d.interview, InterviewStep::Done);
    assert_eq!(d.record()["interview"], "partial");

    // It rejoins and answers now: resumed where it failed.
    sim.dev(SENSOR).zdo_silent.clear();
    sim.log.clear();
    let t = T0 + 3_600_000;
    hub.on_aps_indication(annce(sim.dev(SENSOR)), t);
    sim.run(&mut hub, t);
    assert_eq!(sent_zdo(&sim, NODE_DESC_REQ), 0);
    assert_eq!(sent_zdo(&sim, ACTIVE_EP_REQ), 1);
    let d = hub.device(id).unwrap();
    assert_eq!(d.record()["interview"], "complete");
    assert!(d.entity(1, "temperature").is_some());
    assert_eq!(d.fp.endpoints[0].device_id, 0x0302);
}

/// The interview reads Basic one attribute per request, the identity
/// first; an optional attribute the device does not answer does not make
/// the interview partial.
#[test]
fn basic_is_read_one_attribute_at_a_time() {
    const LAMP: u64 = 0x00158d0000dd0001;
    let id = "zb:00158d0000dd0001";
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    let mut s = SimDevice::new(LAMP, 0x7201, LogicalType::Router, true, 0x1234)
        .endpoint(1, 0x0104, 0x0100, &[0x0000, 0x0006], &[])
        .basic("ACME", "Lamp", 0x01);
    s.silent_attrs.insert((0x0000, 0x4000));
    sim.devices.push(s);
    sim.join(&mut hub, LAMP, T0);
    for k in 1..=4u64 {
        hub.tick(T0 + k * 9_000);
        sim.run(&mut hub, T0 + k * 9_000);
    }
    let reads: Vec<Vec<u8>> = sim
        .aps()
        .iter()
        .filter(|a| {
            a.profile != 0 && a.cluster == 0x0000 && a.payload[2] == global::READ_ATTRIBUTES
        })
        .map(|a| a.payload[3..].to_vec())
        .collect();
    assert!(reads.iter().all(|p| p.len() == 2), "{reads:02x?}");
    assert_eq!(reads[0], [0x05, 0x00]);
    assert_eq!(reads[1], [0x04, 0x00]);
    let d = hub.device(id).unwrap();
    assert_eq!(d.record()["interview"], "complete");
    assert_eq!(d.fp.model_id.as_deref(), Some("Lamp"));
}
