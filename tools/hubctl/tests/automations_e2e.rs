//! End to end: the hub application with its automations behind
//! `hubctl --sim --sim-devices serve` (simulated NCP and devices), over
//! the WebSocket API, across a hard kill and a restart on the same store.

use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value as J, json};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

const P1: &str = "zb:5a52530000000001:1:switch";
const P2: &str = "zb:5a52530000000002:1:switch";

struct Hub {
    child: Child,
    port: u16,
}

impl Hub {
    fn start(store: &Path) -> Self {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let child = Command::new(env!("CARGO_BIN_EXE_hubctl"))
            .args(["--sim", "--sim-devices", "--store"])
            .arg(store)
            .args(["serve", "--listen", &format!("127.0.0.1:{port}"), "--token", "e2e", "--for", "120"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("hubctl");
        Hub { child, port }
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Client {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    id: u64,
}

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
        if let MaybeTlsStream::Plain(s) = ws.get_ref() {
            s.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        }
        ws.send(Message::text(json!({"type": "auth", "token": "e2e"}).to_string())).unwrap();
        let mut c = Client { ws, id: 0 };
        c.next(Duration::from_secs(5), |v| v["type"] == "auth_ok").expect("auth_ok");
        c
    }

    fn next(&mut self, timeout: Duration, want: impl Fn(&J) -> bool) -> Option<J> {
        let until = Instant::now() + timeout;
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

    fn call(&mut self, mut req: J) -> Result<J, J> {
        self.id += 1;
        let id = self.id;
        req["id"] = id.into();
        self.ws.send(Message::text(req.to_string())).unwrap();
        let r = self.next(Duration::from_secs(10), |v| v["type"] == "result" && v["id"] == id).expect("result");
        if r["success"] == true { Ok(r["result"].clone()) } else { Err(r["error"].clone()) }
    }

    fn state(&mut self, uid: &str) -> J {
        self.call(json!({"type": "entities/states", "uids": [uid]})).unwrap()[0]["state"].clone()
    }

    fn wait(&mut self, what: &str, timeout: Duration, mut cond: impl FnMut(&mut Self) -> bool) {
        let until = Instant::now() + timeout;
        while !cond(self) {
            assert!(Instant::now() < until, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn traces(&mut self, id: &str) -> Vec<J> {
        self.call(json!({"type": "automations/traces", "automation": id})).unwrap().as_array().unwrap().clone()
    }

    fn runs(&mut self, id: &str) -> J {
        self.call(json!({"type": "automations/get", "automation": id})).unwrap()["runs"].clone()
    }
}

fn rule(id: &str, triggers: J, actions: J) -> J {
    json!({"type": "automations/create", "rule": {"id": id, "triggers": triggers, "actions": actions}})
}

fn changed(uid: &str) -> J {
    json!([{"type": "state", "uid": uid, "op": "changed"}])
}

fn cmd(uid: &str, c: &str) -> J {
    json!({"type": "command", "uid": uid, "command": c})
}

#[test]
fn automations_end_to_end_and_across_a_restart() {
    let store = Path::new(env!("CARGO_TARGET_TMPDIR")).join("automations-e2e.store");
    let _ = std::fs::remove_file(&store);
    let hub = Hub::start(&store);
    let mut c = Client::connect(hub.port);
    c.wait("the simulated plugs", Duration::from_secs(30), |c| {
        let s = c.call(json!({"type": "entities/states", "uids": [P1, P2]})).unwrap();
        s.as_array().is_some_and(|a| a.len() == 2 && a.iter().all(|x| x["state"] == false))
    });
    let info = c.call(json!({"type": "hub/info"})).unwrap();
    assert_eq!(info["automations"]["format"], 1);
    c.call(json!({"type": "subscribe", "topics": ["automations"]})).unwrap();

    // Plug 2 follows plug 1.
    c.call(rule("follow", changed(P1), json!([cmd(P2, "toggle")]))).unwrap();
    c.call(json!({"type": "entities/command", "uid": P1, "command": "turn_on"})).unwrap();
    let ev = c.next(Duration::from_secs(10), |v| v["event"]["topic"] == "automation_run").expect("automation_run event");
    assert_eq!(ev["event"]["automation"], "follow");
    c.wait("plug 2 on", Duration::from_secs(10), |c| c.state(P2) == true);
    let t = c.traces("follow");
    assert_eq!(t[0]["result"], "done");
    assert_eq!((&t[0]["trigger"]["from"], &t[0]["trigger"]["to"]), (&json!(false), &json!(true)));

    // Plug 1 follows plug 2 too: a loop through two rules, stopped.
    c.call(rule("echo", changed(P2), json!([cmd(P1, "toggle")]))).unwrap();
    c.call(json!({"type": "entities/command", "uid": P1, "command": "turn_off"})).unwrap();
    c.wait("the loop to be stopped", Duration::from_secs(10), |c| c.traces("follow")[0]["result"] == "loop");
    assert_eq!(c.traces("follow")[0]["chain"], json!(["follow", "echo"]));
    let runs = c.runs("follow");
    thread::sleep(Duration::from_secs(1));
    assert_eq!(c.runs("follow"), runs, "no more runs");
    c.call(json!({"type": "automations/disable", "automation": "echo"})).unwrap();

    // A delay, as a test run.
    c.call(rule("pulse", json!([]), json!([cmd(P2, "turn_off"), {"type": "delay", "s": 1}, cmd(P2, "turn_on")]))).unwrap();
    let r = c.call(json!({"type": "automations/run", "automation": "pulse"})).unwrap();
    assert_eq!(r["trace"]["result"], "running");
    c.wait("plug 2 off", Duration::from_secs(5), |c| c.state(P2) == false);
    c.wait("plug 2 on again", Duration::from_secs(5), |c| c.state(P2) == true);
    assert_eq!(c.traces("pulse")[0]["result"], "done");

    c.call(rule("boot", json!([{"type": "start"}]), json!([{"type": "notify", "message": "hub started"}]))).unwrap();
    let err = c.call(rule("bad", json!([{"type": "time", "cron": "61 * * * *"}]), json!([cmd(P1, "toggle")]))).unwrap_err();
    assert_eq!(err["code"], "invalid_value");

    // Power cut: no shutdown; the rules were written when they changed.
    drop(c);
    drop(hub);
    let hub = Hub::start(&store);
    let mut c = Client::connect(hub.port);
    let list = c.call(json!({"type": "automations/list"})).unwrap();
    let ids: Vec<&str> = list["rules"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["boot", "echo", "follow", "pulse"]);
    assert_eq!(list["rules"][1]["enabled"], false);
    assert_eq!(list["time_synced"], true, "a desktop clock is set");
    c.wait("the start trigger", Duration::from_secs(20), |c| c.traces("boot").first().is_some_and(|t| t["result"] == "done"));
    // The restored rule works on the restored devices.
    c.wait("the plugs' states", Duration::from_secs(30), |c| c.state(P1) != J::Null && c.state(P2) != J::Null);
    let p2 = c.state(P2);
    c.call(json!({"type": "entities/command", "uid": P1, "command": "toggle"})).unwrap();
    c.wait("plug 2 toggled", Duration::from_secs(10), |c| c.state(P2) != p2);
    c.call(json!({"type": "automations/delete", "automation": "pulse"})).unwrap();
    assert_eq!(c.call(json!({"type": "automations/get", "automation": "pulse"})).unwrap_err()["code"], "not_found");
}
