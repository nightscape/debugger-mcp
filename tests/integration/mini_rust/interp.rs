use std::collections::BTreeMap;

use super::ast::{BinOp, Block, Expr, PrimType, Program, Stmt, Type};
use super::value::{MapKind, PrimValue, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub line: u32,
    pub vars: BTreeMap<String, Value>,
}

pub struct Interp<'a> {
    env: BTreeMap<String, Value>,
    line_of: &'a BTreeMap<u32, u32>,
    trace: Vec<Snapshot>,
}

/// Run a program against a line map keyed by `Stmt::Observe` ID.
pub fn run(prog: &Program, line_of: &BTreeMap<u32, u32>) -> Vec<Snapshot> {
    let mut interp = Interp {
        env: BTreeMap::new(),
        line_of,
        trace: Vec::new(),
    };
    interp.exec_block(&prog.body);
    interp.trace
}

impl<'a> Interp<'a> {
    fn exec_block(&mut self, block: &Block) {
        for stmt in block {
            self.exec_stmt(stmt);
        }
    }

    fn exec_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let { name, ty, value } => {
                let v = self.eval(value);
                check_type(&v, ty);
                self.env.insert(name.clone(), v);
            }
            Stmt::Assign { name, value } => {
                let v = self.eval(value);
                let slot = self.env.get_mut(name)
                    .unwrap_or_else(|| panic!("assign to undeclared `{name}`"));
                *slot = v;
            }
            Stmt::VecPush { name, value } => {
                let v = self.eval(value);
                let target = self.env.get_mut(name).expect("vec push target");
                match target {
                    Value::Vec(items) => items.push(v),
                    other => panic!("vec push on non-Vec: {other:?}"),
                }
            }
            Stmt::VecPop { name } => {
                let target = self.env.get_mut(name).expect("vec pop target");
                match target {
                    Value::Vec(items) => { items.pop(); }
                    other => panic!("vec pop on non-Vec: {other:?}"),
                }
            }
            Stmt::MapInsert { name, key, value } => {
                let k = self.eval(key).into_prim();
                let v = self.eval(value);
                let target = self.env.get_mut(name).expect("map insert target");
                match target {
                    Value::Map { entries, .. } => { entries.insert(k, v); }
                    other => panic!("map insert on non-Map: {other:?}"),
                }
            }
            Stmt::MapRemove { name, key } => {
                let k = self.eval(key).into_prim();
                let target = self.env.get_mut(name).expect("map remove target");
                match target {
                    Value::Map { entries, .. } => { entries.remove(&k); }
                    other => panic!("map remove on non-Map: {other:?}"),
                }
            }
            Stmt::If { cond, then, else_ } => {
                if self.eval(cond).as_bool() {
                    self.exec_block(then);
                } else if let Some(e) = else_ {
                    self.exec_block(e);
                }
            }
            Stmt::While { cond, body } => {
                while self.eval(cond).as_bool() {
                    self.exec_block(body);
                }
            }
            Stmt::Observe(id) => {
                let line = *self.line_of.get(id)
                    .unwrap_or_else(|| panic!("missing line mapping for observe #{id}"));
                self.trace.push(Snapshot {
                    line,
                    vars: self.env.clone(),
                });
            }
        }
    }

    fn eval(&self, e: &Expr) -> Value {
        match e {
            Expr::LitI64(n) => Value::i(*n),
            Expr::LitBool(b) => Value::b(*b),
            Expr::LitString(s) => Value::s(s.clone()),
            Expr::Var(name) => self.env.get(name)
                .unwrap_or_else(|| panic!("read undeclared `{name}`"))
                .clone(),
            Expr::BinOp(op, l, r) => eval_binop(*op, self.eval(l), self.eval(r)),
            Expr::Not(x) => Value::b(!self.eval(x).as_bool()),
            Expr::Len(name) => {
                let v = self.env.get(name).expect("len target");
                let n = match v {
                    Value::Vec(items) => items.len(),
                    Value::Map { entries, .. } => entries.len(),
                    other => panic!("len on non-collection: {other:?}"),
                };
                Value::i(n as i64)
            }
            Expr::VecGet(name, i) => {
                let idx = self.eval(i).as_i64();
                let v = self.env.get(name).expect("vec get target");
                match v {
                    Value::Vec(items) => items.get(idx as usize)
                        .cloned()
                        .unwrap_or_else(|| panic!("vec index {idx} out of bounds (len={})", items.len())),
                    other => panic!("vec get on non-Vec: {other:?}"),
                }
            }
            Expr::MapGet(name, k) => {
                let key = self.eval(k).into_prim();
                let v = self.env.get(name).expect("map get target");
                match v {
                    Value::Map { entries, .. } => entries.get(&key)
                        .cloned()
                        .unwrap_or_else(|| panic!("map missing key {key:?}")),
                    other => panic!("map get on non-Map: {other:?}"),
                }
            }
            Expr::MapContains(name, k) => {
                let key = self.eval(k).into_prim();
                let v = self.env.get(name).expect("map contains target");
                match v {
                    Value::Map { entries, .. } => Value::b(entries.contains_key(&key)),
                    other => panic!("map contains on non-Map: {other:?}"),
                }
            }
            Expr::EmptyVec(_) => Value::Vec(Vec::new()),
            Expr::EmptyHashMap(_, _) => Value::Map { kind: MapKind::Hash, entries: BTreeMap::new() },
            Expr::EmptyBTreeMap(_, _) => Value::Map { kind: MapKind::BTree, entries: BTreeMap::new() },
        }
    }
}

fn eval_binop(op: BinOp, l: Value, r: Value) -> Value {
    use BinOp::*;
    match (op, &l, &r) {
        (Add | Sub | Mul | Div | Mod, _, _) => {
            let a = l.as_i64(); let b = r.as_i64();
            let n = match op {
                Add => a + b, Sub => a - b, Mul => a * b,
                Div => { assert!(b != 0, "div by zero"); a / b }
                Mod => { assert!(b != 0, "mod by zero"); a % b }
                _ => unreachable!(),
            };
            Value::i(n)
        }
        (Lt | Le | Gt | Ge, _, _) => {
            let a = l.as_i64(); let b = r.as_i64();
            let v = match op {
                Lt => a < b, Le => a <= b, Gt => a > b, Ge => a >= b,
                _ => unreachable!(),
            };
            Value::b(v)
        }
        (Eq, _, _) => Value::b(l == r),
        (Ne, _, _) => Value::b(l != r),
        (And, _, _) => Value::b(l.as_bool() && r.as_bool()),
        (Or, _, _) => Value::b(l.as_bool() || r.as_bool()),
    }
}

fn check_type(v: &Value, ty: &Type) {
    let ok = match (v, ty) {
        (Value::Prim(PrimValue::I64(_)),    Type::Prim(PrimType::I64))    => true,
        (Value::Prim(PrimValue::Bool(_)),   Type::Prim(PrimType::Bool))   => true,
        (Value::Prim(PrimValue::String(_)), Type::Prim(PrimType::String)) => true,
        (Value::Vec(_),                     Type::Vec(_))                 => true,
        (Value::Map { kind: MapKind::Hash,  .. }, Type::HashMap(_, _))    => true,
        (Value::Map { kind: MapKind::BTree, .. }, Type::BTreeMap(_, _))   => true,
        _ => false,
    };
    assert!(ok, "type mismatch: value {v:?} not assignable to {ty:?}");
}
