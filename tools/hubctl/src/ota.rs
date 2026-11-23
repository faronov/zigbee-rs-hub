//! OTA Upgrade helpers of `hubctl`:
//!
//! - `ota-upload`: the upload client the web UI and the Home Assistant
//!   integration also implement (device-support 14.7): checks the file,
//!   cuts a vendor prefix before the OTA header, then `ota/upload/begin`,
//!   `ota/upload/chunk`s (the hub's `chunk_max`, 4 KiB) from where the hub
//!   says, `ota/upload/commit`.
//!   After a dropped connection the same command continues the upload.
//! - `ota-make`: wraps a firmware image into a Zigbee OTA file (one
//!   upgrade-image sub-element), e.g. for zigbee-rs devices.
//! - `--sim-ota`: a simulated device with an OTA client on the simulated
//!   NCP, for trying the OTA tab and the API without hardware.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use hub_ncp::sim::{SimAps, SimNcp, indication};
use hub_ota::file;
use hub_ota::sim::SimClient;
use serde_json::{Value as J, json};
use sha2::{Digest, Sha256};
use tungstenite::{Message, WebSocket, stream::MaybeTlsStream};

type Ws = WebSocket<MaybeTlsStream<std::net::TcpStream>>;

const CHUNK: usize = 8 * 1024;

fn next(ws: &mut Ws) -> Result<J, String> {
    loop {
        match ws.read().map_err(|e| e.to_string())? {
            Message::Text(t) => return serde_json::from_str(&t).map_err(|e| e.to_string()),
            Message::Close(_) => return Err("connection closed".into()),
            _ => {}
        }
    }
}

fn call(ws: &mut Ws, id: &mut u64, mut req: J) -> Result<J, String> {
    *id += 1;
    req["id"] = (*id).into();
    ws.send(Message::text(req.to_string())).map_err(|e| e.to_string())?;
    loop {
        let m = next(ws)?;
        if m["type"] == "result" && m["id"] == *id {
            return if m["success"] == true { Ok(m["result"].clone()) } else { Err(format!("{}: {}", req["type"], m["error"])) };
        }
    }
}

/// The OTA file inside `data` (a vendor prefix before the header is cut)
/// and its check.
pub fn prepare(data: &[u8]) -> Result<(&[u8], file::Checked), String> {
    let at = file::find_magic(&data[..data.len().min(4096)]).ok_or("not a Zigbee OTA file (no 0x0BEEF11E header)")?;
    let f = &data[at..];
    let c = file::check(f).map_err(|e| e.to_string())?;
    Ok((f, c))
}

/// `hubctl ota-upload URL --token T FILE`.
pub fn upload(url: &str, token: &str, path: &str, name: Option<String>, versions: (Option<u32>, Option<u32>), replace: bool) -> Result<(), String> {
    let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let (f, c) = prepare(&data)?;
    let h = &c.header;
    println!(
        "{path}: manufacturer 0x{:04X} image type 0x{:04X} file version 0x{:08X} {} B{}{}",
        h.manufacturer,
        h.image_type,
        h.file_version,
        f.len(),
        if f.len() != data.len() { format!(" (a {} B prefix cut)", data.len() - f.len()) } else { String::new() },
        if h.label().is_empty() { String::new() } else { format!(" \"{}\"", h.label()) }
    );
    for w in &c.warnings {
        println!("warning: {w}");
    }
    let sha: [u8; 32] = Sha256::digest(f).into();
    let sha: String = sha.iter().map(|b| format!("{b:02x}")).collect();
    let (mut ws, _) = tungstenite::connect(url).map_err(|e| format!("{url}: {e}"))?;
    next(&mut ws)?;
    ws.send(Message::text(json!({"type": "auth", "token": token}).to_string())).map_err(|e| e.to_string())?;
    if next(&mut ws)?["type"] != "auth_ok" {
        return Err("authentication failed".into());
    }
    let mut id = 0;
    let name = name.unwrap_or_else(|| std::path::Path::new(path).file_name().map_or("image.ota".into(), |n| n.to_string_lossy().into_owned()));
    let mut begin = json!({"type": "ota/upload/begin", "size": f.len(), "name": name, "sha256": sha, "replace": replace});
    if let Some(v) = versions.0 {
        begin["min_file_version"] = v.into();
    }
    if let Some(v) = versions.1 {
        begin["max_file_version"] = v.into();
    }
    let b = call(&mut ws, &mut id, begin)?;
    let up = b["upload"].clone();
    let chunk = b["chunk_max"].as_u64().map_or(CHUNK, |c| c as usize).min(CHUNK);
    let mut at = b["offset"].as_u64().unwrap_or(0) as usize;
    if at > 0 {
        println!("continuing at {at}");
    }
    let t = Instant::now();
    while at < f.len() {
        let end = (at + chunk).min(f.len());
        let data = base64::engine::general_purpose::STANDARD.encode(&f[at..end]);
        call(&mut ws, &mut id, json!({"type": "ota/upload/chunk", "upload": up, "offset": at, "data": data}))?;
        at = end;
        eprint!("\r{at}/{} B", f.len());
    }
    eprintln!();
    let r = call(&mut ws, &mut id, json!({"type": "ota/upload/commit", "upload": up}))?;
    println!("stored as image {} in {:.1} s; updates {}", r["image"], t.elapsed().as_secs_f64(), r["devices"]);
    let _ = ws.close(None);
    Ok(())
}

/// `hubctl ota-make`: an OTA file of `image`.
pub fn make(image: &[u8], manufacturer: u16, image_type: u16, version: u32, label: &str, hw: Option<(u16, u16)>) -> Vec<u8> {
    file::build(manufacturer, image_type, version, label, image, hw)
}

/// How a stream session ended.
enum Ended {
    /// The device runs the image.
    Done,
    /// The connection is gone (hub restart, Wi-Fi): connect again.
    Dropped(String),
}

/// No frame for this long: ping the hub.
const PING_AFTER: Duration = Duration::from_secs(10);
/// No frame (not even the pong) for this long: the connection is dead.
const DEAD_AFTER: Duration = Duration::from_secs(25);

/// `hubctl ota-stream URL --token T FILE [--device ID]`: offers the file
/// as a stream (no storage on the hub, the 4 MB layout's way) and serves
/// `ota/block_needed` until the device runs it (or forever without a
/// device). A dropped connection (a hub restart, Wi-Fi) is noticed within
/// 25 s (ping) and the command connects again, offers the file again and
/// notifies the device again; the device continues from its own offset.
pub fn stream(url: &str, token: &str, path: &str, device: Option<String>) -> Result<(), String> {
    let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let (f, c) = prepare(&data)?;
    let h = &c.header;
    println!(
        "{path}: manufacturer 0x{:04X} image type 0x{:04X} file version 0x{:08X} {} B",
        h.manufacturer,
        h.image_type,
        h.file_version,
        f.len()
    );
    let name = std::path::Path::new(path).file_name().map_or("image.ota".into(), |n| n.to_string_lossy().into_owned());
    let mut served = 0usize;
    let mut connected_once = false;
    loop {
        match stream_session(url, token, f, &name, device.as_deref(), &mut served, &mut connected_once)? {
            Ended::Done => return Ok(()),
            // Never got through: the URL or the hub is wrong.
            Ended::Dropped(why) if !connected_once => return Err(why),
            Ended::Dropped(why) => println!("connection lost ({why}); connecting again"),
        }
        let mut backoff = Duration::from_secs(1);
        loop {
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_secs(10));
            if url_reachable(url) {
                break;
            }
        }
    }
}

/// The hub's TCP port answers.
fn url_reachable(url: &str) -> bool {
    use std::net::ToSocketAddrs;
    let host = url.split("://").nth(1).unwrap_or(url).split('/').next().unwrap_or("");
    let host = if host.contains(':') { host.to_string() } else { format!("{host}:80") };
    host.to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .is_some_and(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_secs(2)).is_ok())
}

/// Why a read or call failed.
enum Fail {
    /// The connection is gone.
    Dropped(String),
    /// The hub answered with an error.
    Refused(String),
}

/// One connection: offer, notify, serve. `Err`: give up (a refused file,
/// authentication, the job failed); `Dropped`: connect again.
fn stream_session(
    url: &str,
    token: &str,
    f: &[u8],
    name: &str,
    device: Option<&str>,
    served: &mut usize,
    connected_once: &mut bool,
) -> Result<Ended, String> {
    let mut ws = match tungstenite::connect(url) {
        Ok((ws, _)) => ws,
        Err(e) => return Ok(Ended::Dropped(format!("{url}: {e}"))),
    };
    if let MaybeTlsStream::Plain(s) = ws.get_ref() {
        let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
    }
    let mut live = Live::new();
    match serve_on(&mut ws, &mut live, token, f, name, device, served, connected_once) {
        Ok(()) => Ok(Ended::Done),
        Err(Fail::Dropped(e)) => Ok(Ended::Dropped(e)),
        Err(Fail::Refused(e)) => Err(e),
    }
}

#[allow(clippy::too_many_arguments)]
fn serve_on(
    ws: &mut Ws,
    live: &mut Live,
    token: &str,
    f: &[u8],
    name: &str,
    device: Option<&str>,
    served: &mut usize,
    connected_once: &mut bool,
) -> Result<(), Fail> {
    let mut id = 0;
    live.next(ws)?;
    send(ws, json!({"type": "auth", "token": token}))?;
    if live.next(ws)?["type"] != "auth_ok" {
        return Err(Fail::Refused("authentication failed".into()));
    }
    *connected_once = true;
    live.call(ws, &mut id, json!({"type": "subscribe", "topics": ["ota"]}))?;
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let head = &f[..f.len().min(256)];
    let o = live.call(ws, &mut id, json!({"type": "ota/stream/offer", "size": f.len(), "header": b64(head), "name": name}))?;
    let session = o["session"].clone();
    println!("stream session {session}, for {}", o["devices"]);
    if let Some(d) = device {
        // A running download (this command run again, or connected again
        // after a hub restart whose ABORT made the device wait for a
        // notify): notify again; the device resumes from its own offset.
        let start = json!({"type": "ota/start", "device": d, "image": session});
        match live.call(ws, &mut id, start.clone()) {
            Err(Fail::Refused(e)) if e.contains("busy") => {
                let mut again = start;
                again["force"] = true.into();
                live.call(ws, &mut id, again)?;
                println!("{d}: running download, notified again");
            }
            r => {
                r?;
                println!("{d}: notified");
            }
        }
    }
    let mut last = String::new();
    loop {
        let m = live.next(ws)?;
        if m["type"] == "ota/block_needed" && m["session"] == session {
            let off = m["offset"].as_u64().unwrap_or(0) as usize;
            let end = (off + m["len"].as_u64().unwrap_or(0) as usize).min(f.len());
            id += 1;
            let req = json!({"id": id, "type": "ota/block_data", "session": session, "offset": off, "data": b64(&f[off.min(end)..end])});
            send(ws, req)?;
            *served += end.saturating_sub(off);
            continue;
        }
        if m["type"] == "result" && m["success"] == false {
            println!("error: {}", m["error"]);
            continue;
        }
        if m["event"]["topic"] != "ota_device" {
            continue;
        }
        let d = &m["event"]["device"];
        let this_device = device.is_none_or(|x| d["device"] == x);
        // A job that ended without the stream (aborted after the grace
        // time) no longer names the session: match it by version.
        let mine = this_device && (d["job"]["image"] == session || (device.is_some() && d["job"]["file_version"] == o["file_version"]));
        if !mine {
            continue;
        }
        let state = d["job"]["state"].as_str().unwrap_or("?").to_string();
        let line = format!("{state} {} % ({served} B served)", d["job"]["progress"]);
        if line != last {
            println!("{}: {line}", d["device"].as_str().unwrap_or("?"));
            last = line;
        }
        if device.is_some() && state == "done" {
            return Ok(());
        }
        if device.is_some() && (state == "failed" || state == "aborted") {
            return Err(Fail::Refused(format!("{state}: {}", d["job"]["error"])));
        }
    }
}

fn send(ws: &mut Ws, req: J) -> Result<(), Fail> {
    ws.send(Message::text(req.to_string())).map_err(|e| Fail::Dropped(e.to_string()))
}

/// Liveness: pings an idle connection, gives up on a silent one.
struct Live {
    last_rx: Instant,
    pinged: bool,
}

impl Live {
    fn new() -> Self {
        Self { last_rx: Instant::now(), pinged: false }
    }

    fn next(&mut self, ws: &mut Ws) -> Result<J, Fail> {
        use tungstenite::Error as E;
        loop {
            match ws.read() {
                Ok(m) => {
                    self.last_rx = Instant::now();
                    self.pinged = false;
                    match m {
                        Message::Text(t) => return serde_json::from_str(&t).map_err(|e| Fail::Dropped(e.to_string())),
                        Message::Close(_) => return Err(Fail::Dropped("connection closed".into())),
                        _ => {}
                    }
                }
                Err(E::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    let idle = self.last_rx.elapsed();
                    if idle >= DEAD_AFTER {
                        return Err(Fail::Dropped(format!("no answer for {} s", idle.as_secs())));
                    }
                    if idle >= PING_AFTER && !self.pinged {
                        self.pinged = true;
                        ws.send(Message::Ping(Vec::new().into())).map_err(|e| Fail::Dropped(e.to_string()))?;
                    }
                }
                Err(e) => return Err(Fail::Dropped(e.to_string())),
            }
        }
    }

    fn call(&mut self, ws: &mut Ws, id: &mut u64, mut req: J) -> Result<J, Fail> {
        *id += 1;
        req["id"] = (*id).into();
        send(ws, req.clone())?;
        loop {
            let m = self.next(ws)?;
            if m["type"] == "result" && m["id"] == *id {
                return if m["success"] == true {
                    Ok(m["result"].clone())
                } else {
                    Err(Fail::Refused(format!("{}: {}", req["type"], m["error"])))
                };
            }
        }
    }
}

/// `--sim-ota`: a device (NWK 0x4F01) joins the simulated network with an
/// OTA client (manufacturer 0x6D6B, image type 0x0102, version
/// 0x00010000), queries once, and answers the hub from then on.
pub fn sim_device(sim: &SimNcp) {
    const NWK: u16 = 0x4F01;
    const IEEE: u64 = 0x6D6B_0000_0000_4F01;
    let client = Arc::new(Mutex::new(SimClient::new(0x6D6B, 0x0102, 0x0001_0000)));
    let c = client.clone();
    let t0 = Instant::now();
    sim.set_aps_hook(Box::new(move |a: &SimAps<'_>| {
        if a.dst != NWK || a.cluster != hub_ota::server::CLUSTER {
            return Vec::new();
        }
        let now = t0.elapsed().as_millis() as u64;
        let frames = c.lock().unwrap().on_zcl(a.asdu, now);
        frames.into_iter().map(|f| indication(NWK, IEEE, a.dst_ep, a.src_ep, 0x0104, hub_ota::server::CLUSTER, &f)).collect()
    }));
    let joined = hub_ncp::zrhp::msg::network::EvtDeviceJoined {
        nwk: NWK,
        ieee: hub_ncp::zrhp::types::Ieee::from_u64(IEEE),
        parent_nwk: 0,
        join_type: 0,
        capability: 0x8E,
        decision: 0,
    };
    let sim = sim.clone();
    std::thread::Builder::new()
        .name("sim-ota".into())
        .spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            sim.inject_event(hub_ncp::zrhp::msg::ids::EVT_DEVICE_JOINED, hub_ncp::driver::encode_body(&joined).unwrap());
            std::thread::sleep(Duration::from_secs(2));
            let q = hub_ota::sim::frame(client.lock().unwrap().query());
            sim.inject_event(hub_ncp::zrhp::msg::ids::EVT_APS_DATA_INDICATION, indication(NWK, IEEE, 1, 1, 0x0104, hub_ota::server::CLUSTER, &q));
            log::info!("sim-ota: device 0x{NWK:04X} (zb:{IEEE:016x}) joined, OTA client 0x6D6B/0x0102 at 0x00010000");
            // WAIT_FOR_DATA delays.
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let now = t0.elapsed().as_millis() as u64;
                for f in client.lock().unwrap().tick_zcl(now) {
                    sim.inject_event(hub_ncp::zrhp::msg::ids::EVT_APS_DATA_INDICATION, indication(NWK, IEEE, 1, 1, 0x0104, hub_ota::server::CLUSTER, &f));
                }
            }
        })
        .expect("sim-ota thread");
}
