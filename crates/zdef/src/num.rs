//! Number encoding (spec 8.1) and packed Tuya manufacturer names (spec 8.4).

use crate::cbor::{Writer, float_len};
use crate::enums::TUYA_PREFIXES;
use crate::model::Num;
use alloc::string::String;
use core::fmt::Write;

/// Significant digits of `str(float)` (spec 7.1).
pub const STR_FLOAT_DIGITS: i32 = 15;

/// `str()` of a number (spec 7.1), into `buf`; no `core::fmt`, so the VM
/// does not link the float formatter. Integers print in full. Floats print
/// with at most 15 significant digits (rounded, trailing zeros dropped):
/// plain decimal for 1e-6 <= |x| < 1e21, otherwise `d.ddde+N` / `d.ddde-N`
/// (JavaScript layout); integral values have no fraction (`21`, not `21.0`).
pub fn format_number(n: Num, buf: &mut [u8; 32]) -> &str {
    let mut w = 0usize;
    let mut push = |b: &[u8], w: &mut usize| {
        buf[*w..*w + b.len()].copy_from_slice(b);
        *w += b.len();
    };
    match n {
        Num::Int(i) => {
            if i < 0 {
                push(b"-", &mut w);
            }
            let mut d = [0u8; 20];
            let mut k = 20;
            let mut u = i.unsigned_abs();
            loop {
                k -= 1;
                d[k] = b'0' + (u % 10) as u8;
                u /= 10;
                if u == 0 {
                    break;
                }
            }
            push(&d[k..], &mut w);
        }
        Num::Float(x) => {
            if x.is_nan() || x.is_infinite() {
                push(
                    if x.is_nan() {
                        b"NaN"
                    } else if x > 0.0 {
                        b"Infinity"
                    } else {
                        b"-Infinity"
                    },
                    &mut w,
                );
            } else if x == 0.0 {
                push(b"0", &mut w);
            } else {
                if x < 0.0 {
                    push(b"-", &mut w);
                }
                let (digits, e) = float_digits(libm::fabs(x));
                let mut d = [0u8; STR_FLOAT_DIGITS as usize];
                let mut u = digits;
                for i in (0..d.len()).rev() {
                    d[i] = b'0' + (u % 10) as u8;
                    u /= 10;
                }
                let mut k = d.len();
                while k > 1 && d[k - 1] == b'0' {
                    k -= 1;
                }
                if (-6..21).contains(&e) {
                    if e < 0 {
                        push(b"0.", &mut w);
                        for _ in 0..(-e - 1) {
                            push(b"0", &mut w);
                        }
                        push(&d[..k], &mut w);
                    } else {
                        let int_len = e as usize + 1;
                        for i in 0..int_len {
                            push(if i < k { &d[i..i + 1] } else { b"0" }, &mut w);
                        }
                        if k > int_len {
                            push(b".", &mut w);
                            push(&d[int_len..k], &mut w);
                        }
                    }
                } else {
                    push(&d[..1], &mut w);
                    if k > 1 {
                        push(b".", &mut w);
                        push(&d[1..k], &mut w);
                    }
                    push(if e < 0 { b"e-" } else { b"e+" }, &mut w);
                    let mut ebuf = [0u8; 32];
                    let es = format_number(Num::Int(e.unsigned_abs() as i64), &mut ebuf);
                    push(es.as_bytes(), &mut w);
                }
            }
        }
    }
    // only ASCII was written
    core::str::from_utf8(&buf[..w]).unwrap_or("")
}

/// `x * 10^n` without overflowing the intermediate power.
fn scale10(x: f64, n: i32) -> f64 {
    if n > 300 {
        x * 1e300 * libm::pow(10.0, (n - 300) as f64)
    } else if n < -300 {
        x * 1e-300 * libm::pow(10.0, (n + 300) as f64)
    } else {
        x * libm::pow(10.0, n as f64)
    }
}

/// `x > 0` as `(digits, e)`: `x ≈ digits * 10^(e - 14)` with exactly 15
/// digits (`10^14 <= digits < 10^15`), rounded half away from zero.
fn float_digits(x: f64) -> (u64, i32) {
    const LO: f64 = 1e14;
    const HI: f64 = 1e15;
    let mut e = libm::floor(libm::log10(x)) as i32;
    let mut d = libm::round(scale10(x, STR_FLOAT_DIGITS - 1 - e));
    for _ in 0..2 {
        if d >= HI {
            e += 1;
        } else if d < LO {
            e -= 1;
        } else {
            break;
        }
        d = libm::round(scale10(x, STR_FLOAT_DIGITS - 1 - e));
    }
    if d >= HI {
        // rounding carried into a 16th digit (e.g. 9.999999999999999e2)
        d = LO;
        e += 1;
    }
    (d as u64, e)
}

/// Shortest decimal `(mantissa, k)` with `v == mantissa / 10^k`, `1 <= k <= 9`,
/// using the shortest round-trip representation of `v`.
pub fn decimal_fraction(v: f64) -> Option<(i64, u8)> {
    if !v.is_finite() {
        return None;
    }
    let mut buf = Buf::<40>::new();
    write!(buf, "{}", v).ok()?;
    let s = buf.as_str();
    if s.contains('e') {
        return None;
    }
    let (int_part, frac) = s.split_once('.')?; // no '.' = integral
    let frac = frac.trim_end_matches('0');
    let k = frac.len();
    if k == 0 || k > 9 {
        return None;
    }
    let neg = int_part.starts_with('-');
    let digits_int = int_part.trim_start_matches('-');
    let mut m: i64 = 0;
    for c in digits_int.bytes().chain(frac.bytes()) {
        m = m.checked_mul(10)?.checked_add((c - b'0') as i64)?;
    }
    Some((if neg { -m } else { m }, k as u8))
}

fn int_len(v: i64) -> usize {
    let u = if v < 0 { (-1 - v) as u64 } else { v as u64 };
    if u < 24 {
        1
    } else if u <= 0xff {
        2
    } else if u <= 0xffff {
        3
    } else if u <= 0xffff_ffff {
        5
    } else {
        9
    }
}

/// Writes a non-integral float as the shorter of the shortest exact IEEE
/// float and a tag-4 decimal fraction (tie: the float).
pub fn write_float(w: &mut Writer, v: f64) {
    if let Some((m, k)) = decimal_fraction(v) {
        let frac_len = 2 + int_len(-(k as i64)) + int_len(m);
        if frac_len < float_len(v) {
            w.tag(4);
            w.array(2);
            w.int(-(k as i64));
            w.int(m);
            return;
        }
    }
    w.float(v);
}

/// Writes a number field: integral values as integers (spec 8.1).
pub fn write_num(w: &mut Writer, n: Num) {
    match n.normalized() {
        Num::Int(i) => w.int(i),
        Num::Float(f) => write_float(w, f),
    }
}

/// Value of a tag-4 decimal fraction.
pub fn decimal_value(exp: i64, mant: i64) -> Option<f64> {
    if !(-18..=0).contains(&exp) {
        return None;
    }
    // 10^k is exact in f64 up to k = 22.
    let mut p = 1.0f64;
    for _ in 0..-exp {
        p *= 10.0;
    }
    Some(mant as f64 / p)
}

const BASE36: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// Packs a Tuya manufacturer name `_TZxxxx_` + 8 × `[0-9a-z]` into 7 bytes
/// (prefix index, 48-bit big-endian base-36 suffix); `None` if not packable.
pub fn pack_tuya_name(s: &str) -> Option<[u8; 7]> {
    let b = s.as_bytes();
    if b.len() != 16 {
        return None;
    }
    let p = TUYA_PREFIXES.iter().position(|p| p.as_bytes() == &b[..8])?;
    let mut n: u64 = 0;
    for &c in &b[8..] {
        let d = BASE36.iter().position(|&x| x == c)? as u64;
        n = n * 36 + d;
    }
    let mut out = [0u8; 7];
    out[0] = p as u8;
    out[1..].copy_from_slice(&n.to_be_bytes()[2..]);
    Some(out)
}

pub fn unpack_tuya_name(b: &[u8]) -> Option<String> {
    if b.len() != 7 {
        return None;
    }
    let prefix = TUYA_PREFIXES.get(b[0] as usize)?;
    let mut n = b[1..].iter().fold(0u64, |a, &x| (a << 8) | x as u64);
    if n >= 36u64.pow(8) {
        return None;
    }
    let mut suffix = [0u8; 8];
    for i in (0..8).rev() {
        suffix[i] = BASE36[(n % 36) as usize];
        n /= 36;
    }
    let mut s = String::with_capacity(16);
    s.push_str(prefix);
    s.push_str(core::str::from_utf8(&suffix).ok()?);
    Some(s)
}

/// Small stack buffer implementing `fmt::Write`.
pub struct Buf<const N: usize> {
    b: [u8; N],
    n: usize,
}

impl<const N: usize> Buf<N> {
    pub fn new() -> Self {
        Buf { b: [0; N], n: 0 }
    }
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.b[..self.n]).unwrap_or("")
    }
}

impl<const N: usize> Default for Buf<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Write for Buf<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        if self.n + s.len() > N {
            return Err(core::fmt::Error);
        }
        self.b[self.n..self.n + s.len()].copy_from_slice(s.as_bytes());
        self.n += s.len();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals() {
        assert_eq!(decimal_fraction(0.1), Some((1, 1)));
        assert_eq!(decimal_fraction(0.000001), Some((1, 6)));
        assert_eq!(decimal_fraction(-1.5), Some((-15, 1)));
        assert_eq!(decimal_fraction(2.0), None);
        assert_eq!(decimal_fraction(0.30000000000000004), None);
        let mut w = Writer::new();
        write_float(&mut w, 0.1);
        assert_eq!(w.buf, [0xc4, 0x82, 0x20, 0x01]);
        let mut w = Writer::new();
        write_float(&mut w, 0.5);
        assert_eq!(w.buf, [0xf9, 0x38, 0x00]);
        let mut w = Writer::new();
        write_float(&mut w, 1e-6);
        assert_eq!(w.buf, [0xc4, 0x82, 0x25, 0x01]);
    }

    #[test]
    fn tuya_names() {
        let p = pack_tuya_name("_TZE200_bvu2wnxz").unwrap();
        assert_eq!(unpack_tuya_name(&p).unwrap(), "_TZE200_bvu2wnxz");
        assert!(pack_tuya_name("_TZE200_4utwoz2").is_none());
        assert!(pack_tuya_name("_TZE200_BVU2WNXZ").is_none());
    }
}

#[cfg(test)]
mod str_tests {
    use super::*;

    fn f(n: Num) -> alloc::string::String {
        let mut b = [0u8; 32];
        format_number(n, &mut b).into()
    }

    #[test]
    fn number_strings() {
        for (x, s) in [
            (21.0, "21"),
            (21.5, "21.5"),
            (-0.25, "-0.25"),
            (0.1 + 0.2, "0.3"),
            (1.0 / 3.0, "0.333333333333333"),
            (2.0 / 3.0, "0.666666666666667"),
            (123456.789, "123456.789"),
            (1e20, "100000000000000000000"),
            (1e21, "1e+21"),
            (1.5e-7, "1.5e-7"),
            (1e-6, "0.000001"),
            (0.000123, "0.000123"),
            (999.9999999999999, "1000"),
            (-1.7976931348623157e308, "-1.79769313486232e+308"),
            (5e-324, "4.94065645841247e-324"),
            (2.71, "2.71"),
        ] {
            assert_eq!(f(Num::Float(x)), s, "{x:e}");
        }
        assert_eq!(f(Num::Int(0)), "0");
        assert_eq!(f(Num::Int(-42)), "-42");
        assert_eq!(f(Num::Int(i64::MIN)), "-9223372036854775808");
    }
}
