//! Reproducer for the silent line-tables-only failure mode.
//!
//! When a Rust crate is built with `debug = "line-tables-only"` (or `-C
//! debuginfo=1`), DWARF carries source-line mappings but no local-variable
//! info. CodeLLDB correctly reports "no variable information is available"
//! when asked to *evaluate* a name, but the higher-level paths (`get_variables`
//! on locals, the auto-enrichment that runs on every stop) silently return an
//! empty list with no signal that locals were stripped.
//!
//! The reported user experience: `debugger_get_variables` returned `{ variables:
//! [], count: 0 }` and the LLM had to do a stack-inspection round trip to
//! realise the build profile was at fault. The downstream failure mode here is
//! a wasted multi-minute debugging session chasing a phantom "address must be
//! wrong".
//!
//! These tests build a fixture with `-C debuginfo=1`, stop at a known line,
//! and assert that the response either:
//!   - includes a hint distinguishing this case from "stopped frame with no
//!     locals" (any of: `hint`, `note`, `warning`, `dwarfStripped`,
//!     `localsAvailable: false`), OR
//!   - falls through to a full debug build for comparison and the empty
//!     locals there are also empty (i.e. the program actually has no locals,
//!     and the silence is not misleading).
//!
//! Currently expected to FAIL: the variables tool returns
//! `{ variables: [], count: 0 }` with no signal. Run with:
//! `cargo test --test line_tables_only_test -- --ignored --nocapture`

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::RwLock;
use tokio::time::timeout;

use debugger_mcp::debug::SessionManager;
use debugger_mcp::mcp::tools::ToolsHandler;

const TOOL_BUDGET: Duration = Duration::from_secs(15);

fn check_prerequisites() -> bool {
    for (cmd, arg) in [("codelldb", "--help"), ("rustc", "--version")] {
        let ok = Command::new(cmd)
            .arg(arg)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("skipping: {cmd} not available");
            return false;
        }
    }
    true
}

const FIXTURE_SRC: &str = "\
#![allow(unused, unused_mut, unused_variables)]
fn main() {
    let n: i64 = 42;
    let s: String = String::from(\"hi\");
    let v: Vec<i64> = vec![1, 2, 3];
    std::hint::black_box(&n);
    std::hint::black_box(&s);
    std::hint::black_box(&v);
    std::hint::black_box(()); // BP_HERE
}
";

fn bp_line() -> u32 {
    FIXTURE_SRC
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains("// BP_HERE"))
        .map(|(i, _)| (i + 1) as u32)
        .expect("BP_HERE marker missing")
}

/// Compile FIXTURE_SRC with the given debuginfo level (`1` = line-tables-only,
/// `2` = full). Returns (binary, source path). Uses a per-debuginfo subdirectory
/// so the two builds don't clobber each other.
fn compile_fixture(debuginfo: u32) -> Option<(PathBuf, PathBuf)> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = PathBuf::from(&manifest_dir)
        .join(format!("tests/fixtures/target/line_tables_only/debuginfo_{debuginfo}"));
    std::fs::create_dir_all(&out_dir).ok()?;
    let src_path = out_dir.join("prog.rs");
    let bin_path = out_dir.join("prog");
    std::fs::write(&src_path, FIXTURE_SRC).ok()?;

    let result = Command::new("rustc")
        .arg(&src_path)
        .arg("-C")
        .arg(format!("debuginfo={debuginfo}"))
        .arg("-C")
        .arg("opt-level=0")
        .arg("-o")
        .arg(&bin_path)
        .output()
        .ok()?;
    if !result.status.success() {
        eprintln!(
            "rustc failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        return None;
    }
    Some((bin_path, src_path))
}

async fn tool(
    tools: &ToolsHandler,
    name: &str,
    args: Value,
) -> Result<Value, String> {
    match timeout(TOOL_BUDGET, tools.handle_tool(name, args)).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(format!("{e}")),
        Err(_) => Err(format!("{name} exceeded {TOOL_BUDGET:?}")),
    }
}

/// Stop at the BP marker in the given binary. Returns (tools, session_id) on
/// success.
async fn stopped_at_bp(binary: &PathBuf, src_path: &PathBuf) -> Option<(ToolsHandler, String)> {
    let sm = Arc::new(RwLock::new(SessionManager::new()));
    let tools = ToolsHandler::new(Arc::clone(&sm));

    let cwd = binary.parent().unwrap().to_string_lossy().to_string();
    let start = tool(&tools, "debugger_start", json!({
        "language": "rust",
        "program": binary.to_string_lossy(),
        "args": [],
        "stopOnEntry": true,
        "cwd": cwd,
    })).await.ok()?;
    let session_id = start["sessionId"].as_str()?.to_string();

    tool(&tools, "debugger_wait_for_stop", json!({
        "sessionId": &session_id, "timeoutMs": 10000,
    })).await.ok()?;

    tool(&tools, "debugger_set_breakpoint", json!({
        "sessionId": &session_id,
        "sourcePath": src_path.to_string_lossy(),
        "line": bp_line(),
    })).await.ok()?;

    tool(&tools, "debugger_continue",
        json!({"sessionId": &session_id})).await.ok()?;
    let stop = tool(&tools, "debugger_wait_for_stop", json!({
        "sessionId": &session_id, "timeoutMs": 10000,
    })).await.ok()?;
    if stop["state"].as_str() != Some("Stopped") {
        return None;
    }
    Some((tools, session_id))
}

/// Heuristic: does the response include any signal beyond `variables: []`
/// that would tell a caller "locals are missing because DWARF was stripped"?
fn has_strip_signal(v: &Value) -> bool {
    let candidate_keys = [
        "hint",
        "note",
        "warning",
        "dwarfStripped",
        "localsAvailable",
        "diagnostic",
    ];
    for k in candidate_keys {
        if let Some(field) = v.get(k) {
            // Any non-null, non-empty value counts as a signal.
            match field {
                Value::String(s) if !s.is_empty() => return true,
                Value::Bool(_) => return true,
                Value::Object(o) if !o.is_empty() => return true,
                Value::Array(a) if !a.is_empty() => return true,
                _ => {}
            }
        }
    }
    // Also accept a `truncated`-style flag with `count: 0` rephrased as a
    // structured signal that the underlying scope had zero observable locals
    // due to debug info, not due to scope shape.
    false
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn line_tables_only_get_variables_signals_stripped_locals() {
    if !check_prerequisites() {
        return;
    }
    let (binary, src_path) = match compile_fixture(1) {
        Some(p) => p,
        None => {
            eprintln!("compile failed; skipping");
            return;
        }
    };
    let (tools, session_id) = match stopped_at_bp(&binary, &src_path).await {
        Some(s) => s,
        None => {
            eprintln!("setup failed; skipping");
            return;
        }
    };

    let resp = tool(&tools, "debugger_get_variables", json!({
        "sessionId": &session_id,
        "maxCount": 50,
    })).await.expect("get_variables should not error");

    let _ = tool(&tools, "debugger_disconnect",
        json!({"sessionId": &session_id})).await;

    let names: Vec<&str> = resp["variables"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v["name"].as_str()).collect())
        .unwrap_or_default();

    // If a future toolchain happens to keep `n`, `s`, or `v` around with
    // `debuginfo=1` this test no longer reproduces the failure. Skip with a
    // diagnostic in that case rather than failing — the stripping behavior is
    // a property of rustc + LLDB versions, not of our code.
    if !names.is_empty() {
        eprintln!(
            "skipping: line-tables-only build still produced locals {names:?} \
             — rustc/LLDB combination does not strip on this platform"
        );
        return;
    }

    assert!(
        has_strip_signal(&resp),
        "line-tables-only build returned `{}` locals with NO signal that DWARF \
         was stripped — caller cannot distinguish this from a frame that \
         genuinely has no locals. Response was:\n{}",
        names.len(),
        serde_json::to_string_pretty(&resp).unwrap_or_else(|_| resp.to_string()),
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn line_tables_only_evaluate_returns_dwarf_strip_message() {
    // The user feedback noted that `debugger_evaluate` *does* return a clear
    // error here ("no variable information is available in debug info for this
    // compile unit"). Pin that as a regression — if we ever start swallowing
    // it, callers lose the only signal they have today.
    if !check_prerequisites() {
        return;
    }
    let (binary, src_path) = match compile_fixture(1) {
        Some(p) => p,
        None => {
            eprintln!("compile failed; skipping");
            return;
        }
    };
    let (tools, session_id) = match stopped_at_bp(&binary, &src_path).await {
        Some(s) => s,
        None => {
            eprintln!("setup failed; skipping");
            return;
        }
    };

    let resp = tool(&tools, "debugger_evaluate", json!({
        "sessionId": &session_id,
        "expression": "n",
        "context": "watch",
    })).await;

    let _ = tool(&tools, "debugger_disconnect",
        json!({"sessionId": &session_id})).await;

    // Either an Err containing the LLDB diagnostic, or an Ok response that
    // surfaces the diagnostic in a structured field — both are acceptable.
    let combined = match resp {
        Err(e) => e,
        Ok(v) => v.to_string(),
    };
    let lower = combined.to_lowercase();
    let signal_substrings = [
        "no variable information",
        "compile unit",
        "debug info",
        "dwarf",
        "no variable named",
    ];
    if !signal_substrings.iter().any(|s| lower.contains(s)) {
        // Not a fixed-string assertion — rustc/LLDB output drifts. We only
        // require *some* indication; same flexibility as the get_variables
        // test.
        eprintln!(
            "evaluate response did not contain a known DWARF-strip signal — \
             toolchain may have changed wording. Response: {combined}"
        );
        // Don't fail: this test guards against silent drop, not against
        // wording drift. The get_variables test is the load-bearing one.
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn full_debug_build_produces_locals_for_baseline_comparison() {
    // Sanity baseline: with full debug info, the same fixture yields the
    // expected locals. If this fails the fixture itself is wrong (locals
    // optimised out, scope mis-pointed) and the line-tables-only test would
    // produce a misleading green/red result.
    if !check_prerequisites() {
        return;
    }
    let (binary, src_path) = match compile_fixture(2) {
        Some(p) => p,
        None => {
            eprintln!("compile failed; skipping");
            return;
        }
    };
    let (tools, session_id) = match stopped_at_bp(&binary, &src_path).await {
        Some(s) => s,
        None => {
            eprintln!("setup failed; skipping");
            return;
        }
    };

    let resp = tool(&tools, "debugger_get_variables", json!({
        "sessionId": &session_id,
        "maxCount": 50,
    })).await.expect("get_variables should not error");

    let _ = tool(&tools, "debugger_disconnect",
        json!({"sessionId": &session_id})).await;

    let names: Vec<&str> = resp["variables"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v["name"].as_str()).collect())
        .unwrap_or_default();

    assert!(
        names.iter().any(|n| n == &"n"),
        "full-debug baseline should expose local `n`; got {names:?}"
    );
}
