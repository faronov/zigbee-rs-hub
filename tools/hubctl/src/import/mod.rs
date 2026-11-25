//! `hubctl import`: a migration from Zigbee2MQTT or ZHA to the hub
//! (`docs/migration.md`). The source's files become the hub's normalized
//! import document (`zigbee-rs-hub/import` v1, `hub_app::migrate`), which
//! is previewed on the hub (`network/import/preview`) or applied
//! (`network/import`), in requests of a few kilobytes.
//!
//! ```text
//! hubctl import z2m /opt/zigbee2mqtt/data [--url ws://hub/api --token T [--apply …]] [--out doc.json]
//! hubctl import zha /config/zigbee.db [--backup backup.json] [--ha-storage /config/.storage] …
//! hubctl import doc doc.json …
//! ```

mod client;
pub mod z2m;
pub mod zha;
pub mod zigpy;

use std::path::Path;

use serde_json::{Value as J, json};

pub use client::{Options, run};

/// A normalized document.
pub fn document(kind: &str, name: &str, network: Option<J>, devices: Vec<J>, groups: Vec<J>) -> J {
    json!({
        "format": hub_app::migrate::FORMAT,
        "version": hub_app::migrate::VERSION,
        "source": {"kind": kind, "name": name},
        "network": network,
        "devices": devices,
        "groups": groups,
    })
}

/// Reads a JSON file.
pub fn json_file(p: &Path) -> Result<J, String> {
    let t = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
    serde_json::from_str(&t).map_err(|e| format!("{}: {e}", p.display()))
}

/// Big-endian hex of an IEEE written `0x…`, `aa:bb:…` or plain (16 digits).
pub fn ieee_hex(s: &str) -> Option<String> {
    let s = s.trim();
    let s = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    let d: String = s
        .chars()
        .filter(|c| *c != ':' && *c != '-')
        .collect::<String>()
        .to_ascii_lowercase();
    (d.len() == 16 && d.bytes().all(|c| c.is_ascii_hexdigit())).then_some(d)
}

/// The `hubctl import` command line (after `import`).
pub fn main(mut args: Vec<String>) -> Result<(), String> {
    let mut take = |k: &str| -> Option<String> {
        let i = args.iter().position(|a| a == k)?;
        args.remove(i);
        (i < args.len()).then(|| args.remove(i))
    };
    let url = take("--url");
    let token = take("--token")
        .or_else(|| std::env::var("HUB_TOKEN").ok())
        .unwrap_or_default();
    let out = take("--out");
    let backup = take("--backup");
    let ha = take("--ha-storage");
    let reconfigure = take("--reconfigure");
    let mut flag = |k: &str| match args.iter().position(|a| a == k) {
        Some(i) => {
            args.remove(i);
            true
        }
        None => false,
    };
    let opts = Options {
        apply: flag("--apply"),
        force: flag("--force"),
        confirm: flag("--confirm"),
        confirm_ieee_write_once: flag("--confirm-ieee-write-once"),
        keep_ieee: flag("--keep-ieee"),
        skip_backup: flag("--skip-backup"),
        remove_missing: flag("--remove-missing"),
        reconfigure,
        json: flag("--json"),
    };
    let (Some(kind), Some(src)) = (args.first().cloned(), args.get(1).cloned()) else {
        return Err(
            "usage: hubctl import z2m DIR|FILE… | zha ZIGBEE_DB | doc FILE [options]".into(),
        );
    };
    let doc = match kind.as_str() {
        "z2m" => {
            let files: Vec<&Path> = args[1..].iter().map(Path::new).collect();
            z2m::from_files(&files)?
        }
        "zha" => {
            let backup = backup.map(|b| json_file(Path::new(&b))).transpose()?;
            let ha = ha.map(|d| zha::HaNames::read(Path::new(&d))).transpose()?;
            zha::from_db(Path::new(&src), backup, ha.as_ref())?
        }
        "doc" => json_file(Path::new(&src))?,
        k => return Err(format!("import {k}: z2m, zha or doc")),
    };
    eprintln!(
        "{}: {} devices, {} groups, network {}",
        doc["source"]["name"].as_str().unwrap_or(&kind),
        doc["devices"].as_array().map_or(0, Vec::len),
        doc["groups"].as_array().map_or(0, Vec::len),
        doc["network"]["extended_pan_id"]
            .as_str()
            .unwrap_or("missing"),
    );
    if let Some(o) = &out {
        std::fs::write(o, serde_json::to_string_pretty(&doc).unwrap())
            .map_err(|e| format!("{o}: {e}"))?;
        eprintln!("{o}: written (it holds the network key: keep it private)");
    }
    match url {
        Some(u) => run(&u, &token, &doc, &opts),
        None if out.is_none() => {
            println!("{}", serde_json::to_string_pretty(&doc).unwrap());
            Ok(())
        }
        None => Ok(()),
    }
}
