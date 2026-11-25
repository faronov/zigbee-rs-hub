//! The hub side of `hubctl import`: the document in requests of at most
//! [`BATCH`] bytes (the hub takes a request of 16 KiB and holds it three
//! times while handling it), then the preview's table or the apply's
//! result.

use std::time::Duration;

use serde_json::{Value as J, json};
use tungstenite::{Message, WebSocket, stream::MaybeTlsStream};

type Ws = WebSocket<MaybeTlsStream<std::net::TcpStream>>;

/// Bytes of the arrays of one request at most.
pub const BATCH: usize = 6 * 1024;

/// What to do on the hub.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// `network/import` instead of the preview.
    pub apply: bool,
    pub force: bool,
    pub confirm: bool,
    pub confirm_ieee_write_once: bool,
    /// `allow_ieee_change: false`.
    pub keep_ieee: bool,
    pub skip_backup: bool,
    pub remove_missing: bool,
    /// `auto`, `always`, `never`.
    pub reconfigure: Option<String>,
    /// Print the answers as JSON.
    pub json: bool,
}

/// The requests of a document: the first one with `document` (its header,
/// the network part without its devices, the groups), then batches of
/// `network_devices` and `devices`; `more` on all but the last.
pub fn requests(doc: &J, ty: &str, o: &Options) -> Vec<J> {
    let mut head = doc.clone();
    let mut net_devices = Vec::new();
    if let Some(n) = head.get_mut("network").and_then(J::as_object_mut) {
        net_devices = n
            .remove("devices")
            .and_then(|d| d.as_array().cloned())
            .unwrap_or_default();
        n.insert("devices".into(), json!([]));
    }
    let devices = head
        .as_object_mut()
        .and_then(|h| h.remove("devices"))
        .and_then(|d| d.as_array().cloned())
        .unwrap_or_default();
    let mut first = json!({"type": ty, "document": head});
    for (k, v) in [
        ("force", o.force),
        ("confirm", o.confirm),
        ("confirm_ieee_write_once", o.confirm_ieee_write_once),
        ("skip_backup", o.skip_backup),
        ("remove_missing", o.remove_missing),
    ] {
        if v {
            first[k] = true.into();
        }
    }
    if o.keep_ieee {
        first["allow_ieee_change"] = false.into();
    }
    if let Some(r) = &o.reconfigure {
        first["reconfigure"] = r.clone().into();
    }
    let mut out = vec![first];
    for (key, list) in [("network_devices", net_devices), ("devices", devices)] {
        let mut batch: Vec<J> = Vec::new();
        let mut size = 0;
        for e in list {
            let n = e.to_string().len() + 1;
            if !batch.is_empty() && size + n > BATCH {
                out.push(json!({"type": ty, key: std::mem::take(&mut batch)}));
                size = 0;
            }
            size += n;
            batch.push(e);
        }
        if !batch.is_empty() {
            out.push(json!({"type": ty, key: batch}));
        }
    }
    let last = out.len() - 1;
    for (i, r) in out.iter_mut().enumerate() {
        r["more"] = (i < last).into();
    }
    out
}

struct Hub {
    ws: Ws,
    id: u64,
}

impl Hub {
    fn connect(url: &str, token: &str) -> Result<Hub, String> {
        let (ws, _) = tungstenite::connect(url).map_err(|e| format!("{url}: {e}"))?;
        let mut h = Hub { ws, id: 0 };
        h.next()?;
        h.ws.send(Message::text(
            json!({"type": "auth", "token": token}).to_string(),
        ))
        .map_err(|e| e.to_string())?;
        if h.next()?["type"] != "auth_ok" {
            return Err("authentication failed".into());
        }
        if let MaybeTlsStream::Plain(s) = h.ws.get_ref() {
            // A restore with NCP resets takes a while.
            let _ = s.set_read_timeout(Some(Duration::from_secs(300)));
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
        self.ws
            .send(Message::text(req.to_string()))
            .map_err(|e| e.to_string())?;
        loop {
            let m = self.next()?;
            if m["type"] == "result" && m["id"] == self.id {
                return if m["success"] == true {
                    Ok(m["result"].clone())
                } else {
                    let e = &m["error"];
                    Err(format!(
                        "{}: {}",
                        e["code"].as_str().unwrap_or("failed"),
                        e["message"].as_str().unwrap_or("")
                    ))
                };
            }
        }
    }
}

fn s(v: &J) -> String {
    match v {
        J::Null => "-".into(),
        J::String(s) => s.clone(),
        v => v.to_string(),
    }
}

/// One line per device row.
pub fn table(rows: &[J]) -> String {
    let mut t = String::new();
    for r in rows {
        let def = r["definition"].get("id").map_or_else(
            || {
                if r["status"] == "skip" {
                    s(&r["reason"])
                } else {
                    "generic".into()
                }
            },
            s,
        );
        t += &format!(
            "{:<17} {:<8} {:<28} {:<24} {:<32} {}\n",
            s(&r["ieee"]),
            s(&r["status"]),
            s(&r["name"]).chars().take(28).collect::<String>(),
            s(&r["model"]).chars().take(24).collect::<String>(),
            def,
            if r["entities"].is_null() {
                String::new()
            } else {
                format!("{} entities", r["entities"])
            }
        );
    }
    t
}

/// Preview or apply `doc` on the hub at `url`.
pub fn run(url: &str, token: &str, doc: &J, o: &Options) -> Result<(), String> {
    let ty = if o.apply {
        "network/import"
    } else {
        "network/import/preview"
    };
    let mut hub = Hub::connect(url, token)?;
    let reqs = requests(doc, ty, o);
    let n = reqs.len();
    let mut rows: Vec<J> = Vec::new();
    let (mut first_net, mut last) = (None, J::Null);
    for (i, r) in reqs.into_iter().enumerate() {
        let a = hub.call(r)?;
        // The apply's last answer counts the devices instead.
        if let Some(d) = a["devices"].as_array() {
            rows.extend(d.iter().cloned());
        }
        if i == 0 {
            first_net = a.get("network").cloned();
        }
        last = a;
    }
    if last.get("network").is_none()
        && let Some(n) = first_net
    {
        last["network"] = n;
    }
    let _ = n;
    if o.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"rows": rows, "result": last})).unwrap()
        );
        return Ok(());
    }
    print!("{}", table(&rows));
    let net = &last["network"];
    if o.apply {
        println!(
            "network {} (PAN 0x{:04X}, channel {}), coordinator changed {}, configured again {}",
            s(&net["action"]),
            net["pan_id"].as_u64().unwrap_or(0),
            s(&net["channel"]),
            s(&last["coordinator_changed"]),
            s(&last["reconfigure"])
        );
        println!(
            "devices {} (failed {}, removed {}), groups {}, stored {}",
            last["devices"],
            last["failed"],
            last["removed"],
            last["groups"],
            s(&last["stored"])
        );
        if !last["backup"].is_null() {
            println!("the replaced network was backed up (network/backup/replaced)");
        }
    } else {
        println!(
            "network {}: {} (PAN 0x{:04X}, channel {}, {} devices, {} link keys), coordinator {}, needs {}, restore check {}",
            s(&net["ext_pan_id"]),
            s(&net["action"]),
            net["pan_id"].as_u64().unwrap_or(0),
            s(&net["channel"]),
            s(&net["devices"]),
            s(&net["link_keys"]),
            s(&net["coordinator"]),
            net["needs"],
            last["restore_check"]
        );
        println!(
            "summary {}, groups {}, not in the import {}",
            last["summary"],
            s(&last["groups"]),
            s(&last["not_imported"]["count"])
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_goes_in_small_requests() {
        let dev = |i: u32| json!({"ieee": format!("{i:016x}"), "nwk": i, "name": "x".repeat(200)});
        let ocb = |i: u32| json!({"ieee_address": format!("{i:016x}"), "nwk_address": "0001"});
        let doc = json!({"format": "zigbee-rs-hub/import", "version": 1, "network": {"pan_id": "1a62", "devices": (0..300).map(ocb).collect::<Vec<_>>()},
            "devices": (0..100).map(dev).collect::<Vec<_>>(), "groups": [{"id": 1}]});
        let o = Options {
            force: true,
            keep_ieee: true,
            ..Options::default()
        };
        let r = requests(&doc, "network/import", &o);
        assert!(r.len() > 3, "{}", r.len());
        assert_eq!(r[0]["document"]["network"]["devices"], json!([]));
        assert_eq!(r[0]["document"]["groups"][0]["id"], 1);
        assert!(r[0]["document"].get("devices").is_none());
        assert_eq!(
            (r[0]["force"].as_bool(), r[0]["allow_ieee_change"].as_bool()),
            (Some(true), Some(false))
        );
        let count = |k: &str| {
            r.iter()
                .filter_map(|x| x[k].as_array())
                .map(Vec::len)
                .sum::<usize>()
        };
        assert_eq!((count("network_devices"), count("devices")), (300, 100));
        for x in &r[1..] {
            assert!(x.to_string().len() < BATCH + 512, "{}", x.to_string().len());
            assert!(x.get("force").is_none(), "options only on the first");
        }
        assert!(
            r[..r.len() - 1].iter().all(|x| x["more"] == true)
                && r.last().unwrap()["more"] == false
        );
        let one = requests(
            &json!({"network": {}}),
            "network/import/preview",
            &Options::default(),
        );
        assert_eq!((one.len(), one[0]["more"].as_bool()), (1, Some(false)));
    }
}
