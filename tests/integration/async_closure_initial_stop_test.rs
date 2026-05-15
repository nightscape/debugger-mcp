//! 6th LLDB failure-mode reproducer: async-closure frame initial-stop hang.
//!
//! User report (2026-05-15): stopping inside an `async fn`'s auto-generated
//! state-machine closure where the captured locals include a recursive
//! `Value::Object(HashMap<String, Value>)` (depth 3, fanout 4) plus several
//! other sibling captures (`&mut self` with nested HashMaps, a `HashSet<String>`,
//! a `Vec<HashMap<String, Value>>`) caused the very first locals fetch to hang
//! ≥10s. The supervisor eventually killed the adapter and the user saw
//! "Debug adapter exited" — misleading, since the adapter had been stuck for
//! seconds before the kill.
//!
//! Why this is its own reproducer, separate from `async_state_machine_pbt_test`:
//! that earlier test exercises the *drill* path (caller asks for a state-
//! machine var_ref's children) and verifies the 2026-05-14 type-aware
//! evaluate-routing fix. This one exercises the *initial enrichment* path —
//! the locals scope is read once on every stop in `build_stop_context`, and
//! even with `format.showRaw=true` CodeLLDB walks each captured container's
//! synthetic provider when computing per-Variable summary strings.
//!
//! Two fixes apply here:
//!   1. 2026-05-15 closure-specific skip via `is_async_closure_frame` in
//!      `async_state.rs`.
//!   2. 2026-05-15 (same day, second pass) — global default: `wait_for_stop`
//!      never pre-fetches locals unless the caller passes `enrichLocals:
//!      true`. The closure-frame `hint` is more specific than the generic
//!      one, but both cases now skip locals by default.
//!
//! Run:
//!   cargo test --test async_closure_initial_stop_test -- --ignored --nocapture

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::RwLock;
use tokio::time::timeout;

use debugger_mcp::debug::SessionManager;
use debugger_mcp::mcp::tools::ToolsHandler;

const SUPERVISOR_RSS_MB: u64 = 600;
const TOOL_BUDGET: Duration = Duration::from_secs(20);
const BP_MARKER: &str = "// BP_HERE";

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

/// Self-contained fixture mirroring the holon shape that hit in production:
/// recursive `Value::Object(HashMap<String, Value>)` 3-levels deep × fanout 4,
/// inside an `async fn` closure stopped after the captures are populated.
/// Uses a hand-rolled noop-waker so no external runtime dep is required.
fn fixture_source() -> String {
    r#"#![allow(unused, unused_mut, unused_variables, clippy::all)]
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

const VTABLE: RawWakerVTable = RawWakerVTable::new(
    |_| RawWaker::new(std::ptr::null(), &VTABLE),
    |_| {}, |_| {}, |_| {},
);
fn noop_waker() -> Waker {
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}
fn block_on<F: Future>(mut f: F) -> F::Output {
    let mut f = unsafe { Pin::new_unchecked(&mut f) };
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    loop {
        match f.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => continue,
        }
    }
}
struct YieldOnce(bool);
impl Future for YieldOnce {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        if self.0 { Poll::Ready(()) } else { self.0 = true; Poll::Pending }
    }
}
fn yield_once() -> YieldOnce { YieldOnce(false) }

#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Array(Vec<Value>),
    Object(HashMap<String, Value>),
}

fn nested(depth: usize, fanout: usize) -> Value {
    if depth == 0 {
        Value::String(format!("leaf-{}", depth))
    } else {
        let mut m = HashMap::new();
        for i in 0..fanout {
            m.insert(format!("k{}", i), nested(depth - 1, fanout));
        }
        m.insert(
            "arr".to_string(),
            Value::Array(vec![
                Value::Int(1), Value::Int(2), Value::Int(3),
                Value::String("hello".to_string()),
            ]),
        );
        Value::Object(m)
    }
}

fn build_row(i: u32) -> HashMap<String, Value> {
    let mut h = HashMap::new();
    h.insert("id".to_string(), Value::String(format!("block:{:010x}", i)));
    h.insert("content".to_string(), Value::String("xyz".repeat(4)));
    h.insert("sort_key".to_string(), Value::Float(i as f64 * 1.5));
    h.insert("properties".to_string(), nested(3, 4));
    h
}

#[derive(Debug, Clone)]
pub struct ReferenceState {
    pub blocks: HashMap<String, HashMap<String, Value>>,
    pub history: Vec<String>,
}
impl ReferenceState {
    fn new() -> Self {
        let mut blocks = HashMap::new();
        for i in 0..25u32 {
            blocks.insert(format!("block:{:010x}", i), build_row(i));
        }
        Self { blocks, history: (0..10).map(|i| format!("step-{}", i)).collect() }
    }
}

pub struct E2ESut {
    pub doc_uri_map: HashMap<String, String>,
    pub ref_state_cache: Option<ReferenceState>,
    pub all_blocks: Option<HashMap<String, HashMap<String, Value>>>,
}
impl E2ESut {
    fn new() -> Self {
        Self {
            doc_uri_map: HashMap::new(),
            ref_state_cache: Some(ReferenceState::new()),
            all_blocks: Some(HashMap::from_iter(
                (0..30u32).map(|i| (format!("block:{:010x}", i), build_row(i))),
            )),
        }
    }
    async fn apply_split_block(&mut self, ref_state: &ReferenceState) {
        let expected_ids: HashSet<String> = ref_state.blocks.keys().cloned().collect();
        let expected_count: usize = ref_state.blocks.values().count();
        let timeout = std::time::Duration::from_secs(5);
        let _t = timeout;
        yield_once().await;
        let db_rows: Vec<HashMap<String, Value>> =
            (0..24u32).map(build_row).collect();
        // BP_HERE — inside async-closure frame, captures populated
        std::hint::black_box(&expected_ids);
        std::hint::black_box(&db_rows);
        std::hint::black_box(expected_count);
        assert_eq!(db_rows.len(), expected_count.min(24), "len ok");
    }
}

fn main() {
    let mut sut = E2ESut::new();
    let ref_state = ReferenceState::new();
    block_on(sut.apply_split_block(&ref_state));
}
"#.to_string()
}

fn find_bp_line(src: &str) -> u32 {
    src.lines()
        .enumerate()
        .find(|(_, l)| l.contains(BP_MARKER))
        .map(|(i, _)| (i + 1) as u32)
        .expect("BP_MARKER not in source")
}

async fn tool(tools: &ToolsHandler, name: &str, args: Value) -> Result<Value, String> {
    match timeout(TOOL_BUDGET, tools.handle_tool(name, args)).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(format!("{e}")),
        Err(_) => Err(format!("{name} exceeded {TOOL_BUDGET:?}")),
    }
}

async fn arrive_at_async_breakpoint(
) -> Result<(ToolsHandler, String, String, PathBuf, Value), String> {
    let src = fixture_source();
    let bp_line = find_bp_line(&src);
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = PathBuf::from(&manifest_dir)
        .join("tests/fixtures/target/async_closure_initial_stop");
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("mkdir: {e}"))?;
    let src_path = out_dir.join("prog.rs");
    let bin_path = out_dir.join("prog");
    std::fs::write(&src_path, &src).map_err(|e| format!("write: {e}"))?;

    let compile = Command::new("rustc")
        .arg(&src_path)
        .arg("-g").arg("-C").arg("opt-level=0")
        .arg("--edition").arg("2021")
        .arg("-o").arg(&bin_path)
        .output().map_err(|e| format!("rustc spawn: {e}"))?;
    if !compile.status.success() {
        return Err(format!(
            "rustc failed:\n{}\n--- source ---\n{src}",
            String::from_utf8_lossy(&compile.stderr),
        ));
    }

    let mut manager = SessionManager::new();
    manager.set_supervisor_rss_limit(SUPERVISOR_RSS_MB);
    let sm = Arc::new(RwLock::new(manager));
    let tools = ToolsHandler::new(Arc::clone(&sm));

    let cwd = bin_path.parent().unwrap().to_string_lossy().to_string();
    let start = tool(&tools, "debugger_start", json!({
        "language": "rust",
        "program": bin_path.to_string_lossy(),
        "args": [],
        "stopOnEntry": true,
        "cwd": cwd,
    })).await?;
    let session_id = start["sessionId"].as_str().unwrap().to_string();

    tool(&tools, "debugger_wait_for_stop", json!({
        "sessionId": &session_id, "timeoutMs": 10000,
    })).await?;
    tool(&tools, "debugger_set_breakpoint", json!({
        "sessionId": &session_id,
        "sourcePath": src_path.to_string_lossy(),
        "line": bp_line,
    })).await?;
    tool(&tools, "debugger_continue", json!({"sessionId": &session_id})).await?;
    let stop = tool(&tools, "debugger_wait_for_stop", json!({
        "sessionId": &session_id, "timeoutMs": 15000,
    })).await?;
    if stop["state"].as_str() != Some("Stopped") {
        let _ = tool(&tools, "debugger_disconnect",
            json!({"sessionId": &session_id})).await;
        return Err(format!(
            "did not stop at async BP, state={:?}; bp_line={bp_line}\n--- source ---\n{src}",
            stop["state"]
        ));
    }
    Ok((tools, session_id, src, src_path, stop))
}

async fn assert_not_failed(tools: &ToolsHandler, session_id: &str) -> Result<(), String> {
    let state = tool(tools, "debugger_session_state", json!({
        "sessionId": session_id,
    })).await?;
    if state["state"].as_str() == Some("Failed") {
        let err = state["details"]["error"].as_str().unwrap_or("(no error)");
        return Err(format!("session moved to Failed (likely supervisor kill): {err}"));
    }
    Ok(())
}

// ============================================================================
// Regression guard for the 2026-05-15 skip-locals fix.
//
// `wait_for_stop` returns the enriched stop context. With the fix in place,
// stopping inside an async-closure frame must surface `localVariablesSkipped:
// true` and a `hint` instead of attempting the locals fetch (which would hang
// or OOM CodeLLDB on the recursive `Value::Object(HashMap<String, Value>)`
// captures). Also: the wait_for_stop call itself must complete well inside
// the TOOL_BUDGET, and the session must NOT move to Failed.
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn wait_for_stop_skips_locals_in_async_closure_frame() {
    if !check_prerequisites() {
        return;
    }
    let (tools, session_id, src, _src_path, stop) = match arrive_at_async_breakpoint().await {
        Ok(v) => v,
        Err(e) => panic!("setup: {e}"),
    };

    let state_check = assert_not_failed(&tools, &session_id).await;
    let _ = tool(&tools, "debugger_disconnect",
        json!({"sessionId": &session_id})).await;

    if let Err(e) = state_check {
        panic!("supervisor kill during initial stop in async-closure frame: {e}\n\
                --- source ---\n{src}");
    }

    let top_frame_name = stop["topFrame"]["name"].as_str().unwrap_or("");
    assert!(
        top_frame_name.contains("{closure#") || top_frame_name.contains("{{closure}}"),
        "expected closure frame, got topFrame.name={top_frame_name:?}; full stop={stop}"
    );

    let skipped = stop["localVariablesSkipped"].as_bool().unwrap_or(false);
    let has_hint = stop["hint"].as_str().is_some();
    let has_locals = stop["localVariables"].is_array();

    assert!(
        skipped && has_hint && !has_locals,
        "expected skip-locals signal in async-closure frame: \
         localVariablesSkipped={skipped} has_hint={has_hint} has_locals={has_locals}\n\
         full stop={stop}"
    );
    eprintln!("ok: async-closure frame skipped locals enrichment with hint");
}

// ============================================================================
// Recovery path: after the initial-stop skip, the agent can still drill into
// individual locals explicitly via `debugger_get_variables` (which needs a
// frameId + noSynthetic on Locals scope to avoid the same hang).
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn get_variables_with_no_synthetic_recovers_in_async_closure_frame() {
    if !check_prerequisites() {
        return;
    }
    let (tools, session_id, src, _src_path, stop) = match arrive_at_async_breakpoint().await {
        Ok(v) => v,
        Err(e) => panic!("setup: {e}"),
    };

    let frame_id = stop["topFrame"]["id"].as_i64().unwrap_or(0) as i32;
    let result = tool(&tools, "debugger_get_variables", json!({
        "sessionId": &session_id,
        "frameId": frame_id,
        "maxCount": 30,
        "noSynthetic": true,
    })).await;

    let state_check = assert_not_failed(&tools, &session_id).await;
    let _ = tool(&tools, "debugger_disconnect",
        json!({"sessionId": &session_id})).await;

    if let Err(e) = state_check {
        panic!("supervisor kill during explicit noSynthetic get_variables: {e}\n\
                --- source ---\n{src}");
    }
    match result {
        Ok(v) => {
            let count = v["variables"].as_array().map(|a| a.len()).unwrap_or(0);
            eprintln!("ok: explicit get_variables(noSynthetic:true) returned {count} entries");
        }
        Err(e) => panic!(
            "explicit get_variables on async-closure frame failed: {e}\n--- source ---\n{src}"
        ),
    }
}

// ============================================================================
// Recovery path: when a tool call gets stuck, `debugger_cancel` clears
// in-flight requests without losing breakpoints, and the session keeps
// working. Smoke test that the new MCP tool surface is wired up — the
// "stuck recovery" semantics are covered at the dap::client unit level
// (test_timeout_ping_responsive_classifier).
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn debugger_cancel_is_callable_on_live_session() {
    if !check_prerequisites() {
        return;
    }
    let (tools, session_id, _src, _src_path, _stop) =
        match arrive_at_async_breakpoint().await {
            Ok(v) => v,
            Err(e) => panic!("setup: {e}"),
        };

    let res = tool(&tools, "debugger_cancel",
        json!({"sessionId": &session_id})).await;
    let _ = tool(&tools, "debugger_disconnect",
        json!({"sessionId": &session_id})).await;

    match res {
        Ok(v) => {
            assert!(
                v["cancelled"].is_number(),
                "expected `cancelled` count, got: {v}"
            );
            eprintln!("ok: debugger_cancel returned {v}");
        }
        Err(e) => panic!("debugger_cancel failed: {e}"),
    }
}

// LLDB formatter-limits verification note: `DebugSession::apply_lldb_formatter_limits`
// sends three `settings set` commands after launch. CodeLLDB doesn't surface
// `settings show` output through the channels we capture, so a read-back test
// can't observe the value cleanly. The init path logs each successful apply at
// `info!` level and each rejection at `warn!` level — operators see regressions
// via logs, and the no-OOM regression guards in this test file plus
// `async_state_machine_pbt_test` would surface a behaviour regression if a
// future LLDB silently dropped one of these keys.
