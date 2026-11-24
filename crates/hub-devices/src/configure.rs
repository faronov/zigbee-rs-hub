//! Configure (spec 5.3, 6.8, 10.3): binds, reporting, initial reads, IAS
//! enrollment, Poll Control, definition ops, Tuya flags. Every op has an
//! idempotence key; results are recorded per key so a failed configure is
//! retried without repeating what worked, and nothing is redone unless the
//! configure hash changed.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::device::{Device, InterviewStep, Millis};
use crate::hub::control::Then;
use crate::hub::{Hub, HubEvent, OpKind, OpRun, Purpose};
use crate::model::*;
use crate::transport::{Request, ZigbeeTransport};
use crate::values;
use crate::zcl::frame::{self as zf, ReportingConfig, global};
use crate::zcl::{FrameType, PROFILE_GP, TypedValue, ZclFrame, ZclType, ZclValue, cluster as cl};
use crate::zdo::{self, BindDst};

fn mfr_s(m: Option<u16>) -> String {
    m.map_or("-".into(), |m| format!("{m:04x}"))
}

fn first_ep_with(d: &Device, cluster: u16) -> u8 {
    let app = d.fp.endpoints.iter().filter(|e| e.profile != PROFILE_GP);
    app.clone()
        .find(|e| e.in_clusters.contains(&cluster) || e.out_clusters.contains(&cluster))
        .or_else(|| app.clone().next())
        .map_or(1, |e| e.ep)
}

/// Builds the configure op list of a device (in execution order).
pub fn build_ops(
    d: &Device,
    coordinator: u64,
    hub_ep: u8,
    env: &dyn crate::expr::ExprEnv,
) -> Vec<OpRun> {
    let mut ops: Vec<OpRun> = Vec::new();
    let push = |ops: &mut Vec<OpRun>, key: String, kind: OpKind| {
        if !ops.iter().any(|o| o.key == key) {
            ops.push(OpRun { key, kind });
        }
    };
    if d.gp.is_some() {
        return ops;
    }
    let tuya = d.resolution.tuya.clone();
    let basic_ep =
        d.fp.endpoints
            .iter()
            .find(|e| e.in_clusters.contains(&cl::BASIC))
            .map_or_else(|| first_ep_with(d, cl::BASIC), |e| e.ep);
    if tuya.as_ref().is_some_and(|t| t.magic_packet) {
        let attrs = vec![0x0004, 0x0000, 0x0001, 0x0005, 0x0007, 0xFFFE];
        push(
            &mut ops,
            format!("read:{basic_ep}:0000:-:magic"),
            OpKind::Read {
                ep: basic_ep,
                cluster: cl::BASIC,
                mfr: None,
                attrs,
            },
        );
    }
    // Reporting configs by (ep, cluster, mfr).
    let sleepy_cap = (d.power() != PowerKind::Mains || d.sleepy()).then(|| {
        d.resolution
            .device
            .checkin_s
            .map_or(21600, |c| c.min(21600)) as u16
    });
    let mut reports: BTreeMap<(u8, u16, Option<u16>), Vec<ReportingConfig>> = BTreeMap::new();
    let mut reads: BTreeMap<(u8, u16, Option<u16>), Vec<u16>> = BTreeMap::new();
    let mut binds: Vec<(u8, u16)> = Vec::new();
    for e in d.entities.iter().filter(|e| !e.orphaned) {
        for (_, v) in e.r.def.values() {
            let (cluster, attr, ty, mfr) = match &v.binding {
                Binding::Zcl(z) => (z.cluster, z.attr, z.ty, z.mfr),
                Binding::Struct {
                    cluster, attr, mfr, ..
                } => (*cluster, *attr, ZclType::OCTET_STR, *mfr),
                _ => continue,
            };
            let ep = e.r.ep;
            if let Some(ReportSpec::Config { min, max, change }) = v.report {
                let max = sleepy_cap.map_or(max, |c| max.min(c.max(min)));
                let rec = ReportingConfig {
                    attr,
                    ty,
                    min,
                    max,
                    change,
                };
                let l = reports.entry((ep, cluster, mfr)).or_default();
                if !l.iter().any(|r| r.attr == attr) {
                    l.push(rec);
                }
                if !binds.contains(&(ep, cluster)) {
                    binds.push((ep, cluster));
                }
            }
            if e.r.def.access_of(v).read {
                let l = reads.entry((ep, cluster, mfr)).or_default();
                if !l.contains(&attr) {
                    l.push(attr);
                }
            }
        }
        // Client clusters whose commands become events (5.4).
        for t in &e.r.def.triggers {
            if let TriggerSource::Cmd { ep, cluster, .. } = t.source {
                for epd in
                    d.fp.endpoints.iter().filter(|x| {
                        x.out_clusters.contains(&cluster) && ep.is_none_or(|e| e == x.ep)
                    })
                {
                    if !binds.contains(&(epd.ep, cluster)) {
                        binds.push((epd.ep, cluster));
                    }
                }
            }
        }
    }
    for (ep, cluster, attrs) in crate::generic::probe_reads(&d.fp) {
        let l = reads.entry((ep, cluster, None)).or_default();
        for a in attrs {
            if !l.contains(&a) {
                l.push(a);
            }
        }
    }
    let has_poll =
        d.fp.endpoints
            .iter()
            .find(|e| e.in_clusters.contains(&cl::POLL_CONTROL))
            .map(|e| e.ep);
    if let Some(ep) = has_poll {
        binds.push((ep, cl::POLL_CONTROL));
    }
    if tuya.as_ref().is_some_and(|t| t.bind_basic) {
        binds.push((basic_ep, cl::BASIC));
    }
    for (ep, cluster) in binds {
        push(
            &mut ops,
            format!("bind:{ep}:{cluster:04x}"),
            OpKind::Bind { ep, cluster },
        );
    }
    for ((ep, cluster, mfr), recs) in reports {
        for chunk in recs.chunks(3) {
            let desc: Vec<String> = chunk
                .iter()
                .map(|r| format!("{:04x}/{}/{}/{}/{:?}", r.attr, r.ty, r.min, r.max, r.change))
                .collect();
            push(
                &mut ops,
                format!(
                    "report:{ep}:{cluster:04x}:{}:{}",
                    mfr_s(mfr),
                    desc.join(",")
                ),
                OpKind::Report {
                    ep,
                    cluster,
                    mfr,
                    records: chunk.to_vec(),
                },
            );
        }
    }
    // IAS enrollment: CIE address before the reads of ZoneState/ZoneType.
    for epd in
        d.fp.endpoints
            .iter()
            .filter(|e| e.in_clusters.contains(&cl::IAS_ZONE))
    {
        push(
            &mut ops,
            format!("write:{}:0500:-:0010={coordinator:016x}", epd.ep),
            OpKind::Write {
                ep: epd.ep,
                cluster: cl::IAS_ZONE,
                mfr: None,
                attr: 0x0010,
                value: TypedValue::uint(ZclType::IEEE, coordinator),
            },
        );
    }
    for ((ep, cluster, mfr), attrs) in reads {
        if cluster == cl::TUYA {
            continue;
        }
        for chunk in attrs.chunks(5) {
            let desc: Vec<String> = chunk.iter().map(|a| format!("{a:04x}")).collect();
            push(
                &mut ops,
                format!("read:{ep}:{cluster:04x}:{}:{}", mfr_s(mfr), desc.join(",")),
                OpKind::Read {
                    ep,
                    cluster,
                    mfr,
                    attrs: chunk.to_vec(),
                },
            );
        }
    }
    // Definition ops (6.8).
    for op in &d.resolution.configure {
        match op {
            ConfigureOp::Bind { ep, clusters } => {
                for &c in clusters {
                    let ep = ep.unwrap_or_else(|| first_ep_with(d, c));
                    push(
                        &mut ops,
                        format!("bind:{ep}:{c:04x}"),
                        OpKind::Bind { ep, cluster: c },
                    );
                }
            }
            ConfigureOp::Write {
                ep,
                cluster,
                attr,
                ty,
                value,
                mfr,
            } => {
                let ep = ep.unwrap_or_else(|| first_ep_with(d, *cluster));
                if let Ok(v) = values::to_zcl(*ty, value) {
                    push(
                        &mut ops,
                        format!("write:{ep}:{cluster:04x}:{}:{attr:04x}={v:?}", mfr_s(*mfr)),
                        OpKind::Write {
                            ep,
                            cluster: *cluster,
                            mfr: *mfr,
                            attr: *attr,
                            value: TypedValue::new(*ty, v),
                        },
                    );
                }
            }
            ConfigureOp::Read {
                ep,
                cluster,
                attrs,
                mfr,
            } => {
                let ep = ep.unwrap_or_else(|| first_ep_with(d, *cluster));
                for chunk in attrs.chunks(5) {
                    let desc: Vec<String> = chunk.iter().map(|a| format!("{a:04x}")).collect();
                    push(
                        &mut ops,
                        format!("read:{ep}:{cluster:04x}:{}:{}", mfr_s(*mfr), desc.join(",")),
                        OpKind::Read {
                            ep,
                            cluster: *cluster,
                            mfr: *mfr,
                            attrs: chunk.to_vec(),
                        },
                    );
                }
            }
            ConfigureOp::Report {
                ep,
                cluster,
                attr,
                ty,
                report,
                mfr,
            } => {
                let ep = ep.unwrap_or_else(|| first_ep_with(d, *cluster));
                let ReportSpec::Config { min, max, change } = *report else {
                    continue;
                };
                let rec = ReportingConfig {
                    attr: *attr,
                    ty: *ty,
                    min,
                    max,
                    change,
                };
                push(
                    &mut ops,
                    format!(
                        "report:{ep}:{cluster:04x}:{}:{attr:04x}/{ty}/{min}/{max}/{change:?}",
                        mfr_s(*mfr)
                    ),
                    OpKind::Report {
                        ep,
                        cluster: *cluster,
                        mfr: *mfr,
                        records: vec![rec],
                    },
                );
                push(
                    &mut ops,
                    format!("bind:{ep}:{cluster:04x}"),
                    OpKind::Bind {
                        ep,
                        cluster: *cluster,
                    },
                );
            }
            ConfigureOp::Cmd {
                ep,
                cluster,
                id,
                mfr,
                payload,
            } => {
                let ep = ep.unwrap_or_else(|| first_ep_with(d, *cluster));
                let payload = match payload {
                    PressPayload::Bytes(b) => Ok(b.clone()),
                    PressPayload::Expr(x) => match x.eval(&crate::expr::ExprCtx {
                        x: crate::expr::Value::Null,
                        payload: None,
                        last: None,
                        env,
                    }) {
                        crate::expr::Value::Bytes(b) => Ok(b),
                        _ => Err("payload expression did not yield bytes".to_string()),
                    },
                };
                match payload {
                    Ok(payload) => push(
                        &mut ops,
                        format!(
                            "cmd:{ep}:{cluster:04x}:{}:{id:02x}:{}",
                            mfr_s(*mfr),
                            crate::expr::hex(&payload)
                        ),
                        OpKind::Cmd {
                            ep,
                            cluster: *cluster,
                            mfr: *mfr,
                            id: *id,
                            payload,
                            no_rsp: *cluster == cl::TUYA,
                        },
                    ),
                    Err(reason) => push(
                        &mut ops,
                        format!("cmd:{ep}:{cluster:04x}:{}:{id:02x}:invalid", mfr_s(*mfr)),
                        OpKind::Invalid { reason },
                    ),
                }
            }
            ConfigureOp::Group { ep, group } => {
                let ep = ep.unwrap_or(hub_ep);
                push(
                    &mut ops,
                    format!("group:{ep}:{group:04x}"),
                    OpKind::Group { ep, group: *group },
                );
            }
            ConfigureOp::Checkin { interval_qs } => {
                let ep = has_poll.unwrap_or_else(|| first_ep_with(d, cl::POLL_CONTROL));
                push(
                    &mut ops,
                    format!("write:{ep}:0020:-:0000={interval_qs}"),
                    OpKind::Write {
                        ep,
                        cluster: cl::POLL_CONTROL,
                        mfr: None,
                        attr: 0x0000,
                        value: TypedValue::uint(ZclType::U32, u64::from(*interval_qs)),
                    },
                );
            }
        }
    }
    if let Some(t) = &tuya {
        let ep = first_ep_with(d, cl::TUYA);
        if t.mcu_version {
            push(
                &mut ops,
                format!("cmd:{ep}:ef00:-:10"),
                OpKind::Cmd {
                    ep,
                    cluster: cl::TUYA,
                    mfr: None,
                    id: crate::tuya::cmd::MCU_VERSION_REQUEST,
                    payload: vec![0, 0],
                    no_rsp: true,
                },
            );
        }
        if t.query_on_configure {
            push(
                &mut ops,
                format!("cmd:{ep}:ef00:-:03"),
                OpKind::Cmd {
                    ep,
                    cluster: cl::TUYA,
                    mfr: None,
                    id: crate::tuya::cmd::DATA_QUERY,
                    payload: Vec::new(),
                    no_rsp: true,
                },
            );
        }
    }
    ops
}

/// Configure hash (3.4): SHA-256 over the op keys, first 16 bytes.
pub fn configure_hash(ops: &[OpRun]) -> String {
    let mut h = Sha256::new();
    for o in ops {
        h.update(o.key.as_bytes());
        h.update(b"\n");
    }
    crate::expr::hex(&h.finalize()[..16])
}

impl<T: ZigbeeTransport> Hub<T> {
    pub(crate) fn start_configure(&mut self, id: &str, now: Millis) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        if d.gp.is_some() {
            return;
        }
        d.interview = InterviewStep::Configure;
        self.emit(HubEvent::InterviewProgress {
            device: id.to_string(),
            step: "configure".into(),
        });
        if self.devices[id].resolution.layers.is_empty() {
            self.resolve_device(id, now);
        }
        self.redo_configure(id, now);
    }

    /// `devices/reconfigure`: forgets all results and runs everything.
    pub fn reconfigure(&mut self, id: &str, now: Millis) -> Result<(), crate::hub::ApiError> {
        let d = self
            .devices
            .get_mut(id)
            .ok_or_else(|| crate::hub::ApiError::new("not_found", id.to_string()))?;
        d.configure.done.clear();
        d.configure.hash = None;
        self.redo_configure(id, now);
        Ok(())
    }

    /// Rebuilds the op list and queues what has not succeeded yet.
    pub(crate) fn redo_configure(&mut self, id: &str, now: Millis) {
        let (coord, hub_ep, tz) = (
            self.cfg.coordinator_ieee,
            self.cfg.hub_endpoint,
            self.cfg.tz_offset_s,
        );
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        if d.gp.is_some()
            || matches!(
                d.interview,
                InterviewStep::NodeDesc
                    | InterviewStep::ActiveEp
                    | InterviewStep::SimpleDesc(_)
                    | InterviewStep::Basic(_)
            )
        {
            return;
        }
        let ops = build_ops(d, coord, hub_ep, &crate::hub::DevEnv { dev: d, now, tz });
        d.configure.hash = Some(configure_hash(&ops));
        let queued: Vec<String> = d.configure.queue.iter().map(|o| o.key.clone()).collect();
        for op in ops {
            if d.configure.done.get(&op.key) == Some(&true) || queued.contains(&op.key) {
                continue;
            }
            d.configure.queue.push_back(op);
        }
        if let Some(t) = &d.resolution.tuya {
            if let Some(iv) = t.query_interval_s {
                d.tuya_query_at.get_or_insert(now + u64::from(iv) * 1000);
            }
        }
        self.pump_configure(id, now);
    }

    /// Sends the next op if none is in flight.
    pub(crate) fn pump_configure(&mut self, id: &str, now: Millis) {
        loop {
            let Some(d) = self.devices.get_mut(id) else {
                return;
            };
            if d.configure.in_flight {
                return;
            }
            let Some(op) = d.configure.queue.pop_front() else {
                if d.interview == InterviewStep::Configure {
                    d.interview = InterviewStep::Done;
                    self.emit(HubEvent::InterviewProgress {
                        device: id.to_string(),
                        step: "done".into(),
                    });
                    self.emit(HubEvent::DeviceUpdated(id.to_string()));
                }
                return;
            };
            if d.configure.done.get(&op.key) == Some(&true) {
                continue;
            }
            d.configure.in_flight = true;
            let ieee = d.ieee().unwrap_or(0);
            let key = op.key.clone();
            let purpose = Purpose::Configure(key.clone());
            match op.kind {
                OpKind::Invalid { reason } => {
                    self.configure_done(id, &key, false, format!("INVALID: {reason}"), now);
                    continue;
                }
                OpKind::Bind { ep, cluster } => {
                    let tsn = self.next_zdo_tsn();
                    let p = zdo::bind_req(
                        tsn,
                        ieee,
                        ep,
                        cluster,
                        BindDst::Ieee(self.cfg.coordinator_ieee, self.cfg.hub_endpoint),
                    );
                    self.send_zdo(id, zdo::cluster::BIND_REQ, p, purpose, now);
                }
                OpKind::Report {
                    ep,
                    cluster,
                    mfr,
                    records,
                } => {
                    let tsn = self.next_zcl_tsn();
                    let payload = zf::configure_reporting(&records).unwrap_or_default();
                    let f = ZclFrame::command(
                        FrameType::Global,
                        tsn,
                        global::CONFIGURE_REPORTING,
                        mfr,
                        payload,
                    );
                    self.send_zcl(id, ep, cluster, f, purpose, now);
                }
                OpKind::Read {
                    ep,
                    cluster,
                    mfr,
                    attrs,
                } => self.read_attrs(id, ep, cluster, mfr, &attrs, purpose, now),
                OpKind::Write {
                    ep,
                    cluster,
                    mfr,
                    attr,
                    value,
                } => {
                    let tsn = self.next_zcl_tsn();
                    let payload = zf::write_attributes(&[(attr, value)]).unwrap_or_default();
                    let f = ZclFrame::command(
                        FrameType::Global,
                        tsn,
                        global::WRITE_ATTRIBUTES,
                        mfr,
                        payload,
                    );
                    self.send_zcl(id, ep, cluster, f, purpose, now);
                }
                OpKind::Cmd {
                    ep,
                    cluster,
                    mfr,
                    id: cmd,
                    payload,
                    no_rsp,
                } => {
                    let tsn = self.next_zcl_tsn();
                    let mut f = ZclFrame::command(FrameType::Cluster, tsn, cmd, mfr, payload);
                    f.header.disable_default_response = no_rsp;
                    if no_rsp {
                        self.send_zcl(id, ep, cluster, f, Purpose::None, now);
                        self.configure_done(id, &key, true, "SENT".into(), now);
                        continue;
                    }
                    self.send_zcl(id, ep, cluster, f, purpose, now);
                }
                OpKind::Group { ep, group } => {
                    // `configure_done` follows the outcome.
                    let req = Request::Group {
                        endpoint: ep,
                        group,
                        add: true,
                    };
                    let then = Then::Configure {
                        device: id.to_string(),
                        key,
                    };
                    self.control(req, then, None, now);
                }
            }
            return;
        }
    }

    /// Records an op result.
    pub(crate) fn configure_done(
        &mut self,
        id: &str,
        key: &str,
        ok: bool,
        status: String,
        now: Millis,
    ) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        d.configure.in_flight = false;
        d.configure.done.insert(key.to_string(), ok);
        d.diag.configure.insert(key.to_string(), status);
        if ok {
            d.configure.backoff_s = 0;
        } else {
            let b = if d.configure.backoff_s == 0 {
                60
            } else {
                (d.configure.backoff_s * 2).min(3600)
            };
            d.configure.backoff_s = b;
            d.configure.retry_at = Some(if d.sleepy() { now } else { now + b * 1000 });
            if !d.warnings.iter().any(|w| w == "configure_failed") {
                d.warnings.push("configure_failed".into());
            }
        }
        if d.configure.done.values().all(|v| *v) {
            d.warnings.retain(|w| w != "configure_failed");
        }
        self.pump_configure(id, now);
    }
}

/// Encodes a value for a ZCL write from an entity's binding (commands use
/// it too).
pub fn zcl_write_value(ty: ZclType, v: &crate::expr::Value) -> Result<ZclValue, String> {
    values::to_zcl(ty, v)
}
