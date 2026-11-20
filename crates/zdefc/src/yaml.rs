//! Strict YAML 1.2 (core schema) reader (spec 6.1, D22).
//!
//! Plain scalars resolve with the YAML 1.2 core schema, restricted to forms
//! every YAML 1.1 and 1.2 parser reads the same way:
//!
//! - null: `null`, `Null`, `NULL`, `~`, empty; booleans: `true`/`false` (also
//!   `True`, `TRUE`, `False`, `FALSE`);
//! - integers: `-?(0|[1-9][0-9]*)` and `0x[0-9a-fA-F]+`;
//! - floats: `-?(0|[1-9][0-9]*)\.[0-9]+`.
//!
//! Plain scalars that YAML 1.1 or YAML 1.2 would read as anything else are
//! rejected as ambiguous and must be quoted: `yes`/`no`/`on`/`off`/`y`/`n`,
//! octal or leading-zero integers, `0b`/`0o` forms, `_` separators,
//! sexagesimal numbers, exponents, `.inf`/`.nan`, `+` signs, timestamps.
//! Anchors, aliases, tags, merge keys, duplicate keys and multiple documents
//! are errors.

use crate::tree::{Diag, Key, Node, Pos, Value};
use regex_lite::Regex;
use std::sync::OnceLock;
use yaml_rust2::parser::{Event, Parser};
use yaml_rust2::scanner::{Marker, TScalarStyle};

struct Rx {
    int: Regex,
    hex: Regex,
    float: Regex,
    ambiguous: Vec<Regex>,
}

fn rx() -> &'static Rx {
    static R: OnceLock<Rx> = OnceLock::new();
    R.get_or_init(|| Rx {
        int: Regex::new(r"^-?(0|[1-9][0-9]*)$").unwrap(),
        hex: Regex::new(r"^0x[0-9a-fA-F]+$").unwrap(),
        float: Regex::new(r"^-?(0|[1-9][0-9]*)\.[0-9]+$").unwrap(),
        ambiguous: [
            // YAML 1.1 booleans
            r"^(y|Y|yes|Yes|YES|n|N|no|No|NO|on|On|ON|off|Off|OFF)$",
            // YAML 1.2 core int/float
            r"^[-+]?[0-9]+$",
            r"^0o[0-7]+$",
            r"^0x[0-9a-fA-F]+$",
            r"^[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?$",
            r"^[-+]?\.(inf|Inf|INF)$",
            r"^\.(nan|NaN|NAN)$",
            // YAML 1.1 int/float
            r"^[-+]?0b[0-1_]+$",
            r"^[-+]?0[0-7_]+$",
            r"^[-+]?(0|[1-9][0-9_]*)$",
            r"^[-+]?0x[0-9a-fA-F_]+$",
            r"^[-+]?[1-9][0-9_]*(:[0-5]?[0-9])+$",
            r"^[-+]?([0-9][0-9_]*)?\.[0-9_]*([eE][-+][0-9]+)?$",
            r"^[-+]?[0-9][0-9_]*(:[0-5]?[0-9])+\.[0-9_]*$",
            // YAML 1.1 timestamps
            r"^[0-9]{4}-[0-9]{1,2}-[0-9]{1,2}",
            // YAML 1.1 value/merge keys
            r"^(=|<<)$",
        ]
        .iter()
        .map(|s| Regex::new(s).unwrap())
        .collect(),
    })
}

fn pos(m: &Marker) -> Pos {
    Pos {
        line: m.line() as u32,
        col: m.col() as u32 + 1,
    }
}

/// Resolves a plain scalar.
pub fn resolve_plain(s: &str, at: Pos) -> Result<Value, Diag> {
    let r = rx();
    match s {
        "" | "~" | "null" | "Null" | "NULL" => return Ok(Value::Null),
        "true" | "True" | "TRUE" => return Ok(Value::Bool(true)),
        "false" | "False" | "FALSE" => return Ok(Value::Bool(false)),
        _ => {}
    }
    if r.int.is_match(s) {
        return s
            .parse::<i64>()
            .map(Value::Int)
            .map_err(|_| Diag::at(at, format!("integer {s} out of range")));
    }
    if r.hex.is_match(s) {
        return i64::from_str_radix(&s[2..], 16)
            .map(Value::Int)
            .map_err(|_| Diag::at(at, format!("integer {s} out of range")));
    }
    if r.float.is_match(s) {
        return s
            .parse::<f64>()
            .map(Value::Float)
            .map_err(|_| Diag::at(at, format!("bad number {s}")));
    }
    if r.ambiguous.iter().any(|x| x.is_match(s)) {
        return Err(Diag::at(
            at,
            format!(
                "ambiguous plain scalar {s:?}: YAML 1.1 and 1.2 read it differently; quote it if it is a string, or write numbers as decimal/0x integers or d.d floats"
            ),
        ));
    }
    Ok(Value::Str(s.to_string()))
}

enum Frame {
    Seq(Vec<Node>, Pos),
    Map(Vec<(Key, Pos, Node)>, Pos, Option<(Key, Pos)>),
}

/// Parses one strict YAML document.
pub fn parse(src: &str) -> Result<Node, Diag> {
    let mut p = Parser::new_from_str(src);
    let mut stack: Vec<Frame> = Vec::new();
    let mut root: Option<Node> = None;
    let mut docs = 0;
    loop {
        let (ev, m) = p
            .next_token()
            .map_err(|e| Diag::at(pos(e.marker()), format!("YAML syntax: {}", e.info())))?;
        let at = pos(&m);
        let node = match ev {
            Event::StreamStart | Event::Nothing => continue,
            Event::StreamEnd => break,
            Event::DocumentStart => {
                docs += 1;
                if docs > 1 {
                    return Err(Diag::at(at, "only one YAML document per file"));
                }
                continue;
            }
            Event::DocumentEnd => continue,
            Event::Alias(_) => return Err(Diag::at(at, "YAML aliases are not allowed")),
            Event::Scalar(s, style, anchor, tag) => {
                if anchor != 0 {
                    return Err(Diag::at(at, "YAML anchors are not allowed"));
                }
                if tag.is_some() {
                    return Err(Diag::at(at, "YAML tags are not allowed"));
                }
                let v = match style {
                    TScalarStyle::Plain => resolve_plain(&s, at)?,
                    _ => Value::Str(s),
                };
                Node::new(v, at)
            }
            Event::SequenceStart(anchor, tag) => {
                if anchor != 0 || tag.is_some() {
                    return Err(Diag::at(at, "YAML anchors and tags are not allowed"));
                }
                stack.push(Frame::Seq(Vec::new(), at));
                continue;
            }
            Event::MappingStart(anchor, tag) => {
                if anchor != 0 || tag.is_some() {
                    return Err(Diag::at(at, "YAML anchors and tags are not allowed"));
                }
                stack.push(Frame::Map(Vec::new(), at, None));
                continue;
            }
            Event::SequenceEnd => match stack.pop() {
                Some(Frame::Seq(v, p0)) => Node::new(Value::Seq(v), p0),
                _ => return Err(Diag::at(at, "unbalanced YAML sequence")),
            },
            Event::MappingEnd => match stack.pop() {
                Some(Frame::Map(v, p0, None)) => Node::new(Value::Map(v), p0),
                _ => return Err(Diag::at(at, "unbalanced YAML mapping")),
            },
        };
        match stack.last_mut() {
            None => root = Some(node),
            Some(Frame::Seq(v, _)) => v.push(node),
            Some(Frame::Map(entries, _, pending)) => match pending.take() {
                None => {
                    let key = match node.v {
                        Value::Str(s) => {
                            if matches!(
                                s.as_str(),
                                "on" | "off"
                                    | "yes"
                                    | "no"
                                    | "On"
                                    | "Off"
                                    | "Yes"
                                    | "No"
                                    | "ON"
                                    | "OFF"
                                    | "YES"
                                    | "NO"
                                    | "y"
                                    | "n"
                                    | "Y"
                                    | "N"
                            ) {
                                return Err(Diag::at(
                                    node.pos,
                                    format!("{s:?} is not allowed as a key"),
                                ));
                            }
                            Key::Str(s)
                        }
                        Value::Int(i) => Key::Int(i),
                        _ => {
                            return Err(Diag::at(
                                node.pos,
                                "mapping keys must be strings or integers",
                            ));
                        }
                    };
                    if entries.iter().any(|e| e.0 == key) {
                        return Err(Diag::at(
                            node.pos,
                            format!("duplicate key {}", key.as_json()),
                        ));
                    }
                    *pending = Some((key, node.pos));
                }
                Some((k, kp)) => entries.push((k, kp, node)),
            },
        }
    }
    root.ok_or(Diag::at(Pos { line: 1, col: 1 }, "empty document"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strictness() {
        assert!(parse("a: on").is_err());
        assert!(parse("a: 'on'").is_ok());
        assert!(parse("a: 0777").is_err());
        assert!(parse("a: 1e3").is_err());
        assert!(parse("a: 1_000").is_err());
        assert!(parse("a: 12:30").is_err());
        assert!(parse("a: &x 1\nb: *x").is_err());
        assert!(parse("a: 1\na: 2").is_err());
        assert!(parse("a: !!str 1").is_err());
        assert!(parse("a: 1\n---\nb: 2").is_err());
        let n = parse("a: 0x0402\nb: -32768\nc: 0.5\nd: 3V_2100\ne: °C\nf: ~\n0: low").unwrap();
        assert_eq!(n.get("a").unwrap().v, Value::Int(0x402));
        assert_eq!(n.get("b").unwrap().v, Value::Int(-32768));
        assert_eq!(n.get("c").unwrap().v, Value::Float(0.5));
        assert_eq!(n.get("d").unwrap().v, Value::Str("3V_2100".into()));
        assert_eq!(n.get("f").unwrap().v, Value::Null);
    }
}
