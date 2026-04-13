use std::collections::BTreeMap;

use super::ast::{BinOp, Block, Expr, PrimType, Program, Stmt, Type};

const HEADER: &str = "\
#![allow(unused, unused_mut, unused_assignments, unused_parens, clippy::all)]
use std::collections::{HashMap, BTreeMap};
";

pub struct Generated {
    pub source: String,
    /// line number (1-indexed) keyed by `Stmt::Observe` ID.
    pub observe_lines: BTreeMap<u32, u32>,
}

pub fn emit(prog: &Program) -> Generated {
    let mut cg = Cg::default();
    for raw in HEADER.lines() {
        cg.line(raw);
    }
    cg.line("");
    cg.line("fn main() {");
    cg.indent = 1;
    for stmt in &prog.body {
        cg.emit_stmt(stmt);
    }
    cg.indent = 0;
    cg.line("}");
    Generated { source: cg.out, observe_lines: cg.observe_lines }
}

#[derive(Default)]
struct Cg {
    out: String,
    cur_line: u32,
    indent: usize,
    observe_lines: BTreeMap<u32, u32>,
}

impl Cg {
    fn line(&mut self, s: &str) {
        for _ in 0..self.indent { self.out.push_str("    "); }
        self.out.push_str(s);
        self.out.push('\n');
        self.cur_line += 1;
    }

    fn next_line_num(&self) -> u32 { self.cur_line + 1 }

    fn emit_stmt(&mut self, s: &Stmt) {
        match s {
            Stmt::Let { name, ty, value } => {
                let text = format!("let mut {name}: {} = {};", ty_str(ty), expr_str(value));
                self.line(&text);
            }
            Stmt::Assign { name, value } => {
                self.line(&format!("{name} = {};", expr_str(value)));
            }
            Stmt::VecPush { name, value } => {
                self.line(&format!("{name}.push({});", expr_str(value)));
            }
            Stmt::VecPop { name } => {
                self.line(&format!("let _ = {name}.pop();"));
            }
            Stmt::MapInsert { name, key, value } => {
                self.line(&format!(
                    "{name}.insert({}, {});",
                    expr_str(key), expr_str(value),
                ));
            }
            Stmt::MapRemove { name, key } => {
                self.line(&format!("let _ = {name}.remove(&{});", expr_str(key)));
            }
            Stmt::If { cond, then, else_ } => {
                self.line(&format!("if {} {{", expr_str(cond)));
                self.indent += 1;
                self.emit_block(then);
                self.indent -= 1;
                if let Some(els) = else_ {
                    self.line("} else {");
                    self.indent += 1;
                    self.emit_block(els);
                    self.indent -= 1;
                }
                self.line("}");
            }
            Stmt::While { cond, body } => {
                self.line(&format!("while {} {{", expr_str(cond)));
                self.indent += 1;
                self.emit_block(body);
                self.indent -= 1;
                self.line("}");
            }
            Stmt::Observe(id) => {
                let line_num = self.next_line_num();
                self.observe_lines.insert(*id, line_num);
                // black_box produces a real instruction so consecutive observes
                // remain distinct breakpoint locations.
                self.line("std::hint::black_box(());");
            }
        }
    }

    fn emit_block(&mut self, b: &Block) {
        for s in b { self.emit_stmt(s); }
    }
}

fn ty_str(t: &Type) -> String {
    match t {
        Type::Prim(p) => prim_ty_str(p).to_string(),
        Type::Vec(inner) => format!("Vec<{}>", ty_str(inner)),
        Type::HashMap(k, v) => format!("HashMap<{}, {}>", prim_ty_str(k), ty_str(v)),
        Type::BTreeMap(k, v) => format!("BTreeMap<{}, {}>", prim_ty_str(k), ty_str(v)),
    }
}

fn prim_ty_str(p: &PrimType) -> &'static str {
    match p {
        PrimType::I64 => "i64",
        PrimType::Bool => "bool",
        PrimType::String => "String",
    }
}

fn expr_str(e: &Expr) -> String {
    match e {
        Expr::LitI64(n) => format!("{n}i64"),
        Expr::LitBool(b) => b.to_string(),
        Expr::LitString(s) => format!("String::from({s:?})"),
        Expr::Var(name) => format!("{name}.clone()"),
        Expr::BinOp(op, l, r) => format!("({} {} {})", expr_str(l), bin_op_str(*op), expr_str(r)),
        Expr::Not(x) => format!("(!{})", expr_str(x)),
        Expr::Len(name) => format!("({name}.len() as i64)"),
        Expr::VecGet(name, i) => format!("{name}[{} as usize].clone()", expr_str(i)),
        Expr::MapGet(name, k) => format!("{name}.get(&{}).cloned().unwrap()", expr_str(k)),
        Expr::MapContains(name, k) => format!("{name}.contains_key(&{})", expr_str(k)),
        Expr::EmptyVec(t) => format!("Vec::<{}>::new()", prim_ty_str(t)),
        Expr::EmptyHashMap(k, v) => format!("HashMap::<{}, {}>::new()", prim_ty_str(k), prim_ty_str(v)),
        Expr::EmptyBTreeMap(k, v) => format!("BTreeMap::<{}, {}>::new()", prim_ty_str(k), prim_ty_str(v)),
    }
}

fn bin_op_str(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+", BinOp::Sub => "-", BinOp::Mul => "*",
        BinOp::Div => "/", BinOp::Mod => "%",
        BinOp::Eq => "==", BinOp::Ne => "!=",
        BinOp::Lt => "<", BinOp::Le => "<=",
        BinOp::Gt => ">", BinOp::Ge => ">=",
        BinOp::And => "&&", BinOp::Or => "||",
    }
}
