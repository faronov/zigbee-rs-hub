//! Evaluator tests against a scripted world.

use std::collections::BTreeMap;

use hub_devices::hub::{ApiError, HubEvent};
use serde_json::{Map, Value as J, json};

use super::cron::{Cron, Local};
use super::*;

/// Entities as `uid → state`; commands are recorded; `turn_on`/`turn_off`
/// of a `switch.*`-like entity change its state at once (as a device
/// would report it), producing the event for the next `observe`.
#[derive(Default)]
struct Fake {
    states: BTreeMap<String, J>,
    avail: BTreeMap<String, bool>,
    commands: Vec<(String, String, J)>,
    requests: Vec<J>,
    pending: Vec<HubEvent>,
    fail: Option<ApiError>,
    deferred: bool,
    ticket: u64,
    /// Request results follow later ([`Automations::request_result`]).
    requests_wait: bool,
}

impl Fake {
    fn set(&mut self, uid: &str, v: J) -> HubEvent {
        self.states.insert(uid.into(), v.clone());
        HubEvent::EntityState(json!({"uid": uid, "state": v, "attributes": {}, "available": true}))
    }
}

impl World for Fake {
    fn state(&self, uid: &str) -> Option<J> {
        self.states.get(uid).map(|v| json!({"uid": uid, "state": v, "attributes": {"brightness": 7}}))
    }
    fn available(&self, device: &str) -> Option<bool> {
        self.avail.get(device).copied()
    }
    fn command(&mut self, uid: &str, command: &str, params: &J) -> Result<(u64, &'static str), ApiError> {
        if let Some(e) = self.fail.clone() {
            return Err(e);
        }
        self.commands.push((uid.into(), command.into(), params.clone()));
        let new = match command {
            "turn_on" => Some(J::from("on")),
            "turn_off" => Some(J::from("off")),
            "toggle" => Some(J::from(if self.states.get(uid) == Some(&J::from("on")) { "off" } else { "on" })),
            _ => None,
        };
        if let Some(v) = new {
            let ev = self.set(uid, v);
            self.pending.push(ev);
        }
        self.ticket += 1;
        Ok((self.ticket, if self.deferred { "pending" } else { "ok" }))
    }
    fn request(&mut self, req: &J) -> Result<(u64, Option<J>), ApiError> {
        self.requests.push(req.clone());
        Ok((self.requests.len() as u64, (!self.requests_wait).then_some(J::Null)))
    }
}

struct Rig {
    a: Automations,
    w: Fake,
    clk: Clock,
}

impl Rig {
    fn new() -> Self {
        Self { a: Automations::default(), w: Fake::default(), clk: Clock { mono_ms: 1000, now_ms: 1_000_000, ..Clock::default() } }
    }
    fn req(&mut self, kind: &str, v: J) -> Result<J, ApiError> {
        self.a.request(kind, &v, &mut self.w, &self.clk).expect("an automations request")
    }
    fn create(&mut self, rule: J) -> J {
        self.req("automations/create", json!({"rule": rule})).unwrap()
    }
    /// Delivers events and what the world produced in reply, in rounds.
    fn feed(&mut self, evs: Vec<HubEvent>) {
        let mut evs = evs;
        for _ in 0..8 {
            if evs.is_empty() {
                return;
            }
            self.a.observe(&evs, &mut self.w, &self.clk);
            evs = std::mem::take(&mut self.w.pending);
        }
    }
    fn state(&mut self, uid: &str, v: J) {
        let ev = self.w.set(uid, v);
        self.feed(vec![ev]);
    }
    fn advance(&mut self, ms: u64) {
        self.clk.mono_ms += ms;
        self.clk.now_ms += ms;
        if let Some(w) = self.clk.wall_s.as_mut() {
            *w += (ms / 1000) as i64;
        }
        self.a.tick(&mut self.w, &self.clk);
        let evs = std::mem::take(&mut self.w.pending);
        self.feed(evs);
    }
    fn traces(&mut self, id: &str) -> Vec<J> {
        self.req("automations/traces", json!({"automation": id})).unwrap().as_array().unwrap().clone()
    }
    fn cmds(&self) -> Vec<(&str, &str)> {
        self.w.commands.iter().map(|(u, c, _)| (u.as_str(), c.as_str())).collect()
    }
}

fn cmd(uid: &str, c: &str) -> J {
    json!({"type": "command", "uid": uid, "command": c})
}

fn state_trig(uid: &str, op: &str, v: J) -> J {
    json!({"type": "state", "uid": uid, "op": op, "value": v})
}

#[test]
fn threshold_triggers_fire_on_crossing_only() {
    let mut r = Rig::new();
    r.w.states.insert("t".into(), json!(20.0));
    r.create(json!({"id": "cold", "triggers": [state_trig("t", "below", json!(18))], "actions": [cmd("heater", "turn_on")]}));
    r.state("t", json!(19.5));
    assert!(r.cmds().is_empty(), "not below yet");
    r.state("t", json!(17.9));
    assert_eq!(r.cmds(), [("heater", "turn_on")]);
    // Still below: no new run; the same report repeated changes nothing.
    r.state("t", json!(17.0));
    r.state("t", json!(17.0));
    assert_eq!(r.w.commands.len(), 1);
    r.state("t", json!(18.5));
    r.state("t", json!("17.2"));
    assert_eq!(r.w.commands.len(), 2, "numeric strings compare as numbers");
    let t = r.traces("cold");
    assert_eq!(t[0]["result"], "done");
    assert_eq!(t[0]["trigger"]["from"], 18.5);
    assert_eq!(t[0]["actions"][0]["result"], "ok");
}

#[test]
fn changed_equals_and_attributes() {
    let mut r = Rig::new();
    r.w.states.insert("s".into(), json!("off"));
    r.create(json!({"id": "a", "triggers": [{"type": "state", "uid": "s", "op": "changed"}], "actions": [cmd("x", "toggle")]}));
    r.create(json!({"id": "b", "triggers": [state_trig("s", "equals", json!(true))], "actions": [cmd("y", "turn_on")]}));
    r.create(json!({"id": "c", "triggers": [{"type": "state", "uid": "s", "attribute": "level", "op": "above", "value": 50}], "actions": [cmd("z", "turn_on")]}));
    r.state("s", json!("off"));
    assert!(r.cmds().is_empty(), "an unchanged report is not a change");
    r.state("s", json!("on"));
    assert_eq!(r.cmds(), [("x", "toggle"), ("y", "turn_on")], "rules run in ID order; `true` equals \"on\"");
    let ev = HubEvent::EntityState(json!({"uid": "s", "state": "on", "attributes": {"level": 80}}));
    r.feed(vec![ev]);
    assert_eq!(r.cmds().last(), Some(&("z", "turn_on")));
    assert_eq!(r.w.commands.len(), 3, "an attribute change is not a state change");
}

#[test]
fn unknown_values_do_not_count_as_changes() {
    let mut r = Rig::new();
    r.create(json!({"id": "a", "triggers": [{"type": "state", "uid": "s", "op": "changed"}], "actions": [cmd("x", "turn_on")]}));
    r.state("s", J::Null);
    r.state("s", json!(5));
    assert!(r.cmds().is_empty(), "unknown → 5 is the first value, not a change");
    r.state("s", json!(6));
    assert_eq!(r.w.commands.len(), 1);
}

#[test]
fn events_and_availability() {
    let mut r = Rig::new();
    r.w.avail.insert("zb:1".into(), true);
    r.create(json!({"id": "btn", "triggers": [{"type": "event", "uid": "remote:1:action", "event_type": "single"}], "actions": [cmd("l", "toggle")]}));
    r.create(json!({"id": "lost", "triggers": [{"type": "availability", "device": "zb:1", "to": "unavailable"}], "actions": [{"type": "notify", "message": "gone"}]}));
    let press = |t: &str| HubEvent::EntityEvent { uid: "remote:1:action".into(), event_type: t.into(), attributes: Map::new(), time: 0.0 };
    r.feed(vec![press("double"), press("single")]);
    assert_eq!(r.cmds(), [("l", "toggle")]);
    r.feed(vec![HubEvent::DeviceUpdated("zb:1".into())]);
    assert!(r.a.events.iter().all(|e| e["topic"] != "automation_notify"), "no change");
    r.w.avail.insert("zb:1".into(), false);
    r.feed(vec![HubEvent::DeviceUpdated("zb:1".into())]);
    let n: Vec<&J> = r.a.events.iter().filter(|e| e["topic"] == "automation_notify").collect();
    assert_eq!(n.len(), 1);
    assert_eq!(n[0]["message"], "gone");
    assert_eq!(n[0]["automation"], "lost");
    r.w.avail.insert("zb:1".into(), true);
    r.feed(vec![HubEvent::DeviceUpdated("zb:1".into())]);
    assert_eq!(r.a.events.iter().filter(|e| e["topic"] == "automation_notify").count(), 1, "only to unavailable");
}

#[test]
fn conditions_short_circuit_and_nest() {
    let mut r = Rig::new();
    r.w.states.insert("lux".into(), json!(300));
    r.w.states.insert("mode".into(), json!("away"));
    r.create(json!({
        "id": "m",
        "triggers": [state_trig("motion", "equals", json!(true))],
        "conditions": [
            state_trig("lux", "below", json!(100)),
            {"type": "or", "conditions": [state_trig("mode", "equals", json!("home")), {"type": "not", "conditions": [state_trig("mode", "equals", json!("away"))]}]}
        ],
        "actions": [cmd("light", "turn_on")]
    }));
    r.state("motion", json!(true));
    let t = r.traces("m");
    assert_eq!(t[0]["result"], "conditions_failed");
    assert_eq!(t[0]["conditions"].as_array().unwrap().len(), 1, "stops at the first failing condition");
    assert_eq!(t[0]["conditions"][0]["value"], 300);
    r.w.states.insert("lux".into(), json!(50));
    r.state("motion", json!(false));
    r.state("motion", json!(true));
    assert_eq!(r.traces("m")[0]["conditions"][1]["result"], false);
    r.w.states.insert("mode".into(), json!("home"));
    r.state("motion", json!(false));
    r.state("motion", json!(true));
    assert_eq!(r.cmds(), [("light", "turn_on")]);
    // A repeated outcome is counted on one trace.
    let failed: Vec<J> = r.traces("m").into_iter().filter(|t| t["result"] == "conditions_failed").collect();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["count"], 2);
}

#[test]
fn time_conditions_need_a_synchronised_clock() {
    let mut r = Rig::new();
    r.create(json!({
        "id": "night",
        "triggers": [state_trig("door", "equals", json!("open"))],
        "conditions": [{"type": "time", "after": "22:00", "before": "06:00"}, {"type": "weekday", "days": ["sat", 7]}],
        "actions": [cmd("light", "turn_on")]
    }));
    r.state("door", json!("open"));
    let t = r.traces("night");
    assert_eq!(t[0]["result"], "conditions_failed");
    assert_eq!(t[0]["conditions"][0]["note"], "time not synchronised");
    // Saturday 2026-11-21 23:30 UTC+2.
    r.clk.wall_s = Some(1_795_296_600);
    r.clk.tz_offset_s = 7200;
    let l = r.clk.local().unwrap();
    assert_eq!((l.weekday, l.text().as_str()), (6, "2026-11-21 23:30"));
    r.state("door", json!("closed"));
    r.state("door", json!("open"));
    assert_eq!(r.cmds(), [("light", "turn_on")], "the window wraps midnight");
    r.clk.wall_s = Some(1_795_296_600 + 9 * 3600);
    r.state("door", json!("closed"));
    r.state("door", json!("open"));
    assert_eq!(r.w.commands.len(), 1, "08:30 is outside");
}

#[test]
fn self_triggering_rules_are_stopped_as_loops() {
    let mut r = Rig::new();
    r.w.states.insert("a".into(), json!("off"));
    r.w.states.insert("b".into(), json!("off"));
    // a → toggles b, b → toggles a: a cycle through two rules.
    r.create(json!({"id": "ab", "triggers": [{"type": "state", "uid": "a", "op": "changed"}], "actions": [cmd("b", "toggle")]}));
    r.create(json!({"id": "ba", "triggers": [{"type": "state", "uid": "b", "op": "changed"}], "actions": [cmd("a", "toggle")]}));
    r.state("a", json!("on"));
    assert_eq!(r.cmds(), [("b", "toggle"), ("a", "toggle")]);
    let t = r.traces("ab");
    assert_eq!(t[0]["result"], "loop");
    assert_eq!(t[0]["chain"], json!(["ab", "ba"]));
    // A direct self-trigger.
    r.w.states.insert("c".into(), json!("off"));
    r.create(json!({"id": "self", "triggers": [{"type": "state", "uid": "c", "op": "changed"}], "actions": [cmd("c", "toggle")]}));
    r.state("c", json!("on"));
    assert_eq!(r.cmds().iter().filter(|c| c.0 == "c").count(), 1);
    assert_eq!(r.traces("self")[0]["result"], "loop");
    // After the cause window a change is the user's again.
    r.advance(CAUSE_MS + 1);
    r.state("c", json!("on"));
    assert_eq!(r.cmds().iter().filter(|c| c.0 == "c").count(), 2);
}

#[test]
fn rate_limit_per_rule() {
    let mut r = Rig::new();
    r.create(json!({"id": "x", "max_per_min": 3, "triggers": [{"type": "event", "uid": "e"}], "actions": [cmd("l", "press")]}));
    let ev = || HubEvent::EntityEvent { uid: "e".into(), event_type: "x".into(), attributes: Map::new(), time: 0.0 };
    for _ in 0..10 {
        r.feed(vec![ev()]);
    }
    assert_eq!(r.w.commands.len(), 3);
    let t = r.traces("x");
    assert_eq!(t[0]["result"], "rate_limited");
    assert_eq!(t[0]["count"], 7);
    r.advance(60_000);
    r.feed(vec![ev()]);
    assert_eq!(r.w.commands.len(), 4);
}

#[test]
fn delays_modes_and_cancel() {
    let mut r = Rig::new();
    r.create(json!({
        "id": "stairs",
        "triggers": [{"type": "event", "uid": "m"}],
        "actions": [cmd("l", "turn_on"), {"type": "delay", "s": 30}, cmd("l", "turn_off")]
    }));
    let ev = || HubEvent::EntityEvent { uid: "m".into(), event_type: "motion".into(), attributes: Map::new(), time: 0.0 };
    r.feed(vec![ev()]);
    assert_eq!(r.cmds(), [("l", "turn_on")]);
    assert_eq!(r.a.next_wake(), Some(r.clk.mono_ms + 30_000));
    assert_eq!(r.req("automations/get", json!({"automation": "stairs"})).unwrap()["running"], true);
    r.feed(vec![ev()]);
    assert_eq!(r.traces("stairs")[0]["result"], "already_running", "single mode");
    r.advance(29_000);
    assert_eq!(r.w.commands.len(), 1);
    r.advance(1_000);
    assert_eq!(r.cmds(), [("l", "turn_on"), ("l", "turn_off")]);
    let t = r.traces("stairs");
    assert_eq!((t[0]["result"].as_str(), t[1]["result"].as_str()), (Some("already_running"), Some("done")), "newest first");
    assert_eq!(r.a.next_wake(), None);

    // Restart: a new trigger cancels the waiting run.
    let mut rule = r.req("automations/get", json!({"automation": "stairs"})).unwrap();
    rule["mode"] = "restart".into();
    r.req("automations/update", json!({"automation": "stairs", "rule": rule})).unwrap();
    r.feed(vec![ev()]);
    r.advance(20_000);
    r.feed(vec![ev()]);
    let t = r.traces("stairs");
    assert_eq!((t[0]["result"].as_str(), t[1]["result"].as_str()), (Some("running"), Some("cancelled")));
    r.advance(20_000);
    assert_eq!(r.cmds().iter().filter(|c| c.1 == "turn_off").count(), 1, "the cancelled run did not switch off");
    r.advance(10_000);
    assert_eq!(r.cmds().iter().filter(|c| c.1 == "turn_off").count(), 2);

    // Disabling ends a waiting run.
    r.feed(vec![ev()]);
    r.req("automations/disable", json!({"automation": "stairs"})).unwrap();
    assert_eq!(r.traces("stairs")[0]["result"], "cancelled");
    r.feed(vec![ev()]);
    r.advance(40_000);
    assert_eq!(r.cmds().iter().filter(|c| c.1 == "turn_on").count(), 4, "disabled rules do not run");
}

#[test]
fn schedules_fire_once_per_minute_and_wait_for_the_clock() {
    let mut r = Rig::new();
    r.create(json!({"id": "wake", "triggers": [{"type": "time", "cron": "30 7 * * mon-fri"}], "actions": [cmd("blinds", "open_cover")]}));
    let l = r.req("automations/list", json!({})).unwrap();
    assert_eq!(l["time_synced"], false);
    assert!(l["rules"][0]["suspended"].as_str().unwrap().contains("not synchronised"));
    for _ in 0..3 {
        r.advance(60_000);
    }
    assert!(r.cmds().is_empty());
    // Monday 2026-11-23 07:29:30 UTC.
    r.clk.wall_s = Some(1_795_418_970);
    assert_eq!(Local::at(1_795_418_970, 0).weekday, 1);
    r.advance(1_000);
    assert!(r.cmds().is_empty(), "the first synchronised minute only sets the reference");
    for _ in 0..40 {
        r.advance(1_000);
    }
    assert_eq!(r.cmds(), [("blinds", "open_cover")], "07:30 once");
    assert_eq!(r.traces("wake")[0]["trigger"]["local"], "2026-11-23 07:30");
    // The clock steps back a minute: no second run.
    r.clk.wall_s = Some(1_795_418_970 + 31);
    r.advance(1_000);
    r.advance(60_000);
    assert_eq!(r.w.commands.len(), 1);
    assert!(r.req("automations/list", json!({})).unwrap()["rules"][0].get("suspended").is_none());
}

#[test]
fn cron_fields() {
    let t = |s: i64| Local::at(s, 0);
    // 2026-11-22 (Sunday) 12:05 UTC.
    let sun = t(1_795_349_100);
    assert_eq!((sun.weekday, sun.day, sun.month, sun.minute_of_day), (7, 22, 11, 725));
    assert!(Cron::parse("*/5 12 * * 0").unwrap().matches(&sun));
    assert!(Cron::parse("5 12 * * 7").unwrap().matches(&sun));
    assert!(Cron::parse("5 12 * nov sun").unwrap().matches(&sun));
    assert!(!Cron::parse("5 12 * * 1-5").unwrap().matches(&sun));
    assert!(Cron::parse("0,5,10 10-14/2 22 * 1").unwrap().matches(&sun), "day of month or weekday");
    assert!(!Cron::parse("5 12 1 * 1").unwrap().matches(&sun));
    for bad in ["", "* * * *", "60 * * * *", "* 24 * * *", "* * 0 * *", "*/0 * * * *", "5-1 * * * *", "* * * * mon-xyz"] {
        assert!(Cron::parse(bad).is_err(), "{bad:?}");
    }
    assert_eq!(Local::at(951_782_400, 0).text(), "2000-02-29 00:00");
    assert_eq!(Local::at(-1, 0).text(), "1969-12-31 23:59");
}

#[test]
fn enable_disable_actions_and_start() {
    let mut r = Rig::new();
    r.create(json!({"id": "once", "triggers": [{"type": "start"}], "actions": [cmd("l", "turn_on"), {"type": "disable", "automation": "once"}, {"type": "enable", "automation": "later"}]}));
    r.create(json!({"id": "later", "enabled": false, "triggers": [{"type": "event", "uid": "e"}], "actions": [{"type": "notify", "message": "hi", "webhook": "http://x"}]}));
    r.a.dirty.clear();
    r.a.started(&mut r.w, &r.clk);
    r.a.started(&mut r.w, &r.clk);
    assert_eq!(r.cmds(), [("l", "turn_on")], "start fires once");
    assert!(!r.a.rule("once").unwrap().enabled);
    assert!(r.a.rule("later").unwrap().enabled);
    assert_eq!(r.a.dirty.iter().map(String::as_str).collect::<Vec<_>>(), ["later", "once"]);
    assert_eq!(r.traces("once")[0]["result"], "done");
    r.feed(vec![HubEvent::EntityEvent { uid: "e".into(), event_type: "x".into(), attributes: Map::new(), time: 0.0 }]);
    let t = r.traces("later");
    assert_eq!(t[0]["actions"][0]["note"], "webhooks are not sent yet");
}

#[test]
fn errors_and_late_command_results_reach_the_trace() {
    let mut r = Rig::new();
    r.w.deferred = true;
    r.create(json!({"id": "x", "actions": [cmd("a", "turn_on"), cmd("b", "turn_on"), {"type": "disable", "automation": "nope"}]}));
    let out = r.req("automations/run", json!({"automation": "x"})).unwrap();
    let run = out["run"].as_u64().unwrap();
    assert_eq!(out["trace"]["result"], "error", "the unknown rule");
    assert_eq!(out["trace"]["actions"][0]["result"], "pending");
    assert_eq!(out["trace"]["actions"][2]["error"]["code"], "not_found");
    r.a.command_result(1, Ok(()));
    r.a.command_result(2, Err(ApiError::new("timeout", "no response")));
    r.a.command_result(99, Ok(()));
    let t = &r.traces("x")[0];
    assert_eq!(t["run"], run);
    assert_eq!(t["actions"][0]["result"], "ok");
    assert_eq!(t["actions"][1]["error"]["code"], "timeout");
    // A failing command does not stop the run.
    r.w.fail = Some(ApiError::new("device_unavailable", "gone"));
    r.w.deferred = false;
    let out = r.req("automations/run", json!({"automation": "x"})).unwrap();
    assert_eq!(out["trace"]["actions"].as_array().unwrap().len(), 3);
}

#[test]
fn a_waiting_request_action_holds_its_run_and_later_triggers() {
    let mut r = Rig::new();
    r.w.requests_wait = true;
    r.w.states.insert("s".into(), json!("off"));
    let req = json!({"type": "request", "request": {"type": "network/permit_join", "duration_s": 30}});
    r.create(json!({"id": "pj", "triggers": [{"type": "state", "uid": "s", "op": "changed"}], "actions": [req, cmd("x", "turn_on")]}));
    r.state("s", json!("on"));
    // The command waits for the request's result; so does the next trigger.
    r.state("s", json!("off"));
    assert_eq!(r.w.requests.len(), 1);
    assert!(r.cmds().is_empty());
    assert_eq!(r.a.waiting_requests(), [1]);
    let err = ApiError::new("busy", "ZRHP status 0x04");
    r.a.request_result(1, Err(err), &mut r.w, &r.clk);
    // The first run went on, then the deferred trigger ran its own.
    assert_eq!(r.cmds(), [("x", "turn_on")]);
    assert_eq!(r.w.requests.len(), 2);
    assert_eq!(r.a.waiting_requests(), [2]);
    r.a.request_result(2, Ok(J::Null), &mut r.w, &r.clk);
    let t = r.traces("pj");
    assert_eq!((t[1]["result"].as_str(), t[0]["result"].as_str()), (Some("error"), Some("done")), "{t:?}");
    assert_eq!(t[1]["actions"][0]["error"]["message"], "ZRHP status 0x04");
    assert_eq!(t[1]["actions"][1]["result"], "ok");
    assert_eq!(t[0]["trigger"]["to"], "off");
}

/// A rule whose request action waits for its result (permit join), then
/// turns `x` on.
fn waiting_rule(r: &mut Rig, mode: &str) {
    r.w.requests_wait = true;
    r.w.states.insert("s".into(), json!("off"));
    let req = json!({"type": "request", "request": {"type": "network/permit_join", "duration_s": 30}});
    r.create(json!({"id": "pj", "mode": mode, "triggers": [{"type": "state", "uid": "s", "op": "changed"}],
        "actions": [req, cmd("x", "turn_on")]}));
}

/// Results for every request action runs wait for, until none waits.
fn answer_all(r: &mut Rig) {
    loop {
        let keys = r.a.waiting_requests();
        if keys.is_empty() {
            return;
        }
        for k in keys {
            r.a.request_result(k, Ok(J::Null), &mut r.w, &r.clk);
        }
    }
}

#[test]
fn a_manual_run_while_a_run_waits_starts_after_it_and_answers_then() {
    let mut r = Rig::new();
    waiting_rule(&mut r, "single");
    r.state("s", json!("on"));
    let v = r.req("automations/run", json!({"automation": "pj"})).unwrap();
    let run = v["run"].as_u64().unwrap() as u32;
    assert!(r.a.pending(run), "{v}");
    assert_eq!(r.a.waiting_requests(), [1]);
    r.a.request_result(1, Ok(J::Null), &mut r.w, &r.clk);
    // The first run is done; the manual one runs and waits in its turn.
    assert_eq!(r.cmds(), [("x", "turn_on")]);
    assert!(r.a.pending(run));
    answer_all(&mut r);
    assert!(!r.a.pending(run));
    let a = r.a.run_result(run);
    assert_eq!((a["run"].as_u64(), a["trace"]["result"].as_str()), (Some(run.into()), Some("done")), "{a}");
    assert_eq!(a["trace"]["trigger"]["type"], "manual");
    assert_eq!(r.cmds(), [("x", "turn_on"), ("x", "turn_on")]);
}

#[test]
fn restart_mode_does_not_cancel_a_run_that_waits() {
    let mut r = Rig::new();
    waiting_rule(&mut r, "restart");
    r.state("s", json!("on"));
    r.state("s", json!("off"));
    answer_all(&mut r);
    // Both runs went through, as when the request answered in place.
    assert_eq!(r.cmds(), [("x", "turn_on"), ("x", "turn_on")]);
    let t = r.traces("pj");
    let results: Vec<_> = t.iter().map(|t| t["result"].as_str().unwrap()).collect();
    assert_eq!(results, ["done", "done"], "{t:?}");
    assert_eq!(r.w.requests.len(), 2);
}

#[test]
fn triggers_beyond_the_deferral_bound_are_dropped_visibly() {
    let mut r = Rig::new();
    waiting_rule(&mut r, "single");
    for i in 0..20 {
        r.state("s", json!(if i % 2 == 0 { "on" } else { "off" }));
    }
    // One run waits, MAX_DEFERRED triggers wait for it, the rest are dropped.
    let t = r.traces("pj");
    assert_eq!((t[0]["result"].as_str(), t[0]["count"].as_u64()), (Some("deferred_overflow"), Some(3)), "{t:?}");
    answer_all(&mut r);
    assert_eq!(r.w.commands.len(), 1 + MAX_DEFERRED);
    let t = r.traces("pj");
    assert_eq!(t.iter().filter(|t| t["result"] == "done").count(), 1 + MAX_DEFERRED);
    assert!(t.iter().all(|t| t["result"] != "already_running" && t["result"] != "cancelled"), "{t:?}");
}

#[test]
fn manual_runs_skip_conditions_unless_asked() {
    let mut r = Rig::new();
    r.create(json!({"id": "x", "enabled": false, "conditions": [state_trig("s", "equals", json!(1))], "actions": [cmd("a", "turn_on")]}));
    assert!(r.req("automations/run", json!({"automation": "x", "conditions": true})).unwrap()["run"].is_null());
    assert!(r.req("automations/run", json!({"automation": "x"})).unwrap()["run"].is_u64(), "a test run of a disabled rule");
    assert_eq!(r.w.commands.len(), 1);
}

#[test]
fn validation_and_limits() {
    let mut r = Rig::new();
    let bad = [
        json!({"actions": []}),
        json!({"id": "Bad Id", "actions": [cmd("a", "b")]}),
        json!({"triggers": [{"type": "state", "uid": "x", "op": "above", "value": "warm"}], "actions": [cmd("a", "b")]}),
        json!({"triggers": [{"type": "time", "cron": "99 * * * *"}], "actions": [cmd("a", "b")]}),
        json!({"conditions": [{"type": "time"}], "actions": [cmd("a", "b")]}),
        json!({"conditions": [{"type": "state", "uid": "x", "op": "changed"}], "actions": [cmd("a", "b")]}),
        json!({"conditions": [{"type": "and", "conditions": [{"type": "or", "conditions": [{"type": "not", "conditions": [{"type": "and", "conditions": [state_trig("x", "equals", json!(1))]}]}]}]}], "actions": [cmd("a", "b")]}),
        json!({"actions": [{"type": "delay", "s": 100_000}]}),
        json!({"actions": [{"type": "request", "request": {"type": "automations/delete", "automation": "x"}}]}),
        json!({"actions": [{"type": "request", "request": {"type": "network/reset"}}]}),
        json!({"actions": [{"type": "jump"}]}),
        json!({"v": 2, "actions": [cmd("a", "b")]}),
    ];
    for b in bad {
        let e = r.req("automations/create", json!({"rule": b})).unwrap_err();
        assert!(matches!(e.code.as_str(), "invalid_value" | "unsupported"), "{b}: {e:?}");
        let v = r.req("automations/validate", json!({"rule": b})).unwrap();
        assert_eq!(v["ok"], false);
    }
    let many: Vec<J> = (0..17).map(|_| cmd("a", "b")).collect();
    assert!(r.req("automations/create", json!({"rule": {"actions": many}})).is_err());
    let ok = r.create(json!({"name": "G", "actions": [{"type": "request", "request": {"id": 5, "type": "groups/command", "group": 3, "command": "turn_on"}}]}));
    assert_eq!(ok["id"], "r1");
    assert_eq!(ok["v"], 1);
    assert_eq!(r.create(json!({"actions": [cmd("a", "b")]}))["id"], "r2");
    assert_eq!(r.req("automations/create", json!({"rule": {"id": "r1", "actions": [cmd("a", "b")]}})).unwrap_err().code, "invalid_value");
    r.req("automations/run", json!({"automation": "r1"})).unwrap();
    assert_eq!(r.w.requests, [json!({"type": "groups/command", "group": 3, "command": "turn_on"})]);
    for i in 2..MAX_RULES {
        r.create(json!({"id": format!("n{i}"), "actions": [cmd("a", "b")]}));
    }
    assert_eq!(r.req("automations/create", json!({"rule": {"actions": [cmd("a", "b")]}})).unwrap_err().code, "no_space");
    assert_eq!(r.req("automations/get", json!({"automation": "zz"})).unwrap_err().code, "not_found");
    assert!(r.a.request("devices/list", &J::Null, &mut r.w, &r.clk).is_none());
}

#[test]
fn all_rules_together_have_a_size_budget() {
    let mut r = Rig::new();
    let big = |i: usize| json!({"id": format!("b{i}"), "actions": [{"type": "notify", "message": "x", "data": "y".repeat(3500)}]});
    for i in 0..4 {
        r.create(big(i));
    }
    let e = r.req("automations/create", json!({"rule": big(9)})).unwrap_err();
    assert_eq!(e.code, "no_space");
    // Replacing a rule counts it once.
    r.req("automations/update", json!({"automation": "b0", "rule": big(0)})).unwrap();
    // A hub low on heap takes no new or changed rules; it still runs them.
    r.a.free_heap = Some(30 * 1024);
    let e = r.req("automations/update", json!({"automation": "b0", "rule": big(0)})).unwrap_err();
    assert_eq!((e.code.as_str(), e.message.as_str()), ("no_space", "low memory (30 KB free)"));
    r.req("automations/run", json!({"automation": "b0"})).unwrap();
}

#[test]
fn rules_survive_a_restart() {
    let mut r = Rig::new();
    r.w.states.insert("t".into(), json!(30));
    r.create(json!({"id": "hot", "name": "Hot", "triggers": [state_trig("t", "above", json!(25))], "actions": [cmd("fan", "turn_on")]}));
    r.create(json!({"id": "off", "triggers": [{"type": "start"}], "actions": [cmd("fan", "turn_off")]}));
    r.req("automations/disable", json!({"automation": "off"})).unwrap();
    // What the app writes for the dirty IDs.
    let store: Vec<(String, Vec<u8>)> =
        r.a.rules().map(|x| (format!("{PREFIX}{}", x.id), x.src.clone().into_bytes())).collect();
    let mut again = Automations::default();
    assert_eq!(again.load(store), 2);
    assert_eq!(again.rule("hot").unwrap(), r.a.rule("hot").unwrap());
    assert!(!again.rule("off").unwrap().enabled);
    // Primed with the current value: still above, no run at the first report.
    let mut w = Fake::default();
    w.states.insert("t".into(), json!(30));
    again.prime(&w);
    again.started(&mut w, &r.clk);
    let ev = w.set("t", json!(31));
    again.observe(&[ev], &mut w, &r.clk);
    assert!(w.commands.is_empty());
    let ev = w.set("t", json!(20));
    again.observe(&[ev], &mut w, &r.clk);
    let ev = w.set("t", json!(26));
    again.observe(&[ev], &mut w, &r.clk);
    assert_eq!(w.commands.len(), 1);
    // A record of a newer format or broken JSON is skipped.
    let mut x = Automations::default();
    assert_eq!(x.load(vec![("auto/a".into(), br#"{"v":9,"actions":[]}"#.to_vec()), ("auto/b".into(), b"{".to_vec())]), 0);
}

#[test]
fn traces_are_bounded() {
    let mut r = Rig::new();
    r.create(json!({"id": "x", "max_per_min": 60, "actions": [cmd("a", "b")]}));
    for _ in 0..(MAX_TRACES + 5) {
        r.req("automations/run", json!({"automation": "x"})).unwrap();
    }
    assert_eq!(r.traces("x").len(), MAX_TRACES);
    assert_eq!(r.req("automations/traces", json!({"limit": 2})).unwrap().as_array().unwrap().len(), 2);
}
