//! `hubctl serve`: the WebSocket JSON API (spec device-support 14) of the
//! hub application on a desktop, the embedded web UI on `/` and its tab
//! modules on `/ui/<build>/<name>.js` (as the firmware serves them), and
//! `hubctl ws-call`, a minimal client.

use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use hub_app::App;
use tungstenite::{Message, WebSocket};

fn timed_out(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
}

/// A plain HTTP request (no WebSocket upgrade): the web UI on `/` and
/// `/ui/…`, the web tools on `/zdefc/…`.
/// Returns `false` if the connection is a WebSocket upgrade.
fn http_page(app: &App, stream: &mut TcpStream) -> io::Result<bool> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 4096];
    let mut n = 0;
    while n < buf.len() {
        n = stream.peek(&mut buf)?;
        if n == 0 || buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let head = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
    if head.lines().any(|l| l.starts_with("upgrade:") && l.contains("websocket")) {
        return Ok(false);
    }
    let path = head.split_whitespace().nth(1).unwrap_or("");
    let inm = head.lines().find_map(|l| l.strip_prefix("if-none-match:")).unwrap_or("");
    let mut resp = Vec::new();
    if path == "/" || path == "/index.html" {
        let fresh = hub_web::not_modified(inm);
        write!(resp, "HTTP/1.1 {}\r\nConnection: close\r\n", if fresh { "304 Not Modified" } else { "200 OK" })?;
        for (k, v) in hub_web::HEADERS {
            write!(resp, "{k}: {v}\r\n")?;
        }
        if fresh {
            write!(resp, "\r\n")?;
        } else {
            write!(resp, "Content-Length: {}\r\n\r\n", hub_web::INDEX_HTML_GZ.len())?;
            resp.extend_from_slice(hub_web::INDEX_HTML_GZ);
        }
    } else if let Some(m) = hub_web::module(path) {
        // A tab module of this build (the developer tools only in developer mode).
        if !m.served(app.developer_mode()) {
            write!(resp, "HTTP/1.1 403 Forbidden\r\nConnection: close\r\nCache-Control: no-store\r\nContent-Length: 0\r\n\r\n")?;
        } else if hub_web::matches(inm, m.etag) {
            write!(resp, "HTTP/1.1 304 Not Modified\r\nConnection: close\r\nETag: {}\r\nCache-Control: {}\r\n\r\n", m.etag, m.cache_control())?;
        } else {
            write!(resp, "HTTP/1.1 200 OK\r\nConnection: close\r\n")?;
            for (k, v) in m.headers() {
                write!(resp, "{k}: {v}\r\n")?;
            }
            write!(resp, "Content-Length: {}\r\n\r\n", m.gz.len())?;
            resp.extend_from_slice(m.gz);
        }
    } else if let Some(name) = path.strip_prefix("/zdefc/") {
        // Web tools from hubdata (the definition compiler of the web UI).
        app.web().serve(name, |f| -> io::Result<()> {
            match f {
                Some(f) => {
                    write!(resp, "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: {}\r\nContent-Encoding: gzip\r\nCache-Control: no-cache\r\nETag: {}\r\nContent-Length: {}\r\n\r\n", f.content_type, f.etag, f.data.len())?;
                    resp.extend_from_slice(f.data);
                }
                None => write!(resp, "HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")?,
            }
            Ok(())
        })?;
    } else {
        write!(resp, "HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")?;
    }
    // Consume the peeked request: closing a socket with unread input
    // resets it, which cuts large responses (the web tools) short.
    let _ = io::Read::read(stream, &mut buf[..n]);
    stream.write_all(&resp)?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(true)
}

/// One connection: the web UI, or text frames to the app and the app's
/// frames back.
fn connection(app: App, mut stream: TcpStream) {
    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let _ = stream.set_nodelay(true);
    match http_page(&app, &mut stream) {
        Ok(true) => return,
        Ok(false) => {}
        Err(e) => {
            log::warn!("http {peer}: {e}");
            return;
        }
    }
    let mut ws = match tungstenite::accept(stream) {
        Ok(ws) => ws,
        Err(e) => {
            log::warn!("ws {peer}: handshake: {e}");
            return;
        }
    };
    let _ = ws.get_ref().set_read_timeout(Some(Duration::from_millis(20)));
    let (tx, rx) = mpsc::channel::<String>();
    let Some(sid) = app.connect(Box::new(tx)) else { return };
    log::info!("ws {peer}: session {sid}");
    'conn: loop {
        match ws.read() {
            Ok(Message::Text(t)) => app.text(sid, t.to_string()),
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(e) if timed_out(&e) => {}
            Err(e) => {
                log::info!("ws {peer}: {e}");
                break;
            }
        }
        while let Ok(frame) = rx.try_recv() {
            if let Err(e) = ws.send(Message::text(frame)) {
                log::info!("ws {peer}: {e}");
                break 'conn;
            }
        }
    }
    app.disconnect(sid);
    log::info!("ws {peer}: closed");
}

/// Serve on `addr` until `until` (forever if `None`).
pub fn serve(app: &App, addr: &str, until: Option<Instant>) -> io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    log::info!("web UI on http://{addr}/, WebSocket JSON API on ws://{addr}/api");
    loop {
        if until.is_some_and(|t| Instant::now() >= t) {
            return Ok(());
        }
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                let app = app.clone();
                thread::spawn(move || connection(app, stream));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(e),
        }
    }
}

fn next_text(ws: &mut WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>) -> Result<String, String> {
    loop {
        match ws.read().map_err(|e| e.to_string())? {
            Message::Text(t) => return Ok(t.to_string()),
            Message::Close(_) => return Err("closed".into()),
            _ => {}
        }
    }
}

/// Authenticate, send each request (an `id` is added when missing), print
/// every frame until all results arrived, then for `listen` more seconds.
pub fn ws_call(url: &str, token: &str, requests: &[String], listen: Duration) -> Result<(), String> {
    let (mut ws, _) = tungstenite::connect(url).map_err(|e| format!("{url}: {e}"))?;
    println!("{}", next_text(&mut ws)?);
    ws.send(Message::text(serde_json::json!({"type": "auth", "token": token}).to_string()))
        .map_err(|e| e.to_string())?;
    let ok = next_text(&mut ws)?;
    println!("{ok}");
    if !ok.contains("\"auth_ok\"") {
        return Err("authentication failed".into());
    }
    let mut waiting = Vec::new();
    for (i, r) in requests.iter().enumerate() {
        let mut v: serde_json::Value = serde_json::from_str(r).map_err(|e| format!("{r}: {e}"))?;
        if v.get("id").is_none() {
            v["id"] = (i as u64 + 1).into();
        }
        waiting.push(v["id"].clone());
        ws.send(Message::text(v.to_string())).map_err(|e| e.to_string())?;
    }
    while !waiting.is_empty() {
        let t = next_text(&mut ws)?;
        println!("{t}");
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t)
            && v["type"] == "result"
        {
            waiting.retain(|id| *id != v["id"]);
        }
    }
    if let tungstenite::stream::MaybeTlsStream::Plain(s) = ws.get_ref() {
        let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
    }
    let until = Instant::now() + listen;
    while Instant::now() < until {
        match ws.read() {
            Ok(Message::Text(t)) => println!("{t}"),
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(e) if timed_out(&e) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    let _ = ws.close(None);
    Ok(())
}
