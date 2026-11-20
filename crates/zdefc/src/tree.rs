//! Source tree: the one data model all source formats parse into (spec D22).

use std::fmt;

/// 1-based line and column in the source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pos {
    pub line: u32,
    pub col: u32,
}

impl fmt::Display for Pos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            Ok(())
        } else {
            write!(f, "{}:{}", self.line, self.col)
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Key {
    Str(String),
    Int(i64),
}

impl Key {
    /// The key as JSON sees it (object keys are strings).
    pub fn as_json(&self) -> String {
        match self {
            Key::Str(s) => s.clone(),
            Key::Int(i) => i.to_string(),
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Key::Str(s) => Some(s),
            Key::Int(_) => None,
        }
    }

    /// Integer value of an int key or a string key written as an integer
    /// (decimal or 0x hex): JSON objects only have string keys.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Key::Int(i) => Some(*i),
            Key::Str(s) => parse_int_text(s),
        }
    }
}

pub fn parse_int_text(s: &str) -> Option<i64> {
    let (neg, body) = match s.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, s),
    };
    let v = if let Some(h) = body.strip_prefix("0x") {
        if h.is_empty() || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        i64::from_str_radix(h, 16).ok()?
    } else {
        if body.is_empty()
            || !body.bytes().all(|b| b.is_ascii_digit())
            || (body.len() > 1 && body.starts_with('0'))
        {
            return None;
        }
        body.parse::<i64>().ok()?
    };
    Some(if neg { -v } else { v })
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Seq(Vec<Node>),
    Map(Vec<(Key, Pos, Node)>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub v: Value,
    pub pos: Pos,
    /// Display hint for emitters: write this integer in hex with this many digits.
    pub hex: Option<u8>,
    /// Display hint: prefer flow style for this map/sequence.
    pub flow: bool,
}

impl Node {
    pub fn new(v: Value, pos: Pos) -> Self {
        Node {
            v,
            pos,
            hex: None,
            flow: false,
        }
    }

    pub fn synth(v: Value) -> Self {
        Node::new(v, Pos::default())
    }

    pub fn str(s: impl Into<String>) -> Self {
        Node::synth(Value::Str(s.into()))
    }

    pub fn int(i: i64) -> Self {
        Node::synth(Value::Int(i))
    }

    pub fn hex(i: i64, digits: u8) -> Self {
        Node {
            hex: Some(digits),
            ..Node::synth(Value::Int(i))
        }
    }

    pub fn get(&self, k: &str) -> Option<&Node> {
        match &self.v {
            Value::Map(m) => m.iter().find(|e| e.0.as_str() == Some(k)).map(|e| &e.2),
            _ => None,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self.v {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Int(_) => "integer",
            Value::Float(_) => "number",
            Value::Str(_) => "string",
            Value::Seq(_) => "array",
            Value::Map(_) => "object",
        }
    }
}

/// A source error with a position.
#[derive(Clone, Debug, PartialEq)]
pub struct Diag {
    pub pos: Pos,
    /// JSON-pointer-like path (`/entities/3/from`), may be empty.
    pub path: String,
    pub message: String,
}

impl fmt::Display for Diag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.pos.line, self.path.is_empty()) {
            (0, true) => write!(f, "{}", self.message),
            (0, false) => write!(f, "{}: {}", self.path, self.message),
            (_, true) => write!(f, "{}: {}", self.pos, self.message),
            (_, false) => write!(f, "{}: {}: {}", self.pos, self.path, self.message),
        }
    }
}

impl Diag {
    pub fn at(pos: Pos, message: impl Into<String>) -> Self {
        Diag {
            pos,
            path: String::new(),
            message: message.into(),
        }
    }
}
