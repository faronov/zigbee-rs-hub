//! Heap the bridge's model takes per device (a counting allocator around
//! the snapshot), and the transient peak of building `bridge/devices`.
//! Prints the numbers; asserts generous bounds.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};

use hub_devices::api::ApiServer;
use hub_devices::hub::{Hub, HubConfig};
use hub_devices::resolve::MemoryCatalog;
use hub_devices::sim_device::{Sim, SimDevice};
use hub_devices::transport::MockTransport;
use hub_devices::zcl::{TypedValue, ZclType, ZclValue};
use hub_devices::zdo::LogicalType;
use hub_mqtt::{Bridge, Limits, MqttConfig};

struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let n = LIVE.fetch_add(l.size() as isize, Ordering::Relaxed) + l.size() as isize;
        PEAK.fetch_max(n.max(0) as usize, Ordering::Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn devices(n: u64) -> Vec<SimDevice> {
    (0..n)
        .map(|i| {
            let ieee = 0x00158d0002000000 + i;
            let nwk = 0x2000 + i as u16;
            if i % 2 == 0 {
                SimDevice::new(ieee, nwk, LogicalType::EndDevice, true, 0x115F)
                    .endpoint(
                        1,
                        0x0104,
                        0x0302,
                        &[0x0000, 0x0001, 0x0003, 0x0402, 0x0405],
                        &[0x0003],
                    )
                    .basic("LUMI", "lumi.weather", 3)
                    .attr(1, 0x0001, 0x0021, TypedValue::uint(ZclType::U8, 200))
                    .attr(1, 0x0402, 0x0000, TypedValue::int(ZclType::I16, 2137))
                    .attr(1, 0x0405, 0x0000, TypedValue::uint(ZclType::U16, 5520))
            } else {
                SimDevice::new(ieee, nwk, LogicalType::Router, true, 0x117C)
                    .endpoint(
                        1,
                        0x0104,
                        0x010C,
                        &[0x0000, 0x0003, 0x0004, 0x0005, 0x0006, 0x0008, 0x0300],
                        &[0x0019],
                    )
                    .basic("IKEA of Sweden", "TRADFRI bulb", 1)
                    .attr(
                        1,
                        0x0006,
                        0x0000,
                        TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(true))),
                    )
                    .attr(1, 0x0008, 0x0000, TypedValue::uint(ZclType::U8, 254))
                    .attr(1, 0x0300, 0x0007, TypedValue::uint(ZclType::U16, 370))
                    .attr(1, 0x0300, 0x400A, TypedValue::uint(ZclType::MAP16, 0x0010))
            }
        })
        .collect()
}

#[test]
fn model_heap_per_device() {
    const N: u64 = 40;
    let mut hub = Hub::new(
        MockTransport::default(),
        HubConfig::default(),
        Box::new(MemoryCatalog::default()),
    );
    let mut sim = Sim::default();
    for d in devices(N) {
        let ieee = d.ieee;
        sim.devices.push(d);
        sim.join(&mut hub, ieee, 1_000_000);
    }
    let _ = hub.drain_events();
    let mut api = ApiServer::new("zigbee-rs-hub 0.1.8", "t");
    let (sid, hello) = api.connect();
    let mut frames = vec![hello];
    frames.extend(api.handle_text(&mut hub, sid, r#"{"type":"auth","token":"t"}"#, 1_000_000));
    let mut b = Bridge::new(
        MqttConfig {
            enabled: true,
            host: "x".into(),
            discovery: true,
            ..MqttConfig::default()
        },
        Limits::default(),
    );
    for f in &frames {
        b.on_api(f);
    }
    let mut snapshot = String::new();
    for q in b.take_api() {
        for f in api.handle_text(&mut hub, sid, &q, 1_000_000) {
            if f.contains("\"topic\":\"snapshot\"") {
                snapshot = f;
            } else {
                b.on_api(&f);
            }
        }
    }
    assert!(!snapshot.is_empty());
    let before = LIVE.load(Ordering::SeqCst);
    PEAK.store(before.max(0) as usize, Ordering::SeqCst);
    b.on_api(&snapshot);
    let model = LIVE.load(Ordering::SeqCst) - before;
    let parse_peak = PEAK.load(Ordering::SeqCst) as isize - before;
    assert_eq!(b.devices() as u64, N);
    // Publishing everything once (connect): the transient peak.
    b.on_connected();
    b.tick(10_000);
    let base = LIVE.load(Ordering::SeqCst);
    PEAK.store(base.max(0) as usize, Ordering::SeqCst);
    let (mut n, mut bytes, mut largest) = (0, 0, 0);
    let mut publish_peak = 0;
    let mut worst = String::new();
    loop {
        let base = LIVE.load(Ordering::SeqCst);
        PEAK.store(base.max(0) as usize, Ordering::SeqCst);
        let Some(m) = b.next_msg() else { break };
        let p = PEAK.load(Ordering::SeqCst) as isize - base;
        if p > publish_peak {
            publish_peak = p;
            worst = m.topic.clone();
        }
        n += 1;
        bytes += m.payload.len();
        largest = largest.max(m.payload.len());
    }
    let _ = base;
    println!("worst step: {worst}");
    println!(
        "{N} devices ({} entities): model {model} B ({} B/device), snapshot {} B, parse peak {parse_peak} B; \
         first publication: {n} messages, {bytes} B, largest {largest} B, peak {publish_peak} B",
        b.model().entities.len(),
        model / N as isize,
        snapshot.len()
    );
    assert!(model / (N as isize) < 8 * 1024, "{model}");
}
