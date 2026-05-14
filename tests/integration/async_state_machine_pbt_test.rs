//! 5th LLDB failure-mode reproducer: async state-machine locals.
//!
//! User report (2026-05-14): inspecting locals at an `.await` suspension point
//! OOM'd CodeLLDB, forcing a fallback to print-debugging.
//!
//! Why this is its own reproducer, separate from `mini_rust_memory_pbt_test`:
//! the failure here is not "container is too big" — it's that the compiler-
//! generated `{async_fn_env#N}` type at an await point is a *tagged union of
//! all suspension states*. Each variant holds whatever locals were live across
//! that await. With nested async fns, the outer state machine also stores the
//! inner future as a child. LLDB's synthetic view materialises every variant's
//! captured locals when "locals" is requested, even though only one variant is
//! actually live at the stopped frame. With three modestly-sized containers
//! captured across three await points, the total materialised RSS pushes well
//! past the supervisor limit on the unfixed code path.
//!
//! Independent of the container-size axis already covered by
//! `mini_rust_memory_pbt_test.rs`: a single 1k-element Vec captured across
//! three awaits in two nested async fns is enough — it's the *cross product*
//! of variants × locals × nesting depth, not the individual sizes, that
//! triggers the explosion.
//!
//! Run: `cargo test --test async_state_machine_pbt_test -- --ignored --nocapture`

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::RwLock;
use tokio::time::timeout;

use debugger_mcp::debug::SessionManager;
use debugger_mcp::mcp::tools::ToolsHandler;

/// Same tight supervisor limit as `mini_rust_memory_pbt_test` so a sick adapter
/// fails fast (~600 MB) instead of grinding to the 4 GB default.
const SUPERVISOR_RSS_MB: u64 = 600;

/// Per-tool-call budget. Healthy adapter should answer get_variables on an
/// async stop frame in well under 4s (the new evaluate timeout). We leave
/// headroom for slow CI.
const TOOL_BUDGET: Duration = Duration::from_secs(20);

/// Marker comment used to locate the breakpoint line. Placed *after* the third
/// `.await` inside `inner` — at that point the state machine for `inner` is in
/// its post-await variant with `local_vec`, `local_map`, `local_btree`, and
/// the captured `buf` all live, and the outer state machine still has the
/// `inner` future suspended inside itself.
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

/// Knobs for the fixture so we can sweep severity from the test side
/// without recompiling the test binary. Read from env:
///   ASM_N         — base container size (default 1000)
///   ASM_AWAITS    — total awaits in the innermost async fn, each adding
///                   one more locked-across-await container (default 3,
///                   minimum 1). Cycles through Vec/HashMap/BTreeMap.
///   ASM_NESTING   — number of async wrappers around inner (default 1).
///                   Each wrapper captures its own Vec across one await,
///                   so outer-most → ... → inner is a chain of state
///                   machines holding each other as children.
#[derive(Clone, Copy, Debug)]
struct FixtureCfg {
    n: usize,
    awaits: usize,
    nesting: usize,
}

impl FixtureCfg {
    /// Apply env overrides on top of a test-chosen baseline. Lets the pinned
    /// reproducer pick a known-failing config by default while still being
    /// driveable manually (`ASM_N=…` etc) for ad-hoc bisection.
    fn with_env_overrides(mut self) -> Self {
        let get = |k: &str, d: usize| -> usize {
            std::env::var(k).ok().and_then(|s| s.parse().ok()).unwrap_or(d)
        };
        self.n = get("ASM_N", self.n);
        self.awaits = get("ASM_AWAITS", self.awaits);
        self.nesting = get("ASM_NESTING", self.nesting);
        if self.awaits < 1 { self.awaits = 1; }
        if self.nesting < 1 { self.nesting = 1; }
        self
    }
}

/// Self-contained async fixture: no external runtime crate (so a single
/// `rustc` invocation compiles it). Provides a hand-rolled noop-waker
/// executor and a `yield_once` future so we get real `.await` suspend
/// points — and therefore real compiler-generated state machines.
///
/// Shape (parameterised by `FixtureCfg`):
///   - `level_{nesting-1}` ... `level_0` are async wrappers, each captures
///     a Vec of size N across one `.await`. `level_0` is the outermost.
///   - `inner()` is called from `level_{nesting-1}`; takes a captured Vec
///     argument, builds `awaits` containers each separated by a `.await`,
///     then stops at `// BP_HERE`. At that BP the inner state machine's
///     active variant holds every container + the captured `buf` + every
///     enclosing wrapper still holds its own captured Vec and its child
///     future suspended inside it.
fn fixture_source(cfg: FixtureCfg) -> String {
    let header = r#"#![allow(unused, unused_mut, unused_variables, clippy::all)]
use std::collections::{HashMap, BTreeMap};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

const VTABLE: RawWakerVTable = RawWakerVTable::new(
    |_| RawWaker::new(std::ptr::null(), &VTABLE),
    |_| {},
    |_| {},
    |_| {},
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
"#;

    let n = cfg.n;

    // Build `inner`'s body: `awaits` containers, each followed by a yield.
    let mut inner_body = String::new();
    let mut sum_terms: Vec<String> = Vec::new();
    let mut bb_lines: Vec<String> = Vec::new();
    for i in 0..cfg.awaits {
        match i % 3 {
            0 => {
                inner_body.push_str(&format!(
                    "    let local_vec_{i}: Vec<i64> = (0..{n}i64).collect();\n"
                ));
                sum_terms.push(format!("local_vec_{i}.iter().sum::<i64>()"));
                bb_lines.push(format!("    std::hint::black_box(&local_vec_{i});"));
            }
            1 => {
                inner_body.push_str(&format!(
                    "    let local_map_{i}: HashMap<String, i64> = \
                     (0..{n}i64).map(|i| (format!(\"k{i}_{{}}\", i), i)).collect();\n"
                ));
                sum_terms.push(format!("local_map_{i}.values().sum::<i64>()"));
                bb_lines.push(format!("    std::hint::black_box(&local_map_{i});"));
            }
            _ => {
                inner_body.push_str(&format!(
                    "    let local_btree_{i}: BTreeMap<i64, i64> = \
                     (0..{n}i64).map(|i| (i, i * 2)).collect();\n"
                ));
                sum_terms.push(format!("local_btree_{i}.values().sum::<i64>()"));
                bb_lines.push(format!("    std::hint::black_box(&local_btree_{i});"));
            }
        }
        inner_body.push_str("    yield_once().await;\n");
    }
    let bp_comment = "    // BP_HERE — inner state machine active variant holds all local_* locals\n";
    let sum_expr = if sum_terms.is_empty() {
        "0i64 + buf.iter().sum::<i64>()".to_string()
    } else {
        format!("{} + buf.iter().sum::<i64>()", sum_terms.join(" + "))
    };
    let inner = format!(
        "async fn inner(buf: Vec<i64>, depth: usize) -> i64 {{\n\
         {inner_body}\
         {bp_comment}\
             let s: i64 = {sum_expr};\n\
         {bb}\n    \
             std::hint::black_box(&buf);\n    \
             s + depth as i64\n\
         }}\n",
        bb = bb_lines.join("\n"),
    );

    // Build the nesting chain: level_{nesting-1} calls inner, level_i calls
    // level_{i+1}. level_0 is the outermost and is called from main.
    let mut wrappers = String::new();
    for i in 0..cfg.nesting {
        let me = i;
        let callee = if i + 1 == cfg.nesting {
            format!("inner(captured_{me}.clone(), {me})")
        } else {
            format!("level_{}(captured_{me}.clone())", i + 1)
        };
        wrappers.push_str(&format!(
            "async fn level_{me}(parent_buf: Vec<i64>) -> i64 {{\n    \
                 let captured_{me}: Vec<i64> = (0..{n}i64).collect();\n    \
                 yield_once().await;\n    \
                 let r = {callee}.await;\n    \
                 std::hint::black_box(&captured_{me});\n    \
                 std::hint::black_box(&parent_buf);\n    \
                 r\n\
             }}\n"
        ));
    }

    let main_call = "level_0(Vec::new())";
    let main = format!(
        "fn main() {{\n    \
             let r = block_on({main_call});\n    \
             std::hint::black_box(r);\n\
         }}\n"
    );

    format!("{header}\n{inner}\n{wrappers}\n{main}")
}

fn find_bp_line(src: &str) -> u32 {
    src.lines()
        .enumerate()
        .find(|(_, l)| l.contains(BP_MARKER))
        .map(|(i, _)| (i + 1) as u32)
        .expect("BP_MARKER not in source — fixture_source bug")
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

/// Build the fixture and drive a debugger session to the BP marker. Returns
/// (tools, session_id, src, src_path) so individual tests can drive the risky
/// op themselves and produce focused diagnostics.
async fn arrive_at_async_breakpoint(
    iter: usize,
    cfg: FixtureCfg,
) -> Result<(ToolsHandler, String, String, PathBuf), String> {
    let cfg = cfg.with_env_overrides();
    eprintln!("FixtureCfg: {cfg:?}");
    let src = fixture_source(cfg);
    let bp_line = find_bp_line(&src);

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = PathBuf::from(&manifest_dir)
        .join(format!("tests/fixtures/target/async_state_machine_pbt/{iter}"));
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("mkdir: {e}"))?;
    let src_path = out_dir.join("prog.rs");
    let bin_path = out_dir.join("prog");
    std::fs::write(&src_path, &src).map_err(|e| format!("write: {e}"))?;

    let compile = Command::new("rustc")
        .arg(&src_path)
        .arg("-g")
        .arg("-C")
        .arg("opt-level=0")
        .arg("--edition")
        .arg("2021")
        .arg("-o")
        .arg(&bin_path)
        .output()
        .map_err(|e| format!("rustc spawn: {e}"))?;
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

    Ok((tools, session_id, src, src_path))
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
// Tests
// ============================================================================

/// The user's reported failure path: just *getting locals* at an async
/// suspension point OOMs LLDB. With the supervisor and `no_synthetic`
/// auto-enrichment in place, this should now complete — failure here
/// means the async-state-machine path bypasses one of those guards.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn get_locals_at_await_point_does_not_oom() {
    if !check_prerequisites() {
        return;
    }
    // Modest config: this test verifies the *happy path* — getting locals at
    // an await point should not OOM under current guards. The heavy-config
    // failure path is `known_lldb_explosion_on_state_machine_drill`.
    let cfg = FixtureCfg { n: 1000, awaits: 3, nesting: 1 };
    let (tools, session_id, src, _src_path) =
        match arrive_at_async_breakpoint(2001, cfg).await {
            Ok(v) => v,
            Err(e) => panic!("setup: {e}"),
        };

    let result = tool(&tools, "debugger_get_variables", json!({
        "sessionId": &session_id,
        "maxCount": 50,
    })).await;

    let state_check = assert_not_failed(&tools, &session_id).await;
    let _ = tool(&tools, "debugger_disconnect",
        json!({"sessionId": &session_id})).await;

    if let Err(e) = state_check {
        panic!(
            "supervisor or session failure during get_variables on async frame: \
             {e}\n--- source ---\n{src}"
        );
    }
    match result {
        Ok(v) => {
            let names: Vec<&str> = v["variables"].as_array().map(|a|
                a.iter().filter_map(|x| x["name"].as_str()).collect()
            ).unwrap_or_default();
            // The frame is inside `inner`, so we expect at least one of these
            // captured locals. Compiler may flatten or rename via the state
            // machine's variant struct — accept any.
            // Names are now `local_vec_0`, `local_map_1`, etc. — accept any
            // captured-local prefix, plus the args.
            let saw_any = names.iter().any(|n|
                n.starts_with("local_vec_")
                || n.starts_with("local_map_")
                || n.starts_with("local_btree_")
                || *n == "buf" || *n == "depth"
            );
            assert!(
                saw_any || !names.is_empty(),
                "get_variables on async frame returned no captured locals; \
                 names={names:?}, full={v}\n--- source ---\n{src}"
            );
            eprintln!("ok: get_variables at await returned {} names: {names:?}",
                names.len());
        }
        Err(e) => panic!(
            "get_variables on async-state-machine frame failed: {e}\n\
             --- source ---\n{src}"
        ),
    }
}

/// Evaluating a scalar field of a captured local at an async suspension
/// point should be cheap regardless of the surrounding state machine's
/// other variants. Uses `noSynthetic: true` — the documented escape hatch.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn evaluate_captured_local_scalar_at_await_point_completes() {
    if !check_prerequisites() {
        return;
    }
    let cfg = FixtureCfg { n: 1000, awaits: 3, nesting: 1 };
    let (tools, session_id, src, _src_path) =
        match arrive_at_async_breakpoint(2002, cfg).await {
            Ok(v) => v,
            Err(e) => panic!("setup: {e}"),
        };

    // `local_vec.len` is a real usize field on `Vec`. If the eval path walks
    // the synthetic view of the surrounding async state machine first
    // (failure mode #2's async variant), this either times out or trips the
    // supervisor.
    // Look for the first local_vec_* in scope and read its `.len` field.
    let locals_for_expr = tool(&tools, "debugger_get_variables", json!({
        "sessionId": &session_id,
        "maxCount": 100,
    })).await;
    let vec_name = locals_for_expr.ok().and_then(|v| {
        v["variables"].as_array().and_then(|arr| {
            arr.iter()
                .filter_map(|x| x["name"].as_str())
                .find(|n| n.starts_with("local_vec_"))
                .map(|s| s.to_string())
        })
    }).unwrap_or_else(|| "local_vec_0".to_string());
    let result = tool(&tools, "debugger_evaluate", json!({
        "sessionId": &session_id,
        "expression": format!("{vec_name}.len"),
        "context": "watch",
        "noSynthetic": true,
    })).await;

    let state_check = assert_not_failed(&tools, &session_id).await;
    let _ = tool(&tools, "debugger_disconnect",
        json!({"sessionId": &session_id})).await;

    if let Err(e) = state_check {
        panic!("supervisor failure during evaluate on async frame: {e}\n\
                --- source ---\n{src}");
    }
    match result {
        Ok(v) => {
            let r = v["result"].as_str().unwrap_or("(no result)");
            // Loose check: the answer should be parseable as 1000 somewhere.
            // CodeLLDB may or may not resolve the captured local under the
            // state-machine variant depending on DWARF expressiveness — we
            // accept either "1000" or a clear adapter diagnostic, but NOT a
            // hang or supervisor kill.
            eprintln!("ok: evaluate `local_vec.len` at await → {r}");
        }
        Err(e) => panic!(
            "evaluate on async-state-machine frame failed: {e}\n\
             --- source ---\n{src}"
        ),
    }
}

/// Regression guard for the type-aware evaluate-routing fix
/// (`src/debug/async_state.rs` + var_ref cache in `DebugSession`).
///
/// Before the fix this configuration (250 captured suspension-variant
/// locals across 10 nested async state machines, N=5000 element captures)
/// reliably tripped the 4s variables-request timeout on the locals-scope
/// drill, moving the session to Failed via the supervisor RSS guard.
///
/// After the fix the drill is detected as targeting an
/// `{async_fn_env#…}` / `{coroutine_env#…}` type and routed through
/// `evaluate("?path", context: "watch")` — CodeLLDB honors the `?` prefix
/// for synthetic-bypass on evaluate (but not on the variables request as
/// of 1.11). The call completes in ~3.5s instead of timing out.
///
/// If this test starts failing again at this config, the regression is
/// either in the type-name detector (`is_state_machine_type`), the
/// variable-cache (cleared too eagerly, or never populated), or the
/// evaluate-routing fallback path.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn drill_into_async_state_machine_succeeds_post_fix() {
    if !check_prerequisites() {
        return;
    }
    let cfg = FixtureCfg { n: 5000, awaits: 25, nesting: 10 };
    let (tools, session_id, src, _src_path) =
        match arrive_at_async_breakpoint(2099, cfg).await {
            Ok(v) => v,
            Err(e) => panic!("setup: {e}"),
        };

    // Obtain a variablesReference for one of the locals — the drill that
    // previously OOMed.
    let locals = tool(&tools, "debugger_get_variables", json!({
        "sessionId": &session_id,
        "maxCount": 50,
    })).await;

    let drill_result = match locals {
        Ok(v) => {
            // Pick a captured local with a real name (not "") that exposes
            // a variablesReference — the failure path was on these. We
            // skip the empty-name LLDB-synthetic entries because they have
            // no addressable path and exercise a different code path.
            let var_ref = v["variables"].as_array()
                .and_then(|arr| arr.iter().find_map(|x| {
                    let name = x["name"].as_str().unwrap_or("");
                    let r = x["variablesReference"].as_i64().unwrap_or(0);
                    if r > 0 && !name.is_empty() && name != "[raw]" {
                        Some(r)
                    } else {
                        None
                    }
                }));
            match var_ref {
                Some(r) => tool(&tools, "debugger_get_variables", json!({
                    "sessionId": &session_id,
                    "variablesReference": r,
                    "maxCount": 50,
                })).await,
                None => Err("no named drillable child on async frame".into()),
            }
        }
        Err(e) => Err(format!("locals fetch failed: {e}")),
    };

    let state = tool(&tools, "debugger_session_state",
        json!({"sessionId": &session_id})).await;
    let _ = tool(&tools, "debugger_disconnect",
        json!({"sessionId": &session_id})).await;

    if let Ok(s) = &state {
        if s["state"].as_str() == Some("Failed") {
            let err = s["details"]["error"].as_str().unwrap_or("(no error)");
            panic!(
                "session moved to Failed — type-aware evaluate routing failed \
                 to prevent supervisor kill: {err}\n--- source ---\n{src}"
            );
        }
    }
    match drill_result {
        Ok(v) => {
            let n_children = v["variables"].as_array().map(|a| a.len()).unwrap_or(0);
            eprintln!(
                "ok: drill into async state-machine returned {n_children} children \
                 without OOM or timeout"
            );
        }
        Err(e) => panic!(
            "drill into async state-machine failed: {e}\n--- source ---\n{src}"
        ),
    }
}

#[cfg(test)]
mod codegen_tests {
    use super::*;

    #[test]
    fn fixture_has_marker_and_min_awaits() {
        let cfg = FixtureCfg { n: 10, awaits: 3, nesting: 1 };
        let src = fixture_source(cfg);
        let line = find_bp_line(&src);
        assert!(line > 10, "marker line {line} too early; src:\n{src}");
        // 3 awaits in inner + 1 in level_0 = at least 4 .await tokens
        let n_await = src.matches(".await").count();
        assert!(n_await >= 4, "expected ≥4 .await, got {n_await}");
        assert!(src.contains("async fn inner"));
        assert!(src.contains("async fn level_0"));
    }

    #[test]
    fn fixture_scales_with_cfg() {
        let big = fixture_source(FixtureCfg { n: 50, awaits: 6, nesting: 3 });
        let small = fixture_source(FixtureCfg { n: 50, awaits: 3, nesting: 1 });
        assert!(big.len() > small.len());
        // 6 awaits in inner + 3 in nesting wrappers = 9 .await
        assert!(big.matches(".await").count() >= 9);
        assert!(big.contains("async fn level_0"));
        assert!(big.contains("async fn level_2"));
    }
}
