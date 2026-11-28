//! End to end, for clients' tests (the Home Assistant integration's): the
//! simulator's `sim/*` requests of `hubctl --sim --sim-devices serve` make
//! network events happen (a refused join, a rejoin, an announcement, a
//! leave, a new device whose interview ends partial) and `--sim-crash`
//! makes `hub/system` report a crashed previous boot, on the ZRHP and the
//! EZSP backend.

use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value as J, json};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

const P1: &str = "zb:5a52530000000001";
const P2: &str = "zb:5a52530000000002";

struct Hub(Child);

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Client {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    id: u64,
}

impl Client {
    fn connect(port: u16) -> Self {
        let until = Instant::now() + Duration::from_secs(30);
        let (ws, _) = loop {
            match tungstenite::connect(format!("ws://127.0.0.1:{port}/api")) {
                Ok(c) => break c,
                Err(_) if Instant::now() < until => thread::sleep(Duration::from_millis(100)),
                Err(e) => panic!("connect: {e}"),
            }
        };
        if let MaybeTlsStream::Plain(s) = ws.get_ref() {
            s.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        }
        let mut c = Client { ws, id: 0 };
        c.ws.send(Message::text(json!({"type": "auth", "token": "e2e"}).to_string())).unwrap();
        c.next(|v| v["type"] == "auth_ok").expect("auth_ok");
        c
    }

    fn next(&mut self, want: impl Fn(&J) -> bool) -> Option<J> {
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            match self.ws.read() {
                Ok(Message::Text(t)) => {
                    let v: J = serde_json::from_str(&t).unwrap();
                    if want(&v) {
                        return Some(v);
                    }
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(e) => panic!("read: {e}"),
            }
        }
        None
    }

    fn call(&mut self, req: J) -> J {
        let r = self.result(req.clone());
        assert_eq!(r["success"], true, "{req}: {r}");
        r["result"].clone()
    }

    fn result(&mut self, mut req: J) -> J {
        self.id += 1;
        let id = self.id;
        req["id"] = id.into();
        self.ws.send(Message::text(req.to_string())).unwrap();
        self.next(|v| v["type"] == "result" && v["id"] == id).expect("result")
    }

    fn wait(&mut self, what: &str, mut cond: impl FnMut(&mut Self) -> bool) {
        let until = Instant::now() + Duration::from_secs(30);
        while !cond(self) {
            assert!(Instant::now() < until, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// The device record, null before the device is known.
    fn device(&mut self, id: &str) -> J {
        self.result(json!({"type": "devices/get", "device": id}))["result"].clone()
    }
}

#[test]
fn simulated_network_events_and_crash_on_zrhp() {
    sim_events("zrhp");
}

#[test]
fn simulated_network_events_and_crash_on_ezsp() {
    sim_events("ezsp");
}

fn sim_events(backend: &str) {
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let _hub = Hub(
        Command::new(env!("CARGO_BIN_EXE_hubctl"))
            .args(["--backend", backend, "--sim", "--sim-devices", "--sim-crash"])
            .args(["serve", "--listen", &format!("127.0.0.1:{port}"), "--token", "e2e", "--for", "120"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("hubctl"),
    );
    let mut c = Client::connect(port);
    c.wait("the simulated plugs", |c| [P1, P2].iter().all(|d| c.device(d)["interview"] == "complete"));

    let sys = c.call(json!({"type": "hub/system"}));
    assert_eq!((&sys["reset_reason"], &sys["crash"]["crashed"], &sys["crash"]["streak"]), (&json!(4), &json!(true), &json!(1)), "{sys}");
    assert!(sys["crash"]["panic"].as_str().is_some_and(|p| p.contains("simulated crash")), "{sys}");
    assert_eq!(sys["boot_id"].as_str().map(str::len), Some(8), "{sys}");

    // A new device whose Simple descriptor read fails.
    let new = c.call(json!({"type": "sim/join", "partial": true}))["device"].as_str().unwrap().to_string();
    c.wait("the partial interview", |c| c.device(&new)["interview"] == "partial");
    assert_eq!(c.device(&new)["interview_failed"], json!(["simple_descriptor_0"]));

    c.call(json!({"type": "sim/join", "device": P1, "how": "secured", "nwk": 0x5A21}));
    c.call(json!({"type": "sim/announce", "device": P1}));
    c.call(json!({"type": "sim/join", "device": "zb:5a525300000000ee", "denied": true}));
    if backend == "zrhp" {
        c.call(json!({"type": "sim/leave", "device": P2, "aged_out": true}));
        c.wait("aged out", |c| c.call(json!({"type": "network/events"})).as_array().unwrap().iter().any(|e| e["event"] == "aged_out"));
        assert_eq!(c.device(P2)["available"], true);
    }
    c.call(json!({"type": "sim/leave", "device": P2}));
    c.wait("the leave", |c| c.device(P2)["unavailable_reason"] == "left");
    let p1 = c.device(P1);
    assert_eq!((&p1["nwk"], &p1["rejoins"], &p1["announces"]), (&json!(0x5A21), &json!(1), &json!(1)), "{p1}");

    let log = c.call(json!({"type": "network/events"}));
    let seen: Vec<(J, J)> = log.as_array().unwrap().iter().map(|e| (e["event"].clone(), e["device"].clone())).collect();
    for want in [
        (json!("joined"), json!(new)),
        (json!("rejoined"), json!(P1)),
        (json!("announced"), json!(P1)),
        (json!("joined"), json!("zb:5a525300000000ee")),
        (json!("left"), json!(P2)),
    ] {
        assert!(seen.contains(&want), "{backend}: {want:?} in {log}");
    }
    let denied = log.as_array().unwrap().iter().find(|e| e["device"] == "zb:5a525300000000ee").unwrap();
    assert_eq!((&denied["decision"], &denied["reason"]), (&json!("denied"), &json!("permit_closed")), "{denied}");
}
