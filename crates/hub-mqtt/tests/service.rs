//! The service end to end in one process: the hub application on the
//! simulated NCP, the bridge thread with the TCP client, the test broker.
//! Settings over the API, LWT and reconnect, credentials, disable.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hub_app::{App, AppConfig};
use hub_mqtt::broker::Broker;
use hub_mqtt::codec::Packet;
use hub_mqtt::{
    MemoryStore, MqttConfig, MqttService, ServiceConfig, SettingsStore, Transport, TransportFactory,
};
use hub_ncp::lifecycle::{FormRequest, Hub as Ncp, HubConfig, Rng};
use hub_ncp::sim::SimNcp;
use hub_ncp::store::Store;
use hub_ncp::transport::{Line, pipe};
use serde_json::{Value as J, json};

const T: Duration = Duration::from_secs(10);

fn rng() -> Rng {
    let n = Arc::new(AtomicU32::new(7));
    Arc::new(move |buf: &mut [u8]| {
        for b in buf {
            *b = (n.fetch_add(0x9E37_79B9, Ordering::Relaxed) >> 11) as u8;
        }
    })
}

fn ncp() -> (Ncp, SimNcp) {
    let (host, ncp_end) = pipe::pair();
    let sim = SimNcp::start(ncp_end);
    let (rx, tx) = host.split();
    let mut cfg = HubConfig::default();
    cfg.driver.lines = vec![Line::BR_DEFAULT];
    let ncp = Ncp::start(cfg, rx, tx, None, Store::volatile(), rng());
    assert!(ncp.wait_ready(T));
    ncp.form(FormRequest {
        channel_mask: 1 << 15,
        ..FormRequest::default()
    })
    .unwrap();
    (ncp, sim)
}

fn session(app: &App) -> (u64, Receiver<String>) {
    let (tx, rx) = mpsc::channel();
    let sid = app.connect(Box::new(tx)).unwrap();
    let _hello = rx.recv_timeout(T).unwrap();
    app.text(sid, json!({"type": "auth", "token": "secret"}).to_string());
    let ok: J = serde_json::from_str(&rx.recv_timeout(T).unwrap()).unwrap();
    assert_eq!(ok["type"], "auth_ok");
    (sid, rx)
}

fn call(app: &App, sid: u64, rx: &Receiver<String>, id: u64, mut body: J) -> J {
    body["id"] = id.into();
    app.text(sid, body.to_string());
    loop {
        let f: J = serde_json::from_str(&rx.recv_timeout(T).unwrap()).unwrap();
        if f["type"] == "result" && f["id"] == id {
            return f;
        }
    }
}

fn until(what: &str, f: impl Fn() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < T, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn settings_lwt_reconnect_credentials_and_disable() {
    let broker = Broker::start("127.0.0.1:0", Some(("hub".into(), "pw".into()))).unwrap();
    let (ncp, sim) = ncp();
    let app = App::start(
        ncp.clone(),
        AppConfig {
            api_token: "secret".into(),
            ..AppConfig::default()
        },
    );
    assert!(app.wait(T, |s| s.endpoint_registered));
    let store = Arc::new(MemoryStore::default());
    let cfg = MqttConfig {
        enabled: true,
        host: "127.0.0.1".into(),
        port: broker.addr().port(),
        username: "hub".into(),
        password: "pw".into(),
        keepalive_s: 5,
        ..MqttConfig::default()
    };
    store.save(&cfg.to_store()).unwrap();
    let factory: TransportFactory =
        Arc::new(|| Box::new(hub_mqtt::tcp::TcpTransport::new()) as Box<dyn Transport>);
    let svc = MqttService::start(&app, store.clone(), factory, ServiceConfig::default());
    let online = || {
        broker.retained("zigbee2mqtt/bridge/state").as_deref()
            == Some(&br#"{"state":"online"}"#[..])
    };
    let offline = || {
        broker.retained("zigbee2mqtt/bridge/state").as_deref()
            == Some(&br#"{"state":"offline"}"#[..])
    };
    until("bridge/state online", online);
    until("bridge/info", || {
        broker.retained("zigbee2mqtt/bridge/info").is_some()
    });
    let info: J =
        serde_json::from_slice(&broker.retained("zigbee2mqtt/bridge/info").unwrap()).unwrap();
    assert_eq!(info["coordinator"]["ieee_address"], "0x020100000000515a");
    assert_eq!(info["network"]["channel"], 15);

    // The API: settings without the password, status, the feature.
    let (sid, rx) = session(&app);
    let got = call(&app, sid, &rx, 1, json!({"type": "mqtt/config/get"}));
    assert_eq!(got["result"]["password_set"], true, "{got}");
    assert!(got["result"].get("password").is_none());
    let st = call(&app, sid, &rx, 2, json!({"type": "mqtt/status"}));
    assert_eq!(st["result"]["state"], "connected", "{st}");
    assert_eq!(st["result"]["tls"], false);
    let hub = call(&app, sid, &rx, 3, json!({"type": "hub/info"}));
    assert!(
        hub["result"]["maintenance"]
            .as_array()
            .unwrap()
            .contains(&json!("mqtt")),
        "{hub}"
    );
    let bad = call(
        &app,
        sid,
        &rx,
        4,
        json!({"type": "mqtt/config/set", "base_topic": "a/#"}),
    );
    assert_eq!(bad["error"]["code"], "invalid_value", "{bad}");

    // permit_join over MQTT reaches the host core.
    broker.publish(
        "zigbee2mqtt/bridge/request/permit_join",
        br#"{"time": 30, "transaction": 1}"#,
        false,
    );
    until("permit_join response", || {
        broker
            .published()
            .iter()
            .any(|(t, _, _)| t == "zigbee2mqtt/bridge/response/permit_join")
    });
    until("permit join open", || ncp.permit_join_remaining() > 0);
    until("bridge/info permit_join", || {
        broker
            .retained("zigbee2mqtt/bridge/info")
            .and_then(|p| serde_json::from_slice::<J>(&p).ok())
            .is_some_and(|i| i["permit_join"] == true)
    });

    // The broker drops the connection: the will says offline, the bridge
    // reconnects (1 s backoff) and is online again.
    let connects = broker.connects();
    broker.kick();
    until("will: offline", offline);
    until("reconnected", || broker.connects() > connects && online());

    // A wrong password: refused, retried, reported.
    let r = call(
        &app,
        sid,
        &rx,
        5,
        json!({"type": "mqtt/config/set", "password": "nope"}),
    );
    assert_eq!(r["success"], true, "{r}");
    until("refused", || {
        svc.status()
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("bad user name or password"))
    });
    assert_eq!(
        MqttConfig::from_store(store.load().as_deref()).password,
        "nope",
        "stored"
    );
    // Disabled: offline, no client.
    call(
        &app,
        sid,
        &rx,
        6,
        json!({"type": "mqtt/config/set", "password": "pw"}),
    );
    until("online with the right password", online);
    call(
        &app,
        sid,
        &rx,
        7,
        json!({"type": "mqtt/config/set", "enabled": false}),
    );
    until("offline after disable", offline);
    until("no clients", || broker.clients() == 0);
    assert_eq!(svc.status().state, "disabled");
    app.shutdown();
    ncp.shutdown();
    sim.stop();
}

#[test]
fn low_memory_keeps_the_bridge_waiting() {
    let broker = Broker::start("127.0.0.1:0", None).unwrap();
    let (ncp, sim) = ncp();
    let app = App::start(
        ncp.clone(),
        AppConfig {
            api_token: "secret".into(),
            ..AppConfig::default()
        },
    );
    let store = Arc::new(MemoryStore::default());
    let cfg = MqttConfig {
        enabled: true,
        host: "127.0.0.1".into(),
        port: broker.addr().port(),
        ..MqttConfig::default()
    };
    store.save(&cfg.to_store()).unwrap();
    let factory: TransportFactory =
        Arc::new(|| Box::new(hub_mqtt::tcp::TcpTransport::new()) as Box<dyn Transport>);
    let svc = MqttService::start(
        &app,
        store,
        factory,
        ServiceConfig {
            heap_free: Some(|| 10_000),
            min_free: 64 * 1024,
            ..ServiceConfig::default()
        },
    );
    until("low_memory", || svc.status().state == "low_memory");
    assert!(svc.status().last_error.unwrap().contains("9 KB free heap"));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(broker.connects(), 0);
    svc.stop();
    assert_eq!(svc.status().state, "disabled");
    app.shutdown();
    ncp.shutdown();
    sim.stop();
}

/// Each CONNECT's user name and whether it had a password.
type Connects = Arc<Mutex<Vec<(Option<String>, bool)>>>;

/// A broker refusing anonymous clients like Mosquitto with an auth plugin
/// (Home Assistant's add-on): CONNACK 5 and the close in one go, or (mode
/// 2) a close without a CONNACK.
fn refusing_broker(mode: Arc<AtomicU8>) -> (u16, Connects) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { return };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 512];
            let connect = loop {
                match s.read(&mut chunk) {
                    Ok(0) | Err(_) => break None,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
                if let Ok(Some((p, _))) = Packet::decode(&buf, 4096) {
                    break Some(p);
                }
            };
            if let Some(Packet::Connect {
                username, password, ..
            }) = connect
            {
                log.lock().unwrap().push((username, password.is_some()));
                if mode.load(Ordering::SeqCst) == 1 {
                    let _ = s.write_all(&[0x20, 2, 0, 5]);
                }
            }
            let _ = s.shutdown(Shutdown::Both);
        }
    });
    (port, seen)
}

#[test]
fn refusals_of_anonymous_clients_are_reported() {
    let mode = Arc::new(AtomicU8::new(1));
    let (port, seen) = refusing_broker(mode.clone());
    let (ncp, sim) = ncp();
    let app = App::start(
        ncp.clone(),
        AppConfig {
            api_token: "secret".into(),
            ..AppConfig::default()
        },
    );
    // A password left from an earlier user name: not sent without one.
    let store = Arc::new(MemoryStore::default());
    let cfg = MqttConfig {
        enabled: true,
        host: "127.0.0.1".into(),
        port,
        password: "stale".into(),
        ..MqttConfig::default()
    };
    store.save(&cfg.to_store()).unwrap();
    let factory: TransportFactory =
        Arc::new(|| Box::new(hub_mqtt::tcp::TcpTransport::new()) as Box<dyn Transport>);
    let svc = MqttService::start(&app, store, factory, ServiceConfig::default());
    let error = || svc.status().last_error.unwrap_or_default();
    until("CONNACK 5 reported", || {
        error() == "connection refused: not authorized"
    });
    mode.store(2, Ordering::SeqCst);
    until("the close reported", || {
        error()
            == "the broker closed the connection after CONNECT \
                (it may require a user name and password)"
    });
    assert_eq!(svc.status().state, "retrying");
    let seen = seen.lock().unwrap();
    assert!(seen.len() >= 2, "{seen:?}");
    assert!(
        seen.iter().all(|c| *c == (None, false)),
        "anonymous CONNECTs: {seen:?}"
    );
    drop(seen);
    svc.stop();
    app.shutdown();
    ncp.shutdown();
    sim.stop();
}
