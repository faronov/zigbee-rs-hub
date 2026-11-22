//! A small MQTT 3.1.1 broker for tests and local development (`hubctl
//! mqtt-broker`): QoS 0 delivery (QoS 1 publishes are acknowledged),
//! retained messages, wills, `+`/`#` filters, optional user name and
//! password. Not for production use.

use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::codec::{Packet, Will, topic_matches};

struct Client {
    w: TcpStream,
    subs: Vec<String>,
    id: String,
}

#[derive(Default)]
struct State {
    clients: BTreeMap<u64, Client>,
    retained: BTreeMap<String, Vec<u8>>,
    log: Vec<(String, Vec<u8>, bool)>,
    next: u64,
    connects: u32,
}

/// A running broker (stops when dropped).
pub struct Broker {
    addr: SocketAddr,
    st: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
}

fn route(st: &mut State, topic: &str, payload: &[u8], retain: bool) {
    st.log.push((topic.to_string(), payload.to_vec(), retain));
    if retain {
        if payload.is_empty() {
            st.retained.remove(topic);
        } else {
            st.retained.insert(topic.to_string(), payload.to_vec());
        }
    }
    let p = Packet::Publish {
        topic: topic.into(),
        payload: payload.to_vec(),
        qos: 0,
        retain: false,
        dup: false,
        pid: 0,
    }
    .encode();
    for c in st.clients.values_mut() {
        if c.subs.iter().any(|f| topic_matches(f, topic)) {
            let _ = c.w.write_all(&p);
        }
    }
}

fn session(
    st: Arc<Mutex<State>>,
    mut s: TcpStream,
    auth: Option<(String, String)>,
    stop: Arc<AtomicBool>,
) {
    let _ = s.set_read_timeout(Some(Duration::from_millis(100)));
    let Ok(w) = s.try_clone() else { return };
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut me: Option<u64> = None;
    let mut will: Option<Will> = None;
    let mut clean = false;
    'outer: while !stop.load(Ordering::SeqCst) {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if me.is_some_and(|id| !st.lock().unwrap().clients.contains_key(&id)) {
                    break; // kicked
                }
                continue;
            }
            Err(_) => break,
        }
        loop {
            let (p, used) = match Packet::decode(&buf, 1 << 20) {
                Ok(Some(x)) => x,
                Ok(None) => break,
                Err(_) => break 'outer,
            };
            buf.drain(..used);
            let mut g = st.lock().unwrap();
            match p {
                Packet::Connect {
                    client_id,
                    username,
                    password,
                    will: w2,
                    ..
                } => {
                    let ok = match &auth {
                        None => true,
                        Some((u, pw)) => {
                            username.as_deref() == Some(u)
                                && password.as_deref() == Some(pw.as_bytes())
                        }
                    };
                    let mut ws = w.try_clone().expect("clone");
                    let _ = ws.write_all(
                        &Packet::ConnAck {
                            session_present: false,
                            code: if ok { 0 } else { 4 },
                        }
                        .encode(),
                    );
                    if !ok {
                        break 'outer;
                    }
                    // A second connection with the same ID replaces the first.
                    let dup: Vec<u64> = g
                        .clients
                        .iter()
                        .filter(|(_, c)| c.id == client_id)
                        .map(|(k, _)| *k)
                        .collect();
                    for k in dup {
                        if let Some(c) = g.clients.remove(&k) {
                            let _ = c.w.shutdown(Shutdown::Both);
                        }
                    }
                    g.next += 1;
                    g.connects += 1;
                    let id = g.next;
                    g.clients.insert(
                        id,
                        Client {
                            w: ws,
                            subs: Vec::new(),
                            id: client_id,
                        },
                    );
                    me = Some(id);
                    will = w2;
                }
                Packet::Subscribe { pid, filters } => {
                    let Some(id) = me else { break 'outer };
                    let codes = vec![0; filters.len()];
                    let retained: Vec<(String, Vec<u8>)> = g
                        .retained
                        .iter()
                        .filter(|(t, _)| filters.iter().any(|(f, _)| topic_matches(f, t)))
                        .map(|(t, p)| (t.clone(), p.clone()))
                        .collect();
                    if let Some(c) = g.clients.get_mut(&id) {
                        c.subs.extend(filters.into_iter().map(|(f, _)| f));
                        let _ = c.w.write_all(&Packet::SubAck { pid, codes }.encode());
                        for (t, p) in retained {
                            let _ = c.w.write_all(
                                &Packet::Publish {
                                    topic: t,
                                    payload: p,
                                    qos: 0,
                                    retain: true,
                                    dup: false,
                                    pid: 0,
                                }
                                .encode(),
                            );
                        }
                    }
                }
                Packet::Unsubscribe { pid, filters } => {
                    if let Some(c) = me.and_then(|id| g.clients.get_mut(&id)) {
                        c.subs.retain(|s| !filters.contains(s));
                        let _ = c.w.write_all(&Packet::UnsubAck(pid).encode());
                    }
                }
                Packet::Publish {
                    topic,
                    payload,
                    qos,
                    retain,
                    pid,
                    ..
                } => {
                    if qos > 0
                        && let Some(c) = me.and_then(|id| g.clients.get_mut(&id))
                    {
                        let _ = c.w.write_all(&Packet::PubAck(pid).encode());
                    }
                    route(&mut g, &topic, &payload, retain);
                }
                Packet::PingReq => {
                    if let Some(c) = me.and_then(|id| g.clients.get_mut(&id)) {
                        let _ = c.w.write_all(&Packet::PingResp.encode());
                    }
                }
                Packet::Disconnect => {
                    clean = true;
                    break 'outer;
                }
                _ => {}
            }
        }
    }
    let mut g = st.lock().unwrap();
    if let Some(id) = me {
        g.clients.remove(&id);
    }
    if !clean && let Some(w) = will {
        route(&mut g, &w.topic, &w.payload, w.retain);
    }
    let _ = s.shutdown(Shutdown::Both);
}

impl Broker {
    /// Listens on `addr` (`127.0.0.1:0`: any free port).
    pub fn start(addr: &str, auth: Option<(String, String)>) -> std::io::Result<Self> {
        let l = TcpListener::bind(addr)?;
        l.set_nonblocking(true)?;
        let addr = l.local_addr()?;
        let st = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (st2, stop2) = (st.clone(), stop.clone());
        thread::Builder::new()
            .name("mqtt-broker".into())
            .spawn(move || {
                while !stop2.load(Ordering::SeqCst) {
                    match l.accept() {
                        Ok((s, _)) => {
                            let _ = s.set_nonblocking(false);
                            let (st3, a, stop3) = (st2.clone(), auth.clone(), stop2.clone());
                            let _ = thread::Builder::new()
                                .name("mqtt-broker-conn".into())
                                .spawn(move || session(st3, s, a, stop3));
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10))
                        }
                        Err(_) => thread::sleep(Duration::from_millis(50)),
                    }
                }
            })?;
        Ok(Self { addr, st, stop })
    }

    /// The address it listens on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The retained message of a topic.
    pub fn retained(&self, topic: &str) -> Option<Vec<u8>> {
        self.st.lock().unwrap().retained.get(topic).cloned()
    }

    /// Retained topics matching a filter.
    pub fn retained_matching(&self, filter: &str) -> Vec<(String, Vec<u8>)> {
        self.st
            .lock()
            .unwrap()
            .retained
            .iter()
            .filter(|(t, _)| topic_matches(filter, t))
            .map(|(t, p)| (t.clone(), p.clone()))
            .collect()
    }

    /// Every publication so far: `(topic, payload, retain)`.
    pub fn published(&self) -> Vec<(String, Vec<u8>, bool)> {
        self.st.lock().unwrap().log.clone()
    }

    /// Publishes as the broker itself (a client's command).
    pub fn publish(&self, topic: &str, payload: &[u8], retain: bool) {
        route(&mut self.st.lock().unwrap(), topic, payload, retain);
    }

    /// Accepted connections so far.
    pub fn connects(&self) -> u32 {
        self.st.lock().unwrap().connects
    }

    /// Connected clients.
    pub fn clients(&self) -> usize {
        self.st.lock().unwrap().clients.len()
    }

    /// Drops every client connection abruptly (wills are published).
    pub fn kick(&self) {
        let mut g = self.st.lock().unwrap();
        for c in g.clients.values() {
            let _ = c.w.shutdown(Shutdown::Both);
        }
        g.clients.clear();
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.kick();
    }
}
