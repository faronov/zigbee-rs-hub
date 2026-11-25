//! End to end: `hubctl import z2m` against the hub application behind
//! `hubctl --sim --sim-devices serve`: Zigbee2MQTT's data directory
//! (generated) is previewed, then imported; the network is the backup's,
//! a device the hub interviewed keeps its records and takes its friendly
//! name, a new one appears at once.

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value as J, json};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

struct Hub(Child, u16);

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn hub() -> Hub {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let child = Command::new(env!("CARGO_BIN_EXE_hubctl"))
        .args([
            "--sim",
            "--sim-devices",
            "serve",
            "--listen",
            &format!("127.0.0.1:{port}"),
            "--token",
            "e2e",
            "--for",
            "120",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("hubctl");
    Hub(child, port)
}

struct Client(WebSocket<MaybeTlsStream<TcpStream>>, u64);

impl Client {
    fn connect(port: u16) -> Self {
        let until = Instant::now() + Duration::from_secs(30);
        let (mut ws, _) = loop {
            match tungstenite::connect(format!("ws://127.0.0.1:{port}/api")) {
                Ok(c) => break c,
                Err(_) if Instant::now() < until => thread::sleep(Duration::from_millis(100)),
                Err(e) => panic!("connect: {e}"),
            }
        };
        let _ = ws.read().unwrap();
        ws.send(Message::text(
            json!({"type": "auth", "token": "e2e"}).to_string(),
        ))
        .unwrap();
        let _ = ws.read().unwrap();
        Client(ws, 0)
    }

    fn call(&mut self, mut req: J) -> J {
        self.1 += 1;
        req["id"] = self.1.into();
        self.0.send(Message::text(req.to_string())).unwrap();
        loop {
            if let Message::Text(t) = self.0.read().unwrap() {
                let m: J = serde_json::from_str(&t).unwrap();
                if m["type"] == "result" && m["id"] == self.1 {
                    return m;
                }
            }
        }
    }
}

/// Zigbee2MQTT's data directory: plug 1 of `--sim-devices` (named) and a
/// sensor the hub does not know, on a network of their own.
fn z2m_dir() -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("z2m-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let db = [
        json!({"id": 1, "type": "Coordinator", "ieeeAddr": "0x020100000000515a", "nwkAddr": 0, "endpoints": {}}),
        json!({"id": 2, "type": "Router", "ieeeAddr": "0x5a52530000000001", "nwkAddr": 0x5A01, "manufName": "zigbee-rs", "modelId": "SIM-PLUG",
            "powerSource": "Mains (single phase)", "endpoints": {"1": {"profId": 260, "epId": 1, "devId": 81, "inClusterList": [0, 3, 6], "outClusterList": []}},
            "interviewCompleted": true}),
        json!({"id": 3, "type": "EndDevice", "ieeeAddr": "0x00158d0003c4d5e6", "nwkAddr": 1202, "manufName": "LUMI", "modelId": "lumi.weather",
            "powerSource": "Battery", "endpoints": {"1": {"profId": 260, "epId": 1, "devId": 24321, "inClusterList": [0, 3, 1026, 1029], "outClusterList": [0]}},
            "interviewCompleted": true}),
    ]
    .iter()
    .map(J::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    std::fs::write(d.join("database.db"), db).unwrap();
    std::fs::write(
        d.join("configuration.yaml"),
        "devices:\n  '0x5a52530000000001':\n    friendly_name: Desk plug\n  '0x00158d0003c4d5e6':\n    friendly_name: Kitchen sensor\n",
    )
    .unwrap();
    let backup = json!({"metadata": {"format": "zigpy/open-coordinator-backup", "version": 1, "source": "zigbee-herdsman@3.2.0",
            "internal": {"date": "2026-11-20T10:00:00.000Z", "znpVersion": 2}},
        "stack_specific": {"zstack": {"tclk_seed": "77777777777777777777777777777777"}},
        "coordinator_ieee": "020100000000515a", "pan_id": "1b2c", "extended_pan_id": "a1a1a1a1a1a1a1a1", "nwk_update_id": 0,
        "security_level": 5, "channel": 15, "channel_mask": [15],
        "network_key": {"key": "8899aabbccddeeff0011223344556677", "sequence_number": 0, "frame_counter": 5000},
        "devices": [{"nwk_address": "4b2", "ieee_address": "00158d0003c4d5e6"}]});
    std::fs::write(d.join("coordinator_backup.json"), backup.to_string()).unwrap();
    d
}

fn import(dir: &Path, port: u16, extra: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_hubctl"))
        .args(["import", "z2m"])
        .arg(dir)
        .args([
            "--url",
            &format!("ws://127.0.0.1:{port}/api"),
            "--token",
            "e2e",
        ])
        .args(extra)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    text
}

#[test]
fn a_zigbee2mqtt_directory_is_previewed_and_imported() {
    let hub = hub();
    let mut c = Client::connect(hub.1);
    let until = Instant::now() + Duration::from_secs(60);
    while c.call(json!({"type": "devices/get", "device": "zb:5a52530000000001"}))["result"]["interview"]
        != "complete"
    {
        assert!(Instant::now() < until, "the simulated plug interviewed");
        thread::sleep(Duration::from_millis(200));
    }
    let dir = z2m_dir();
    let preview = import(&dir, hub.1, &[]);
    let row = |ieee: &str, status: &str| {
        preview
            .lines()
            .any(|l| l.starts_with(ieee) && l.split_whitespace().nth(1) == Some(status))
    };
    assert!(
        row("5a52530000000001", "keep") && row("00158d0003c4d5e6", "new"),
        "{preview}"
    );
    assert!(
        preview.contains("restore") && preview.contains("force"),
        "{preview}"
    );
    let applied = import(&dir, hub.1, &["--apply", "--force"]);
    assert!(applied.contains("network restored"), "{applied}");
    let info = c.call(json!({"type": "network/info"}));
    assert_eq!(info["result"]["ext_pan_id"], "a1a1a1a1a1a1a1a1", "{info}");
    let plug = c.call(json!({"type": "devices/get", "device": "zb:5a52530000000001"}));
    assert_eq!(
        (
            plug["result"]["name"].as_str(),
            plug["result"]["interview"].as_str()
        ),
        (Some("Desk plug"), Some("complete")),
        "{plug}"
    );
    let sensor = c.call(json!({"type": "devices/get", "device": "zb:00158d0003c4d5e6"}));
    assert_eq!(
        (
            sensor["result"]["name"].as_str(),
            sensor["result"]["interview"].as_str()
        ),
        (Some("Kitchen sensor"), Some("imported")),
        "{sensor}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
