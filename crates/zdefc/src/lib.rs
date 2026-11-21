//! `zdefc`: the zdef definition compiler (docs/device-support.md 6-9, 14.4, D15, D22).
//!
//! Source (strict YAML 1.2, JSON, JSONC, JSON5) -> one tree -> JSON Schema
//! validation -> `zdef` model (expressions compiled) -> `zdef` semantic
//! validation -> canonical CBOR. Plus the decompiler (CBOR -> YAML/JSON/JSON5)
//! and the catalog bank builder.

// Nested `if let` reads better than let-chains here.
#![allow(clippy::collapsible_if)]

pub mod bank;
pub mod catalog;
pub mod emit;
pub mod expr;
pub mod json5;
pub mod keys;
pub mod lower;
pub mod raise;
pub mod schema;
pub mod tree;
pub mod web;
pub mod yaml;

use tree::{Diag, Node};
use zdef::{ContentHash, Definition};

/// Source format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Yaml,
    Json,
    Jsonc,
    Json5,
}

impl Format {
    pub fn from_name(s: &str) -> Option<Format> {
        Some(
            match s.trim_start_matches('.').to_ascii_lowercase().as_str() {
                "yaml" | "yml" => Format::Yaml,
                "json" => Format::Json,
                "jsonc" => Format::Jsonc,
                "json5" => Format::Json5,
                _ => return None,
            },
        )
    }

    /// From a file name's extension (default YAML).
    pub fn from_path(p: &str) -> Format {
        p.rsplit_once('.')
            .and_then(|(_, e)| Format::from_name(e))
            .unwrap_or(Format::Yaml)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Compute `requires` (spec 8.5). Off only to reproduce the prototype.
    pub emit_requires: bool,
    /// Drop `description` (catalog form, spec 6.1).
    pub strip_description: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            emit_requires: true,
            strip_description: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Compiled {
    pub def: Definition,
    pub cbor: Vec<u8>,
    pub hash: ContentHash,
    pub longest_expr: usize,
}

pub fn parse_source(src: &str, fmt: Format) -> Result<Node, Vec<Diag>> {
    match fmt {
        Format::Yaml => yaml::parse(src),
        Format::Json => json5::parse(src, json5::Dialect::Json),
        Format::Jsonc => json5::parse(src, json5::Dialect::Jsonc),
        Format::Json5 => json5::parse(src, json5::Dialect::Json5),
    }
    .map_err(|e| vec![e])
}

/// Compiles a parsed tree.
pub fn compile_tree(root: &Node, opts: &Options) -> Result<Compiled, Vec<Diag>> {
    let errs = schema::validator().validate(root);
    if !errs.is_empty() {
        return Err(errs);
    }
    let lowered = lower::lower(root, opts.emit_requires)?;
    let mut def = lowered.def;
    if opts.strip_description {
        def.description = None;
    }
    if let Err(v) = def.validate() {
        let ignore_requires = !opts.emit_requires;
        let v: Vec<Diag> = v
            .into_iter()
            .filter(|e| !(ignore_requires && e.path.section == "requires"))
            .map(|e| {
                let path = e.path.to_string();
                let full = e.to_string();
                let message = full
                    .strip_prefix(&format!("{path}: "))
                    .unwrap_or(&full)
                    .to_string();
                Diag {
                    pos: Default::default(),
                    path: format!("/{}", path.replace(['.', '['], "/").replace(']', "")),
                    message,
                }
            })
            .collect();
        if !v.is_empty() {
            return Err(v);
        }
    }
    let cbor = def
        .encode()
        .map_err(|e| vec![Diag::at(Default::default(), format!("encode: {e:?}"))])?;
    // The compiler's output must be canonical: decode + re-encode is identical.
    match zdef::codec::decode_canonical(&cbor) {
        Ok(back) if back == def => {}
        Ok(_) => {
            return Err(vec![Diag::at(
                Default::default(),
                "internal: decoded model differs from the compiled one",
            )]);
        }
        Err(e) => {
            return Err(vec![Diag::at(
                Default::default(),
                format!("internal: output does not decode: {e}"),
            )]);
        }
    }
    Ok(Compiled {
        hash: ContentHash::of(&cbor),
        def,
        cbor,
        longest_expr: lowered.longest_expr,
    })
}

/// Compiles source text.
pub fn compile_str(src: &str, fmt: Format, opts: &Options) -> Result<Compiled, Vec<Diag>> {
    compile_tree(&parse_source(src, fmt)?, opts)
}

/// Decompiles canonical CBOR into source text.
pub fn decompile(cbor: &[u8], fmt: Format) -> Result<String, Vec<String>> {
    let def = zdef::codec::decode(cbor).map_err(|e| vec![e.to_string()])?;
    decompile_def(&def, fmt)
}

pub fn decompile_def(def: &Definition, fmt: Format) -> Result<String, Vec<String>> {
    let tree = raise::raise(def)?;
    Ok(match fmt {
        Format::Yaml => emit::yaml(&tree),
        Format::Json => emit::json(&tree),
        Format::Jsonc | Format::Json5 => emit::json5(&tree),
    })
}

/// Formats diagnostics, one per line, prefixed with a file name.
pub fn format_diags(file: &str, d: &[Diag]) -> String {
    d.iter().map(|e| format!("{file}:{e}\n")).collect()
}

/// Hex helper.
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
