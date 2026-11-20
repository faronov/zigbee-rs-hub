//! Expression property test: random well-typed programs are compiled,
//! verified, decompiled (round trip) and executed on the VM; results must
//! equal an independent reference interpreter over the AST.

use zdef::expr::{Context, Inputs, NoHost, Site, Ty, Value, Vm};
use zdefc::expr::{codegen, decompile, parse};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Reference values.
#[derive(Clone, Debug, PartialEq)]
enum R {
    Null,
    B(bool),
    I(i64),
    F(f64),
}

/// Generates source text for an int- or bool-typed expression, with its reference evaluator.
struct G<'r> {
    rng: &'r mut Rng,
    lets: Vec<String>,
}

impl G<'_> {
    fn int(&mut self, d: u32) -> String {
        let leaf = d == 0 || self.rng.below(4) == 0;
        if leaf {
            return match self.rng.below(6) {
                0 => "x".into(),
                1 => format!("payload[{}]", self.rng.below(6)),
                2 if !self.lets.is_empty() => {
                    self.lets[self.rng.below(self.lets.len() as u64) as usize].clone()
                }
                3 => format!("{}", self.rng.below(300)),
                4 => format!("u16le(payload, {})", self.rng.below(5)),
                _ => format!("{}", self.rng.below(5)),
            };
        }
        match self.rng.below(12) {
            0..=4 => {
                let op = ["+", "-", "*", "//", "%", "&", "|", "^", "<<", ">>"]
                    [self.rng.below(10) as usize];
                format!("({} {} {})", self.int(d - 1), op, self.int(d - 1))
            }
            5 => format!("-{}", self.int(d - 1)),
            6 => format!(
                "({} ? {} : {})",
                self.boolean(d - 1),
                self.int(d - 1),
                self.int(d - 1)
            ),
            7 => format!("({} ?? {})", self.int(d - 1), self.int(d - 1)),
            8 => format!(
                "{}({}, {})",
                ["min", "max"][self.rng.below(2) as usize],
                self.int(d - 1),
                self.int(d - 1)
            ),
            9 => format!("abs({})", self.int(d - 1)),
            10 => format!(
                "bits({}, {}, {})",
                self.int(d - 1),
                self.rng.below(8),
                1 + self.rng.below(8)
            ),
            _ => {
                let name = format!("a{}", self.lets.len());
                let v = self.int(d - 1);
                self.lets.push(name.clone());
                let body = self.int(d - 1);
                self.lets.pop();
                format!("(let {name} = {v} in {body})")
            }
        }
    }

    fn boolean(&mut self, d: u32) -> String {
        if d == 0 || self.rng.below(4) == 0 {
            return match self.rng.below(3) {
                0 => "true".into(),
                1 => "false".into(),
                _ => format!("bit(x, {})", self.rng.below(8)),
            };
        }
        match self.rng.below(5) {
            0 => {
                let op = ["==", "!=", "<", "<=", ">", ">="][self.rng.below(6) as usize];
                format!("({} {} {})", self.int(d - 1), op, self.int(d - 1))
            }
            1 => format!("({} && {})", self.boolean(d - 1), self.boolean(d - 1)),
            2 => format!("({} || {})", self.boolean(d - 1), self.boolean(d - 1)),
            3 => format!("!{}", self.boolean(d - 1)),
            _ => format!("isnull({})", self.int(d - 1)),
        }
    }
}

/// Independent reference interpreter over the parsed AST (spec 7.1 semantics).
fn eval(a: &zdefc::expr::Ast, x: i64, p: &[u8], locals: &mut [R; 8]) -> Result<R, ()> {
    use zdef::expr::op;
    use zdefc::expr::Ast::*;
    let int = |r: &R| match r {
        R::I(i) => Ok(*i),
        _ => Err(()),
    };
    Ok(match a {
        Int(i) => R::I(*i),
        Bool(b) => R::B(*b),
        Null => R::Null,
        X => R::I(x),
        Local(n) => locals[*n as usize].clone(),
        Index(b, i) => {
            let i = int(&eval(i, x, p, locals)?)?;
            assert!(matches!(**b, Payload), "generator indexes payload only");
            R::I(*p.get(usize::try_from(i).map_err(|_| ())?).ok_or(())? as i64)
        }
        Unary(o, e) => {
            let v = eval(e, x, p, locals)?;
            match (*o, v) {
                (_, R::Null) if *o != op::LNOT => R::Null,
                (op::NEG, R::I(i)) => R::I(i.checked_neg().ok_or(())?),
                (op::LNOT, R::B(b)) => R::B(!b),
                (op::LNOT, R::Null) => R::B(true),
                _ => return Err(()),
            }
        }
        Bin(o, l, r) => {
            let (a, b) = (eval(l, x, p, locals)?, eval(r, x, p, locals)?);
            match *o {
                op::EQ => R::B(a == b),
                op::NE => R::B(a != b),
                op::LAND | op::LOR => {
                    let t = |v: &R| match v {
                        R::B(b) => Ok(*b),
                        R::Null => Ok(false),
                        _ => Err(()),
                    };
                    R::B(if *o == op::LAND {
                        t(&a)? && t(&b)?
                    } else {
                        t(&a)? || t(&b)?
                    })
                }
                _ if a == R::Null || b == R::Null => R::Null,
                op::LT => R::B(int(&a)? < int(&b)?),
                op::LE => R::B(int(&a)? <= int(&b)?),
                op::GT => R::B(int(&a)? > int(&b)?),
                op::GE => R::B(int(&a)? >= int(&b)?),
                _ => {
                    let (a, b) = (int(&a)? as i128, int(&b)? as i128);
                    let v: i128 = match *o {
                        op::ADD => a + b,
                        op::SUB => a - b,
                        op::MUL => a * b,
                        op::IDIV => {
                            if b == 0 {
                                return Err(());
                            }
                            a.div_euclid(b) - if b < 0 && a.rem_euclid(b) != 0 { 1 } else { 0 }
                        }
                        op::MOD => {
                            if b == 0 {
                                return Err(());
                            }
                            let r = a % b;
                            if r != 0 && (r < 0) != (b < 0) {
                                r + b
                            } else {
                                r
                            }
                        }
                        op::AND => a & b,
                        op::OR => a | b,
                        op::XOR => a ^ b,
                        op::SHL | op::SHR => {
                            if !(0..=63).contains(&b) {
                                return Err(());
                            }
                            if *o == op::SHL {
                                ((a as i64) << b) as i128
                            } else {
                                ((a as i64) >> b) as i128
                            }
                        }
                        _ => return Err(()),
                    };
                    R::I(i64::try_from(v).map_err(|_| ())?)
                }
            }
        }
        Coalesce(l, r) => match eval(l, x, p, locals)? {
            R::Null => eval(r, x, p, locals)?,
            v => v,
        },
        Cond(c, t, e) => match eval(c, x, p, locals)? {
            R::B(true) => eval(t, x, p, locals)?,
            R::B(false) | R::Null => eval(e, x, p, locals)?,
            _ => return Err(()),
        },
        Let(binds, body) => {
            for (slot, v) in binds {
                locals[*slot as usize] = eval(v, x, p, locals)?;
            }
            eval(body, x, p, locals)?
        }
        Call(f, args) => {
            let name = zdef::expr::FUNCS[*f as usize].name;
            // `payload` (only an argument of u16le here) evaluates to a placeholder
            let vals: Vec<R> = args
                .iter()
                .map(|a| {
                    if matches!(a, Payload) {
                        Ok(R::I(0))
                    } else {
                        eval(a, x, p, locals)
                    }
                })
                .collect::<Result<_, _>>()?;
            if name == "isnull" {
                return Ok(R::B(vals[0] == R::Null));
            }
            if vals.contains(&R::Null) {
                return Ok(R::Null);
            }
            match name {
                "min" => R::I(int(&vals[0])?.min(int(&vals[1])?)),
                "max" => R::I(int(&vals[0])?.max(int(&vals[1])?)),
                "abs" => R::I(int(&vals[0])?.checked_abs().ok_or(())?),
                "bit" => R::B((int(&vals[0])? >> int(&vals[1])?) & 1 == 1),
                "bits" => R::I(
                    ((int(&vals[0])? as u64) >> int(&vals[1])?) as i64
                        & ((1i64 << int(&vals[2])?) - 1),
                ),
                "u16le" => {
                    let o = int(&vals[1])? as usize;
                    let s = p.get(o..o + 2).ok_or(())?;
                    R::I(u16::from_le_bytes([s[0], s[1]]) as i64)
                }
                _ => return Err(()),
            }
        }
        _ => return Err(()),
    })
}

fn vm_value(v: Value<'_>) -> R {
    match v {
        Value::Null => R::Null,
        Value::Bool(b) => R::B(b),
        Value::Int(i) => R::I(i),
        Value::Float(f) => R::F(f),
        _ => R::Null,
    }
}

#[test]
fn random_programs_match_reference() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let ctx = Context::new(Site::Event).with_x(Ty::INT);
    let mut vm = Vm::new();
    let (mut ran, mut errors) = (0, 0);
    for case in 0..4000 {
        let mut g = G {
            rng: &mut rng,
            lets: Vec::new(),
        };
        let src = if case % 3 == 0 {
            g.boolean(4)
        } else {
            g.int(4)
        };
        let ast = parse(&src).unwrap_or_else(|e| panic!("{src}: {e}"));
        let mut pool = Vec::new();
        let Ok(code) = codegen(&ast, &mut pool) else {
            continue;
        }; // longer than 255 B
        zdef::expr::verify(&code, &pool, &ctx).unwrap_or_else(|e| panic!("{src}: {e:?}"));
        let back = decompile(&code, &pool).unwrap_or_else(|e| panic!("{src}: {e}"));
        let mut p2 = pool.clone();
        assert_eq!(
            codegen(&parse(&back).unwrap(), &mut p2).unwrap(),
            code,
            "{src} -> {back}"
        );
        for _ in 0..8 {
            let x = rng.below(600) as i64 - 300;
            let payload: Vec<u8> = (0..rng.below(8)).map(|_| rng.next() as u8).collect();
            let want = eval(&ast, x, &payload, &mut std::array::from_fn(|_| R::Null));
            let got = vm
                .eval(
                    &code,
                    &pool,
                    &Inputs {
                        x: Value::Int(x),
                        payload: Some(&payload),
                        last: None,
                    },
                    &NoHost,
                )
                .map(vm_value);
            match (want, got) {
                (Ok(w), Ok(g)) => assert_eq!(w, g, "{src} x={x} p={payload:?}"),
                (Err(()), Err(_)) => errors += 1,
                (w, g) => panic!("{src} x={x} p={payload:?}: reference {w:?}, vm {g:?}"),
            }
            ran += 1;
        }
    }
    assert!(ran > 20000 && errors > 0, "ran {ran}, errors {errors}");
}
