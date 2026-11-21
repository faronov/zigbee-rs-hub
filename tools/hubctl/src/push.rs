//! `hubctl catalog-push`: the catalog client the web UI and the Home
//! Assistant integration also implement (device-support 9.3/14.4). The hub
//! downloads nothing; this fetches the signed release files from the
//! catalog mirror (GitHub Pages, or a directory) and uploads them:
//!
//! 1. `manifest.cose` → target `manifest`; the answer's `wants` names the
//!    bank for the hub's slot size (if newer) and the web tools (if they
//!    differ);
//! 2. those files → targets `bank` / `web`;
//! 3. `catalog/status` `missing`: devices that resolve to generic only and
//!    the shards holding their keys → each `shard-hh.cbor` → target `shard`.
//!
//! Uploads go in 12 KiB chunks; after a dropped connection the next run
//! continues where `catalog/status` `upload.written` says.

use std::io::Read;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value as J, json};
use tungstenite::{Message, WebSocket, stream::MaybeTlsStream};

type Ws = WebSocket<MaybeTlsStream<std::net::TcpStream>>;

const CHUNK: usize = 12 * 1024;

struct Hub {
    ws: Ws,
    id: u64,
}

impl Hub {
    fn connect(url: &str, token: &str) -> Result<Hub, String> {
        let (ws, _) = tungstenite::connect(url).map_err(|e| format!("{url}: {e}"))?;
        let mut h = Hub { ws, id: 0 };
        h.next()?;
        h.ws.send(Message::text(json!({"type": "auth", "token": token}).to_string())).map_err(|e| e.to_string())?;
        if h.next()?["type"] != "auth_ok" {
            return Err("authentication failed".into());
        }
        Ok(h)
    }

    fn next(&mut self) -> Result<J, String> {
        loop {
            match self.ws.read().map_err(|e| e.to_string())? {
                Message::Text(t) => return serde_json::from_str(&t).map_err(|e| e.to_string()),
                Message::Close(_) => return Err("connection closed".into()),
                _ => {}
            }
        }
    }

    fn call(&mut self, mut req: J) -> Result<J, String> {
        self.id += 1;
        req["id"] = self.id.into();
        self.ws.send(Message::text(req.to_string())).map_err(|e| e.to_string())?;
        loop {
            let m = self.next()?;
            if m["type"] == "result" && m["id"] == self.id {
                return if m["success"] == true {
                    Ok(m["result"].clone())
                } else {
                    Err(format!("{}: {}", req["type"], m["error"]["message"].as_str().unwrap_or("failed")))
                };
            }
        }
    }

    fn upload(&mut self, target: &str, bytes: &[u8], from: usize, commit: J) -> Result<J, String> {
        let mut at = from;
        while at < bytes.len() || at == 0 {
            let end = (at + CHUNK).min(bytes.len());
            let mut req = json!({"type": "catalog/upload_chunk", "target": target, "offset": at,
                "data": base64::engine::general_purpose::STANDARD.encode(&bytes[at..end])});
            if at == 0 {
                req["size"] = bytes.len().into();
            }
            self.call(req)?;
            at = end;
            if bytes.is_empty() {
                break;
            }
        }
        let mut c = commit;
        c["type"] = "catalog/commit".into();
        c["target"] = target.into();
        self.call(c)
    }
}

/// Where the release files come from.
pub enum Source {
    /// Base URL (`…/latest`).
    Url(String),
    /// A release directory.
    Dir(std::path::PathBuf),
}

impl Source {
    fn get(&self, name: &str) -> Result<Vec<u8>, String> {
        match self {
            Source::Dir(d) => std::fs::read(d.join(name)).map_err(|e| format!("{name}: {e}")),
            Source::Url(base) => {
                let url = format!("{}/{name}", base.trim_end_matches('/'));
                let agent =
                    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(60))).build().new_agent();
                let resp = agent.get(&url).call().map_err(|e| format!("{url}: {e}"))?;
                let mut v = Vec::new();
                resp.into_body().into_reader().take(4 << 20).read_to_end(&mut v).map_err(|e| format!("{url}: {e}"))?;
                Ok(v)
            }
        }
    }
}

/// Runs the client against the hub at `url`.
pub fn push(url: &str, token: &str, source: Option<Source>, fetch: bool) -> Result<(), String> {
    let mut hub = Hub::connect(url, token)?;
    let st = hub.call(json!({"type": "catalog/status"}))?;
    let src = source.unwrap_or_else(|| Source::Url(st["config"]["base_url"].as_str().unwrap_or_default().to_string()));
    let m = hub.upload("manifest", &src.get("manifest.cose")?, 0, json!({}))?;
    println!("manifest {} installed", m["version"]);
    let resume = |st: &J, target: &str| {
        (st["upload"]["target"] == target).then(|| st["upload"]["written"].as_u64().unwrap_or(0) as usize).unwrap_or(0)
    };
    for target in ["bank", "web"] {
        let Some(name) = m["wants"][target]["name"].as_str() else { continue };
        let bytes = src.get(name)?;
        let from = resume(&st, target).min(bytes.len());
        let r = hub.upload(target, &bytes, from, json!({}))?;
        println!("{name} ({} B) installed: {r}", bytes.len());
    }
    if fetch {
        let st = hub.call(json!({"type": "catalog/status"}))?;
        let mut shards: Vec<u64> = st["missing"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|d| d["shards"].as_array().cloned().unwrap_or_default())
            .filter_map(|n| n.as_u64())
            .collect();
        shards.sort_unstable();
        shards.dedup();
        for n in shards {
            let r = hub.upload("shard", &src.get(&format!("shard-{n:02x}.cbor"))?, 0, json!({"shard": n}))?;
            println!("shard {n:02x}: {}", r["definitions"]);
        }
    }
    let _ = hub.ws.close(None);
    Ok(())
}
