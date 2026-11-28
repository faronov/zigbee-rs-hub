//! Tuya EF00 runtime (spec 10.2, 10.3): DP dispatch, de-duplication,
//! unknown-DP diagnostics, time sync, gateway status, MCU version, queries
//! and DP writes.

use crate::device::Millis;
use std::collections::BTreeMap;

use crate::hub::{DKey, DevEnv, Hub, Purpose, Ticket, eval_trigger, set_value, tuya_awaited};
use crate::model::{Binding, DeviceField, TimeSync, TriggerSource, TuyaSend, TuyaType};
use crate::transport::{ApsRequest, ZigbeeTransport};
use crate::tuya::{self, DpRecord, cmd};
use crate::zcl::{FrameType, PROFILE_GP, ZclFrame, ZclHeader, cluster as cl};

/// How long after the hub's dataQuery (any DP) or DP write (those DPs)
/// goes out the device's data frames answer it.
const ANSWER_MS: Millis = 10_000;
/// Outstanding requests noted per device; beyond, any DP answers for
/// [`ANSWER_MS`].
const ASKED_MAX: usize = 16;

/// Whether a request to `id` that went out at `at` (`p`: the DP written,
/// `None`: a query) may still be answered: for [`ANSWER_MS`], a write as
/// long as a command waits for its DP.
fn asking(
    tickets: &BTreeMap<u64, Ticket>,
    id: &str,
    at: Millis,
    p: Option<u8>,
    now: Millis,
) -> bool {
    now.saturating_sub(at) < ANSWER_MS || p.is_some_and(|p| tuya_awaited(tickets, id, p))
}

fn fnv(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in *p {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h ^= 0xFF;
    }
    h | (1 << 63)
}

impl<T: ZigbeeTransport> Hub<T> {
    fn tuya_ep(&self, id: &str) -> u8 {
        self.devices
            .get(id)
            .and_then(|d| {
                d.fp.endpoints
                    .iter()
                    .filter(|e| e.profile != PROFILE_GP)
                    .find(|e| e.in_clusters.contains(&cl::TUYA))
                    .map(|e| e.ep)
            })
            .unwrap_or(1)
    }

    fn tuya_frame(&mut self, cmd_id: u8, payload: Vec<u8>) -> ZclFrame {
        let tsn = self.next_zcl_tsn();
        let mut f = ZclFrame::command(FrameType::Cluster, tsn, cmd_id, None, payload);
        f.header.disable_default_response = true;
        f
    }

    /// dataQuery (0x03).
    pub(crate) fn tuya_query(&mut self, id: &str, now: Millis) {
        let ep = self.tuya_ep(id);
        let f = self.tuya_frame(cmd::DATA_QUERY, Vec::new());
        self.send_zcl(id, ep, cl::TUYA, f, Purpose::None, now);
    }

    fn tuya_time(&mut self, id: &str, now: Millis) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        let epoch = d
            .resolution
            .tuya
            .as_ref()
            .map_or(TimeSync::Off, |t| t.time_sync);
        let Some(p) = tuya::time_sync_payload(
            crate::device::wall_ms(now).div_euclid(1000),
            self.cfg.tz_offset_s,
            epoch,
        ) else {
            return;
        };
        d.tuya_time_at = Some(now);
        let ep = self.tuya_ep(id);
        let f = self.tuya_frame(cmd::MCU_SYNC_TIME, p);
        self.send_zcl(id, ep, cl::TUYA, f, Purpose::None, now);
    }

    /// Sends DP records with dataRequest / sendData; returns the frame TSN.
    pub(crate) fn tuya_send(&mut self, id: &str, records: &[DpRecord], now: Millis) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        d.tuya_seq = d.tuya_seq.wrapping_add(1);
        let seq = d.tuya_seq;
        let c = match d
            .resolution
            .tuya
            .as_ref()
            .map_or(TuyaSend::DataRequest, |t| t.send)
        {
            TuyaSend::DataRequest => cmd::DATA_REQUEST,
            TuyaSend::SendData => cmd::SEND_DATA,
        };
        let ep = self.tuya_ep(id);
        let f = self.tuya_frame(c, tuya::encode_data(seq, records));
        self.send_zcl(id, ep, cl::TUYA, f, Purpose::None, now);
    }

    /// Notes the hub's dataQuery or DP write to `id` as it goes out (a
    /// sleepy device's queued frame when it is sent, whoever built it).
    #[inline(never)]
    pub(crate) fn tuya_note_sent(&mut self, id: &str, req: &ApsRequest, now: Millis) {
        let f = &req.payload;
        if req.cluster != cl::TUYA || req.profile == PROFILE_GP || f.len() < 3 {
            return;
        }
        // Cluster-specific, client to server; the command after the TSN
        // (and a manufacturer code).
        let c = if f[0] & 0x04 != 0 { 4 } else { 2 };
        if f[0] & 0x0B != 0x01
            || !matches!(
                f.get(c),
                Some(&(cmd::DATA_QUERY | cmd::DATA_REQUEST | cmd::SEND_DATA))
            )
        {
            return;
        }
        let (rx, tickets) = (self.rx_n, &self.tickets);
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        let asked = &mut d.tuya_asked;
        asked.retain(|&(at, _, p)| asking(tickets, id, at, p, now));
        // Again: the same request refreshed in place.
        let mut note = |p: Option<u8>| match asked.iter_mut().find(|e| e.2 == p) {
            Some(e) => *e = (now, rx, p),
            None => asked.push((now, rx, p)),
        };
        if f[c] == cmd::DATA_QUERY {
            note(None);
        } else {
            // Sequence number (2), then DP, type, length (2, BE), data.
            let mut i = c + 3;
            while i + 4 <= f.len() {
                note(Some(f[i]));
                i += 4 + usize::from(u16::from_be_bytes([f[i + 2], f[i + 3]]));
            }
        }
        if asked.len() > ASKED_MAX {
            asked.clear();
            asked.push((now, rx, None));
        }
    }

    /// Whether the device's data frame of `dp` (the indication handled)
    /// answers the hub: a dataQuery or a write of `dp` that went out less
    /// than [`ANSWER_MS`] before, a write as long as a command waits for
    /// its DP; never the uplink that released a sleepy device's queued
    /// request. Otherwise it is the device's own, whatever the command:
    /// many TS0601 send their state changes as dataResponse, and answer a
    /// query or a write with dataReports (zigbee-herdsman and ZHA take
    /// them all alike).
    fn tuya_answers(&mut self, id: &str, dp: u8, now: Millis) -> bool {
        let (rx, tickets) = (self.rx_n, &self.tickets);
        let Some(d) = self.devices.get_mut(id) else {
            return false;
        };
        let mut answers = false;
        d.tuya_asked.retain(|&(at, n, p)| {
            let keep = asking(tickets, id, at, p, now);
            answers |= keep && n != rx && p.is_none_or(|p| p == dp);
            keep
        });
        answers
    }

    pub(crate) fn handle_tuya(
        &mut self,
        id: &str,
        ep: u8,
        h: &ZclHeader,
        payload: &[u8],
        now: Millis,
    ) {
        let Some(d) = self.devices.get(id) else {
            return;
        };
        let flags = d.resolution.tuya.clone().unwrap_or_default();
        // Forced time pushes on any message.
        if let Some(s) = flags.force_time_sync_s {
            if d.tuya_time_at
                .is_none_or(|t| now.saturating_sub(t) >= u64::from(s) * 1000)
                && flags.time_sync != TimeSync::Off
            {
                self.tuya_time(id, now);
            }
        }
        match h.cmd {
            c if tuya::is_data_command(c) => {
                let Ok((seq, recs)) = tuya::parse_data(payload) else {
                    if let Some(d) = self.devices.get_mut(id) {
                        d.diag.count("tuya_malformed");
                    }
                    return;
                };
                // By context, not by command: what answers the hub's query
                // or write is not the device's own (`pushed`).
                for r in recs {
                    let push = !self.tuya_answers(id, r.dp, now);
                    self.tuya_dp(id, ep, seq, &r, flags.dedup, push, now);
                }
            }
            cmd::MCU_SYNC_TIME => self.tuya_time(id, now),
            cmd::MCU_GATEWAY_CONNECTION_STATUS if flags.gateway_status => {
                let f = self.tuya_frame(
                    cmd::MCU_GATEWAY_CONNECTION_STATUS,
                    tuya::gateway_status_payload(),
                );
                self.send_zcl(id, ep, cl::TUYA, f, Purpose::None, now);
            }
            cmd::MCU_VERSION_RESPONSE if payload.len() >= 3 => {
                let v = tuya::mcu_version(payload[2]);
                if let Some(d) = self.devices.get_mut(id) {
                    let idx: Vec<usize> = (0..d.entities.len())
                        .filter(|&i| {
                            matches!(
                                d.entities[i].r.def.value.binding,
                                Binding::DeviceField(DeviceField::McuVersion)
                            )
                        })
                        .collect();
                    for i in &idx {
                        set_value(d, *i, None, crate::expr::Value::Str(v.clone()), now);
                    }
                    let uids: Vec<String> =
                        idx.iter().map(|&i| d.entities[i].uid.clone()).collect();
                    for u in uids {
                        self.publish_state(&u);
                    }
                }
            }
            _ => {
                if let Some(d) = self.devices.get_mut(id) {
                    d.diag.count("tuya_unhandled_command");
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn tuya_dp(
        &mut self,
        id: &str,
        _ep: u8,
        seq: u16,
        r: &DpRecord,
        dedup: bool,
        push: bool,
        now: Millis,
    ) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        if dedup {
            let key = fnv(&[&seq.to_be_bytes(), &[r.dp, r.ty], &r.data]);
            d.dedup.retain(|(t, _)| now.saturating_sub(*t) < 2_000);
            if d.dedup.iter().any(|(_, k)| *k == key) {
                d.diag.count("tuya_duplicate");
                return;
            }
            d.dedup.push_back((now, key));
        }
        let raw = tuya::dp_value(r.ty, &r.data);
        let bound = self
            .dispatch
            .get(id)
            .and_then(|t| t.get(&DKey::Tuya(r.dp)))
            .cloned()
            .unwrap_or_default();
        let d = self.devices.get_mut(id).unwrap();
        if bound.is_empty() {
            d.diag
                .tuya_dps
                .insert(r.dp, (r.ty, crate::expr::hex(&r.data), now));
        }
        let mut work = Vec::new();
        for (i, p) in bound {
            let vd = crate::hub::value_of(&d.entities[i].r.def, p);
            if let Binding::Tuya { ty, .. } = vd.binding {
                if ty as u8 != r.ty && !(ty == TuyaType::Raw && r.ty == TuyaType::String as u8) {
                    d.diag.count("tuya_type_mismatch");
                }
            }
            work.push((i, p, raw.clone()));
        }
        // Tuya triggers (events from DPs).
        let mut fired = Vec::new();
        {
            let env = DevEnv {
                dev: d,
                now,
                tz: self.cfg.tz_offset_s,
            };
            for (i, e) in d.entities.iter().enumerate() {
                if e.orphaned {
                    continue;
                }
                for t in &e.r.def.triggers {
                    if t.source == (TriggerSource::Tuya { dp: r.dp }) {
                        if let Some(ev) =
                            eval_trigger(t, &r.data, e.state.last_event.as_deref(), &env)
                        {
                            fired.push((i, ev.0, ev.1));
                            break;
                        }
                    }
                }
            }
        }
        self.apply_raw(id, work, push, now);
        for (i, et, attrs) in fired {
            self.fire_event(id, i, et, attrs, now);
        }
        self.tuya_confirm(id, r.dp, now);
    }
}
