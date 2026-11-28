//! ZRHP wire messages ⇄ the backend-neutral types of `hub-coord`
//! ([`CoordEvent`], [`ApsRequest`], [`hub_coord::gp`]; field names are 1:1
//! with `zigbee_zrhp::msg`). Moved from `hub-app` (`convert.rs`).

use hub_coord::gp as eg;
use hub_coord::{ApsDst as EApsDst, ApsIndication, ApsRequest, CoordEvent, JoinKind, NetChange};
use zigbee_zrhp::codec::List;
use zigbee_zrhp::msg::data::{ApsDataRequest, ApsDst, EvtApsDataIndication, indication_flags, tx_options};
use zigbee_zrhp::msg::gp as wg;
use zigbee_zrhp::msg::ids;
use zigbee_zrhp::msg::network::{EvtDeviceJoined, EvtDeviceLeft, EvtNetworkState, EvtPermitJoinState};
use zigbee_zrhp::records::Record;
use zigbee_zrhp::types::Ieee;

use crate::driver::Message;

/// `EVT_DEVICE_LEFT.reason`: a child of the NCP aged out (an end device
/// missed its End Device Timeout, a router child its Link Status); it need
/// not have left the network.
pub(crate) const LEFT_CHILD_TIMEOUT: u8 = 4;

fn gpd(id: &wg::GpdId) -> eg::GpdId {
    match *id {
        wg::GpdId::SrcId(s) => eg::GpdId::SrcId(s),
        wg::GpdId::Ieee { ieee, endpoint } => eg::GpdId::Ieee { ieee: ieee.to_u64(), endpoint },
    }
}

fn wire_gpd(id: &eg::GpdId) -> wg::GpdId {
    match *id {
        eg::GpdId::SrcId(s) => wg::GpdId::SrcId(s),
        eg::GpdId::Ieee { ieee, endpoint } => wg::GpdId::Ieee { ieee: Ieee::from_u64(ieee), endpoint },
    }
}

fn sink(r: &wg::GpSinkRecord) -> eg::GpSinkRecord {
    eg::GpSinkRecord {
        gpd_id: gpd(&r.gpd_id),
        device_id: r.device_id,
        communication_mode: r.communication_mode,
        group: r.group,
        assigned_alias: r.assigned_alias,
        flags: r.flags,
        security_level: r.security_level,
        key_type: r.key_type,
        key: r.key,
        frame_counter: r.frame_counter,
        groupcast_radius: r.groupcast_radius,
    }
}

fn translation(t: &wg::GpTranslation<'_>) -> eg::GpTranslation {
    eg::GpTranslation {
        gpd_command: t.gpd_command,
        endpoint: t.endpoint,
        profile: t.profile,
        cluster: t.cluster,
        zcl_command: t.zcl_command,
        payload: match t.payload_len {
            wg::gp_translation_payload::DERIVED => eg::TranslationPayload::Derived,
            wg::gp_translation_payload::COPY_GPD => eg::TranslationPayload::Copy,
            _ => eg::TranslationPayload::Fixed(t.payload.to_vec()),
        },
    }
}

/// Converts an NCP event; `None` for events the hub does not take.
pub fn coord_event(m: &Message) -> Option<CoordEvent> {
    Some(match m.msg_id {
        ids::EVT_APS_DATA_INDICATION => {
            let e: EvtApsDataIndication = m.decode().ok()?;
            CoordEvent::Indication(ApsIndication {
                src_nwk: e.src_nwk,
                src_ieee: (e.flags & indication_flags::SRC_IEEE_VALID != 0).then(|| e.src_ieee.to_u64()),
                src_ep: e.src_ep,
                dst_ep: e.dst_ep,
                group: (e.dst_mode == 0x01).then_some(e.dst),
                profile: e.profile,
                cluster: e.cluster,
                payload: e.asdu.0.to_vec(),
                lqi: e.lqi,
                rssi: e.rssi,
            })
        }
        ids::EVT_PERMIT_JOIN_STATE => {
            let e: EvtPermitJoinState = m.decode().ok()?;
            CoordEvent::PermitJoin { remaining_s: e.remaining_s.into() }
        }
        ids::EVT_NETWORK_STATE => {
            let e: EvtNetworkState = m.decode().ok()?;
            CoordEvent::NetworkState(match e.state {
                0 => NetChange::Down,
                1 => NetChange::Forming,
                2 => NetChange::Up,
                _ => NetChange::Leaving,
            })
        }
        ids::EVT_DEVICE_JOINED => {
            let e: EvtDeviceJoined = m.decode().ok()?;
            let kind = match e.join_type {
                1 => JoinKind::SecuredRejoin,
                2 => JoinKind::TcRejoin,
                3 => JoinKind::Direct,
                _ => JoinKind::Association,
            };
            let (ieee, nwk) = (e.ieee.to_u64(), e.nwk);
            if e.decision != 0 {
                return Some(CoordEvent::JoinDenied { ieee, nwk, kind });
            }
            CoordEvent::DeviceJoined { ieee, nwk, kind }
        }
        // Flags bit 0: it leaves to rejoin; reason 4: a child of the NCP
        // aged out (an end device missed its timeout, a router child its
        // Link Status), which need not be a leave.
        ids::EVT_DEVICE_LEFT => {
            let e: EvtDeviceLeft = m.decode().ok()?;
            CoordEvent::DeviceLeft {
                ieee: e.ieee.to_u64(),
                nwk: e.nwk,
                rejoin: Some(e.flags & 0x01 != 0),
                aged_out: e.reason == LEFT_CHILD_TIMEOUT,
            }
        }
        ids::EVT_GP_FRAME => {
            let e: wg::EvtGpFrame = m.decode().ok()?;
            CoordEvent::Gp(Box::new(eg::GpEvent::Frame(eg::GpFrame {
                origin: e.origin,
                result: e.result,
                gpd_id: gpd(&e.gpd_id),
                device_id: e.device_id,
                command_id: e.command_id,
                frame_counter: e.frame_counter,
                security_level: e.security_level,
                key_type: e.key_type,
                flags: e.flags,
                proxy_nwk: e.proxy_nwk,
                gpp_gpd_link: e.gpp_gpd_link,
                rssi: e.rssi,
                lqi: e.lqi,
                payload: e.payload.0.to_vec(),
            })))
        }
        ids::EVT_GP_COMMISSIONING => {
            let e: wg::EvtGpCommissioning = m.decode().ok()?;
            CoordEvent::Gp(Box::new(eg::GpEvent::Commissioning { state: e.state, window_s: e.window_s, exit_reason: e.exit_reason }))
        }
        ids::EVT_GP_COMMISSIONED => {
            let e: wg::EvtGpCommissioned = m.decode().ok()?;
            let Record::GpSink(r) = &e.record else { return None };
            CoordEvent::Gp(Box::new(eg::GpEvent::Commissioned {
                gpd_id: gpd(&e.gpd_id),
                device_id: e.device_id,
                flags: e.flags,
                record: sink(r),
            }))
        }
        ids::EVT_GP_COMMISSIONING_FAILED => {
            let e: wg::EvtGpCommissioningFailed = m.decode().ok()?;
            CoordEvent::Gp(Box::new(eg::GpEvent::CommissioningFailed { gpd_id: gpd(&e.gpd_id), reason: e.reason }))
        }
        ids::EVT_GP_DECOMMISSIONED => {
            let e: wg::EvtGpDecommissioned = m.decode().ok()?;
            CoordEvent::Gp(Box::new(eg::GpEvent::Decommissioned { gpd_id: gpd(&e.gpd_id) }))
        }
        ids::EVT_GP_REMOTE_STATUS => {
            let e: wg::EvtGpRemoteStatus = m.decode().ok()?;
            CoordEvent::Gp(Box::new(eg::GpEvent::RemoteStatus {
                src_nwk: e.src_nwk,
                zcl_tsn: e.zcl_tsn,
                command_id: e.command_id,
                zcl_status: e.zcl_status,
            }))
        }
        ids::EVT_GP_REMOTE_SINK_TABLE => {
            let e: wg::EvtGpRemoteSinkTable = m.decode().ok()?;
            let records = e
                .records
                .iter()
                .filter_map(|r| match r {
                    Record::GpSink(s) => Some(sink(&s)),
                    _ => None,
                })
                .collect();
            CoordEvent::Gp(Box::new(eg::GpEvent::RemoteSinkTable { src_nwk: e.src_nwk, zcl_tsn: e.zcl_tsn, status: e.status, records }))
        }
        super::EVT_SCAN_PROGRESS => return super::scan_progress(m),
        ids::EVT_GP_REMOTE_TRANSLATION_TABLE => {
            let e: wg::EvtGpRemoteTranslationTable = m.decode().ok()?;
            let records = e
                .records
                .iter()
                .filter_map(|r| match r {
                    Record::GpTranslation(t) => Some((gpd(&t.gpd_id), translation(&t.translation))),
                    _ => None,
                })
                .collect();
            CoordEvent::Gp(Box::new(eg::GpEvent::RemoteTranslationTable { src_nwk: e.src_nwk, zcl_tsn: e.zcl_tsn, status: e.status, records }))
        }
        _ => return None,
    })
}

/// `APS_DATA_REQUEST` payload of an engine request.
pub fn aps_request(req: &ApsRequest) -> Result<Vec<u8>, crate::Error> {
    let mut opts = 0;
    if req.options.ack {
        opts |= tx_options::APS_ACK;
    }
    if req.options.extended_timeout {
        opts |= tx_options::EXTENDED_TIMEOUT;
    }
    let body = ApsDataRequest {
        dst: match req.dst {
            EApsDst::Nwk(n) => ApsDst::Nwk(n),
            EApsDst::Ieee(i) => ApsDst::Ieee(Ieee::from_u64(i)),
            EApsDst::Group(g) => ApsDst::Group(g),
        },
        dst_ep: req.dst_ep,
        src_ep: req.src_ep,
        profile: req.profile,
        cluster: req.cluster,
        tx_options: opts,
        radius: 0,
        non_member_radius: 0,
        priority: 1,
        relays: None,
        alias: None,
        asdu: &req.payload,
    };
    crate::driver::encode_body(&body)
}

/// ZRHP message ID and payload of an engine GP request.
pub fn gp_request(req: &eg::GpRequest) -> Result<(u16, Vec<u8>), crate::Error> {
    use crate::driver::encode_body as enc;
    let payload = match req {
        eg::GpRequest::SinkConfig { mode, communication_mode, group, exit_mode, window_s, security_level, groupcast_radius } => {
            enc(&wg::GpSinkConfig {
                mode: *mode,
                communication_mode: *communication_mode,
                group: *group,
                exit_mode: *exit_mode,
                window_s: *window_s,
                security_level: *security_level,
                groupcast_radius: *groupcast_radius,
            })?
        }
        eg::GpRequest::SinkCommissioning { action, flags, window_s } => {
            enc(&wg::GpSinkCommissioning { action: *action, flags: *flags, window_s: *window_s })?
        }
        eg::GpRequest::SinkRemove { gpd_id, flags } => enc(&wg::GpSinkRemove { gpd_id: wire_gpd(gpd_id), flags: *flags })?,
        eg::GpRequest::PairingConfiguration(p) => {
            let groups: Vec<wg::GpGroup> = p.groups.iter().map(|&(group, alias)| wg::GpGroup { group, alias }).collect();
            enc(&wg::GpPairingConfiguration {
                dst: p.dst,
                actions: p.actions,
                gpd_id: wire_gpd(&p.gpd_id),
                device_id: p.device_id,
                communication_mode: p.communication_mode,
                flags: p.flags,
                assigned_alias: p.assigned_alias,
                groupcast_radius: p.groupcast_radius,
                security_level: p.security_level,
                key_type: p.key_type,
                key: p.key,
                frame_counter: p.frame_counter,
                groups: List::new(&groups),
                paired: p.paired,
                endpoints: List::new(&p.endpoints),
                app_info: p.app_info,
                manufacturer_id: p.manufacturer_id,
                model_id: p.model_id,
                gpd_commands: List::new(&p.gpd_commands),
                server_clusters: List::new(&p.server_clusters),
                client_clusters: List::new(&p.client_clusters),
            })?
        }
        eg::GpRequest::TranslationUpdate { dst, action, gpd_id, translations } => {
            let wire: Vec<wg::GpTranslation<'_>> = translations
                .iter()
                .map(|t| {
                    let (payload_len, payload): (u8, &[u8]) = match &t.payload {
                        eg::TranslationPayload::Fixed(b) => (b.len() as u8, b),
                        eg::TranslationPayload::Derived => (wg::gp_translation_payload::DERIVED, &[]),
                        eg::TranslationPayload::Copy => (wg::gp_translation_payload::COPY_GPD, &[]),
                    };
                    wg::GpTranslation {
                        gpd_command: t.gpd_command,
                        endpoint: t.endpoint,
                        profile: t.profile,
                        cluster: t.cluster,
                        zcl_command: t.zcl_command,
                        payload_len,
                        payload,
                    }
                })
                .collect();
            enc(&wg::GpTranslationUpdate { dst: *dst, action: *action, gpd_id: wire_gpd(gpd_id), translations: List::new(&wire) })?
        }
        eg::GpRequest::TranslationRequest { dst, start_index } => {
            enc(&wg::GpTranslationRequest { dst: *dst, start_index: *start_index })?
        }
        eg::GpRequest::RemoteSinkTableRequest { dst, index, gpd_id } => {
            enc(&wg::GpRemoteSinkTableRequest { dst: *dst, index: *index, gpd_id: wire_gpd(gpd_id) })?
        }
        eg::GpRequest::RemoteProxyTableRequest { dst, index, gpd_id } => {
            enc(&wg::GpRemoteProxyTableRequest { dst: *dst, index: *index, gpd_id: wire_gpd(gpd_id) })?
        }
    };
    Ok((req.zrhp_id(), payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zigbee_zrhp::codec::Bytes16;
    use zigbee_zrhp::msg::Kind;

    #[test]
    fn indication_keeps_ieee_byte_order_and_group() {
        let ind = EvtApsDataIndication {
            dst_mode: 0x01,
            dst: 0x0B84,
            dst_ep: 1,
            src_nwk: 0x1234,
            src_ieee: Ieee::from_u64(0x00158d0001a2b3c4),
            src_ep: 1,
            profile: 0x0104,
            cluster: 0x0006,
            flags: indication_flags::SRC_IEEE_VALID,
            lqi: 200,
            rssi: -50,
            last_hop_nwk: 0x1234,
            aps_counter: 1,
            key_source: 0,
            rx_time_ms: 5,
            asdu: Bytes16(&[0x18, 0x01, 0x0A]),
        };
        let m = Message { kind: Kind::Event, msg_id: ids::EVT_APS_DATA_INDICATION, tid: 1, payload: crate::driver::encode_body(&ind).unwrap() };
        let Some(CoordEvent::Indication(i)) = coord_event(&m) else { panic!() };
        assert_eq!(i.src_ieee, Some(0x00158d0001a2b3c4));
        assert_eq!(i.group, Some(0x0B84));
        assert_eq!(i.payload, [0x18, 0x01, 0x0A]);
    }

    #[test]
    fn joins_say_how_refusals_and_leaves_are_events() {
        let ev = |msg_id, payload| coord_event(&Message { kind: Kind::Event, msg_id, tid: 1, payload });
        let join = |join_type, decision| {
            let e = EvtDeviceJoined { nwk: 0x1234, ieee: Ieee::from_u64(7), parent_nwk: 0, join_type, capability: 0x80, decision };
            ev(ids::EVT_DEVICE_JOINED, crate::driver::encode_body(&e).unwrap())
        };
        assert_eq!(join(2, 0), Some(CoordEvent::DeviceJoined { ieee: 7, nwk: 0x1234, kind: JoinKind::TcRejoin }));
        assert_eq!(join(0, 1), Some(CoordEvent::JoinDenied { ieee: 7, nwk: 0x1234, kind: JoinKind::Association }));
        let left = |flags, reason| {
            let e = EvtDeviceLeft { nwk: 0x1234, ieee: Ieee::from_u64(7), flags, reason };
            ev(ids::EVT_DEVICE_LEFT, crate::driver::encode_body(&e).unwrap())
        };
        assert_eq!(left(0x01, 1), Some(CoordEvent::DeviceLeft { ieee: 7, nwk: 0x1234, rejoin: Some(true), aged_out: false }));
        assert_eq!(left(0, 4), Some(CoordEvent::DeviceLeft { ieee: 7, nwk: 0x1234, rejoin: Some(false), aged_out: true }));
    }

    #[test]
    fn translation_payload_modes_round_trip() {
        let req = eg::GpRequest::TranslationUpdate {
            dst: 0x7C87,
            action: 0,
            gpd_id: eg::GpdId::SrcId(0x7A6B9879),
            translations: vec![
                eg::GpTranslation { gpd_command: 0x20, endpoint: 1, profile: 0x0104, cluster: 6, zcl_command: 0, payload: eg::TranslationPayload::Fixed(vec![]) },
                eg::GpTranslation { gpd_command: 0x22, endpoint: 1, profile: 0x0104, cluster: 8, zcl_command: 4, payload: eg::TranslationPayload::Derived },
            ],
        };
        let (id, payload) = gp_request(&req).unwrap();
        assert_eq!(id, ids::GP_TRANSLATION_UPDATE);
        let back: wg::GpTranslationUpdate = zigbee_zrhp::codec::Wire::read(&mut zigbee_zrhp::codec::Reader::new(&payload)).unwrap();
        let t: Vec<_> = back.translations.iter().map(|t| translation(&t)).collect();
        assert_eq!(t[1].payload, eg::TranslationPayload::Derived);
        assert_eq!(t[0].payload, eg::TranslationPayload::Fixed(vec![]));
    }
}
