//! WebAssembly bindings of `zdefc` (spec 14.4, D15): the web UI compiles
//! YAML/JSON/JSON5 to CBOR client-side and decompiles CBOR for editing.
//!
//! Build: `cargo build -p zdefc-wasm --release --target wasm32-unknown-unknown`,
//! then `wasm-bindgen --target web` on the `.wasm`.

use wasm_bindgen::prelude::*;
use zdefc::tree::Diag;
use zdefc::{Format, Options};

fn fmt(name: &str) -> Result<Format, JsError> {
    Format::from_name(name).ok_or_else(|| JsError::new(&format!("unknown format {name}")))
}

fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn diags_json(d: &[Diag]) -> String {
    let items: Vec<String> = d
        .iter()
        .map(|e| {
            let line = if e.pos.line > 0 {
                format!(",\"line\":{},\"col\":{}", e.pos.line, e.pos.col)
            } else {
                String::new()
            };
            format!(
                "{{\"path\":{}{},\"message\":{}}}",
                json_str(&e.path),
                line,
                json_str(&e.message)
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// Compiles source text to canonical CBOR; throws with the diagnostics as JSON.
#[wasm_bindgen]
pub fn compile(source: &str, format: &str) -> Result<Vec<u8>, JsError> {
    zdefc::compile_str(source, fmt(format)?, &Options::default())
        .map(|c| c.cbor)
        .map_err(|d| JsError::new(&diags_json(&d)))
}

/// Validates source text. Returns JSON
/// `{ok, errors: [{path, line?, col?, message}], size?, content_hash?, requires?}`.
#[wasm_bindgen]
pub fn check(source: &str, format: &str) -> Result<String, JsError> {
    Ok(
        match zdefc::compile_str(source, fmt(format)?, &Options::default()) {
            Ok(c) => {
                let req: Vec<String> = c
                    .def
                    .requires
                    .iter()
                    .map(|f| {
                        json_str(
                            zdef::Feature::from_code(*f as u64)
                                .map(|x| x.as_str())
                                .unwrap_or("?"),
                        )
                    })
                    .collect();
                format!(
                    "{{\"ok\":true,\"errors\":[],\"size\":{},\"content_hash\":\"{}\",\"requires\":[{}]}}",
                    c.cbor.len(),
                    c.hash,
                    req.join(",")
                )
            }
            Err(d) => format!("{{\"ok\":false,\"errors\":{}}}", diags_json(&d)),
        },
    )
}

/// Decompiles CBOR to `yaml`, `json` or `json5` text.
#[wasm_bindgen]
pub fn decompile(cbor: &[u8], format: &str) -> Result<String, JsError> {
    zdefc::decompile(cbor, fmt(format)?).map_err(|e| JsError::new(&e.join("; ")))
}

/// SHA-256 content hash (hex) of CBOR bytes (spec 8.6).
#[wasm_bindgen]
pub fn content_hash(cbor: &[u8]) -> String {
    zdef::ContentHash::of(cbor).to_string()
}

/// Compiles one expression; returns `"<hex bytecode> <disassembly>"`.
#[wasm_bindgen]
pub fn expr(source: &str) -> Result<String, JsError> {
    let ctx = zdef::expr::Context::new(zdef::expr::Site::From)
        .with_payload(true)
        .with_gpd(true);
    let mut pool = Vec::new();
    let code =
        zdefc::expr::compile(source, &mut pool, &ctx).map_err(|e| JsError::new(&e.to_string()))?;
    Ok(format!(
        "{} {}",
        zdefc::hex(&code),
        zdefc::expr::disasm(&code)
    ))
}

/// The JSON Schema of the source format.
#[wasm_bindgen]
pub fn schema() -> String {
    zdefc::schema::schema_json()
}
