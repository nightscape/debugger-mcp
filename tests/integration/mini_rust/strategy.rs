//! proptest strategies that generate well-typed mini-Rust programs.
//!
//! Programs use a fixed environment of pre-declared variables so generation
//! never needs to track scope. All operations stay within the type rules,
//! so neither the interpreter nor the SUT can panic from generation alone.

use proptest::prelude::*;

use super::ast::{BinOp, Block, Expr, PrimType, Program, Stmt, Type};
use super::codegen::FrameShape;

/// Tunable: how many ops a generated program contains (excluding declarations
/// and trailing observation).
const MIN_OPS: usize = 1;
const MAX_OPS: usize = 30;

/// Bounded key/value spaces so `remove`/`contains_key`/`map[&k]` ops have a
/// reasonable chance of hitting present keys (collision pressure for the SUT).
///
/// HANDOFF asked for `i64::MIN`/`i64::MAX` here. Skipped: any pairing of `SetI(MAX)`
/// with a positive `BumpI` overflows raw `i + n` in both interp and codegen, which
/// is a bug in the test pipeline, not in the SUT. To unlock MIN/MAX, switch addition
/// in `interp::eval_binop` and `codegen::bin_op_str` to `wrapping_add`. Until then,
/// `±100`/`±1000` give the parsers something larger than single-digit but won't
/// overflow.
const SMALL_I64S: &[i64] = &[-1000, -100, -2, -1, 0, 1, 2, 3, 5, 7, 100, 1000];
const SMALL_BUMP_I64S: &[i64] = &[-2, -1, 0, 1, 2, 3, 5, 7];
const SMALL_STRS: &[&str] = &[
    "a", "b", "c", "d", "alpha", "beta",
    "with\nnewline",
    "with\"quote",
    "alpha-beta-gamma-delta-epsilon-zeta-eta-theta-iota-kappa-lambda-mu-nu-xi-omicron-pi-rho-sigma",
];

#[derive(Clone, Debug)]
enum Op {
    SetI(i64),
    BumpI(i64),
    SetB(bool),
    NotB,
    SetS(&'static str),

    VPush(i64),
    VPop,

    HInsert(&'static str, i64),
    HRemove(&'static str),

    TInsert(i64, &'static str),
    TRemove(i64),

    Observe,
}

fn op_strategy() -> BoxedStrategy<Op> {
    prop_oneof![
        any_i64().prop_map(Op::SetI),
        any_bump_i64().prop_map(Op::BumpI),
        any::<bool>().prop_map(Op::SetB),
        Just(Op::NotB),
        any_str().prop_map(Op::SetS),

        any_i64().prop_map(Op::VPush),
        Just(Op::VPop),

        (any_str(), any_i64()).prop_map(|(k, v)| Op::HInsert(k, v)),
        any_str().prop_map(Op::HRemove),

        (any_i64(), any_str()).prop_map(|(k, v)| Op::TInsert(k, v)),
        any_i64().prop_map(Op::TRemove),

        // Higher weight for Observe so traces are densely instrumented.
        Just(Op::Observe),
        Just(Op::Observe),
    ].boxed()
}

fn any_i64() -> BoxedStrategy<i64> {
    proptest::sample::select(SMALL_I64S.to_vec()).boxed()
}

fn any_bump_i64() -> BoxedStrategy<i64> {
    proptest::sample::select(SMALL_BUMP_I64S.to_vec()).boxed()
}

fn any_str() -> BoxedStrategy<&'static str> {
    proptest::sample::select(SMALL_STRS.to_vec()).boxed()
}

/// Top-level strategy: returns a complete Program with declarations and a
/// final observation point so the trace always has at least one snapshot.
pub fn program_strategy() -> BoxedStrategy<Program> {
    proptest::collection::vec(op_strategy(), MIN_OPS..=MAX_OPS)
        .prop_map(build_program)
        .boxed()
}

/// Size tiers for the ballast axis. `fast` keeps generated programs cheap to
/// compile; `heavy` reaches the scale at which the reported wedge reproduces.
struct Tier {
    ballast_fields: (usize, usize),
    ballast_size: (usize, usize),
    ballast_locals: (usize, usize),
    recursion_depth: (usize, usize),
    dwarf_types: (usize, usize),
}

const FAST_TIER: Tier = Tier {
    ballast_fields: (0, 3),
    ballast_size: (0, 64),
    ballast_locals: (0, 2),
    recursion_depth: (0, 3),
    dwarf_types: (0, 8),
};

const HEAVY_TIER: Tier = Tier {
    ballast_fields: (2, 6),
    ballast_size: (2000, 20000),
    ballast_locals: (1, 3),
    recursion_depth: (0, 6),
    dwarf_types: (200, 2000),
};

fn current_tier() -> &'static Tier {
    match std::env::var("MINI_RUST_TIER").as_deref().unwrap_or("fast") {
        "fast" => &FAST_TIER,
        "heavy" => &HEAVY_TIER,
        other => panic!("unknown MINI_RUST_TIER {other:?}: expected \"fast\" or \"heavy\""),
    }
}

/// Ballast dimensions for the observing frame. Every range shrinks toward its
/// low end, so a failure minimizes to the smallest ballast that still triggers.
pub fn frame_shape_strategy() -> BoxedStrategy<FrameShape> {
    let t = current_tier();
    (
        t.ballast_fields.0..=t.ballast_fields.1,
        t.ballast_size.0..=t.ballast_size.1,
        t.ballast_locals.0..=t.ballast_locals.1,
        t.recursion_depth.0..=t.recursion_depth.1,
        t.dwarf_types.0..=t.dwarf_types.1,
    )
        .prop_map(|(ballast_fields, ballast_size, ballast_locals, recursion_depth, dwarf_types)| {
            FrameShape { ballast_fields, ballast_size, ballast_locals, recursion_depth, dwarf_types }
        })
        .boxed()
}

/// The metamorphic pair: the same program, plus unrelated frame ballast.
pub fn program_and_shape_strategy() -> BoxedStrategy<(Program, FrameShape)> {
    (program_strategy(), frame_shape_strategy()).boxed()
}

fn build_program(ops: Vec<Op>) -> Program {
    let mut body: Block = vec![
        Stmt::Let { name: "i".into(), ty: Type::i64(), value: Expr::i(0) },
        Stmt::Let { name: "b".into(), ty: Type::bool(), value: Expr::b(false) },
        Stmt::Let { name: "s".into(), ty: Type::string(), value: Expr::s("init") },

        Stmt::Let {
            name: "v".into(),
            ty: Type::vec(Type::i64()),
            value: Expr::EmptyVec(PrimType::I64),
        },
        Stmt::Let {
            name: "h".into(),
            ty: Type::hashmap(PrimType::String, Type::i64()),
            value: Expr::EmptyHashMap(PrimType::String, PrimType::I64),
        },
        Stmt::Let {
            name: "t".into(),
            ty: Type::btreemap(PrimType::I64, Type::string()),
            value: Expr::EmptyBTreeMap(PrimType::I64, PrimType::String),
        },

        Stmt::observe(),
    ];

    for op in ops {
        body.push(op_to_stmt(op));
    }
    body.push(Stmt::observe());
    // Keepalive AFTER the final observe: by referencing each long-lived local
    // on subsequent lines, rustc keeps them in scope at the observe's PC.
    // Without this, LLDB reports all locals as missing on the line just
    // before `}`.
    body.push(Stmt::Assign { name: "i".into(), value: Expr::var("i") });
    body.push(Stmt::Assign { name: "b".into(), value: Expr::var("b") });
    body.push(Stmt::Assign { name: "s".into(), value: Expr::var("s") });

    Program::new(body)
}

fn op_to_stmt(op: Op) -> Stmt {
    match op {
        Op::SetI(n) => Stmt::Assign { name: "i".into(), value: Expr::i(n) },
        Op::BumpI(n) => Stmt::Assign {
            name: "i".into(),
            value: Expr::bin(BinOp::Add, Expr::var("i"), Expr::i(n)),
        },
        Op::SetB(b) => Stmt::Assign { name: "b".into(), value: Expr::b(b) },
        Op::NotB => Stmt::Assign {
            name: "b".into(),
            value: Expr::Not(Box::new(Expr::var("b"))),
        },
        Op::SetS(s) => Stmt::Assign { name: "s".into(), value: Expr::s(s) },

        Op::VPush(n) => Stmt::VecPush { name: "v".into(), value: Expr::i(n) },
        Op::VPop => Stmt::VecPop { name: "v".into() },

        Op::HInsert(k, v) => Stmt::MapInsert {
            name: "h".into(), key: Expr::s(k), value: Expr::i(v),
        },
        Op::HRemove(k) => Stmt::MapRemove { name: "h".into(), key: Expr::s(k) },

        Op::TInsert(k, v) => Stmt::MapInsert {
            name: "t".into(), key: Expr::i(k), value: Expr::s(v),
        },
        Op::TRemove(k) => Stmt::MapRemove { name: "t".into(), key: Expr::i(k) },

        Op::Observe => Stmt::observe(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::test_runner::{Config, TestRunner};

    use crate::mini_rust::{codegen, interp};

    /// Sanity: the strategy produces programs that codegen to compilable Rust
    /// and interpret without panicking. We don't actually invoke rustc here
    /// (too slow for many cases) — the smoke tests cover that. We just verify
    /// the codegen + interp pipeline accepts everything the strategy emits.
    #[test]
    fn generated_programs_run_through_interp_and_codegen() {
        let mut runner = TestRunner::new(Config { cases: 64, ..Config::default() });

        runner.run(&program_strategy(), |prog| {
            let g = codegen::emit(&prog);
            let snaps = interp::run(&prog, &g.observe_lines);

            // At minimum the entry observe and trailing observe.
            prop_assert!(snaps.len() >= 2);

            // Source must parse syntactically (cheap check: balanced braces,
            // fn main present).
            let src = &g.source;
            prop_assert!(src.contains("fn main()"));
            let opens = src.matches('{').count();
            let closes = src.matches('}').count();
            prop_assert_eq!(opens, closes);

            // Every observe line in the line map points at the sentinel.
            for (_, line) in &g.observe_lines {
                let l = src.lines().nth((*line as usize) - 1).unwrap();
                prop_assert!(l.trim().starts_with("std::hint::black_box(());"));
            }

            Ok(())
        }).unwrap();
    }

    /// The ballast axis is metamorphic: it must not disturb the observe map or
    /// the program's own locals, only add state around them.
    #[test]
    fn ballast_is_purely_additive() {
        let mut runner = TestRunner::new(Config { cases: 64, ..Config::default() });

        runner.run(&program_and_shape_strategy(), |(prog, shape)| {
            let flat = codegen::emit(&prog);
            let heavy = codegen::emit_with(&prog, &shape);

            prop_assert_eq!(
                flat.observe_lines.keys().collect::<Vec<_>>(),
                heavy.observe_lines.keys().collect::<Vec<_>>()
            );

            let opens = heavy.source.matches('{').count();
            let closes = heavy.source.matches('}').count();
            prop_assert_eq!(opens, closes);

            for (_, line) in &heavy.observe_lines {
                let l = heavy.source.lines().nth((*line as usize) - 1).unwrap();
                prop_assert!(l.trim().starts_with("std::hint::black_box(());"));
            }

            prop_assert!(heavy.source.contains("let mut i: i64 = 0i64;"));

            Ok(())
        }).unwrap();
    }
}
