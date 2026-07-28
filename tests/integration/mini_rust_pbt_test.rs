//! Property-based test that drives a real CodeLLDB session against
//! interpreter-generated reference state.
//!
//! For each generated mini-Rust program we:
//!   1. Codegen to a temp .rs file (records line numbers for every Observe).
//!   2. Run the interpreter to produce a Snapshot per Observe execution.
//!   3. Compile with rustc -g, launch a debug session.
//!   4. Set breakpoints at every observation line.
//!   5. Continue → wait for stop → run oracle::check_snapshot, snapshot-by-snapshot.
//!   6. Fail with a structured Mismatch report on any divergence.
//!
//! Three properties ride on each generated program:
//!
//!   * **Ballast invariance** — the generator also picks a `FrameShape`: unrelated
//!     heavy state (a `&mut self` owning large maps, heavy sibling locals,
//!     recursion depth, dead DWARF types) added to the frame the observe points
//!     live in. The program's own locals are untouched by it, so every expected
//!     value is unchanged by construction. Anything that breaks under ballast is
//!     the debugger, not the program.
//!   * **Access-path agreement** — the same scalar read through all five
//!     documented paths must return the same value. A disagreement, a loud
//!     failure, and an answer of nothing at all are all violations.
//!   * **No poisoning** — after each of those reads the session must still
//!     answer a cheap stack trace and must not have moved to Failed.
//!
//! Size tier comes from `MINI_RUST_TIER` (`fast` default, `heavy` for the
//! expensive end).
//!
//! This test is `#[ignore]` because it spawns a real CodeLLDB process.
//! Run with: `cargo test --test mini_rust_pbt_test -- --ignored --nocapture`

#[path = "mini_rust/mod.rs"]
mod mini_rust;

use proptest::test_runner::{Config, TestCaseError, TestRunner};
use serde_json::json;
use std::cell::Cell;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Runtime;
use tokio::sync::RwLock;
use tokio::time::timeout;

use debugger_mcp::debug::SessionManager;
use debugger_mcp::mcp::tools::ToolsHandler;

use mini_rust::ast::Program;
use mini_rust::codegen::FrameShape;
use mini_rust::interp::Snapshot;
use mini_rust::value::{PrimValue, Value};
use mini_rust::{codegen, interp, oracle, strategy};

const START_TIMEOUT: Duration = Duration::from_secs(30);
const STEP_TIMEOUT: Duration = Duration::from_secs(15);
const TOOL_TIMEOUT: Duration = Duration::from_secs(10);

/// Budget for one access-path read. Deliberately generous compared to the
/// adapter's own 4s/8s request timeouts: this asserts that a small scalar is
/// *readable*, and we want the adapter's timeout to be what fails, with its own
/// diagnostics, rather than ours firing first and hiding it.
const PATH_BUDGET: Duration = Duration::from_secs(12);

fn check_prerequisites() -> bool {
    for (cmd, arg) in [("codelldb", "--help"), ("rustc", "--version")] {
        let ok = Command::new(cmd).arg(arg).output()
            .map(|o| o.status.success()).unwrap_or(false);
        if !ok {
            eprintln!("skipping: {cmd} not available");
            return false;
        }
    }
    true
}

async fn tool_call(
    tools: &ToolsHandler,
    name: &str,
    args: serde_json::Value,
    dur: Duration,
) -> Result<serde_json::Value, String> {
    match timeout(dur, tools.handle_tool(name, args)).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(format!("{e}")),
        Err(_) => Err(format!("{name} timed out after {dur:?}")),
    }
}

/// Gather diagnostic context after a snapshot failure. None of these calls are
/// allowed to fail the test on their own — a failed get-output is just less
/// signal, not a different failure. Returns a multi-section string for embedding
/// into the test error.
async fn gather_failure_context(
    tools: &ToolsHandler,
    session_id: &str,
    snaps: &[Snapshot],
    failing_idx: usize,
) -> String {
    let session_state = tool_call(tools, "debugger_session_state",
        json!({"sessionId": session_id}), TOOL_TIMEOUT).await
        .map(|v| serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string()))
        .unwrap_or_else(|e| format!("(unavailable: {e})"));

    let stack_trace = tool_call(tools, "debugger_stack_trace",
        json!({"sessionId": session_id, "includeLocals": false}), TOOL_TIMEOUT).await
        .map(|v| serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string()))
        .unwrap_or_else(|e| format!("(unavailable: {e})"));

    let output_tail = tool_call(tools, "debugger_get_output",
        json!({"sessionId": session_id, "limit": 50}), TOOL_TIMEOUT).await
        .map(|v| serde_json::to_string_pretty(v.get("entries").unwrap_or(&json!([])))
            .unwrap_or_else(|_| v.to_string()))
        .unwrap_or_else(|e| format!("(unavailable: {e})"));

    let trace_so_far: Vec<String> = snaps.iter()
        .enumerate()
        .take(failing_idx + 1)
        .map(|(i, s)| {
            let marker = if i == failing_idx { " <-- FAIL" } else { "" };
            format!("  [{}] line {} vars={:#?}{}", i, s.line, s.vars, marker)
        })
        .collect();

    format!(
        "--- session_state ---\n{}\n\n--- stack_trace ---\n{}\n\n\
         --- last output (50 lines) ---\n{}\n\n--- snapshot trace through fail ---\n{}",
        session_state, stack_trace, output_tail, trace_so_far.join("\n"),
    )
}

/// The scalars every generated program declares, in the order we prefer to probe
/// them. Only one is probed per snapshot — five access paths each is already
/// five round-trips, and rotating through them across snapshots covers all three
/// primitive types without multiplying the cost per stop.
fn scalar_to_probe(snap: &Snapshot, idx: usize) -> Option<(&str, PrimValue)> {
    const NAMES: [&str; 3] = ["i", "b", "s"];
    let name = NAMES[idx % NAMES.len()];
    match snap.vars.get(name) {
        Some(Value::Prim(p)) => Some((name, p.clone())),
        _ => None,
    }
}

async fn run_one(prog: Program, shape: FrameShape, iter: usize) -> Result<(), String> {
    let g = codegen::emit_with(&prog, &shape);
    let snaps = interp::run(&prog, &g.observe_lines);

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = PathBuf::from(&manifest_dir)
        .join(format!("tests/fixtures/target/mini_rust_pbt/{iter}"));
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("mkdir: {e}"))?;
    let src = out_dir.join("prog.rs");
    let bin = out_dir.join("prog");
    std::fs::write(&src, &g.source).map_err(|e| format!("write: {e}"))?;

    let compile = Command::new("rustc")
        .arg(&src).arg("-g").arg("-C").arg("opt-level=0")
        .arg("-o").arg(&bin)
        .output().map_err(|e| format!("rustc spawn: {e}"))?;
    if !compile.status.success() {
        return Err(format!(
            "rustc failed:\n{}\n--- source ---\n{}",
            String::from_utf8_lossy(&compile.stderr), g.source,
        ));
    }

    let session_manager = Arc::new(RwLock::new(SessionManager::new()));
    let tools = ToolsHandler::new(Arc::clone(&session_manager));

    let cwd = bin.parent().unwrap().to_string_lossy().to_string();
    let start = tool_call(&tools, "debugger_start", json!({
        "language": "rust",
        "program": bin.to_string_lossy(),
        "args": [],
        "stopOnEntry": true,
        "cwd": cwd,
    }), START_TIMEOUT).await?;
    let session_id = start["sessionId"].as_str().unwrap().to_string();

    tool_call(&tools, "debugger_wait_for_stop", json!({
        "sessionId": &session_id, "timeoutMs": 10000,
    }), STEP_TIMEOUT).await?;

    let mut observe_lines: Vec<u32> = g.observe_lines.values().copied().collect();
    observe_lines.sort();
    observe_lines.dedup();
    for line in &observe_lines {
        tool_call(&tools, "debugger_set_breakpoint", json!({
            "sessionId": &session_id,
            "sourcePath": src.to_string_lossy(),
            "line": *line,
        }), TOOL_TIMEOUT).await?;
    }

    for (idx, snap) in snaps.iter().enumerate() {
        tool_call(&tools, "debugger_continue", json!({"sessionId": &session_id}),
            TOOL_TIMEOUT).await?;
        let stop = tool_call(&tools, "debugger_wait_for_stop", json!({
            "sessionId": &session_id, "timeoutMs": 10000,
        }), STEP_TIMEOUT).await?;

        if stop["state"].as_str().unwrap_or("") == "Terminated" {
            let ctx = gather_failure_context(&tools, &session_id, &snaps, idx).await;
            let _ = tool_call(&tools, "debugger_disconnect",
                json!({"sessionId": &session_id}), TOOL_TIMEOUT).await;
            return Err(format!(
                "snap {idx}: program terminated, expected stop at line {}\n\n{ctx}\n\n\
                 --- source ---\n{}",
                snap.line, g.source,
            ));
        }

        let mut mismatches =
            oracle::check_snapshot_via_variables(&tools, &session_id, snap).await;

        if let Some((name, expected)) = scalar_to_probe(snap, idx) {
            mismatches.extend(
                oracle::check_scalar_all_paths(
                    &tools, &session_id, name, &expected, PATH_BUDGET,
                )
                .await,
            );
            mismatches.extend(
                oracle::check_session_live(
                    &tools,
                    &session_id,
                    &format!("reading `{name}` through all access paths at snap {idx}"),
                )
                .await,
            );
        }

        if !mismatches.is_empty() {
            let ctx = gather_failure_context(&tools, &session_id, &snaps, idx).await;
            let _ = tool_call(&tools, "debugger_disconnect",
                json!({"sessionId": &session_id}), TOOL_TIMEOUT).await;
            return Err(format!(
                "snap {idx} at line {} (shape {shape:?}):\n{:#?}\n\n{ctx}\n\n--- source ---\n{}",
                snap.line, mismatches, g.source,
            ));
        }
    }

    let _ = tool_call(&tools, "debugger_continue", json!({"sessionId": &session_id}),
        TOOL_TIMEOUT).await;
    let _ = tool_call(&tools, "debugger_wait_for_stop", json!({
        "sessionId": &session_id, "timeoutMs": 5000,
    }), STEP_TIMEOUT).await;
    let _ = tool_call(&tools, "debugger_disconnect",
        json!({"sessionId": &session_id}), TOOL_TIMEOUT).await;

    Ok(())
}

#[test]
#[ignore]
fn pbt_generated_programs() {
    if !check_prerequisites() { return; }

    let cases: u32 = std::env::var("PROPTEST_CASES")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(8);
    let mut runner = TestRunner::new(Config { cases, ..Config::default() });

    let rt = Runtime::new().unwrap();
    let iter = Cell::new(0usize);

    runner.run(&strategy::program_and_shape_strategy(), |(prog, shape)| {
        iter.set(iter.get() + 1);
        let this_iter = iter.get();
        rt.block_on(run_one(prog, shape, this_iter))
            .map_err(|e| TestCaseError::fail(format!("iter {this_iter}: {e}")))?;
        Ok(())
    }).unwrap();
}
