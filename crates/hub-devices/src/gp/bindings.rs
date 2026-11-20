//! GP bindings (spec 13.8, D23): a GPD command bound to a ZCL command on a
//! device endpoint or a group, executed either on the target itself
//! (Translation Table programmed through ZRHP remote GP provisioning,
//! 0x0A08–0x0A0D) or by the hub (virtual binding on `EVT_GP_FRAME`).

use serde_json::{Map, Value as J, json};

use super::types::*;
use crate::device::{DeviceKind, Millis, ts};
use crate::hub::{ApiError, Hub, HubEvent, Purpose};
use crate::transport::{ApsDst, ApsRequest, TxOptions, ZigbeeTransport};
use crate::zcl::frame::{ReadRecord, Status};
use crate::zcl::{
    Direction, FrameType, PROFILE_GP, PROFILE_HA, ZclFrame, ZclHeader, ZclValue, cluster as cl,
    frame as zf,
};

/// Remote request timeout (13.8: 3 s after the APS confirm; the engine
/// has no confirm hook yet, so a slightly longer budget from the send).
pub const REMOTE_TIMEOUT_MS: Millis = 5_000;
/// Retries of a remote request.
pub const REMOTE_RETRIES: u8 = 2;
/// Translations per GP Translation Table Update.
pub const MAX_TRANSLATIONS_PER_UPDATE: usize = 7;

/// What triggers a binding.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BindTrigger {
    /// GPD command.
    pub command_id: u8,
    /// Contact bitmask (0x69/0x6A generic switches).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_mask: Option<u8>,
    /// Required contact bits under the mask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_value: Option<u8>,
}

/// Binding target.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum BindTarget {
    /// A device endpoint.
    Device {
        /// Device ID (`zb:…`).
        device: String,
        /// Endpoint.
        endpoint: u8,
    },
    /// A group; `members` are the group's device endpoints the hub knows
    /// (needed for on-device execution).
    Group {
        /// Group.
        group: u16,
        /// Members `(device, endpoint)`.
        #[serde(default)]
        members: Vec<(String, u8)>,
    },
}

/// Binding action (client → server ZCL command).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BindAction {
    /// Profile (0x0104).
    #[serde(default = "ha")]
    pub profile: u16,
    /// Cluster.
    pub cluster: u16,
    /// Command.
    pub command: u8,
    /// Payload: fixed (≤ 8 B), derived or copied.
    pub payload: TranslationPayload,
}

fn ha() -> u16 {
    PROFILE_HA
}

/// Executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Executor {
    /// The target's Translation Table.
    Device,
    /// The hub.
    Hub,
}

/// Executor requested by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorChoice {
    /// On-device when every target qualifies, else hub.
    #[default]
    Auto,
    /// Force on-device.
    Device,
    /// Force hub.
    Hub,
}

/// Binding state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum BindState {
    /// Being decided or programmed.
    Pending,
    /// Executing.
    Active,
    /// A target differs from the binding; being repaired.
    Drift,
    /// Being removed from the target.
    Removing,
    /// Failed.
    Failed {
        /// ZCL status of the target, if any.
        zcl_status: Option<String>,
        /// Reason.
        reason: String,
    },
    /// `device` forced on a target that does not qualify.
    Unsupported,
}

/// Programming step of one target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetStep {
    /// Waiting for the capability probe.
    Probe,
    /// GP Pairing Configuration sent.
    Pairing,
    /// GP Translation Table Update sent.
    Translation,
    /// GP Translation Table Request sent.
    Verify,
    /// Programmed and verified.
    Done,
    /// Removal: Translation Table Update (remove) sent.
    RemoveTranslation,
    /// Removal: Pairing Configuration (remove pairing) sent.
    RemovePairing,
    /// Removed.
    Removed,
}

/// Per-target state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TargetState {
    /// Device ID.
    pub device: String,
    /// Endpoint.
    pub endpoint: u8,
    /// Step.
    pub step: TargetStep,
    /// Last verification, Unix ms.
    pub last_verified: Option<Millis>,
    /// Last error.
    pub error: Option<String>,
}

/// One binding (persisted in `hubdata`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GpBindingRecord {
    /// ID.
    pub id: u32,
    /// GPD.
    pub gpd: GpdId,
    /// Trigger.
    pub trigger: BindTrigger,
    /// Target.
    pub target: BindTarget,
    /// Action.
    pub action: BindAction,
    /// Requested executor.
    pub choice: ExecutorChoice,
    /// Chosen executor (`None` while deciding).
    pub executor: Option<Executor>,
    /// State.
    #[serde(flatten)]
    pub state: BindState,
    /// On-device targets.
    pub targets: Vec<TargetState>,
    /// Executor to switch to once the old one is removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_choice: Option<ExecutorChoice>,
}

impl GpBindingRecord {
    /// API representation.
    pub fn to_json(&self) -> J {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        if let Some(o) = v.as_object_mut() {
            o.insert("gpd".into(), self.gpd.device_id().into());
        }
        v
    }

    fn translation(&self, endpoint: u8) -> GpTranslation {
        GpTranslation {
            gpd_command: self.trigger.command_id,
            endpoint,
            profile: self.action.profile,
            cluster: self.action.cluster,
            zcl_command: self.action.command,
            payload: self.action.payload.clone(),
        }
    }

    fn matches(&self, cmd: u8, payload: &[u8]) -> bool {
        if self.trigger.command_id != cmd {
            return false;
        }
        match self.trigger.contact_mask {
            Some(m) => payload
                .first()
                .is_some_and(|b| b & m == self.trigger.contact_value.unwrap_or(m) & m),
            None => true,
        }
    }
}

/// Probed capabilities of a target (`gp/targets/probe`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TargetCaps {
    /// gpsFunctionality bit 11 (CT-based commissioning) and a working
    /// sink (Sink Table Request answered SUCCESS / NOT_FOUND).
    pub sink: bool,
    /// gpsFunctionality bit 18.
    pub translation_table: bool,
    /// Accepted GPD security levels (bits 13/14/15/16).
    pub security_levels: Vec<u8>,
    /// gpsMaxSinkTableEntries.
    pub max_sink_entries: Option<u8>,
    /// Raw gpsFunctionality.
    pub functionality: Option<u32>,
    /// Probe finished.
    pub probed: bool,
}

impl TargetCaps {
    /// Whether this target can execute a binding of a GPD at `level`.
    pub fn qualifies(&self, level: u8) -> bool {
        self.sink && self.translation_table && self.security_levels.contains(&level)
    }
}

/// An outstanding remote GP request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteWait {
    /// Binding (none for probes).
    pub binding: Option<u32>,
    /// Target device ID.
    pub target: String,
    /// Target NWK.
    pub nwk: u16,
    /// Step.
    pub step: TargetStep,
    /// Sent at.
    pub sent_at: Millis,
    /// Retries so far.
    pub retries: u8,
    /// The request (for retries).
    pub req: GpRequest,
}

/// `gp/bindings/create` parameters.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CreateBinding {
    /// GPD device ID (`gp:…`).
    pub gpd: String,
    /// Trigger.
    pub trigger: BindTrigger,
    /// Target.
    pub target: BindTarget,
    /// Action.
    pub action: BindAction,
    /// Executor.
    #[serde(default)]
    pub executor: ExecutorChoice,
}

fn level_bit(level: u8) -> u32 {
    match level {
        0 => 13,
        1 => 14,
        2 => 15,
        _ => 16,
    }
}

impl<T: ZigbeeTransport> Hub<T> {
    fn binding_event(&mut self, id: u32) {
        let Some(b) = self.gp.bindings.iter().find(|b| b.id == id) else {
            return;
        };
        let mut body = b.to_json();
        if let (
            BindState::Failed {
                zcl_status: Some(s),
                ..
            },
            Some(o),
        ) = (&b.state, body.as_object_mut())
        {
            o.insert("zcl_status".into(), s.clone().into());
        }
        self.emit(HubEvent::Gp {
            kind: "gp/binding".into(),
            body,
        });
    }

    fn binding_mut(&mut self, id: u32) -> Option<&mut GpBindingRecord> {
        self.gp.bindings.iter_mut().find(|b| b.id == id)
    }

    /// Target device endpoints of a binding.
    fn target_endpoints(target: &BindTarget) -> Vec<(String, u8)> {
        match target {
            BindTarget::Device { device, endpoint } => vec![(device.clone(), *endpoint)],
            BindTarget::Group { members, .. } => members.clone(),
        }
    }

    fn nwk_of(&self, device: &str) -> Option<u16> {
        self.devices.get(device).and_then(|d| d.nwk())
    }

    /// `gp/bindings/create`.
    pub fn create_binding(&mut self, req: CreateBinding, now: Millis) -> Result<J, ApiError> {
        let gpd = match self.devices.get(&req.gpd).map(|d| &d.kind) {
            Some(DeviceKind::Gp { gpd_id }) => *gpd_id,
            _ => return Err(ApiError::new("not_found", format!("no GPD {}", req.gpd))),
        };
        for (dev, _) in Self::target_endpoints(&req.target) {
            if self.nwk_of(&dev).is_none() {
                return Err(ApiError::new(
                    "not_found",
                    format!("no Zigbee device {dev}"),
                ));
            }
        }
        if let TranslationPayload::Fixed(b) = &req.action.payload {
            if b.len() > 8 {
                return Err(ApiError::new(
                    "invalid_value",
                    "fixed payload must be ≤ 8 bytes",
                ));
            }
        }
        self.gp.next_id += 1;
        let id = self.gp.next_id;
        let rec = GpBindingRecord {
            id,
            gpd,
            trigger: req.trigger,
            target: req.target,
            action: req.action,
            choice: req.executor,
            executor: None,
            state: BindState::Pending,
            targets: Vec::new(),
            next_choice: None,
        };
        self.gp.bindings.push(rec);
        self.decide_binding(id, now);
        Ok(self
            .gp
            .bindings
            .iter()
            .find(|b| b.id == id)
            .map(GpBindingRecord::to_json)
            .unwrap_or_default())
    }

    /// Chooses the executor once all target capabilities are known.
    fn decide_binding(&mut self, id: u32, now: Millis) {
        let Some(b) = self.gp.bindings.iter().find(|b| b.id == id).cloned() else {
            return;
        };
        let level = self
            .devices
            .get(&b.gpd.device_id())
            .and_then(|d| d.gp.as_ref())
            .map_or(0, |g| g.security_level);
        let members = Self::target_endpoints(&b.target);
        let structural_hub_only = b.trigger.contact_mask.is_some() || members.is_empty();
        let to_hub = |hub: &mut Self| {
            if let Some(b) = hub.binding_mut(id) {
                b.executor = Some(Executor::Hub);
                b.state = BindState::Active;
                b.targets.clear();
            }
            hub.binding_event(id);
        };
        match b.choice {
            ExecutorChoice::Hub => return to_hub(self),
            ExecutorChoice::Device if structural_hub_only => {
                if let Some(b) = self.binding_mut(id) {
                    b.state = BindState::Unsupported;
                }
                return self.binding_event(id);
            }
            ExecutorChoice::Auto if structural_hub_only => return to_hub(self),
            _ => {}
        }
        // Probe unknown targets first.
        let mut waiting = false;
        for (dev, _) in &members {
            if !self.gp.caps.get(dev).is_some_and(|c| c.probed) {
                waiting = true;
                self.probe_target(dev, Some(b.gpd), now);
            }
        }
        if waiting {
            if let Some(b) = self.binding_mut(id) {
                b.state = BindState::Pending;
                b.targets = members
                    .iter()
                    .map(|(d, e)| TargetState {
                        device: d.clone(),
                        endpoint: *e,
                        step: TargetStep::Probe,
                        last_verified: None,
                        error: None,
                    })
                    .collect();
            }
            return self.binding_event(id);
        }
        let all = members
            .iter()
            .all(|(d, _)| self.gp.caps.get(d).is_some_and(|c| c.qualifies(level)));
        match (all, b.choice) {
            (true, _) => {
                if let Some(b) = self.binding_mut(id) {
                    b.executor = Some(Executor::Device);
                    b.state = BindState::Pending;
                    b.targets = members
                        .iter()
                        .map(|(d, e)| TargetState {
                            device: d.clone(),
                            endpoint: *e,
                            step: TargetStep::Pairing,
                            last_verified: None,
                            error: None,
                        })
                        .collect();
                }
                for (dev, _) in members {
                    self.program_target(id, &dev, now);
                }
                self.binding_event(id);
            }
            (false, ExecutorChoice::Device) => {
                if let Some(b) = self.binding_mut(id) {
                    b.state = BindState::Unsupported;
                }
                self.binding_event(id);
            }
            (false, _) => to_hub(self),
        }
    }

    /// Starts a capability probe of a target (`gp/targets/probe`).
    pub fn probe_target(&mut self, device: &str, gpd: Option<GpdId>, now: Millis) {
        let Some(d) = self.devices.get(device) else {
            return;
        };
        let Some(nwk) = d.nwk() else { return };
        if self.gp.caps.get(device).is_some_and(|c| !c.probed) {
            return; // already probing
        }
        self.gp
            .caps
            .insert(device.to_string(), TargetCaps::default());
        let tsn = self.next_zcl_tsn();
        let f = ZclFrame::command(
            FrameType::Global,
            tsn,
            crate::zcl::frame::global::READ_ATTRIBUTES,
            None,
            zf::read_attributes(&[0x0000, 0x0006]),
        );
        // The GP cluster lives on endpoint 242 with the GP profile.
        let req = ApsRequest {
            dst: ApsDst::Nwk(nwk),
            dst_ep: super::GP_ENDPOINT,
            src_ep: self.cfg.hub_endpoint,
            profile: PROFILE_GP,
            cluster: cl::GREEN_POWER,
            payload: f.encode(),
            options: TxOptions {
                ack: true,
                extended_timeout: false,
            },
        };
        let _ = gpd;
        self.send_tracked(device, req, Purpose::GpProbe(nwk), tsn, false, now);
    }

    /// Probe answers (Read Attributes Response or a timeout with `recs`
    /// empty).
    pub(crate) fn gp_probe_result(&mut self, nwk: u16, recs: &[ReadRecord], now: Millis) {
        let Some(device) = self
            .devices
            .values()
            .find(|d| d.nwk() == Some(nwk))
            .map(|d| d.id.clone())
        else {
            return;
        };
        let get = |a: u16| {
            recs.iter()
                .find(|r| r.attr == a && r.status == Status::SUCCESS)
                .and_then(|r| r.value.as_ref())
                .and_then(|v| v.value.as_i64())
        };
        let func = get(0x0006).map(|v| v as u32);
        let max = get(0x0000).map(|v| v as u8);
        let mut caps = TargetCaps {
            functionality: func,
            max_sink_entries: max,
            ..Default::default()
        };
        if let Some(f) = func {
            caps.translation_table = f & (1 << 18) != 0;
            caps.security_levels = (0..=3u8)
                .filter(|l| f & (1 << level_bit(*l)) != 0)
                .collect();
            if f & (1 << 11) != 0 {
                // Confirm a working sink with a Sink Table Request by GPD ID.
                let gpd = self
                    .gp
                    .bindings
                    .iter()
                    .find(|b| {
                        Self::target_endpoints(&b.target)
                            .iter()
                            .any(|(d, _)| *d == device)
                    })
                    .map_or(GpdId::SrcId(0xFFFF_FFFE), |b| b.gpd);
                let req = GpRequest::RemoteSinkTableRequest {
                    dst: nwk,
                    index: 0xFF,
                    gpd_id: gpd,
                };
                self.gp.caps.insert(device.clone(), caps);
                self.remote_send(None, &device, nwk, TargetStep::Probe, req, now);
                return;
            }
        }
        caps.probed = true;
        self.gp.caps.insert(device.clone(), caps);
        self.probe_finished(&device, now);
    }

    fn probe_finished(&mut self, device: &str, now: Millis) {
        let caps = self.gp.caps.get(device).cloned().unwrap_or_default();
        self.emit(HubEvent::Gp {
            kind: "gp/target".into(),
            body: json!({"device": device, "capabilities": caps}),
        });
        let waiting: Vec<u32> = self
            .gp
            .bindings
            .iter()
            .filter(|b| {
                b.state == BindState::Pending
                    && b.executor.is_none()
                    && Self::target_endpoints(&b.target)
                        .iter()
                        .any(|(d, _)| d == device)
            })
            .map(|b| b.id)
            .collect();
        for id in waiting {
            self.decide_binding(id, now);
        }
    }

    fn remote_send(
        &mut self,
        binding: Option<u32>,
        target: &str,
        nwk: u16,
        step: TargetStep,
        req: GpRequest,
        now: Millis,
    ) {
        match self.transport.gp_request(req.clone()) {
            Ok(Some(tsn)) => {
                self.gp.remote.insert(
                    tsn,
                    RemoteWait {
                        binding,
                        target: target.to_string(),
                        nwk,
                        step,
                        sent_at: now,
                        retries: 0,
                        req,
                    },
                );
            }
            Ok(None) => {}
            Err(e) => {
                if let Some(id) = binding {
                    self.binding_failed(id, None, format!("transport: {e}"));
                }
            }
        }
    }

    fn gpd_pairing(&self, gpd: GpdId, nwk: u16, actions: u8) -> PairingConfiguration {
        let d = self.devices.get(&gpd.device_id());
        let g = d.and_then(|d| d.gp.as_ref());
        let sink = g.and_then(|g| g.sink.clone());
        let fp = d.and_then(|d| d.fp.gp.as_ref());
        let level = g.map_or(0, |g| g.security_level);
        let mut flags = 0;
        if level > 0 {
            flags |= pairing_config_flags::SECURITY;
        }
        if fp.is_some_and(|f| f.rx_on_capability) {
            flags |= pairing_config_flags::RX_ON_CAPABLE;
        }
        let mode = sink.as_ref().map_or(2, |s| s.communication_mode);
        let group = sink
            .as_ref()
            .map(|s| s.group)
            .filter(|g| *g != 0xFFFF)
            .unwrap_or(self.cfg.gp_group);
        let (mfr, model) = (
            fp.and_then(|f| f.manufacturer_id),
            fp.and_then(|f| f.model_id),
        );
        PairingConfiguration {
            dst: nwk,
            actions,
            gpd_id: gpd,
            device_id: fp.map_or(0xFE, |f| f.device_id),
            communication_mode: mode,
            flags,
            assigned_alias: sink.as_ref().map_or(0xFFFF, |s| s.assigned_alias),
            groupcast_radius: sink.as_ref().map_or(0, |s| s.groupcast_radius),
            security_level: level,
            key_type: sink.as_ref().map_or(0, |s| s.key_type),
            key: sink.as_ref().and_then(|s| s.key).unwrap_or([0; 16]),
            // The current counter: the larger of the Sink Table's and the
            // last accepted frame's, so a reset target never accepts replays.
            frame_counter: sink
                .as_ref()
                .map_or(0, |s| s.frame_counter)
                .max(g.map_or(0, |g| g.frame_counter)),
            groups: if mode == 2 {
                vec![(group, 0xFFFF)]
            } else {
                Vec::new()
            },
            paired: 0x00,
            endpoints: Vec::new(),
            app_info: u8::from(mfr.is_some()) | (u8::from(model.is_some()) << 1),
            manufacturer_id: mfr.unwrap_or(0),
            model_id: model.unwrap_or(0),
            gpd_commands: Vec::new(),
            server_clusters: Vec::new(),
            client_clusters: Vec::new(),
        }
    }

    /// Step 1 of on-device programming for one target.
    fn program_target(&mut self, id: u32, device: &str, now: Millis) {
        let Some(b) = self.gp.bindings.iter().find(|b| b.id == id).cloned() else {
            return;
        };
        let Some(nwk) = self.nwk_of(device) else {
            return;
        };
        let pc = self.gpd_pairing(
            b.gpd,
            nwk,
            pairing_actions::EXTEND | pairing_actions::SEND_GP_PAIRING,
        );
        if let Some(t) = self
            .binding_mut(id)
            .and_then(|b| b.targets.iter_mut().find(|t| t.device == device))
        {
            t.step = TargetStep::Pairing;
        }
        self.remote_send(
            Some(id),
            device,
            nwk,
            TargetStep::Pairing,
            GpRequest::PairingConfiguration(pc),
            now,
        );
    }

    /// All device-executed translations of a GPD on a target.
    fn translations_for(
        &self,
        gpd: GpdId,
        device: &str,
        exclude: Option<u32>,
    ) -> Vec<GpTranslation> {
        self.gp
            .bindings
            .iter()
            .filter(|b| {
                b.gpd == gpd
                    && b.executor == Some(Executor::Device)
                    && Some(b.id) != exclude
                    && !matches!(
                        b.state,
                        BindState::Removing | BindState::Failed { .. } | BindState::Unsupported
                    )
            })
            .flat_map(|b| {
                Self::target_endpoints(&b.target)
                    .into_iter()
                    .filter(|(d, _)| d == device)
                    .map(|(_, ep)| b.translation(ep))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn binding_failed(&mut self, id: u32, zcl_status: Option<String>, reason: String) {
        if let Some(b) = self.binding_mut(id) {
            b.state = BindState::Failed { zcl_status, reason };
        }
        self.binding_event(id);
    }

    pub(crate) fn gp_remote_status(
        &mut self,
        src_nwk: u16,
        tsn: u8,
        _command_id: u8,
        status: u8,
        now: Millis,
    ) {
        let Some(w) = self.gp.remote.get(&tsn).cloned() else {
            return;
        };
        if w.nwk != src_nwk && src_nwk != 0xFFFF {
            return;
        }
        self.gp.remote.remove(&tsn);
        let Some(id) = w.binding else { return };
        if status != 0 {
            let st = Status(status).name().to_string();
            if matches!(
                w.step,
                TargetStep::RemoveTranslation | TargetStep::RemovePairing
            ) {
                // Removal refused: keep the record in `removing` for a retry.
                if let Some(t) = self
                    .binding_mut(id)
                    .and_then(|b| b.targets.iter_mut().find(|t| t.device == w.target))
                {
                    t.error = Some(st);
                }
                return self.binding_event(id);
            }
            return self.binding_failed(id, Some(st.clone()), format!("{st} at {:?}", w.step));
        }
        let Some(b) = self.gp.bindings.iter().find(|b| b.id == id).cloned() else {
            return;
        };
        match w.step {
            TargetStep::Pairing => {
                let tr = self.translations_for(b.gpd, &w.target, None);
                let tr: Vec<_> = tr.into_iter().take(MAX_TRANSLATIONS_PER_UPDATE).collect();
                if let Some(t) = self
                    .binding_mut(id)
                    .and_then(|b| b.targets.iter_mut().find(|t| t.device == w.target))
                {
                    t.step = TargetStep::Translation;
                }
                let req = GpRequest::TranslationUpdate {
                    dst: w.nwk,
                    action: translation_action::REPLACE,
                    gpd_id: b.gpd,
                    translations: tr,
                };
                self.remote_send(
                    Some(id),
                    &w.target,
                    w.nwk,
                    TargetStep::Translation,
                    req,
                    now,
                );
            }
            TargetStep::Translation => {
                if let Some(t) = self
                    .binding_mut(id)
                    .and_then(|b| b.targets.iter_mut().find(|t| t.device == w.target))
                {
                    t.step = TargetStep::Verify;
                }
                let req = GpRequest::TranslationRequest {
                    dst: w.nwk,
                    start_index: 0,
                };
                self.remote_send(Some(id), &w.target, w.nwk, TargetStep::Verify, req, now);
            }
            TargetStep::RemoveTranslation => {
                let others = self.translations_for(b.gpd, &w.target, Some(id));
                if others.is_empty() {
                    if let Some(t) = self
                        .binding_mut(id)
                        .and_then(|b| b.targets.iter_mut().find(|t| t.device == w.target))
                    {
                        t.step = TargetStep::RemovePairing;
                    }
                    let pc = self.gpd_pairing(
                        b.gpd,
                        w.nwk,
                        pairing_actions::REMOVE_PAIRING | pairing_actions::SEND_GP_PAIRING,
                    );
                    self.remote_send(
                        Some(id),
                        &w.target,
                        w.nwk,
                        TargetStep::RemovePairing,
                        GpRequest::PairingConfiguration(pc),
                        now,
                    );
                } else {
                    self.target_removed(id, &w.target, now);
                }
            }
            TargetStep::RemovePairing => self.target_removed(id, &w.target, now),
            _ => {}
        }
    }

    fn target_removed(&mut self, id: u32, device: &str, now: Millis) {
        let Some(b) = self.binding_mut(id) else {
            return;
        };
        if let Some(t) = b.targets.iter_mut().find(|t| t.device == device) {
            t.step = TargetStep::Removed;
        }
        if b.targets.iter().all(|t| t.step == TargetStep::Removed) {
            match b.next_choice.take() {
                Some(choice) => {
                    // Executor change: the old one is gone, start the new one.
                    b.choice = choice;
                    b.executor = None;
                    b.state = BindState::Pending;
                    b.targets.clear();
                    self.decide_binding(id, now);
                }
                None => {
                    let rec = b.to_json();
                    self.gp.bindings.retain(|x| x.id != id);
                    let mut body = rec;
                    body["state"] = "removed".into();
                    self.emit(HubEvent::Gp {
                        kind: "gp/binding".into(),
                        body,
                    });
                }
            }
        }
    }

    pub(crate) fn gp_remote_sink_table(
        &mut self,
        src_nwk: u16,
        tsn: u8,
        status: u8,
        _records: &[GpSinkRecord],
        now: Millis,
    ) {
        let Some(w) = self.gp.remote.remove(&tsn) else {
            return;
        };
        let _ = src_nwk;
        if w.step == TargetStep::Probe {
            let ok = status == Status::SUCCESS.0 || status == Status::NOT_FOUND.0;
            if let Some(c) = self.gp.caps.get_mut(&w.target) {
                c.sink = ok;
                c.probed = true;
            }
            self.probe_finished(&w.target, now);
        }
    }

    pub(crate) fn gp_remote_translation_table(
        &mut self,
        src_nwk: u16,
        tsn: u8,
        status: u8,
        records: &[(GpdId, GpTranslation)],
        now: Millis,
    ) {
        let Some(w) = self.gp.remote.remove(&tsn) else {
            return;
        };
        let _ = src_nwk;
        let Some(id) = w.binding else { return };
        let Some(b) = self.gp.bindings.iter().find(|b| b.id == id).cloned() else {
            return;
        };
        if status != 0 && status != Status::NOT_FOUND.0 {
            return self.binding_failed(
                id,
                Some(Status(status).name().into()),
                "translation table request failed".into(),
            );
        }
        let want = self.translations_for(b.gpd, &w.target, None);
        let have: Vec<&GpTranslation> = records
            .iter()
            .filter(|(g, _)| *g == b.gpd)
            .map(|(_, t)| t)
            .collect();
        let complete = want.iter().all(|t| have.contains(&t));
        if complete {
            if let Some(bm) = self.binding_mut(id) {
                if let Some(t) = bm.targets.iter_mut().find(|t| t.device == w.target) {
                    t.step = TargetStep::Done;
                    t.last_verified = Some(now);
                    t.error = None;
                }
                if bm.targets.iter().all(|t| t.step == TargetStep::Done) {
                    bm.state = BindState::Active;
                }
            }
            self.binding_event(id);
        } else if b.state == BindState::Active || b.state == BindState::Drift {
            // Drift: reprogram (the frame counter is re-sent with it).
            if let Some(bm) = self.binding_mut(id) {
                bm.state = BindState::Drift;
            }
            self.binding_event(id);
            self.program_target(id, &w.target, now);
        } else {
            self.binding_failed(
                id,
                None,
                "target does not hold the translation after programming".into(),
            );
        }
    }

    /// Remote request timeouts (called from `tick`).
    pub(crate) fn gp_tick(&mut self, now: Millis) {
        let late: Vec<u8> = self
            .gp
            .remote
            .iter()
            .filter(|(_, w)| now.saturating_sub(w.sent_at) > REMOTE_TIMEOUT_MS)
            .map(|(k, _)| *k)
            .collect();
        for tsn in late {
            let Some(mut w) = self.gp.remote.remove(&tsn) else {
                continue;
            };
            if w.retries < REMOTE_RETRIES {
                w.retries += 1;
                w.sent_at = now;
                if let Ok(Some(t)) = self.transport.gp_request(w.req.clone()) {
                    self.gp.remote.insert(t, w);
                }
                continue;
            }
            match (w.binding, w.step) {
                (None, TargetStep::Probe) => {
                    if let Some(c) = self.gp.caps.get_mut(&w.target) {
                        c.sink = false;
                        c.probed = true;
                    }
                    self.probe_finished(&w.target, now);
                }
                (Some(id), TargetStep::RemoveTranslation | TargetStep::RemovePairing) => {
                    // Unreachable: stay `removing`, retried at the next announcement.
                    if let Some(t) = self
                        .binding_mut(id)
                        .and_then(|b| b.targets.iter_mut().find(|t| t.device == w.target))
                    {
                        t.error = Some("timeout".into());
                    }
                    self.binding_event(id);
                }
                (Some(id), step) => self.binding_failed(id, None, format!("timeout at {step:?}")),
                _ => {}
            }
        }
    }

    /// `gp/bindings/delete`.
    pub fn delete_binding(&mut self, id: u32, now: Millis) -> Result<(), ApiError> {
        let b = self
            .gp
            .bindings
            .iter()
            .find(|b| b.id == id)
            .cloned()
            .ok_or_else(|| ApiError::new("not_found", format!("no binding {id}")))?;
        let programmed = b.executor == Some(Executor::Device)
            && b.targets
                .iter()
                .any(|t| !matches!(t.step, TargetStep::Probe | TargetStep::Removed));
        if !programmed {
            self.gp.bindings.retain(|x| x.id != id);
            let mut body = b.to_json();
            body["state"] = "removed".into();
            self.emit(HubEvent::Gp {
                kind: "gp/binding".into(),
                body,
            });
            return Ok(());
        }
        self.start_removal(id, now);
        Ok(())
    }

    fn start_removal(&mut self, id: u32, now: Millis) {
        let Some(b) = self.binding_mut(id) else {
            return;
        };
        b.state = BindState::Removing;
        let b = b.clone();
        for t in &b.targets {
            if t.step == TargetStep::Removed {
                continue;
            }
            let Some(nwk) = self.nwk_of(&t.device) else {
                continue;
            };
            if let Some(tm) = self
                .binding_mut(id)
                .and_then(|x| x.targets.iter_mut().find(|x| x.device == t.device))
            {
                tm.step = TargetStep::RemoveTranslation;
            }
            let req = GpRequest::TranslationUpdate {
                dst: nwk,
                action: translation_action::REMOVE,
                gpd_id: b.gpd,
                translations: vec![b.translation(t.endpoint)],
            };
            self.remote_send(
                Some(id),
                &t.device,
                nwk,
                TargetStep::RemoveTranslation,
                req,
                now,
            );
        }
        self.binding_event(id);
    }

    /// `gp/bindings/update`: a new executor, action or trigger. Changing
    /// away from an on-device executor removes the old programming first,
    /// so a press never runs twice.
    pub fn update_binding(
        &mut self,
        id: u32,
        executor: Option<ExecutorChoice>,
        action: Option<BindAction>,
        trigger: Option<BindTrigger>,
        now: Millis,
    ) -> Result<J, ApiError> {
        let b = self
            .gp
            .bindings
            .iter()
            .find(|b| b.id == id)
            .cloned()
            .ok_or_else(|| ApiError::new("not_found", format!("no binding {id}")))?;
        let choice = executor.unwrap_or(b.choice);
        let was_device = b.executor == Some(Executor::Device)
            && b.targets.iter().any(|t| t.step != TargetStep::Probe);
        if was_device {
            if let Some(bm) = self.binding_mut(id) {
                bm.next_choice = Some(choice);
            }
            self.start_removal(id, now);
            if let Some(bm) = self.binding_mut(id) {
                if let Some(a) = action {
                    bm.action = a;
                }
                if let Some(t) = trigger {
                    bm.trigger = t;
                }
            }
        } else {
            if let Some(bm) = self.binding_mut(id) {
                bm.choice = choice;
                bm.executor = None;
                bm.state = BindState::Pending;
                bm.targets.clear();
                if let Some(a) = action {
                    bm.action = a;
                }
                if let Some(t) = trigger {
                    bm.trigger = t;
                }
            }
            self.decide_binding(id, now);
        }
        Ok(self
            .gp
            .bindings
            .iter()
            .find(|b| b.id == id)
            .map(GpBindingRecord::to_json)
            .unwrap_or_default())
    }

    /// `gp/bindings/verify`: re-reads on-device targets, repairs drift.
    pub fn verify_bindings(&mut self, id: Option<u32>, now: Millis) {
        let ids: Vec<u32> = self
            .gp
            .bindings
            .iter()
            .filter(|b| id.is_none_or(|x| x == b.id) && b.executor == Some(Executor::Device))
            .map(|b| b.id)
            .collect();
        for bid in ids {
            let Some(b) = self.gp.bindings.iter().find(|b| b.id == bid).cloned() else {
                continue;
            };
            if b.state == BindState::Removing {
                self.start_removal(bid, now);
                continue;
            }
            for t in &b.targets {
                let Some(nwk) = self.nwk_of(&t.device) else {
                    continue;
                };
                let req = GpRequest::TranslationRequest {
                    dst: nwk,
                    start_index: 0,
                };
                self.remote_send(Some(bid), &t.device, nwk, TargetStep::Verify, req, now);
            }
        }
    }

    /// `gp/bindings/list`.
    pub fn list_bindings(&self, gpd: Option<&str>, target: Option<&str>) -> Vec<J> {
        self.gp
            .bindings
            .iter()
            .filter(|b| gpd.is_none_or(|g| b.gpd.device_id() == g))
            .filter(|b| {
                target.is_none_or(|t| {
                    Self::target_endpoints(&b.target)
                        .iter()
                        .any(|(d, _)| d == t)
                })
            })
            .map(GpBindingRecord::to_json)
            .collect()
    }

    /// Bindings for persistence.
    pub fn export_bindings(&self) -> Vec<GpBindingRecord> {
        self.gp.bindings.clone()
    }

    /// Restores persisted bindings; on-device ones are re-verified as
    /// drift (13.8 "a restore of a hub backup reprograms on-device
    /// bindings as drift").
    pub fn import_bindings(&mut self, list: Vec<GpBindingRecord>, now: Millis) {
        self.gp.next_id = list
            .iter()
            .map(|b| b.id)
            .max()
            .unwrap_or(0)
            .max(self.gp.next_id);
        self.gp.bindings = list;
        self.verify_bindings(None, now);
    }

    /// A target announced itself (Device_annce, rejoin): retry removals and
    /// check for drift.
    pub(crate) fn gp_target_announced(&mut self, device: &str, now: Millis) {
        let ids: Vec<u32> = self
            .gp
            .bindings
            .iter()
            .filter(|b| {
                b.executor == Some(Executor::Device) && b.targets.iter().any(|t| t.device == device)
            })
            .map(|b| b.id)
            .collect();
        for id in ids {
            self.verify_bindings(Some(id), now);
        }
    }

    /// Hub-side execution (13.8 b).
    pub(crate) fn gp_execute_bindings(&mut self, f: &GpFrame, now: Millis) {
        let list: Vec<GpBindingRecord> = self
            .gp
            .bindings
            .iter()
            .filter(|b| {
                b.gpd == f.gpd_id
                    && b.executor == Some(Executor::Hub)
                    && b.state == BindState::Active
                    && b.matches(f.command_id, &f.payload)
            })
            .cloned()
            .collect();
        for b in list {
            let payload = match &b.action.payload {
                TranslationPayload::Fixed(p) => p.clone(),
                TranslationPayload::Copy => f.payload.clone(),
                TranslationPayload::Derived => {
                    zigbee_gp::translation::translate(f.command_id, &f.payload)
                        .map(|t| t.payload.to_vec())
                        .unwrap_or_default()
                }
            };
            let group = matches!(b.target, BindTarget::Group { .. });
            let tsn = self.next_zcl_tsn();
            let frame = ZclFrame {
                header: ZclHeader {
                    frame_type: FrameType::Cluster,
                    mfr: None,
                    direction: Direction::ClientToServer,
                    disable_default_response: group,
                    tsn,
                    cmd: b.action.command,
                },
                payload,
            };
            let (dst, dst_ep) = match &b.target {
                BindTarget::Device { device, endpoint } => match self.nwk_of(device) {
                    Some(n) => (ApsDst::Nwk(n), *endpoint),
                    None => continue,
                },
                BindTarget::Group { group, .. } => (ApsDst::Group(*group), 0xFF),
            };
            let req = ApsRequest {
                dst,
                dst_ep,
                src_ep: self.cfg.hub_endpoint,
                profile: b.action.profile,
                cluster: b.action.cluster,
                payload: frame.encode(),
                options: TxOptions {
                    ack: !group,
                    extended_timeout: false,
                },
            };
            let _ = self.transport.aps_send(req);
            let _ = now;
        }
    }

    /// Event attributes for a GP event entity: the bindings of the frame
    /// being processed and their executor (13.8).
    pub(crate) fn gp_binding_attrs(&self, device: &str, _entity: usize) -> Option<Map<String, J>> {
        let d = self.devices.get(device)?;
        let DeviceKind::Gp { gpd_id } = d.kind else {
            return None;
        };
        let (cmd, payload) = self
            .gp_last_frame
            .as_ref()
            .filter(|(g, _, _)| *g == gpd_id)
            .map(|(_, c, p)| (*c, p.clone()))?;
        let bs: Vec<&GpBindingRecord> = self
            .gp
            .bindings
            .iter()
            .filter(|b| b.gpd == gpd_id && b.matches(cmd, &payload))
            .collect();
        if bs.is_empty() {
            return None;
        }
        let mut m = Map::new();
        m.insert(
            "bindings".into(),
            json!(bs.iter().map(|b| b.id).collect::<Vec<_>>()),
        );
        let ex = bs.iter().find_map(|b| b.executor).map_or("pending", |e| {
            if e == Executor::Device {
                "device"
            } else {
                "hub"
            }
        });
        m.insert("executor".into(), ex.into());
        Some(m)
    }

    /// Last verified time of an on-device target, s.
    pub fn binding_last_verified(&self, id: u32) -> Option<f64> {
        self.gp
            .bindings
            .iter()
            .find(|b| b.id == id)?
            .targets
            .iter()
            .filter_map(|t| t.last_verified)
            .max()
            .map(ts)
    }
}

/// `true` for a ZCL Read value that is a map24 functionality.
pub fn functionality_of(v: &ZclValue) -> Option<u32> {
    v.as_i64().map(|x| x as u32)
}
