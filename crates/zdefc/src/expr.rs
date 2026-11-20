//! Expression compiler (spec 7.2 grammar -> 7.5 bytecode) and decompiler.
//!
//! Code generation follows `tools/zdefc-proto` exactly (no constant folding),
//! so its outputs stay byte-identical. The decompiler turns compiler output
//! back into source that recompiles to the same bytes; `decompile` checks
//! that and fails otherwise.

use zdef::expr::{self as zx, Context, FP_FIELDS, FUNCS, GPD_FIELDS, MAX_CODE, MAX_LOCALS, op};
use zdef::model::{Const, Scalar};

#[derive(Clone, Debug, PartialEq)]
pub struct ExprError {
    /// 1-based column in the expression source (0: unknown).
    pub col: usize,
    pub message: String,
}

impl std::fmt::Display for ExprError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.col > 0 {
            write!(f, "column {}: {}", self.col, self.message)
        } else {
            f.write_str(&self.message)
        }
    }
}

fn err<T>(col: usize, m: impl Into<String>) -> Result<T, ExprError> {
    Err(ExprError {
        col,
        message: m.into(),
    })
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Int(i64),
    Float(f64),
    Str(String),
    Name(String),
    Op(&'static str),
    Eof,
}

const OPS: &[&str] = &[
    "??", "==", "!=", "<=", ">=", "&&", "||", "<<", ">>", "//", "-", "+", "*", "/", "%", "&", "|",
    "^", "~", "!", "<", ">", "?", ":", "(", ")", "[", "]", ",", "{", "}", "=",
];

fn lex(src: &str) -> Result<Vec<(Tok, usize)>, ExprError> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let col = src[..i].chars().count() + 1;
        if c.is_ascii_digit() {
            let st = i;
            if b[i..].starts_with(b"0x") || b[i..].starts_with(b"0X") {
                i += 2;
                let hs = i;
                while i < b.len() && b[i].is_ascii_hexdigit() {
                    i += 1;
                }
                if i == hs {
                    return err(col, "bad hexadecimal number");
                }
                let v = i64::from_str_radix(&src[hs..i], 16).map_err(|_| ExprError {
                    col,
                    message: "integer out of range".into(),
                })?;
                out.push((Tok::Int(v), col));
                continue;
            }
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            let mut float = false;
            if i < b.len() && b[i] == b'.' {
                float = true;
                i += 1;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                let save = i;
                i += 1;
                if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
                    i += 1;
                }
                let es = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                if i == es {
                    i = save;
                } else {
                    float = true;
                }
            }
            let t = &src[st..i];
            if float {
                let f: f64 = t.parse().map_err(|_| ExprError {
                    col,
                    message: format!("bad number {t}"),
                })?;
                if !f.is_finite() {
                    return err(col, "number out of range");
                }
                out.push((Tok::Float(f), col));
            } else {
                out.push((
                    Tok::Int(t.parse().map_err(|_| ExprError {
                        col,
                        message: format!("integer {t} out of range"),
                    })?),
                    col,
                ));
            }
            continue;
        }
        if c == b'"' {
            i += 1;
            let mut s = String::new();
            loop {
                let Some(&ch) = b.get(i) else {
                    return err(col, "unterminated string");
                };
                match ch {
                    b'"' => {
                        i += 1;
                        break;
                    }
                    b'\\' => {
                        let e = *b.get(i + 1).ok_or(ExprError {
                            col,
                            message: "unterminated string".into(),
                        })?;
                        s.push(match e {
                            b'"' => '"',
                            b'\\' => '\\',
                            b'n' => '\n',
                            b't' => '\t',
                            _ => return err(col, format!("bad escape \\{}", e as char)),
                        });
                        i += 2;
                    }
                    _ => {
                        let ch = src[i..].chars().next().unwrap();
                        s.push(ch);
                        i += ch.len_utf8();
                    }
                }
            }
            out.push((Tok::Str(s), col));
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let st = i;
            let ident = |i: &mut usize| {
                while *i < b.len() && (b[*i].is_ascii_alphanumeric() || b[*i] == b'_') {
                    *i += 1;
                }
            };
            ident(&mut i);
            if i + 1 < b.len()
                && b[i] == b'.'
                && (b[i + 1].is_ascii_alphabetic() || b[i + 1] == b'_')
            {
                i += 1;
                ident(&mut i);
            }
            out.push((Tok::Name(src[st..i].to_string()), col));
            continue;
        }
        match OPS.iter().find(|o| b[i..].starts_with(o.as_bytes())) {
            Some(o) => {
                out.push((Tok::Op(o), col));
                i += o.len();
            }
            None => {
                return err(
                    col,
                    format!(
                        "unexpected character {:?}",
                        src[i..].chars().next().unwrap()
                    ),
                );
            }
        }
    }
    out.push((Tok::Eof, src.chars().count() + 1));
    Ok(out)
}

/// Expression AST.
#[derive(Clone, Debug, PartialEq)]
pub enum Ast {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Null,
    X,
    Payload,
    Last,
    State(String),
    Opt(String),
    Gpd(u8),
    Fp(u8),
    Local(u8),
    Unary(u8, Box<Ast>),
    Bin(u8, Box<Ast>, Box<Ast>),
    Coalesce(Box<Ast>, Box<Ast>),
    Cond(Box<Ast>, Box<Ast>, Box<Ast>),
    Index(Box<Ast>, Box<Ast>),
    Call(u8, Vec<Ast>),
    Lookup(Box<Ast>, Vec<(Scalar, Scalar)>, Option<Box<Ast>>),
    Let(Vec<(u8, Ast)>, Box<Ast>),
}

// Binary levels, lowest first (Rust precedence: comparisons below bit operators).
const LEVELS: &[&[(&str, u8)]] = &[
    &[("??", 0)],
    &[("||", op::LOR)],
    &[("&&", op::LAND)],
    &[
        ("==", op::EQ),
        ("!=", op::NE),
        ("<", op::LT),
        ("<=", op::LE),
        (">", op::GT),
        (">=", op::GE),
    ],
    &[("|", op::OR)],
    &[("^", op::XOR)],
    &[("&", op::AND)],
    &[("<<", op::SHL), (">>", op::SHR)],
    &[("+", op::ADD), ("-", op::SUB)],
    &[
        ("*", op::MUL),
        ("/", op::DIV),
        ("//", op::IDIV),
        ("%", op::MOD),
    ],
];
const CMP_LEVEL: usize = 3;

const RESERVED: &[&str] = &[
    "x", "payload", "last", "state", "opt", "gpd", "fp", "true", "false", "null", "let", "in",
    "lookup",
];

struct Parser {
    t: Vec<(Tok, usize)>,
    i: usize,
    scopes: Vec<(String, u8)>,
    next_slot: u8,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.t[self.i].0
    }
    fn col(&self) -> usize {
        self.t[self.i].1
    }
    fn is_op(&self, o: &str) -> bool {
        matches!(self.peek(), Tok::Op(x) if *x == o)
    }
    fn expect(&mut self, o: &str) -> Result<(), ExprError> {
        if self.is_op(o) {
            self.i += 1;
            Ok(())
        } else {
            err(self.col(), format!("expected '{o}'"))
        }
    }

    fn expr(&mut self) -> Result<Ast, ExprError> {
        if matches!(self.peek(), Tok::Name(n) if n == "let") {
            return self.let_expr();
        }
        self.cond()
    }

    fn let_expr(&mut self) -> Result<Ast, ExprError> {
        self.i += 1;
        let depth = self.scopes.len();
        let mut binds = Vec::new();
        loop {
            let col = self.col();
            let Tok::Name(name) = self.peek().clone() else {
                return err(col, "expected a name after let");
            };
            if name.contains('.')
                || RESERVED.contains(&name.as_str())
                || zx::func_index(&name).is_some()
            {
                return err(col, format!("{name:?} cannot be a let name"));
            }
            self.i += 1;
            self.expect("=")?;
            let v = self.cond()?;
            if self.next_slot as usize >= MAX_LOCALS {
                return err(col, "more than 8 let names");
            }
            let slot = self.next_slot;
            self.next_slot += 1;
            self.scopes.push((name, slot));
            binds.push((slot, v));
            if self.is_op(",") {
                self.i += 1;
                continue;
            }
            break;
        }
        if !matches!(self.peek(), Tok::Name(n) if n == "in") {
            return err(self.col(), "expected 'in'");
        }
        self.i += 1;
        let body = self.expr()?;
        self.scopes.truncate(depth);
        Ok(Ast::Let(binds, Box::new(body)))
    }

    fn cond(&mut self) -> Result<Ast, ExprError> {
        let c = self.binary(0)?;
        if self.is_op("?") {
            self.i += 1;
            let a = self.expr()?;
            self.expect(":")?;
            let b = self.expr()?;
            return Ok(Ast::Cond(Box::new(c), Box::new(a), Box::new(b)));
        }
        Ok(c)
    }

    fn binary(&mut self, level: usize) -> Result<Ast, ExprError> {
        if level == LEVELS.len() {
            return self.unary();
        }
        let mut l = self.binary(level + 1)?;
        while let Tok::Op(o) = self.peek() {
            let Some(&(_, code)) = LEVELS[level].iter().find(|x| x.0 == *o) else {
                break;
            };
            self.i += 1;
            let r = self.binary(level + 1)?;
            l = if level == 0 {
                Ast::Coalesce(Box::new(l), Box::new(r))
            } else {
                Ast::Bin(code, Box::new(l), Box::new(r))
            };
            if level == CMP_LEVEL {
                if let Tok::Op(o2) = self.peek() {
                    if LEVELS[CMP_LEVEL].iter().any(|x| x.0 == *o2) {
                        return err(self.col(), "comparisons do not chain");
                    }
                }
                break;
            }
        }
        Ok(l)
    }

    fn unary(&mut self) -> Result<Ast, ExprError> {
        for (s, code) in [("-", op::NEG), ("!", op::LNOT), ("~", op::BNOT)] {
            if self.is_op(s) {
                self.i += 1;
                return Ok(Ast::Unary(code, Box::new(self.unary()?)));
            }
        }
        let mut p = self.primary()?;
        while self.is_op("[") {
            self.i += 1;
            let ix = self.expr()?;
            self.expect("]")?;
            p = Ast::Index(Box::new(p), Box::new(ix));
        }
        Ok(p)
    }

    fn primary(&mut self) -> Result<Ast, ExprError> {
        let col = self.col();
        let t = self.peek().clone();
        self.i += 1;
        Ok(match t {
            Tok::Int(v) => Ast::Int(v),
            Tok::Float(v) => Ast::Float(v),
            Tok::Str(s) => Ast::Str(s),
            Tok::Op("(") => {
                let e = self.expr()?;
                self.expect(")")?;
                e
            }
            Tok::Name(n) => return self.name(n, col),
            Tok::Eof => return err(col, "unexpected end of expression"),
            t => return err(col, format!("unexpected {t:?}")),
        })
    }

    fn name(&mut self, n: String, col: usize) -> Result<Ast, ExprError> {
        match n.as_str() {
            "x" => return Ok(Ast::X),
            "payload" => return Ok(Ast::Payload),
            "last" => return Ok(Ast::Last),
            "true" => return Ok(Ast::Bool(true)),
            "false" => return Ok(Ast::Bool(false)),
            "null" => return Ok(Ast::Null),
            _ => {}
        }
        if let Some((ns, f)) = n.split_once('.') {
            return Ok(match ns {
                "state" => Ast::State(f.into()),
                "opt" => Ast::Opt(f.into()),
                "gpd" => Ast::Gpd(GPD_FIELDS.iter().position(|x| *x == f).ok_or(ExprError {
                    col,
                    message: format!("unknown field gpd.{f}"),
                })? as u8),
                "fp" => Ast::Fp(FP_FIELDS.iter().position(|x| x.0 == f).ok_or(ExprError {
                    col,
                    message: format!("unknown field fp.{f}"),
                })? as u8),
                _ => return err(col, format!("unknown name {n}")),
            });
        }
        if self.is_op("(") {
            return self.call(n, col);
        }
        if let Some((_, slot)) = self.scopes.iter().rev().find(|s| s.0 == n) {
            return Ok(Ast::Local(*slot));
        }
        err(col, format!("unknown identifier {n}"))
    }

    fn call(&mut self, f: String, col: usize) -> Result<Ast, ExprError> {
        self.expect("(")?;
        if f == "lookup" {
            let x = self.expr()?;
            self.expect(",")?;
            let m = self.map_literal()?;
            let d = if self.is_op(",") {
                self.i += 1;
                Some(Box::new(self.expr()?))
            } else {
                None
            };
            self.expect(")")?;
            return Ok(Ast::Lookup(Box::new(x), m, d));
        }
        let fi = zx::func_index(&f).ok_or(ExprError {
            col,
            message: format!("unknown function {f}"),
        })?;
        let mut args = Vec::new();
        if !self.is_op(")") {
            loop {
                args.push(self.expr()?);
                if self.is_op(",") {
                    self.i += 1;
                    continue;
                }
                break;
            }
        }
        self.expect(")")?;
        let fd = &FUNCS[fi as usize];
        if args.len() < fd.min_args as usize || args.len() > fd.max_args as usize {
            return err(
                col,
                format!(
                    "{f} takes {}..{} arguments, got {}",
                    fd.min_args,
                    fd.max_args,
                    args.len()
                ),
            );
        }
        if fi == zx::func::PACK && !matches!(args.first(), Some(Ast::Str(_))) {
            return err(col, "pack needs a literal format string");
        }
        Ok(Ast::Call(fi, args))
    }

    fn literal(&mut self) -> Result<Scalar, ExprError> {
        let col = self.col();
        let neg = self.is_op("-");
        if neg {
            self.i += 1;
        }
        let t = self.peek().clone();
        self.i += 1;
        Ok(match (t, neg) {
            (Tok::Int(v), n) => Scalar::Int(if n { -v } else { v }),
            (Tok::Float(v), n) => Scalar::Float(if n { -v } else { v }),
            (Tok::Str(s), false) => Scalar::Str(s),
            (Tok::Name(n), false) if n == "true" => Scalar::Bool(true),
            (Tok::Name(n), false) if n == "false" => Scalar::Bool(false),
            (Tok::Name(n), false) if n == "null" => Scalar::Null,
            _ => return err(col, "expected a literal"),
        })
    }

    fn map_literal(&mut self) -> Result<Vec<(Scalar, Scalar)>, ExprError> {
        self.expect("{")?;
        let mut pairs = Vec::new();
        while !self.is_op("}") {
            let k = self.literal()?;
            self.expect(":")?;
            let v = self.literal()?;
            pairs.push((k, v));
            if self.is_op(",") {
                self.i += 1;
            } else {
                break;
            }
        }
        self.expect("}")?;
        Ok(pairs)
    }
}

/// Parses an expression source.
pub fn parse(src: &str) -> Result<Ast, ExprError> {
    let mut p = Parser {
        t: lex(src)?,
        i: 0,
        scopes: Vec::new(),
        next_slot: 0,
    };
    let e = p.expr()?;
    if p.peek() != &Tok::Eof {
        return err(p.col(), "unexpected trailing input");
    }
    Ok(e)
}

/// Index of `c` in the pool, appending it if absent.
pub fn intern(pool: &mut Vec<Const>, c: Const) -> usize {
    if let Some(i) = pool.iter().position(|x| *x == c) {
        return i;
    }
    pool.push(c);
    pool.len() - 1
}

struct Gen<'p> {
    code: Vec<u8>,
    pool: &'p mut Vec<Const>,
}

impl Gen<'_> {
    fn uleb(&mut self, v: u64) {
        zx::write_uleb(&mut self.code, v);
    }
    fn konst(&mut self, opc: u8, c: Const) {
        let i = intern(self.pool, c);
        self.code.push(opc);
        self.uleb(i as u64);
    }
    fn jump(&mut self, opc: u8) -> usize {
        self.code.push(opc);
        self.code.push(0);
        self.code.len() - 1
    }
    fn patch(&mut self, at: usize) -> Result<(), ExprError> {
        let off = self.code.len() - at - 1;
        if off > 255 {
            return err(0, "jump too long");
        }
        self.code[at] = off as u8;
        Ok(())
    }

    fn emit(&mut self, a: &Ast) -> Result<(), ExprError> {
        match a {
            Ast::Int(v) => {
                if (-128..128).contains(v) {
                    self.code.extend_from_slice(&[op::I8, *v as i8 as u8]);
                } else {
                    self.code.push(op::INT);
                    self.uleb(zx::zigzag(*v));
                }
            }
            Ast::Float(f) => self.konst(op::CONST, Const::Float(*f)),
            Ast::Str(s) => self.konst(op::CONST, Const::Str(s.clone())),
            Ast::Bool(b) => self.code.push(if *b { op::TRUE } else { op::FALSE }),
            Ast::Null => self.code.push(op::NULL),
            Ast::X => self.code.push(op::X),
            Ast::Payload => self.code.push(op::PAYLOAD),
            Ast::Last => self.code.push(op::LAST),
            Ast::State(k) => self.konst(op::STATE, Const::Str(k.clone())),
            Ast::Opt(k) => self.konst(op::OPT, Const::Str(k.clone())),
            Ast::Gpd(f) => self.code.extend_from_slice(&[op::GPD, *f]),
            Ast::Fp(f) => self.code.extend_from_slice(&[op::FP, *f]),
            Ast::Local(n) => self.code.extend_from_slice(&[op::LDL, *n]),
            Ast::Unary(o, x) => {
                self.emit(x)?;
                self.code.push(*o);
            }
            Ast::Bin(o, l, r) => {
                self.emit(l)?;
                self.emit(r)?;
                self.code.push(*o);
            }
            Ast::Coalesce(l, r) => {
                self.emit(l)?;
                let j = self.jump(op::JNN);
                self.code.push(op::POP);
                self.emit(r)?;
                self.patch(j)?;
            }
            Ast::Cond(c, a, b) => {
                self.emit(c)?;
                let jz = self.jump(op::JZ);
                self.emit(a)?;
                let end = self.jump(op::JMP);
                self.patch(jz)?;
                self.emit(b)?;
                self.patch(end)?;
            }
            Ast::Index(b, i) => {
                self.emit(b)?;
                self.emit(i)?;
                self.code.push(op::INDEX);
            }
            Ast::Call(f, args) => {
                for x in args {
                    self.emit(x)?;
                }
                self.code
                    .extend_from_slice(&[op::CALL, *f, args.len() as u8]);
            }
            Ast::Lookup(x, m, d) => {
                self.emit(x)?;
                self.konst(op::LOOKUP, Const::Map(m.clone()));
                if let Some(d) = d {
                    let j = self.jump(op::JNN);
                    self.code.push(op::POP);
                    self.emit(d)?;
                    self.patch(j)?;
                }
            }
            Ast::Let(binds, body) => {
                for (slot, v) in binds {
                    self.emit(v)?;
                    self.code.extend_from_slice(&[op::STL, *slot]);
                }
                self.emit(body)?;
            }
        }
        Ok(())
    }
}

/// Generates bytecode for an AST with a shared constant pool.
pub fn codegen(a: &Ast, pool: &mut Vec<Const>) -> Result<Vec<u8>, ExprError> {
    let mut g = Gen {
        code: Vec::new(),
        pool,
    };
    g.emit(a)?;
    if g.code.len() > MAX_CODE {
        return err(
            0,
            format!(
                "expression compiles to {} bytes (limit {MAX_CODE})",
                g.code.len()
            ),
        );
    }
    Ok(g.code)
}

/// Compiles and verifies one expression for a context.
pub fn compile(src: &str, pool: &mut Vec<Const>, ctx: &Context) -> Result<Vec<u8>, ExprError> {
    let ast = parse(src)?;
    let code = codegen(&ast, pool)?;
    if let Err(e) = zx::verify(&code, pool, ctx) {
        return err(0, verify_message(&e, &code, ctx));
    }
    Ok(code)
}

/// Human-readable verifier error.
pub fn verify_message(e: &zx::VerifyError, code: &[u8], ctx: &Context) -> String {
    use zx::VerifyErrorKind as K;
    let what = |o: u8| -> String {
        match o {
            op::CALL => code
                .get(e.offset + 1)
                .and_then(|f| FUNCS.get(*f as usize))
                .map(|f| format!("{}()", f.name))
                .unwrap_or("call".into()),
            op::X => "x".into(),
            op::PAYLOAD => "payload".into(),
            op::LAST => "last".into(),
            op::STATE => "state.*".into(),
            op::OPT => "opt.*".into(),
            op::GPD => "gpd.*".into(),
            op::FP => "fp.*".into(),
            _ => op::name(o).unwrap_or("?").to_lowercase(),
        }
    };
    match e.kind {
        K::NotAllowed(o) => format!("{} is not available in `{}`", what(o), ctx.site.name()),
        K::Type(o) => format!("type error: {} cannot take these operand types", what(o)),
        K::StackOverflow => "expression too deep (stack limit 16)".into(),
        K::TooLong => "expression longer than 255 bytes".into(),
        k => format!("bytecode rejected at offset {}: {:?}", e.offset, k),
    }
}

// ------------------------------------------------------------------ decompiler

struct Frame {
    base: usize,
    binds: Vec<(u8, Ast)>,
}

fn scalar_of(c: &Const) -> Option<Ast> {
    Some(match c {
        Const::Str(s) => Ast::Str(s.clone()),
        Const::Float(f) => Ast::Float(*f),
        Const::Int(i) => Ast::Int(*i),
        Const::Map(_) => return None,
    })
}

/// Closes the let frames whose result slot is below `keep` (all frames for -1).
fn close_frames(stack: &mut [Ast], frames: &mut Vec<Frame>, keep_above: isize) {
    while let Some(f) = frames.last() {
        if (f.base as isize) <= keep_above {
            break;
        }
        let f = frames.pop().unwrap();
        if let Some(slot) = stack.get_mut(f.base) {
            let body = std::mem::replace(slot, Ast::Null);
            *slot = Ast::Let(f.binds, Box::new(body));
        }
    }
}

fn dec_range(code: &[u8], consts: &[Const], start: usize, end: usize) -> Result<Ast, String> {
    let mut stack: Vec<Ast> = Vec::new();
    let mut frames: Vec<Frame> = Vec::new();
    let mut pc = start;
    let bad = |pc: usize| format!("cannot decompile at offset {pc}");
    while pc < end {
        let o = code[pc];
        let n = zx::insn_len(code, pc).ok_or_else(|| bad(pc))?;
        let imm = code.get(pc + 1).copied().unwrap_or(0);
        let konst = |pc: usize| -> Result<&Const, String> {
            let (i, _) = zx::read_uleb(code, pc + 1).ok_or_else(|| bad(pc))?;
            consts.get(i as usize).ok_or_else(|| bad(pc))
        };
        macro_rules! pops {
            ($k:expr) => {{
                let k: usize = $k;
                if stack.len() < k {
                    return Err(bad(pc));
                }
                let keep = (stack.len() - k) as isize;
                close_frames(&mut stack, &mut frames, keep);
                stack.split_off(stack.len() - k)
            }};
        }
        match o {
            op::X => stack.push(Ast::X),
            op::I8 => stack.push(Ast::Int(imm as i8 as i64)),
            op::INT => stack.push(Ast::Int(zx::unzigzag(
                zx::read_uleb(code, pc + 1).ok_or_else(|| bad(pc))?.0,
            ))),
            op::CONST => stack.push(scalar_of(konst(pc)?).ok_or_else(|| bad(pc))?),
            op::NULL => stack.push(Ast::Null),
            op::TRUE => stack.push(Ast::Bool(true)),
            op::FALSE => stack.push(Ast::Bool(false)),
            op::LDL => stack.push(Ast::Local(imm)),
            op::STL => {
                let extend = frames.last().is_some_and(|f| f.base + 1 == stack.len());
                let v = if extend {
                    stack.pop().ok_or_else(|| bad(pc))?
                } else {
                    pops!(1).pop().unwrap()
                };
                if extend {
                    frames.last_mut().unwrap().binds.push((imm, v));
                } else {
                    frames.push(Frame {
                        base: stack.len(),
                        binds: vec![(imm, v)],
                    });
                }
            }
            op::STATE | op::OPT => {
                let Const::Str(k) = konst(pc)? else {
                    return Err(bad(pc));
                };
                stack.push(if o == op::STATE {
                    Ast::State(k.clone())
                } else {
                    Ast::Opt(k.clone())
                });
            }
            op::PAYLOAD => stack.push(Ast::Payload),
            op::LAST => stack.push(Ast::Last),
            op::GPD => stack.push(Ast::Gpd(imm)),
            op::FP => stack.push(Ast::Fp(imm)),
            op::NEG | op::LNOT | op::BNOT => {
                let a = pops!(1).pop().unwrap();
                stack.push(Ast::Unary(o, Box::new(a)));
            }
            op::INDEX => {
                let mut v = pops!(2);
                let i = v.pop().unwrap();
                let b = v.pop().unwrap();
                stack.push(Ast::Index(Box::new(b), Box::new(i)));
            }
            op::LOOKUP => {
                let Const::Map(m) = konst(pc)? else {
                    return Err(bad(pc));
                };
                let x = pops!(1).pop().unwrap();
                stack.push(Ast::Lookup(Box::new(x), m.clone(), None));
            }
            op::CALL => {
                let argc = code[pc + 2] as usize;
                let args = pops!(argc);
                stack.push(Ast::Call(imm, args));
            }
            op::JZ => {
                let c = pops!(1).pop().unwrap();
                let t = pc + 2 + imm as usize;
                if t < 2 || t > end || code.get(t - 2) != Some(&op::JMP) {
                    return Err(bad(pc));
                }
                let e = t + code[t - 1] as usize;
                if e > end {
                    return Err(bad(pc));
                }
                let a = dec_range(code, consts, pc + 2, t - 2)?;
                let b = dec_range(code, consts, t, e)?;
                stack.push(Ast::Cond(Box::new(c), Box::new(a), Box::new(b)));
                pc = e;
                continue;
            }
            op::JNN => {
                let l = pops!(1).pop().unwrap();
                let t = pc + 2 + imm as usize;
                if code.get(pc + 2) != Some(&op::POP) || t > end {
                    return Err(bad(pc));
                }
                let r = dec_range(code, consts, pc + 3, t)?;
                stack.push(match l {
                    Ast::Lookup(x, m, None) => Ast::Lookup(x, m, Some(Box::new(r))),
                    l => Ast::Coalesce(Box::new(l), Box::new(r)),
                });
                pc = t;
                continue;
            }
            _ if op::name(o).is_some() && o != op::JMP && o != op::POP => {
                let mut v = pops!(2);
                let r = v.pop().unwrap();
                let l = v.pop().unwrap();
                stack.push(Ast::Bin(o, Box::new(l), Box::new(r)));
            }
            _ => return Err(bad(pc)),
        }
        pc += n;
    }
    close_frames(&mut stack, &mut frames, -1);
    if stack.len() != 1 {
        return Err(format!("cannot decompile range {start}..{end}"));
    }
    Ok(stack.pop().unwrap())
}

fn prec(a: &Ast) -> u8 {
    match a {
        Ast::Let(..) | Ast::Cond(..) => 0,
        Ast::Coalesce(..) => 1,
        Ast::Bin(o, ..) => match *o {
            op::LOR => 2,
            op::LAND => 3,
            op::EQ | op::NE | op::LT | op::LE | op::GT | op::GE => 4,
            op::OR => 5,
            op::XOR => 6,
            op::AND => 7,
            op::SHL | op::SHR => 8,
            op::ADD | op::SUB => 9,
            _ => 10,
        },
        Ast::Unary(..) => 11,
        _ => 12,
    }
}

fn bin_sym(o: u8) -> &'static str {
    for l in LEVELS {
        for (s, c) in *l {
            if *c == o && *s != "??" {
                return s;
            }
        }
    }
    "?"
}

pub fn fmt_float(f: f64) -> String {
    let s = format!("{f}");
    if s.contains('.') || s.contains('e') || s.contains("inf") || s.contains("NaN") {
        s
    } else {
        format!("{s}.0")
    }
}

fn fmt_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn fmt_scalar(s: &Scalar) -> String {
    match s {
        Scalar::Null => "null".into(),
        Scalar::Bool(b) => b.to_string(),
        Scalar::Int(i) => i.to_string(),
        Scalar::Float(f) => fmt_float(*f),
        Scalar::Str(s) => fmt_str(s),
    }
}

/// Prints an AST as source.
pub fn print(a: &Ast) -> String {
    let mut s = String::new();
    pr(a, 0, &mut s);
    s
}

fn pr(a: &Ast, min: u8, o: &mut String) {
    let p = prec(a);
    let paren = p < min;
    if paren {
        o.push('(');
    }
    match a {
        Ast::Int(i) => o.push_str(&i.to_string()),
        Ast::Float(f) => o.push_str(&fmt_float(*f)),
        Ast::Str(s) => o.push_str(&fmt_str(s)),
        Ast::Bool(b) => o.push_str(if *b { "true" } else { "false" }),
        Ast::Null => o.push_str("null"),
        Ast::X => o.push('x'),
        Ast::Payload => o.push_str("payload"),
        Ast::Last => o.push_str("last"),
        Ast::State(k) => o.push_str(&format!("state.{k}")),
        Ast::Opt(k) => o.push_str(&format!("opt.{k}")),
        Ast::Gpd(f) => o.push_str(&format!(
            "gpd.{}",
            GPD_FIELDS.get(*f as usize).unwrap_or(&"?")
        )),
        Ast::Fp(f) => o.push_str(&format!(
            "fp.{}",
            FP_FIELDS.get(*f as usize).map(|x| x.0).unwrap_or("?")
        )),
        Ast::Local(n) => o.push_str(&format!("v{n}")),
        Ast::Unary(u, x) => {
            o.push_str(match *u {
                op::NEG => "-",
                op::LNOT => "!",
                _ => "~",
            });
            pr(x, 11, o);
        }
        Ast::Bin(b, l, r) => {
            let lp = if p == 4 { 5 } else { p };
            pr(l, lp, o);
            o.push_str(&format!(" {} ", bin_sym(*b)));
            pr(r, p + 1, o);
        }
        Ast::Coalesce(l, r) => {
            pr(l, 1, o);
            o.push_str(" ?? ");
            pr(r, 2, o);
        }
        Ast::Cond(c, x, y) => {
            pr(c, 1, o);
            o.push_str(" ? ");
            pr(x, 1, o);
            o.push_str(" : ");
            pr(y, 0, o);
        }
        Ast::Index(b, i) => {
            pr(b, 12, o);
            o.push('[');
            pr(i, 0, o);
            o.push(']');
        }
        Ast::Call(f, args) => {
            o.push_str(FUNCS.get(*f as usize).map(|f| f.name).unwrap_or("?"));
            o.push('(');
            for (i, x) in args.iter().enumerate() {
                if i > 0 {
                    o.push_str(", ");
                }
                pr(x, 0, o);
            }
            o.push(')');
        }
        Ast::Lookup(x, m, d) => {
            o.push_str("lookup(");
            pr(x, 0, o);
            o.push_str(", {");
            for (i, (k, v)) in m.iter().enumerate() {
                if i > 0 {
                    o.push_str(", ");
                }
                o.push_str(&fmt_scalar(k));
                o.push_str(": ");
                o.push_str(&fmt_scalar(v));
            }
            o.push('}');
            if let Some(d) = d {
                o.push_str(", ");
                pr(d, 0, o);
            }
            o.push(')');
        }
        Ast::Let(binds, body) => {
            o.push_str("let ");
            for (i, (slot, v)) in binds.iter().enumerate() {
                if i > 0 {
                    o.push_str(", ");
                }
                o.push_str(&format!("v{slot} = "));
                pr(v, 1, o);
            }
            o.push_str(" in ");
            pr(body, 0, o);
        }
    }
    if paren {
        o.push(')');
    }
}

/// Decompiles bytecode to source; fails unless the source recompiles to
/// identical bytes against `pool` (which must contain every constant used).
pub fn decompile(code: &[u8], pool: &[Const]) -> Result<String, String> {
    if code.is_empty() || code.len() > MAX_CODE {
        return Err("bad expression length".into());
    }
    let ast = dec_range(code, pool, 0, code.len())?;
    let src = print(&ast);
    let mut p2 = pool.to_vec();
    let again = parse(&src)
        .and_then(|a| codegen(&a, &mut p2))
        .map_err(|e| format!("decompiled {src:?} does not recompile: {e}"))?;
    if again != code || p2.len() != pool.len() {
        return Err(format!(
            "bytecode is not in canonical compiler form (decompiled as {src:?})"
        ));
    }
    Ok(src)
}

/// One-line disassembly.
pub fn disasm(code: &[u8]) -> String {
    let mut out = Vec::new();
    let mut pc = 0;
    while pc < code.len() {
        let Some(n) = zx::insn_len(code, pc) else {
            out.push(format!("?{:02x}", code[pc]));
            break;
        };
        let name = op::name(code[pc]).unwrap_or("?");
        let imm: Vec<String> = code[pc + 1..pc + n]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        out.push(if imm.is_empty() {
            name.to_string()
        } else {
            format!("{name} {}", imm.join(" "))
        });
        pc += n;
    }
    out.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use zdef::expr::{Site, Ty};

    fn hex(b: &[u8]) -> String {
        b.iter()
            .map(|x| format!("{x:02x}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn c(src: &str, ctx: Context) -> (String, Vec<Const>) {
        let mut pool = Vec::new();
        let code = compile(src, &mut pool, &ctx).unwrap_or_else(|e| panic!("{src}: {e}"));
        assert_eq!(
            decompile(&code, &pool)
                .map(|s| {
                    let mut p = pool.clone();
                    codegen(&parse(&s).unwrap(), &mut p).unwrap()
                })
                .unwrap(),
            code,
            "{src}"
        );
        (hex(&code), pool)
    }

    #[test]
    fn spec_7_6_examples() {
        let from = Context::new(Site::From).with_x(Ty::INT);
        assert_eq!(c("x == 1", from).0, "01 02 01 30");
        assert_eq!(
            c("x > 130536 ? 0 : x - 65536", from).0,
            "01 03 d0 f7 0f 34 3d 04 02 00 3c 06 01 03 80 80 08 11"
        );
        assert_eq!(
            c(
                "(state.detection_interval ?? 30) + 2",
                Context::new(Site::ResetAfter)
            )
            .0,
            "0a 00 3e 03 3f 02 1e 02 02 10"
        );
        assert_eq!(
            c(
                "bit(payload[0], 0) && last != \"press\"",
                Context::new(Site::When)
            )
            .0,
            "0c 02 00 40 02 00 70 00 02 0d 04 00 31 37"
        );
        assert_eq!(
            c("x == 0 ? 0 : round(pow10((x - 1) / 10000))", from).0,
            "01 02 00 30 3d 04 02 00 3c 0f 01 02 01 11 03 a0 9c 01 13 70 20 01 70 1b 01"
        );
        assert_eq!(
            c("u16le(payload, 2)", Context::new(Site::Event)).0,
            "0c 02 02 70 07 02"
        );
    }

    #[test]
    fn let_and_lookup() {
        let from = Context::new(Site::From).with_x(Ty::INT);
        let (h, _) = c("let a = x * 2, b = a + 1 in a > 10 ? b : -b", from);
        assert!(h.starts_with("01 02 02 12 09 00"));
        c("lookup(x, {1: \"on\", -2: \"off\"}, \"unknown\")", from);
        c(
            "f32le(pack(\"u16le u8\", x, 1), 0) * 0.5",
            Context::new(Site::To).with_x(Ty::INT),
        );
        c("a(1)".replace("a(1)", "min(x, 3, 4)").as_str(), from);
        c("1 - (2 - 3)", from);
        c("(1 < 2) == true", from);
        c("-(-x)", from);
        assert!(compile("1 < 2 < 3", &mut Vec::new(), &from).is_err());
        assert!(compile("last", &mut Vec::new(), &from).is_err());
        assert!(compile("now()", &mut Vec::new(), &from).is_err());
        assert!(compile("\"a\" - 1", &mut Vec::new(), &from).is_err());
        assert!(compile("let x = 1 in x", &mut Vec::new(), &from).is_err());
    }
}
