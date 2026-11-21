//! Network maintenance on top of the lifecycle (a child module of
//! [`lifecycle`](super), so it shares the hub's state): energy scan,
//! channel change, TX power, NCP diagnostics, the coordinator's own tables,
//! backup export and restore as ZRHP record streams (spec 9), and the
//! leave/reset that precedes forming a new network.
//!
//! Every operation is a blocking call; the application runs them off its
//! own thread. Keys only travel inside record streams and are never logged.

use std::time::Duration;

use log::{info, warn};
use zigbee_zrhp::codec::{Reader, Wire, Writer};
use zigbee_zrhp::msg::backup::{
    BkpExportBegin, BkpExportBeginRsp, BkpExportEnd, BkpExportRead, BkpRestoreAbort, BkpRestoreBegin,
    BkpRestoreBeginRsp, BkpRestoreCommit, BkpRestoreCommitRsp, BkpRestorePutRsp,
};
use zigbee_zrhp::msg::diag::{
    Child, DiagGetChildren, DiagGetChildrenRsp, DiagGetCounters, DiagGetCountersRsp, DiagGetNeighbors,
    DiagGetNeighborsRsp, DiagGetRoutes, DiagGetRoutesRsp, DiagResetCounters, Neighbor, Route,
};
use zigbee_zrhp::msg::{MessageDef, ids};
use zigbee_zrhp::msg::network::{
    EvtChannelChanged, EvtEnergyScanResult, NwkChangeChannel, NwkEnergyScan, NwkLeaveReset, NwkSetTxPower,
    NwkSetTxPowerRsp,
};
use zigbee_zrhp::msg::security::SecGetFrameCountersRsp;
use zigbee_zrhp::msg::system::{SysGetStatusRsp, caps};
use zigbee_zrhp::records::{EndRecord, RawRecords, Record, StreamCrc, record_type, verify_stream};
use zigbee_zrhp::types::Status;

use super::{Hub, Ownership};
use crate::driver::{self, CallOpts, Confirm, Error};
use crate::store::{Change, FcRecord, Meta, NetworkRecord};

/// Host store key of the TX power the operator chose (one `i8`), replayed
/// on every resume: the NCP does not keep it across a reboot.
pub const TX_POWER_KEY: &str = "cfg/tx_power";

/// Record bytes per `BKP_EXPORT_READ` (the NCP caps it at 480).
const EXPORT_CHUNK: u16 = 480;
/// Record bytes per `BKP_RESTORE_PUT` (one message, well below a frame).
const RESTORE_CHUNK: usize = 400;
/// Upper bound of an exported stream (about 1 500 devices with keys).
const MAX_STREAM: usize = 64 * 1024;
/// Bound on table pages.
const MAX_PAGES: usize = 32;

/// One channel of an energy scan (`EVT_ENERGY_SCAN_RESULT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnergyChannel {
    /// Channel (11–26).
    pub channel: u8,
    /// Highest RSSI seen, dBm (antenna-referred).
    pub max_rssi_dbm: i8,
    /// Energy (0–255, ED scale).
    pub energy: u8,
}

/// A completed channel change (`EVT_CHANNEL_CHANGED`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelChange {
    /// The channel the radio is on (the old one if the tune failed).
    pub channel: u8,
    /// NWK update ID now in use.
    pub nwk_update_id: u8,
}

/// Restore parameters (spec 9.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestoreOptions {
    /// Let the NCP take the backup's coordinator IEEE (`IEEE_REWRITABLE`).
    pub allow_ieee_change: bool,
    /// Added to the network-key frame counter.
    pub nwk_margin: u32,
    /// Added to the APS frame counter (2²⁰ when the backup has no TCLK
    /// counter, spec 9.4 rule 6).
    pub aps_margin: u32,
}

impl Default for RestoreOptions {
    fn default() -> Self {
        Self { allow_ieee_change: true, nwk_margin: 10_000, aps_margin: 10_000 }
    }
}

/// Outcome of [`Hub::backup_restore`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    /// First NWK frame counter of the restored network.
    pub nwk_fc_start: u32,
    /// First APS frame counter.
    pub aps_fc_start: u32,
    /// Link keys the NCP caches.
    pub link_keys_on_ncp: u16,
    /// Link keys only the host holds.
    pub link_keys_host_backed: u16,
    /// Children restored into the NCP's child table.
    pub children: u16,
    /// `(record index, note)` of `BKP_RESTORE_PUT` (spec 9.4 notes).
    pub notes: Vec<(u16, u8)>,
}

fn status_of(e: &Error) -> Option<Status> {
    match e {
        Error::Status(s) => Some(*s),
        _ => None,
    }
}

impl Hub {
    fn caps(&self) -> u64 {
        self.inner.client.session().map_or(0, |s| s.negotiated.ncp_caps)
    }

    /// Whether the current NCP advertises every bit of `caps`
    /// ([`zigbee_zrhp::msg::system::caps`]).
    pub fn ncp_has(&self, caps: u64) -> bool {
        self.caps() & caps == caps
    }

    /// `NWK_ENERGY_SCAN` and its `EVT_ENERGY_SCAN_RESULT` (spec 7.2):
    /// `duration_exp` per channel (15.36 ms × (2ⁿ + 1)), `count` scans.
    pub fn energy_scan(&self, channel_mask: u32, duration_exp: u8, count: u8) -> Result<Vec<EnergyChannel>, Error> {
        let channels = (channel_mask & 0x07FF_F800).count_ones().max(1);
        let per_channel_ms = 15.36 * f64::from((1u32 << duration_exp.min(14)) + 1);
        let scan = Duration::from_millis((per_channel_ms * f64::from(channels) * f64::from(count.max(1))) as u64);
        let body = NwkEnergyScan { channel_mask, duration_exp, count: count.max(1) };
        // The NCP measures one channel per pass between stack steps
        // (zigbee-rs 3ad4a999), so allow twice the radio time.
        let confirm = Confirm { event_id: ids::EVT_ENERGY_SCAN_RESULT, tid_offset: 0, timeout: scan * 2 + Duration::from_secs(15) };
        let opts = CallOpts { confirm: Some(confirm), ..CallOpts::default() };
        let outcome = self.inner.client.call_raw(NwkEnergyScan::ID, &driver::encode_body(&body)?, opts)?;
        let ev: EvtEnergyScanResult = outcome.confirm.as_ref().ok_or(Error::Codec)?.decode()?;
        if ev.status != 0 {
            return Err(Error::Status(Status::from_u8(ev.status)));
        }
        Ok(ev
            .results
            .iter()
            .map(|r| EnergyChannel { channel: r.channel, max_rssi_dbm: r.max_rssi_dbm, energy: r.energy })
            .collect())
    }

    /// Move the running network to `channel` (`NWK_CHANGE_CHANNEL`, spec
    /// 7.2): the NCP broadcasts `Mgmt_NWK_Update_req` and tunes about 9 s
    /// later. The store follows the new channel and update ID.
    pub fn change_channel(&self, channel: u8) -> Result<ChannelChange, Error> {
        if !(11..=26).contains(&channel) {
            return Err(Error::Status(Status::InvalidParameter));
        }
        let confirm = Confirm { event_id: ids::EVT_CHANNEL_CHANGED, tid_offset: 2, timeout: Duration::from_secs(30) };
        let opts = CallOpts { confirm: Some(confirm), ..CallOpts::default() };
        let body = NwkChangeChannel { channel, nwk_update_id: 0 };
        let outcome = self.inner.client.call_raw(NwkChangeChannel::ID, &driver::encode_body(&body)?, opts)?;
        let ev: EvtChannelChanged = outcome.confirm.as_ref().ok_or(Error::Codec)?.decode()?;
        self.on_channel_changed(ev.channel, ev.nwk_update_id);
        let _ = self.get_network_info(CallOpts::default());
        if ev.channel != channel {
            return Err(Error::Other(format!(
                "the NCP stayed on channel {} (the new channel {channel} is stored and used after its next reboot)",
                ev.channel
            )));
        }
        info!("hub: network moved to channel {} (update ID {})", ev.channel, ev.nwk_update_id);
        Ok(ChannelChange { channel: ev.channel, nwk_update_id: ev.nwk_update_id })
    }

    /// The store and status follow a channel change (also one the NCP
    /// reports on its own).
    pub(super) fn on_channel_changed(&self, channel: u8, nwk_update_id: u8) {
        let stored = self.inner.store.lock().unwrap().state().network;
        if let Some(mut n) = stored
            && (n.channel != channel || n.nwk_update_id != nwk_update_id)
        {
            n.channel = channel;
            n.nwk_update_id = nwk_update_id;
            self.commit(Change::Network(n));
        }
        self.update(|s| {
            if let Some(n) = s.network.as_mut() {
                n.channel = channel;
                n.nwk_update_id = nwk_update_id;
            }
        });
    }

    /// `NWK_SET_TX_POWER` (antenna-referred dBm); returns what the NCP
    /// applied. Persisted and replayed on every resume.
    pub fn set_tx_power(&self, dbm: i8) -> Result<i8, Error> {
        let applied = self.apply_tx_power(dbm, false)?;
        self.with_store(|s| s.kv_put(TX_POWER_KEY, &[dbm as u8])).map_err(|e| Error::Other(e.to_string()))?;
        Ok(applied)
    }

    /// The TX power the operator set (`None`: the NCP's default).
    pub fn stored_tx_power(&self) -> Option<i8> {
        self.inner.store.lock().unwrap().kv_get(TX_POWER_KEY).and_then(|v| v.first()).map(|b| *b as i8)
    }

    pub(super) fn apply_tx_power(&self, dbm: i8, during_resume: bool) -> Result<i8, Error> {
        let opts = CallOpts { during_resume, ..CallOpts::timeout(Duration::from_secs(5)) };
        let rsp = self.inner.client.call(&NwkSetTxPower { dbm }, opts)?;
        let applied = rsp.body::<NwkSetTxPowerRsp>().map_or(dbm, |r| r.applied_dbm);
        self.update(|s| {
            if let Some(n) = s.network.as_mut() {
                n.tx_power = applied;
            }
        });
        Ok(applied)
    }

    /// Replay of [`set_tx_power`](Self::set_tx_power) in a resume; an NCP
    /// without `NWK_SET_TX_POWER` only gets a warning.
    pub(super) fn replay_tx_power(&self) -> Result<(), Error> {
        let Some(dbm) = self.stored_tx_power() else { return Ok(()) };
        match self.apply_tx_power(dbm, true) {
            Ok(applied) => {
                info!("hub: TX power {dbm} dBm replayed (applied {applied} dBm)");
                Ok(())
            }
            Err(e @ (Error::LinkReset | Error::Closed)) => Err(e),
            Err(e) => {
                warn!("hub: NWK_SET_TX_POWER {dbm}: {e}");
                Ok(())
            }
        }
    }

    /// `DIAG_GET_COUNTERS` for the groups in `group_mask` (bit0 link, bit1
    /// MAC, bit2 NWK, bit3 APS, bit4 security, bit5 tables, bit6 queues,
    /// bit7 GP): `(counter ID, value)`.
    pub fn diag_counters(&self, group_mask: u8) -> Result<Vec<(u16, u32)>, Error> {
        let rsp = self.inner.client.call(&DiagGetCounters { group_mask }, CallOpts::default())?;
        let body: DiagGetCountersRsp = rsp.body()?;
        Ok(body.counters.iter().map(|c| (c.id, c.value)).collect())
    }

    /// `DIAG_RESET_COUNTERS`.
    pub fn diag_reset_counters(&self, group_mask: u8) -> Result<(), Error> {
        self.inner.client.call(&DiagResetCounters { group_mask }, CallOpts::default()).map(|_| ())
    }

    /// `SYS_GET_STATUS` (flags bit0 key export locked, bit1 paging, bit2
    /// PTA coexistence enabled, bit3 GP commissioning, bit4 permit join).
    pub fn ncp_status(&self) -> Result<SysGetStatusRsp, Error> {
        let rsp = self.inner.client.call_empty(ids::SYS_GET_STATUS, CallOpts::default())?;
        rsp.body()
    }

    /// `SEC_GET_FRAME_COUNTERS` (also kept in [`HubStatus::counters`](super::HubStatus)).
    pub fn frame_counters(&self) -> Result<SecGetFrameCountersRsp, Error> {
        let rsp = self.inner.client.call_empty(ids::SEC_GET_FRAME_COUNTERS, CallOpts::default())?;
        let fc: SecGetFrameCountersRsp = rsp.body()?;
        self.update(|s| s.counters = Some(fc.clone()));
        Ok(fc)
    }

    /// The coordinator's neighbour table (`DIAG_GET_NEIGHBORS`, all pages).
    pub fn coordinator_neighbors(&self) -> Result<Vec<Neighbor>, Error> {
        let mut out = Vec::new();
        for _ in 0..MAX_PAGES {
            let rsp = self.inner.client.call(&DiagGetNeighbors { start: out.len() as u8 }, CallOpts::default())?;
            let body: DiagGetNeighborsRsp = rsp.body()?;
            let n = body.entries.len();
            out.extend(body.entries.iter());
            if n == 0 || out.len() >= usize::from(body.total) {
                break;
            }
        }
        Ok(out)
    }

    /// The coordinator's routing table (`DIAG_GET_ROUTES`, all pages).
    pub fn coordinator_routes(&self) -> Result<Vec<Route>, Error> {
        let mut out = Vec::new();
        for _ in 0..MAX_PAGES {
            let rsp = self.inner.client.call(&DiagGetRoutes { start: out.len() as u8 }, CallOpts::default())?;
            let body: DiagGetRoutesRsp = rsp.body()?;
            let n = body.entries.len();
            out.extend(body.entries.iter());
            if n == 0 || out.len() >= usize::from(body.total) {
                break;
            }
        }
        Ok(out)
    }

    /// The coordinator's children (`DIAG_GET_CHILDREN`, all pages).
    pub fn coordinator_children(&self) -> Result<Vec<Child>, Error> {
        let mut out = Vec::new();
        for _ in 0..MAX_PAGES {
            let rsp = self.inner.client.call(&DiagGetChildren { start: out.len() as u8 }, CallOpts::default())?;
            let body: DiagGetChildrenRsp = rsp.body()?;
            let n = body.entries.len();
            out.extend(body.entries.iter());
            if n == 0 || out.len() >= usize::from(body.total) {
                break;
            }
        }
        Ok(out)
    }

    /// `BKP_EXPORT_*` (spec 9.5): the NCP's record stream (network, keys
    /// when `keys` and the NCP allows it, devices, `RS_PARAMS`), verified
    /// against its `END`. Needs the network up.
    pub fn backup_export(&self, keys: bool) -> Result<Vec<u8>, Error> {
        let c = &self.inner.client;
        let flags = 0x02 | u8::from(keys);
        let rsp = c.call(&BkpExportBegin { flags }, CallOpts::default())?;
        let begin: BkpExportBeginRsp = rsp.body()?;
        let mut stream = Vec::new();
        let result = (|| {
            loop {
                let rsp = c.call(&BkpExportRead { txn: begin.txn, max_bytes: EXPORT_CHUNK }, CallOpts::default())?;
                let more = *rsp.payload.get(1).ok_or(Error::Codec)?;
                stream.extend_from_slice(rsp.payload.get(2..).unwrap_or(&[]));
                if stream.len() > MAX_STREAM {
                    return Err(Error::Other("backup stream too large".into()));
                }
                if more == 0 {
                    return Ok(());
                }
            }
        })();
        let _ = c.call(&BkpExportEnd { txn: begin.txn }, CallOpts::default());
        result?;
        verify_stream(&stream).map_err(|e| Error::Other(format!("backup stream: {e:?}")))?;
        Ok(stream)
    }

    /// `BKP_RESTORE_*` (spec 9.4): stage `stream` (records; an `END` in it
    /// is replaced by one computed here), commit, start the network and
    /// adopt it into the store. The NCP's previous network, if any, is
    /// replaced at commit; the caller confirms that with the operator.
    pub fn backup_restore(&self, stream: &[u8], opts: RestoreOptions) -> Result<RestoreReport, Error> {
        let c = &self.inner.client;
        let mut tlvs: Vec<&[u8]> = Vec::new();
        let mut network: Option<zigbee_zrhp::records::NetworkRecord> = None;
        let mut key: Option<zigbee_zrhp::records::NetworkKeyRecord> = None;
        let mut node_ieee = None;
        for item in RawRecords::new(stream) {
            let (rtype, tlv) = item.map_err(|_| Error::Status(Status::InvalidParameter))?;
            if rtype == record_type::END {
                break;
            }
            match Reader::new(tlv).read::<Record>() {
                Ok(Record::Network(n)) => network = Some(n),
                Ok(Record::NetworkKey(k)) => key = Some(k),
                Ok(Record::Node(n)) => node_ieee = Some(n.ieee.0),
                _ => {}
            }
            tlvs.push(tlv);
        }
        let (Some(network), Some(key)) = (network, key) else {
            return Err(Error::Other("the backup has no network or no network key".into()));
        };
        let flags = u8::from(opts.allow_ieee_change) | 0x02;
        let begin = BkpRestoreBegin {
            flags,
            nwk_margin: opts.nwk_margin,
            aps_margin: opts.aps_margin,
            expected_records: tlvs.len() as u32,
        };
        let rsp = c.call(&begin, CallOpts::default())?;
        let b: BkpRestoreBeginRsp = rsp.body()?;
        let per_put = usize::from(b.max_records_per_put.max(1));
        let mut report = RestoreReport::default();
        let staged = (|| {
            let mut crc = StreamCrc::new();
            let mut i = 0;
            while i <= tlvs.len() {
                let mut payload = b.txn.to_le_bytes().to_vec();
                let mut n = 0;
                while i < tlvs.len() && n < per_put && (n == 0 || payload.len() + tlvs[i].len() <= RESTORE_CHUNK) {
                    payload.extend_from_slice(tlvs[i]);
                    crc.add(tlvs[i]);
                    i += 1;
                    n += 1;
                }
                let last = i == tlvs.len();
                if last && n < per_put {
                    let mut buf = [0u8; 16];
                    let mut w = Writer::new(&mut buf);
                    Record::End(crc.end()).write(&mut w).map_err(|_| Error::Codec)?;
                    payload.extend_from_slice(w.written());
                    i += 1;
                }
                let rsp = c.call_raw(ids::BKP_RESTORE_PUT, &payload, CallOpts::default())?.reply;
                if !rsp.status().is_success() {
                    return Err(Error::Status(rsp.status()));
                }
                let put: BkpRestorePutRsp = rsp.body()?;
                report.notes.extend(put.notes.iter().map(|n| (n.index, n.note)));
            }
            let opts = CallOpts::timeout(Duration::from_secs(60));
            let rsp = c.call(&BkpRestoreCommit { txn: b.txn }, opts)?;
            let commit: BkpRestoreCommitRsp = rsp.body()?;
            Ok::<_, Error>(commit)
        })();
        let commit = match staged {
            Ok(commit) => commit,
            Err(e) => {
                if status_of(&e) != Some(Status::NotFound) {
                    let _ = c.call(&BkpRestoreAbort { txn: b.txn }, CallOpts::default());
                }
                return Err(e);
            }
        };
        report.nwk_fc_start = commit.nwk_fc_start;
        report.aps_fc_start = commit.aps_fc_start;
        report.link_keys_on_ncp = commit.link_keys_on_ncp;
        report.link_keys_host_backed = commit.link_keys_host_backed;
        report.children = commit.children;

        // Adopt the restored network: identity, key, counters as floors.
        self.commit(Change::Network(NetworkRecord {
            pan_id: network.pan_id,
            ext_pan_id: network.ext_pan_id,
            channel: network.channel,
            channel_mask: network.channel_mask,
            network_key: key.key,
            key_seq: key.seq,
            nwk_update_id: network.nwk_update_id,
            tx_power: network.tx_power,
            nwk_manager: network.nwk_manager,
        }));
        let fc = self.inner.store.lock().unwrap().state().fc;
        for (domain, start) in [(1u8, commit.nwk_fc_start), (2, commit.aps_fc_start)] {
            let logged = fc[usize::from(domain) - 1];
            if logged.is_none_or(|f| f.limit < start) {
                self.commit(Change::Fc(domain, FcRecord { key_seq: key.seq, limit: start, generation: 0 }));
            }
        }
        let mut meta: Meta = self.inner.store.lock().unwrap().state().meta;
        let session_ieee = self.inner.client.session().and_then(|s| s.info.as_ref().map(|i| i.ieee_active));
        if let Some(ieee) = (if opts.allow_ieee_change { node_ieee } else { None }).or(session_ieee) {
            meta.ncp_ieee = ieee;
        }
        self.commit(Change::Meta(meta));
        let mut nwk = self.get_network_info(CallOpts::default())?;
        if nwk.state == 1 {
            let stride = self.inner.client.session().and_then(|s| s.negotiated.capacity(zigbee_zrhp::msg::system::capacity::FC_STRIDE));
            self.nwk_init(stride, 1)?;
            nwk = self.get_network_info(CallOpts::default())?;
        }
        self.update(|s| s.ownership = Ownership::Owned);
        info!(
            "hub: restored PAN 0x{:04X} on channel {} (state {}), {} children, {} keys on the NCP",
            nwk.pan_id, nwk.channel, nwk.state, report.children, report.link_keys_on_ncp
        );
        Ok(report)
    }

    /// `NWK_LEAVE_RESET` (keeping the IEEE override and the host
    /// endpoints): the NCP leaves and wipes its network; the store forgets
    /// it and the NCP's device records, so a new one can be formed. Device
    /// entries of the application are not touched.
    pub fn leave_reset(&self) -> Result<(), Error> {
        let opts = CallOpts::timeout(Duration::from_secs(30));
        self.inner.client.call(&NwkLeaveReset { flags: 0x03 }, opts)?;
        self.commit(Change::NetworkClear);
        let devices: Vec<[u8; 8]> = self.inner.store.lock().unwrap().state().devices.keys().copied().collect();
        for ieee in devices {
            self.commit(Change::DeviceDel(ieee));
        }
        *self.inner.permit_until.lock().unwrap() = None;
        self.update(|s| {
            s.network_state = 0;
            s.network = None;
            s.ownership = Ownership::NoNetwork;
            s.permit_join_s = 0;
        });
        info!("hub: the NCP left its network and wiped it");
        Ok(())
    }

    /// Capabilities of the current NCP that the maintenance API depends
    /// on, by name.
    pub fn maintenance_caps(&self) -> Vec<&'static str> {
        let c = self.caps();
        [
            (caps::ENERGY_SCAN, "energy_scan"),
            (caps::TX_POWER, "tx_power"),
            (caps::BACKUP_RESTORE, "backup_restore"),
            (caps::KEY_EXPORT, "key_export"),
            (caps::IEEE_REWRITABLE, "ieee_rewritable"),
            (caps::PTA_COEX, "pta_coex"),
            (caps::NCP_FW_UPDATE, "ncp_fw_update"),
        ]
        .into_iter()
        .filter(|(bit, _)| c & bit != 0)
        .map(|(_, n)| n)
        .collect()
    }
}

/// `END` of a record stream without one (for streams built by the host).
pub fn end_record(stream: &[u8]) -> EndRecord {
    let mut crc = StreamCrc::new();
    for (_, tlv) in RawRecords::new(stream).flatten() {
        crc.add(tlv);
    }
    crc.end()
}
