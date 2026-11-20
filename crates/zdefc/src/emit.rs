//! Source emitters: canonical YAML, JSON and JSON5.

use crate::tree::{Key, Node, Value};

const WIDTH: usize = 110;

fn is_scalar(n: &Node) -> bool {
    !matches!(n.v, Value::Seq(_) | Value::Map(_))
}

fn plain_ok(s: &str, flow: bool) -> bool {
    if s.is_empty() || s != s.trim() {
        return false;
    }
    let first = s.chars().next().unwrap();
    if "-?:,[]{}#&*!|>'\"%@`".contains(first) {
        return false;
    }
    if s.ends_with(':') || s.contains(": ") || s.contains(" #") || s.chars().any(|c| c.is_control())
    {
        return false;
    }
    if flow && s.chars().any(|c| ",[]{}:".contains(c)) {
        return false;
    }
    matches!(crate::yaml::resolve_plain(s, Default::default()), Ok(Value::Str(ref x)) if x == s)
}

fn dq(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            '\r' => o.push_str("\\r"),
            c if c.is_control() => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn yaml_str(s: &str, flow: bool) -> String {
    if plain_ok(s, flow) {
        s.to_string()
    } else if !s.chars().any(|c| c.is_control()) {
        format!("'{}'", s.replace('\'', "''"))
    } else {
        dq(s)
    }
}

fn int_text(i: i64, hex: Option<u8>, allow_hex: bool) -> String {
    match hex {
        Some(w) if allow_hex && i >= 0 => format!("0x{:0w$X}", i, w = w as usize),
        _ => i.to_string(),
    }
}

fn yaml_scalar(n: &Node, flow: bool) -> String {
    match &n.v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => int_text(*i, n.hex, true),
        Value::Float(f) => crate::expr::fmt_float(*f),
        Value::Str(s) => yaml_str(s, flow),
        _ => unreachable!(),
    }
}

fn yaml_key(k: &Key) -> String {
    match k {
        Key::Int(i) => i.to_string(),
        Key::Str(s) => yaml_str(s, true),
    }
}

fn yaml_flow(n: &Node) -> String {
    match &n.v {
        Value::Seq(v) => format!(
            "[{}]",
            v.iter().map(yaml_flow).collect::<Vec<_>>().join(", ")
        ),
        Value::Map(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, _, v)| format!("{}: {}", yaml_key(k), yaml_flow(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => yaml_scalar(n, true),
    }
}

fn flow_fits(n: &Node, indent: usize, prefix: usize) -> Option<String> {
    let all_scalar_seq = matches!(&n.v, Value::Seq(v) if v.iter().all(is_scalar));
    if !(n.flow || all_scalar_seq) {
        return None;
    }
    let s = yaml_flow(n);
    (indent + prefix + s.len() <= WIDTH).then_some(s)
}

fn yaml_map(
    m: &[(Key, crate::tree::Pos, Node)],
    indent: usize,
    out: &mut String,
    first_inline: bool,
) {
    for (i, (k, _, v)) in m.iter().enumerate() {
        if !(first_inline && i == 0) {
            out.push_str(&" ".repeat(indent));
        }
        let key = yaml_key(k);
        out.push_str(&key);
        out.push(':');
        yaml_value(v, indent, key.len() + 2, out);
    }
}

fn yaml_value(v: &Node, indent: usize, prefix: usize, out: &mut String) {
    if is_scalar(v) {
        out.push(' ');
        out.push_str(&yaml_scalar(v, false));
        out.push('\n');
        return;
    }
    if let Some(f) = flow_fits(v, indent, prefix) {
        out.push(' ');
        out.push_str(&f);
        out.push('\n');
        return;
    }
    out.push('\n');
    match &v.v {
        Value::Map(m) => yaml_map(m, indent + 2, out, false),
        Value::Seq(items) => yaml_seq(items, indent + 2, out),
        _ => unreachable!(),
    }
}

fn yaml_seq(items: &[Node], indent: usize, out: &mut String) {
    for it in items {
        out.push_str(&" ".repeat(indent));
        out.push('-');
        if is_scalar(it) {
            out.push(' ');
            out.push_str(&yaml_scalar(it, false));
            out.push('\n');
            continue;
        }
        if let Some(f) = flow_fits(it, indent, 2) {
            out.push(' ');
            out.push_str(&f);
            out.push('\n');
            continue;
        }
        match &it.v {
            Value::Map(m) if !m.is_empty() => {
                out.push(' ');
                yaml_map(m, indent + 2, out, true);
            }
            Value::Seq(s) => {
                out.push('\n');
                yaml_seq(s, indent + 2, out);
            }
            _ => {
                out.push(' ');
                out.push_str(&yaml_flow(it));
                out.push('\n');
            }
        }
    }
}

/// Canonical strict YAML 1.2 text of a tree (top level must be a map).
pub fn yaml(n: &Node) -> String {
    let mut out = String::new();
    match &n.v {
        Value::Map(m) => yaml_map(m, 0, &mut out, false),
        _ => out.push_str(&yaml_flow(n)),
    }
    out
}

fn ident_key(s: &str) -> bool {
    let mut c = s.chars();
    c.next()
        .is_some_and(|f| f.is_ascii_alphabetic() || f == '_' || f == '$')
        && c.all(|x| x.is_ascii_alphanumeric() || x == '_' || x == '$')
}

fn json_inline(n: &Node, j5: bool) -> String {
    match &n.v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => int_text(*i, n.hex, j5),
        Value::Float(f) => crate::expr::fmt_float(*f),
        Value::Str(s) => dq(s),
        Value::Seq(v) => format!(
            "[{}]",
            v.iter()
                .map(|x| json_inline(x, j5))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Map(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, _, v)| format!("{}: {}", json_key(k, j5), json_inline(v, j5)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn json_key(k: &Key, j5: bool) -> String {
    let s = k.as_json();
    if j5 && ident_key(&s) { s } else { dq(&s) }
}

fn json_block(n: &Node, indent: usize, j5: bool, out: &mut String) {
    let inline = json_inline(n, j5);
    if is_scalar(n)
        || ((n.flow || matches!(&n.v, Value::Seq(v) if v.iter().all(is_scalar)))
            && indent + inline.len() <= WIDTH)
    {
        out.push_str(&inline);
        return;
    }
    let pad = " ".repeat(indent + 2);
    match &n.v {
        Value::Map(m) => {
            out.push_str("{\n");
            for (i, (k, _, v)) in m.iter().enumerate() {
                out.push_str(&pad);
                out.push_str(&json_key(k, j5));
                out.push_str(": ");
                json_block(v, indent + 2, j5, out);
                if i + 1 < m.len() || j5 {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&" ".repeat(indent));
            out.push('}');
        }
        Value::Seq(v) => {
            out.push_str("[\n");
            for (i, x) in v.iter().enumerate() {
                out.push_str(&pad);
                json_block(x, indent + 2, j5, out);
                if i + 1 < v.len() || j5 {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&" ".repeat(indent));
            out.push(']');
        }
        _ => unreachable!(),
    }
}

/// JSON (strict) text.
pub fn json(n: &Node) -> String {
    let mut o = String::new();
    json_block(n, 0, false, &mut o);
    o.push('\n');
    o
}

/// JSON5 text (unquoted keys, hex numbers, trailing commas).
pub fn json5(n: &Node) -> String {
    let mut o = String::new();
    json_block(n, 0, true, &mut o);
    o.push('\n');
    o
}
