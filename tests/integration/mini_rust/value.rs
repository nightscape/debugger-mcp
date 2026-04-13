use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PrimValue {
    I64(i64),
    Bool(bool),
    String(String),
}

impl fmt::Display for PrimValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrimValue::I64(n) => write!(f, "{n}"),
            PrimValue::Bool(b) => write!(f, "{b}"),
            PrimValue::String(s) => write!(f, "{s:?}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapKind { Hash, BTree }

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Prim(PrimValue),
    Vec(Vec<Value>),
    Map { kind: MapKind, entries: BTreeMap<PrimValue, Value> },
    Unit,
}

impl Value {
    pub fn i(n: i64) -> Self { Value::Prim(PrimValue::I64(n)) }
    pub fn b(v: bool) -> Self { Value::Prim(PrimValue::Bool(v)) }
    pub fn s(v: impl Into<String>) -> Self { Value::Prim(PrimValue::String(v.into())) }

    pub fn as_i64(&self) -> i64 {
        match self {
            Value::Prim(PrimValue::I64(n)) => *n,
            other => panic!("expected i64, got {other:?}"),
        }
    }
    pub fn as_bool(&self) -> bool {
        match self {
            Value::Prim(PrimValue::Bool(b)) => *b,
            other => panic!("expected bool, got {other:?}"),
        }
    }
    pub fn as_prim(&self) -> &PrimValue {
        match self {
            Value::Prim(p) => p,
            other => panic!("expected primitive value, got {other:?}"),
        }
    }
    pub fn into_prim(self) -> PrimValue {
        match self {
            Value::Prim(p) => p,
            other => panic!("expected primitive value, got {other:?}"),
        }
    }
}
