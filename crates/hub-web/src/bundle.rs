//! One build's web UI (shared by `build.rs` and the tests): the page
//! (`web/index.html`) and the tabs it loads on demand (`web/ui/<name>.html`:
//! the tab's markup, then its `<script>`), each with the build's feature
//! sections selected and minified, and the build hash over all of them.
//!
//! A module as served (`/ui/<build>/<name>.js`) is a classic script run in
//! the page's scope (it uses the page's helpers and state, its functions
//! and variables join them). Its first line puts the markup into the tab,
//! `"use strict";uiMod("<build>","<name>",'<markup>');`, and the page
//! checks that line, of its own build, before it runs the script.

use std::format;
use std::string::String;
use std::vec::Vec;

use sha2::{Digest, Sha256};

use super::minify::minify;
use super::variant::{Features, select};

/// The page's stand-in for the build hash (`UIB`), replaced once.
pub const BUILD_PLACEHOLDER: &str = "__UI_BUILD__";

/// Every module source, `web/ui/<name>.html`.
#[allow(dead_code)]
pub const SOURCES: [&str; 6] = ["conv", "catalog", "zcl", "ota", "mig", "diag"];

/// The modules of a build: name (the `data-mod` of its tab) and whether
/// the hub's developer mode gates it. The ZCL console: with `advanced`,
/// always in the developer edition (`dev`); Updates with `ota`; the
/// migration wizard (Settings) and the device page's diagnostics always.
pub fn modules(f: Features) -> Vec<(&'static str, bool)> {
    let mut m = std::vec![("conv", false), ("catalog", false), ("mig", false), ("diag", false)];
    if f.advanced || f.dev {
        m.push(("zcl", !f.dev));
    }
    if f.ota || f.ota_store {
        m.push(("ota", false));
    }
    m
}

/// A module as served.
pub struct Module {
    pub name: &'static str,
    /// Served only while the hub's developer mode is on.
    pub developer: bool,
    pub js: String,
}

/// The page and the modules of one build.
pub struct Bundle {
    /// 16 hex digits: SHA-256 of everything below (without the hash).
    pub build: String,
    pub page: String,
    pub modules: Vec<Module>,
}

/// The first line of a module up to its markup (what the page checks).
pub fn header(build: &str, name: &str) -> String {
    format!("\"use strict\";uiMod(\"{build}\",\"{name}\",")
}

/// The build of `f`; `src(file)` reads `index.html` or `ui/<name>.html`.
pub fn bundle(f: Features, src: &dyn Fn(&str) -> String) -> Result<Bundle, String> {
    let on = f.sections();
    let file = |name: &str| -> Result<String, String> {
        let s = select(&src(name), &on).map_err(|e| format!("{name}: {e}"))?;
        minify(&s).map_err(|e| format!("{name}: {e}"))
    };
    let page = file("index.html")?;
    let mut h = Sha256::new();
    h.update(page.as_bytes());
    let mut parts = Vec::new();
    for (name, developer) in modules(f) {
        let m = file(&format!("ui/{name}.html"))?;
        let (html, js) = split(&m).ok_or_else(|| format!("ui/{name}.html: markup, then one <script> to the end"))?;
        for p in [name, html, js] {
            h.update([0]);
            h.update(p.as_bytes());
        }
        parts.push((name, developer, string(html), String::from(js)));
    }
    let build: String = h.finalize()[..8].iter().map(|b| format!("{b:02x}")).collect();
    if page.matches(BUILD_PLACEHOLDER).count() != 1 {
        return Err(format!("index.html: {BUILD_PLACEHOLDER} once"));
    }
    let page = page.replacen(BUILD_PLACEHOLDER, &build, 1);
    let modules = parts
        .into_iter()
        .map(|(name, developer, html, js)| Module { name, developer, js: format!("{}{html});\n{js}", header(&build, name)) })
        .collect();
    Ok(Bundle { build, page, modules })
}

/// A minified module: its markup and its script.
fn split(m: &str) -> Option<(&str, &str)> {
    let (html, rest) = match m.strip_prefix("<script>\n") {
        Some(rest) => ("", rest),
        None => {
            let i = m.find("\n<script>\n")?;
            (&m[..i], &m[i + "\n<script>\n".len()..])
        }
    };
    let js = rest.strip_suffix("</script>\n")?;
    (!js.contains("<script>") && !js.contains("</script>")).then_some((html, js))
}

/// `s` as a single-quoted JavaScript string (the markup's attributes use
/// double quotes).
fn string(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('\'');
    for c in s.chars() {
        match c {
            '\'' => o.push_str("\\'"),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\u{2028}' | '\u{2029}' => o.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('\'');
    o
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// Deterministic gzip (no file name, mtime 0, OS "unknown").
pub fn gzip(data: &[u8]) -> Vec<u8> {
    let mut gz = std::vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 2, 0xff];
    gz.extend(miniz_oxide::deflate::compress_to_vec(data, 10));
    gz.extend(crc32(data).to_le_bytes());
    gz.extend((data.len() as u32).to_le_bytes());
    gz
}

/// Quoted strong ETag of compressed bytes (SHA-256, 16 hex digits).
pub fn etag(gz: &[u8]) -> String {
    let d = Sha256::digest(gz);
    let hex: String = d[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("\"{hex}\"")
}
