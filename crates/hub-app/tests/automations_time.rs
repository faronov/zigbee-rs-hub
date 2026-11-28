//! Run times before and after the hub's clock is set (SNTP): the engine's
//! clock counts from power-on and the API shows it through the wall-clock
//! offset, so a run from before the clock was set reads as Unix time after
//! it, never as days since 1970. A test binary of its own: it sets the
//! process-wide offset.

use hub_app::automations::{Automations, Clock, World};
use hub_devices::device::set_wall_offset;
use hub_devices::hub::{ApiError, HubEvent};
use serde_json::{Value as J, json};

#[derive(Default)]
struct W(u64);

impl World for W {
    fn state(&self, _: &str) -> Option<J> {
        None
    }
    fn available(&self, _: &str) -> Option<bool> {
        None
    }
    fn command(&mut self, _: &str, _: &str, _: &J) -> Result<(u64, &'static str), ApiError> {
        self.0 += 1;
        Ok((self.0, "ok"))
    }
    fn request(&mut self, _: &J) -> Result<(u64, Option<J>), ApiError> {
        Ok((1, Some(J::Null)))
    }
}

fn req(a: &mut Automations, w: &mut W, clk: &Clock, kind: &str, v: J) -> J {
    a.request(kind, &v, w, clk).unwrap().unwrap()
}

fn press(at_ms: i64) -> HubEvent {
    HubEvent::EntityState(json!({"uid": "b", "state": "emergency", "attributes": {}, "last_reported": at_ms as f64 / 1000.0}))
}

#[test]
fn run_times_before_and_after_the_clock_is_set() {
    // Power-on: the engine's clock is the uptime.
    let mut clk = Clock { mono_ms: 5_000, now_ms: 5_000, ..Clock::default() };
    let (mut a, mut w) = (Automations::default(), W::default());
    let rule = json!({"id": "sos", "triggers": [{"type": "state", "uid": "b", "op": "reported"}],
        "actions": [{"type": "command", "uid": "p", "command": "toggle"}]});
    req(&mut a, &mut w, &clk, "automations/create", json!({"rule": rule}));
    a.observe(&[press(5_000)], &mut w, &clk);
    assert_eq!(w.0, 1);
    let time = |a: &mut Automations, w: &mut W, clk: &Clock| {
        let t = req(a, w, clk, "automations/traces", json!({}))[0]["time"].clone();
        let l = req(a, w, clk, "automations/list", json!({}))["rules"][0]["last_run"].clone();
        assert_eq!(t, l);
        t.as_f64().unwrap()
    };
    assert_eq!(time(&mut a, &mut w, &clk), 5.0, "relative to the hub's `time` until the clock is set");

    // SNTP: 2026-11-27 00:00 UTC is engine time 6 s.
    const SYNC: i64 = 1_795_737_600_000;
    let off = SYNC - 6_000;
    set_wall_offset(off);
    (clk.mono_ms, clk.now_ms, clk.wall_s) = (66_000, 66_000, Some(SYNC / 1000 + 60));
    assert_eq!(time(&mut a, &mut w, &clk), (off + 5_000) as f64 / 1000.0, "the earlier run in Unix time");
    // The same report, published again under the new offset: no press.
    a.observe(&[press(off + 5_000)], &mut w, &clk);
    assert_eq!(w.0, 1);
    a.events.clear();
    a.observe(&[press(off + 66_000)], &mut w, &clk);
    assert_eq!(w.0, 2);
    assert_eq!(time(&mut a, &mut w, &clk), (SYNC + 60_000) as f64 / 1000.0);
    assert_eq!(a.events.iter().find(|e| e["topic"] == "automation_run").unwrap()["time"], (SYNC + 60_000) as f64 / 1000.0);
    set_wall_offset(0);
}
