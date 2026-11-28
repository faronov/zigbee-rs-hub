//! The `snapshot` event of a subscription (14.1), rendered piece by piece.
//!
//! A WebSocket sink sends the pieces as the fragments of one message, so the
//! client sees the same text as from [`SnapshotStream::render`]; the hub
//! makes the next piece only once the previous ones have been sent, and no
//! large contiguous buffer is needed. The device list is taken when the
//! subscription is made; each device is rendered when its turn comes, and
//! the events that follow the snapshot repeat whatever changed meanwhile.

use std::collections::VecDeque;

use serde_json::Value as J;

use crate::device::Millis;
use crate::hub::Hub;
use crate::transport::ZigbeeTransport;

/// What a device contributes to a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PerDevice {
    Record,
    Entities,
    States,
}

/// Lists the engine renders whole (groups and Green Power: short).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Items {
    ZgDevices,
    ZgEntities,
    ZgStates,
    Groups,
    GroupBindings,
    GpBindings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    GpCommissioning,
    Network,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    /// `,"key":[`
    Open(&'static str),
    /// The next device of [`SnapshotStream::ids`] from this position.
    Devices(PerDevice, usize),
    Items(Items),
    /// `]`
    Close,
    /// `,"key":value`
    Field(&'static str, Field),
    /// `,"topic":text` of a service outside the engine ([`Hub::app_snapshots`]).
    App(&'static str),
    /// `}}`
    End,
}

/// A snapshot still being rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotStream {
    head: Option<String>,
    ids: Vec<String>,
    steps: VecDeque<Step>,
    /// No item in the open list yet.
    first: bool,
}

impl SnapshotStream {
    pub(crate) fn new<T: ZigbeeTransport>(
        hub: &Hub<T>,
        topics: &[String],
        id: u64,
        seq: u64,
    ) -> Self {
        let has = |t: &str| topics.iter().any(|x| x == t);
        fn list(
            steps: &mut VecDeque<Step>,
            key: &'static str,
            per: Option<PerDevice>,
            items: Items,
        ) {
            steps.push_back(Step::Open(key));
            if let Some(per) = per {
                steps.push_back(Step::Devices(per, 0));
            }
            steps.push_back(Step::Items(items));
            steps.push_back(Step::Close);
        }
        let mut steps = VecDeque::new();
        if has("devices") {
            list(
                &mut steps,
                "devices",
                Some(PerDevice::Record),
                Items::ZgDevices,
            );
        }
        if has("entities") {
            list(
                &mut steps,
                "entities",
                Some(PerDevice::Entities),
                Items::ZgEntities,
            );
            list(
                &mut steps,
                "states",
                Some(PerDevice::States),
                Items::ZgStates,
            );
        }
        if has("groups") {
            list(&mut steps, "groups", None, Items::Groups);
            list(&mut steps, "bindings", None, Items::GroupBindings);
        }
        if has("gp") {
            list(&mut steps, "bindings", None, Items::GpBindings);
            steps.push_back(Step::Field("gp_commissioning", Field::GpCommissioning));
        }
        if has("network") {
            steps.push_back(Step::Field("network", Field::Network));
        }
        for &t in hub.app_snapshots.keys() {
            if has(t) {
                steps.push_back(Step::App(t));
            }
        }
        steps.push_back(Step::End);
        let per_device = has("devices") || has("entities");
        Self {
            head: Some(format!(
                "{{\"type\":\"event\",\"id\":{id},\"seq\":{seq},\"event\":{{\"topic\":\"snapshot\""
            )),
            ids: if per_device {
                hub.devices().map(|d| d.id.clone()).collect()
            } else {
                Vec::new()
            },
            steps,
            first: true,
        }
    }

    /// The next piece, about `size` bytes (a device's records are never
    /// split), and whether it is the last; `None` once the last was taken.
    pub fn next_piece<T: ZigbeeTransport>(
        &mut self,
        hub: &mut Hub<T>,
        now: Millis,
        size: usize,
    ) -> Option<(String, bool)> {
        if self.steps.is_empty() {
            return None;
        }
        let mut s = String::with_capacity(size.min(16 * 1024) + 512);
        if let Some(head) = self.head.take() {
            s.push_str(&head);
        }
        while s.len() < size.max(1) {
            let Some(step) = self.steps.front_mut() else {
                break;
            };
            match step {
                Step::Open(key) => {
                    s.push_str(",\"");
                    s.push_str(key);
                    s.push_str("\":[");
                    self.first = true;
                    self.steps.pop_front();
                }
                Step::Devices(per, pos) => {
                    let Some(id) = self.ids.get(*pos) else {
                        self.steps.pop_front();
                        continue;
                    };
                    *pos += 1;
                    let per = *per;
                    // A device removed meanwhile: its `device_removed` event follows.
                    if let Some(d) = hub.device(id) {
                        match per {
                            PerDevice::Record => {
                                item(&mut s, &mut self.first, &hub.device_record(d))
                            }
                            PerDevice::Entities => {
                                for e in &d.entities {
                                    item(&mut s, &mut self.first, &e.record(&d.id));
                                }
                            }
                            PerDevice::States => {
                                for e in &d.entities {
                                    item(&mut s, &mut self.first, &e.state_msg(d.available));
                                }
                            }
                        }
                    }
                }
                Step::Items(kind) => {
                    let values = match kind {
                        Items::ZgDevices => hub.zg_device_records(),
                        Items::ZgEntities => hub.zg_entity_records(),
                        Items::ZgStates => hub.zg_states(),
                        Items::Groups => hub.groups_list(),
                        Items::GroupBindings => hub.bindings_list(None),
                        Items::GpBindings => hub.list_bindings(None, None),
                    };
                    for v in &values {
                        item(&mut s, &mut self.first, v);
                    }
                    self.steps.pop_front();
                }
                Step::Close => {
                    s.push(']');
                    self.steps.pop_front();
                }
                Step::Field(key, field) => {
                    let v = match field {
                        Field::GpCommissioning => hub.gp_commissioning_status(now),
                        Field::Network => super::network_info(hub, now).unwrap_or(J::Null),
                    };
                    s.push_str(",\"");
                    s.push_str(key);
                    s.push_str("\":");
                    s.push_str(&v.to_string());
                    self.steps.pop_front();
                }
                Step::App(key) => {
                    // Kept current by the application; gone meanwhile: left out.
                    if let Some(text) = hub.app_snapshots.get(key) {
                        s.push_str(",\"");
                        s.push_str(key);
                        s.push_str("\":");
                        s.push_str(text);
                    }
                    self.steps.pop_front();
                }
                Step::End => {
                    s.push_str("}}");
                    self.steps.pop_front();
                }
            }
        }
        Some((s, self.steps.is_empty()))
    }

    /// The whole snapshot text (sinks that take whole messages only).
    pub fn render<T: ZigbeeTransport>(mut self, hub: &mut Hub<T>, now: Millis) -> String {
        let mut out = String::new();
        while let Some((piece, _)) = self.next_piece(hub, now, usize::MAX) {
            out.push_str(&piece);
        }
        out
    }
}

fn item(s: &mut String, first: &mut bool, v: &J) {
    if !std::mem::take(first) {
        s.push(',');
    }
    s.push_str(&v.to_string());
}
