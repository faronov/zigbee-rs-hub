//! A device simulator answering the engine's ZDO/ZCL/EF00 traffic, for
//! interview-to-entity golden scenarios without hardware.

use std::collections::{BTreeMap, BTreeSet};

use crate::device::Millis;
use crate::hub::Hub;
use crate::transport::{ApsDst, ApsIndication, ApsRequest, MockTransport, Sent};
use crate::tuya;
use crate::zcl::frame::{self as zf, ReadRecord, Status, global};
use crate::zcl::{Direction, FrameType, TypedValue, ZclFrame, ZclHeader, ZclType, ZclValue};
use crate::zdo::{self, LogicalType, NodeDescriptor, SimpleDescriptor};

/// A simulated Zigbee device.
#[derive(Debug, Clone)]
pub struct SimDevice {
    /// IEEE.
    pub ieee: u64,
    /// NWK.
    pub nwk: u16,
    /// Node descriptor.
    pub node: NodeDescriptor,
    /// Simple descriptors.
    pub endpoints: Vec<SimpleDescriptor>,
    /// Attributes `(ep, cluster, attr, mfr)`.
    pub attrs: BTreeMap<(u8, u16, u16, Option<u16>), TypedValue>,
    /// Attributes that refuse Configure Reporting.
    pub unreportable: BTreeSet<(u16, u16)>,
    /// Received cluster commands `(ep, cluster, cmd, mfr, payload)`.
    pub commands: Vec<(u8, u16, u8, Option<u16>, Vec<u8>)>,
    /// Bind requests `(ep, cluster)`.
    pub binds: Vec<(u8, u16)>,
    /// Configure Reporting records `(ep, cluster, config)`.
    pub reporting: Vec<(u8, u16, zf::ReportingConfig)>,
    /// Writes `(ep, cluster, attr, mfr, value)`.
    pub writes: Vec<(u8, u16, u16, Option<u16>, TypedValue)>,
    /// Tuya DP state.
    pub tuya: BTreeMap<u8, (u8, Vec<u8>)>,
    /// Tuya frames received `(cmd, payload)`.
    pub tuya_rx: Vec<(u8, Vec<u8>)>,
    /// Answers at all.
    pub responsive: bool,
    /// Answer Default Responses for cluster commands with this status.
    pub cmd_status: Status,
}

impl SimDevice {
    /// A new device.
    pub fn new(ieee: u64, nwk: u16, logical_type: LogicalType, rx_on: bool, mfr_code: u16) -> Self {
        let mut mac = 0x80;
        if rx_on {
            mac |= 0x0C;
        }
        if logical_type == LogicalType::Router {
            mac |= 0x02;
        }
        Self {
            ieee,
            nwk,
            node: NodeDescriptor {
                logical_type,
                mac_capabilities: mac,
                manufacturer_code: mfr_code,
                max_buffer_size: 82,
                max_incoming_transfer: 82,
                server_mask: 0x2C00,
                max_outgoing_transfer: 82,
            },
            endpoints: Vec::new(),
            attrs: BTreeMap::new(),
            unreportable: BTreeSet::new(),
            commands: Vec::new(),
            binds: Vec::new(),
            reporting: Vec::new(),
            writes: Vec::new(),
            tuya: BTreeMap::new(),
            tuya_rx: Vec::new(),
            responsive: true,
            cmd_status: Status::SUCCESS,
        }
    }

    /// Adds an endpoint.
    pub fn endpoint(
        mut self,
        ep: u8,
        profile: u16,
        device_id: u16,
        inc: &[u16],
        out: &[u16],
    ) -> Self {
        self.endpoints.push(SimpleDescriptor {
            endpoint: ep,
            profile,
            device_id,
            version: 1,
            in_clusters: inc.to_vec(),
            out_clusters: out.to_vec(),
        });
        self
    }

    /// Sets an attribute.
    pub fn attr(mut self, ep: u8, cluster: u16, attr: u16, v: TypedValue) -> Self {
        self.attrs.insert((ep, cluster, attr, None), v);
        self
    }

    /// Sets Basic strings and power source.
    pub fn basic(self, manufacturer: &str, model: &str, power: u8) -> Self {
        let ep = self.endpoints.first().map_or(1, |e| e.endpoint);
        self.attr(
            ep,
            0,
            0x0004,
            TypedValue::new(ZclType::CHAR_STR, ZclValue::String(manufacturer.into())),
        )
        .attr(
            ep,
            0,
            0x0005,
            TypedValue::new(ZclType::CHAR_STR, ZclValue::String(model.into())),
        )
        .attr(
            ep,
            0,
            0x0007,
            TypedValue::uint(ZclType::ENUM8, u64::from(power)),
        )
        .attr(ep, 0, 0x0000, TypedValue::uint(ZclType::U8, 8))
        .attr(ep, 0, 0x0001, TypedValue::uint(ZclType::U8, 1))
    }

    /// An indication from this device.
    pub fn ind(&self, src_ep: u8, profile: u16, cluster: u16, payload: Vec<u8>) -> ApsIndication {
        ApsIndication {
            src_nwk: self.nwk,
            src_ieee: Some(self.ieee),
            src_ep,
            dst_ep: 1,
            group: None,
            profile,
            cluster,
            payload,
            lqi: 200,
            rssi: -55,
        }
    }

    /// A ZCL frame from this device (server → client).
    pub fn zcl(
        &self,
        ep: u8,
        cluster: u16,
        frame_type: FrameType,
        mfr: Option<u16>,
        tsn: u8,
        cmd: u8,
        payload: Vec<u8>,
    ) -> ApsIndication {
        let f = ZclFrame {
            header: ZclHeader {
                frame_type,
                mfr,
                direction: Direction::ServerToClient,
                disable_default_response: true,
                tsn,
                cmd,
            },
            payload,
        };
        self.ind(ep, 0x0104, cluster, f.encode())
    }

    /// A Report Attributes frame.
    pub fn report(
        &self,
        ep: u8,
        cluster: u16,
        mfr: Option<u16>,
        recs: &[(u16, TypedValue)],
    ) -> ApsIndication {
        self.zcl(
            ep,
            cluster,
            FrameType::Global,
            mfr,
            0x70,
            global::REPORT_ATTRIBUTES,
            zf::report_attributes(recs).unwrap(),
        )
    }

    /// A Tuya data frame (dataReport by default).
    pub fn tuya_frame(&self, cmd: u8, seq: u16, dps: &[(u8, u8, Vec<u8>)]) -> ApsIndication {
        let recs: Vec<tuya::DpRecord> = dps
            .iter()
            .map(|(dp, ty, data)| tuya::DpRecord {
                dp: *dp,
                ty: *ty,
                data: data.clone(),
            })
            .collect();
        self.zcl(
            1,
            0xEF00,
            FrameType::Cluster,
            None,
            0x10,
            cmd,
            tuya::encode_data(seq, &recs),
        )
    }

    fn answer(&mut self, req: &ApsRequest) -> Vec<ApsIndication> {
        if !self.responsive {
            return Vec::new();
        }
        let mut out = Vec::new();
        if req.profile == 0 {
            let tsn = req.payload[0];
            let p = match req.cluster {
                zdo::cluster::NODE_DESC_REQ => Some((
                    zdo::cluster::NODE_DESC_RSP,
                    zdo::node_desc_rsp(tsn, self.nwk, &self.node),
                )),
                zdo::cluster::ACTIVE_EP_REQ => {
                    let eps: Vec<u8> = self.endpoints.iter().map(|e| e.endpoint).collect();
                    Some((
                        zdo::cluster::ACTIVE_EP_RSP,
                        zdo::active_ep_rsp(tsn, self.nwk, &eps),
                    ))
                }
                zdo::cluster::SIMPLE_DESC_REQ => {
                    let ep = req.payload[3];
                    self.endpoints.iter().find(|e| e.endpoint == ep).map(|d| {
                        (
                            zdo::cluster::SIMPLE_DESC_RSP,
                            zdo::simple_desc_rsp(tsn, self.nwk, d),
                        )
                    })
                }
                zdo::cluster::BIND_REQ => {
                    if let Ok((_, _, ep, c, _)) = zdo::parse_bind_req(&req.payload) {
                        self.binds.push((ep, c));
                    }
                    Some((zdo::cluster::BIND_RSP, vec![tsn, 0]))
                }
                zdo::cluster::MGMT_LEAVE_REQ => Some((zdo::cluster::MGMT_LEAVE_RSP, vec![tsn, 0])),
                _ => None,
            };
            if let Some((c, p)) = p {
                out.push(self.ind(0, 0, c, p));
            }
            return out;
        }
        let Ok(f) = ZclFrame::parse(&req.payload) else {
            return out;
        };
        let ep = req.dst_ep;
        let h = f.header;
        let rsp = |ft: FrameType, cmd: u8, payload: Vec<u8>| ZclFrame {
            header: ZclHeader {
                frame_type: ft,
                mfr: h.mfr,
                direction: Direction::ServerToClient,
                disable_default_response: true,
                tsn: h.tsn,
                cmd,
            },
            payload,
        };
        let reply = match (h.frame_type, h.cmd) {
            (FrameType::Global, global::READ_ATTRIBUTES) => {
                let attrs = zf::parse_read_attributes(&f.payload).unwrap_or_default();
                let recs: Vec<ReadRecord> = attrs
                    .iter()
                    .map(|a| match self.attrs.get(&(ep, req.cluster, *a, h.mfr)) {
                        Some(v) => ReadRecord {
                            attr: *a,
                            status: Status::SUCCESS,
                            value: Some(v.clone()),
                        },
                        None => ReadRecord {
                            attr: *a,
                            status: Status::UNSUPPORTED_ATTRIBUTE,
                            value: None,
                        },
                    })
                    .collect();
                Some(rsp(
                    FrameType::Global,
                    global::READ_ATTRIBUTES_RSP,
                    zf::read_attributes_rsp(&recs).unwrap(),
                ))
            }
            (FrameType::Global, global::WRITE_ATTRIBUTES) => {
                for (a, v) in zf::parse_write_attributes(&f.payload).unwrap_or_default() {
                    self.writes.push((ep, req.cluster, a, h.mfr, v.clone()));
                    self.attrs.insert((ep, req.cluster, a, h.mfr), v);
                }
                Some(rsp(
                    FrameType::Global,
                    global::WRITE_ATTRIBUTES_RSP,
                    vec![0],
                ))
            }
            (FrameType::Global, global::CONFIGURE_REPORTING) => {
                let mut fails = Vec::new();
                for c in zf::parse_configure_reporting(&f.payload).unwrap_or_default() {
                    if self.unreportable.contains(&(req.cluster, c.attr)) {
                        fails.push((Status::UNREPORTABLE_ATTRIBUTE, c.attr));
                    } else {
                        self.reporting.push((ep, req.cluster, c));
                    }
                }
                Some(rsp(
                    FrameType::Global,
                    global::CONFIGURE_REPORTING_RSP,
                    zf::configure_reporting_rsp(&fails),
                ))
            }
            // Discover Attributes: the simulated attributes of the cluster.
            (FrameType::Global, 0x0C) => {
                let start = u16::from_le_bytes([f.payload[0], f.payload[1]]);
                let mut list: Vec<(u16, u8)> = self
                    .attrs
                    .iter()
                    .filter(|((e, c, a, m), _)| {
                        *e == ep && *c == req.cluster && *a >= start && *m == h.mfr
                    })
                    .map(|((_, _, a, _), v)| (*a, v.ty.0))
                    .collect();
                list.sort_unstable();
                let mut p = vec![1];
                for (a, t) in list {
                    p.extend_from_slice(&a.to_le_bytes());
                    p.push(t);
                }
                Some(rsp(FrameType::Global, 0x0D, p))
            }
            (FrameType::Global, _) => None,
            (FrameType::Cluster, cmd) => {
                self.commands
                    .push((ep, req.cluster, cmd, h.mfr, f.payload.clone()));
                if req.cluster == 0xEF00 {
                    self.tuya_rx.push((cmd, f.payload.clone()));
                    if matches!(cmd, tuya::cmd::DATA_REQUEST | tuya::cmd::SEND_DATA) {
                        if let Ok((seq, recs)) = tuya::parse_data(&f.payload) {
                            for r in &recs {
                                self.tuya.insert(r.dp, (r.ty, r.data.clone()));
                            }
                            let mut r = rsp(
                                FrameType::Cluster,
                                tuya::cmd::DATA_RESPONSE,
                                tuya::encode_data(seq, &recs),
                            );
                            r.header.tsn = h.tsn.wrapping_add(100);
                            out.push(self.ind(ep, req.profile, req.cluster, r.encode()));
                        }
                    }
                    None
                } else {
                    if req.cluster == 0x0006 && cmd <= 0x02 {
                        let cur = matches!(
                            self.attrs.get(&(ep, 6, 0, None)).map(|v| &v.value),
                            Some(ZclValue::Bool(Some(true)))
                        );
                        let new = match cmd {
                            0 => false,
                            1 => true,
                            _ => !cur,
                        };
                        self.attrs.insert(
                            (ep, 6, 0, None),
                            TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(new))),
                        );
                    }
                    (!h.disable_default_response).then(|| {
                        rsp(
                            FrameType::Global,
                            global::DEFAULT_RSP,
                            zf::default_rsp(cmd, self.cmd_status),
                        )
                    })
                }
            }
        };
        if let Some(r) = reply {
            out.push(self.ind(ep, req.profile, req.cluster, r.encode()));
        }
        out
    }
}

/// The simulated network.
#[derive(Debug, Default)]
pub struct Sim {
    /// Devices.
    pub devices: Vec<SimDevice>,
    /// Everything the hub sent, in order.
    pub log: Vec<Sent>,
}

impl Sim {
    /// The device by IEEE.
    pub fn dev(&mut self, ieee: u64) -> &mut SimDevice {
        self.devices
            .iter_mut()
            .find(|d| d.ieee == ieee)
            .expect("sim device")
    }

    /// Delivers the hub's traffic until the network is quiet.
    pub fn run(&mut self, hub: &mut Hub<MockTransport>, now: Millis) {
        for _ in 0..200 {
            let sent = hub.transport.take();
            if sent.is_empty() {
                return;
            }
            let mut inds = Vec::new();
            for s in sent {
                if let Sent::Aps(req) = &s {
                    if let ApsDst::Nwk(n) = req.dst {
                        if let Some(d) = self.devices.iter_mut().find(|d| d.nwk == n) {
                            inds.extend(d.answer(req));
                        }
                    }
                }
                self.log.push(s);
            }
            for i in inds {
                hub.on_aps_indication(i, now);
            }
        }
        panic!("simulation did not settle");
    }

    /// Joins a device and runs the interview to the end.
    pub fn join(&mut self, hub: &mut Hub<MockTransport>, ieee: u64, now: Millis) {
        let nwk = self.dev(ieee).nwk;
        hub.on_device_joined(ieee, nwk, now);
        self.run(hub, now);
    }

    /// APS requests sent so far.
    pub fn aps(&self) -> Vec<&ApsRequest> {
        self.log
            .iter()
            .filter_map(|s| match s {
                Sent::Aps(a) => Some(a),
                _ => None,
            })
            .collect()
    }
}
