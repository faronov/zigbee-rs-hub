//! Devices imported from another coordinator's records (a Zigbee2MQTT or
//! ZHA migration, `docs/migration.md`): the device appears at once, with
//! the entities its definition gives the imported fingerprint, as if this
//! hub had interviewed it; nothing is sent to it until it is heard from.
//!
//! Verification, once per imported device, when it first talks (a report,
//! a poll, an announce) or answers a probe:
//!
//! - [`IMPORT_VERIFY`] (the coordinator IEEE did not change: the device's
//!   binds, reporting and IAS CIE address still name the hub): the
//!   configure of its definition is taken as done, and its identity
//!   (DateCode, SWBuildID) and the states the hub does not know yet are
//!   read.
//! - [`IMPORT_RECONFIGURE`] (another coordinator IEEE): the definition's
//!   configure runs as after a join, [`IMPORT_SLOTS`] devices at a time.
//! - Records of an interview the source never finished
//!   ([`ImportedDevice::incomplete`]): the device is interviewed.
//!
//! Sleepy devices are verified when they wake; mains devices not heard
//! from are probed (a Basic read; `Active_EP_req` for an unfinished
//! record or one without Basic) one every [`IMPORT_PROBE_MS`], so that an
//! import of a few hundred routers does not flood the network; a device
//! that answers none of [`IMPORT_PROBE_TRIES`] probes waits until it is
//! heard from.

use std::collections::BTreeMap;

use crate::configure::{build_ops, configure_hash};
use crate::device::{Device, DeviceKind, InterviewStep, Millis};
use crate::fingerprint::Fingerprint;
use crate::hub::{DevEnv, Hub, HubEvent, Purpose, zigbee_id};
use crate::persist::{AttrSnap, DeviceSnapshot, SNAPSHOT_VERSION, persistent_value};
use crate::resolve::LayerRef;
use crate::transport::ZigbeeTransport;
use crate::zcl::{ZclValue, cluster as cl};
use crate::zdo::{self, NodeDescriptor};

/// The coordinator IEEE is the one the device knows: verify only.
pub const IMPORT_VERIFY: u8 = 1;
/// Another coordinator IEEE: configure the device again.
pub const IMPORT_RECONFIGURE: u8 = 2;
/// Imported devices configured again at once.
pub const IMPORT_SLOTS: usize = 2;
/// Spacing of the probes of imported mains devices not heard from.
pub const IMPORT_PROBE_MS: Millis = 3_000;
/// A probed device that did not answer is probed again after this,
/// doubled after every further probe.
pub const IMPORT_PROBE_RETRY_MS: Millis = 600_000;
/// Probes of a device that does not answer, at most.
pub const IMPORT_PROBE_TRIES: u8 = 4;

/// One device of an import (the normalized document's `devices[]` entry,
/// `docs/migration.md`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportedDevice {
    /// IEEE.
    pub ieee: u64,
    /// Short address.
    pub nwk: u16,
    /// Name the user gave it at the source.
    pub name: Option<String>,
    /// What the source's interview found.
    pub fp: Fingerprint,
    /// Node descriptor, if the source kept it.
    pub node_desc: Option<NodeDescriptor>,
    /// Cached attribute values `(ep, cluster, attr, mfr, value)`; only those
    /// the hub keeps ([`persistent_value`]) are imported.
    pub attrs: Vec<(u8, u16, u16, Option<u16>, ZclValue)>,
    /// The source says its interview did not finish (`interviewed: false`).
    pub partial: bool,
}

impl ImportedDevice {
    /// The source's interview is unfinished: said so, or no endpoints, an
    /// endpoint without clusters (a simple descriptor never read) or no
    /// model (Basic never read). The device is interviewed when it is
    /// heard from instead of being taken as it is.
    pub fn incomplete(&self) -> bool {
        let fp = &self.fp;
        self.partial
            || fp.endpoints.is_empty()
            || fp.model_id.is_none()
            || fp
                .endpoints
                .iter()
                .any(|e| e.in_clusters.is_empty() && e.out_clusters.is_empty())
    }
}

/// What an import does with a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportAction {
    /// The hub does not know it: added.
    New,
    /// The hub has it from an earlier import not verified yet, or without a
    /// finished interview: replaced (its user settings stay).
    Replace,
    /// The hub interviewed it: kept as it is (its name is taken only when
    /// it has none).
    Keep,
}

impl ImportAction {
    /// Name in the API.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Replace => "replace",
            Self::Keep => "keep",
        }
    }
}

/// What the hub would make of an imported device (`network/import/preview`).
#[derive(Debug, Clone)]
pub struct ImportPreview {
    /// What the import does with it.
    pub action: ImportAction,
    /// The definition layers it resolves to, bottom to top.
    pub layers: Vec<LayerRef>,
    /// Display vendor and model.
    pub vendor: Option<String>,
    /// Display model.
    pub model: Option<String>,
    /// Entities.
    pub entities: usize,
}

/// The snapshot an imported device is restored from.
fn snapshot(d: &ImportedDevice, mode: u8) -> DeviceSnapshot {
    let fp = &d.fp;
    let basic_ep = fp
        .endpoints
        .iter()
        .find(|e| e.in_clusters.contains(&cl::BASIC))
        .map(|e| e.ep);
    let mut attrs: Vec<AttrSnap> = Vec::new();
    let mut put = |ep: u8, cluster: u16, attr: u16, mfr: Option<u16>, v: ZclValue| {
        if !persistent_value(cluster, attr) {
            return;
        }
        attrs.retain(|a| (a.0, a.1, a.2, a.3) != (ep, cluster, attr, mfr));
        attrs.push((ep, cluster, attr, mfr, true, Some(v)));
    };
    for &(ep, cluster, attr, mfr, ref v) in &d.attrs {
        put(ep, cluster, attr, mfr, v.clone());
    }
    if let Some(ep) = basic_ep {
        let text = |v: &Option<String>| v.clone().map(ZclValue::String);
        let uint = |v: Option<u32>| v.map(|v| ZclValue::Uint(u64::from(v)));
        for (attr, v) in [
            (0x0000, uint(fp.zcl_version)),
            (0x0001, uint(fp.app_version)),
            (0x0002, uint(fp.stack_version)),
            (0x0003, uint(fp.hw_version)),
            (0x0004, text(&fp.manufacturer_name)),
            (0x0005, text(&fp.model_id)),
            (0x0006, text(&fp.date_code)),
            (
                0x0007,
                fp.power_source.map(|p| ZclValue::Uint(u64::from(p))),
            ),
            (0x4000, text(&fp.sw_build_id)),
        ] {
            if let Some(v) = v {
                put(ep, cl::BASIC, attr, None, v);
            }
        }
    }
    DeviceSnapshot {
        v: SNAPSHOT_VERSION,
        id: zigbee_id(d.ieee),
        kind: DeviceKind::Zigbee {
            ieee: d.ieee,
            nwk: d.nwk,
        },
        fp: d.fp.clone(),
        node_desc: d.node_desc,
        active_eps: d.fp.endpoints.iter().map(|e| e.ep).collect(),
        interviewed: true,
        interview_partial: d.incomplete(),
        attrs,
        user_name: d.name.clone(),
        disabled: false,
        pinned: None,
        options: BTreeMap::new(),
        configure_hash: None,
        configure_done: BTreeMap::new(),
        unreportable: Vec::new(),
        warnings: Vec::new(),
        gp: None,
        schema: None,
        entities: Vec::new(),
        reporting: Vec::new(),
        imported: mode,
    }
}

impl<T: ZigbeeTransport> Hub<T> {
    /// What an import would do with the device `ieee`.
    pub fn import_action(&self, ieee: u64) -> ImportAction {
        match self.devices.get(&zigbee_id(ieee)) {
            None => ImportAction::New,
            Some(d)
                if d.imported != 0
                    || d.interview != InterviewStep::Done
                    || d.fp.endpoints.is_empty() =>
            {
                ImportAction::Replace
            }
            Some(_) => ImportAction::Keep,
        }
    }

    /// What the definitions make of an imported device (nothing changes).
    pub fn preview_import(&self, d: &ImportedDevice) -> ImportPreview {
        let action = self.import_action(d.ieee);
        let s = snapshot(d, IMPORT_VERIFY);
        let mut dev = Device::new(s.id.clone(), s.kind.clone(), 0);
        dev.fp = s.fp;
        for (ep, c, a, mfr, _, v) in s.attrs {
            dev.attrs.set(ep, c, a, mfr, v);
        }
        if action == ImportAction::Keep
            && let Some(own) = self.devices.get(&dev.id)
        {
            dev.fp = own.fp.clone();
            dev.attrs = own.attrs.clone();
            dev.pinned = own.pinned.clone();
        }
        let r = self.resolution_of(&dev);
        ImportPreview {
            action,
            vendor: r.vendor.clone().or(dev.fp.manufacturer_name.clone()),
            model: r.model.clone().or(dev.fp.model_id.clone()),
            entities: r.entities.len(),
            layers: r.layers,
        }
    }

    /// Adds (or replaces, see [`ImportAction`]) an imported device: it
    /// appears with its entities, its verification follows when it is
    /// heard from. `mode`: [`IMPORT_VERIFY`] or [`IMPORT_RECONFIGURE`].
    /// Importing the same device again changes nothing more.
    pub fn import_device(
        &mut self,
        d: &ImportedDevice,
        mode: u8,
        now: Millis,
    ) -> Result<ImportAction, String> {
        let id = zigbee_id(d.ieee);
        let action = self.import_action(d.ieee);
        if action == ImportAction::Keep {
            if let Some(dev) = self.devices.get_mut(&id)
                && dev.user_name.is_none()
                && d.name.is_some()
            {
                dev.user_name = d.name.clone();
                self.emit(HubEvent::DeviceUpdated(id));
            }
            return Ok(action);
        }
        let mut s = snapshot(d, mode);
        if let Some(old) = self.devices.get(&id) {
            // The user's settings of a device imported before stay.
            let mine = DeviceSnapshot::of(old);
            s.user_name = mine.user_name.or(s.user_name);
            s.disabled = mine.disabled;
            s.pinned = mine.pinned;
            s.options = mine.options;
            s.entities = mine.entities;
            s.reporting = mine.reporting;
        }
        self.restore_device(s, None, now)?;
        Ok(action)
    }

    /// Imported devices not verified yet.
    pub fn imported_pending(&self) -> usize {
        self.devices.values().filter(|d| d.imported != 0).count()
    }

    /// Imported devices being configured again; a sleeping one's requests
    /// wait for its wake and take no slot.
    fn import_configuring(&self, now: Millis) -> usize {
        self.devices
            .values()
            .filter(|d| {
                d.imported != 0
                    && d.interview == InterviewStep::Configure
                    && !(d.sleepy() && now > d.awake_until)
            })
            .count()
    }

    /// An imported device was heard from: its verification starts (once
    /// per run).
    pub(crate) fn import_contact(&mut self, id: &str, now: Millis) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        if d.imported == 0 || d.import_contacted || d.gp.is_some() {
            return;
        }
        d.import_contacted = true;
        if d.interview_partial || d.fp.endpoints.is_empty() {
            // The source never finished its interview.
            d.imported = 0;
            self.emit(HubEvent::DeviceUpdated(id.to_string()));
            let _ = self.reinterview(id, now);
            return;
        }
        if d.imported == IMPORT_RECONFIGURE {
            let sleepy = d.sleepy();
            if self.import_configuring(now) >= IMPORT_SLOTS {
                if sleepy {
                    // Asleep when a slot frees: its next wake tries again.
                    if let Some(d) = self.devices.get_mut(id) {
                        d.import_contacted = false;
                    }
                } else if !self.import_queue.iter().any(|q| q == id) {
                    self.import_queue.push_back(id.to_string());
                }
                return;
            }
            self.import_reconfigure(id, now);
            return;
        }
        // The configure the previous coordinator did stands.
        let (coord, hub_ep, tz) = (
            self.cfg.coordinator_ieee,
            self.cfg.hub_endpoint,
            self.cfg.tz_offset_s,
        );
        if let Some(d) = self.devices.get_mut(id) {
            let ops = build_ops(d, coord, hub_ep, &DevEnv { dev: d, now, tz });
            d.configure.hash = Some(configure_hash(&ops));
            for op in ops {
                d.configure.done.insert(op.key, true);
            }
            d.imported = 0;
        }
        self.emit(HubEvent::DeviceUpdated(id.to_string()));
        // A probed device's identity was just read.
        self.refresh_identity(id, false, now);
        self.refresh_unknown_of(id, now);
    }

    fn import_reconfigure(&mut self, id: &str, now: Millis) {
        self.refresh_identity(id, false, now);
        self.refresh_unknown_of(id, now);
        self.start_configure(id, now);
    }

    /// Every tick: queued reconfigures as slots free, and one probe of an
    /// imported mains device not heard from every [`IMPORT_PROBE_MS`].
    pub(crate) fn import_tick(&mut self, now: Millis) {
        while self.import_configuring(now) < IMPORT_SLOTS {
            let Some(id) = self.import_queue.pop_front() else {
                break;
            };
            let Some(d) = self.devices.get_mut(&id) else {
                continue;
            };
            if d.imported != IMPORT_RECONFIGURE || d.interview != InterviewStep::Done {
                continue;
            }
            if d.sleepy() && now > d.awake_until {
                d.import_contacted = false;
                continue;
            }
            self.import_reconfigure(&id, now);
        }
        if !self.import_queue.is_empty()
            || self.import_configuring(now) >= IMPORT_SLOTS
            || self
                .import_probe_last
                .is_some_and(|t| now.saturating_sub(t) < IMPORT_PROBE_MS)
        {
            return;
        }
        let next = self
            .devices
            .values()
            .find(|d| {
                d.imported != 0
                    && !d.import_contacted
                    && d.gp.is_none()
                    && !d.sleepy()
                    && d.available
                    && d.import_probes < IMPORT_PROBE_TRIES
                    && d.import_probe_at.is_none_or(|t| {
                        now.saturating_sub(t)
                            >= IMPORT_PROBE_RETRY_MS << d.import_probes.saturating_sub(1)
                    })
            })
            .map(|d| d.id.clone());
        let Some(id) = next else {
            return;
        };
        self.import_probe_last = Some(now);
        let Some(d) = self.devices.get_mut(&id) else {
            return;
        };
        d.import_probe_at = Some(now);
        d.import_probes += 1;
        // Its answer is the contact that verifies it (or starts the
        // interview of an unfinished record).
        let basic = !d.interview_partial
            && d.fp
                .endpoints
                .iter()
                .any(|e| e.in_clusters.contains(&cl::BASIC));
        let nwk = d.nwk().unwrap_or(0xFFFE);
        if basic {
            self.refresh_identity(&id, true, now);
        } else {
            let tsn = self.next_zdo_tsn();
            self.send_zdo(
                &id,
                zdo::cluster::ACTIVE_EP_REQ,
                zdo::nwk_req(tsn, nwk),
                Purpose::None,
                now,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::EndpointFp;
    use crate::hub::HubConfig;
    use crate::resolve::MemoryCatalog;
    use crate::transport::MockTransport;
    use crate::zdo::LogicalType;

    fn plug(ieee: u64) -> ImportedDevice {
        ImportedDevice {
            ieee,
            nwk: 0x1234,
            name: Some("Desk lamp".into()),
            fp: Fingerprint {
                manufacturer_name: Some("ACME".into()),
                model_id: Some("PLUG-1".into()),
                mfr_code: Some(0x1234),
                logical_type: Some(LogicalType::Router),
                power_source: Some(1),
                rx_on_when_idle: Some(true),
                mac_mains: Some(true),
                endpoints: vec![EndpointFp {
                    ep: 1,
                    profile: 0x0104,
                    device_id: 0x0051,
                    in_clusters: vec![0x0000, 0x0003, 0x0006],
                    out_clusters: vec![],
                }],
                ..Fingerprint::default()
            },
            ..ImportedDevice::default()
        }
    }

    fn hub() -> Hub<MockTransport> {
        Hub::new(
            MockTransport::default(),
            HubConfig::default(),
            Box::new(MemoryCatalog::default()),
        )
    }

    #[test]
    fn the_snapshot_carries_basic_and_kept_attributes_only() {
        let mut d = plug(1);
        d.attrs = vec![
            (1, 0x0300, 0x400A, None, ZclValue::Uint(0x1F)),
            (1, 0x0006, 0x0000, None, ZclValue::Bool(Some(true))),
        ];
        let s = snapshot(&d, IMPORT_RECONFIGURE);
        assert_eq!(s.imported, IMPORT_RECONFIGURE);
        assert!(s.interviewed && !s.interview_partial);
        assert_eq!(s.active_eps, vec![1]);
        let has = |c: u16, a: u16| s.attrs.iter().any(|x| x.1 == c && x.2 == a);
        assert!(has(0x0300, 0x400A), "colour capabilities are kept");
        assert!(!has(0x0006, 0x0000), "a measured state is not");
        assert!(has(0x0000, 0x0005) && has(0x0000, 0x0007));
        let back = DeviceSnapshot::from_bytes(&s.to_bytes()).unwrap();
        assert_eq!(back.imported, IMPORT_RECONFIGURE);
        let plain = DeviceSnapshot {
            imported: 0,
            ..back
        };
        assert!(
            !String::from_utf8(plain.to_bytes())
                .unwrap()
                .contains("imported"),
            "absent when 0"
        );
    }

    #[test]
    fn actions_follow_what_the_hub_knows() {
        let mut h = hub();
        assert_eq!(h.import_action(1), ImportAction::New);
        assert_eq!(
            h.import_device(&plug(1), IMPORT_VERIFY, 0),
            Ok(ImportAction::New)
        );
        assert_eq!(
            h.import_action(1),
            ImportAction::Replace,
            "imported, not verified yet"
        );
        h.devices.get_mut("zb:0000000000000001").unwrap().imported = 0;
        assert_eq!(h.import_action(1), ImportAction::Keep);
        // Kept: only a missing name is taken.
        h.devices.get_mut("zb:0000000000000001").unwrap().user_name = None;
        assert_eq!(
            h.import_device(&plug(1), IMPORT_VERIFY, 0),
            Ok(ImportAction::Keep)
        );
        assert_eq!(
            h.device("zb:0000000000000001")
                .unwrap()
                .user_name
                .as_deref(),
            Some("Desk lamp")
        );
        assert_eq!(h.device("zb:0000000000000001").unwrap().imported, 0);
        // Mid-interview (the NCP's inventory made it): replaced.
        h.on_device_joined(2, 0x2222, 0);
        assert_eq!(h.import_action(2), ImportAction::Replace);
    }

    #[test]
    fn importing_again_keeps_the_users_changes() {
        let mut h = hub();
        h.import_device(&plug(1), IMPORT_VERIFY, 0).unwrap();
        let id = "zb:0000000000000001";
        h.devices.get_mut(id).unwrap().user_name = Some("Renamed".into());
        let before = h.device(id).unwrap().entities.len();
        assert_eq!(
            h.import_device(&plug(1), IMPORT_VERIFY, 10),
            Ok(ImportAction::Replace)
        );
        let d = h.device(id).unwrap();
        assert_eq!(d.user_name.as_deref(), Some("Renamed"));
        assert_eq!(d.entities.len(), before);
        assert_eq!(d.record()["interview"], "imported");
    }
}
