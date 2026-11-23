//! A plain MQTT 3.1.1 client over `std::net::TcpStream` ([`Transport`]) for
//! `hubctl serve` and the tests. One reader thread per connection
//! delivers events and keeps the connection alive (PINGREQ at half the
//! keepalive, failure after 1.5 × keepalive without a packet).

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::bridge::Msg;
use crate::codec::{Packet, Will};
use crate::service::{ConnectOptions, Event, EventFn, Transport};

/// Largest packet taken from the broker by default; larger ones are
/// skipped (a retained giant on a subscribed topic must not cut the
/// connection).
pub const MAX_PACKET: usize = 64 * 1024;

struct Conn {
    w: Mutex<(TcpStream, Instant)>,
    stop: AtomicBool,
    pid: AtomicU16,
}

impl Conn {
    fn write(&self, p: &Packet) -> std::io::Result<()> {
        let mut g = self.w.lock().unwrap();
        g.0.write_all(&p.encode())?;
        g.1 = Instant::now();
        Ok(())
    }
}

/// The client.
pub struct TcpTransport {
    conn: Option<Arc<Conn>>,
    reader: Option<JoinHandle<()>>,
    max: usize,
    stack: usize,
}

impl Default for TcpTransport {
    fn default() -> Self {
        Self {
            conn: None,
            reader: None,
            max: MAX_PACKET,
            stack: 16 * 1024,
        }
    }
}

impl TcpTransport {
    /// A client (not connected).
    pub fn new() -> Self {
        Self::default()
    }

    /// A client taking packets up to `max` bytes, its reader thread with
    /// `stack` bytes of stack (small devices).
    pub fn with_limits(max: usize, stack: usize) -> Self {
        Self {
            conn: None,
            reader: None,
            max,
            stack,
        }
    }
}

fn reader(conn: Arc<Conn>, mut s: TcpStream, keepalive: Duration, max: usize, events: EventFn) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 512];
    let mut skip = 0usize;
    let mut last_rx = Instant::now();
    let fail = |e: String| {
        if !conn.stop.load(Ordering::SeqCst) {
            events(Event::Disconnected(e));
        }
    };
    loop {
        if conn.stop.load(Ordering::SeqCst) {
            return;
        }
        match s.read(&mut chunk) {
            Ok(0) => return fail("the broker closed the connection".into()),
            Ok(n) => {
                last_rx = Instant::now();
                let mut data = &chunk[..n];
                if skip > 0 {
                    let k = skip.min(data.len());
                    skip -= k;
                    data = &data[k..];
                }
                buf.extend_from_slice(data);
                loop {
                    if skip == 0
                        && let Ok(Some((h, len))) = Packet::header(&buf)
                        && len > max
                    {
                        // Too large for us: dropped as it streams by.
                        let have = buf.len().min(h + len);
                        skip = h + len - have;
                        buf.drain(..have);
                        continue;
                    }
                    match Packet::decode(&buf, max) {
                        Ok(Some((p, used))) => {
                            buf.drain(..used);
                            match p {
                                Packet::ConnAck { code: 0, .. } => events(Event::Connected),
                                Packet::ConnAck { code, .. } => {
                                    let why = match code {
                                        1 => "unacceptable protocol version",
                                        2 => "client identifier rejected",
                                        3 => "server unavailable",
                                        4 => "bad user name or password",
                                        5 => "not authorized",
                                        _ => "refused",
                                    };
                                    return fail(format!("connection refused: {why}"));
                                }
                                Packet::Publish {
                                    topic,
                                    payload,
                                    qos,
                                    pid,
                                    ..
                                } => {
                                    if qos > 0 {
                                        let _ = conn.write(&Packet::PubAck(pid));
                                    }
                                    events(Event::Message(topic, payload));
                                }
                                _ => {}
                            }
                        }
                        Ok(None) => break,
                        Err(e) => return fail(e.to_string()),
                    }
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return fail(e.to_string()),
        }
        if !keepalive.is_zero() {
            if last_rx.elapsed() > keepalive + keepalive / 2 {
                return fail("keepalive timeout".into());
            }
            let idle = conn.w.lock().unwrap().1.elapsed();
            if idle >= keepalive / 2 {
                let _ = conn.write(&Packet::PingReq);
            }
        }
    }
}

impl Transport for TcpTransport {
    fn connect(&mut self, o: &ConnectOptions, events: EventFn) -> Result<(), String> {
        self.disconnect();
        let addr = (o.host.as_str(), o.port)
            .to_socket_addrs()
            .map_err(|e| format!("{}: {e}", o.host))?
            .next()
            .ok_or_else(|| format!("{}: no address", o.host))?;
        let s = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
            .map_err(|e| format!("{addr}: {e}"))?;
        let _ = s.set_nodelay(true);
        s.set_read_timeout(Some(Duration::from_millis(200)))
            .map_err(|e| e.to_string())?;
        let r = s.try_clone().map_err(|e| e.to_string())?;
        let conn = Arc::new(Conn {
            w: Mutex::new((s, Instant::now())),
            stop: AtomicBool::new(false),
            pid: AtomicU16::new(1),
        });
        let will = Will {
            topic: o.will.topic.clone(),
            payload: o.will.payload.clone(),
            qos: o.will.qos,
            retain: o.will.retain,
        };
        conn.write(&Packet::Connect {
            client_id: o.client_id.clone(),
            keepalive: o.keepalive_s,
            clean: true,
            username: o.username.clone(),
            password: o.password.clone().map(String::into_bytes),
            will: Some(will),
        })
        .map_err(|e| e.to_string())?;
        let c2 = conn.clone();
        let ka = Duration::from_secs(u64::from(o.keepalive_s));
        let max = self.max;
        let h = thread::Builder::new()
            .name("mqtt-rx".into())
            .stack_size(self.stack)
            .spawn(move || reader(c2, r, ka, max, events))
            .map_err(|e| e.to_string())?;
        self.conn = Some(conn);
        self.reader = Some(h);
        Ok(())
    }

    fn disconnect(&mut self) {
        if let Some(c) = self.conn.take() {
            c.stop.store(true, Ordering::SeqCst);
            let _ = c.write(&Packet::Disconnect);
            let _ = c.w.lock().unwrap().0.shutdown(Shutdown::Both);
        }
        if let Some(h) = self.reader.take()
            && h.thread().id() != thread::current().id()
        {
            let _ = h.join();
        }
    }

    fn publish(&mut self, m: &Msg) -> Result<(), String> {
        let c = self.conn.as_ref().ok_or("not connected")?;
        let pid = if m.qos > 0 {
            c.pid.fetch_add(1, Ordering::Relaxed).max(1)
        } else {
            0
        };
        c.write(&Packet::Publish {
            topic: m.topic.clone(),
            payload: m.payload.clone(),
            qos: m.qos,
            retain: m.retain,
            dup: false,
            pid,
        })
        .map_err(|e| e.to_string())
    }

    fn subscribe(&mut self, filters: &[String]) -> Result<(), String> {
        let c = self.conn.as_ref().ok_or("not connected")?;
        let pid = c.pid.fetch_add(1, Ordering::Relaxed).max(1);
        c.write(&Packet::Subscribe {
            pid,
            filters: filters.iter().map(|f| (f.clone(), 0)).collect(),
        })
        .map_err(|e| e.to_string())
    }
}

impl Drop for TcpTransport {
    fn drop(&mut self) {
        self.disconnect();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::broker::Broker;

    #[test]
    fn oversized_packets_are_skipped_not_fatal() {
        let b = Broker::start("127.0.0.1:0", None).unwrap();
        let (tx, rx) = mpsc::channel();
        let mut c = TcpTransport::with_limits(256, 64 * 1024);
        let opts = ConnectOptions {
            host: "127.0.0.1".into(),
            port: b.addr().port(),
            client_id: "t".into(),
            username: None,
            password: None,
            keepalive_s: 30,
            will: Msg {
                topic: "w".into(),
                payload: Vec::new(),
                qos: 0,
                retain: false,
            },
        };
        c.connect(
            &opts,
            Arc::new(move |e| {
                let _ = tx.send(e);
            }),
        )
        .unwrap();
        let t = Duration::from_secs(5);
        assert_eq!(rx.recv_timeout(t).unwrap(), Event::Connected);
        c.subscribe(&["t/#".into()]).unwrap();
        // The SUBSCRIBE reaches the broker before its first publication.
        thread::sleep(Duration::from_millis(200));
        b.publish("t/big", &[b'x'; 5000], false);
        b.publish("t/small", b"ok", false);
        assert_eq!(
            rx.recv_timeout(t).unwrap(),
            Event::Message("t/small".into(), b"ok".to_vec())
        );
        c.publish(&Msg {
            topic: "t/after".into(),
            payload: b"1".to_vec(),
            qos: 1,
            retain: false,
        })
        .unwrap();
        assert_eq!(
            rx.recv_timeout(t).unwrap(),
            Event::Message("t/after".into(), b"1".to_vec())
        );
    }
}
