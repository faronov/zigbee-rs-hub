//! Expression boundary (spec 7).
//!
//! The bytecode compiler, verifier and VM belong to `zdef` (branch `zdef`).
//! The engine only needs to *run* an expression in a context, so it sees an
//! expression as a trait object; the `zdef` VM program implements
//! [`Expression`] through a thin adapter, the generic tier uses
//! [`NativeExpr`] closures, and the host tests use a reference interpreter
//! of the source language (`testutil::expr`).

use std::fmt;
use std::sync::Arc;

/// A run-time value (spec 7.1).
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Value {
    /// Unknown.
    #[default]
    Null,
    /// Boolean.
    Bool(bool),
    /// Integer.
    Int(i64),
    /// Float.
    Float(f64),
    /// String.
    Str(String),
    /// Bytes.
    Bytes(Vec<u8>),
}

impl Value {
    /// `true` for [`Value::Null`].
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
    /// Numeric view.
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Self::Int(i) => Some(i as f64),
            Self::Float(f) => Some(f),
            Self::Bool(b) => Some(f64::from(u8::from(b))),
            _ => None,
        }
    }
    /// Integer view (integral floats included).
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Self::Int(i) => Some(i),
            Self::Float(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => Some(f as i64),
            Self::Bool(b) => Some(i64::from(b)),
            _ => None,
        }
    }
    /// Truthiness for bool platforms: `Bool` as is, numbers non-zero.
    pub fn truthy(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            Self::Int(i) => Some(*i != 0),
            Self::Float(f) => Some(*f != 0.0),
            _ => None,
        }
    }
    /// String view.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }
    /// Converts to JSON (integral floats stay floats; non-finite → null).
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Null => serde_json::Value::Null,
            Self::Bool(b) => (*b).into(),
            Self::Int(i) => (*i).into(),
            Self::Float(f) => serde_json::Number::from_f64(*f).map_or(serde_json::Value::Null, Into::into),
            Self::Str(s) => s.clone().into(),
            Self::Bytes(b) => hex(b).into(),
        }
    }
    /// Converts from JSON (arrays and objects → null).
    pub fn from_json(v: &serde_json::Value) -> Self {
        match v {
            serde_json::Value::Bool(b) => Self::Bool(*b),
            serde_json::Value::Number(n) => n.as_i64().map_or_else(|| Self::Float(n.as_f64().unwrap_or(f64::NAN)), Self::Int),
            serde_json::Value::String(s) => Self::Str(s.clone()),
            _ => Self::Null,
        }
    }
}

/// Lower-case hex of bytes.
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Everything an expression may read besides `x`, `payload` and `last`.
pub trait ExprEnv {
    /// `state.<key>`.
    fn state(&self, _key: &str) -> Value {
        Value::Null
    }
    /// `opt.<key>`.
    fn opt(&self, _key: &str) -> Value {
        Value::Null
    }
    /// `gpd.<field>`.
    fn gpd(&self, _field: &str) -> Value {
        Value::Null
    }
    /// `fp.<field>`.
    fn fp(&self, _field: &str) -> Value {
        Value::Null
    }
    /// `battery_percent(mV)` with the device's battery curve.
    fn battery_percent(&self, _mv: f64) -> Value {
        Value::Null
    }
    /// `now()`, Unix seconds.
    fn now(&self) -> i64 {
        0
    }
    /// `tz_offset()`, seconds.
    fn tz_offset(&self) -> i64 {
        0
    }
}

/// An environment with nothing in it.
#[derive(Debug, Default, Clone, Copy)]
pub struct EmptyEnv;
impl ExprEnv for EmptyEnv {}

/// Evaluation context of one expression run.
pub struct ExprCtx<'a> {
    /// `x`.
    pub x: Value,
    /// `payload`.
    pub payload: Option<&'a [u8]>,
    /// `last`.
    pub last: Option<&'a str>,
    /// The rest.
    pub env: &'a dyn ExprEnv,
}

impl<'a> ExprCtx<'a> {
    /// A context with only `x`.
    pub fn with_x(x: Value, env: &'a dyn ExprEnv) -> Self {
        Self { x, payload: None, last: None, env }
    }
}

/// A verified, runnable expression. Run-time errors yield [`Value::Null`]
/// (spec 7.1) and never panic.
pub trait Expression: Send + Sync + fmt::Debug {
    /// Evaluates the expression.
    fn eval(&self, ctx: &ExprCtx<'_>) -> Value;
    /// Source text, for diagnostics.
    fn source(&self) -> Option<&str> {
        None
    }
}

/// Shared expression handle.
pub type Expr = Arc<dyn Expression>;

/// An expression implemented natively (generic tier, tests).
pub struct NativeExpr {
    name: &'static str,
    f: Box<dyn Fn(&ExprCtx<'_>) -> Value + Send + Sync>,
}

impl NativeExpr {
    /// Wraps a closure.
    pub fn new(name: &'static str, f: impl Fn(&ExprCtx<'_>) -> Value + Send + Sync + 'static) -> Expr {
        Arc::new(Self { name, f: Box::new(f) })
    }
}

impl fmt::Debug for NativeExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "native:{}", self.name)
    }
}

impl Expression for NativeExpr {
    fn eval(&self, ctx: &ExprCtx<'_>) -> Value {
        (self.f)(ctx)
    }
    fn source(&self) -> Option<&str> {
        Some(self.name)
    }
}

/// Compiles source expressions (the `zdef` compiler on the client side, the
/// reference interpreter in tests). The hub itself only receives compiled
/// bytecode (D15); this trait exists for the host tests and tools.
pub trait ExprCompiler {
    /// Compiles `src`.
    fn compile(&self, src: &str) -> Result<Expr, String>;
}
