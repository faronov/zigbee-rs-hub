//! ZCL console (spec 14.7): raw foundation and cluster requests to one
//! device endpoint, for writing and debugging definitions. The device's
//! answer comes back decoded ([`HubEvent::ConsoleResult`]); the request
//! goes through the normal send path (queued for a sleepy device, APS
//! confirm, timeouts), and the answer is also processed as usual (a read
//! updates entity states). Also the live frame tap (topic `frames`) and
//! Tuya datapoint sends.

use serde_json::Value as J;

use crate::commands::CommandAck;
use crate::device::{DeviceKind, Millis, ts};
use crate::hub::{ApiError, Hub, HubEvent, Purpose};
use crate::model::{Binding, TuyaType};
use crate::transport::{ApsIndication, ApsRequest, ZigbeeTransport};
use crate::tuya::{self, DpRecord};
use crate::values;
use crate::zcl::frame::{self as zf, global};
use crate::zcl::types::{Reader, decode_value};
use crate::zcl::{
    CodecError, Direction, FrameType, ReportingConfig, Status, TypedValue, ZclFrame, ZclHeader,
    ZclType, cluster as cl,
};

/// Foundation commands of the console beyond [`global`] (ZCL r8 Table 2-3).
mod disc {
    pub const ATTRIBUTES: u8 = 0x0C;
    pub const ATTRIBUTES_RSP: u8 = 0x0D;
    pub const COMMANDS_RECEIVED: u8 = 0x11;
    pub const COMMANDS_RECEIVED_RSP: u8 = 0x12;
    pub const COMMANDS_GENERATED: u8 = 0x13;
    pub const COMMANDS_GENERATED_RSP: u8 = 0x14;
}

/// What to send.
#[derive(Debug, Clone, PartialEq)]
pub enum ConsoleOp {
    /// Read Attributes.
    Read(Vec<u16>),
    /// Write Attributes (one record). `ty` `None`: from the resolved
    /// definition's binding of that attribute.
    Write {
        /// Attribute.
        attr: u16,
        /// Type.
        ty: Option<ZclType>,
        /// Value (JSON number, bool, string; hex string for octets).
        value: J,
    },
    /// Configure Reporting (one record).
    ConfigureReporting {
        /// Attribute.
        attr: u16,
        /// Type (`None`: from the definition).
        ty: Option<ZclType>,
        /// Minimum interval, s.
        min: u16,
        /// Maximum interval, s.
        max: u16,
        /// Reportable change in raw units (analog types).
        change: Option<f64>,
    },
    /// Read Reporting Configuration.
    ReadReporting(Vec<u16>),
    /// Discover Attributes (`what` 0), Commands Received (1) or Generated (2).
    Discover {
        /// 0 attributes, 1 commands received, 2 commands generated.
        what: u8,
        /// First attribute / command ID.
        start: u16,
        /// Most IDs in the answer.
        max: u8,
    },
    /// Any command: cluster-specific or foundation, raw payload.
    Command {
        /// Command ID.
        cmd: u8,
        /// Payload after the ZCL header.
        payload: Vec<u8>,
        /// Foundation instead of cluster-specific.
        global: bool,
        /// Server to client (e.g. answering a device's request).
        to_client: bool,
        /// Disable Default Response (then only a cluster answer completes it).
        no_default_rsp: bool,
    },
}

/// A console request.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsoleReq {
    /// Device ID.
    pub device: String,
    /// Device endpoint.
    pub ep: u8,
    /// Cluster.
    pub cluster: u16,
    /// Manufacturer code.
    pub mfr: Option<u16>,
    /// Operation.
    pub op: ConsoleOp,
}

/// A JSON object from pairs: one shared builder instead of `json!`
/// expanding every insert inline (flash).
#[inline(never)]
fn obj<const N: usize>(pairs: [(&str, J); N]) -> J {
    // Inserts, not `collect`: a collected map brings a stable sort (~15 KB).
    let mut m = serde_json::Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), v);
    }
    J::Object(m)
}

fn bad(m: impl Into<String>) -> ApiError {
    ApiError::new("invalid_value", m)
}

/// Lowercase hex.
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Parses hex (spaces allowed).
pub fn unhex(s: &str) -> Result<Vec<u8>, ApiError> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let s = s.strip_prefix("0x").unwrap_or(&s);
    if !s.len().is_multiple_of(2) {
        return Err(bad("hex payload needs an even number of digits"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| bad("invalid hex payload")))
        .collect()
}

/// The decoded body of a device's answer to a console request.
pub fn decode_answer(f: &ZclFrame) -> J {
    let h = &f.header;
    let mut o = obj([
        ("tsn", h.tsn.into()),
        ("frame", frame_kind(h.frame_type)),
        ("command", h.cmd.into()),
        ("manufacturer", h.mfr.into()),
        ("payload", hex(&f.payload).into()),
    ]);
    if h.frame_type == FrameType::Global {
        if let Ok(v) = decode_global(h.cmd, &f.payload) {
            o.as_object_mut().unwrap().extend(v);
        }
    }
    o
}

fn frame_kind(t: FrameType) -> J {
    J::from(if t == FrameType::Global {
        "global"
    } else {
        "cluster"
    })
}

fn st(s: u8) -> J {
    J::from(Status(s).name())
}

fn decode_global(cmd: u8, p: &[u8]) -> Result<serde_json::Map<String, J>, CodecError> {
    let mut r = Reader::new(p);
    let mut m = serde_json::Map::new();
    let mut recs = Vec::new();
    match cmd {
        global::READ_ATTRIBUTES_RSP => {
            for x in zf::parse_read_attributes_rsp(p)? {
                recs.push(obj([
                    ("attribute", x.attr.into()),
                    ("status", x.status.name().into()),
                    ("type", x.value.as_ref().map(|v| v.ty.0).into()),
                    (
                        "value",
                        x.value
                            .as_ref()
                            .map_or(J::Null, |v| values::from_zcl(&v.value).to_json()),
                    ),
                ]));
            }
        }
        global::WRITE_ATTRIBUTES_RSP | global::CONFIGURE_REPORTING_RSP => {
            if p.len() == 1 {
                recs.push(obj([("status", st(p[0]))]));
            } else {
                while r.remaining() > 0 {
                    let s = r.u8()?;
                    if cmd == global::CONFIGURE_REPORTING_RSP {
                        r.u8()?;
                    }
                    recs.push(obj([("status", st(s)), ("attribute", r.u16()?.into())]));
                }
            }
        }
        global::READ_REPORTING_CONFIG_RSP => {
            while r.remaining() > 0 {
                let s = r.u8()?;
                let dir = r.u8()?;
                let attr = r.u16()?;
                let mut e = obj([
                    ("status", st(s)),
                    ("attribute", attr.into()),
                    ("direction", dir.into()),
                ]);
                if s == 0 && dir == 0 {
                    let ty = ZclType(r.u8()?);
                    e["type"] = ty.0.into();
                    e["min"] = r.u16()?.into();
                    e["max"] = r.u16()?.into();
                    if ty.is_analog() {
                        e["change"] = values::from_zcl(&decode_value(ty, &mut r)?).to_json();
                    }
                } else if s == 0 {
                    e["timeout"] = r.u16()?.into();
                }
                recs.push(e);
            }
        }
        global::DEFAULT_RSP => {
            let (c, s) = zf::parse_default_rsp(p)?;
            m.insert("for_command".into(), c.into());
            m.insert("status".into(), s.name().into());
        }
        disc::ATTRIBUTES_RSP => {
            m.insert("complete".into(), J::from(r.u8()? != 0));
            while r.remaining() > 0 {
                recs.push(obj([
                    ("attribute", r.u16()?.into()),
                    ("type", r.u8()?.into()),
                ]));
            }
        }
        disc::COMMANDS_RECEIVED_RSP | disc::COMMANDS_GENERATED_RSP => {
            m.insert("complete".into(), J::from(r.u8()? != 0));
            while r.remaining() > 0 {
                recs.push(r.u8()?.into());
            }
        }
        _ => return Ok(m),
    }
    if !recs.is_empty() {
        m.insert("records".into(), J::Array(recs));
    }
    Ok(m)
}

/// Foundation commands that answer a console request.
fn is_answer(cmd: u8) -> bool {
    matches!(
        cmd,
        global::READ_ATTRIBUTES_RSP
            | global::WRITE_ATTRIBUTES_RSP
            | global::CONFIGURE_REPORTING_RSP
            | global::READ_REPORTING_CONFIG_RSP
            | global::DEFAULT_RSP
            | disc::ATTRIBUTES_RSP
            | disc::COMMANDS_RECEIVED_RSP
            | disc::COMMANDS_GENERATED_RSP
    )
}

/// Tuya datapoints of an EF00 frame, decoded (DP viewer).
fn frame_dps(f: &ZclFrame) -> Option<J> {
    if f.header.frame_type != FrameType::Cluster || !tuya::is_data_command(f.header.cmd) {
        return None;
    }
    let (seq, recs) = tuya::parse_data(&f.payload).ok()?;
    let dps = recs.iter().map(|r| {
        obj([
            ("dp", r.dp.into()),
            ("type", r.ty.into()),
            ("value", tuya::dp_value(r.ty, &r.data).to_json()),
            ("data", hex(&r.data).into()),
        ])
    });
    Some(obj([("seq", seq.into()), ("dps", J::Array(dps.collect()))]))
}

/// One frame for the `frames` topic.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn frame_event(
    device: &str,
    rx: bool,
    ep: u8,
    profile: u16,
    cluster: u16,
    payload: &[u8],
    lqi: Option<u8>,
    now: Millis,
) -> J {
    let mut o = obj([
        ("topic", "frame".into()),
        ("device", device.into()),
        ("dir", if rx { "rx" } else { "tx" }.into()),
        ("endpoint", ep.into()),
        ("profile", profile.into()),
        ("cluster", cluster.into()),
        ("hex", hex(payload).into()),
        ("lqi", lqi.into()),
        ("time", ts(now).into()),
    ]);
    if profile != 0 {
        if let Ok(f) = ZclFrame::parse(payload) {
            let h = &f.header;
            o["zcl"] = obj([
                ("frame", frame_kind(h.frame_type)),
                ("command", h.cmd.into()),
                ("manufacturer", h.mfr.into()),
                ("tsn", h.tsn.into()),
                (
                    "to_client",
                    (h.direction == Direction::ServerToClient).into(),
                ),
            ]);
            if cluster == cl::TUYA {
                if let Some(t) = frame_dps(&f) {
                    o["tuya"] = t;
                }
            }
        }
    }
    o
}

impl<T: ZigbeeTransport> Hub<T> {
    /// Frame tap: an incoming frame (when `frames` has subscribers).
    pub(crate) fn tap_rx(&mut self, id: &str, ind: &ApsIndication, now: Millis) {
        if self.frame_tap {
            let e = frame_event(
                id,
                true,
                ind.src_ep,
                ind.profile,
                ind.cluster,
                &ind.payload,
                Some(ind.lqi),
                now,
            );
            self.emit(HubEvent::Frame(e));
        }
    }

    /// Frame tap: an outgoing frame.
    pub(crate) fn tap_tx(&mut self, id: &str, req: &ApsRequest, now: Millis) {
        if self.frame_tap {
            let e = frame_event(
                id,
                false,
                req.dst_ep,
                req.profile,
                req.cluster,
                &req.payload,
                None,
                now,
            );
            self.emit(HubEvent::Frame(e));
        }
    }

    /// The definition type of `(ep, cluster, attr)`, for writes without a type.
    fn bound_type(&self, r: &ConsoleReq, attr: u16) -> Option<ZclType> {
        let d = self.devices.get(&r.device)?;
        d.entities
            .iter()
            .filter(|e| e.r.ep == r.ep)
            .flat_map(|e| e.r.def.values().map(|(_, v)| v.binding.clone()))
            .find_map(|b| match b {
                Binding::Zcl(z) if z.cluster == r.cluster && z.attr == attr && z.mfr == r.mfr => {
                    Some(z.ty)
                }
                _ => None,
            })
    }

    /// Sends a console request; the answer follows as
    /// [`HubEvent::ConsoleResult`] with the returned ticket.
    pub fn console(&mut self, r: ConsoleReq, now: Millis) -> Result<CommandAck, ApiError> {
        let d = self
            .devices
            .get(&r.device)
            .ok_or_else(|| ApiError::new("not_found", r.device.clone()))?;
        if !matches!(d.kind, DeviceKind::Zigbee { .. }) {
            return Err(ApiError::new(
                "unsupported",
                "Green Power devices take no requests",
            ));
        }
        let queued = d.sleepy() && now > d.awake_until;
        let ty_of = |ty: Option<ZclType>, attr: u16| {
            ty.or_else(|| self.bound_type(&r, attr)).ok_or_else(|| {
                bad(format!(
                    "type of attribute 0x{attr:04x} unknown: give `data_type` (a read shows it)"
                ))
            })
        };
        let (frame_type, cmd, payload, to_client, ddr, cluster_answer) = match &r.op {
            ConsoleOp::Read(a) => (
                FrameType::Global,
                global::READ_ATTRIBUTES,
                zf::read_attributes(a),
                false,
                false,
                false,
            ),
            ConsoleOp::Write { attr, ty, value } => {
                let ty = ty_of(*ty, *attr)?;
                let v = match (ty, value) {
                    (ZclType::OCTET_STR, J::String(s)) => crate::expr::Value::Bytes(unhex(s)?),
                    _ => crate::expr::Value::from_json(value),
                };
                let zv = values::to_zcl(ty, &v).map_err(bad)?;
                let p = zf::write_attributes(&[(*attr, TypedValue::new(ty, zv))])
                    .map_err(|e| bad(e.to_string()))?;
                (
                    FrameType::Global,
                    global::WRITE_ATTRIBUTES,
                    p,
                    false,
                    false,
                    false,
                )
            }
            ConsoleOp::ConfigureReporting {
                attr,
                ty,
                min,
                max,
                change,
            } => {
                let ty = ty_of(*ty, *attr)?;
                let rec = ReportingConfig {
                    attr: *attr,
                    ty,
                    min: *min,
                    max: *max,
                    change: *change,
                };
                let p = zf::configure_reporting(&[rec]).map_err(|e| bad(e.to_string()))?;
                (
                    FrameType::Global,
                    global::CONFIGURE_REPORTING,
                    p,
                    false,
                    false,
                    false,
                )
            }
            ConsoleOp::ReadReporting(a) => {
                let p = a.iter().flat_map(|x| {
                    let b = x.to_le_bytes();
                    [0, b[0], b[1]]
                });
                (
                    FrameType::Global,
                    global::READ_REPORTING_CONFIG,
                    p.collect(),
                    false,
                    false,
                    false,
                )
            }
            ConsoleOp::Discover { what, start, max } => {
                let (c, mut p) = match what {
                    0 => (disc::ATTRIBUTES, start.to_le_bytes().to_vec()),
                    1 | 2 => {
                        let c = if *what == 1 {
                            disc::COMMANDS_RECEIVED
                        } else {
                            disc::COMMANDS_GENERATED
                        };
                        (
                            c,
                            vec![u8::try_from(*start).map_err(|_| bad("start > 255"))?],
                        )
                    }
                    _ => {
                        return Err(bad(
                            "what: attributes, commands_received or commands_generated",
                        ));
                    }
                };
                p.push(*max);
                (FrameType::Global, c, p, false, false, false)
            }
            ConsoleOp::Command {
                cmd,
                payload,
                global,
                to_client,
                no_default_rsp,
            } => {
                let ft = if *global {
                    FrameType::Global
                } else {
                    FrameType::Cluster
                };
                (
                    ft,
                    *cmd,
                    payload.clone(),
                    *to_client,
                    *no_default_rsp,
                    !*global,
                )
            }
        };
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        let tsn = self.next_zcl_tsn();
        let f = ZclFrame {
            header: ZclHeader {
                frame_type,
                mfr: r.mfr,
                direction: if to_client {
                    Direction::ServerToClient
                } else {
                    Direction::ClientToServer
                },
                disable_default_response: ddr,
                tsn,
                cmd,
            },
            payload,
        };
        let purpose = Purpose::Console {
            ticket,
            cluster_answer,
        };
        self.send_zcl(&r.device, r.ep, r.cluster, f, purpose, now);
        Ok(CommandAck {
            ticket,
            status: if queued { "queued" } else { "pending" },
        })
    }

    /// A frame from `id`: completes the console request it answers.
    #[inline(never)]
    pub(crate) fn console_answer(&mut self, id: &str, f: &ZclFrame) {
        let key = (false, f.header.tsn);
        let Some(p) = self.pending.get(&key) else {
            return;
        };
        let Purpose::Console {
            ticket,
            cluster_answer,
        } = p.purpose
        else {
            return;
        };
        let answers = match f.header.frame_type {
            FrameType::Global => is_answer(f.header.cmd),
            FrameType::Cluster => cluster_answer,
        };
        if p.device != id || !answers {
            return;
        }
        self.pending.remove(&key);
        self.emit(HubEvent::ConsoleResult {
            ticket,
            result: Ok(decode_answer(f)),
        });
    }

    /// `tuya/send`: datapoints to a Tuya device (EF00 data request); the
    /// result follows the device's dataResponse/report of every DP, or a
    /// timeout after 15 s (10 min for a sleeping device).
    pub fn tuya_send_dps(
        &mut self,
        device: &str,
        dps: &[(u8, TuyaType, J)],
        now: Millis,
    ) -> Result<CommandAck, ApiError> {
        if !self.devices.contains_key(device) {
            return Err(ApiError::new("not_found", device.to_string()));
        }
        let mut recs = Vec::new();
        for (dp, ty, v) in dps {
            let v = match (ty, v) {
                (TuyaType::Raw, J::String(s)) => crate::expr::Value::Bytes(unhex(s)?),
                _ => crate::expr::Value::from_json(v),
            };
            let data = tuya::dp_encode(*ty, &v).map_err(bad)?;
            recs.push(DpRecord {
                dp: *dp,
                ty: *ty as u8,
                data,
            });
        }
        let queued = self.devices[device].sleepy() && now > self.devices[device].awake_until;
        let ticket = self.new_ticket();
        self.tuya_send(device, &recs, now);
        let ids: Vec<u8> = recs.iter().map(|r| r.dp).collect();
        // A sleepy device gets the frame when it is next heard from.
        let wait = if queued { 600_000 } else { 15_000 };
        self.ticket_wait_tuya(ticket, device, &ids, now + wait);
        Ok(CommandAck {
            ticket,
            status: if queued { "queued" } else { "pending" },
        })
    }
}

/// `data_type`: a ZCL type code, as a number or `"0x29"` (the web UI maps
/// definition type names).
fn zcl_type(v: &J) -> Result<Option<ZclType>, ApiError> {
    let t = match v {
        J::Null => return Ok(None),
        J::String(s) => s
            .strip_prefix("0x")
            .and_then(|h| u8::from_str_radix(h, 16).ok()),
        v => v.as_u64().and_then(|n| u8::try_from(n).ok()),
    };
    t.map(|t| Some(ZclType(t)))
        .ok_or_else(|| bad("data_type: a ZCL type code (0x00-0xFF)"))
}

/// An optional integer argument (absent: `default`).
fn int<N: TryFrom<u64>>(a: &J, k: &str, default: N) -> Result<N, ApiError> {
    match &a[k] {
        J::Null => Ok(default),
        v => v
            .as_u64()
            .and_then(|x| N::try_from(x).ok())
            .ok_or_else(|| bad(format!("{k}: integer out of range"))),
    }
}

fn ints(a: &J, k: &str) -> Result<Vec<u16>, ApiError> {
    let v: Option<Vec<u16>> = a[k]
        .as_array()
        .map(|l| {
            l.iter()
                .map(|x| x.as_u64().and_then(|x| u16::try_from(x).ok()))
                .collect()
        })
        .unwrap_or_default();
    match v {
        Some(v) if !v.is_empty() => Ok(v),
        _ => Err(bad(format!("{k}: a list of attribute IDs"))),
    }
}

/// The console request of an API request (`None`: not a `zcl/*` request).
pub fn request(r: &crate::api::messages::Request) -> Option<Result<ConsoleReq, ApiError>> {
    use crate::api::messages::Request as R;
    let a = match r {
        R::ZclRead(a)
        | R::ZclReadReporting(a)
        | R::ZclWrite(a)
        | R::ZclConfigureReporting(a)
        | R::ZclDiscover(a)
        | R::ZclCommand(a) => a,
        _ => return None,
    };
    let op = || -> Result<ConsoleOp, ApiError> {
        let flag = |k: &str| a[k].as_bool().unwrap_or(false);
        Ok(match r {
            R::ZclRead(_) => ConsoleOp::Read(ints(a, "attributes")?),
            R::ZclReadReporting(_) => ConsoleOp::ReadReporting(ints(a, "attributes")?),
            R::ZclWrite(_) => ConsoleOp::Write {
                attr: int(a, "attribute", 0)?,
                ty: zcl_type(&a["data_type"])?,
                value: a["value"].clone(),
            },
            R::ZclConfigureReporting(_) => ConsoleOp::ConfigureReporting {
                attr: int(a, "attribute", 0)?,
                ty: zcl_type(&a["data_type"])?,
                min: int(a, "min", 0)?,
                max: int(a, "max", 0)?,
                change: a["change"].as_f64(),
            },
            R::ZclDiscover(_) => {
                let what = match a["what"].as_str().unwrap_or("attributes") {
                    "attributes" => 0,
                    "commands_received" => 1,
                    "commands_generated" => 2,
                    w => return Err(bad(format!("unknown what {w:?}"))),
                };
                ConsoleOp::Discover {
                    what,
                    start: int(a, "start", 0)?,
                    max: int(a, "max_results", if what == 0 { 32 } else { 64 })?,
                }
            }
            _ => ConsoleOp::Command {
                cmd: int(a, "command", 0)?,
                payload: unhex(a["payload"].as_str().unwrap_or(""))?,
                global: flag("global"),
                to_client: flag("to_client"),
                no_default_rsp: flag("disable_default_response"),
            },
        })
    };
    Some(op().and_then(|op| {
        Ok(ConsoleReq {
            device: a["device"].as_str().unwrap_or_default().to_string(),
            ep: int(a, "endpoint", 1)?,
            cluster: int(a, "cluster", 0)?,
            mfr: match &a["manufacturer"] {
                J::Null => None,
                _ => Some(int(a, "manufacturer", 0)?),
            },
            op,
        })
    }))
}

/// `tuya/send` datapoints: `{dp, type, value}`.
pub fn parse_dps(a: &J) -> Result<Vec<(u8, TuyaType, J)>, ApiError> {
    let dps = a["dps"].as_array().map(Vec::as_slice).unwrap_or_default();
    if dps.is_empty() {
        return Err(bad("dps: at least one"));
    }
    dps.iter()
        .map(|d| {
            let dp = d["dp"]
                .as_u64()
                .and_then(|x| u8::try_from(x).ok())
                .ok_or_else(|| bad("dp: 1-255"))?;
            let ty = match &d["type"] {
                J::Number(n) => n.as_u64().and_then(|c| TuyaType::from_code(c as u8)),
                J::String(s) => ["raw", "bool", "value", "string", "enum", "bitmap"]
                    .iter()
                    .position(|n| n == s)
                    .and_then(|c| TuyaType::from_code(c as u8)),
                _ => None,
            }
            .ok_or_else(|| bad("type: raw, bool, value, string, enum or bitmap"))?;
            Ok((dp, ty, d["value"].clone()))
        })
        .collect()
}
