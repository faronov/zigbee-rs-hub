//! Control requests (GP requests, group membership, device removal, permit
//! join) and the outcomes of every request ([`Hub::on_outcome`]).
//!
//! A control request carries what follows its outcome ([`Then`]). An API
//! request that a control request decides gets an answer ticket: answered
//! at once when the outcome is known by the time the handler returns
//! ([`Hub::answer`]), otherwise later as [`HubEvent::Answer`], which the
//! API delivers as the deferred result of that request
//! ([`Hub::take_deferred`]).

use super::*;
use crate::gp::GpSinkSettings;
use crate::gp::bindings::RemoteWait;
use crate::gp::types::GpEvent;
use crate::transport::{CoordError, Outcome, Progress, Request, RequestId, Terminal};

/// Answer tickets, apart from command tickets (which count from 1).
const ANSWER_TICKETS: u64 = 1 << 63;

/// Remote GP answers kept until their request is accepted.
const MAX_EARLY: usize = 4;

/// A control request in flight.
pub(crate) struct Control {
    then: Then,
    /// The API answer it decides.
    answer: Option<AnswerTo>,
}

/// An API answer that a control request decides.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AnswerTo {
    ticket: u64,
    /// The events when the request began: the answer goes before every
    /// event the request caused, as when the handler returned it.
    mark: usize,
    /// `Hub::drains` then: after a drain the mark no longer applies.
    drains: u64,
}

impl AnswerTo {
    /// Its ticket.
    pub(crate) fn ticket(self) -> u64 {
        self.ticket
    }
}

/// What follows the outcome of a control request.
pub(crate) enum Then {
    /// Nothing (device removal).
    Nothing,
    /// Hub endpoint group membership: `hub_error`, then the rest of the
    /// group request.
    Membership { group: u16, tail: GroupTail },
    /// A configure step (`group` op).
    Configure { device: String, key: String },
    /// `network/permit_join`, then the GP window for `gp` seconds.
    PermitJoin { gp: Option<u8>, at: Millis },
    /// The GP commissioning window opened.
    GpOpen {
        duration_s: u16,
        involve_proxies: bool,
        exit_on_first_pairing: bool,
        at: Millis,
    },
    /// The GP commissioning window closed.
    GpClose,
    /// GP sink settings written.
    GpConfig(GpSinkSettings),
    /// A GPD's Sink Table entry removed, then its device.
    GpRemove(GpRemoved),
    /// A remote GP request, until the NCP accepts it with its ZCL TSN.
    GpRemote(Box<RemoteWait>),
}

/// The rest of a group request after the hub endpoint's membership.
pub(crate) enum GroupTail {
    /// Nothing (`groups_resync`).
    None,
    /// `groups/create`.
    Create,
    /// `groups/update`.
    Update,
    /// `groups/delete`.
    Delete,
}

/// The device record dropped once its Sink Table entry is gone.
pub(crate) enum GpRemoved {
    /// `gp/devices/remove`.
    Entry(String),
    /// `devices/remove`.
    Device(String),
}

/// The ZCL TSN of a remote GP answer.
fn remote_tsn(ev: &GpEvent) -> Option<u8> {
    match ev {
        GpEvent::RemoteStatus { zcl_tsn, .. }
        | GpEvent::RemoteSinkTable { zcl_tsn, .. }
        | GpEvent::RemoteTranslationTable { zcl_tsn, .. } => Some(*zcl_tsn),
        _ => None,
    }
}

impl<T: ZigbeeTransport> Hub<T> {
    /// The progress or the final outcome of request `id`, in the order the
    /// coordinator reports them.
    pub fn on_outcome(&mut self, id: RequestId, outcome: Outcome, now: Millis) {
        match outcome {
            Outcome::Progress(Progress::Accepted { zcl_tsn }) => self.gp_accepted(id, zcl_tsn, now),
            Outcome::Done(t) => match self.controls.remove(&id) {
                Some(c) => {
                    let r = match t {
                        Terminal::Delivered | Terminal::Sent | Terminal::Done(_) => Ok(()),
                        Terminal::NotDelivered(e) | Terminal::Refused(e) | Terminal::Unknown(e) => {
                            Err(e)
                        }
                        Terminal::Cancelled => Err(CoordError::Other("cancelled".into())),
                    };
                    self.finish(c, r, now);
                }
                None => self.aps_done(id, t, now),
            },
        }
    }

    /// `network/permit_join` on one router or all; `include_gp` then opens
    /// the GP commissioning window for as long.
    pub fn permit_join(
        &mut self,
        duration_s: u8,
        target: Option<u16>,
        include_gp: bool,
        now: Millis,
    ) -> Result<J, ApiError> {
        let ticket = self.answer_ticket();
        let then = Then::PermitJoin {
            gp: include_gp.then_some(duration_s),
            at: now,
        };
        let req = Request::PermitJoin { duration_s, target };
        self.control(req, then, Some(ticket), now);
        self.answer(ticket)
    }

    /// Submits a control request; a refusal ends it at once.
    pub(crate) fn control(
        &mut self,
        req: Request,
        then: Then,
        answer: Option<AnswerTo>,
        now: Millis,
    ) {
        match self.transport.submit(req) {
            Ok(id) => {
                self.controls.insert(id, Control { then, answer });
                while let Some((id, o)) = self.transport.completed() {
                    self.on_outcome(id, o, now);
                }
            }
            Err(e) => self.finish(Control { then, answer }, Err(e), now),
        }
    }

    /// A ticket for an API answer that a control request decides; taken
    /// before the request emits any event.
    pub(crate) fn answer_ticket(&mut self) -> AnswerTo {
        self.answers += 1;
        AnswerTo {
            ticket: ANSWER_TICKETS | self.answers,
            mark: self.events.len(),
            drains: self.drains,
        }
    }

    /// The API answer `a`: its result when the control request already
    /// ended, otherwise `null` and the ticket left for the API
    /// ([`Self::take_deferred`]).
    pub(crate) fn answer(&mut self, a: AnswerTo) -> Result<J, ApiError> {
        let i = self
            .events
            .iter()
            .position(|e| matches!(e, HubEvent::Answer { ticket, .. } if *ticket == a.ticket));
        match i.map(|i| self.events.remove(i)) {
            Some(HubEvent::Answer { result, .. }) => result,
            _ => {
                self.deferred = Some(a.ticket);
                Ok(J::Null)
            }
        }
    }

    /// The ticket whose [`HubEvent::Answer`] answers the request just
    /// handled (its return value is a placeholder).
    pub(crate) fn take_deferred(&mut self) -> Option<u64> {
        self.deferred.take()
    }

    fn finish(&mut self, c: Control, r: Result<(), CoordError>, now: Millis) {
        let Control { then, answer } = c;
        let start = self.events.len();
        let busy = |e: CoordError| ApiError::new("busy", e.to_string());
        let result = match then {
            Then::Nothing => None,
            Then::Membership { group, tail } => self.membership_done(group, tail, r.err(), now),
            Then::Configure { device, key } => {
                let full = r.as_ref().is_err_and(|e| {
                    matches!(
                        e.meaning(),
                        CoordError::Backend {
                            kind: hub_coord::Kind::TableFull,
                            ..
                        }
                    )
                });
                self.group_table_full |= full;
                let status = match (&r, full) {
                    (Ok(()), _) => "SUCCESS",
                    (_, true) => "TABLE_FULL",
                    _ => "FAILURE",
                };
                self.configure_done(&device, &key, r.is_ok(), status.into(), now);
                None
            }
            Then::PermitJoin { gp, at } => match (r, gp) {
                (Err(e), _) => Some(Err(busy(e))),
                (Ok(()), Some(d)) => {
                    self.gp_open(u16::from(d), true, false, answer, at);
                    None
                }
                (Ok(()), None) => Some(Ok(J::Null)),
            },
            Then::GpOpen {
                duration_s,
                involve_proxies,
                exit_on_first_pairing,
                at,
            } => Some(r.map_err(busy).map(|()| {
                self.gp_window_opened(duration_s, involve_proxies, exit_on_first_pairing, at);
                J::Null
            })),
            Then::GpClose => Some(r.map_err(busy).map(|()| {
                self.gp_window_closed(1, now);
                J::Null
            })),
            Then::GpConfig(s) => Some(r.map_err(busy).map(|()| self.gp_sink_set(s))),
            Then::GpRemove(removed) => Some(r.map_err(busy).map(|()| {
                match removed {
                    GpRemoved::Entry(id) => self.remove_device_entry(&id),
                    GpRemoved::Device(id) => self.forget_device(&id),
                }
                J::Null
            })),
            Then::GpRemote(w) => {
                // Ended without a TSN: nothing to wait for. A failure of the
                // first send fails the binding.
                if let (Err(e), Some(id), 0) = (r, w.binding, w.retries) {
                    self.binding_failed(id, None, format!("transport: {e}"));
                }
                self.gp_early_gc();
                None
            }
        };
        if let (Some(a), Some(result)) = (answer, result) {
            // Before the events of the request, as when the handler returned
            // it; once those went out, before the events of what follows.
            let at = if a.drains == self.drains {
                a.mark
            } else {
                start
            };
            let at = at.min(self.events.len());
            let ticket = a.ticket;
            self.events.insert(at, HubEvent::Answer { ticket, result });
        }
    }

    /// A remote GP request accepted: its wait is keyed by the ZCL TSN from
    /// now on, and answers that came first are handled.
    fn gp_accepted(&mut self, id: RequestId, zcl_tsn: Option<u8>, now: Millis) {
        if !matches!(self.controls.get(&id), Some(c) if matches!(c.then, Then::GpRemote(_))) {
            return;
        }
        let Some(Control {
            then: Then::GpRemote(w),
            ..
        }) = self.controls.remove(&id)
        else {
            return;
        };
        if let Some(t) = zcl_tsn {
            self.gp.remote.insert(t, *w);
            let (due, keep) = std::mem::take(&mut self.gp.early)
                .into_iter()
                .partition(|e| remote_tsn(e) == Some(t));
            self.gp.early = keep;
            for ev in due {
                self.on_gp_event(ev, now);
            }
        }
        self.gp_early_gc();
    }

    /// A remote GP answer that may belong to a request not accepted yet
    /// (its TSN unknown) is kept for it: `None`.
    pub(crate) fn gp_early(&mut self, ev: GpEvent) -> Option<GpEvent> {
        match remote_tsn(&ev) {
            Some(t) if !self.gp.remote.contains_key(&t) && self.gp_unaccepted() => {
                if self.gp.early.len() >= MAX_EARLY {
                    self.gp.early.pop_front();
                }
                self.gp.early.push_back(ev);
                None
            }
            _ => Some(ev),
        }
    }

    fn gp_unaccepted(&self) -> bool {
        self.controls
            .values()
            .any(|c| matches!(c.then, Then::GpRemote(_)))
    }

    fn gp_early_gc(&mut self) {
        if !self.gp_unaccepted() {
            self.gp.early.clear();
        }
    }
}
