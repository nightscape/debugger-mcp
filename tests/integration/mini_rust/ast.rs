use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrimType {
    I64,
    Bool,
    String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Prim(PrimType),
    Vec(Box<Type>),
    HashMap(PrimType, Box<Type>),
    BTreeMap(PrimType, Box<Type>),
}

impl Type {
    pub fn i64() -> Self { Type::Prim(PrimType::I64) }
    pub fn bool() -> Self { Type::Prim(PrimType::Bool) }
    pub fn string() -> Self { Type::Prim(PrimType::String) }
    pub fn vec(t: Type) -> Self { Type::Vec(Box::new(t)) }
    pub fn hashmap(k: PrimType, v: Type) -> Self { Type::HashMap(k, Box::new(v)) }
    pub fn btreemap(k: PrimType, v: Type) -> Self { Type::BTreeMap(k, Box::new(v)) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinOp {
    Add, Sub, Mul, Div, Mod,
    Eq, Ne, Lt, Le, Gt, Ge,
    And, Or,
}

impl fmt::Display for BinOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            BinOp::Add => "+", BinOp::Sub => "-", BinOp::Mul => "*",
            BinOp::Div => "/", BinOp::Mod => "%",
            BinOp::Eq => "==", BinOp::Ne => "!=",
            BinOp::Lt => "<",  BinOp::Le => "<=",
            BinOp::Gt => ">",  BinOp::Ge => ">=",
            BinOp::And => "&&", BinOp::Or => "||",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    LitI64(i64),
    LitBool(bool),
    LitString(String),
    Var(String),

    BinOp(BinOp, Box<Expr>, Box<Expr>),
    Not(Box<Expr>),

    Len(String),                       // x.len() as i64
    VecGet(String, Box<Expr>),         // x[i].clone()      — index must be in bounds
    MapGet(String, Box<Expr>),         // x.get(&k).cloned().unwrap()
    MapContains(String, Box<Expr>),    // x.contains_key(&k)

    EmptyVec(PrimType),
    EmptyHashMap(PrimType, PrimType),
    EmptyBTreeMap(PrimType, PrimType),
}

impl Expr {
    pub fn i(n: i64) -> Self { Expr::LitI64(n) }
    pub fn b(v: bool) -> Self { Expr::LitBool(v) }
    pub fn s(v: impl Into<String>) -> Self { Expr::LitString(v.into()) }
    pub fn var(name: impl Into<String>) -> Self { Expr::Var(name.into()) }
    pub fn bin(op: BinOp, l: Expr, r: Expr) -> Self {
        Expr::BinOp(op, Box::new(l), Box::new(r))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Let { name: String, ty: Type, value: Expr },
    Assign { name: String, value: Expr },

    VecPush { name: String, value: Expr },
    VecPop { name: String },                                // discards the result
    MapInsert { name: String, key: Expr, value: Expr },
    MapRemove { name: String, key: Expr },                  // discards the result

    If { cond: Expr, then: Block, else_: Option<Block> },
    While { cond: Expr, body: Block },

    /// Observation point. The ID is assigned in preorder by `Program::new` so each
    /// site has a stable identifier that survives re-execution inside loops.
    /// Pass `Stmt::observe()` and let the constructor renumber.
    Observe(u32),
}

impl Stmt {
    pub fn observe() -> Self { Stmt::Observe(u32::MAX) }
}

pub type Block = Vec<Stmt>;

#[derive(Debug, Clone)]
pub struct Program {
    pub body: Block,
    pub observe_count: u32,
}

impl Program {
    pub fn new(mut body: Block) -> Self {
        let mut counter = 0u32;
        renumber_observes(&mut body, &mut counter);
        Self { body, observe_count: counter }
    }
}

fn renumber_observes(block: &mut Block, counter: &mut u32) {
    for stmt in block {
        match stmt {
            Stmt::Observe(id) => { *id = *counter; *counter += 1; }
            Stmt::If { then, else_, .. } => {
                renumber_observes(then, counter);
                if let Some(els) = else_ { renumber_observes(els, counter); }
            }
            Stmt::While { body, .. } => renumber_observes(body, counter),
            _ => {}
        }
    }
}
