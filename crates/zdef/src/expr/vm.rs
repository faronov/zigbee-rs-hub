//! Bounded bytecode interpreter (spec 7.1, 7.4, 7.5).
//!
//! No heap use: a 16-slot value stack on the Rust stack and a fixed string
//! arena inside [`Vm`]. Every run-time error is returned as [`EvalError`];
//! the caller turns it into `null` and counts it (spec 7.1).

use super::{
    FP_FIELDS, FUNCS, GPD_FIELDS, MAX_CODE, MAX_LOCALS, MAX_STACK, MAX_STR, PACK_ITEMS, func,
    insn_len, op, read_uleb, unzigzag,
};
use crate::model::{Const, Num, Scalar};

/// Bytes of string/byte-string scratch per evaluation.
pub const ARENA_SIZE: usize = 512;

/// A run-time value. Strings and bytes borrow from the inputs, the constant
/// pool or the VM arena.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value<'a> {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(&'a str),
    Bytes(&'a [u8]),
}

impl Value<'_> {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Value::Int(i) => Some(i as f64),
            Value::Float(f) => Some(f),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvalError {
    /// Bad opcode, immediate, constant or stack shape (unverified code).
    Malformed,
    /// Backward or out-of-range jump.
    BadJump,
    StackOverflow,
    StackUnderflow,
    /// Operand of the wrong type.
    Type,
    /// Integer overflow or a non-finite float result.
    Overflow,
    DivisionByZero,
    /// Byte index, shift, bit position or pack value out of range.
    Range,
    /// String or byte string longer than 64 bytes, or arena exhausted.
    TooLong,
    /// The host does not provide the requested value (e.g. `now()`).
    Unavailable,
}

/// Per-evaluation inputs.
#[derive(Clone, Copy, Debug)]
pub struct Inputs<'a> {
    pub x: Value<'a>,
    pub payload: Option<&'a [u8]>,
    pub last: Option<&'a str>,
}

impl<'a> Inputs<'a> {
    pub const NONE: Inputs<'static> = Inputs {
        x: Value::Null,
        payload: None,
        last: None,
    };
    pub fn x(x: Value<'a>) -> Self {
        Inputs {
            x,
            payload: None,
            last: None,
        }
    }
    pub fn payload(p: &'a [u8]) -> Self {
        Inputs {
            x: Value::Null,
            payload: Some(p),
            last: None,
        }
    }
}

/// Device-side values the hub provides to expressions.
pub trait Host<'a> {
    /// `state.<key>`: current state of another entity of the device.
    fn state(&self, _key: &str) -> Value<'a> {
        Value::Null
    }
    /// `opt.<key>`: value of a `hub` binding entity.
    fn opt(&self, _key: &str) -> Value<'a> {
        Value::Null
    }
    /// `gpd.<field>`, index into [`GPD_FIELDS`].
    fn gpd(&self, _field: u8) -> Value<'a> {
        Value::Null
    }
    /// `fp.<field>`, index into [`FP_FIELDS`].
    fn fp(&self, _field: u8) -> Value<'a> {
        Value::Null
    }
    /// Unix seconds.
    fn now(&self) -> Option<i64> {
        None
    }
    /// Local time offset in seconds.
    fn tz_offset(&self) -> Option<i64> {
        None
    }
    /// `battery_percent(mV)` using the device's `device.battery` curve.
    fn battery_percent(&self, _mv: f64) -> Option<f64> {
        None
    }
}

/// A host that provides nothing.
pub struct NoHost;
impl Host<'_> for NoHost {}

#[derive(Clone, Copy, Debug)]
enum Slot<'a> {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(&'a str),
    Bytes(&'a [u8]),
    AStr(u16, u8),
    ABytes(u16, u8),
}

/// The interpreter. Reusable; holds only the scratch arena.
pub struct Vm {
    arena: [u8; ARENA_SIZE],
    used: usize,
}

impl Default for Vm {
    fn default() -> Self {
        Self::new()
    }
}

type R<T> = Result<T, EvalError>;

fn fin(f: f64) -> R<Slot<'static>> {
    if f.is_finite() {
        Ok(Slot::Float(f))
    } else {
        Err(EvalError::Overflow)
    }
}

fn scalar_slot(s: &Scalar) -> Slot<'_> {
    match s {
        Scalar::Null => Slot::Null,
        Scalar::Bool(b) => Slot::Bool(*b),
        Scalar::Int(i) => Slot::Int(*i),
        Scalar::Float(f) => Slot::Float(*f),
        Scalar::Str(s) => Slot::Str(s),
    }
}

fn value_slot(v: Value<'_>) -> Slot<'_> {
    match v {
        Value::Null => Slot::Null,
        Value::Bool(b) => Slot::Bool(b),
        Value::Int(i) => Slot::Int(i),
        Value::Float(f) => Slot::Float(f),
        Value::Str(s) => Slot::Str(s),
        Value::Bytes(b) => Slot::Bytes(b),
    }
}

impl Vm {
    pub const fn new() -> Self {
        Vm {
            arena: [0; ARENA_SIZE],
            used: 0,
        }
    }

    fn bytes_of<'s, 'a: 's>(&'s self, s: &Slot<'a>) -> Option<&'s [u8]> {
        match *s {
            Slot::Bytes(b) => Some(b),
            Slot::ABytes(o, n) => Some(&self.arena[o as usize..o as usize + n as usize]),
            _ => None,
        }
    }

    fn str_of<'s, 'a: 's>(&'s self, s: &Slot<'a>) -> Option<&'s str> {
        match *s {
            Slot::Str(x) => Some(x),
            Slot::AStr(o, n) => {
                core::str::from_utf8(&self.arena[o as usize..o as usize + n as usize]).ok()
            }
            _ => None,
        }
    }

    fn alloc(&mut self, data: &[u8]) -> R<(u16, u8)> {
        if data.len() > MAX_STR || self.used + data.len() > ARENA_SIZE {
            return Err(EvalError::TooLong);
        }
        let o = self.used;
        self.arena[o..o + data.len()].copy_from_slice(data);
        self.used += data.len();
        Ok((o as u16, data.len() as u8))
    }

    /// Copies `b` into the arena as a string slot.
    fn arena_str(&mut self, b: &[u8]) -> R<Slot<'static>> {
        if self.used + b.len() > ARENA_SIZE || b.len() > MAX_STR {
            return Err(EvalError::TooLong);
        }
        let start = self.used;
        self.arena[start..start + b.len()].copy_from_slice(b);
        self.used += b.len();
        Ok(Slot::AStr(start as u16, b.len() as u8))
    }

    fn eq(&self, a: &Slot<'_>, b: &Slot<'_>) -> bool {
        match (a, b) {
            (Slot::Null, Slot::Null) => true,
            (Slot::Bool(x), Slot::Bool(y)) => x == y,
            (Slot::Int(x), Slot::Int(y)) => x == y,
            (Slot::Int(x), Slot::Float(y)) | (Slot::Float(y), Slot::Int(x)) => (*x as f64) == *y,
            (Slot::Float(x), Slot::Float(y)) => x == y,
            _ => {
                if let (Some(x), Some(y)) = (self.str_of(a), self.str_of(b)) {
                    x == y
                } else if let (Some(x), Some(y)) = (self.bytes_of(a), self.bytes_of(b)) {
                    x == y
                } else {
                    false
                }
            }
        }
    }

    fn cmp(&self, a: &Slot<'_>, b: &Slot<'_>) -> R<core::cmp::Ordering> {
        use core::cmp::Ordering;
        match (a, b) {
            (Slot::Int(x), Slot::Int(y)) => Ok(x.cmp(y)),
            _ => {
                if let (Some(x), Some(y)) = (num(a), num(b)) {
                    x.partial_cmp(&y).ok_or(EvalError::Type)
                } else if let (Some(x), Some(y)) = (self.str_of(a), self.str_of(b)) {
                    Ok(x.as_bytes().cmp(y.as_bytes()))
                } else {
                    let _ = Ordering::Equal;
                    Err(EvalError::Type)
                }
            }
        }
    }

    /// Evaluates `code` and returns its value. Errors are run-time errors
    /// (the expression's value is then `null`, spec 7.1).
    pub fn eval<'s, 'a: 's>(
        &'s mut self,
        code: &[u8],
        consts: &'a [Const],
        inputs: &Inputs<'a>,
        host: &dyn Host<'a>,
    ) -> Result<Value<'s>, EvalError> {
        self.used = 0;
        let out = self.run(code, consts, inputs, host)?;
        self.resolve(out)
    }

    /// [`Vm::eval`] with errors mapped to `Value::Null`.
    pub fn eval_or_null<'s, 'a: 's>(
        &'s mut self,
        code: &[u8],
        consts: &'a [Const],
        inputs: &Inputs<'a>,
        host: &dyn Host<'a>,
    ) -> Value<'s> {
        self.used = 0;
        match self.run(code, consts, inputs, host) {
            Ok(out) => self.resolve(out).unwrap_or(Value::Null),
            Err(_) => Value::Null,
        }
    }

    fn resolve<'s, 'a: 's>(&'s self, out: Slot<'a>) -> Result<Value<'s>, EvalError> {
        Ok(match out {
            Slot::Null => Value::Null,
            Slot::Bool(b) => Value::Bool(b),
            Slot::Int(i) => Value::Int(i),
            Slot::Float(f) => Value::Float(f),
            Slot::Str(s) => Value::Str(s),
            Slot::Bytes(b) => Value::Bytes(b),
            Slot::AStr(o, n) => Value::Str(
                core::str::from_utf8(&self.arena[o as usize..o as usize + n as usize])
                    .map_err(|_| EvalError::Malformed)?,
            ),
            Slot::ABytes(o, n) => Value::Bytes(&self.arena[o as usize..o as usize + n as usize]),
        })
    }

    fn run<'a>(
        &mut self,
        code: &[u8],
        consts: &'a [Const],
        inputs: &Inputs<'a>,
        host: &dyn Host<'a>,
    ) -> R<Slot<'a>> {
        if code.is_empty() || code.len() > MAX_CODE {
            return Err(EvalError::Malformed);
        }
        let mut st: [Slot<'a>; MAX_STACK] = [Slot::Null; MAX_STACK];
        let mut sp = 0usize;
        let mut locals: [Slot<'a>; MAX_LOCALS] = [Slot::Null; MAX_LOCALS];
        let mut pc = 0usize;
        macro_rules! pop {
            () => {{
                if sp == 0 {
                    return Err(EvalError::StackUnderflow);
                }
                sp -= 1;
                st[sp]
            }};
        }
        macro_rules! push {
            ($v:expr) => {{
                let v = $v;
                if sp >= MAX_STACK {
                    return Err(EvalError::StackOverflow);
                }
                st[sp] = v;
                sp += 1;
            }};
        }
        let konst = |pc: usize| -> R<&'a Const> {
            let (i, _) = read_uleb(code, pc + 1).ok_or(EvalError::Malformed)?;
            consts.get(i as usize).ok_or(EvalError::Malformed)
        };
        while pc < code.len() {
            let o = code[pc];
            let n = insn_len(code, pc).ok_or(EvalError::Malformed)?;
            let imm = code.get(pc + 1).copied().unwrap_or(0);
            match o {
                op::X => push!(value_slot(inputs.x)),
                op::I8 => push!(Slot::Int(imm as i8 as i64)),
                op::INT => {
                    let (v, _) = read_uleb(code, pc + 1).ok_or(EvalError::Malformed)?;
                    push!(Slot::Int(unzigzag(v)))
                }
                op::CONST => push!(match konst(pc)? {
                    Const::Str(s) => Slot::Str(s),
                    Const::Float(f) => Slot::Float(*f),
                    Const::Int(i) => Slot::Int(*i),
                    Const::Map(_) => return Err(EvalError::Malformed),
                }),
                op::NULL => push!(Slot::Null),
                op::TRUE => push!(Slot::Bool(true)),
                op::FALSE => push!(Slot::Bool(false)),
                op::LDL => push!(*locals.get(imm as usize).ok_or(EvalError::Malformed)?),
                op::STL => {
                    let v = pop!();
                    *locals.get_mut(imm as usize).ok_or(EvalError::Malformed)? = v;
                }
                op::STATE | op::OPT => {
                    let Const::Str(k) = konst(pc)? else {
                        return Err(EvalError::Malformed);
                    };
                    push!(value_slot(if o == op::STATE {
                        host.state(k)
                    } else {
                        host.opt(k)
                    }))
                }
                op::PAYLOAD => push!(inputs.payload.map(Slot::Bytes).unwrap_or(Slot::Null)),
                op::LAST => push!(inputs.last.map(Slot::Str).unwrap_or(Slot::Null)),
                op::GPD => {
                    if imm as usize >= GPD_FIELDS.len() {
                        return Err(EvalError::Malformed);
                    }
                    push!(value_slot(host.gpd(imm)))
                }
                op::FP => {
                    if imm as usize >= FP_FIELDS.len() {
                        return Err(EvalError::Malformed);
                    }
                    push!(value_slot(host.fp(imm)))
                }
                op::ADD | op::SUB | op::MUL | op::DIV | op::IDIV | op::MOD => {
                    let b = pop!();
                    let a = pop!();
                    push!(self.arith(o, a, b)?)
                }
                op::NEG => {
                    let a = pop!();
                    push!(match a {
                        Slot::Null => Slot::Null,
                        Slot::Int(i) => Slot::Int(i.checked_neg().ok_or(EvalError::Overflow)?),
                        Slot::Float(f) => Slot::Float(-f),
                        _ => return Err(EvalError::Type),
                    })
                }
                op::AND | op::OR | op::XOR | op::SHL | op::SHR => {
                    let b = pop!();
                    let a = pop!();
                    push!(match (a, b) {
                        (Slot::Null, _) | (_, Slot::Null) => Slot::Null,
                        (Slot::Int(x), Slot::Int(y)) => Slot::Int(match o {
                            op::AND => x & y,
                            op::OR => x | y,
                            op::XOR => x ^ y,
                            _ => {
                                if !(0..=63).contains(&y) {
                                    return Err(EvalError::Range);
                                }
                                if o == op::SHL { x << y } else { x >> y }
                            }
                        }),
                        _ => return Err(EvalError::Type),
                    })
                }
                op::BNOT => {
                    let a = pop!();
                    push!(match a {
                        Slot::Null => Slot::Null,
                        Slot::Int(x) => Slot::Int(!x),
                        _ => return Err(EvalError::Type),
                    })
                }
                op::EQ | op::NE => {
                    let b = pop!();
                    let a = pop!();
                    let e = self.eq(&a, &b);
                    push!(Slot::Bool(if o == op::EQ { e } else { !e }))
                }
                op::LT | op::LE | op::GT | op::GE => {
                    let b = pop!();
                    let a = pop!();
                    if matches!(a, Slot::Null) || matches!(b, Slot::Null) {
                        push!(Slot::Null)
                    } else {
                        let c = self.cmp(&a, &b)?;
                        use core::cmp::Ordering::*;
                        push!(Slot::Bool(match o {
                            op::LT => c == Less,
                            op::LE => c != Greater,
                            op::GT => c == Greater,
                            _ => c != Less,
                        }))
                    }
                }
                op::LNOT => {
                    let a = pop!();
                    push!(Slot::Bool(!truthy(a)?))
                }
                op::LAND | op::LOR => {
                    let b = truthy(pop!())?;
                    let a = truthy(pop!())?;
                    push!(Slot::Bool(if o == op::LAND { a && b } else { a || b }))
                }
                op::JMP | op::JZ | op::JNN => {
                    let target = pc + 2 + imm as usize;
                    if target > code.len() {
                        return Err(EvalError::BadJump);
                    }
                    let jump = match o {
                        op::JMP => true,
                        op::JZ => !truthy(pop!())?,
                        _ => {
                            if sp == 0 {
                                return Err(EvalError::StackUnderflow);
                            }
                            !matches!(st[sp - 1], Slot::Null)
                        }
                    };
                    if jump {
                        pc = target;
                        continue;
                    }
                }
                op::POP => {
                    pop!();
                }
                op::INDEX => {
                    let i = pop!();
                    let b = pop!();
                    push!(match (b, i) {
                        (Slot::Null, _) | (_, Slot::Null) => Slot::Null,
                        (_, Slot::Int(i)) => {
                            let bytes = self.bytes_of(&b).ok_or(EvalError::Type)?;
                            let i = usize::try_from(i).map_err(|_| EvalError::Range)?;
                            Slot::Int(*bytes.get(i).ok_or(EvalError::Range)? as i64)
                        }
                        _ => return Err(EvalError::Type),
                    })
                }
                op::LOOKUP => {
                    let Const::Map(m) = konst(pc)? else {
                        return Err(EvalError::Malformed);
                    };
                    let k = pop!();
                    let mut r = Slot::Null;
                    if !matches!(k, Slot::Null) {
                        for (mk, mv) in m {
                            if self.eq(&scalar_slot(mk), &k) {
                                r = scalar_slot(mv);
                                break;
                            }
                        }
                    }
                    push!(r)
                }
                op::CALL => {
                    let f = imm;
                    let argc = *code.get(pc + 2).ok_or(EvalError::Malformed)? as usize;
                    let def = FUNCS.get(f as usize).ok_or(EvalError::Malformed)?;
                    if argc < def.min_args as usize || argc > def.max_args as usize || argc > sp {
                        return Err(EvalError::Malformed);
                    }
                    let base = sp - argc;
                    let mut args = [Slot::Null; MAX_STACK];
                    args[..argc].copy_from_slice(&st[base..sp]);
                    sp = base;
                    let r = self.call(f, &args[..argc], host)?;
                    push!(r)
                }
                _ => return Err(EvalError::Malformed),
            }
            pc += n;
        }
        if sp != 1 {
            return Err(EvalError::Malformed);
        }
        Ok(st[0])
    }

    fn arith<'a>(&mut self, o: u8, a: Slot<'a>, b: Slot<'a>) -> R<Slot<'a>> {
        if matches!(a, Slot::Null) || matches!(b, Slot::Null) {
            return Ok(Slot::Null);
        }
        if let (Slot::Int(x), Slot::Int(y)) = (a, b) {
            return Ok(Slot::Int(match o {
                op::ADD => x.checked_add(y).ok_or(EvalError::Overflow)?,
                op::SUB => x.checked_sub(y).ok_or(EvalError::Overflow)?,
                op::MUL => x.checked_mul(y).ok_or(EvalError::Overflow)?,
                op::DIV => {
                    if y == 0 {
                        return Err(EvalError::DivisionByZero);
                    }
                    return fin(x as f64 / y as f64);
                }
                op::IDIV => {
                    if y == 0 {
                        return Err(EvalError::DivisionByZero);
                    }
                    let q = x.checked_div(y).ok_or(EvalError::Overflow)?;
                    if (x % y != 0) && ((x < 0) != (y < 0)) {
                        q - 1
                    } else {
                        q
                    }
                }
                _ => {
                    if y == 0 {
                        return Err(EvalError::DivisionByZero);
                    }
                    let r = x.checked_rem(y).ok_or(EvalError::Overflow)?;
                    if r != 0 && ((r < 0) != (y < 0)) {
                        r + y
                    } else {
                        r
                    }
                }
            }));
        }
        if let (Some(x), Some(y)) = (num(&a), num(&b)) {
            return fin(match o {
                op::ADD => x + y,
                op::SUB => x - y,
                op::MUL => x * y,
                op::DIV => {
                    if y == 0.0 {
                        return Err(EvalError::DivisionByZero);
                    }
                    x / y
                }
                op::IDIV => {
                    if y == 0.0 {
                        return Err(EvalError::DivisionByZero);
                    }
                    libm::floor(x / y)
                }
                _ => {
                    if y == 0.0 {
                        return Err(EvalError::DivisionByZero);
                    }
                    x - y * libm::floor(x / y)
                }
            });
        }
        if o == op::ADD {
            if let (Some(x), Some(y)) = (self.str_of(&a), self.str_of(&b)) {
                let total = x.len() + y.len();
                if total > MAX_STR || self.used + total > ARENA_SIZE {
                    return Err(EvalError::TooLong);
                }
                let mut buf = [0u8; MAX_STR];
                buf[..x.len()].copy_from_slice(x.as_bytes());
                buf[x.len()..total].copy_from_slice(y.as_bytes());
                let (o, n) = self.alloc(&buf[..total])?;
                return Ok(Slot::AStr(o, n));
            }
        }
        Err(EvalError::Type)
    }

    fn call<'a>(&mut self, f: u8, a: &[Slot<'a>], host: &dyn Host<'a>) -> R<Slot<'a>> {
        if f == func::ISNULL {
            return Ok(Slot::Bool(matches!(a[0], Slot::Null)));
        }
        if a.iter().any(|s| matches!(s, Slot::Null)) {
            return Ok(Slot::Null);
        }
        let int = |s: &Slot<'_>| match s {
            Slot::Int(i) => Ok(*i),
            _ => Err(EvalError::Type),
        };
        let numf = |s: &Slot<'_>| num(s).ok_or(EvalError::Type);
        let name = FUNCS[f as usize].name;
        match name {
            "bit" => {
                let n = int(&a[1])?;
                if !(0..=63).contains(&n) {
                    return Err(EvalError::Range);
                }
                Ok(Slot::Bool((int(&a[0])? >> n) & 1 == 1))
            }
            "bits" => {
                let (x, lo, len) = (int(&a[0])?, int(&a[1])?, int(&a[2])?);
                if !(0..=63).contains(&lo) || !(1..=64).contains(&len) || lo + len > 64 {
                    return Err(EvalError::Range);
                }
                let v = ((x as u64) >> lo)
                    & if len == 64 {
                        u64::MAX
                    } else {
                        (1u64 << len) - 1
                    };
                Ok(Slot::Int(v as i64))
            }
            "sext" => {
                let (x, bits) = (int(&a[0])?, int(&a[1])?);
                if !(1..=64).contains(&bits) {
                    return Err(EvalError::Range);
                }
                let sh = 64 - bits as u32;
                Ok(Slot::Int((x << sh) >> sh))
            }
            "bcd" => {
                let mut x = int(&a[0])?;
                if x < 0 {
                    return Err(EvalError::Range);
                }
                let (mut out, mut mul) = (0i64, 1i64);
                while x != 0 {
                    let d = x & 0xf;
                    if d > 9 {
                        return Err(EvalError::Range);
                    }
                    out += d * mul;
                    mul = mul.saturating_mul(10);
                    x >>= 4;
                }
                Ok(Slot::Int(out))
            }
            "len" => {
                if let Some(b) = self.bytes_of(&a[0]) {
                    Ok(Slot::Int(b.len() as i64))
                } else {
                    Ok(Slot::Int(
                        self.str_of(&a[0]).ok_or(EvalError::Type)?.len() as i64
                    ))
                }
            }
            "u8" | "i8" | "u16le" | "u16be" | "i16le" | "i16be" | "u24le" | "u24be" | "i24le"
            | "i24be" | "u32le" | "u32be" | "i32le" | "i32be" | "f32le" | "f32be" => {
                let b = self.bytes_of(&a[0]).ok_or(EvalError::Type)?;
                let o = usize::try_from(int(&a[1])?).map_err(|_| EvalError::Range)?;
                let nb = name.as_bytes();
                let width = match nb[1] {
                    b'8' => 1,
                    b'1' => 2,
                    b'2' => 3,
                    _ => 4,
                };
                let be = name.ends_with("be");
                let s = b
                    .get(o..o.checked_add(width).ok_or(EvalError::Range)?)
                    .ok_or(EvalError::Range)?;
                let mut v: u64 = 0;
                for i in 0..width {
                    let byte = if be { s[i] } else { s[width - 1 - i] };
                    v = (v << 8) | byte as u64;
                }
                match nb[0] {
                    b'u' => Ok(Slot::Int(v as i64)),
                    b'i' => {
                        let sh = 64 - 8 * width as u32;
                        Ok(Slot::Int(((v << sh) as i64) >> sh))
                    }
                    _ => fin(f32::from_bits(v as u32) as f64),
                }
            }
            "ascii" => {
                let o = usize::try_from(int(&a[1])?).map_err(|_| EvalError::Range)?;
                let n = usize::try_from(int(&a[2])?).map_err(|_| EvalError::Range)?;
                let b = self.bytes_of(&a[0]).ok_or(EvalError::Type)?;
                let s = b
                    .get(o..o.checked_add(n).ok_or(EvalError::Range)?)
                    .ok_or(EvalError::Range)?;
                if !s.is_ascii() {
                    return Err(EvalError::Range);
                }
                let mut buf = [0u8; MAX_STR];
                if s.len() > MAX_STR {
                    return Err(EvalError::TooLong);
                }
                buf[..s.len()].copy_from_slice(s);
                let (o, n) = self.alloc(&buf[..s.len()])?;
                Ok(Slot::AStr(o, n))
            }
            "hex" => {
                let b = self.bytes_of(&a[0]).ok_or(EvalError::Type)?;
                if b.len() * 2 > MAX_STR {
                    return Err(EvalError::TooLong);
                }
                let mut buf = [0u8; MAX_STR];
                const H: &[u8; 16] = b"0123456789abcdef";
                for (i, x) in b.iter().enumerate() {
                    buf[2 * i] = H[(x >> 4) as usize];
                    buf[2 * i + 1] = H[(x & 15) as usize];
                }
                let n = b.len() * 2;
                let (o, n) = self.alloc(&buf[..n])?;
                Ok(Slot::AStr(o, n))
            }
            "min" | "max" => {
                let mut best = a[0];
                for s in &a[1..] {
                    numf(s)?;
                    let c = self.cmp(s, &best)?;
                    if (name == "min" && c == core::cmp::Ordering::Less)
                        || (name == "max" && c == core::cmp::Ordering::Greater)
                    {
                        best = *s;
                    }
                }
                numf(&best)?;
                Ok(best)
            }
            "clamp" => {
                let (x, lo, hi) = (a[0], a[1], a[2]);
                numf(&x)?;
                numf(&lo)?;
                numf(&hi)?;
                if self.cmp(&x, &lo)? == core::cmp::Ordering::Less {
                    Ok(lo)
                } else if self.cmp(&x, &hi)? == core::cmp::Ordering::Greater {
                    Ok(hi)
                } else {
                    Ok(x)
                }
            }
            "abs" => match a[0] {
                Slot::Int(i) => Ok(Slot::Int(i.checked_abs().ok_or(EvalError::Overflow)?)),
                Slot::Float(x) => Ok(Slot::Float(libm::fabs(x))),
                _ => Err(EvalError::Type),
            },
            "round" => {
                if a.len() == 2 {
                    let d = int(&a[1])?;
                    if !(0..=15).contains(&d) {
                        return Err(EvalError::Range);
                    }
                    let p = libm::pow(10.0, d as f64);
                    return fin(libm::round(numf(&a[0])? * p) / p);
                }
                match a[0] {
                    Slot::Int(i) => Ok(Slot::Int(i)),
                    _ => to_int(libm::round(numf(&a[0])?)),
                }
            }
            "floor" => match a[0] {
                Slot::Int(i) => Ok(Slot::Int(i)),
                _ => to_int(libm::floor(numf(&a[0])?)),
            },
            "ceil" => match a[0] {
                Slot::Int(i) => Ok(Slot::Int(i)),
                _ => to_int(libm::ceil(numf(&a[0])?)),
            },
            "int" => match a[0] {
                Slot::Bool(b) => Ok(Slot::Int(b as i64)),
                Slot::Int(i) => Ok(Slot::Int(i)),
                _ => to_int(libm::trunc(numf(&a[0])?)),
            },
            "float" => match a[0] {
                Slot::Bool(b) => Ok(Slot::Float(b as u8 as f64)),
                _ => fin(numf(&a[0])?),
            },
            "pow10" => fin(libm::pow(10.0, numf(&a[0])?)),
            "log10" => {
                let x = numf(&a[0])?;
                if x <= 0.0 {
                    return Err(EvalError::Range);
                }
                fin(libm::log10(x))
            }
            "str" => match a[0] {
                Slot::Str(_) | Slot::AStr(..) => Ok(a[0]),
                Slot::Bool(b) => Ok(Slot::Str(if b { "true" } else { "false" })),
                Slot::Int(i) => {
                    let mut b = [0u8; 32];
                    self.arena_str(crate::num::format_number(Num::Int(i), &mut b).as_bytes())
                }
                Slot::Float(x) => {
                    let mut b = [0u8; 32];
                    self.arena_str(crate::num::format_number(Num::Float(x), &mut b).as_bytes())
                }
                _ => Err(EvalError::Type),
            },
            "pack" => {
                let fmt = self.str_of(&a[0]).ok_or(EvalError::Type)?;
                let mut buf = [0u8; MAX_STR];
                let mut n = 0usize;
                let mut k = 1usize;
                for item in fmt.split(' ').filter(|s| !s.is_empty()) {
                    let &(_, w, signed, be) = PACK_ITEMS
                        .iter()
                        .find(|p| p.0 == item)
                        .ok_or(EvalError::Malformed)?;
                    let v = int(a.get(k).ok_or(EvalError::Malformed)?)?;
                    k += 1;
                    let bits = 8 * w as u32;
                    let ok = if signed {
                        v >= -(1i64 << (bits - 1)) && v < (1i64 << (bits - 1))
                    } else {
                        v >= 0 && (v as u64) < (1u64 << bits)
                    };
                    if !ok {
                        return Err(EvalError::Range);
                    }
                    if n + w as usize > MAX_STR {
                        return Err(EvalError::TooLong);
                    }
                    for i in 0..w as usize {
                        let sh = if be { 8 * (w as usize - 1 - i) } else { 8 * i };
                        buf[n + i] = (v >> sh) as u8;
                    }
                    n += w as usize;
                }
                if k != a.len() {
                    return Err(EvalError::Malformed);
                }
                let (o, len) = self.alloc(&buf[..n])?;
                Ok(Slot::ABytes(o, len))
            }
            "now" => Ok(Slot::Int(host.now().ok_or(EvalError::Unavailable)?)),
            "tz_offset" => Ok(Slot::Int(host.tz_offset().ok_or(EvalError::Unavailable)?)),
            "battery_percent" => fin(host
                .battery_percent(numf(&a[0])?)
                .ok_or(EvalError::Unavailable)?),
            _ => Err(EvalError::Malformed),
        }
    }
}

fn num(s: &Slot<'_>) -> Option<f64> {
    match *s {
        Slot::Int(i) => Some(i as f64),
        Slot::Float(f) => Some(f),
        _ => None,
    }
}

fn to_int(f: f64) -> R<Slot<'static>> {
    if !(-9.223_372_036_854_776e18..9.223_372_036_854_776e18).contains(&f) {
        return Err(EvalError::Overflow);
    }
    Ok(Slot::Int(f as i64))
}

fn truthy(s: Slot<'_>) -> R<bool> {
    match s {
        Slot::Bool(b) => Ok(b),
        Slot::Null => Ok(false),
        _ => Err(EvalError::Type),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;
    use alloc::vec::Vec;

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect()
    }

    #[test]
    fn spec_examples_run() {
        let mut vm = Vm::new();
        let c = hex("01 03 d0 f7 0f 34 3d 04 02 00 3c 06 01 03 80 80 08 11");
        assert_eq!(
            vm.eval(&c, &[], &Inputs::x(Value::Int(65636)), &NoHost)
                .unwrap(),
            Value::Int(100)
        );
        assert_eq!(
            vm.eval(&c, &[], &Inputs::x(Value::Int(200000)), &NoHost)
                .unwrap(),
            Value::Int(0)
        );
        let flower =
            hex("01 02 00 30 3d 04 02 00 3c 0f 01 02 01 11 03 a0 9c 01 13 70 20 01 70 1b 01");
        assert_eq!(
            vm.eval(&flower, &[], &Inputs::x(Value::Int(0)), &NoHost)
                .unwrap(),
            Value::Int(0)
        );
        assert_eq!(
            vm.eval(&flower, &[], &Inputs::x(Value::Int(20001)), &NoHost)
                .unwrap(),
            Value::Int(100)
        );
        let consts = vec![Const::Str("detection_interval".to_string())];
        let c = hex("0a 00 3e 03 3f 02 1e 02 02 10");
        assert_eq!(
            vm.eval(&c, &consts, &Inputs::NONE, &NoHost).unwrap(),
            Value::Int(32)
        );
        struct H;
        impl<'a> Host<'a> for H {
            fn state(&self, _k: &str) -> Value<'a> {
                Value::Int(60)
            }
        }
        assert_eq!(
            vm.eval(&c, &consts, &Inputs::NONE, &H).unwrap(),
            Value::Int(62)
        );
        let c = hex("0c 02 02 70 07 02");
        assert_eq!(
            vm.eval(&c, &[], &Inputs::payload(&[0, 0, 0x34, 0x12]), &NoHost)
                .unwrap(),
            Value::Int(0x1234)
        );
        assert_eq!(
            vm.eval(&c, &[], &Inputs::payload(&[0, 0, 0x34]), &NoHost),
            Err(EvalError::Range)
        );
    }

    #[test]
    fn strings_and_errors() {
        let mut vm = Vm::new();
        let consts = vec![Const::Str("recall_".to_string())];
        // "recall_" + str(payload[2])
        let c = hex("04 00 0c 02 02 40 70 22 01 10");
        assert_eq!(
            vm.eval(&c, &consts, &Inputs::payload(&[0, 0, 7]), &NoHost)
                .unwrap(),
            Value::Str("recall_7")
        );
        // 1 / 0
        assert_eq!(
            vm.eval(&hex("02 01 02 00 13"), &[], &Inputs::NONE, &NoHost),
            Err(EvalError::DivisionByZero)
        );
        // -7 // 2 == -4, -7 % 2 == 1
        assert_eq!(
            vm.eval(&hex("02 f9 02 02 14"), &[], &Inputs::NONE, &NoHost)
                .unwrap(),
            Value::Int(-4)
        );
        assert_eq!(
            vm.eval(&hex("02 f9 02 02 15"), &[], &Inputs::NONE, &NoHost)
                .unwrap(),
            Value::Int(1)
        );
        // backward jump
        assert_eq!(
            vm.eval(&hex("06 3c fe"), &[], &Inputs::NONE, &NoHost),
            Err(EvalError::BadJump)
        );
        // overflow
        let max = {
            let mut v = vec![op::INT];
            super::super::write_uleb(&mut v, super::super::zigzag(i64::MAX));
            v.extend_from_slice(&[op::I8, 1, op::ADD]);
            v
        };
        assert_eq!(
            vm.eval(&max, &[], &Inputs::NONE, &NoHost),
            Err(EvalError::Overflow)
        );
        // pack("u16le u8", 0x1234, 5)
        let consts = vec![Const::Str("u16le u8".to_string())];
        let c = hex("04 00 03 e8 48 02 05 70 24 03");
        assert_eq!(
            vm.eval(&c, &consts, &Inputs::NONE, &NoHost).unwrap(),
            Value::Bytes(&[0x34, 0x12, 5])
        );
    }
}
