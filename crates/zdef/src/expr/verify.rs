//! Bytecode verifier (spec 7.5).

use super::{
    Context, FP_FIELDS, FUNCS, GPD_FIELDS, MAX_CODE, MAX_LOCALS, MAX_STACK, Ty, insn_len, op,
    read_uleb, unzigzag,
};
use crate::model::{Const, Scalar};
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerifyError {
    /// Byte offset of the offending instruction.
    pub offset: usize,
    pub kind: VerifyErrorKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyErrorKind {
    Empty,
    TooLong,
    UnknownOpcode(u8),
    BadImmediate,
    /// Backward jump, jump outside the program or into an instruction.
    BadJump,
    StackOverflow,
    StackUnderflow,
    /// Different stack depths where control flow merges.
    StackMismatch,
    /// Code no path reaches.
    Unreachable,
    /// The program does not leave exactly one value.
    FinalDepth,
    /// Constant index out of range or of the wrong kind for the opcode.
    BadConst,
    Arity,
    /// An operand can never have a type the operation accepts.
    Type(u8),
    /// The variable or function is not available at this site (spec 7.3, 7.4).
    NotAllowed(u8),
    UndefinedLocal,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct State {
    depth: u8,
    stack: [Ty; MAX_STACK],
    defined: u8,
    locals: [Ty; MAX_LOCALS],
}

impl State {
    fn merge(&mut self, o: &State) -> bool {
        if self.depth != o.depth {
            return false;
        }
        for i in 0..self.depth as usize {
            self.stack[i] = self.stack[i].or(o.stack[i]);
        }
        self.defined &= o.defined;
        for i in 0..MAX_LOCALS {
            self.locals[i] = self.locals[i].or(o.locals[i]);
        }
        true
    }
}

fn scalar_ty(s: &Scalar) -> Ty {
    match s {
        Scalar::Null => Ty::NULL,
        Scalar::Bool(_) => Ty::BOOL,
        Scalar::Int(_) => Ty::INT,
        Scalar::Float(_) => Ty::FLOAT,
        Scalar::Str(_) => Ty::STR,
    }
}

fn arith(a: Ty, b: Ty) -> Ty {
    let mut out = Ty::NULL;
    if !a.and(Ty::NUM).is_empty() && !b.and(Ty::NUM).is_empty() {
        if !a.and(Ty::INT).is_empty() && !b.and(Ty::INT).is_empty() {
            out = out.or(Ty::INT);
        }
        if !a.and(Ty::FLOAT).is_empty() || !b.and(Ty::FLOAT).is_empty() {
            out = out.or(Ty::FLOAT);
        }
    }
    out
}

/// Verifies `code` against the constant pool and the site context. Returns
/// the static type set of the result.
pub fn verify(code: &[u8], consts: &[Const], ctx: &Context) -> Result<Ty, VerifyError> {
    let e = |offset, kind| VerifyError { offset, kind };
    if code.is_empty() {
        return Err(e(0, VerifyErrorKind::Empty));
    }
    if code.len() > MAX_CODE {
        return Err(e(MAX_CODE, VerifyErrorKind::TooLong));
    }
    // Instruction boundaries.
    let mut starts = [0u64; 4]; // bit set over 256 offsets
    let mut pc = 0;
    while pc < code.len() {
        starts[pc / 64] |= 1 << (pc % 64);
        let n = insn_len(code, pc).ok_or_else(|| {
            if op::name(code[pc]).is_none() {
                e(pc, VerifyErrorKind::UnknownOpcode(code[pc]))
            } else {
                e(pc, VerifyErrorKind::BadImmediate)
            }
        })?;
        pc += n;
    }
    let is_start =
        |t: usize| t == code.len() || (t < code.len() && starts[t / 64] & (1 << (t % 64)) != 0);

    let mut pending: Vec<(usize, State)> = Vec::new();
    let mut cur: Option<State> = Some(State {
        depth: 0,
        stack: [Ty::EMPTY; MAX_STACK],
        defined: 0,
        locals: [Ty::EMPTY; MAX_LOCALS],
    });
    let mut pc = 0;
    loop {
        // Merge jump states arriving here.
        let mut i = 0;
        while i < pending.len() {
            if pending[i].0 == pc {
                let (_, s) = pending.swap_remove(i);
                match &mut cur {
                    Some(c) => {
                        if !c.merge(&s) {
                            return Err(e(pc, VerifyErrorKind::StackMismatch));
                        }
                    }
                    None => cur = Some(s),
                }
            } else {
                i += 1;
            }
        }
        let Some(mut s) = cur else {
            return Err(e(pc, VerifyErrorKind::Unreachable));
        };
        if pc == code.len() {
            if s.depth != 1 {
                return Err(e(pc, VerifyErrorKind::FinalDepth));
            }
            return Ok(s.stack[0]);
        }
        let o = code[pc];
        let n = insn_len(code, pc).unwrap_or(1);
        macro_rules! pop {
            () => {{
                if s.depth == 0 {
                    return Err(e(pc, VerifyErrorKind::StackUnderflow));
                }
                s.depth -= 1;
                s.stack[s.depth as usize]
            }};
        }
        macro_rules! push {
            ($t:expr) => {{
                if s.depth as usize >= MAX_STACK {
                    return Err(e(pc, VerifyErrorKind::StackOverflow));
                }
                s.stack[s.depth as usize] = $t;
                s.depth += 1;
            }};
        }
        macro_rules! need {
            ($t:expr, $acc:expr) => {
                if !$t.compatible($acc) {
                    return Err(e(pc, VerifyErrorKind::Type(o)));
                }
            };
        }
        let const_at = |pc: usize| -> Result<&Const, VerifyError> {
            let (i, _) = read_uleb(code, pc + 1).ok_or(e(pc, VerifyErrorKind::BadImmediate))?;
            consts
                .get(i as usize)
                .ok_or(e(pc, VerifyErrorKind::BadConst))
        };
        let mut next: Option<State> = None;
        match o {
            op::X => {
                if ctx.x.is_empty() {
                    return Err(e(pc, VerifyErrorKind::NotAllowed(o)));
                }
                push!(ctx.x)
            }
            op::I8 => push!(Ty::INT),
            op::INT => {
                let (v, _) = read_uleb(code, pc + 1).ok_or(e(pc, VerifyErrorKind::BadImmediate))?;
                let _ = unzigzag(v);
                push!(Ty::INT)
            }
            op::CONST => {
                let t = match const_at(pc)? {
                    Const::Str(_) => Ty::STR,
                    Const::Float(_) => Ty::FLOAT,
                    Const::Int(_) => Ty::INT,
                    Const::Map(_) => return Err(e(pc, VerifyErrorKind::BadConst)),
                };
                push!(t)
            }
            op::NULL => push!(Ty::NULL),
            op::TRUE | op::FALSE => push!(Ty::BOOL),
            op::LDL => {
                let l = code[pc + 1] as usize;
                if l >= MAX_LOCALS {
                    return Err(e(pc, VerifyErrorKind::BadImmediate));
                }
                if s.defined & (1 << l) == 0 {
                    return Err(e(pc, VerifyErrorKind::UndefinedLocal));
                }
                push!(s.locals[l])
            }
            op::STL => {
                let l = code[pc + 1] as usize;
                if l >= MAX_LOCALS {
                    return Err(e(pc, VerifyErrorKind::BadImmediate));
                }
                let t = pop!();
                s.defined |= 1 << l;
                s.locals[l] = t;
            }
            op::STATE | op::OPT => {
                if !ctx.allows_state() {
                    return Err(e(pc, VerifyErrorKind::NotAllowed(o)));
                }
                if !matches!(const_at(pc)?, Const::Str(_)) {
                    return Err(e(pc, VerifyErrorKind::BadConst));
                }
                push!(Ty::ANY)
            }
            op::PAYLOAD => {
                if !ctx.payload {
                    return Err(e(pc, VerifyErrorKind::NotAllowed(o)));
                }
                push!(Ty::BYTES.or(Ty::NULL))
            }
            op::LAST => {
                if !ctx.allows_last() {
                    return Err(e(pc, VerifyErrorKind::NotAllowed(o)));
                }
                push!(Ty::STR.or(Ty::NULL))
            }
            op::GPD => {
                if !ctx.gpd {
                    return Err(e(pc, VerifyErrorKind::NotAllowed(o)));
                }
                if code[pc + 1] as usize >= GPD_FIELDS.len() {
                    return Err(e(pc, VerifyErrorKind::BadImmediate));
                }
                push!(Ty::INT.or(Ty::NULL))
            }
            op::FP => {
                if !ctx.allows_fp() {
                    return Err(e(pc, VerifyErrorKind::NotAllowed(o)));
                }
                let f = FP_FIELDS
                    .get(code[pc + 1] as usize)
                    .ok_or(e(pc, VerifyErrorKind::BadImmediate))?;
                push!(f.1.or(Ty::NULL))
            }
            op::ADD => {
                let b = pop!();
                let a = pop!();
                need!(a, Ty::NUM.or(Ty::STR));
                need!(b, Ty::NUM.or(Ty::STR));
                let mut out = arith(a, b);
                if !a.and(Ty::STR).is_empty() && !b.and(Ty::STR).is_empty() {
                    out = out.or(Ty::STR);
                }
                if out == Ty::NULL && !(a.without_null().is_empty() || b.without_null().is_empty())
                {
                    return Err(e(pc, VerifyErrorKind::Type(o)));
                }
                push!(out)
            }
            op::SUB | op::MUL | op::IDIV | op::MOD => {
                let b = pop!();
                let a = pop!();
                need!(a, Ty::NUM);
                need!(b, Ty::NUM);
                push!(arith(a, b))
            }
            op::DIV => {
                let b = pop!();
                let a = pop!();
                need!(a, Ty::NUM);
                need!(b, Ty::NUM);
                push!(Ty::FLOAT.or(Ty::NULL))
            }
            op::NEG => {
                let a = pop!();
                need!(a, Ty::NUM);
                push!(a.and(Ty::NUM).or(Ty::NULL))
            }
            op::AND | op::OR | op::XOR | op::SHL | op::SHR => {
                let b = pop!();
                let a = pop!();
                need!(a, Ty::INT);
                need!(b, Ty::INT);
                push!(Ty::INT.or(Ty::NULL))
            }
            op::BNOT => {
                let a = pop!();
                need!(a, Ty::INT);
                push!(Ty::INT.or(Ty::NULL))
            }
            op::EQ | op::NE => {
                pop!();
                pop!();
                push!(Ty::BOOL)
            }
            op::LT | op::LE | op::GT | op::GE => {
                let b = pop!();
                let a = pop!();
                need!(a, Ty::NUM.or(Ty::STR));
                need!(b, Ty::NUM.or(Ty::STR));
                push!(Ty::BOOL.or(Ty::NULL))
            }
            op::LNOT => {
                let a = pop!();
                need!(a, Ty::BOOL);
                push!(Ty::BOOL.or(Ty::NULL))
            }
            op::LAND | op::LOR => {
                let b = pop!();
                let a = pop!();
                need!(a, Ty::BOOL);
                need!(b, Ty::BOOL);
                push!(Ty::BOOL.or(Ty::NULL))
            }
            op::JMP | op::JZ | op::JNN => {
                let target = pc + 2 + code[pc + 1] as usize;
                if target > code.len() || !is_start(target) {
                    return Err(e(pc, VerifyErrorKind::BadJump));
                }
                match o {
                    op::JMP => {
                        pending.push((target, s));
                        cur = None;
                        pc += n;
                        continue;
                    }
                    op::JZ => {
                        let c = pop!();
                        need!(c, Ty::BOOL);
                        pending.push((target, s));
                    }
                    _ => {
                        let t = pop!();
                        let mut taken = s;
                        taken.stack[taken.depth as usize] = t.without_null();
                        taken.depth += 1;
                        pending.push((target, taken));
                        push!(Ty::NULL);
                    }
                }
            }
            op::POP => {
                pop!();
            }
            op::INDEX => {
                let i = pop!();
                let b = pop!();
                need!(b, Ty::BYTES);
                need!(i, Ty::INT);
                push!(Ty::INT.or(Ty::NULL))
            }
            op::LOOKUP => {
                let Const::Map(m) = const_at(pc)? else {
                    return Err(e(pc, VerifyErrorKind::BadConst));
                };
                pop!();
                let mut t = Ty::NULL;
                for (_, v) in m {
                    t = t.or(scalar_ty(v));
                }
                push!(t)
            }
            op::CALL => {
                let fi = code[pc + 1];
                let argc = code[pc + 2];
                let f = FUNCS
                    .get(fi as usize)
                    .ok_or(e(pc, VerifyErrorKind::BadImmediate))?;
                if argc < f.min_args || argc > f.max_args {
                    return Err(e(pc, VerifyErrorKind::Arity));
                }
                if (fi == super::func::NOW || fi == super::func::TZ_OFFSET) && !ctx.allows_time() {
                    return Err(e(pc, VerifyErrorKind::NotAllowed(o)));
                }
                if fi == super::func::BATTERY_PERCENT && !ctx.allows_state() {
                    return Err(e(pc, VerifyErrorKind::NotAllowed(o)));
                }
                if (s.depth as usize) < argc as usize {
                    return Err(e(pc, VerifyErrorKind::StackUnderflow));
                }
                let base = s.depth as usize - argc as usize;
                for k in 0..argc as usize {
                    let acc = f.args[k.min(f.args.len().saturating_sub(1))];
                    need!(s.stack[base + k], acc);
                }
                s.depth = base as u8;
                let ret = if fi == super::func::ISNULL {
                    Ty::BOOL
                } else {
                    f.ret.or(Ty::NULL)
                };
                push!(ret)
            }
            _ => return Err(e(pc, VerifyErrorKind::UnknownOpcode(o))),
        }
        if next.is_none() {
            next = Some(s);
        }
        cur = next;
        pc += n;
    }
}

/// Alias kept for API symmetry with [`super::Vm::eval`].
pub fn verify_with(code: &[u8], consts: &[Const], ctx: &Context) -> Result<Ty, VerifyError> {
    verify(code, consts, ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::Site;
    use alloc::string::ToString;
    use alloc::vec;

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect()
    }

    #[test]
    fn spec_examples_verify() {
        let from = Context::new(Site::From).with_x(Ty::INT);
        assert_eq!(verify(&hex("01 02 01 30"), &[], &from).unwrap(), Ty::BOOL);
        assert!(
            verify(
                &hex("01 03 d0 f7 0f 34 3d 04 02 00 3c 06 01 03 80 80 08 11"),
                &[],
                &from
            )
            .is_ok()
        );
        let consts = vec![Const::Str("detection_interval".to_string())];
        assert!(
            verify(
                &hex("0a 00 3e 03 3f 02 1e 02 02 10"),
                &consts,
                &Context::new(Site::ResetAfter)
            )
            .is_ok()
        );
        let consts = vec![
            Const::Str("p".into()),
            Const::Str("q".into()),
            Const::Str("r".into()),
            Const::Str("s".into()),
            Const::Str("press".into()),
        ];
        assert!(
            verify(
                &hex("0c 02 00 40 02 00 70 00 02 0d 04 04 31 37"),
                &consts,
                &Context::new(Site::When)
            )
            .is_ok()
        );
        assert!(
            verify(
                &hex("01 02 00 30 3d 04 02 00 3c 0f 01 02 01 11 03 a0 9c 01 13 70 20 01 70 1b 01"),
                &[],
                &from
            )
            .is_ok()
        );
    }

    #[test]
    fn rejects() {
        let from = Context::new(Site::From).with_x(Ty::INT);
        let k = |c: &str| verify(&hex(c), &[], &from).unwrap_err().kind;
        assert_eq!(k("01 01"), VerifyErrorKind::FinalDepth);
        assert_eq!(k("10"), VerifyErrorKind::StackUnderflow);
        assert_eq!(k("01 3d 05 01"), VerifyErrorKind::BadJump);
        assert_eq!(k("ff"), VerifyErrorKind::UnknownOpcode(0xff));
        assert_eq!(k("0d"), VerifyErrorKind::NotAllowed(op::LAST));
        assert_eq!(k("06 02 01 10"), VerifyErrorKind::Type(op::ADD));
        assert_eq!(k("06 3d 01 01 01"), VerifyErrorKind::StackMismatch);
        assert_eq!(k("01 3c 02 01 01"), VerifyErrorKind::Unreachable);
        assert_eq!(k("08 00"), VerifyErrorKind::UndefinedLocal);
        let deep = vec![op::X; 17];
        assert_eq!(
            verify(&deep, &[], &from).unwrap_err().kind,
            VerifyErrorKind::StackOverflow
        );
        assert_eq!(
            verify(&[op::X; 256], &[], &from).unwrap_err().kind,
            VerifyErrorKind::TooLong
        );
    }
}
