//! Device registry persistence: what of a [`Device`] survives a hub
//! restart, as a serde record, and the engine side of saving/restoring it.
//!
//! Kept: identity and kind, the fingerprint, node descriptor and endpoints,
//! attribute support, the attribute values the resolution reads (Basic,
//! descriptions, colour capabilities, thermostat limits, metering and
//! electrical scaling: [`persistent_value`]), the self-description fragment
//! (as zdef CBOR, decoded again on restore), user settings (name, disabled,
//! pinned definition, hub options, entity name/enable overrides,
//! reporting overrides), the
//! configure hash and results (so a restart does not reconfigure) and the GP
//! sink record.
//!
//! Not kept, so that a snapshot only changes when the device's structure or
//! settings do (no flash write per report): entity states and measured
//! attribute values (they come back with the next report or refresh),
//! LQI/RSSI, frame counters, the Tuya sequence, runtime queues, orphaned
//! entities, diagnostics. A restored device starts available with
//! `last_seen` = restore time; its availability timers run from there.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value as J;

use crate::device::{Device, DeviceKind, GpState, InterviewStep, Millis};
use crate::expr::Value;
use crate::fingerprint::Fingerprint;
use crate::generic::Support;
use crate::gp::types::GpSinkRecord;
use crate::hub::{Hub, HubEvent, SchemaDecoder};
use crate::model::LayerSource;
use crate::transport::ZigbeeTransport;
use crate::zcl::ZclValue;
use crate::zdo::NodeDescriptor;

/// Format version of [`DeviceSnapshot`].
pub const SNAPSHOT_VERSION: u8 = 1;

/// Attribute values a snapshot keeps (the ones resolution and the generic
/// tier read); everything else keeps only its support flag.
pub fn persistent_value(cluster: u16, attr: u16) -> bool {
    match cluster {
        0x0000 => true,
        0x0001 => (0x0031..=0x0036).contains(&attr),
        0x0201 => matches!(attr, 0x0015 | 0x0016 | 0x001B),
        0x0202 => attr == 0x0001,
        0x0300 => (0x400A..=0x400C).contains(&attr),
        0x0702 => (0x0300..=0x0306).contains(&attr),
        0x0B04 => (0x0600..=0x0605).contains(&attr),
        _ => attr == 0x001C,
    }
}

/// One cached attribute: `(ep, cluster, attr, mfr, supported, value)`.
pub type AttrSnap = (u8, u16, u16, Option<u16>, bool, Option<ZclValue>);

/// User overrides of one entity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntitySnap {
    /// uid.
    pub uid: String,
    /// Name override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Enable override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// The reporting override of one attribute (device-support 5.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportSnap {
    /// Endpoint.
    pub ep: u8,
    /// Cluster.
    pub cluster: u16,
    /// Attribute.
    pub attr: u16,
    /// Manufacturer code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mfr: Option<u16>,
    /// Setting (`[min, max, change]`, `"off"`, `"revert"`).
    pub report: crate::reporting::ReportSetting,
}

/// GP specifics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpSnap {
    /// Sink record.
    pub sink: Option<GpSinkRecord>,
    /// Security level of the last frame.
    pub security_level: u8,
}

/// Self-description fragment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchemaSnap {
    /// 8-byte schema hash (Device Schema attribute 1).
    pub hash: Vec<u8>,
    /// zdef CBOR of the decoded fragment.
    pub cbor: Vec<u8>,
}

/// The persistent part of a device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceSnapshot {
    /// [`SNAPSHOT_VERSION`].
    pub v: u8,
    /// `zb:<ieee>` / `gp:<srcid>`.
    pub id: String,
    /// Kind (IEEE + NWK, or GPD ID).
    pub kind: DeviceKind,
    /// Fingerprint.
    pub fp: Fingerprint,
    /// Node descriptor.
    pub node_desc: Option<NodeDescriptor>,
    /// Active endpoints.
    pub active_eps: Vec<u8>,
    /// The interview finished (`Done`); otherwise it restarts on restore.
    pub interviewed: bool,
    /// Some interview step could not complete.
    pub interview_partial: bool,
    /// Attribute knowledge.
    pub attrs: Vec<AttrSnap>,
    /// User name.
    pub user_name: Option<String>,
    /// Disabled by the user.
    pub disabled: bool,
    /// Pinned definition.
    pub pinned: Option<String>,
    /// Hub option values (JSON).
    pub options: BTreeMap<String, J>,
    /// Configure hash.
    pub configure_hash: Option<String>,
    /// Configure results by op key.
    pub configure_done: BTreeMap<String, bool>,
    /// Failed reporting configs.
    pub unreportable: Vec<(u8, u16, u16)>,
    /// Warnings.
    pub warnings: Vec<String>,
    /// GP specifics.
    pub gp: Option<GpSnap>,
    /// Self-description.
    pub schema: Option<SchemaSnap>,
    /// Entity user overrides.
    pub entities: Vec<EntitySnap>,
    /// Reporting overrides; absent without any (and in snapshots of
    /// earlier builds, which ignore the field).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reporting: Vec<ReportSnap>,
}

impl DeviceSnapshot {
    /// The persistent part of `d`.
    pub fn of(d: &Device) -> Self {
        let attrs = d
            .attrs
            .map
            .iter()
            .filter(|(_, e)| e.support != Support::Unknown)
            .map(|(&(ep, cl, at, mfr), e)| {
                let value = if persistent_value(cl, at) {
                    e.value.clone()
                } else {
                    None
                };
                (ep, cl, at, mfr, e.support == Support::Supported, value)
            })
            .collect();
        let schema = match (&d.schema.hash, &d.schema.fragment) {
            (Some(hash), Some(frag)) => {
                frag.zdef
                    .as_ref()
                    .and_then(|z| z.encode().ok())
                    .map(|cbor| SchemaSnap {
                        hash: hash.clone(),
                        cbor,
                    })
            }
            _ => None,
        };
        Self {
            v: SNAPSHOT_VERSION,
            id: d.id.clone(),
            kind: d.kind.clone(),
            fp: d.fp.clone(),
            node_desc: d.node_desc,
            active_eps: d.active_eps.clone(),
            interviewed: d.interview == InterviewStep::Done,
            interview_partial: d.interview_partial,
            attrs,
            user_name: d.user_name.clone(),
            disabled: d.disabled,
            pinned: d.pinned.clone(),
            options: d
                .options
                .iter()
                .map(|(k, v)| (k.clone(), v.to_json()))
                .collect(),
            configure_hash: d.configure.hash.clone(),
            configure_done: d.configure.done.clone(),
            unreportable: d.configure.unreportable.clone(),
            warnings: d.warnings.clone(),
            gp: d.gp.as_ref().map(|g| GpSnap {
                sink: g.sink.clone(),
                security_level: g.security_level,
            }),
            schema,
            entities: d
                .entities
                .iter()
                .filter(|e| !e.orphaned && (e.user_name.is_some() || e.user_enabled.is_some()))
                .map(|e| EntitySnap {
                    uid: e.uid.clone(),
                    name: e.user_name.clone(),
                    enabled: e.user_enabled,
                })
                .collect(),
            reporting: d
                .report_overrides
                .iter()
                .map(|(&(ep, cluster, attr, mfr), &report)| ReportSnap {
                    ep,
                    cluster,
                    attr,
                    mfr,
                    report,
                })
                .collect(),
        }
    }

    /// JSON bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("snapshot serializes")
    }

    /// Parse JSON bytes; refuses unknown format versions.
    pub fn from_bytes(b: &[u8]) -> Result<Self, String> {
        let s: Self = serde_json::from_slice(b).map_err(|e| e.to_string())?;
        if s.v != SNAPSHOT_VERSION {
            return Err(format!("device snapshot version {}", s.v));
        }
        Ok(s)
    }
}

impl<T: ZigbeeTransport> Hub<T> {
    /// The persistent part of device `id`.
    pub fn snapshot_device(&self, id: &str) -> Option<DeviceSnapshot> {
        self.devices.get(id).map(DeviceSnapshot::of)
    }

    /// Re-creates a device from its snapshot (hub start): resolves it with
    /// the current definitions, re-applies entity overrides and, when the
    /// interview had not finished, restarts it. Emits the usual
    /// `device_added`/`entity_added` events.
    pub fn restore_device(
        &mut self,
        s: DeviceSnapshot,
        decoder: Option<&dyn SchemaDecoder>,
        now: Millis,
    ) -> Result<(), String> {
        if self.devices.contains_key(&s.id) {
            return Err(format!("device {} exists", s.id));
        }
        let mut d = Device::new(s.id.clone(), s.kind.clone(), now);
        // Not heard from yet: a sleepy device's requests wait for it.
        d.awake_until = 0;
        d.fp = s.fp;
        d.node_desc = s.node_desc;
        d.active_eps = s.active_eps;
        d.interview = if s.interviewed {
            InterviewStep::Done
        } else {
            InterviewStep::NodeDesc
        };
        d.interview_partial = s.interview_partial;
        for (ep, cl, at, mfr, supported, value) in s.attrs {
            let support = if supported {
                Support::Supported
            } else {
                Support::Unsupported
            };
            d.attrs.map.insert(
                (ep, cl, at, mfr),
                crate::device::AttrEntry { support, value },
            );
        }
        d.user_name = s.user_name;
        d.disabled = s.disabled;
        d.report_overrides = s
            .reporting
            .into_iter()
            .map(|r| ((r.ep, r.cluster, r.attr, r.mfr), r.report))
            .collect();
        d.pinned = s.pinned;
        d.options = s
            .options
            .iter()
            .map(|(k, v)| (k.clone(), Value::from_json(v)))
            .collect();
        d.configure.hash = s.configure_hash;
        d.configure.done = s.configure_done;
        d.configure.unreportable = s.unreportable;
        d.warnings = s.warnings;
        d.gp = s.gp.map(|g| GpState {
            frame_counter: g.sink.as_ref().map_or(0, |s| s.frame_counter),
            sink: g.sink,
            security_level: g.security_level,
            contacts: 0,
            recent: Default::default(),
        });
        if let (Some(schema), Some(dec)) = (s.schema, decoder) {
            match dec.decode(&schema.cbor) {
                Ok(mut frag) => {
                    use sha2::Digest;
                    frag.source = LayerSource::SelfDescription;
                    frag.content_hash = Some(crate::expr::hex(&sha2::Sha256::digest(&schema.cbor)));
                    let frag = Arc::new(frag);
                    self.schema_cache.insert(schema.hash.clone(), frag.clone());
                    d.schema.hash = Some(schema.hash);
                    d.schema.fragment = Some(frag);
                }
                Err(e) => d.schema.error = Some(format!("restore: {e}")),
            }
        }
        let id = s.id.clone();
        let nwk = d.nwk();
        let interviewed = s.interviewed;
        let is_zigbee = matches!(d.kind, DeviceKind::Zigbee { .. });
        self.devices.insert(id.clone(), d);
        if let Some(nwk) = nwk {
            self.index_nwk(nwk, &id);
        }
        self.emit(HubEvent::DeviceAdded(id.clone()));
        self.resolve_device(&id, now);
        if let Some(d) = self.devices.get_mut(&id) {
            for o in s.entities {
                if let Some(e) = d.entities.iter_mut().find(|e| e.uid == o.uid) {
                    e.user_name = o.name;
                    e.user_enabled = o.enabled;
                }
            }
        }
        if is_zigbee && !interviewed {
            let _ = self.reinterview(&id, now);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips_through_json() {
        let mut d = Device::new(
            "zb:00124b0001020304".into(),
            DeviceKind::Zigbee {
                ieee: 0x00124b0001020304,
                nwk: 0x1234,
            },
            5,
        );
        d.attrs.set(
            1,
            0x0000,
            0x0005,
            None,
            Some(ZclValue::String("lumi.sensor".into())),
        );
        d.attrs.set(1, 0x0402, 0x0000, None, None);
        d.user_name = Some("Kitchen".into());
        d.options.insert("offset".into(), Value::Float(0.5));
        d.configure.hash = Some("abc".into());
        d.configure.done.insert("bind:1:0402".into(), true);
        let s = DeviceSnapshot::of(&d);
        let back = DeviceSnapshot::from_bytes(&s.to_bytes()).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.attrs.len(), 2);
        assert!(
            back.attrs
                .iter()
                .any(|a| a.2 == 0x0005 && a.4 && a.5.is_some())
        );
        // Measured values are not kept: a report does not change the snapshot.
        d.attrs
            .set(1, 0x0402, 0x0000, None, Some(ZclValue::Int(2137)));
        let with_value = DeviceSnapshot::of(&d);
        d.attrs
            .set(1, 0x0402, 0x0000, None, Some(ZclValue::Int(2250)));
        assert_eq!(DeviceSnapshot::of(&d), with_value);
    }
}
