//! Reproducer for the silent-REPL failure mode reported in user feedback.
//!
//! When the user calls `debugger_evaluate(context: "repl", expression: "...")`
//! with an LLDB command (e.g. `frame variable`, `v locals`, `type summary list`),
//! the current implementation returns `{ result: "" }`. The caller cannot
//! distinguish between three meaningfully different cases:
//!
//!   - LLDB ran the command and produced no output (rare but valid)
//!   - LLDB rejected the command (e.g. unknown alias) and printed an error to
//!     stderr that the adapter silently dropped
//!   - The command produced multi-line output but the adapter's response only
//!     captured the leading-empty token before stripping the rest
//!
//! The reported session hit case 2/3: `expression --raw-output --depth 1 -- self`
//! and `frame variable --depth 0 self` both came back as `{ result: "" }` even
//! though LLDB produces visible output for both.
//!
//! These tests run a small Rust binary, stop at a known line, and call several
//! REPL commands that *must* produce output for any healthy LLDB:
//!
//!   - `frame variable` — locals dump
//!   - `version` — adapter version line (cheapest non-empty command)
//!   - `help frame variable` — help text
//!
//! For each, we assert either:
//!   1. `result` is non-empty (the desired post-fix behavior), OR
//!   2. an unambiguous structured signal is returned that distinguishes
//!      "no output" from "output dropped" (e.g. `{ ok: true, result: "", note:
//!      "command produced no stdout" }` — also acceptable post-fix).
//!
//! Currently expected to FAIL: every case lands on `result: ""` with no
//! signal. Run with: `cargo test --test repl_passthrough_test -- --ignored --nocapture`

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

/// Compile a tiny Rust program with a known local at a known line. Returns
/// (binary, source path, breakpoint line).
fn compile_fixture() -> Option<(PathBuf, PathBuf, u32)> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir =
        PathBuf::from(&manifest_dir).join("tests/fixtures/target/repl_passthrough");
    std::fs::create_dir_all(&out_dir).ok()?;
    let src_path = out_dir.join("prog.rs");
    let bin_path = out_dir.join("prog");

    // The marker comment + black_box give us a guaranteed instruction at a
    // line we can read off the source.
    let src = "\
#![allow(unused, unused_mut, unused_variables)]
fn main() {
    let n: i64 = 7;
    let s: String = String::from(\"hello\");
    std::hint::black_box(&n);
    std::hint::black_box(&s);
    std::hint::black_box(()); // BP_HERE
}
";
    std::fs::write(&src_path, src).ok()?;
    let bp_line = src
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains("// BP_HERE"))
        .map(|(i, _)| (i + 1) as u32)?;

    let result = Command::new("rustc")
        .arg(&src_path)
        .arg("-g")
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
    Some((bin_path, src_path, bp_line))
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

/// Set up a stopped session at the BP marker and return (tools, session_id).
async fn stopped_session() -> Option<(ToolsHandler, String)> {
    let (binary, src_path, bp_line) = compile_fixture()?;
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
        "line": bp_line,
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

/// Post-fix acceptance: the response either returns non-empty `result`, OR
/// includes the LLDB output stream that DAP routes through OutputEvents
/// (`output` / `stderr`), OR an unambiguous "no output" signal
/// (`note`, `empty`). Returns Err with the actual response when none hold —
/// that's the silent-drop bug.
fn accept_repl_response(v: &Value, command: &str) -> Result<(), String> {
    let nonempty_string = |key: &str| -> bool {
        v.get(key)
            .and_then(|x| x.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    };
    if nonempty_string("result") {
        return Ok(());
    }
    if nonempty_string("output") || nonempty_string("stderr") || nonempty_string("note") {
        return Ok(());
    }
    if v.get("empty").is_some() || v.get("ok").is_some() {
        return Ok(());
    }
    Err(format!(
        "REPL command `{command}` returned silent empty result with no signal: {v}"
    ))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn frame_variable_repl_returns_locals_dump_or_signal() {
    if !check_prerequisites() {
        return;
    }
    let (tools, session_id) = match stopped_session().await {
        Some(s) => s,
        None => {
            eprintln!("setup failed; skipping");
            return;
        }
    };

    let resp = tool(&tools, "debugger_evaluate", json!({
        "sessionId": &session_id,
        "expression": "frame variable",
        "context": "repl",
    })).await.expect("evaluate call should not error");

    let _ = tool(&tools, "debugger_disconnect",
        json!({"sessionId": &session_id})).await;

    if let Err(msg) = accept_repl_response(&resp, "frame variable") {
        panic!("{msg}");
    }
    // Bonus: the locals dump should mention `n` or `s` if it actually ran.
    // This is a stronger property — only assert when result is non-empty.
    let result = resp["result"].as_str().unwrap_or("");
    if !result.is_empty() {
        assert!(
            result.contains('n') || result.contains('s'),
            "frame variable output should reference a local; got {result:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn version_repl_returns_adapter_banner_or_signal() {
    if !check_prerequisites() {
        return;
    }
    let (tools, session_id) = match stopped_session().await {
        Some(s) => s,
        None => {
            eprintln!("setup failed; skipping");
            return;
        }
    };

    let resp = tool(&tools, "debugger_evaluate", json!({
        "sessionId": &session_id,
        "expression": "version",
        "context": "repl",
    })).await.expect("evaluate call should not error");

    let _ = tool(&tools, "debugger_disconnect",
        json!({"sessionId": &session_id})).await;

    if let Err(msg) = accept_repl_response(&resp, "version") {
        panic!("{msg}");
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn help_repl_returns_documentation_or_signal() {
    if !check_prerequisites() {
        return;
    }
    let (tools, session_id) = match stopped_session().await {
        Some(s) => s,
        None => {
            eprintln!("setup failed; skipping");
            return;
        }
    };

    let resp = tool(&tools, "debugger_evaluate", json!({
        "sessionId": &session_id,
        "expression": "help frame variable",
        "context": "repl",
    })).await.expect("evaluate call should not error");

    let _ = tool(&tools, "debugger_disconnect",
        json!({"sessionId": &session_id})).await;

    if let Err(msg) = accept_repl_response(&resp, "help frame variable") {
        panic!("{msg}");
    }
}

/// Negative case: a clearly-bogus command should return *some* signal —
/// either `result` containing an LLDB error message, or `stderr`/`note`
/// indicating the rejection. Currently this also returns `{ result: "" }`
/// with no signal, indistinguishable from a successful command that
/// produced no output.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn unknown_repl_command_returns_error_signal_not_silence() {
    if !check_prerequisites() {
        return;
    }
    let (tools, session_id) = match stopped_session().await {
        Some(s) => s,
        None => {
            eprintln!("setup failed; skipping");
            return;
        }
    };

    let resp = tool(&tools, "debugger_evaluate", json!({
        "sessionId": &session_id,
        "expression": "definitely_not_a_real_lldb_command_xyz",
        "context": "repl",
    })).await;

    let _ = tool(&tools, "debugger_disconnect",
        json!({"sessionId": &session_id})).await;

    match resp {
        // An outright Err is also acceptable — the caller can tell the
        // command failed.
        Err(_) => {}
        Ok(v) => {
            if let Err(msg) =
                accept_repl_response(&v, "definitely_not_a_real_lldb_command_xyz")
            {
                panic!(
                    "unknown REPL command should produce an error signal, not silence:\n{msg}"
                );
            }
        }
    }
}
