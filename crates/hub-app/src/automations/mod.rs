//! Local automations: rules (triggers → conditions → actions) and
//! schedules that run on the hub itself, without Home Assistant or the
//! internet (device-support 14.8).
//!
//! The evaluator is sans-IO like the engine: [`Automations`] sees the
//! engine's events ([`Automations::observe`]), a 1 s tick and API requests,
//! and acts through a [`World`] (entity states, commands, other API
//! requests). It runs on the app thread and never blocks: a `delay`
//! suspends the run until a monotonic deadline. Everything is bounded:
//! rules, triggers, conditions and actions per rule, concurrent runs,
//! traces, runs per minute per rule, and the causal chain of rules that
//! trigger each other (a rule that would trigger itself, directly or
//! through others, is stopped as a loop).
//!
//! Schedules (`time` triggers, `time`/`weekday` conditions) use the wall
//! clock and are suspended while it is not synchronised
//! ([`Clock::wall_s`] is `None`); delays use the monotonic clock.

pub mod cron;
pub mod rule;

use std::collections::VecDeque;

use hub_devices::device::Millis;
use hub_devices::hub::{ApiError, HubEvent};
use log::warn;
use serde_json::{Map, Value as J, json};

use cron::Local;
use rule::{Action, Body, Cond, MAX_RULES, Mode, Op, Rule, StateRef, Trigger};

/// Store key prefix: `auto/<id>` → the rule's JSON.
pub const PREFIX: &str = "auto/";
/// Concurrent runs (waiting in a delay) on the hub.
pub const MAX_RUNS: usize = 16;
/// Traces kept (all rules together).
pub const MAX_TRACES: usize = 20;
/// Rules in one causal chain (A's command triggers B, B's triggers C …).
pub const MAX_CHAIN: usize = 4;
/// How long a state change or event of an entity counts as caused by the
/// rule that last sent it a command, ms.
pub const CAUSE_MS: u64 = 10_000;
/// Free heap a rule change needs (a hub without PSRAM; each rule takes
/// about 1.2 KB, each trace with its waiting run as much).
pub const MIN_FREE_HEAP: usize = 48 * 1024;
/// Triggers of one rule deferred while its run waits for a request
/// action (a few milliseconds); more are dropped (trace
/// `deferred_overflow`).
pub const MAX_DEFERRED: usize = 16;
/// Deferred triggers of all rules together.
const MAX_DEFERRED_ALL: usize = 64;
/// `automations/run` answers of deferred manual runs kept for the app.
const MAX_LATE: usize = 16;
const MAX_CAUSES: usize = 64;
const MAX_TICKETS: usize = 64;

/// Time as the automations see it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Clock {
    /// Monotonic ms since the app started (delays, rate limits).
    pub mono_ms: u64,
    /// The API's clock, ms (trace times, as `last_seen`).
    pub now_ms: Millis,
    /// Unix time, s, once the wall clock is synchronised.
    pub wall_s: Option<i64>,
    /// Local time offset, s.
    pub tz_offset_s: i64,
}

impl Clock {
    /// Local calendar time, when synchronised.
    pub fn local(&self) -> Option<Local> {
        self.wall_s.map(|s| Local::at(s, self.tz_offset_s))
    }

    fn t(&self) -> f64 {
        self.now_ms as f64 / 1000.0
    }
}

/// What the automations act on.
pub trait World {
    /// The state message of an entity (4.5).
    fn state(&self, uid: &str) -> Option<J>;
    /// A device's availability.
    fn available(&self, device: &str) -> Option<bool>;
    /// `entities/command`: `(key, status)` as the hub acknowledges it (`ok`,
    /// or `pending`/`queued` with [`Automations::command_result`] for `key`
    /// to follow).
    fn command(&mut self, uid: &str, command: &str, params: &J) -> Result<(u64, &'static str), ApiError>;
    /// Any other allowed API request (`rule::REQUEST_PREFIXES`): `(key,
    /// result)`; no result yet when it follows from what the coordinator
    /// still has to do: the run waits for [`Automations::request_result`]
    /// for `key`.
    fn request(&mut self, req: &J) -> Result<(u64, Option<J>), ApiError>;
}

#[derive(Debug, Default)]
struct Runtime {
    window: u64,
    in_window: u32,
    runs: u32,
    last: Option<(f64, &'static str)>,
}

#[derive(Debug)]
struct Run {
    id: u32,
    rule: String,
    actions: Vec<Action>,
    pc: usize,
    wake: Option<u64>,
    /// Waits for the result of its request action: `(key, action index)`.
    waits: Option<(u64, usize)>,
    chain: Vec<String>,
    errors: u32,
}

/// One run's trace. The JSON parts are kept as text: a `Value` object is a
/// B-tree node of about half a kilobyte, too much for 20 traces on a hub
/// without PSRAM.
#[derive(Debug)]
struct Trace {
    run: u32,
    rule: String,
    time: f64,
    trigger: String,
    chain: Vec<String>,
    conditions: String,
    /// Action index and its result object.
    actions: Vec<(usize, String)>,
    result: &'static str,
    count: u32,
}

fn parse(text: &str) -> J {
    serde_json::from_str(text).unwrap_or(J::Null)
}

impl Trace {
    fn json(&self) -> J {
        json!({
            "run": self.run, "automation": self.rule, "time": self.time, "trigger": parse(&self.trigger),
            "chain": self.chain, "conditions": parse(&self.conditions),
            "actions": self.actions.iter().map(|(_, a)| parse(a)).collect::<Vec<_>>(),
            "result": self.result, "count": self.count,
        })
    }
}

/// A trigger that came while a run of its rule waited for the result of a
/// request action: it fires once that run goes on, as it would have after
/// a run that finished in place. Its run ID is given at once.
#[derive(Debug)]
struct Deferred {
    run: u32,
    rule: String,
    /// The trigger as text (a `Value` object takes half a kilobyte).
    trigger: String,
    chain: Vec<String>,
    check: bool,
    manual: bool,
}

/// The automations of a hub (see the module docs).
#[derive(Debug, Default)]
pub struct Automations {
    rules: VecMap<String, Rule>,
    rt: VecMap<String, Runtime>,
    runs: Vec<Run>,
    traces: VecDeque<Trace>,
    next_run: u32,
    tickets: VecMap<u64, (u32, usize)>,
    causes: VecMap<String, (Vec<String>, u64)>,
    /// Watched state values (`uid` or `uid\x1fattribute`).
    last: VecMap<String, J>,
    /// Watched devices' availability.
    avail: VecMap<String, bool>,
    minute: Option<i64>,
    started: bool,
    /// Triggers of rules whose run waits for a request action.
    deferred: Vec<Deferred>,
    /// Deferred manual runs that fired (or were dropped): `(run, started,
    /// rule)`, for their `automations/run` answer.
    late: Vec<(u32, bool, String)>,
    /// Events for the `automations` topic.
    pub events: Vec<J>,
    /// Rule IDs to write to the store (or delete, when gone), sorted.
    pub dirty: Vec<String>,
    /// Free heap, when the platform tells it: new and changed rules are
    /// refused below [`MIN_FREE_HEAP`].
    pub free_heap: Option<usize>,
    /// Rules cannot be stored (the hub's network records are not its own):
    /// enable and disable actions fail.
    pub read_only: bool,
    /// The stored rules were read (the store's gate did not hide them).
    pub loaded: bool,
}

fn key(s: &StateRef) -> String {
    match &s.attribute {
        Some(a) => format!("{}\x1f{a}", s.uid),
        None => s.uid.clone(),
    }
}

fn value_of(msg: &J, attribute: &Option<String>) -> J {
    match attribute {
        None => msg.get("state"),
        Some(a) => msg.get("attributes").and_then(|x| x.get(a)),
    }
    .cloned()
    .unwrap_or(J::Null)
}

fn num(v: &J) -> Option<f64> {
    match v {
        J::Number(n) => n.as_f64(),
        J::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn boolish(v: &J) -> Option<bool> {
    match v {
        J::Bool(b) => Some(*b),
        J::String(s) => match s.as_str() {
            "on" | "true" | "open" => Some(true),
            "off" | "false" | "closed" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Equality as a user means it: numbers by value (`"21.5"` = 21.5),
/// `true` = `"on"`, `false` = `"off"`.
pub fn loose_eq(a: &J, b: &J) -> bool {
    if a == b {
        return true;
    }
    if (a.is_number() || b.is_number())
        && let (Some(x), Some(y)) = (num(a), num(b))
    {
        return (x - y).abs() < 1e-9;
    }
    if (a.is_boolean() || b.is_boolean())
        && let (Some(x), Some(y)) = (boolish(a), boolish(b))
    {
        return x == y;
    }
    false
}

fn holds(op: Op, v: &J, x: &J) -> bool {
    match op {
        _ if v.is_null() => false,
        Op::Equals => loose_eq(v, x),
        Op::NotEquals => !loose_eq(v, x),
        Op::Above => num(v).zip(num(x)).is_some_and(|(v, x)| v > x),
        Op::Below => num(v).zip(num(x)).is_some_and(|(v, x)| v < x),
        Op::Changed => false,
    }
}

/// Whether a state trigger fires for `prev` → `new`: `changed` on any
/// change between known values; the comparisons when they start to hold
/// (an unknown previous value does not hold).
pub fn fires(s: &StateRef, prev: &J, new: &J) -> bool {
    match s.op {
        Op::Changed => !prev.is_null() && !new.is_null() && !loose_eq(prev, new),
        op => holds(op, new, &s.value) && !holds(op, prev, &s.value),
    }
}

fn param<'a>(v: &'a J, k: &str) -> Result<&'a str, ApiError> {
    v.get(k).and_then(J::as_str).ok_or_else(|| ApiError::new("invalid_format", format!("`{k}` is missing")))
}

impl Automations {
    /// Restores the stored rules (`auto/<id>` records); returns how many.
    pub fn load(&mut self, records: Vec<(String, Vec<u8>)>) -> usize {
        for (k, v) in records {
            let id = k.strip_prefix(PREFIX).unwrap_or(&k);
            match serde_json::from_slice::<J>(&v).map_err(|e| ApiError::new("invalid_format", e.to_string())).and_then(|j| rule::parse(&j, id)) {
                Ok(r) if self.rules.len() < MAX_RULES => {
                    self.rules.insert(r.id.clone(), r);
                }
                Ok(_) => warn!("automations: {k}: more than {MAX_RULES} rules"),
                Err(e) => warn!("automations: {k}: {}", e.message),
            }
        }
        self.rules.len()
    }

    /// The rule by ID.
    pub fn rule(&self, id: &str) -> Option<&Rule> {
        self.rules.get(id)
    }

    /// Rules, by ID.
    pub fn rules(&self) -> impl Iterator<Item = &Rule> {
        self.rules.values()
    }

    /// Reads the current values the triggers compare against (after
    /// loading or changing rules, and once the network records loaded):
    /// new ones, and unknown ones (a device or its last values restored
    /// since).
    pub fn prime(&mut self, w: &dyn World) {
        let mut keys = Vec::new();
        let mut devs = Vec::new();
        for r in self.rules.values() {
            for t in &r.triggers {
                match t {
                    Trigger::State(s) => {
                        let k = key(s);
                        if self.last.get(&k).is_none_or(J::is_null) {
                            let v = w.state(&s.uid).map(|m| value_of(&m, &s.attribute)).unwrap_or(J::Null);
                            self.last.insert(k.clone(), v);
                        }
                        keys.push(k);
                    }
                    Trigger::Availability { device, .. } => {
                        if !self.avail.contains_key(device)
                            && let Some(a) = w.available(device)
                        {
                            self.avail.insert(device.clone(), a);
                        }
                        devs.push(device.clone());
                    }
                    _ => {}
                }
            }
        }
        self.last.retain(|k, _| keys.contains(k));
        self.avail.retain(|k, _| devs.contains(k));
    }

    /// When the next delay ends (monotonic ms).
    pub fn next_wake(&self) -> Option<u64> {
        self.runs.iter().filter_map(|r| r.wake).min()
    }

    /// Engine events: state, event and availability triggers.
    pub fn observe(&mut self, events: &[HubEvent], w: &mut dyn World, clk: &Clock) {
        if self.rules.is_empty() {
            return;
        }
        for ev in events {
            match ev {
                HubEvent::EntityState(s) => self.on_state(s, w, clk),
                HubEvent::EntityEvent { uid, event_type, attributes, .. } => {
                    self.on_event(uid, event_type, attributes, w, clk)
                }
                HubEvent::DeviceAdded(id) | HubEvent::DeviceUpdated(id) => self.on_device(id, w, clk),
                _ => {}
            }
        }
    }

    fn chain_of(&self, uid: &str, clk: &Clock) -> Vec<String> {
        self.causes.get(uid).filter(|(_, until)| *until > clk.mono_ms).map(|(c, _)| c.clone()).unwrap_or_default()
    }

    fn on_state(&mut self, msg: &J, w: &mut dyn World, clk: &Clock) {
        let Some(uid) = msg.get("uid").and_then(J::as_str) else { return };
        let mut fire = Vec::new();
        let mut seen: Vec<(String, J)> = Vec::new();
        for r in self.rules.values().filter(|r| r.enabled) {
            let mut hit = None;
            for t in &r.triggers {
                let Trigger::State(s) = t else { continue };
                if s.uid != uid {
                    continue;
                }
                let new = value_of(msg, &s.attribute);
                let k = key(s);
                let prev = self.last.get(&k).cloned().unwrap_or(J::Null);
                if hit.is_none() && fires(s, &prev, &new) {
                    let mut h = json!({"type": "state", "uid": uid, "from": prev, "to": new});
                    if let Some(a) = &s.attribute {
                        h["attribute"] = a.clone().into();
                    }
                    hit = Some(h);
                }
                // An entity never reported (restored or made again: its
                // value follows) keeps the last known value; a reported
                // unknown (invalid, unmapped) is the previous value then.
                if !new.is_null() || msg.get("last_reported").is_some_and(|t| !t.is_null()) {
                    seen.push((k, new));
                }
            }
            if let Some(h) = hit {
                fire.push((r.id.clone(), h));
            }
        }
        for (k, v) in seen {
            self.last.insert(k, v);
        }
        let chain = self.chain_of(uid, clk);
        for (id, t) in fire {
            self.fire(&id, t, chain.clone(), w, clk, true);
        }
    }

    fn on_event(&mut self, uid: &str, ty: &str, attrs: &Map<String, J>, w: &mut dyn World, clk: &Clock) {
        let fire: Vec<String> = self
            .rules
            .values()
            .filter(|r| r.enabled)
            .filter(|r| {
                r.triggers.iter().any(|t| {
                    matches!(t, Trigger::Event { uid: u, event_type } if u == uid && event_type.as_deref().is_none_or(|e| e == ty))
                })
            })
            .map(|r| r.id.clone())
            .collect();
        let chain = self.chain_of(uid, clk);
        for id in fire {
            let t = json!({"type": "event", "uid": uid, "event_type": ty, "attributes": attrs});
            self.fire(&id, t, chain.clone(), w, clk, true);
        }
    }

    fn on_device(&mut self, id: &str, w: &mut dyn World, clk: &Clock) {
        let watched = self.rules.values().any(|r| r.triggers.iter().any(|t| matches!(t, Trigger::Availability { device, .. } if device == id)));
        let Some(now) = w.available(id).filter(|_| watched) else { return };
        if self.avail.insert(id.to_string(), now).is_none_or(|prev| prev == now) {
            return;
        }
        let fire: Vec<String> = self
            .rules
            .values()
            .filter(|r| r.enabled)
            .filter(|r| {
                r.triggers.iter().any(|t| matches!(t, Trigger::Availability { device, to } if device == id && to.is_none_or(|x| x == now)))
            })
            .map(|r| r.id.clone())
            .collect();
        for rid in fire {
            self.fire(&rid, json!({"type": "availability", "device": id, "available": now}), Vec::new(), w, clk, true);
        }
    }

    /// The late result of a command (see [`World::command`]) for its trace.
    pub fn command_result(&mut self, key: u64, result: Result<(), ApiError>) {
        let Some((run, pc)) = self.tickets.remove(&key) else { return };
        let Some(t) = self.traces.iter_mut().find(|t| t.run == run) else { return };
        if let Some((_, text)) = t.actions.iter_mut().find(|(i, _)| *i == pc) {
            let mut a = parse(text);
            match result {
                Ok(_) => a["result"] = "ok".into(),
                Err(e) => {
                    a["result"] = "error".into();
                    a["error"] = json!({"code": e.code, "message": e.message});
                    if t.result == "done" {
                        t.result = "error";
                    }
                }
            }
            *text = a.to_string();
        }
    }

    /// The hub started (its NCP session is up): `start` triggers, once.
    pub fn started(&mut self, w: &mut dyn World, clk: &Clock) {
        if std::mem::replace(&mut self.started, true) {
            return;
        }
        let fire: Vec<String> =
            self.rules.values().filter(|r| r.enabled && r.triggers.contains(&Trigger::Start)).map(|r| r.id.clone()).collect();
        for id in fire {
            self.fire(&id, json!({"type": "start"}), Vec::new(), w, clk, true);
        }
    }

    /// Every second: ended delays, then the schedules (each local minute
    /// once; nothing is caught up after the clock jumped).
    pub fn tick(&mut self, w: &mut dyn World, clk: &Clock) {
        self.resume(w, clk);
        let Some(t) = clk.local() else {
            self.minute = None;
            return;
        };
        match self.minute {
            Some(p) if t.minute > p => {
                self.minute = Some(t.minute);
                let fire: Vec<(String, J)> = self
                    .rules
                    .values()
                    .filter(|r| r.enabled)
                    .filter_map(|r| {
                        r.triggers.iter().find_map(|tr| match tr {
                            Trigger::Time(c) if c.matches(&t) => Some((r.id.clone(), json!({"type": "time", "local": t.text()}))),
                            _ => None,
                        })
                    })
                    .collect();
                for (id, tr) in fire {
                    self.fire(&id, tr, Vec::new(), w, clk, true);
                }
            }
            // The same minute, or the clock stepped back by a minute.
            Some(p) if t.minute + 1 >= p => {}
            _ => self.minute = Some(t.minute),
        }
        let now = clk.mono_ms;
        self.causes.retain(|_, (_, until)| *until > now);
    }

    /// Continues the runs whose delay ended, earliest first.
    pub fn resume(&mut self, w: &mut dyn World, clk: &Clock) {
        while let Some(run) =
            self.runs.iter().filter(|r| r.wake.is_some_and(|t| t <= clk.mono_ms)).min_by_key(|r| (r.wake, r.id)).map(|r| r.id)
        {
            if let Some(r) = self.runs.iter_mut().find(|r| r.id == run) {
                r.wake = None;
            }
            self.step(run, w, clk);
        }
    }

    fn new_run(&mut self) -> u32 {
        self.next_run = self.next_run.wrapping_add(1).max(1);
        self.next_run
    }

    /// A trace for a run that did not start (the newest trace of the same
    /// rule and outcome is counted up instead of a new one).
    fn skipped(&mut self, id: &str, trigger: J, chain: Vec<String>, conditions: Vec<J>, result: &'static str, clk: &Clock) {
        let (trigger, conditions) = (trigger.to_string(), J::Array(conditions).to_string());
        if let Some(t) = self.traces.back_mut().filter(|t| t.rule == id && t.result == result) {
            t.count += 1;
            t.time = clk.t();
            t.trigger = trigger;
            t.conditions = conditions;
            return;
        }
        let run = self.new_run();
        self.push_trace(Trace { run, rule: id.into(), time: clk.t(), trigger, chain, conditions, actions: Vec::new(), result, count: 1 });
        self.finished(run);
    }

    fn push_trace(&mut self, t: Trace) {
        if self.traces.len() >= MAX_TRACES {
            self.traces.pop_front();
        }
        self.traces.push_back(t);
    }

    fn finished(&mut self, run: u32) {
        let Some(t) = self.traces.iter().find(|t| t.run == run) else { return };
        let ev = json!({"topic": "automation_run", "run": run, "automation": t.rule, "result": t.result, "trigger": parse(&t.trigger), "time": t.time});
        if t.result != "running" {
            self.rt.or_default(&t.rule).last = Some((t.time, t.result));
        }
        self.events.push(ev);
    }

    fn eval(&self, c: &Cond, w: &dyn World, clk: &Clock) -> bool {
        match c {
            Cond::State(s) => holds(s.op, &w.state(&s.uid).map(|m| value_of(&m, &s.attribute)).unwrap_or(J::Null), &s.value),
            Cond::Time { after, before } => clk.local().is_some_and(|t| {
                let m = t.minute_of_day;
                match (after, before) {
                    (Some(a), Some(b)) if a > b => m >= *a || m < *b,
                    (a, b) => a.is_none_or(|a| m >= a) && b.is_none_or(|b| m < b),
                }
            }),
            Cond::Weekday(days) => clk.local().is_some_and(|t| days & (1 << (t.weekday - 1)) != 0),
            Cond::And(v) => v.iter().all(|c| self.eval(c, w, clk)),
            Cond::Or(v) => v.iter().any(|c| self.eval(c, w, clk)),
            Cond::Not(v) => !v.iter().any(|c| self.eval(c, w, clk)),
        }
    }

    /// Evaluates the conditions in order up to the first that fails.
    fn conditions(&self, conditions: &[Cond], w: &dyn World, clk: &Clock) -> (bool, Vec<J>) {
        let mut out = Vec::new();
        for (i, c) in conditions.iter().enumerate() {
            let ok = self.eval(c, w, clk);
            let mut e = json!({"i": i, "result": ok});
            match c {
                Cond::State(s) => e["value"] = w.state(&s.uid).map(|m| value_of(&m, &s.attribute)).unwrap_or(J::Null),
                Cond::Time { .. } | Cond::Weekday(_) => {
                    e["local"] = clk.local().map_or(J::Null, |t| t.text().into());
                    if clk.wall_s.is_none() {
                        e["note"] = "time not synchronised".into();
                    }
                }
                _ => {}
            }
            out.push(e);
            if !ok {
                return (false, out);
            }
        }
        (true, out)
    }

    /// Starts a run of rule `id` unless a guard stops it; returns the run
    /// (0: none started). `check` evaluates the conditions. While a run of
    /// the rule waits for a request action, the trigger waits for it
    /// ([`Self::pending`] tells such a run).
    fn fire(&mut self, id: &str, trigger: J, chain: Vec<String>, w: &mut dyn World, clk: &Clock, check: bool) -> u32 {
        if !self.rules.contains_key(id) {
            return 0;
        }
        if self.runs.iter().any(|r| r.rule == id && r.waits.is_some()) || self.deferred.iter().any(|d| d.rule == id) {
            let mine = self.deferred.iter().filter(|d| d.rule == id).count();
            if mine >= MAX_DEFERRED || self.deferred.len() >= MAX_DEFERRED_ALL {
                self.skipped(id, trigger, chain, Vec::new(), "deferred_overflow", clk);
                return 0;
            }
            let run = self.new_run();
            let manual = trigger["type"] == "manual";
            self.deferred.push(Deferred { run, rule: id.into(), trigger: trigger.to_string(), chain, check, manual });
            return run;
        }
        self.start(id, trigger, chain, w, clk, check, None)
    }

    /// The deferred triggers of rule `rule`, in order, while none of its
    /// runs waits for a request action.
    fn fire_deferred(&mut self, rule: &str, w: &mut dyn World, clk: &Clock) {
        while !self.runs.iter().any(|r| r.rule == rule && r.waits.is_some()) {
            let Some(i) = self.deferred.iter().position(|d| d.rule == rule) else { break };
            let d = self.deferred.remove(i);
            let started = self.start(&d.rule, parse(&d.trigger), d.chain, w, clk, d.check, Some(d.run)) > 0;
            if d.manual {
                self.late_answer(d.run, started, d.rule);
            }
        }
    }

    fn late_answer(&mut self, run: u32, started: bool, rule: String) {
        if self.late.len() >= MAX_LATE {
            self.late.remove(0);
        }
        self.late.push((run, started, rule));
    }

    /// [`Self::fire`] without waiting; `run`: the ID a deferred trigger got.
    #[allow(clippy::too_many_arguments)]
    fn start(
        &mut self,
        id: &str,
        trigger: J,
        chain: Vec<String>,
        w: &mut dyn World,
        clk: &Clock,
        check: bool,
        run: Option<u32>,
    ) -> u32 {
        let Some((mode, per_min)) = self.rules.get(id).map(|r| (r.mode, r.max_per_min)) else { return 0 };
        let manual = trigger["type"] == "manual";
        let rt = self.rt.or_default(id);
        if clk.mono_ms.saturating_sub(rt.window) >= 60_000 {
            rt.window = clk.mono_ms;
            rt.in_window = 0;
        }
        let guard = if chain.iter().any(|c| c == id) {
            Some("loop")
        } else if chain.len() >= MAX_CHAIN {
            Some("too_deep")
        } else if !manual && rt.in_window >= per_min {
            Some("rate_limited")
        } else if mode == Mode::Single && self.runs.iter().any(|x| x.rule == id) {
            Some("already_running")
        } else {
            None
        };
        if let Some(g) = guard {
            if g == "loop" {
                warn!("automations: {id}: loop through {chain:?} stopped");
            }
            self.skipped(id, trigger, chain, Vec::new(), g, clk);
            return 0;
        }
        let Body { conditions, actions } = self.rules.get(id).map(Rule::body).unwrap_or_default();
        let (ok, conds) = if check { self.conditions(&conditions, w, clk) } else { (true, Vec::new()) };
        if !ok {
            self.skipped(id, trigger, chain, conds, "conditions_failed", clk);
            return 0;
        }
        if mode == Mode::Restart {
            self.cancel_runs(id, None);
        }
        if self.runs.len() >= MAX_RUNS {
            self.skipped(id, trigger, chain, conds, "busy", clk);
            return 0;
        }
        let rt = self.rt.or_default(id);
        rt.in_window += 1;
        rt.runs += 1;
        let run = run.unwrap_or_else(|| self.new_run());
        self.push_trace(Trace {
            run,
            rule: id.into(),
            time: clk.t(),
            trigger: trigger.to_string(),
            chain: chain.clone(),
            conditions: J::Array(conds).to_string(),
            actions: Vec::new(),
            result: "running",
            count: 1,
        });
        self.runs.push(Run { id: run, rule: id.into(), actions, pc: 0, wake: None, waits: None, chain, errors: 0 });
        self.step(run, w, clk);
        run
    }

    /// Ends the runs of rule `id` (except `keep`) as cancelled, and drops
    /// its deferred triggers (the rule changed).
    fn cancel(&mut self, id: &str, keep: Option<u32>) {
        let (dropped, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.deferred).into_iter().partition(|d| d.rule == id);
        self.deferred = kept;
        for d in dropped.into_iter().filter(|d| d.manual) {
            self.late_answer(d.run, false, d.rule);
        }
        self.cancel_runs(id, keep);
    }

    /// Ends the runs of rule `id` (except `keep`) as cancelled.
    fn cancel_runs(&mut self, id: &str, keep: Option<u32>) {
        let gone: Vec<u32> = self.runs.iter().filter(|r| r.rule == id && Some(r.id) != keep).map(|r| r.id).collect();
        self.runs.retain(|r| !gone.contains(&r.id));
        for run in gone {
            self.end(run, "cancelled");
        }
    }

    fn end(&mut self, run: u32, result: &'static str) {
        self.runs.retain(|r| r.id != run);
        if let Some(t) = self.traces.iter_mut().find(|t| t.run == run) {
            t.result = result;
        }
        self.finished(run);
    }

    /// Executes actions of run `run` up to the next delay or the end.
    fn step(&mut self, run: u32, w: &mut dyn World, clk: &Clock) {
        loop {
            let Some(r) = self.runs.iter_mut().find(|r| r.id == run) else { return };
            let pc = r.pc;
            let Some(a) = r.actions.get(pc).cloned() else {
                let res = if r.errors > 0 { "error" } else { "done" };
                self.end(run, res);
                return;
            };
            r.pc += 1;
            let (rid, chain) = (r.rule.clone(), r.chain.clone());
            let mut out = json!({"i": pc});
            let mut wait = None;
            let mut failed = None;
            let mut waits = None;
            match &a {
                Action::Command { uid, command, params } => {
                    out["type"] = "command".into();
                    match w.command(uid, command, &parse(params)) {
                        Ok((ticket, status)) => {
                            out["result"] = status.into();
                            if status != "ok" {
                                if self.tickets.len() >= MAX_TICKETS {
                                    self.tickets.0.remove(0);
                                }
                                self.tickets.insert(ticket, (run, pc));
                            }
                            self.caused(uid, &chain, &rid, clk);
                        }
                        Err(e) => failed = Some(e),
                    }
                }
                Action::Request(req) => {
                    out["type"] = "request".into();
                    match w.request(&parse(req)) {
                        Ok((_, Some(_))) => out["result"] = "ok".into(),
                        Ok((key, None)) => waits = Some(key),
                        Err(e) => failed = Some(e),
                    }
                }
                Action::Delay(ms) => {
                    out["type"] = "delay".into();
                    out["result"] = "ok".into();
                    wait = Some(clk.mono_ms + ms);
                }
                Action::Notify { message, data, webhook } => {
                    out["type"] = "notify".into();
                    out["result"] = "ok".into();
                    if webhook.is_some() {
                        out["note"] = "webhooks are not sent yet".into();
                    }
                    self.events.push(json!({"topic": "automation_notify", "automation": rid, "run": run, "message": message, "data": parse(data), "time": clk.t()}));
                }
                Action::Enable { rule, enabled } => {
                    out["type"] = if *enabled { "enable" } else { "disable" }.into();
                    if self.read_only {
                        failed = Some(ApiError::new("registry_read_only", "the device registry is not writable"));
                    } else if self.set_enabled(rule, *enabled, Some(run), clk) {
                        out["result"] = "ok".into();
                    } else {
                        failed = Some(ApiError::new("not_found", format!("no automation {rule}")));
                    }
                }
            }
            if let Some(key) = waits {
                if let Some(r) = self.runs.iter_mut().find(|r| r.id == run) {
                    r.waits = Some((key, pc));
                }
                return;
            }
            self.action_done(run, pc, out, failed);
            if let Some(at) = wait {
                if let Some(r) = self.runs.iter_mut().find(|r| r.id == run) {
                    r.wake = Some(at);
                }
                return;
            }
        }
    }

    /// The action's trace entry `out`, with its error.
    fn action_done(&mut self, run: u32, pc: usize, mut out: J, failed: Option<ApiError>) {
        if let Some(e) = failed {
            out["result"] = "error".into();
            out["error"] = json!({"code": e.code, "message": e.message});
            if let Some(r) = self.runs.iter_mut().find(|r| r.id == run) {
                r.errors += 1;
            }
        }
        if let Some(t) = self.traces.iter_mut().find(|t| t.run == run) {
            t.actions.push((pc, out.to_string()));
        }
    }

    /// The result of a request action a run waits for ([`World::request`]):
    /// recorded, the run goes on, then the triggers of its rule that came
    /// meanwhile fire.
    pub fn request_result(&mut self, key: u64, result: Result<J, ApiError>, w: &mut dyn World, clk: &Clock) {
        let Some(r) = self.runs.iter_mut().find(|r| r.waits.is_some_and(|(k, _)| k == key)) else { return };
        let Some((_, pc)) = r.waits.take() else { return };
        let (run, rule) = (r.id, r.rule.clone());
        let mut out = json!({"i": pc, "type": "request"});
        let failed = match result {
            Ok(_) => {
                out["result"] = "ok".into();
                None
            }
            Err(e) => Some(e),
        };
        self.action_done(run, pc, out, failed);
        self.step(run, w, clk);
        self.fire_deferred(&rule, w, clk);
    }

    /// Keys of the request actions runs wait for.
    pub fn waiting_requests(&self) -> Vec<u64> {
        self.runs.iter().filter_map(|r| r.waits.map(|(k, _)| k)).collect()
    }

    /// Whether run `run` waits for a request action, or for a run of its
    /// rule that does (a deferred trigger).
    pub fn pending(&self, run: u32) -> bool {
        self.runs.iter().any(|r| r.id == run && r.waits.is_some()) || self.deferred.iter().any(|d| d.run == run)
    }

    /// The `automations/run` result of run `run` once it is not
    /// [`pending`](Self::pending) any more: as the request answers at once
    /// (`run` null and the rule's last trace when a guard stopped it).
    pub fn run_result(&mut self, run: u32) -> J {
        if let Some(i) = self.late.iter().position(|(r, ..)| *r == run)
            && let (_, false, rule) = self.late.remove(i)
        {
            let t = self.traces.iter().rev().find(|t| t.rule == rule).map_or(J::Null, Trace::json);
            return json!({"run": null, "trace": t});
        }
        let t = self.traces.iter().rev().find(|t| t.run == run).map_or(J::Null, Trace::json);
        json!({"run": run, "trace": t})
    }

    /// Changes of `uid` in the next [`CAUSE_MS`] count as caused by rule
    /// `rule` (after the rules in `chain`).
    fn caused(&mut self, uid: &str, chain: &[String], rule: &str, clk: &Clock) {
        let now = clk.mono_ms;
        if self.causes.len() >= MAX_CAUSES {
            self.causes.retain(|_, (_, until)| *until > now);
            if self.causes.len() >= MAX_CAUSES
                && let Some(k) = self.causes.iter().min_by_key(|(_, (_, u))| *u).map(|(k, _)| k.clone())
            {
                self.causes.remove(&k);
            }
        }
        let mut c = chain.to_vec();
        c.push(rule.to_string());
        self.causes.insert(uid.to_string(), (c, now + CAUSE_MS));
    }

    fn mark(&mut self, id: &str) {
        if let Err(i) = self.dirty.binary_search_by(|x| x.as_str().cmp(id)) {
            self.dirty.insert(i, id.to_string());
        }
    }

    fn set_enabled(&mut self, id: &str, on: bool, keep: Option<u32>, clk: &Clock) -> bool {
        let Some(r) = self.rules.get_mut(id) else { return false };
        if r.enabled != on {
            r.set_enabled(on);
            self.mark(id);
            let v = self.view(id, clk);
            self.events.push(json!({"topic": "automation_updated", "automation": v}));
        }
        if !on {
            self.cancel(id, keep);
        }
        true
    }

    fn view(&self, id: &str, clk: &Clock) -> J {
        let Some(r) = self.rules.get(id) else { return J::Null };
        let mut v = r.json();
        let rt = self.rt.get(id);
        v["running"] = self.runs.iter().any(|x| x.rule == id).into();
        v["runs"] = rt.map_or(0, |x| x.runs).into();
        v["last_run"] = rt.and_then(|x| x.last).map_or(J::Null, |(t, _)| t.into());
        v["last_result"] = rt.and_then(|x| x.last).map_or(J::Null, |(_, r)| r.into());
        if r.scheduled() && clk.wall_s.is_none() {
            v["suspended"] = "time not synchronised: schedules wait for the clock".into();
        }
        v
    }

    fn fresh_id(&self) -> String {
        let n = self.rules.keys().filter_map(|k| k.strip_prefix('r')?.parse::<u32>().ok()).max().unwrap_or(0);
        format!("r{}", n + 1)
    }

    /// `automations/*` requests (`None`: not one of them).
    pub fn request(&mut self, kind: &str, v: &J, w: &mut dyn World, clk: &Clock) -> Option<Result<J, ApiError>> {
        let kind = kind.strip_prefix("automations/")?;
        Some(self.dispatch(kind, v, w, clk))
    }

    fn dispatch(&mut self, kind: &str, v: &J, w: &mut dyn World, clk: &Clock) -> Result<J, ApiError> {
        let missing = |id: &str| ApiError::new("not_found", format!("no automation {id}"));
        Ok(match kind {
            "list" => json!({
                "time_synced": clk.wall_s.is_some(),
                "local_time": clk.local().map(|t| t.text()),
                "rules": self.rules.keys().map(|k| self.view(k, clk)).collect::<Vec<_>>(),
            }),
            "get" => {
                let id = param(v, "automation")?;
                self.rules.get(id).ok_or_else(|| missing(id))?;
                self.view(id, clk)
            }
            "validate" => {
                let r = v.get("rule").unwrap_or(&J::Null);
                let id = r.get("id").and_then(J::as_str).unwrap_or("new");
                match rule::parse(r, id) {
                    Ok(_) => json!({"ok": true, "errors": []}),
                    Err(e) => json!({"ok": false, "errors": [e.message]}),
                }
            }
            "create" | "update" => {
                let r = v.get("rule").ok_or_else(|| ApiError::new("invalid_format", "`rule` is missing"))?;
                let id = if kind == "update" {
                    let id = param(v, "automation")?;
                    self.rules.get(id).ok_or_else(|| missing(id))?;
                    id.to_string()
                } else {
                    let id = r.get("id").and_then(J::as_str).map_or_else(|| self.fresh_id(), str::to_string);
                    if self.rules.contains_key(&id) {
                        return Err(ApiError::new("invalid_value", format!("automation {id} exists")));
                    }
                    if self.rules.len() >= MAX_RULES {
                        return Err(ApiError::new("no_space", format!("at most {MAX_RULES} automations")));
                    }
                    id
                };
                if let Some(f) = self.free_heap.filter(|f| *f < MIN_FREE_HEAP) {
                    return Err(ApiError::new("no_space", format!("low memory ({} KB free)", f / 1024)));
                }
                let parsed = rule::parse(r, &id)?;
                let others: usize = self.rules.iter().filter(|(k, _)| **k != id).map(|(_, x)| x.src.len()).sum();
                if others + parsed.src.len() > rule::MAX_TOTAL_BYTES {
                    return Err(ApiError::new(
                        "no_space",
                        format!("automations above {} KiB together", rule::MAX_TOTAL_BYTES / 1024),
                    ));
                }
                self.cancel(&id, None);
                self.rules.insert(id.clone(), parsed);
                self.prime(w);
                self.mark(&id);
                let out = self.view(&id, clk);
                let topic = if kind == "create" { "automation_added" } else { "automation_updated" };
                self.events.push(json!({"topic": topic, "automation": out}));
                out
            }
            "delete" => {
                let id = param(v, "automation")?.to_string();
                self.rules.remove(&id).ok_or_else(|| missing(&id))?;
                self.cancel(&id, None);
                self.rt.remove(&id);
                self.prime(w);
                self.mark(&id);
                self.events.push(json!({"topic": "automation_removed", "automation": id}));
                J::Null
            }
            "enable" | "disable" => {
                let id = param(v, "automation")?;
                if !self.set_enabled(id, kind == "enable", None, clk) {
                    return Err(missing(id));
                }
                self.view(id, clk)
            }
            "run" => {
                let id = param(v, "automation")?.to_string();
                self.rules.get(&id).ok_or_else(|| missing(&id))?;
                let check = v.get("conditions").and_then(J::as_bool).unwrap_or(false);
                let run = self.fire(&id, json!({"type": "manual"}), Vec::new(), w, clk, check);
                let t = self.traces.iter().rev().find(|t| t.rule == id && (run == 0 || t.run == run)).map_or(J::Null, Trace::json);
                json!({"run": (run > 0).then_some(run), "trace": t})
            }
            "traces" => {
                let id = v.get("automation").and_then(J::as_str);
                let limit = v.get("limit").and_then(J::as_u64).unwrap_or(MAX_TRACES as u64) as usize;
                J::Array(self.traces.iter().rev().filter(|t| id.is_none_or(|i| t.rule == i)).take(limit).map(Trace::json).collect())
            }
            _ => return Err(ApiError::new("invalid_format", format!("unknown request automations/{kind}"))),
        })
    }
}

#[cfg(test)]
mod tests;

/// A small map as a sorted `Vec` (a few dozen entries: far less code than a
/// `BTreeMap` per key and value type).
#[derive(Debug)]
pub struct VecMap<K, V>(Vec<(K, V)>);

impl<K, V> Default for VecMap<K, V> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<K: Ord, V> VecMap<K, V> {
    fn find<Q: Ord + ?Sized>(&self, k: &Q) -> Result<usize, usize>
    where
        K: std::borrow::Borrow<Q>,
    {
        self.0.binary_search_by(|(x, _)| x.borrow().cmp(k))
    }
    fn get<Q: Ord + ?Sized>(&self, k: &Q) -> Option<&V>
    where
        K: std::borrow::Borrow<Q>,
    {
        self.find(k).ok().map(|i| &self.0[i].1)
    }
    fn get_mut<Q: Ord + ?Sized>(&mut self, k: &Q) -> Option<&mut V>
    where
        K: std::borrow::Borrow<Q>,
    {
        self.find(k).ok().map(|i| &mut self.0[i].1)
    }
    fn contains_key<Q: Ord + ?Sized>(&self, k: &Q) -> bool
    where
        K: std::borrow::Borrow<Q>,
    {
        self.find(k).is_ok()
    }
    fn insert(&mut self, k: K, v: V) -> Option<V> {
        match self.find(&k) {
            Ok(i) => Some(std::mem::replace(&mut self.0[i].1, v)),
            Err(i) => {
                self.0.insert(i, (k, v));
                None
            }
        }
    }
    fn remove<Q: Ord + ?Sized>(&mut self, k: &Q) -> Option<V>
    where
        K: std::borrow::Borrow<Q>,
    {
        self.find(k).ok().map(|i| self.0.remove(i).1)
    }
    fn retain(&mut self, mut f: impl FnMut(&K, &mut V) -> bool) {
        self.0.retain_mut(|(k, v)| f(k, v));
    }
    fn len(&self) -> usize {
        self.0.len()
    }
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    fn keys(&self) -> impl Iterator<Item = &K> {
        self.0.iter().map(|(k, _)| k)
    }
    fn values(&self) -> impl Iterator<Item = &V> {
        self.0.iter().map(|(_, v)| v)
    }
    fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.0.iter().map(|(k, v)| (k, v))
    }
}

impl<V: Default> VecMap<String, V> {
    fn or_default(&mut self, k: &str) -> &mut V {
        let i = match self.find(k) {
            Ok(i) => i,
            Err(i) => {
                self.0.insert(i, (k.to_string(), V::default()));
                i
            }
        };
        &mut self.0[i].1
    }
}
