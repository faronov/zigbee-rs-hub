//! Heap of the automations (a hub without PSRAM has ~300 KB in all):
//! counted with this test binary's own allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use hub_app::automations::{Automations, Clock, World};
use hub_devices::hub::{ApiError, HubEvent};
use serde_json::{Value as J, json};

struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        LIVE.fetch_add(l.size() as isize, Ordering::SeqCst);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as isize, Ordering::SeqCst);
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static A: Counting = Counting;

struct W;

impl World for W {
    fn state(&self, uid: &str) -> Option<J> {
        Some(json!({"uid": uid, "state": 20, "attributes": {}}))
    }
    fn available(&self, _: &str) -> Option<bool> {
        Some(true)
    }
    fn command(&mut self, _: &str, _: &str, _: &J) -> Result<(u64, &'static str), ApiError> {
        Ok((1, "pending"))
    }
    fn request(&mut self, _: &J) -> Result<(u64, Option<J>), ApiError> {
        Ok((1, Some(J::Null)))
    }
}

/// A typical rule: a threshold trigger, a time window, light on, delay, off.
fn rule(i: usize) -> J {
    let dev = |n: usize| format!("zb:00158d00{n:08x}");
    json!({"id": format!("rule{i}"), "name": format!("Rule {i}"),
        "triggers": [{"type": "state", "uid": format!("{}:1:temperature", dev(i)), "op": "above", "value": 25}],
        "conditions": [{"type": "time", "after": "18:00", "before": "07:00"}],
        "actions": [{"type": "command", "uid": format!("{}:11:light", dev(i + 100)), "command": "turn_on", "params": {"brightness": 120}},
            {"type": "delay", "s": 90}, {"type": "command", "uid": format!("{}:11:light", dev(i + 100)), "command": "turn_off"}]})
}

#[test]
fn rules_and_traces_fit_a_hub_without_psram() {
    // 20:00 UTC: inside the rules' time window.
    let clk = Clock { mono_ms: 1, now_ms: 1, wall_s: Some(1_795_291_200), tz_offset_s: 0 };
    let before = LIVE.load(Ordering::SeqCst);
    let mut a = Automations::default();
    let mut w = W;
    for i in 0..32 {
        a.request("automations/create", &json!({"rule": rule(i)}), &mut w, &clk).unwrap().unwrap();
    }
    let json: usize = a.rules().map(|r| r.src.len()).sum();
    a.events.clear();
    a.dirty.clear();
    let rules = LIVE.load(Ordering::SeqCst) - before;
    // 20 runs: 16 wait in their delay, 4 are refused (busy); 20 traces.
    for i in 0..20 {
        let uid = format!("zb:00158d00{i:08x}:1:temperature");
        let ev = HubEvent::EntityState(json!({"uid": uid, "state": 30, "attributes": {}}));
        a.observe(&[ev], &mut w, &clk);
    }
    a.events.clear();
    assert_eq!(a.next_wake(), Some(90_001));
    let all = LIVE.load(Ordering::SeqCst) - before;
    eprintln!("32 rules ({json} B of JSON): {rules} B of heap; with 20 traces and 16 waiting runs: {all} B");
    // Measured: 37.9 KB and 25.4 KB (about 1.2 KB per rule, per trace with
    // its waiting run).
    assert!(rules < 44 * 1024, "{rules} B");
    assert!(all - rules < 30 * 1024, "{} B", all - rules);
}

