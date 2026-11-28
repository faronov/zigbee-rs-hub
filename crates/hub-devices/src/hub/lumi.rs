//! Xiaomi (`lumi.*`) devices that do not finish an interview.
//!
//! Old Xiaomi end devices (the WXKG01LM button, a CR2032 sleepy end
//! device) answer the ZDO descriptor requests and the Basic reads of an
//! interview poorly or not at all, but report their ModelIdentifier
//! unasked (after joining, when the button is held). zigbee-herdsman
//! completes such an interview from that model (`interviewQuirks`: an end
//! device of manufacturer LUMI, code 0x1037, on battery), zigpy from a
//! signature (`XiaomiQuickInitDevice`: the node descriptor and endpoints of
//! the model, no configure). The hub does both:
//!
//! - a model of [`LAYOUTS`] (zhaquirks' signatures) gets its endpoints
//!   from the table as soon as a step of its interview has failed once;
//! - any other `lumi.*` model when a step finally fails (its tries spent),
//!   with the endpoints it is known to send from (stubs, `stub_endpoint`).
//!
//! Then the missing identity is filled in, the device resolves and the
//! interview is done, `partial` with `interview_quirk: "lumi"` in its
//! diagnostics; its next rejoin resumes the real interview, which may
//! refine it. Not when the resolution needs IAS enrollment, binds or
//! reporting (a definition asking for them would not work on a guessed
//! layout): the interview goes on.
//!
//! The receive side: Basic 0xFF01/0xFF02 come with any manufacturer code
//! (often a bogus one, 0x1234) and are taken as standard attributes
//! ([`report_mfr`]); their malformed lengths are handled by
//! `zcl::frame::parse_report_lenient`; a button's On/Off client-cluster
//! reports fire its remote events ([`Hub::client_on_off`]). Configure
//! sends lumi devices no binds or reporting a definition does not ask for
//! (`configure::build_ops`), and their identity is re-read only when
//! forced (an upgrade, an imported device's probe), not on a rejoin.

use super::*;
use crate::zdo::LogicalType;

/// Manufacturer code of LUMI United Technology.
const LUMI_MFR: u16 = 0x1037;

/// Endpoint layouts by exact model (one endpoint each): `(model,
/// endpoint, profile, device ID, server clusters, client clusters)`, from
/// zhaquirks' signatures
/// (`zhaquirks/xiaomi/mija/*.py`, quick-init devices).
#[allow(clippy::type_complexity)]
const LAYOUTS: &[(&str, u8, u16, u16, &[u16], &[u16])] = &[
    // WXKG01LM button (sensor_switch.py).
    (
        "lumi.sensor_switch",
        1,
        0x0104,
        0x0104,
        &[0x0000, 0x0003, 0x0019, 0xFFFF],
        &[0x0000, 0x0004, 0x0003, 0x0006, 0x0008, 0x0005, 0x0019],
    ),
    // MCCGQ01LM door and window sensor (sensor_magnet.py).
    (
        "lumi.sensor_magnet",
        1,
        0x0104,
        0x0104,
        &[0x0000, 0x0003, 0xFFFF, 0x0019],
        &[0x0000, 0x0004, 0x0003, 0x0006, 0x0008, 0x0005, 0x0019],
    ),
];

/// A Xiaomi device (zigbee-herdsman's `lumi.` model prefix).
pub(crate) fn is_lumi(fp: &crate::fingerprint::Fingerprint) -> bool {
    fp.model_id
        .as_deref()
        .is_some_and(|m| m.starts_with("lumi."))
}

/// The manufacturer code a report's attributes are stored and dispatched
/// under: none for Xiaomi's Basic structs 0xFF01 and 0xFF02, whatever
/// code the frame carries (zigbee-herdsman-converters reads them as
/// standard attributes).
pub(crate) fn report_mfr(
    cluster: u16,
    mfr: Option<u16>,
    recs: &[(u16, TypedValue)],
) -> Option<u16> {
    if cluster == cl::BASIC
        && !recs.is_empty()
        && recs.iter().all(|(a, _)| matches!(a, 0xFF01 | 0xFF02))
    {
        None
    } else {
        mfr
    }
}

/// Fills what the interview did not learn as zigbee-herdsman's quirk
/// does, and the endpoints of [`LAYOUTS`] when only stubs are known.
fn fill(d: &mut Device) {
    let fp = &mut d.fp;
    fp.manufacturer_name.get_or_insert_with(|| "LUMI".into());
    fp.mfr_code.get_or_insert(LUMI_MFR);
    // The Node descriptor is unknown: an end device asleep while idle.
    if fp.logical_type.is_none() {
        fp.logical_type = Some(LogicalType::EndDevice);
        fp.rx_on_when_idle.get_or_insert(false);
        fp.mac_mains.get_or_insert(false);
    }
    if fp.mac_mains != Some(true) {
        fp.power_source.get_or_insert(0x03);
    }
    let layout = LAYOUTS.iter().find(|l| fp.model_id.as_deref() == Some(l.0));
    if let Some(l) = layout.filter(|_| fp.endpoints.iter().all(is_stub)) {
        fp.endpoints = vec![EndpointFp {
            ep: l.1,
            profile: l.2,
            device_id: l.3,
            in_clusters: l.4.to_vec(),
            out_clusters: l.5.to_vec(),
        }];
        d.active_eps = vec![l.1];
    }
}

/// The resolution needs what a guessed layout cannot give: IAS
/// enrollment, or binds or reporting a definition asks for.
fn needs_configure(d: &Device, res: &crate::resolve::Resolution) -> bool {
    d.fp.endpoints
        .iter()
        .any(|e| e.in_clusters.contains(&cl::IAS_ZONE))
        || res
            .configure
            .iter()
            .any(|op| matches!(op, ConfigureOp::Bind { .. } | ConfigureOp::Report { .. }))
        || res.entities.iter().any(|e| {
            e.source != LayerSource::Generic
                && e.def
                    .values()
                    .any(|(_, v)| matches!(v.report, Some(ReportSpec::Config { .. })))
        })
}

impl<T: ZigbeeTransport> Hub<T> {
    /// Completes the interview of a lumi device that is stuck in it (see
    /// the module): a model of [`LAYOUTS`] at any failed try, another
    /// when a step finally failed (`final_failure`). `true`: completed
    /// (the interview's requests are dropped, configure follows).
    pub(crate) fn lumi_quirk(&mut self, id: &str, final_failure: bool, now: Millis) -> bool {
        let Some(d) = self.devices.get(id) else {
            return false;
        };
        let known = LAYOUTS
            .iter()
            .any(|l| d.fp.model_id.as_deref() == Some(l.0));
        if !is_lumi(&d.fp)
            || d.gp.is_some()
            || !(known || final_failure)
            || !matches!(
                d.interview,
                InterviewStep::NodeDesc
                    | InterviewStep::ActiveEp
                    | InterviewStep::SimpleDesc(_)
                    | InterviewStep::Basic(_)
            )
        {
            return false;
        }
        // Filled in place (a copy of the device would cost the firmware
        // its clone code), put back when the quirk does not apply.
        let Some(d) = self.devices.get_mut(id) else {
            return false;
        };
        let saved = (d.fp.clone(), d.active_eps.clone());
        fill(d);
        let d = &self.devices[id];
        let res = self.resolution_of(d);
        if needs_configure(d, &res) {
            let d = self.devices.get_mut(id).unwrap();
            (d.fp, d.active_eps) = saved;
            return false;
        }
        self.pending
            .retain(|_, p| p.device != id || !matches!(p.purpose, Purpose::Interview(_)));
        let Some(d) = self.devices.get_mut(id) else {
            return false;
        };
        d.queue
            .retain(|x| !matches!(x.purpose, Purpose::Interview(_)));
        d.interview_partial = true;
        let step = d.interview;
        if !self.interview_quirks.iter().any(|x| x == id) {
            self.interview_quirks.push(id.to_string());
        }
        self.note_interview_failed(id, step, now);
        self.materialize(id, res, now);
        self.start_configure(id, now);
        true
    }

    /// Xiaomi buttons ignore the direction: they report the attribute of
    /// their On/Off *client* cluster (OnOff 0x0000: 0 pressed, 1
    /// released) instead of sending commands. Such a report from an
    /// endpoint that has On/Off as a client cluster only is taken as the
    /// command it stands for (Off, On), so the remote's events (5.4) and
    /// a definition's command triggers fire.
    pub(crate) fn client_on_off(
        &mut self,
        id: &str,
        ep: u8,
        tsn: u8,
        recs: &[(u16, TypedValue)],
        now: Millis,
    ) {
        let client = self
            .devices
            .get(id)
            .and_then(|d| d.fp.endpoint(ep))
            .is_some_and(|e| {
                e.out_clusters.contains(&cl::ON_OFF) && !e.in_clusters.contains(&cl::ON_OFF)
            });
        if !client {
            return;
        }
        for (attr, v) in recs {
            if let (0x0000, ZclValue::Bool(Some(on))) = (attr, &v.value) {
                let mut f =
                    ZclFrame::command(FrameType::Cluster, tsn, u8::from(*on), None, Vec::new());
                f.header.disable_default_response = true;
                self.dispatch_command(id, ep, cl::ON_OFF, &f, now);
            }
        }
    }
}
