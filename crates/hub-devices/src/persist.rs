//! Device registry persistence: what of a [`Device`] survives a hub
//! restart, as a serde record, and the engine side of saving/restoring it.
//!
//! Kept: identity and kind, the fingerprint, node descriptor and endpoints,
//! the attribute knowledge the resolution depends on, the self-description
//! fragment (as zdef CBOR, decoded again on restore), user settings (name,
//! disabled, pinned definition, hub options, entity name/enable overrides),
//! the configure hash and results (so a restart does not reconfigure), GP
//! sink record and frame counter, radio quality and the Tuya sequence.
//!
//! Not kept: entity states and runtime queues (states come back with the
//! next report or refresh), orphaned entities, diagnostics. A restored
//! device starts available with `last_seen` = restore time; its
//! availability timers run from there.

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

/// GP specifics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpSnap {
    /// Sink record.
    pub sink: Option<GpSinkRecord>,
    /// Last accepted frame counter.
    pub frame_counter: u32,
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
    /// LQI.
    pub lqi: Option<u8>,
    /// RSSI.
    pub rssi: Option<i8>,
    /// Tuya sequence number.
    pub tuya_seq: u16,
}

impl DeviceSnapshot {
    /// The persistent part of `d`.
    pub fn of(d: &Device) -> Self {
        let attrs = d
            .attrs
            .map
            .iter()
            .filter(|(_, e)| e.support != Support::Unknown)
            .map(|(&(ep, cl, at, mfr), e)| (ep, cl, at, mfr, e.support == Support::Supported, e.value.clone()))
            .collect();
        let schema = match (&d.schema.hash, &d.schema.fragment) {
            (Some(hash), Some(frag)) => frag.zdef.as_ref().and_then(|z| z.encode().ok()).map(|cbor| SchemaSnap {
                hash: hash.clone(),
                cbor,
            }),
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
            options: d.options.iter().map(|(k, v)| (k.clone(), v.to_json())).collect(),
            configure_hash: d.configure.hash.clone(),
            configure_done: d.configure.done.clone(),
            unreportable: d.configure.unreportable.clone(),
            warnings: d.warnings.clone(),
            gp: d.gp.as_ref().map(|g| GpSnap {
                sink: g.sink.clone(),
                frame_counter: g.frame_counter,
                security_level: g.security_level,
            }),
            schema,
            entities: d
                .entities
                .iter()
                .filter(|e| !e.orphaned && (e.user_name.is_some() || e.user_enabled.is_some()))
                .map(|e| EntitySnap { uid: e.uid.clone(), name: e.user_name.clone(), enabled: e.user_enabled })
                .collect(),
            lqi: d.lqi,
            rssi: d.rssi,
            tuya_seq: d.tuya_seq,
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
    pub fn restore_device(&mut self, s: DeviceSnapshot, decoder: Option<&dyn SchemaDecoder>, now: Millis) -> Result<(), String> {
        if self.devices.contains_key(&s.id) {
            return Err(format!("device {} exists", s.id));
        }
        let mut d = Device::new(s.id.clone(), s.kind.clone(), now);
        d.fp = s.fp;
        d.node_desc = s.node_desc;
        d.active_eps = s.active_eps;
        d.interview = if s.interviewed { InterviewStep::Done } else { InterviewStep::NodeDesc };
        d.interview_partial = s.interview_partial;
        for (ep, cl, at, mfr, supported, value) in s.attrs {
            let support = if supported { Support::Supported } else { Support::Unsupported };
            d.attrs.map.insert((ep, cl, at, mfr), crate::device::AttrEntry { support, value });
        }
        d.user_name = s.user_name;
        d.disabled = s.disabled;
        d.pinned = s.pinned;
        d.options = s.options.iter().map(|(k, v)| (k.clone(), Value::from_json(v))).collect();
        d.configure.hash = s.configure_hash;
        d.configure.done = s.configure_done;
        d.configure.unreportable = s.unreportable;
        d.warnings = s.warnings;
        d.gp = s.gp.map(|g| GpState {
            sink: g.sink,
            frame_counter: g.frame_counter,
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
        d.lqi = s.lqi;
        d.rssi = s.rssi;
        d.tuya_seq = s.tuya_seq;
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
        let mut d = Device::new("zb:00124b0001020304".into(), DeviceKind::Zigbee { ieee: 0x00124b0001020304, nwk: 0x1234 }, 5);
        d.attrs.set(1, 0x0000, 0x0005, None, Some(ZclValue::String("lumi.sensor".into())));
        d.attrs.set(1, 0x0402, 0x0000, None, None);
        d.user_name = Some("Kitchen".into());
        d.options.insert("offset".into(), Value::Float(0.5));
        d.configure.hash = Some("abc".into());
        d.configure.done.insert("bind:1:0402".into(), true);
        let s = DeviceSnapshot::of(&d);
        let back = DeviceSnapshot::from_bytes(&s.to_bytes()).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.attrs.len(), 2);
        assert!(back.attrs.iter().any(|a| a.2 == 0x0005 && a.4));
    }
}
