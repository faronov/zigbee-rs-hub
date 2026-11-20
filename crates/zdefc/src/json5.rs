//! JSON (RFC 8259), JSONC (JSON + comments + trailing commas) and JSON5 reader.
//!
//! Integers (no fraction, no exponent; JSON5 also `0x` hex) become `Int`,
//! other numbers `Float`. Duplicate keys are errors; `Infinity`/`NaN` are
//! rejected (not representable in a definition).

use crate::tree::{Diag, Key, Node, Pos, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Json,
    Jsonc,
    Json5,
}

struct P<'a> {
    s: &'a [u8],
    src: &'a str,
    i: usize,
    line: u32,
    line_start: usize,
    d: Dialect,
}

pub fn parse(src: &str, d: Dialect) -> Result<Node, Diag> {
    let mut p = P {
        s: src.as_bytes(),
        src,
        i: 0,
        line: 1,
        line_start: 0,
        d,
    };
    if p.s.starts_with(&[0xef, 0xbb, 0xbf]) {
        p.i = 3;
    }
    p.ws()?;
    let n = p.value()?;
    p.ws()?;
    if p.i < p.s.len() {
        return Err(p.err("trailing characters after the value"));
    }
    Ok(n)
}

impl P<'_> {
    fn pos(&self) -> Pos {
        let col = self.src[self.line_start..self.i.min(self.src.len())]
            .chars()
            .count() as u32
            + 1;
        Pos {
            line: self.line,
            col,
        }
    }

    fn err(&self, m: impl Into<String>) -> Diag {
        Diag::at(self.pos(), m)
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.i += 1;
        if c == b'\n' {
            self.line += 1;
            self.line_start = self.i;
        }
        Some(c)
    }

    fn ws(&mut self) -> Result<(), Diag> {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n' | b'\r') => {
                    self.bump();
                }
                Some(0xc2) if self.d == Dialect::Json5 && self.s.get(self.i + 1) == Some(&0xa0) => {
                    self.i += 2;
                }
                Some(b'/') if self.d != Dialect::Json => match self.s.get(self.i + 1) {
                    Some(b'/') => {
                        while let Some(c) = self.peek() {
                            if c == b'\n' {
                                break;
                            }
                            self.bump();
                        }
                    }
                    Some(b'*') => {
                        let start = self.pos();
                        self.i += 2;
                        loop {
                            match self.bump() {
                                None => return Err(Diag::at(start, "unterminated comment")),
                                Some(b'*') if self.peek() == Some(b'/') => {
                                    self.i += 1;
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => return Err(self.err("unexpected '/'")),
                },
                _ => return Ok(()),
            }
        }
    }

    fn value(&mut self) -> Result<Node, Diag> {
        let at = self.pos();
        let v = match self.peek() {
            None => return Err(self.err("unexpected end of input")),
            Some(b'{') => return self.object(),
            Some(b'[') => return self.array(),
            Some(b'"') => Value::Str(self.string(b'"')?),
            Some(b'\'') if self.d == Dialect::Json5 => Value::Str(self.string(b'\'')?),
            Some(c) if c == b'-' || c == b'+' || c == b'.' || c.is_ascii_digit() => {
                self.number()?
            }
            Some(c) if c.is_ascii_alphabetic() || c == b'_' || c == b'$' => {
                let w = self.ident();
                match w.as_str() {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    "null" => Value::Null,
                    "Infinity" | "NaN" => return Err(Diag::at(at, "Infinity/NaN are not allowed")),
                    _ => return Err(Diag::at(at, format!("unexpected identifier {w:?}"))),
                }
            }
            Some(c) => return Err(self.err(format!("unexpected character {:?}", c as char))),
        };
        Ok(Node::new(v, at))
    }

    fn ident(&mut self) -> String {
        let st = self.i;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' {
                self.i += 1;
            } else {
                break;
            }
        }
        self.src[st..self.i].to_string()
    }

    fn number(&mut self) -> Result<Value, Diag> {
        let at = self.pos();
        let st = self.i;
        let mut neg = false;
        if let Some(c @ (b'-' | b'+')) = self.peek() {
            if c == b'+' && self.d != Dialect::Json5 {
                return Err(self.err("'+' is not allowed"));
            }
            neg = c == b'-';
            self.i += 1;
        }
        if self.d == Dialect::Json5
            && (self.s[self.i..].starts_with(b"0x") || self.s[self.i..].starts_with(b"0X"))
        {
            self.i += 2;
            let hs = self.i;
            while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                self.i += 1;
            }
            let v = i64::from_str_radix(&self.src[hs..self.i], 16)
                .map_err(|_| Diag::at(at, "bad hexadecimal number"))?;
            return Ok(Value::Int(if neg { -v } else { v }));
        }
        if self.s[self.i..].starts_with(b"Infinity") || self.s[self.i..].starts_with(b"NaN") {
            return Err(Diag::at(at, "Infinity/NaN are not allowed"));
        }
        let ds = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        let int_digits = self.i - ds;
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.i += 1;
            let fs = self.i;
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.i += 1;
            }
            if self.d == Dialect::Json5 {
                if int_digits == 0 && self.i == fs {
                    return Err(Diag::at(at, "bad number"));
                }
            } else if int_digits == 0 || self.i == fs {
                return Err(Diag::at(at, "bad number"));
            }
        } else if int_digits == 0 {
            return Err(Diag::at(at, "bad number"));
        }
        if self.d != Dialect::Json5 && int_digits > 1 && self.s[ds] == b'0' {
            return Err(Diag::at(at, "leading zeros are not allowed"));
        }
        if let Some(b'e' | b'E') = self.peek() {
            is_float = true;
            self.i += 1;
            if let Some(b'+' | b'-') = self.peek() {
                self.i += 1;
            }
            let es = self.i;
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.i += 1;
            }
            if self.i == es {
                return Err(Diag::at(at, "bad exponent"));
            }
        }
        let text = &self.src[st..self.i];
        if is_float {
            let f: f64 = text
                .trim_start_matches('+')
                .parse()
                .map_err(|_| Diag::at(at, "bad number"))?;
            if !f.is_finite() {
                return Err(Diag::at(at, "number out of range"));
            }
            Ok(Value::Float(f))
        } else {
            if int_digits > 1 && self.s[ds] == b'0' {
                return Err(Diag::at(
                    at,
                    "leading zeros are not allowed (octal is ambiguous)",
                ));
            }
            text.trim_start_matches('+')
                .parse::<i64>()
                .map(Value::Int)
                .map_err(|_| Diag::at(at, "integer out of range"))
        }
    }

    fn string(&mut self, q: u8) -> Result<String, Diag> {
        let start = self.pos();
        self.i += 1;
        let mut out = String::new();
        loop {
            let st = self.i;
            while let Some(c) = self.peek() {
                if c == q || c == b'\\' || c == b'\n' || c < 0x20 {
                    break;
                }
                self.i += 1;
            }
            out.push_str(&self.src[st..self.i]);
            match self.peek() {
                None => return Err(Diag::at(start, "unterminated string")),
                Some(c) if c == q => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let e = self.bump().ok_or(Diag::at(start, "unterminated string"))?;
                    match e {
                        b'"' => out.push('"'),
                        b'\'' if self.d == Dialect::Json5 => out.push('\''),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'v' if self.d == Dialect::Json5 => out.push('\u{b}'),
                        b'0' if self.d == Dialect::Json5
                            && !self.peek().is_some_and(|c| c.is_ascii_digit()) =>
                        {
                            out.push('\0')
                        }
                        b'\n' if self.d == Dialect::Json5 => {}
                        b'\r' if self.d == Dialect::Json5 => {
                            if self.peek() == Some(b'\n') {
                                self.bump();
                            }
                        }
                        b'x' if self.d == Dialect::Json5 => {
                            let h = self
                                .src
                                .get(self.i..self.i + 2)
                                .ok_or(self.err("bad \\x escape"))?;
                            let v = u8::from_str_radix(h, 16)
                                .map_err(|_| self.err("bad \\x escape"))?;
                            self.i += 2;
                            out.push(v as char);
                        }
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xd800..0xdc00).contains(&cp)
                                && self.s[self.i..].starts_with(b"\\u")
                            {
                                self.i += 2;
                                let lo = self.hex4()?;
                                cp = 0x10000
                                    + ((cp - 0xd800) << 10)
                                    + (lo.wrapping_sub(0xdc00) & 0x3ff);
                            }
                            out.push(char::from_u32(cp).ok_or(self.err("bad \\u escape"))?);
                        }
                        _ => return Err(self.err(format!("bad escape \\{}", e as char))),
                    }
                }
                Some(_) => return Err(self.err("control character or newline in string")),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, Diag> {
        let h = self
            .src
            .get(self.i..self.i + 4)
            .ok_or(self.err("bad \\u escape"))?;
        let v = u32::from_str_radix(h, 16).map_err(|_| self.err("bad \\u escape"))?;
        self.i += 4;
        Ok(v)
    }

    fn object(&mut self) -> Result<Node, Diag> {
        let at = self.pos();
        self.i += 1;
        let mut entries: Vec<(Key, Pos, Node)> = Vec::new();
        loop {
            self.ws()?;
            if self.peek() == Some(b'}') {
                self.i += 1;
                break;
            }
            let kp = self.pos();
            let key = match self.peek() {
                Some(b'"') => self.string(b'"')?,
                Some(b'\'') if self.d == Dialect::Json5 => self.string(b'\'')?,
                Some(c)
                    if self.d == Dialect::Json5
                        && (c.is_ascii_alphabetic() || c == b'_' || c == b'$') =>
                {
                    self.ident()
                }
                _ => return Err(self.err("expected an object key")),
            };
            if entries.iter().any(|e| e.0.as_str() == Some(key.as_str())) {
                return Err(Diag::at(kp, format!("duplicate key {key:?}")));
            }
            self.ws()?;
            if self.peek() != Some(b':') {
                return Err(self.err("expected ':'"));
            }
            self.i += 1;
            self.ws()?;
            let v = self.value()?;
            entries.push((Key::Str(key), kp, v));
            self.ws()?;
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                    if self.d == Dialect::Json {
                        self.ws()?;
                        if self.peek() == Some(b'}') {
                            return Err(self.err("trailing comma"));
                        }
                    }
                }
                Some(b'}') => {
                    self.i += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or '}'")),
            }
        }
        Ok(Node::new(Value::Map(entries), at))
    }

    fn array(&mut self) -> Result<Node, Diag> {
        let at = self.pos();
        self.i += 1;
        let mut items = Vec::new();
        loop {
            self.ws()?;
            if self.peek() == Some(b']') {
                self.i += 1;
                break;
            }
            items.push(self.value()?);
            self.ws()?;
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                    if self.d == Dialect::Json {
                        self.ws()?;
                        if self.peek() == Some(b']') {
                            return Err(self.err("trailing comma"));
                        }
                    }
                }
                Some(b']') => {
                    self.i += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or ']'")),
            }
        }
        Ok(Node::new(Value::Seq(items), at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialects() {
        assert!(parse("{\"a\": 1,}", Dialect::Json).is_err());
        assert!(parse("{\"a\": 1,} // c", Dialect::Jsonc).is_ok());
        let n = parse(
            "{a: 0x0402, 'b': 'it\\'s', c: .5, d: [1, 2,], /* x */ e: \"l1 \\\nl2\"}",
            Dialect::Json5,
        )
        .unwrap();
        assert_eq!(n.get("a").unwrap().v, Value::Int(0x402));
        assert_eq!(n.get("b").unwrap().v, Value::Str("it's".into()));
        assert_eq!(n.get("c").unwrap().v, Value::Float(0.5));
        assert_eq!(n.get("e").unwrap().v, Value::Str("l1 l2".into()));
        assert!(parse("{\"a\": 1, \"a\": 2}", Dialect::Json).is_err());
        assert!(parse("{\"a\": 010}", Dialect::Json5).is_err());
        assert!(
            parse("{\"a\": 1e3}", Dialect::Json)
                .unwrap()
                .get("a")
                .unwrap()
                .v
                == Value::Float(1000.0)
        );
    }
}
