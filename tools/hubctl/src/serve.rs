//! `hubctl serve`: the WebSocket JSON API (spec device-support 14) of the
//! hub application on a desktop, and `hubctl ws-call`, a minimal client.

use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use hub_app::App;
use tungstenite::{Message, WebSocket};

fn timed_out(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
}

/// One connection: text frames to the app, the app's frames back.
fn connection(app: App, stream: TcpStream) {
    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let _ = stream.set_nodelay(true);
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
    log::info!("WebSocket JSON API on ws://{addr}/");
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
