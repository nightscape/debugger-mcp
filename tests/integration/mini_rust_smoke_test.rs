//! Smoke test for the mini-Rust scaffolding (AST + interpreter + codegen).
//!
//! Hand-builds a small program that exercises Vec, HashMap, and BTreeMap,
//! runs the interpreter to produce a snapshot trace, codegens the program
//! to real Rust source, and compiles it with rustc to verify the round-trip.
//!
//! Once this passes we'll wire it into the real CodeLLDB PBT as the SUT.

#[path = "mini_rust/mod.rs"]
mod mini_rust;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use mini_rust::ast::*;
use mini_rust::codegen;
use mini_rust::interp;
use mini_rust::value::{MapKind, PrimValue, Value};

fn rustc_available() -> bool {
    Command::new("rustc").arg("--version").output()
        .map(|o| o.status.success()).unwrap_or(false)
}

fn build_demo_program() -> Program {
    use BinOp::*;

    let body: Block = vec![
        Stmt::Let { name: "i".into(), ty: Type::i64(), value: Expr::i(0) },
        Stmt::Let { name: "sum".into(), ty: Type::i64(), value: Expr::i(0) },
        Stmt::observe(),

        Stmt::While {
            cond: Expr::bin(Lt, Expr::var("i"), Expr::i(3)),
            body: vec![
                Stmt::Assign {
                    name: "sum".into(),
                    value: Expr::bin(Add, Expr::var("sum"), Expr::var("i")),
                },
                Stmt::Assign {
                    name: "i".into(),
                    value: Expr::bin(Add, Expr::var("i"), Expr::i(1)),
                },
                Stmt::observe(),
            ],
        },
    ];
    Program::new(body)
}

fn build_collections_program() -> Program {
    let body: Block = vec![
        Stmt::Let {
            name: "v".into(),
            ty: Type::vec(Type::i64()),
            value: Expr::EmptyVec(PrimType::I64),
        },
        Stmt::VecPush { name: "v".into(), value: Expr::i(10) },
        Stmt::VecPush { name: "v".into(), value: Expr::i(20) },
        Stmt::VecPush { name: "v".into(), value: Expr::i(30) },
        Stmt::observe(),                       // v == [10, 20, 30]
        Stmt::VecPop { name: "v".into() },
        Stmt::observe(),                       // v == [10, 20]

        Stmt::Let {
            name: "h".into(),
            ty: Type::hashmap(PrimType::String, Type::i64()),
            value: Expr::EmptyHashMap(PrimType::String, PrimType::I64),
        },
        Stmt::MapInsert { name: "h".into(), key: Expr::s("a"), value: Expr::i(1) },
        Stmt::MapInsert { name: "h".into(), key: Expr::s("b"), value: Expr::i(2) },
        Stmt::observe(),                       // h == {"a"->1, "b"->2}
        Stmt::MapRemove { name: "h".into(), key: Expr::s("a") },
        Stmt::observe(),                       // h == {"b"->2}

        Stmt::Let {
            name: "t".into(),
            ty: Type::btreemap(PrimType::I64, Type::string()),
            value: Expr::EmptyBTreeMap(PrimType::I64, PrimType::String),
        },
        Stmt::MapInsert { name: "t".into(), key: Expr::i(2), value: Expr::s("two") },
        Stmt::MapInsert { name: "t".into(), key: Expr::i(1), value: Expr::s("one") },
        Stmt::observe(),                       // t == {1->"one", 2->"two"}
    ];
    Program::new(body)
}

#[test]
fn collections_program_interprets_correctly() {
    let prog = build_collections_program();
    let g = codegen::emit(&prog);
    let snaps = interp::run(&prog, &g.observe_lines);

    assert_eq!(snaps.len(), 5);

    // After pushing 10, 20, 30
    let v0 = snaps[0].vars.get("v").unwrap();
    assert_eq!(v0, &Value::Vec(vec![Value::i(10), Value::i(20), Value::i(30)]));

    // After pop
    let v1 = snaps[1].vars.get("v").unwrap();
    assert_eq!(v1, &Value::Vec(vec![Value::i(10), Value::i(20)]));

    // After hashmap inserts
    let h0 = snaps[2].vars.get("h").unwrap();
    let mut expected_h0 = BTreeMap::new();
    expected_h0.insert(PrimValue::String("a".into()), Value::i(1));
    expected_h0.insert(PrimValue::String("b".into()), Value::i(2));
    assert_eq!(h0, &Value::Map { kind: MapKind::Hash, entries: expected_h0 });

    // After hashmap remove
    let h1 = snaps[3].vars.get("h").unwrap();
    let mut expected_h1 = BTreeMap::new();
    expected_h1.insert(PrimValue::String("b".into()), Value::i(2));
    assert_eq!(h1, &Value::Map { kind: MapKind::Hash, entries: expected_h1 });

    // BTreeMap final
    let t0 = snaps[4].vars.get("t").unwrap();
    let mut expected_t0 = BTreeMap::new();
    expected_t0.insert(PrimValue::I64(1), Value::s("one"));
    expected_t0.insert(PrimValue::I64(2), Value::s("two"));
    assert_eq!(t0, &Value::Map { kind: MapKind::BTree, entries: expected_t0 });
}

#[test]
fn collections_program_compiles_with_rustc() {
    if !rustc_available() {
        eprintln!("skipping: rustc not available");
        return;
    }

    let prog = build_collections_program();
    let g = codegen::emit(&prog);

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = PathBuf::from(&manifest_dir).join("tests/fixtures/target/mini_rust_smoke");
    std::fs::create_dir_all(&out_dir).unwrap();

    let src = out_dir.join("collections.rs");
    let bin = out_dir.join("collections");
    std::fs::write(&src, &g.source).unwrap();

    let result = Command::new("rustc")
        .arg(&src).arg("-g").arg("-C").arg("opt-level=0")
        .arg("-o").arg(&bin)
        .output().unwrap();

    assert!(
        result.status.success(),
        "rustc failed:\n--- source ---\n{}\n--- stderr ---\n{}",
        g.source,
        String::from_utf8_lossy(&result.stderr),
    );
}

#[test]
fn interpreter_runs_demo() {
    let prog = build_demo_program();

    // Codegen first to obtain the line map.
    let g = codegen::emit(&prog);
    let snaps = interp::run(&prog, &g.observe_lines);

    // 1 entry observe + 3 loop observes = 4 snapshots.
    assert_eq!(snaps.len(), 4);

    // Final snapshot: i == 3, sum == 0+1+2 == 3.
    let last = snaps.last().unwrap();
    assert_eq!(last.vars.get("i").cloned(), Some(Value::i(3)));
    assert_eq!(last.vars.get("sum").cloned(), Some(Value::i(3)));
}

#[test]
fn codegen_lines_align_with_source() {
    let prog = build_demo_program();
    let g = codegen::emit(&prog);

    // Each Observe maps to a real `let _: () = ();` line in the source.
    for (_, line_num) in &g.observe_lines {
        let line = g.source.lines().nth((*line_num as usize) - 1)
            .expect("line in range");
        assert!(
            line.trim().starts_with("std::hint::black_box(());"),
            "expected observe sentinel at line {line_num}, got: {line:?}"
        );
    }
}

#[test]
fn codegen_compiles_with_rustc() {
    if !rustc_available() {
        eprintln!("skipping: rustc not available");
        return;
    }

    let prog = build_demo_program();
    let g = codegen::emit(&prog);

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = PathBuf::from(&manifest_dir).join("tests/fixtures/target/mini_rust_smoke");
    std::fs::create_dir_all(&out_dir).unwrap();

    let src = out_dir.join("smoke.rs");
    let bin = out_dir.join("smoke");
    std::fs::write(&src, &g.source).unwrap();

    let result = Command::new("rustc")
        .arg(&src).arg("-g").arg("-C").arg("opt-level=0")
        .arg("-o").arg(&bin)
        .output().unwrap();

    assert!(
        result.status.success(),
        "rustc failed:\n--- source ---\n{}\n--- stderr ---\n{}",
        g.source,
        String::from_utf8_lossy(&result.stderr),
    );
}

/// Demonstrates that the Value/MapKind/PrimValue types are usable as oracle data.
#[test]
fn value_types_compose() {
    let mut entries = BTreeMap::new();
    entries.insert(PrimValue::String("a".into()), Value::i(1));
    entries.insert(PrimValue::String("b".into()), Value::i(2));
    let m = Value::Map { kind: MapKind::Hash, entries };

    if let Value::Map { kind, entries } = m {
        assert_eq!(kind, MapKind::Hash);
        assert_eq!(entries.len(), 2);
    } else {
        panic!("expected Map");
    }
}
