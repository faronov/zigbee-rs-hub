//! Tuya EF00 runtime (spec 10.2, 10.3): DP dispatch, de-duplication,
//! unknown-DP diagnostics, time sync, gateway status, MCU version, queries
//! and DP writes.

use crate::device::Millis;
use crate::hub::{DKey, DevEnv, Hub, Purpose, eval_trigger, set_value};
use crate::model::{Binding, DeviceField, TimeSync, TriggerSource, TuyaSend, TuyaType};
use crate::transport::ZigbeeTransport;
use crate::tuya::{self, DpRecord, cmd};
use crate::zcl::{FrameType, PROFILE_GP, ZclFrame, ZclHeader, cluster as cl};

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
            .and_then(|d| d.fp.endpoints.iter().filter(|e| e.profile != PROFILE_GP).find(|e| e.in_clusters.contains(&cl::TUYA)).map(|e| e.ep))
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
        let Some(d) = self.devices.get_mut(id) else { return };
        let epoch = d.resolution.tuya.as_ref().map_or(TimeSync::Off, |t| t.time_sync);
        let Some(p) = tuya::time_sync_payload((now / 1000) as i64, self.cfg.tz_offset_s, epoch) else { return };
        d.tuya_time_at = Some(now);
        let ep = self.tuya_ep(id);
        let f = self.tuya_frame(cmd::MCU_SYNC_TIME, p);
        self.send_zcl(id, ep, cl::TUYA, f, Purpose::None, now);
    }

    /// Sends DP records with dataRequest / sendData; returns the frame TSN.
    pub(crate) fn tuya_send(&mut self, id: &str, records: &[DpRecord], now: Millis) {
        let Some(d) = self.devices.get_mut(id) else { return };
        d.tuya_seq = d.tuya_seq.wrapping_add(1);
        let seq = d.tuya_seq;
        let c = match d.resolution.tuya.as_ref().map_or(TuyaSend::DataRequest, |t| t.send) {
            TuyaSend::DataRequest => cmd::DATA_REQUEST,
            TuyaSend::SendData => cmd::SEND_DATA,
        };
        let ep = self.tuya_ep(id);
        let f = self.tuya_frame(c, tuya::encode_data(seq, records));
        self.send_zcl(id, ep, cl::TUYA, f, Purpose::None, now);
    }

    pub(crate) fn handle_tuya(&mut self, id: &str, ep: u8, h: &ZclHeader, payload: &[u8], now: Millis) {
        let Some(d) = self.devices.get(id) else { return };
        let flags = d.resolution.tuya.clone().unwrap_or_default();
        // Forced time pushes on any message.
        if let Some(s) = flags.force_time_sync_s {
            if d.tuya_time_at.is_none_or(|t| now.saturating_sub(t) >= u64::from(s) * 1000) && flags.time_sync != TimeSync::Off {
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
                for r in recs {
                    self.tuya_dp(id, ep, seq, &r, flags.dedup, now);
                }
            }
            cmd::MCU_SYNC_TIME => self.tuya_time(id, now),
            cmd::MCU_GATEWAY_CONNECTION_STATUS if flags.gateway_status => {
                let f = self.tuya_frame(cmd::MCU_GATEWAY_CONNECTION_STATUS, tuya::gateway_status_payload());
                self.send_zcl(id, ep, cl::TUYA, f, Purpose::None, now);
            }
            cmd::MCU_VERSION_RESPONSE if payload.len() >= 3 => {
                let v = tuya::mcu_version(payload[2]);
                if let Some(d) = self.devices.get_mut(id) {
                    let idx: Vec<usize> = (0..d.entities.len())
                        .filter(|&i| matches!(d.entities[i].r.def.value.binding, Binding::DeviceField(DeviceField::McuVersion)))
                        .collect();
                    for i in &idx {
                        set_value(d, *i, None, crate::expr::Value::Str(v.clone()), now);
                    }
                    let uids: Vec<String> = idx.iter().map(|&i| d.entities[i].uid.clone()).collect();
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

    fn tuya_dp(&mut self, id: &str, _ep: u8, seq: u16, r: &DpRecord, dedup: bool, now: Millis) {
        let Some(d) = self.devices.get_mut(id) else { return };
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
        let bound = self.dispatch.get(id).and_then(|t| t.get(&DKey::Tuya(r.dp))).cloned().unwrap_or_default();
        let d = self.devices.get_mut(id).unwrap();
        if bound.is_empty() {
            d.diag.tuya_dps.insert(r.dp, (r.ty, crate::expr::hex(&r.data), now));
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
            let env = DevEnv { dev: d, now, tz: self.cfg.tz_offset_s };
            for (i, e) in d.entities.iter().enumerate() {
                if e.orphaned {
                    continue;
                }
                for t in &e.r.def.triggers {
                    if t.source == (TriggerSource::Tuya { dp: r.dp }) {
                        if let Some(ev) = eval_trigger(t, &r.data, e.state.last_event.as_deref(), &env) {
                            fired.push((i, ev.0, ev.1));
                            break;
                        }
                    }
                }
            }
        }
        self.apply_raw(id, work, now);
        for (i, et, attrs) in fired {
            self.fire_event(id, i, et, attrs, now);
        }
        self.tuya_confirm(id, r.dp, now);
    }
}
