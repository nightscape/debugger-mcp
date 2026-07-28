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

/// Unrelated state added *around* the program body so the observing frame is
/// heavy. Purely additive: the program's own locals stay ordinary `let mut`
/// bindings in the same frame, so the interpreter oracle is unaffected.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameShape {
    /// Container fields on the `Ballast` struct reached via `&mut self`.
    pub ballast_fields: usize,
    /// Entries per ballast container.
    pub ballast_size: usize,
    /// Extra heavy locals in the observing frame.
    pub ballast_locals: usize,
    /// Self-recursive calls before the body runs.
    pub recursion_depth: usize,
    /// Dead types emitted purely to inflate debug info.
    pub dwarf_types: usize,
}

impl FrameShape {
    pub fn flat() -> Self { Self::default() }

    pub fn is_flat(&self) -> bool { *self == Self::default() }
}

pub fn emit(prog: &Program) -> Generated {
    emit_with(prog, &FrameShape::flat())
}

pub fn emit_with(prog: &Program, shape: &FrameShape) -> Generated {
    let mut cg = Cg::default();
    for raw in HEADER.lines() {
        cg.line(raw);
    }
    if shape.is_flat() {
        cg.line("");
        cg.line("fn main() {");
        cg.indent = 1;
        cg.emit_block(&prog.body);
        cg.indent = 0;
        cg.line("}");
    } else {
        cg.emit_ballast(prog, shape);
    }
    Generated { source: cg.out, observe_lines: cg.observe_lines }
}

/// Cycles the three container kinds across ballast fields.
fn container_ty(idx: usize) -> &'static str {
    match idx % 3 {
        0 => "HashMap<i64, BallastRow>",
        1 => "BTreeMap<i64, BallastRow>",
        _ => "Vec<BallastRow>",
    }
}

fn container_ctor(idx: usize) -> &'static str {
    match idx % 3 {
        0 => "HashMap::new()",
        1 => "BTreeMap::new()",
        _ => "Vec::new()",
    }
}

fn container_fill(idx: usize, target: &str) -> String {
    match idx % 3 {
        0 | 1 => format!("{target}.insert(k, BallastRow::new(k));"),
        _ => format!("{target}.push(BallastRow::new(k));"),
    }
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
        if !s.is_empty() {
            for _ in 0..self.indent { self.out.push_str("    "); }
        }
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

    fn emit_ballast(&mut self, prog: &Program, shape: &FrameShape) {
        self.line("");
        self.line("struct BallastRow { id: i64, tag: String, bytes: Vec<u8> }");
        self.line("");
        self.line("impl BallastRow {");
        self.indent = 1;
        self.line("fn new(id: i64) -> BallastRow {");
        self.indent = 2;
        self.line("BallastRow { id: id, tag: format!(\"ballast-row-{}\", id), bytes: vec![0u8; 32] }");
        self.indent = 1;
        self.line("}");
        self.indent = 0;
        self.line("}");

        self.line("");
        self.line("struct Ballast {");
        self.indent = 1;
        for f in 0..shape.ballast_fields {
            self.line(&format!("c{f}: {},", container_ty(f)));
        }
        self.indent = 0;
        self.line("}");

        self.line("");
        self.line("impl Ballast {");
        self.indent = 1;
        self.line("fn new() -> Ballast {");
        self.indent = 2;
        self.line("let mut b = Ballast {");
        self.indent = 3;
        for f in 0..shape.ballast_fields {
            self.line(&format!("c{f}: {},", container_ctor(f)));
        }
        self.indent = 2;
        self.line("};");
        for f in 0..shape.ballast_fields {
            self.line(&format!("for k in 0..{}i64 {{", shape.ballast_size));
            self.indent = 3;
            self.line(&container_fill(f, &format!("b.c{f}")));
            self.indent = 2;
            self.line("}");
        }
        self.line("b");
        self.indent = 1;
        self.line("}");

        self.line("");
        self.line("fn step(&mut self, depth: usize) {");
        self.indent = 2;
        self.line("if depth > 0 {");
        self.indent = 3;
        self.line("self.step(depth - 1);");
        self.line("return;");
        self.indent = 2;
        self.line("}");
        for l in 0..shape.ballast_locals {
            self.line(&format!("let mut heavy_{l}: BTreeMap<i64, BallastRow> = BTreeMap::new();"));
            self.line(&format!("for k in 0..{}i64 {{", shape.ballast_size));
            self.indent = 3;
            self.line(&format!("heavy_{l}.insert(k, BallastRow::new(k));"));
            self.indent = 2;
            self.line("}");
        }
        self.emit_block(&prog.body);
        // Keeps the heavy locals live across the observe points above.
        for l in 0..shape.ballast_locals {
            self.line(&format!("std::hint::black_box(&heavy_{l});"));
        }
        self.indent = 1;
        self.line("}");
        self.indent = 0;
        self.line("}");

        self.emit_dwarf_types(shape.dwarf_types);

        self.line("");
        self.line("fn main() {");
        self.indent = 1;
        self.line("let mut ballast = Ballast::new();");
        self.line(&format!("ballast.step({});", shape.recursion_depth));
        if shape.dwarf_types > 0 {
            self.line("if std::hint::black_box(false) {");
            self.indent = 2;
            for t in 0..shape.dwarf_types {
                self.line(&format!("dead_fn_{t}(&{});", dead_value(t)));
            }
            self.indent = 1;
            self.line("}");
        }
        self.indent = 0;
        self.line("}");
    }

    /// Types that are compiled and described in DWARF but never executed, so
    /// they drive debug-info *parse* cost independently of container size.
    fn emit_dwarf_types(&mut self, n: usize) {
        for t in 0..n {
            self.line("");
            if t % 2 == 0 {
                self.line(&format!("struct DeadS{t} {{ a: i64, b: String, c: Vec<u8>, d: (i64, bool), e: BTreeMap<i64, String> }}"));
            } else {
                self.line(&format!("enum DeadE{t} {{ A(i64), B {{ x: String, y: Vec<i64> }}, C }}"));
            }
            self.line("#[inline(never)]");
            self.line(&format!("fn dead_fn_{t}(x: &{}) {{", dead_ty(t)));
            self.indent = 1;
            self.line("std::hint::black_box(x);");
            self.indent = 0;
            self.line("}");
        }
    }
}

fn dead_ty(t: usize) -> String {
    if t % 2 == 0 { format!("DeadS{t}") } else { format!("DeadE{t}") }
}

fn dead_value(t: usize) -> String {
    if t % 2 == 0 {
        format!("DeadS{t} {{ a: 0, b: String::new(), c: Vec::new(), d: (0, false), e: BTreeMap::new() }}")
    } else {
        format!("DeadE{t}::A(0)")
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
