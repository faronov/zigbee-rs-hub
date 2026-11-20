//! Reference interpreter of the expression *source* language (spec 7.2–7.4)
//! for host tests. Production expressions are `zdef` bytecode run by the
//! `zdef` VM; this tree-walker exists so golden tests can load the YAML
//! examples before the `zdef` crate lands, and as an oracle for the VM
//! later. Same semantics: run-time errors yield `null`, never a panic.

use std::sync::Arc;

use crate::expr::{Expr, ExprCompiler, ExprCtx, Expression, Value};

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(Value),
    Str(String),
    Ident(String),
    Op(&'static str),
}

const OPS: [&str; 30] = [
    "??", "||", "&&", "==", "!=", "<=", ">=", "<<", ">>", "//", "<", ">", "|", "^", "&", "+", "-",
    "*", "/", "%", "!", "~", "?", ":", "(", ")", "[", "]", ",", ".",
];

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i] as char;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            if c == '0' && i + 1 < b.len() && (b[i + 1] == b'x' || b[i + 1] == b'X') {
                i += 2;
                while i < b.len() && (b[i] as char).is_ascii_hexdigit() {
                    i += 1;
                }
                let v = i64::from_str_radix(&src[start + 2..i], 16).map_err(|e| e.to_string())?;
                out.push(Tok::Num(Value::Int(v)));
                continue;
            }
            while i < b.len() && (b[i] as char).is_ascii_digit() {
                i += 1;
            }
            let mut float = false;
            if i + 1 < b.len() && b[i] == b'.' && (b[i + 1] as char).is_ascii_digit() {
                float = true;
                i += 1;
                while i < b.len() && (b[i] as char).is_ascii_digit() {
                    i += 1;
                }
            }
            if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                float = true;
                i += 1;
                if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
                    i += 1;
                }
                while i < b.len() && (b[i] as char).is_ascii_digit() {
                    i += 1;
                }
            }
            let s = &src[start..i];
            out.push(Tok::Num(if float {
                Value::Float(s.parse().map_err(|_| format!("bad number {s}"))?)
            } else {
                Value::Int(s.parse().map_err(|_| format!("bad number {s}"))?)
            }));
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < b.len() && ((b[i] as char).is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            out.push(Tok::Ident(src[start..i].to_string()));
            continue;
        }
        if c == '"' {
            let start = i + 1;
            i += 1;
            while i < b.len() && b[i] != b'"' {
                i += 1;
            }
            if i >= b.len() {
                return Err("unterminated string".into());
            }
            out.push(Tok::Str(src[start..i].to_string()));
            i += 1;
            continue;
        }
        let op = OPS
            .iter()
            .find(|op| src[i..].starts_with(*op))
            .ok_or_else(|| format!("unexpected {c:?}"))?;
        out.push(Tok::Op(op));
        i += op.len();
    }
    Ok(out)
}

#[derive(Debug, Clone)]
enum Ast {
    Lit(Value),
    Name(String, Option<String>),
    Unary(&'static str, Box<Ast>),
    Bin(&'static str, Box<Ast>, Box<Ast>),
    Cond(Box<Ast>, Box<Ast>, Box<Ast>),
    Index(Box<Ast>, Box<Ast>),
    Call(String, Vec<Ast>),
    Map(Vec<(Value, Value)>),
    Let(Vec<(String, Ast)>, Box<Ast>),
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn eat_op(&mut self, op: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Op(o)) if *o == op) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, op: &str) -> Result<(), String> {
        if self.eat_op(op) {
            Ok(())
        } else {
            Err(format!("expected {op} at token {}", self.pos))
        }
    }
    fn expr(&mut self) -> Result<Ast, String> {
        if matches!(self.peek(), Some(Tok::Ident(s)) if s == "let") {
            self.pos += 1;
            let mut binds = Vec::new();
            loop {
                let Some(Tok::Ident(name)) = self.peek().cloned() else {
                    return Err("let name".into());
                };
                self.pos += 1;
                if !matches!(self.peek(), Some(Tok::Ident(s)) if s == "__ASSIGN") {
                    return Err("expected =".into());
                }
                self.pos += 1;
                binds.push((name, self.cond()?));
                if !self.eat_op(",") {
                    break;
                }
            }
            if !matches!(self.peek(), Some(Tok::Ident(s)) if s == "in") {
                return Err("expected in".into());
            }
            self.pos += 1;
            return Ok(Ast::Let(binds, Box::new(self.expr()?)));
        }
        self.cond()
    }
    fn cond(&mut self) -> Result<Ast, String> {
        let c = self.binary(0)?;
        if self.eat_op("?") {
            let a = self.expr()?;
            self.expect(":")?;
            let b = self.expr()?;
            return Ok(Ast::Cond(Box::new(c), Box::new(a), Box::new(b)));
        }
        Ok(c)
    }
    fn binary(&mut self, level: usize) -> Result<Ast, String> {
        const LEVELS: [&[&str]; 11] = [
            &["??"],
            &["||"],
            &["&&"],
            &["==", "!=", "<", "<=", ">", ">="],
            &["|"],
            &["^"],
            &["&"],
            &["<<", ">>"],
            &["+", "-"],
            &["*", "/", "//", "%"],
            &[],
        ];
        if level == 10 {
            return self.unary();
        }
        let mut lhs = self.binary(level + 1)?;
        while let Some(Tok::Op(op)) = self.peek().cloned() {
            if !LEVELS[level].contains(&op) {
                break;
            }
            self.pos += 1;
            let rhs = self.binary(level + 1)?;
            lhs = Ast::Bin(op, Box::new(lhs), Box::new(rhs));
            if level == 3 {
                break; // comparisons do not chain
            }
        }
        Ok(lhs)
    }
    fn unary(&mut self) -> Result<Ast, String> {
        for op in ["-", "!", "~"] {
            if self.eat_op(op) {
                let op: &'static str = OPS.iter().find(|o| **o == op).copied().unwrap_or("-");
                return Ok(Ast::Unary(op, Box::new(self.unary()?)));
            }
        }
        let mut p = self.primary()?;
        while self.eat_op("[") {
            let i = self.expr()?;
            self.expect("]")?;
            p = Ast::Index(Box::new(p), Box::new(i));
        }
        Ok(p)
    }
    fn literal(&mut self) -> Result<Value, String> {
        let neg = self.eat_op("-");
        let v = match self.peek().cloned() {
            Some(Tok::Num(v)) => v,
            Some(Tok::Str(s)) => Value::Str(s),
            Some(Tok::Ident(s)) if s == "true" => Value::Bool(true),
            Some(Tok::Ident(s)) if s == "false" => Value::Bool(false),
            Some(Tok::Ident(s)) if s == "null" => Value::Null,
            t => return Err(format!("literal expected, got {t:?}")),
        };
        self.pos += 1;
        Ok(match (neg, v) {
            (true, Value::Int(i)) => Value::Int(-i),
            (true, Value::Float(f)) => Value::Float(-f),
            (_, v) => v,
        })
    }
    fn primary(&mut self) -> Result<Ast, String> {
        match self.peek().cloned() {
            Some(Tok::Num(v)) => {
                self.pos += 1;
                Ok(Ast::Lit(v))
            }
            Some(Tok::Str(s)) => {
                self.pos += 1;
                Ok(Ast::Lit(Value::Str(s)))
            }
            Some(Tok::Op("(")) => {
                self.pos += 1;
                let e = self.expr()?;
                self.expect(")")?;
                Ok(e)
            }
            Some(Tok::Ident(id)) => {
                self.pos += 1;
                match id.as_str() {
                    "true" => return Ok(Ast::Lit(Value::Bool(true))),
                    "false" => return Ok(Ast::Lit(Value::Bool(false))),
                    "null" => return Ok(Ast::Lit(Value::Null)),
                    _ => {}
                }
                if self.eat_op("(") {
                    let mut args = Vec::new();
                    if !self.eat_op(")") {
                        loop {
                            if matches!(self.peek(), Some(Tok::Ident(s)) if s == "__LBRACE") {
                                self.pos += 1;
                                let mut m = Vec::new();
                                if !matches!(self.peek(), Some(Tok::Ident(s)) if s == "__RBRACE") {
                                    loop {
                                        let k = self.literal()?;
                                        self.expect(":")?;
                                        let v = self.literal()?;
                                        m.push((k, v));
                                        if !self.eat_op(",") {
                                            break;
                                        }
                                    }
                                }
                                if !matches!(self.peek(), Some(Tok::Ident(s)) if s == "__RBRACE") {
                                    return Err("expected }".into());
                                }
                                self.pos += 1;
                                args.push(Ast::Map(m));
                            } else {
                                args.push(self.expr()?);
                            }
                            if !self.eat_op(",") {
                                break;
                            }
                        }
                        self.expect(")")?;
                    }
                    return Ok(Ast::Call(id, args));
                }
                if self.eat_op(".") {
                    let Some(Tok::Ident(f)) = self.peek().cloned() else {
                        return Err("field name".into());
                    };
                    self.pos += 1;
                    return Ok(Ast::Name(id, Some(f)));
                }
                Ok(Ast::Name(id, None))
            }
            t => Err(format!("unexpected {t:?}")),
        }
    }
}

/// A compiled source expression.
#[derive(Debug)]
pub struct SourceExpr {
    src: String,
    ast: Ast,
}

struct Scope<'a, 'c> {
    ctx: &'a ExprCtx<'c>,
    locals: Vec<(String, Value)>,
}

fn num_bin(op: &str, a: &Value, b: &Value) -> Value {
    use Value::*;
    match (a, b) {
        (Int(x), Int(y)) => {
            let (x, y) = (*x, *y);
            match op {
                "+" => x.checked_add(y).map_or(Null, Int),
                "-" => x.checked_sub(y).map_or(Null, Int),
                "*" => x.checked_mul(y).map_or(Null, Int),
                "/" => {
                    if y == 0 {
                        Null
                    } else {
                        Float(x as f64 / y as f64)
                    }
                }
                "//" => floor_div(x, y).map_or(Null, Int),
                "%" => floor_div(x, y)
                    .and_then(|q| q.checked_mul(y))
                    .and_then(|m| x.checked_sub(m))
                    .map_or(Null, Int),
                _ => Null,
            }
        }
        _ => {
            let (Some(x), Some(y)) = (num(a), num(b)) else {
                return Null;
            };
            let r = match op {
                "+" => x + y,
                "-" => x - y,
                "*" => x * y,
                "/" => {
                    if y == 0.0 {
                        return Null;
                    }
                    x / y
                }
                "//" => {
                    if y == 0.0 {
                        return Null;
                    }
                    (x / y).floor()
                }
                "%" => {
                    if y == 0.0 {
                        return Null;
                    }
                    x - y * (x / y).floor()
                }
                _ => return Null,
            };
            if r.is_finite() { Float(r) } else { Null }
        }
    }
}

fn floor_div(x: i64, y: i64) -> Option<i64> {
    let q = x.checked_div(y)?;
    if x % y != 0 && ((x < 0) != (y < 0)) {
        Some(q - 1)
    } else {
        Some(q)
    }
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

fn int(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        _ => None,
    }
}

fn boolish(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Null => Some(false),
        _ => None,
    }
}

fn read_int(b: &[u8], o: i64, n: usize, be: bool, signed: bool) -> Value {
    let Ok(o) = usize::try_from(o) else {
        return Value::Null;
    };
    let Some(s) = b.get(o..o + n) else {
        return Value::Null;
    };
    let mut v: u64 = 0;
    if be {
        for &x in s {
            v = (v << 8) | u64::from(x);
        }
    } else {
        for &x in s.iter().rev() {
            v = (v << 8) | u64::from(x);
        }
    }
    if signed {
        let sh = 64 - 8 * n as u32;
        Value::Int(((v << sh) as i64) >> sh)
    } else {
        Value::Int(v as i64)
    }
}

impl SourceExpr {
    fn eval_ast(&self, a: &Ast, s: &mut Scope<'_, '_>) -> Value {
        use Value::*;
        match a {
            Ast::Lit(v) => v.clone(),
            Ast::Map(_) => Null,
            Ast::Name(n, f) => match (n.as_str(), f.as_deref()) {
                ("x", None) => s.ctx.x.clone(),
                ("payload", None) => s.ctx.payload.map_or(Null, |p| Bytes(p.to_vec())),
                ("last", None) => s.ctx.last.map_or(Null, |l| Str(l.to_string())),
                ("state", Some(k)) => s.ctx.env.state(k),
                ("opt", Some(k)) => s.ctx.env.opt(k),
                ("gpd", Some(k)) => s.ctx.env.gpd(k),
                ("fp", Some(k)) => s.ctx.env.fp(k),
                (n, None) => s
                    .locals
                    .iter()
                    .rev()
                    .find(|(k, _)| k == n)
                    .map_or(Null, |(_, v)| v.clone()),
                _ => Null,
            },
            Ast::Let(binds, body) => {
                let base = s.locals.len();
                for (n, e) in binds {
                    let v = self.eval_ast(e, s);
                    s.locals.push((n.clone(), v));
                }
                let r = self.eval_ast(body, s);
                s.locals.truncate(base);
                r
            }
            Ast::Unary(op, e) => {
                let v = self.eval_ast(e, s);
                match (*op, v) {
                    ("-", Int(i)) => i.checked_neg().map_or(Null, Int),
                    ("-", Float(f)) => Float(-f),
                    ("!", v) => boolish(&v).map_or(Null, |b| Bool(!b)),
                    ("~", Int(i)) => Int(!i),
                    _ => Null,
                }
            }
            Ast::Cond(c, a, b) => match boolish(&self.eval_ast(c, s)) {
                Some(true) => self.eval_ast(a, s),
                Some(false) => self.eval_ast(b, s),
                None => Null,
            },
            Ast::Index(b, i) => {
                let (bv, iv) = (self.eval_ast(b, s), self.eval_ast(i, s));
                match (bv, iv) {
                    (Bytes(b), Int(i)) => usize::try_from(i)
                        .ok()
                        .and_then(|i| b.get(i).copied())
                        .map_or(Null, |x| Int(i64::from(x))),
                    _ => Null,
                }
            }
            Ast::Bin(op, l, r) => {
                let a = self.eval_ast(l, s);
                if *op == "??" {
                    return if a.is_null() { self.eval_ast(r, s) } else { a };
                }
                let b = self.eval_ast(r, s);
                match *op {
                    "&&" | "||" => match (boolish(&a), boolish(&b)) {
                        (Some(x), Some(y)) => Bool(if *op == "&&" { x && y } else { x || y }),
                        _ => Null,
                    },
                    "==" | "!=" => {
                        let eq = match (&a, &b) {
                            (Int(_) | Float(_), Int(_) | Float(_)) => num(&a) == num(&b),
                            _ => a == b,
                        };
                        Bool(if *op == "==" { eq } else { !eq })
                    }
                    "<" | "<=" | ">" | ">=" => match (num(&a), num(&b)) {
                        (Some(x), Some(y)) => Bool(match *op {
                            "<" => x < y,
                            "<=" => x <= y,
                            ">" => x > y,
                            _ => x >= y,
                        }),
                        _ => Null,
                    },
                    "|" | "^" | "&" | "<<" | ">>" => match (int(&a), int(&b)) {
                        (Some(x), Some(y)) => match *op {
                            "|" => Int(x | y),
                            "^" => Int(x ^ y),
                            "&" => Int(x & y),
                            _ if !(0..=63).contains(&y) => Null,
                            "<<" => Int(x << y),
                            _ => Int(x >> y),
                        },
                        _ => Null,
                    },
                    "+" => match (&a, &b) {
                        (Str(x), Str(y)) => {
                            let r = format!("{x}{y}");
                            if r.len() > 64 { Null } else { Str(r) }
                        }
                        _ => num_bin("+", &a, &b),
                    },
                    op => num_bin(op, &a, &b),
                }
            }
            Ast::Call(f, args) => self.call(f, args, s),
        }
    }

    fn call(&self, f: &str, args: &[Ast], s: &mut Scope<'_, '_>) -> Value {
        use Value::*;
        if f == "lookup" {
            let x = args.first().map_or(Null, |a| self.eval_ast(a, s));
            let Some(Ast::Map(m)) = args.get(1) else {
                return Null;
            };
            let found = m.iter().find(|(k, _)| match (k, &x) {
                (Int(_) | Float(_), Int(_) | Float(_)) => num(k) == num(&x),
                _ => *k == x,
            });
            return match found {
                Some((_, v)) => v.clone(),
                None => args.get(2).map_or(Null, |d| self.eval_ast(d, s)),
            };
        }
        let v: Vec<Value> = args.iter().map(|a| self.eval_ast(a, s)).collect();
        let arg = |i: usize| v.get(i).cloned().unwrap_or(Null);
        let bytes = |i: usize| match v.get(i) {
            Some(Bytes(b)) => Some(b.clone()),
            _ => None,
        };
        let rd = |n: usize, be: bool, signed: bool| match (bytes(0), int(&arg(1))) {
            (Some(b), Some(o)) => read_int(&b, o, n, be, signed),
            _ => Null,
        };
        match f {
            "len" => match arg(0) {
                Bytes(b) => Int(b.len() as i64),
                Str(st) => Int(st.len() as i64),
                _ => Null,
            },
            "u8" => rd(1, false, false),
            "i8" => rd(1, false, true),
            "u16le" => rd(2, false, false),
            "u16be" => rd(2, true, false),
            "i16le" => rd(2, false, true),
            "i16be" => rd(2, true, true),
            "u24le" => rd(3, false, false),
            "u24be" => rd(3, true, false),
            "i24le" => rd(3, false, true),
            "i24be" => rd(3, true, true),
            "u32le" => rd(4, false, false),
            "u32be" => rd(4, true, false),
            "i32le" => rd(4, false, true),
            "i32be" => rd(4, true, true),
            "f32le" | "f32be" => match rd(4, f == "f32be", false) {
                Int(i) => Float(f64::from(f32::from_bits(i as u32))),
                _ => Null,
            },
            "ascii" => match (bytes(0), int(&arg(1)), int(&arg(2))) {
                (Some(b), Some(o), Some(n)) => usize::try_from(o)
                    .ok()
                    .zip(usize::try_from(n).ok())
                    .and_then(|(o, n)| b.get(o..o + n))
                    .map_or(Null, |s| Str(String::from_utf8_lossy(s).into_owned())),
                _ => Null,
            },
            "hex" => bytes(0).map_or(Null, |b| Str(crate::expr::hex(&b))),
            "pack" => {
                let Str(fmt) = arg(0) else { return Null };
                let mut out = Vec::new();
                for (i, item) in fmt.split_whitespace().enumerate() {
                    let Some(x) = int(&arg(i + 1)) else {
                        return Null;
                    };
                    let (n, be) = match item {
                        "u8" | "i8" => (1, false),
                        "u16le" | "i16le" => (2, false),
                        "u16be" | "i16be" => (2, true),
                        "u24le" => (3, false),
                        "u32le" | "i32le" => (4, false),
                        "u32be" | "i32be" => (4, true),
                        _ => return Null,
                    };
                    let le = x.to_le_bytes();
                    if be {
                        out.extend(le[..n].iter().rev());
                    } else {
                        out.extend_from_slice(&le[..n]);
                    }
                }
                if out.len() > 64 { Null } else { Bytes(out) }
            }
            "bit" => match (int(&arg(0)), int(&arg(1))) {
                (Some(x), Some(n)) if (0..64).contains(&n) => Bool((x >> n) & 1 == 1),
                _ => Null,
            },
            "bits" => match (int(&arg(0)), int(&arg(1)), int(&arg(2))) {
                (Some(x), Some(lo), Some(len))
                    if (0..64).contains(&lo) && (1..64).contains(&len) =>
                {
                    Int((x >> lo) & ((1i64 << len) - 1))
                }
                _ => Null,
            },
            "sext" => match (int(&arg(0)), int(&arg(1))) {
                (Some(x), Some(b)) if (1..=64).contains(&b) => {
                    let sh = 64 - b as u32;
                    Int((x << sh) >> sh)
                }
                _ => Null,
            },
            "bcd" => match int(&arg(0)) {
                Some(mut x) if x >= 0 => {
                    let (mut r, mut m) = (0i64, 1i64);
                    while x > 0 {
                        let d = x & 0xF;
                        if d > 9 {
                            return Null;
                        }
                        r += d * m;
                        m *= 10;
                        x >>= 4;
                    }
                    Int(r)
                }
                _ => Null,
            },
            "min" | "max" => match (num(&arg(0)), num(&arg(1))) {
                (Some(a), Some(b)) => {
                    let pick_a = if f == "min" { a <= b } else { a >= b };
                    if pick_a { arg(0) } else { arg(1) }
                }
                _ => Null,
            },
            "clamp" => match (num(&arg(0)), num(&arg(1)), num(&arg(2))) {
                (Some(x), Some(lo), Some(_)) if x < lo => arg(1),
                (Some(x), Some(_), Some(hi)) if x > hi => arg(2),
                (Some(_), Some(_), Some(_)) => arg(0),
                _ => Null,
            },
            "abs" => match arg(0) {
                Int(i) => i.checked_abs().map_or(Null, Int),
                Float(x) => Float(x.abs()),
                _ => Null,
            },
            "round" => match (num(&arg(0)), arg(1)) {
                (Some(x), Null) => Int(x.round() as i64),
                (Some(x), Int(d)) if (0..=9).contains(&d) => {
                    let m = 10f64.powi(d as i32);
                    Float((x * m).round() / m)
                }
                _ => Null,
            },
            "floor" => num(&arg(0)).map_or(Null, |x| Int(x.floor() as i64)),
            "ceil" => num(&arg(0)).map_or(Null, |x| Int(x.ceil() as i64)),
            "int" => match arg(0) {
                Bool(b) => Int(i64::from(b)),
                v => num(&v).map_or(Null, |x| Int(x.trunc() as i64)),
            },
            "float" => num(&arg(0)).map_or(Null, Float),
            "pow10" => num(&arg(0))
                .map(|x| 10f64.powf(x))
                .filter(|r| r.is_finite())
                .map_or(Null, Float),
            "log10" => num(&arg(0))
                .filter(|x| *x > 0.0)
                .map_or(Null, |x| Float(x.log10())),
            "str" => match arg(0) {
                Int(i) => Str(i.to_string()),
                Float(x) => {
                    let mut b = [0u8; 32];
                    Str(zdef::num::format_number(zdef::model::Num::Float(x), &mut b).into())
                }
                Bool(b) => Str(b.to_string()),
                Str(s) => Str(s),
                _ => Null,
            },
            "isnull" => Bool(arg(0).is_null()),
            "battery_percent" => num(&arg(0)).map_or(Null, |mv| s.ctx.env.battery_percent(mv)),
            "now" => Int(s.ctx.env.now()),
            "tz_offset" => Int(s.ctx.env.tz_offset()),
            _ => Null,
        }
    }
}

impl Expression for SourceExpr {
    fn eval(&self, ctx: &ExprCtx<'_>) -> Value {
        let mut s = Scope {
            ctx,
            locals: Vec::new(),
        };
        self.eval_ast(&self.ast, &mut s)
    }
    fn source(&self) -> Option<&str> {
        Some(&self.src)
    }
}

/// Compiles a source expression.
pub fn compile(src: &str) -> Result<Expr, String> {
    // `{`/`}` only appear in lookup() map literals; `=` only in `let`.
    let pre = src.replace('{', " __LBRACE ").replace('}', " __RBRACE ");
    let pre = replace_let_assign(&pre);
    let toks = lex(&pre)?;
    let mut p = Parser { toks, pos: 0 };
    let ast = p.expr()?;
    if p.pos != p.toks.len() {
        return Err(format!("trailing tokens in {src:?}"));
    }
    Ok(Arc::new(SourceExpr {
        src: src.to_string(),
        ast,
    }))
}

/// Rewrites single `=` (let bindings) into a token the lexer accepts as
/// the "=" operator marker.
fn replace_let_assign(s: &str) -> String {
    let b: Vec<char> = s.chars().collect();
    let mut out = String::new();
    for (i, c) in b.iter().enumerate() {
        let prev = if i > 0 { b[i - 1] } else { ' ' };
        let next = b.get(i + 1).copied().unwrap_or(' ');
        if *c == '=' && !matches!(prev, '=' | '!' | '<' | '>') && next != '=' {
            out.push_str(" __ASSIGN ");
        } else {
            out.push(*c);
        }
    }
    out
}

/// The reference compiler as an [`ExprCompiler`].
#[derive(Debug, Default, Clone, Copy)]
pub struct ReferenceCompiler;

impl ExprCompiler for ReferenceCompiler {
    fn compile(&self, src: &str) -> Result<Expr, String> {
        compile(src)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::{EmptyEnv, ExprEnv};

    fn ev(src: &str, x: Value, payload: Option<&[u8]>, last: Option<&str>) -> Value {
        struct Env;
        impl ExprEnv for Env {
            fn state(&self, k: &str) -> Value {
                if k == "detection_interval" {
                    Value::Int(60)
                } else {
                    Value::Null
                }
            }
            fn gpd(&self, k: &str) -> Value {
                if k == "model_id" {
                    Value::Int(0xF103)
                } else {
                    Value::Null
                }
            }
        }
        let e = compile(src).unwrap_or_else(|e| panic!("{src}: {e}"));
        e.eval(&ExprCtx {
            x,
            payload,
            last,
            env: &Env,
        })
    }

    #[test]
    fn spec_examples() {
        assert_eq!(ev("x == 1", Value::Int(1), None, None), Value::Bool(true));
        assert_eq!(
            ev(
                "x > 130536 ? 0 : x - 65536",
                Value::Int(65536 + 120),
                None,
                None
            ),
            Value::Int(120)
        );
        assert_eq!(
            ev(
                "x > 130536 ? 0 : x - 65536",
                Value::Int(200_000),
                None,
                None
            ),
            Value::Int(0)
        );
        assert_eq!(
            ev(
                "(state.detection_interval ?? 30) + 2",
                Value::Null,
                None,
                None
            ),
            Value::Int(62)
        );
        assert_eq!(
            ev("(state.other ?? 30) + 2", Value::Null, None, None),
            Value::Int(32)
        );
        assert_eq!(
            ev(
                r#"bit(payload[0], 0) && last != "press""#,
                Value::Null,
                Some(&[1]),
                Some("release")
            ),
            Value::Bool(true)
        );
        assert_eq!(
            ev(
                r#"bit(payload[0], 0) && last != "press""#,
                Value::Null,
                Some(&[1]),
                Some("press")
            ),
            Value::Bool(false)
        );
        assert_eq!(
            ev(
                "x == 0 ? 0 : round(pow10((x - 1) / 10000))",
                Value::Int(40001),
                None,
                None
            ),
            Value::Int(10000)
        );
        assert_eq!(
            ev(
                "u16le(payload, 2)",
                Value::Null,
                Some(&[0, 0, 0x34, 0x12]),
                None
            ),
            Value::Int(0x1234)
        );
        assert_eq!(
            ev(
                "bit(gpd.model_id, 0) && bit(gpd.model_id, 1)",
                Value::Null,
                None,
                None
            ),
            Value::Bool(true)
        );
        assert_eq!(
            ev(
                r#"lookup(payload[0], {1: "on", 2: "up", 3: "down", 4: "off"}) + "_" + lookup(payload[4], {0: "press", 1: "hold"})"#,
                Value::Null,
                Some(&[2, 0, 0, 0, 1]),
                None
            ),
            Value::Str("up_hold".into())
        );
        assert_eq!(
            ev(
                r#""recall_" + str(payload[2])"#,
                Value::Null,
                Some(&[0, 0, 3]),
                None
            ),
            Value::Str("recall_3".into())
        );
        assert_eq!(
            ev("x & 0x10 == 0x10", Value::Int(0x30), None, None),
            Value::Bool(true)
        );
        assert_eq!(
            ev("let a = x * 2, b = a + 1 in b", Value::Int(3), None, None),
            Value::Int(7)
        );
        assert_eq!(ev("payload[9]", Value::Null, Some(&[1]), None), Value::Null);
        assert_eq!(ev("7 // 2", Value::Null, None, None), Value::Int(3));
        assert_eq!(ev("-7 // 2", Value::Null, None, None), Value::Int(-4));
        assert_eq!(ev("-7 % 2", Value::Null, None, None), Value::Int(1));
        assert_eq!(ev("1 / 0", Value::Null, None, None), Value::Null);
        assert_eq!(
            ev(r#"pack("u8 u16le", 1, 513)"#, Value::Null, None, None),
            Value::Bytes(vec![1, 1, 2])
        );
        assert_eq!(ev("true", Value::Null, None, None), Value::Bool(true));
        let e = compile("x").unwrap();
        assert_eq!(
            e.eval(&ExprCtx::with_x(Value::Int(4), &EmptyEnv)),
            Value::Int(4)
        );
    }
}
