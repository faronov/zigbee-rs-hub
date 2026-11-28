//! `hubctl serve`'s [`Platform`](hub_app::Platform): the desktop stands in
//! for the S3 (system facts of the process, Wi-Fi credentials and the token
//! kept in memory, the log tail from hubctl's own logger), and the
//! simulated NCP's demo network (`--sim-demo`) for trying the settings and
//! the network map in the web UI.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use hub_app::LogRing;
use hub_ncp::lifecycle::{FormRequest, Hub, Rng};
use hub_ncp::sim::{SimNcp, SimRemote};
use serde_json::{Value as J, json};

/// Log lines for `hub/logs`.
pub static LOGS: LogRing = LogRing::new(16 * 1024);

/// The desktop platform.
pub struct Desktop {
    pub started: Instant,
    pub rng: Rng,
    pub wifi_channel: Option<u8>,
    pub wifi: Mutex<Option<String>>,
    /// `--sim-crash`: the previous boot crashed (as an S3 reports it).
    pub crashed: bool,
}

impl hub_app::Platform for Desktop {
    fn system(&self) -> J {
        let mut v = json!({
            "version": format!("hubctl {}", env!("CARGO_PKG_VERSION")),
            "uptime_s": self.started.elapsed().as_secs(),
            "mode": "desktop",
            "wifi": {
                "ssid": self.wifi.lock().unwrap().clone(),
                "state": "simulated",
                "channel": self.wifi_channel,
            },
        });
        if self.crashed {
            v["reset_reason"] = 4.into();
            v["crash"] = json!({
                "crashed": true,
                "streak": 1,
                "panic": "panicked at src/sim.rs:1:1:\nsimulated crash (hubctl --sim-crash)",
                "exception": null,
            });
        }
        v
    }
    fn wifi_channel(&self) -> Option<u8> {
        self.wifi_channel
    }
    fn set_wifi(&self, ssid: &str, _password: &str) -> Result<(), String> {
        log::info!("Wi-Fi credentials for {ssid:?} accepted (not applied on a desktop)");
        *self.wifi.lock().unwrap() = Some(ssid.to_string());
        Ok(())
    }
    fn save_token(&self, _token: &str) -> Result<(), String> {
        log::info!("API token rotated (kept in memory until hubctl exits)");
        Ok(())
    }
    fn random(&self, buf: &mut [u8]) {
        (self.rng)(buf)
    }
    fn logs(&self, max: usize) -> Vec<String> {
        LOGS.lines(max)
    }
    fn features(&self) -> Vec<&'static str> {
        vec!["wifi", "logs", "token_rotate"]
    }
}

/// A small mesh on the simulated NCP: three routers (one only reachable
/// through another), end devices, routes, counters; the network is formed
/// on channel 15 first if the NCP has none.
pub fn sim_demo(hub: &Hub, sim: &SimNcp) -> Result<(), String> {
    if !hub.wait_ready(Duration::from_secs(10)) {
        return Err("simulated NCP not ready".into());
    }
    if hub.status().network_state == 0 {
        hub.form(FormRequest { channel_mask: 1 << 15, ..FormRequest::default() }).map_err(|e| e.to_string())?;
    }
    let own = hub.client().session().and_then(|s| s.info.as_ref().map(|i| i.ieee_active)).unwrap_or_default();
    let ieee = |n: u8| [n, 0x10, 0x20, 0x30, 0x40, 0x8D, 0x15, 0x00];
    let (r1, r2, r3) = (ieee(1), ieee(2), ieee(3));
    let (e1, e2, e3, e4) = (ieee(0x11), ieee(0x12), ieee(0x13), ieee(0x14));
    sim.with(|s| {
        s.table_devices = vec![
            (r1, 0x1A01, None),
            (r2, 0x2B02, None),
            (r3, 0x3C03, None),
            (e1, 0x4D11, Some(0x80)),
            (e2, 0x5E12, None),
            (e3, 0x6F13, None),
            (e4, 0x7A14, None),
        ];
        s.neighbors = Some(vec![(r1, 0x1A01, 1, 2), (r2, 0x2B02, 1, 2), (e1, 0x4D11, 2, 1)]);
        s.children = Some(vec![(e1, 0x4D11, 0x80)]);
        s.routes = vec![(0x1A01, 0x1A01, 0), (0x2B02, 0x2B02, 0), (0x3C03, 0x1A01, 0), (0x6F13, 0x2B02, 1)];
        s.remotes = vec![
            SimRemote {
                nwk: 0x1A01,
                ieee: r1,
                neighbors: vec![(own, 0, 0, 2, 230), (r2, 0x2B02, 1, 2, 120), (r3, 0x3C03, 1, 2, 160), (e2, 0x5E12, 2, 1, 200)],
                routes: vec![(0x0000, 0x0000, 0), (0x3C03, 0x3C03, 0)],
            },
            SimRemote {
                nwk: 0x2B02,
                ieee: r2,
                neighbors: vec![(own, 0, 0, 2, 180), (r1, 0x1A01, 1, 2, 110), (e3, 0x6F13, 2, 1, 75)],
                routes: vec![(0x0000, 0x0000, 0)],
            },
            SimRemote { nwk: 0x3C03, ieee: r3, neighbors: vec![(r1, 0x1A01, 1, 2, 150), (e4, 0x7A14, 2, 1, 40)], routes: vec![(0x0000, 0x1A01, 0)] },
        ];
    });
    sim.with(|s| {
        // Scans take as long as on the H2 (16 channels, duration_exp 3).
        s.scan_time = Duration::from_millis(2800);
        s.beacons = vec![
            (15, 0x2F11, [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88], 0x0000, 140, -71, false),
            (20, 0x6A0B, [0xA0, 0xB0, 0xC0, 0xD0, 0x00, 0x8D, 0x15, 0x00], 0x0000, 90, -84, true),
            (20, 0x6A0B, [0xA0, 0xB0, 0xC0, 0xD0, 0x00, 0x8D, 0x15, 0x00], 0x4F21, 60, -90, true),
        ];
    });
    log::info!("sim demo: network on channel 15, 3 routers, 4 end devices, 2 foreign networks");
    Ok(())
}
