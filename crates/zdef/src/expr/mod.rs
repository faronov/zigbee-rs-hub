//! Expression language (spec 7): bytecode definitions, static types,
//! verifier and interpreter.
//!
//! The bytecode is a stack-machine program of at most [`MAX_CODE`] bytes
//! with forward-only jumps, so a program executes at most `len` instructions.
//! [`verify`] is run on every definition load; [`Vm::eval`] additionally
//! bounds-checks everything, so even unverified input cannot panic or loop.

mod verify;
mod vm;

pub use verify::{VerifyError, VerifyErrorKind, verify, verify_with};
pub use vm::{ARENA_SIZE, EvalError, Host, Inputs, NoHost, Value, Vm};

/// Longest program (bytes).
pub const MAX_CODE: usize = 255;
/// Value-stack depth.
pub const MAX_STACK: usize = 16;
/// `let` locals per expression.
pub const MAX_LOCALS: usize = 8;
/// Longest string or byte string built at run time (`+`, `str`, `pack`, `ascii`, `hex`).
pub const MAX_STR: usize = 64;

/// Opcodes (spec 7.5).
pub mod op {
    pub const X: u8 = 0x01;
    pub const I8: u8 = 0x02;
    pub const INT: u8 = 0x03;
    pub const CONST: u8 = 0x04;
    pub const NULL: u8 = 0x05;
    pub const TRUE: u8 = 0x06;
    pub const FALSE: u8 = 0x07;
    pub const LDL: u8 = 0x08;
    pub const STL: u8 = 0x09;
    pub const STATE: u8 = 0x0A;
    pub const OPT: u8 = 0x0B;
    pub const PAYLOAD: u8 = 0x0C;
    pub const LAST: u8 = 0x0D;
    pub const GPD: u8 = 0x0E;
    pub const FP: u8 = 0x0F;
    pub const ADD: u8 = 0x10;
    pub const SUB: u8 = 0x11;
    pub const MUL: u8 = 0x12;
    pub const DIV: u8 = 0x13;
    pub const IDIV: u8 = 0x14;
    pub const MOD: u8 = 0x15;
    pub const NEG: u8 = 0x16;
    pub const AND: u8 = 0x20;
    pub const OR: u8 = 0x21;
    pub const XOR: u8 = 0x22;
    pub const BNOT: u8 = 0x23;
    pub const SHL: u8 = 0x24;
    pub const SHR: u8 = 0x25;
    pub const EQ: u8 = 0x30;
    pub const NE: u8 = 0x31;
    pub const LT: u8 = 0x32;
    pub const LE: u8 = 0x33;
    pub const GT: u8 = 0x34;
    pub const GE: u8 = 0x35;
    pub const LNOT: u8 = 0x36;
    pub const LAND: u8 = 0x37;
    pub const LOR: u8 = 0x38;
    pub const JMP: u8 = 0x3C;
    pub const JZ: u8 = 0x3D;
    pub const JNN: u8 = 0x3E;
    pub const POP: u8 = 0x3F;
    pub const INDEX: u8 = 0x40;
    pub const LOOKUP: u8 = 0x48;
    pub const CALL: u8 = 0x70;

    /// Mnemonic of an opcode (disassembly, error messages).
    pub fn name(op: u8) -> Option<&'static str> {
        Some(match op {
            X => "X",
            I8 => "I8",
            INT => "INT",
            CONST => "CONST",
            NULL => "NULL",
            TRUE => "TRUE",
            FALSE => "FALSE",
            LDL => "LDL",
            STL => "STL",
            STATE => "STATE",
            OPT => "OPT",
            PAYLOAD => "PAYLOAD",
            LAST => "LAST",
            GPD => "GPD",
            FP => "FP",
            ADD => "ADD",
            SUB => "SUB",
            MUL => "MUL",
            DIV => "DIV",
            IDIV => "IDIV",
            MOD => "MOD",
            NEG => "NEG",
            AND => "AND",
            OR => "OR",
            XOR => "XOR",
            BNOT => "BNOT",
            SHL => "SHL",
            SHR => "SHR",
            EQ => "EQ",
            NE => "NE",
            LT => "LT",
            LE => "LE",
            GT => "GT",
            GE => "GE",
            LNOT => "LNOT",
            LAND => "LAND",
            LOR => "LOR",
            JMP => "JMP",
            JZ => "JZ",
            JNN => "JNN",
            POP => "POP",
            INDEX => "INDEX",
            LOOKUP => "LOOKUP",
            CALL => "CALL",
            _ => return None,
        })
    }
}

/// Static type set (bit set over the run-time types).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Ty(pub u8);

impl Ty {
    pub const EMPTY: Ty = Ty(0);
    pub const NULL: Ty = Ty(1);
    pub const BOOL: Ty = Ty(2);
    pub const INT: Ty = Ty(4);
    pub const FLOAT: Ty = Ty(8);
    pub const STR: Ty = Ty(16);
    pub const BYTES: Ty = Ty(32);
    pub const NUM: Ty = Ty(4 | 8);
    pub const ANY: Ty = Ty(63);

    pub const fn or(self, o: Ty) -> Ty {
        Ty(self.0 | o.0)
    }
    pub const fn and(self, o: Ty) -> Ty {
        Ty(self.0 & o.0)
    }
    pub const fn without_null(self) -> Ty {
        Ty(self.0 & !1)
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
    /// True if some non-null member of `self` is accepted by `accepted`, or `self` is only null.
    pub fn compatible(self, accepted: Ty) -> bool {
        let v = self.without_null();
        v.is_empty() || !v.and(accepted).is_empty()
    }
}

/// Where an expression is used; decides the available variables (spec 7.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Site {
    From,
    To,
    ResetAfter,
    PresentIf,
    When,
    Event,
    Attr,
    PressPayload,
}

impl Site {
    pub fn name(self) -> &'static str {
        match self {
            Site::From => "from",
            Site::To => "to",
            Site::ResetAfter => "reset_after",
            Site::PresentIf => "present_if",
            Site::When => "when",
            Site::Event => "event",
            Site::Attr => "attrs",
            Site::PressPayload => "payload",
        }
    }
}

/// Static context of one expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Context {
    pub site: Site,
    /// Type set of `x` (EMPTY: `x` not available).
    pub x: Ty,
    /// `payload` available.
    pub payload: bool,
    /// `gpd.*` available (GP definitions; always in `present_if`).
    pub gpd: bool,
}

impl Context {
    /// Defaults for a site: `x` of any type in `from`/`to`, `payload` in
    /// event and press contexts, `gpd.*` in `present_if`.
    pub fn new(site: Site) -> Self {
        let x = if matches!(site, Site::From | Site::To) {
            Ty::ANY
        } else {
            Ty::EMPTY
        };
        let payload = matches!(
            site,
            Site::When | Site::Event | Site::Attr | Site::PressPayload
        );
        Context {
            site,
            x,
            payload,
            gpd: site == Site::PresentIf,
        }
    }

    pub fn with_x(mut self, x: Ty) -> Self {
        self.x = x;
        self
    }
    pub fn with_payload(mut self, p: bool) -> Self {
        self.payload = p;
        self
    }
    pub fn with_gpd(mut self, g: bool) -> Self {
        self.gpd = g || self.site == Site::PresentIf;
        self
    }

    pub fn allows_state(&self) -> bool {
        self.site != Site::PresentIf
    }
    pub fn allows_last(&self) -> bool {
        matches!(self.site, Site::When | Site::Event)
    }
    pub fn allows_fp(&self) -> bool {
        self.site == Site::PresentIf
    }
    pub fn allows_time(&self) -> bool {
        matches!(self.site, Site::To | Site::PressPayload)
    }
}

/// `gpd.<field>` (immediate of `GPD`).
pub const GPD_FIELDS: &[&str] = &["device_id", "manufacturer_id", "model_id", "src_id"];

/// `fp.<field>` (immediate of `FP`) and its static type.
pub const FP_FIELDS: &[(&str, Ty)] = &[
    ("app_version", Ty::INT),
    ("hw_version", Ty::INT),
    ("mfr_code", Ty::INT),
    ("sw_build", Ty::STR),
    ("stack_version", Ty::INT),
    ("zcl_version", Ty::INT),
    ("date_code", Ty::STR),
];

/// Built-in function (immediate `f` of `CALL f n`).
#[derive(Clone, Copy, Debug)]
pub struct Func {
    pub name: &'static str,
    pub min_args: u8,
    pub max_args: u8,
    /// Accepted (non-null) type of each argument; the last entry repeats.
    pub args: &'static [Ty],
    pub ret: Ty,
}

const B: Ty = Ty::BYTES;
const I: Ty = Ty::INT;
const N: Ty = Ty::NUM;

macro_rules! f {
    ($n:literal, $min:literal, $max:literal, [$($a:expr),*], $r:expr) => {
        Func { name: $n, min_args: $min, max_args: $max, args: &[$($a),*], ret: $r }
    };
}

/// Function table, indexed by the `CALL` immediate (prototype order; append-only).
pub const FUNCS: &[Func] = &[
    f!("bit", 2, 2, [I, I], Ty::BOOL),
    f!("bits", 3, 3, [I, I, I], Ty::INT),
    f!("sext", 2, 2, [I, I], Ty::INT),
    f!("bcd", 1, 1, [I], Ty::INT),
    f!("len", 1, 1, [Ty(16 | 32)], Ty::INT),
    f!("u8", 2, 2, [B, I], Ty::INT),
    f!("i8", 2, 2, [B, I], Ty::INT),
    f!("u16le", 2, 2, [B, I], Ty::INT),
    f!("u16be", 2, 2, [B, I], Ty::INT),
    f!("i16le", 2, 2, [B, I], Ty::INT),
    f!("i16be", 2, 2, [B, I], Ty::INT),
    f!("u24le", 2, 2, [B, I], Ty::INT),
    f!("u24be", 2, 2, [B, I], Ty::INT),
    f!("i24le", 2, 2, [B, I], Ty::INT),
    f!("i24be", 2, 2, [B, I], Ty::INT),
    f!("u32le", 2, 2, [B, I], Ty::INT),
    f!("u32be", 2, 2, [B, I], Ty::INT),
    f!("i32le", 2, 2, [B, I], Ty::INT),
    f!("i32be", 2, 2, [B, I], Ty::INT),
    f!("f32le", 2, 2, [B, I], Ty::FLOAT),
    f!("f32be", 2, 2, [B, I], Ty::FLOAT),
    f!("ascii", 3, 3, [B, I, I], Ty::STR),
    f!("hex", 1, 1, [B], Ty::STR),
    f!("min", 2, 8, [N], Ty::NUM),
    f!("max", 2, 8, [N], Ty::NUM),
    f!("clamp", 3, 3, [N], Ty::NUM),
    f!("abs", 1, 1, [N], Ty::NUM),
    f!("round", 1, 2, [N, I], Ty::NUM),
    f!("floor", 1, 1, [N], Ty::INT),
    f!("ceil", 1, 1, [N], Ty::INT),
    f!("int", 1, 1, [Ty(2 | 4 | 8)], Ty::INT),
    f!("float", 1, 1, [Ty(2 | 4 | 8)], Ty::FLOAT),
    f!("pow10", 1, 1, [N], Ty::FLOAT),
    f!("log10", 1, 1, [N], Ty::FLOAT),
    f!("str", 1, 1, [Ty(2 | 4 | 8 | 16)], Ty::STR),
    f!("isnull", 1, 1, [Ty::ANY], Ty::BOOL),
    f!("pack", 1, 15, [Ty::STR, N], Ty::BYTES),
    f!("now", 0, 0, [], Ty::INT),
    f!("tz_offset", 0, 0, [], Ty::INT),
    f!("battery_percent", 1, 1, [N], Ty::NUM),
];

pub mod func {
    pub const BIT: u8 = 0;
    pub const ISNULL: u8 = 35;
    pub const PACK: u8 = 36;
    pub const NOW: u8 = 37;
    pub const TZ_OFFSET: u8 = 38;
    pub const BATTERY_PERCENT: u8 = 39;
}

pub fn func_index(name: &str) -> Option<u8> {
    FUNCS.iter().position(|f| f.name == name).map(|i| i as u8)
}

/// `pack` format items (spec 7.4): name, width, signed, big-endian.
pub const PACK_ITEMS: &[(&str, u8, bool, bool)] = &[
    ("u8", 1, false, false),
    ("i8", 1, true, false),
    ("u16le", 2, false, false),
    ("u16be", 2, false, true),
    ("i16le", 2, true, false),
    ("i16be", 2, true, true),
    ("u24le", 3, false, false),
    ("u32le", 4, false, false),
    ("u32be", 4, false, true),
    ("i32le", 4, true, false),
    ("i32be", 4, true, true),
];

/// Reads an unsigned LEB128 immediate. Returns (value, bytes used).
pub fn read_uleb(code: &[u8], at: usize) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    let mut shift = 0u32;
    let mut i = at;
    loop {
        let b = *code.get(i)?;
        i += 1;
        if shift == 63 && b > 1 {
            return None;
        }
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            // reject non-minimal encodings (trailing zero groups)
            if b == 0 && i - at > 1 {
                return None;
            }
            return Some((v, i - at));
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

pub fn write_uleb(out: &mut alloc::vec::Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            out.push(b | 0x80);
        } else {
            out.push(b);
            return;
        }
    }
}

pub fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

pub fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// Length of the instruction at `pc` (opcode + immediates), `None` if malformed.
pub fn insn_len(code: &[u8], pc: usize) -> Option<usize> {
    let o = *code.get(pc)?;
    Some(match o {
        op::I8 | op::LDL | op::STL | op::GPD | op::FP | op::JMP | op::JZ | op::JNN => {
            code.get(pc + 1)?;
            2
        }
        op::INT | op::CONST | op::STATE | op::OPT | op::LOOKUP => 1 + read_uleb(code, pc + 1)?.1,
        op::CALL => {
            code.get(pc + 2)?;
            3
        }
        _ => {
            op::name(o)?;
            1
        }
    })
}
