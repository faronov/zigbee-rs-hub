//! End to end: `hubctl --sim --sim-z2m-devices serve --mqtt …` against an MQTT
//! broker, driven by an MQTT client like a Zigbee2MQTT tool would. The
//! broker is the in-process test broker, or the one `MQTT_E2E_BROKER`
//! names (`host:port`, e.g. a local mosquitto).

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hub_mqtt::broker::Broker;
use hub_mqtt::tcp::TcpTransport;
use hub_mqtt::{ConnectOptions, Event, Msg, Transport};
use serde_json::{Value as J, json};

const T: Duration = Duration::from_secs(30);
const LIGHT: &str = "TRADFRI bulb E27 WS opal 980lm";

struct Kill(Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

type Log = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

struct Client {
    c: TcpTransport,
    log: Log,
}

impl Client {
    fn connect(host: &str, port: u16, base: &str) -> Self {
        let log: Log = Arc::default();
        let l2 = log.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut c = TcpTransport::new();
        let opts = ConnectOptions {
            host: host.into(),
            port,
            client_id: format!("e2e-{}", std::process::id()),
            username: None,
            password: None,
            keepalive_s: 30,
            will: Msg { topic: "e2e/will".into(), payload: Vec::new(), qos: 0, retain: false },
        };
        c.connect(
            &opts,
            Arc::new(move |e| match e {
                Event::Message(t, p) => l2.lock().unwrap().push((t, p)),
                other => {
                    let _ = tx.send(other);
                }
            }),
        )
        .unwrap();
        assert_eq!(rx.recv_timeout(T).unwrap(), Event::Connected);
        c.subscribe(&[format!("{base}/#"), "homeassistant/#".into()]).unwrap();
        Self { c, log }
    }

    fn publish(&mut self, topic: &str, payload: &str) {
        self.c.publish(&Msg { topic: topic.into(), payload: payload.as_bytes().to_vec(), qos: 0, retain: false }).unwrap();
    }

    /// Waits for a message on `topic` whose JSON satisfies `f`; returns it.
    fn wait(&self, topic: &str, f: impl Fn(&J) -> bool) -> J {
        let t = Instant::now();
        loop {
            {
                let log = self.log.lock().unwrap();
                for (tp, p) in log.iter().rev() {
                    if tp == topic
                        && let Ok(v) = serde_json::from_slice::<J>(p)
                        && f(&v)
                    {
                        return v;
                    }
                }
            }
            if t.elapsed() > T {
                let log = self.log.lock().unwrap();
                let seen: Vec<String> =
                    log.iter().filter(|(t, _)| t == topic).map(|(_, p)| String::from_utf8_lossy(p).into_owned()).collect();
                panic!("timed out on {topic}; seen there: {seen:?}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn mark(&self) -> usize {
        self.log.lock().unwrap().len()
    }

    /// Raw payloads on `topic` after `mark`.
    fn since(&self, mark: usize, topic: &str) -> Vec<Vec<u8>> {
        self.log.lock().unwrap()[mark..].iter().filter(|(t, _)| t == topic).map(|(_, p)| p.clone()).collect()
    }
}

#[test]
fn z2m_tool_against_hubctl_sim() {
    let local;
    let (host, port) = match std::env::var("MQTT_E2E_BROKER") {
        Ok(hp) => {
            let (h, p) = hp.rsplit_once(':').expect("MQTT_E2E_BROKER=host:port");
            (h.to_string(), p.parse().unwrap())
        }
        Err(_) => {
            local = Broker::start("127.0.0.1:0", None).unwrap();
            ("127.0.0.1".to_string(), local.addr().port())
        }
    };
    // A base topic of our own: an external broker may serve others too.
    let base = format!("e2e{}", std::process::id());
    let hub = Kill(
        Command::new(env!("CARGO_BIN_EXE_hubctl"))
            .args(["--sim", "--sim-z2m-devices", "serve", "--listen", &format!("127.0.0.1:{}", free_port()), "--token", "e2e", "--for", "120"])
            .args(["--mqtt", &format!("{host}:{port}"), "--mqtt-base", &base, "--mqtt-discovery"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut c = Client::connect(&host, port, &base);
    let t = |s: &str| format!("{base}/{s}");
    c.wait(&t("bridge/state"), |v| v["state"] == "online");
    // The three devices with their Zigbee2MQTT names and states.
    let devs = c.wait(&t("bridge/devices"), |v| {
        v.as_array().is_some_and(|a| a.len() == 4 && a.iter().all(|d| d["interview_completed"] == true))
    });
    let names: Vec<&str> = devs.as_array().unwrap().iter().map(|d| d["friendly_name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["Coordinator", "lumi.weather", LIGHT, "SP 120"]);
    let sensor = c.wait(&t("lumi.weather"), |v| v.get("temperature").is_some() && v.get("humidity").is_some());
    assert_eq!((sensor["humidity"].clone(), sensor["battery"].clone()), (json!(55.2), json!(100)));
    c.wait(&t(&format!("{LIGHT}/availability")), |v| v["state"] == "online");
    c.wait(&t(LIGHT), |v| v["state"] == "OFF" && v["color_temp"] == 370);
    // Discovery for Home Assistant (opt-in, enabled here).
    let cfg = c.wait("homeassistant/light/0x00158d0001000002/light/config", |v| v["schema"] == "json");
    assert_eq!(cfg["command_topic"], t(&format!("{LIGHT}/set")));

    // Commands as a Zigbee2MQTT tool sends them; the device reports.
    c.publish(&t(&format!("{LIGHT}/set")), r#"{"state": "ON", "brightness": 200}"#);
    c.wait(&t(LIGHT), |v| v["state"] == "ON" && v["brightness"] == 200);
    c.publish(&t(&format!("{LIGHT}/set/color_temp")), "300");
    c.wait(&t(LIGHT), |v| v["color_temp"] == 300);
    c.publish(&t("SP 120/set"), r#"{"state": "TOGGLE"}"#);
    c.wait(&t("SP 120"), |v| v["state"] == "ON");

    // Bridge requests with transactions.
    c.publish(&t("bridge/request/permit_join"), r#"{"time": 20, "transaction": "pj"}"#);
    let r = c.wait(&t("bridge/response/permit_join"), |v| v["transaction"] == "pj");
    assert_eq!(r, json!({"data": {"time": 20}, "status": "ok", "transaction": "pj"}));
    c.wait(&t("bridge/info"), |v| v["permit_join"] == true);
    c.publish(&t("bridge/request/permit_join"), r#"{"time": 0}"#);
    c.wait(&t("bridge/info"), |v| v["permit_join"] == false);
    let mark = c.mark();
    c.publish(&t("bridge/request/device/rename"), r#"{"from": "SP 120", "to": "hall/plug", "transaction": 2}"#);
    let r = c.wait(&t("bridge/response/device/rename"), |v| v["transaction"] == 2);
    assert_eq!(r["status"], "ok", "{r}");
    c.wait(&t("hall/plug"), |v| v["state"] == "ON");
    c.wait(&t("hall/plug/availability"), |v| v["state"] == "online");
    // The old name's retained topics are cleared.
    let start = Instant::now();
    while c.since(mark, &t("SP 120/availability")).iter().all(|p| !p.is_empty()) {
        assert!(start.elapsed() < T, "old availability not cleared");
        std::thread::sleep(Duration::from_millis(50));
    }
    c.publish(&t("hall/plug/set/state"), "OFF");
    c.wait(&t("hall/plug"), |v| v["state"] == "OFF");
    let cfg = c.wait("homeassistant/switch/0x00158d0001000003/switch/config", |v| v["state_topic"] == t("hall/plug"));
    assert_eq!(cfg["unique_id"], format!("0x00158d0001000003_switch_{base}"));

    // The hub dies: the broker publishes the will.
    drop(hub);
    c.wait(&t("bridge/state"), |v| v["state"] == "offline");
}
