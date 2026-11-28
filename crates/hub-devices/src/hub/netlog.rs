//! What the API shows of the devices' network life beyond their own
//! records (API 1.4): the network log, the last [`NET_LOG`] joins, rejoins,
//! announcements, leaves and refused joins (`network/events`, event
//! `device_network` on topic `network`), and per device ([`DevNote`]) the
//! interview's failed steps, since when it is partial, and why and since
//! when the device is unavailable. All of it is kept for this run only and
//! beside the devices, not in [`Device`]: a larger `Device` hits the Xtensa
//! LLVM bug noted at `Hub::interview_quirks`.

use std::collections::VecDeque;

use hub_coord::JoinKind;
use serde_json::Value as J;

use super::{Hub, HubEvent, zigbee_id};
use crate::device::{Device, InterviewStep, Millis, ts};
use crate::transport::ZigbeeTransport;

/// Network log entries kept.
pub const NET_LOG: usize = 32;

/// What happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetEvent {
    /// A join (association, direct), admitted or not.
    Joined,
    /// A rejoin (secured, through the trust center), admitted or not.
    Rejoined,
    /// A Device_annce.
    Announced,
    /// The coordinator reported a leave.
    Left,
    /// The NCP dropped a child that timed out (it need not have left).
    AgedOut,
}

/// One network log entry.
#[derive(Debug, Clone)]
pub struct NetEntry {
    /// When, engine time.
    pub at: Millis,
    /// Device IEEE.
    pub ieee: u64,
    /// Its short address.
    pub nwk: u16,
    /// The short address it had before, when it changed.
    pub prev: Option<u16>,
    /// What.
    pub event: NetEvent,
    /// Joins and rejoins: refused by the trust center.
    pub denied: bool,
    /// Joins: `association`, `direct`, `secured`, `tc`.
    pub how: &'static str,
    /// Refused joins: `permit_closed`, `refused`; leaves: `self`, `removed`.
    pub reason: &'static str,
    /// Leaves: whether it said it rejoins (when the backend knows).
    pub rejoin: Option<bool>,
}

impl NetEntry {
    /// The entry as a JSON object (text): the `device_network` event's body.
    pub fn text(&self) -> String {
        use std::fmt::Write;
        let event = match self.event {
            NetEvent::Joined => "joined",
            NetEvent::Rejoined => "rejoined",
            NetEvent::Announced => "announced",
            NetEvent::Left => "left",
            NetEvent::AgedOut => "aged_out",
        };
        let mut s = format!(
            "{{\"topic\":\"device_network\",\"event\":\"{event}\",\"device\":\"{}\",\"nwk\":{},\"time\":{}",
            zigbee_id(self.ieee),
            self.nwk,
            ts(self.at)
        );
        if let Some(p) = self.prev {
            let _ = write!(s, ",\"prev_nwk\":{p}");
        }
        if matches!(self.event, NetEvent::Joined | NetEvent::Rejoined) {
            let d = if self.denied { "denied" } else { "admitted" };
            let _ = write!(s, ",\"decision\":\"{d}\",\"how\":\"{}\"", self.how);
        }
        if !self.reason.is_empty() {
            let _ = write!(s, ",\"reason\":\"{}\"", self.reason);
        }
        if let Some(r) = self.rejoin {
            let _ = write!(s, ",\"rejoin\":{r}");
        }
        s.push('}');
        s
    }
}

/// A device's interview and availability details (this run).
#[derive(Debug, Clone, Default)]
pub struct DevNote {
    /// Since when its interview is partial.
    pub partial_since: Option<Millis>,
    /// The interview steps that failed in its last attempt.
    pub failed: Vec<String>,
    /// Since when and why it is unavailable: `left`, `no_response` (a
    /// mains device did not answer the availability check), `timeout`
    /// (silent longer than its availability timeout).
    pub unavailable: Option<(Millis, &'static str)>,
}

fn how(kind: JoinKind) -> &'static str {
    match kind {
        JoinKind::Association => "association",
        JoinKind::SecuredRejoin => "secured",
        JoinKind::TcRejoin => "tc",
        JoinKind::Direct => "direct",
    }
}

/// The name of an interview step (event `interview_progress`).
pub(crate) fn step_name(step: InterviewStep) -> String {
    match step {
        InterviewStep::NodeDesc => "node_descriptor".to_string(),
        InterviewStep::ActiveEp => "active_endpoints".into(),
        InterviewStep::SimpleDesc(i) => format!("simple_descriptor_{i}"),
        InterviewStep::Basic(i) => format!("basic_{i}"),
        InterviewStep::SchemaHeader => "self_description".into(),
        InterviewStep::SchemaChunk(i) => format!("self_description_chunk_{i}"),
        InterviewStep::Configure => "configure".into(),
        InterviewStep::Done => "done".into(),
    }
}

impl<T: ZigbeeTransport> Hub<T> {
    /// Keeps a network log entry and announces it (`device_network`).
    pub fn net_log(&mut self, e: NetEntry) {
        if self.netlog.len() >= NET_LOG {
            self.netlog.pop_front();
        }
        let text = e.text();
        self.netlog.push_back(e);
        self.emit(HubEvent::AppText {
            topic: "network",
            text,
        });
    }

    /// The network log, oldest first (`network/events`).
    pub fn network_events(&self) -> J {
        let mut s = String::from("[");
        for (i, e) in self.netlog.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&e.text());
        }
        s.push(']');
        serde_json::from_str(&s).unwrap_or_default()
    }

    /// The network log.
    pub fn net_entries(&self) -> &VecDeque<NetEntry> {
        &self.netlog
    }

    /// A device the trust center admitted: logged (`joined`, `rejoined`
    /// with the device's rejoin count), then [`Hub::on_device_joined`].
    pub fn on_join(&mut self, ieee: u64, nwk: u16, kind: JoinKind, now: Millis) {
        let id = zigbee_id(ieee);
        let rejoin = matches!(kind, JoinKind::SecuredRejoin | JoinKind::TcRejoin);
        let known = self.devices.get_mut(&id).map(|d| {
            if rejoin {
                d.diag.count("rejoins");
            }
            d.nwk().filter(|p| *p != nwk)
        });
        self.net_log(NetEntry {
            at: now,
            ieee,
            nwk,
            prev: known.flatten(),
            event: if rejoin {
                NetEvent::Rejoined
            } else {
                NetEvent::Joined
            },
            denied: false,
            how: how(kind),
            reason: "",
            rejoin: None,
        });
        self.on_device_joined(ieee, nwk, now);
        if known.is_some() {
            self.updated(&id);
        }
    }

    /// A join or rejoin the trust center refused; `closed`: joining was
    /// closed then.
    pub fn on_join_denied(
        &mut self,
        ieee: u64,
        nwk: u16,
        kind: JoinKind,
        closed: bool,
        now: Millis,
    ) {
        self.net_log(NetEntry {
            at: now,
            ieee,
            nwk,
            prev: None,
            event: if matches!(kind, JoinKind::SecuredRejoin | JoinKind::TcRejoin) {
                NetEvent::Rejoined
            } else {
                NetEvent::Joined
            },
            denied: true,
            how: how(kind),
            reason: if closed { "permit_closed" } else { "refused" },
            rejoin: None,
        });
    }

    /// The coordinator reported a leave (`nwk` finds the device when the
    /// IEEE is not known, 0): logged (`left`, `removed` when the hub
    /// removed it), counted, and unless it said it rejoins, the device is
    /// unavailable ([`Hub::on_device_left`]). `aged_out`: the NCP only
    /// dropped it from its child table after a timeout (a router child
    /// routing through others now, a sleeping or moved end device): logged
    /// as `aged_out`, nothing else; its availability stays the usual
    /// rules' (4.6).
    pub fn on_leave(
        &mut self,
        ieee: u64,
        nwk: u16,
        rejoin: Option<bool>,
        removed: bool,
        aged_out: bool,
        now: Millis,
    ) {
        let ieee = match ieee {
            0 => self
                .nwk_index
                .get(&nwk)
                .and_then(|id| self.devices.get(id))
                .and_then(Device::ieee)
                .unwrap_or(0),
            i => i,
        };
        let aged_out = aged_out && !removed;
        self.net_log(NetEntry {
            at: now,
            ieee,
            nwk,
            prev: None,
            event: if aged_out {
                NetEvent::AgedOut
            } else {
                NetEvent::Left
            },
            denied: false,
            how: "",
            reason: match (aged_out, removed) {
                (true, _) => "",
                (_, true) => "removed",
                _ => "self",
            },
            rejoin: rejoin.filter(|_| !aged_out),
        });
        if aged_out {
            return;
        }
        let id = zigbee_id(ieee);
        let Some(d) = self.devices.get_mut(&id) else {
            return;
        };
        d.diag.count("leaves");
        if rejoin == Some(true) {
            self.updated(&id);
        } else {
            self.on_device_left(ieee, now);
        }
    }

    /// `device_updated` for `id`, unless one is queued already.
    pub(crate) fn updated(&mut self, id: &str) {
        if !self
            .events
            .iter()
            .any(|e| matches!(e, HubEvent::DeviceUpdated(x) if x == id))
        {
            self.emit(HubEvent::DeviceUpdated(id.to_string()));
        }
    }

    /// A device's note, created when missing.
    pub(crate) fn note(&mut self, id: &str) -> &mut DevNote {
        self.notes.entry(id.to_owned()).or_default()
    }

    /// An interview step of the device failed and made it partial.
    pub(crate) fn note_interview_failed(&mut self, id: &str, step: InterviewStep, now: Millis) {
        let name = step_name(step);
        let n = self.note(id);
        n.partial_since.get_or_insert(now);
        if !n.failed.contains(&name) {
            n.failed.push(name);
        }
    }

    /// A new interview attempt: the failed steps of the last one go;
    /// `fresh` (a requested interview, or one that completed) also ends
    /// the partial interview's time.
    pub(crate) fn note_interview_start(&mut self, id: &str, fresh: bool) {
        if let Some(n) = self.notes.get_mut(id) {
            n.failed.clear();
            if fresh {
                n.partial_since = None;
            }
        }
    }

    /// The device record (4.1) with what is noted about it: `unavailable_reason`
    /// and `unavailable_since`; while the interview is partial
    /// `interview_since` and `interview_failed`; `interview_quirk`.
    pub fn device_record(&self, d: &Device) -> J {
        let mut v = d.record();
        if let Some(n) = self.notes.get(d.id.as_str()) {
            if let Some((t, why)) = n.unavailable.filter(|_| !d.available) {
                v["unavailable_reason"] = why.into();
                v["unavailable_since"] = ts(t).into();
            }
            if d.interview_partial {
                if let Some(t) = n.partial_since {
                    v["interview_since"] = ts(t).into();
                }
                if !n.failed.is_empty() {
                    v["interview_failed"] = n.failed.clone().into();
                }
            }
        }
        if self.interview_quirks.contains(&d.id) {
            v["interview_quirk"] = "lumi".into();
        }
        v
    }
}
