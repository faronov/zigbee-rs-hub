//! `hub cpu [ms]`: CPU use per FreeRTOS task over a window (run-time
//! stats, `CONFIG_FREERTOS_GENERATE_RUN_TIME_STATS`), and FreeRTOS names
//! for the Rust threads, which are otherwise all `pthread`.

use std::collections::BTreeMap;
use std::ffi::CStr;
use std::time::Duration;

use esp_idf_svc::sys;

struct Task {
    name: String,
    prio: u32,
    runtime: u32,
    stack_free: u32,
}

fn sample() -> (BTreeMap<usize, Task>, u32) {
    let mut total = 0u32;
    let mut out = BTreeMap::new();
    unsafe {
        let n = sys::uxTaskGetNumberOfTasks() as usize;
        let mut tasks: Vec<sys::TaskStatus_t> = vec![core::mem::zeroed(); n + 4];
        let got =
            sys::uxTaskGetSystemState(tasks.as_mut_ptr(), tasks.len() as u32, &mut total) as usize;
        for t in &tasks[..got] {
            out.insert(
                t.xHandle as usize,
                Task {
                    name: CStr::from_ptr(t.pcTaskName).to_string_lossy().into_owned(),
                    prio: t.uxCurrentPriority,
                    runtime: t.ulRunTimeCounter,
                    stack_free: t.usStackHighWaterMark,
                },
            );
        }
    }
    (out, total)
}

/// Busiest tasks over `window`, in % of one core (two cores: 200 % total).
pub fn report(window: Duration) -> String {
    let (a, t0) = sample();
    std::thread::sleep(window);
    let (b, t1) = sample();
    let elapsed = t1.wrapping_sub(t0).max(1) as u64;
    let mut rows: Vec<(u64, &Task)> = b
        .iter()
        .map(|(h, t)| {
            (
                a.get(h).map_or(0, |p| t.runtime.wrapping_sub(p.runtime)) as u64,
                t,
            )
        })
        .collect();
    rows.sort_by(|x, y| y.0.cmp(&x.0));
    let mut s = format!(
        "cpu over {} ms (% of one core; IDLE0/IDLE1 = free):\r\n",
        window.as_millis()
    );
    for (d, t) in rows.iter().filter(|(d, _)| *d > 0) {
        s += &format!(
            "  {:<16} prio {:>2} {:>6.2} %  stack free {}\r\n",
            t.name,
            t.prio,
            *d as f64 * 100.0 / elapsed as f64,
            t.stack_free
        );
    }
    s
}

/// Runs `f` so that the threads it spawns (and theirs) are named `name`.
pub fn named<R>(name: &'static CStr, f: impl FnOnce() -> R) -> R {
    unsafe {
        let mut cfg = sys::esp_pthread_get_default_config();
        cfg.thread_name = name.as_ptr();
        cfg.inherit_cfg = true;
        sys::esp_pthread_set_cfg(&cfg);
    }
    let r = f();
    unsafe {
        let cfg = sys::esp_pthread_get_default_config();
        sys::esp_pthread_set_cfg(&cfg);
    }
    r
}

/// `hub bench`: thread hand-over latency of std channels (round trips
/// between two threads), plain and with the timeouts the ZRHP driver uses.
pub fn bench() -> String {
    use std::sync::mpsc::channel;
    use std::time::Instant;
    let mut s = String::new();
    for (label, timeout) in [
        ("recv", None),
        ("recv_timeout 200 ms", Some(Duration::from_millis(200))),
    ] {
        let (to_b, b_rx) = channel::<u32>();
        let (to_a, a_rx) = channel::<u32>();
        let peer = std::thread::Builder::new()
            .stack_size(4096)
            .spawn(move || {
                loop {
                    let v = match timeout {
                        None => b_rx.recv().ok(),
                        Some(t) => match b_rx.recv_timeout(t) {
                            Ok(v) => Some(v),
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                            Err(_) => None,
                        },
                    };
                    match v {
                        Some(v) if to_a.send(v).is_ok() => {}
                        _ => break,
                    }
                }
            })
            .expect("bench thread");
        let n = 200u32;
        let t = Instant::now();
        let mut worst = Duration::ZERO;
        for i in 0..n {
            let r = Instant::now();
            let _ = to_b.send(i);
            let _ = match timeout {
                None => a_rx.recv().ok(),
                Some(t) => a_rx.recv_timeout(t).ok(),
            };
            worst = worst.max(r.elapsed());
        }
        let mean = t.elapsed() / n;
        drop(to_b);
        let _ = peer.join();
        s += &format!("{label}: round trip mean {mean:?} worst {worst:?}\r\n");
    }
    s
}
