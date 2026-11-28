//! [`NetworkAdmin`] on the host core: each method is the [`Hub`] call (with
//! its call options) the maintenance API made directly before.

use std::sync::Arc;
use std::time::{Duration, Instant};

use hub_coord::{
    BackendKind, BackupSink, Beacon, ChannelChange, Child, Code, CoordError, CoordStatus,
    DiagCounter, Diagnostics, EnergyChannel, FormParams, FrameCounters, NcpLog, NcpStatus,
    Neighbor, NetworkAdmin, NetworkParams, RestoreBlob, RestoreEncoder, RestoreOptions,
    RestoreReport, Route, TablesRead,
};
use zigbee_zrhp::msg::data::{ApsDataRequest, ApsDst, EvtApsDataIndication};
use zigbee_zrhp::msg::ids;
use zigbee_zrhp::msg::system::caps;
use zigbee_zrhp::types::Status;

use super::backup::{RecordEncoder, export_items};
use super::error::{coord_error, legacy_zb_status, zb_status_text};
use super::status::{self, Facts};
use crate::driver::Error;
use crate::lifecycle::{FormRequest, Hub};

/// Network administration of a ZRHP NCP. NCP firmware updates
/// ([`NetworkAdmin::firmware`]) are not offered yet: the maintenance API
/// does not update the NCP, `hubctl` and the firmware call the [`Hub`]
/// (coordinator boundary PR H).
#[derive(Clone)]
pub struct ZrhpAdmin {
    hub: Hub,
    facts: Arc<Facts>,
}

impl ZrhpAdmin {
    /// Administration through `hub`.
    pub fn new(hub: Hub) -> Self {
        Self {
            hub,
            facts: Arc::default(),
        }
    }
}

/// Counter names (ZRHP 5.12 link counters, 7.8 registry).
const COUNTER_NAMES: &[(u16, &str)] = &[
    (0x0101, "tx_frames"),
    (0x0102, "tx_bytes"),
    (0x0103, "tx_retransmissions"),
    (0x0104, "tx_rto_expiries"),
    (0x0105, "tx_rejected"),
    (0x0106, "tx_blocked_credit_ms"),
    (0x0107, "tx_blocked_window_ms"),
    (0x0108, "tx_discarded_no_host"),
    (0x0109, "naks_sent"),
    (0x010A, "naks_received"),
    (0x010B, "sessions_established"),
    (0x010C, "link_down_events"),
    (0x0111, "rx_frames"),
    (0x0112, "rx_bytes"),
    (0x0113, "rx_bad_frame"),
    (0x0114, "rx_duplicate"),
    (0x0115, "rx_out_of_order"),
    (0x0116, "rx_epoch_mismatch"),
    (0x0117, "rx_not_in_session"),
    (0x0118, "rx_rejected_sent"),
    (0x0119, "rx_overflow"),
    (0x011A, "rx_stale_reset"),
    (0x011B, "peer_restarted"),
    (0x011C, "peer_resync"),
    (0x0201, "mac_tx"),
    (0x0202, "mac_tx_ack_requested"),
    (0x0203, "mac_tx_no_ack"),
    (0x0204, "mac_tx_errors"),
    (0x0205, "mac_tx_deferred"),
    (0x0206, "mac_data_requests"),
    (0x0207, "frame_pending_acks"),
    (0x0208, "mac_ack_stall_recoveries"),
    (0x0220, "coex_denied"),
    (0x0221, "coex_ack_rx_denied"),
    (0x0222, "coex_ack_tx_denied"),
    (0x0301, "nwk_secured_rx"),
    (0x0302, "nwk_decrypt_failures"),
    (0x0303, "nwk_missing_key"),
    (0x0304, "nwk_replay_rejects"),
    (0x0305, "replay_table_full"),
    (0x0306, "nwk_security_header_errors"),
    (0x0401, "aps_tx"),
    (0x0402, "aps_tx_failed"),
    (0x0403, "aps_rx"),
    (0x0501, "joins_admitted"),
    (0x0502, "joins_denied"),
    (0x0503, "devices_authorized"),
    (0x0504, "authorization_failures"),
    (0x0505, "devices_left"),
    (0x0701, "aps_queue_high_water"),
    (0x0702, "busy_responses"),
    (0x0703, "indications_dropped"),
    (0x0704, "log_dropped"),
    (0x0705, "bad_batches"),
];

fn counter_name(id: u16) -> Option<&'static str> {
    COUNTER_NAMES
        .iter()
        .find(|(i, _)| *i == id)
        .map(|(_, n)| *n)
}

fn group_name(id: u16) -> &'static str {
    [
        "", "link", "mac", "nwk", "aps", "security", "tables", "queues", "gp",
    ]
    .get(usize::from(id >> 8))
    .copied()
    .unwrap_or("other")
}

fn counter((id, value): (u16, u32)) -> DiagCounter {
    DiagCounter {
        id,
        group: group_name(id),
        name: counter_name(id),
        value,
    }
}

impl NetworkAdmin for ZrhpAdmin {
    fn status(&self) -> Option<CoordStatus> {
        Some(status::status(&self.hub, &self.facts))
    }

    fn form(&self, p: FormParams) -> Result<NetworkParams, CoordError> {
        let req = FormRequest {
            channel_mask: p.channel_mask,
            pan_id: p.pan_id,
            ext_pan_id: p.ext_pan_id,
            network_key: p.network_key,
            tx_power: p.tx_power,
        };
        let r = self.hub.form(req).map_err(coord_error)?;
        Ok(NetworkParams {
            pan_id: r.pan_id,
            ext_pan_id: r.ext_pan_id,
            channel: r.channel,
            nwk_update_id: r.nwk_update_id,
            key_seq: r.key_seq,
            tx_power: Some(r.tx_power),
        })
    }

    fn leave_reset(&self, announce: bool) -> Result<(), CoordError> {
        self.hub.leave_reset(announce).map_err(coord_error)
    }

    fn energy_scan(&self, mask: u32, exp: u8) -> Result<Vec<EnergyChannel>, CoordError> {
        let scan = self.hub.energy_scan(mask, exp, 1).map_err(coord_error)?;
        Ok(scan
            .iter()
            .map(|c| EnergyChannel {
                channel: c.channel,
                max_rssi_dbm: c.max_rssi_dbm,
                energy: c.energy,
            })
            .collect())
    }

    fn active_scan(&self, mask: u32, exp: u8) -> Result<Vec<Beacon>, CoordError> {
        let found = self.hub.active_scan(mask, exp).map_err(coord_error)?;
        Ok(found
            .iter()
            .map(|b| Beacon {
                channel: b.channel,
                pan_id: b.pan,
                ext_pan_id: b.epid,
                src: b.src,
                lqi: b.lqi,
                rssi: b.rssi,
                permit_join: b.permit != 0,
                stack_profile: b.stack_profile,
                update_id: b.update_id,
            })
            .collect())
    }

    fn change_channel(&self, ch: u8) -> Result<ChannelChange, CoordError> {
        let r = self.hub.change_channel(ch).map_err(coord_error)?;
        Ok(ChannelChange {
            channel: r.channel,
            nwk_update_id: r.nwk_update_id,
        })
    }

    fn set_tx_power(&self, dbm: i8) -> Result<i8, CoordError> {
        self.hub.set_tx_power(dbm).map_err(coord_error)
    }

    fn tx_power_setting(&self) -> Option<i8> {
        self.hub.stored_tx_power()
    }

    fn ncp_log(&self, level: Option<u8>) -> Result<NcpLog, CoordError> {
        if let Some(level) = level {
            self.hub.set_ncp_log(level).map_err(coord_error)?;
        }
        let l = self.hub.ncp_log();
        Ok(NcpLog {
            level: l.level,
            forwarded: l.forwarded,
            suppressed: l.suppressed,
            dropped: l.dropped,
        })
    }

    fn coordinator_tables(&self, routes: bool) -> TablesRead {
        let neighbors = self
            .hub
            .coordinator_neighbors()
            .map_err(coord_error)
            .map(|list| {
                list.iter()
                    .map(|n| Neighbor {
                        ieee: n.ieee.0,
                        nwk: n.nwk,
                        device_type: n.device_type,
                        rx_on: n.rx_on != 0,
                        relationship: n.relationship,
                        lqi: n.lqi,
                        outgoing_cost: n.outgoing_cost,
                        age: n.age,
                    })
                    .collect()
            });
        let children = self
            .hub
            .coordinator_children()
            .map_err(coord_error)
            .map(|list| {
                list.iter()
                    .map(|c| Child {
                        ieee: c.ieee.0,
                        nwk: c.nwk,
                        capability: c.capability,
                        timeout_exp: c.timeout_exp,
                        last_poll_ms: c.last_poll_ms,
                    })
                    .collect()
            });
        let routes = routes.then(|| {
            self.hub
                .coordinator_routes()
                .map_err(coord_error)
                .map(|list| {
                    list.iter()
                        .map(|r| Route {
                            dst: r.dst,
                            next_hop: r.next_hop,
                            status: r.status,
                            flags: r.flags,
                        })
                        .collect()
                })
        });
        TablesRead {
            neighbors,
            children,
            routes,
        }
    }

    fn zdo_request(
        &self,
        nwk: u16,
        cluster: u16,
        zdp: &[u8],
        t: Duration,
    ) -> Result<Vec<u8>, CoordError> {
        let Some(&tsn) = zdp.first() else {
            return Err(CoordError::Other(
                "no ZDP transaction sequence number".into(),
            ));
        };
        let req = ApsDataRequest {
            dst: ApsDst::Nwk(nwk),
            dst_ep: 0,
            src_ep: 0,
            profile: 0,
            cluster,
            tx_options: 0,
            radius: 0,
            non_member_radius: 0,
            priority: 1,
            relays: None,
            alias: None,
            asdu: zdp,
        };
        // Subscribed before sending: the response may precede the confirm.
        let events = self.hub.subscribe();
        let confirm = self.hub.send_aps(&req, t).map_err(|e| match e {
            // No confirm in time: delivered or not, unknown.
            Error::Timeout => CoordError::described(CoordError::DeliveryUnknown, e.to_string()),
            e => coord_error(e),
        })?;
        if confirm.status != 0 {
            // Named in the numbering of the NCP image in use.
            let legacy =
                (self.hub.client().session()).is_some_and(|s| legacy_zb_status(&s.negotiated.fw));
            let code = Code {
                backend: BackendKind::Zrhp,
                value: u16::from(confirm.zb_status),
                text: zb_status_text(confirm.zb_status, legacy),
            };
            return Err(CoordError::NoAck(code));
        }
        let until = Instant::now() + t;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            let Ok(m) = events.recv_timeout(left) else {
                break;
            };
            if m.msg_id != ids::EVT_APS_DATA_INDICATION {
                continue;
            }
            let Ok(ind) = m.decode::<EvtApsDataIndication>() else {
                continue;
            };
            if ind.src_nwk == nwk
                && ind.profile == 0
                && ind.cluster == cluster | 0x8000
                && ind.asdu.0.first() == Some(&tsn)
            {
                return Ok(ind.asdu.0[1..].to_vec());
            }
        }
        Err(CoordError::Timeout)
    }

    fn diagnostics(&self, groups: u8, reset: bool) -> Diagnostics {
        let ncp = &self.hub;
        let mut unsupported: Vec<&'static str> = Vec::new();
        let mut errors = Vec::new();
        let mut note = |what: &'static str, e: Error| match e {
            Error::Status(Status::UnsupportedCommand) => unsupported.push(what),
            e => errors.push(format!("{what}: {e}")),
        };
        if reset && let Err(e) = ncp.diag_reset_counters(groups) {
            note("DIAG_RESET_COUNTERS", e);
        }
        let counters = ncp.diag_counters(groups).unwrap_or_else(|e| {
            note("DIAG_GET_COUNTERS", e);
            Vec::new()
        });
        let status = ncp.ncp_status().map_err(|e| note("SYS_GET_STATUS", e)).ok();
        let fc = ncp
            .frame_counters()
            .map_err(|e| note("SEC_GET_FRAME_COUNTERS", e))
            .ok();
        let host = ncp.client().counters().entries();
        Diagnostics {
            status: status.map(|s| NcpStatus {
                network_state: s.network_state,
                uptime_s: s.uptime_s,
                free_aps_slots: s.free_aps_slots,
                host_absent_queue: s.host_absent_queue,
                dirty_writebacks: s.dirty_writebacks,
                restore_in_progress: s.restore_in_progress != 0,
                key_export_locked: s.flags & 0x01 != 0,
                paging_active: s.flags & 0x02 != 0,
                pta_coex: s.flags & 0x04 != 0,
                gp_commissioning: s.flags & 0x08 != 0,
                permit_join: s.flags & 0x10 != 0,
            }),
            counters: counters.into_iter().map(counter).collect(),
            frame_counters: fc.map(|f| FrameCounters {
                nwk: f.nwk_current,
                nwk_limit: f.nwk_limit,
                aps: f.aps_current,
                aps_limit: f.aps_limit,
                stride: f.stride,
            }),
            host_link: host.into_iter().map(counter).collect(),
            coex: ncp.ncp_has(caps::PTA_COEX),
            tx_power_set: ncp.stored_tx_power(),
            ezsp_hashed_tclk: Some(ncp.ncp_has(caps::EZSP_HASHED_TCLK)),
            ncp_log: self.ncp_log(None).ok(),
            unsupported,
            errors,
        }
    }

    fn backup_export(&self, keys: bool, sink: &mut dyn BackupSink) -> Result<(), CoordError> {
        let stream = self.hub.backup_export(keys).map_err(coord_error)?;
        export_items(&stream, sink)
    }

    fn restore_encoder(&self) -> Result<Box<dyn RestoreEncoder>, CoordError> {
        Ok(Box::<RecordEncoder>::default())
    }

    fn backup_restore(
        &self,
        blob: RestoreBlob,
        o: RestoreOptions,
    ) -> Result<RestoreReport, CoordError> {
        let opts = crate::lifecycle::RestoreOptions {
            allow_ieee_change: o.allow_ieee_change,
            nwk_margin: o.nwk_margin,
            aps_margin: o.aps_margin,
        };
        let r = self
            .hub
            .backup_restore(&blob.0, opts)
            .map_err(coord_error)?;
        Ok(RestoreReport {
            nwk_fc_start: r.nwk_fc_start,
            aps_fc_start: r.aps_fc_start,
            link_keys_on_ncp: r.link_keys_on_ncp,
            link_keys_host_backed: r.link_keys_host_backed,
            children: r.children,
            notes: r.notes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_names() {
        assert_eq!(counter_name(0x0101), Some("tx_frames"));
        assert_eq!(counter_name(0x0113), Some("rx_bad_frame"));
        assert_eq!(counter_name(0x011C), Some("peer_resync"));
        assert_eq!(counter_name(0x0702), Some("busy_responses"));
        assert_eq!(counter_name(0x0203), Some("mac_tx_no_ack"));
        assert_eq!(counter_name(0x0505), Some("devices_left"));
        assert_eq!(counter_name(0x0805), None);
        assert_eq!(group_name(0x0805), "gp");
        assert_eq!(
            counter((0x0805, 3)),
            DiagCounter {
                id: 0x0805,
                group: "gp",
                name: None,
                value: 3
            }
        );
    }
}
