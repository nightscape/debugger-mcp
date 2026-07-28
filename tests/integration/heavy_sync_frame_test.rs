//! Reading small values out of a heavy *synchronous* frame.
//!
//! The fixture mirrors a DBSP circuit: a plain recursive `execute_node(&mut
//! self, node_id: i64, ...)` where `self` owns four large maps and a paged
//! btree-cursor struct, and the frame's own locals include a `DeltaPair` and a
//! `DbspStateCursors`. What the caller wants from it is one `i64` parameter.
//!
//! The guards here cover the four ways that goes wrong:
//!   1. scope-level `debugger_get_variables({frameId})` must not exceed its
//!      budget or get the adapter supervisor-killed,
//!   2. a multi-name `frame variable` through the REPL must return every name
//!      and leave the dispatcher answering,
//!   3. a bare identifier in `context: "repl"` must be rejected with the
//!      working alternatives rather than LLDB's command-interpreter error,
//!   4. `frame variable <name>` must return the value, not an empty response.
//!
//! Why this is separate from the async reproducers: `async_state_machine_pbt_test`
//! covers the drill into a coroutine var_ref and `async_closure_initial_stop_test`
//! covers the auto-enrichment fetch inside a `{{closure}}` frame, and both fixes
//! key off async type/frame-name detection (`src/debug/async_state.rs`). Neither
//! fires on an ordinary `impl` method — here the expense comes purely from the
//! summary strings CodeLLDB computes for `&mut self`'s containers while
//! answering the scope-level `variables` request.
//!
//! Size knobs (raise to bisect):
//!   HSF_OPERATORS  — entries in each of the circuit's operator/edge/state maps
//!   HSF_ROWS       — rows per btree cursor page map
//!   HSF_DELTAS     — elements in the `deltas` local
//!
//! Run:
//!   cargo test --test heavy_sync_frame_test -- --ignored --nocapture

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::RwLock;
use tokio::time::timeout;

use debugger_mcp::debug::SessionManager;
use debugger_mcp::mcp::tools::ToolsHandler;

const SUPERVISOR_RSS_MB: u64 = 600;
const TOOL_BUDGET: Duration = Duration::from_secs(20);
const BP_MARKER: &str = "// BP_HERE";

fn knob(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

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

/// Fixture mirroring a DBSP circuit: a recursive
/// `execute_node(&mut self, node_id: i64, ...)` whose `self` owns four large
/// maps plus a paged btree-cursor struct, and whose own locals include a
/// `DeltaPair` and a `DbspStateCursors`. Everything the caller actually wants
/// is one `i64` parameter sitting next to all of that.
fn fixture_source(operators: usize, rows: usize, deltas: usize) -> String {
    format!(
        r#"#![allow(unused, unused_mut, unused_variables, clippy::all)]
use std::collections::{{BTreeMap, HashMap}};

const OPERATORS: usize = {operators};
const ROWS: usize = {rows};
const DELTAS: usize = {deltas};

#[derive(Debug, Clone)]
pub struct Row {{
    pub rowid: i64,
    pub values: Vec<String>,
}}

#[derive(Debug, Clone)]
pub struct Delta {{
    pub row: Row,
    pub weight: i64,
}}

#[derive(Debug, Clone)]
pub struct DeltaPair {{
    pub left: Vec<Delta>,
    pub right: Vec<Delta>,
}}

/// Stand-in for the paged btree cursors: two maps of page-id -> page bytes.
#[derive(Debug)]
pub struct DbspStateCursors {{
    pub index_pages: BTreeMap<i64, Vec<u8>>,
    pub table_pages: BTreeMap<i64, Vec<u8>>,
}}

#[derive(Debug, Clone)]
pub enum Operator {{
    Input {{ table: String }},
    Filter {{ predicate: String, arity: usize }},
    Join {{ left: i64, right: i64, keys: Vec<String> }},
    Aggregate {{ group_by: Vec<String>, acc: BTreeMap<String, i64> }},
}}

pub struct DbspCircuit {{
    pub operators: HashMap<i64, Operator>,
    pub edges: HashMap<i64, Vec<i64>>,
    pub node_state: HashMap<i64, BTreeMap<i64, Row>>,
    pub zsets: HashMap<String, BTreeMap<i64, i64>>,
    pub cursors: DbspStateCursors,
}}

fn make_row(i: usize) -> Row {{
    Row {{
        rowid: i as i64,
        values: vec![format!("v{{}}", i), "payload".repeat(2)],
    }}
}}

impl DbspCircuit {{
    fn new() -> Self {{
        let mut operators = HashMap::new();
        let mut edges = HashMap::new();
        let mut node_state = HashMap::new();
        let mut zsets = HashMap::new();
        for i in 0..OPERATORS {{
            let id = i as i64;
            operators.insert(id, match i % 4 {{
                0 => Operator::Input {{ table: format!("t{{}}", i) }},
                1 => Operator::Filter {{ predicate: format!("c{{}} > 0", i), arity: i % 7 }},
                2 => Operator::Join {{
                    left: (i as i64) - 1,
                    right: (i as i64) - 2,
                    keys: vec![format!("k{{}}", i)],
                }},
                _ => Operator::Aggregate {{
                    group_by: vec![format!("g{{}}", i)],
                    acc: (0..4).map(|j| (format!("a{{}}", j), (i * j) as i64)).collect(),
                }},
            }});
            edges.insert(id, vec![id + 1, id + 2]);
            let mut st = BTreeMap::new();
            for j in 0..(ROWS.min(8)) {{
                st.insert(j as i64, make_row(i + j));
            }}
            node_state.insert(id, st);
            zsets.insert(
                format!("z{{}}", i),
                (0..4).map(|j| (j as i64, (i as i64) - (j as i64))).collect(),
            );
        }}
        let mut index_pages = BTreeMap::new();
        let mut table_pages = BTreeMap::new();
        for p in 0..ROWS {{
            index_pages.insert(p as i64, vec![(p % 251) as u8; 64]);
            table_pages.insert(p as i64, vec![(p % 253) as u8; 64]);
        }}
        Self {{
            operators,
            edges,
            node_state,
            zsets,
            cursors: DbspStateCursors {{ index_pages, table_pages }},
        }}
    }}

    /// Recursive, like the real `execute_node`. The breakpoint sits where
    /// `node_id` is live alongside the fat `self` and the two local structs.
    fn execute_node(&mut self, node_id: i64, depth: usize) -> i64 {{
        let deltas = DeltaPair {{
            left: (0..DELTAS)
                .map(|i| Delta {{ row: make_row(i), weight: 1 }})
                .collect(),
            right: (0..DELTAS)
                .map(|i| Delta {{ row: make_row(i + 1), weight: -1 }})
                .collect(),
        }};
        let mut cursors = DbspStateCursors {{
            index_pages: (0..ROWS.min(32))
                .map(|p| (p as i64, vec![(p % 199) as u8; 32]))
                .collect(),
            table_pages: (0..ROWS.min(32))
                .map(|p| (p as i64, vec![(p % 197) as u8; 32]))
                .collect(),
        }};
        let mut current_idx: usize = 0;
        let mut weight: i64 = 0;
        for d in deltas.left.iter() {{
            let row = &d.row;
            weight += d.weight;
            current_idx += 1;
            // BP_HERE — node_id, weight, current_idx, row all live next to fat self
            std::hint::black_box(&self.operators);
            std::hint::black_box(&cursors);
            std::hint::black_box(row.rowid);
            if current_idx > 2 {{
                break;
            }}
        }}
        if depth > 0 {{
            let next = (node_id + 1) % (OPERATORS as i64);
            weight += self.execute_node(next, depth - 1);
        }}
        std::hint::black_box(&deltas);
        weight
    }}
}}

fn main() {{
    let mut circuit = DbspCircuit::new();
    let out = circuit.execute_node(3, 2);
    println!("done {{}}", out);
}}
"#
    )
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

struct Arrived {
    tools: ToolsHandler,
    session_id: String,
    src: String,
    frame_id: i32,
}

async fn arrive_at_heavy_frame() -> Result<Arrived, String> {
    let operators = knob("HSF_OPERATORS", 2000);
    let rows = knob("HSF_ROWS", 512);
    let deltas = knob("HSF_DELTAS", 256);
    eprintln!("fixture knobs: operators={operators} rows={rows} deltas={deltas}");

    let src = fixture_source(operators, rows, deltas);
    let bp_line = find_bp_line(&src);
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = PathBuf::from(&manifest_dir).join("tests/fixtures/target/heavy_sync_frame");
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
            "rustc failed:\n{}",
            String::from_utf8_lossy(&compile.stderr)
        ));
    }

    let mut manager = SessionManager::new();
    manager.set_supervisor_rss_limit(SUPERVISOR_RSS_MB);
    let sm = Arc::new(RwLock::new(manager));
    let tools = ToolsHandler::new(Arc::clone(&sm));

    let cwd = bin_path.parent().unwrap().to_string_lossy().to_string();
    let start = tool(
        &tools,
        "debugger_start",
        json!({
            "language": "rust",
            "program": bin_path.to_string_lossy(),
            "args": [],
            "stopOnEntry": true,
            "cwd": cwd,
        }),
    )
    .await?;
    let session_id = start["sessionId"].as_str().unwrap().to_string();

    tool(
        &tools,
        "debugger_wait_for_stop",
        json!({"sessionId": &session_id, "timeoutMs": 10000}),
    )
    .await?;
    tool(
        &tools,
        "debugger_set_breakpoint",
        json!({
            "sessionId": &session_id,
            "sourcePath": src_path.to_string_lossy(),
            "line": bp_line,
        }),
    )
    .await?;
    tool(&tools, "debugger_continue", json!({"sessionId": &session_id})).await?;
    let stop = tool(
        &tools,
        "debugger_wait_for_stop",
        json!({"sessionId": &session_id, "timeoutMs": 20000}),
    )
    .await?;
    if stop["state"].as_str() != Some("Stopped") {
        let _ = tool(
            &tools,
            "debugger_disconnect",
            json!({"sessionId": &session_id}),
        )
        .await;
        return Err(format!(
            "did not stop at BP line {bp_line}, state={:?}",
            stop["state"]
        ));
    }

    let frame_id = stop["topFrame"]["id"].as_i64().unwrap_or(0) as i32;
    let frame_name = stop["topFrame"]["name"].as_str().unwrap_or("").to_string();
    // Guard the premise: if this stopped in a closure/coroutine frame the
    // existing async fixes would apply and the reproducer would be testing
    // something else.
    if frame_name.contains("closure") || frame_name.contains("coroutine") {
        let _ = tool(
            &tools,
            "debugger_disconnect",
            json!({"sessionId": &session_id}),
        )
        .await;
        return Err(format!(
            "expected a plain sync frame, stopped in {frame_name:?}"
        ));
    }
    eprintln!("stopped in frame {frame_name:?} (id={frame_id})");

    Ok(Arrived {
        tools,
        session_id,
        src,
        frame_id,
    })
}

async fn session_failed(tools: &ToolsHandler, session_id: &str) -> Option<String> {
    let state = tool(
        tools,
        "debugger_session_state",
        json!({"sessionId": session_id}),
    )
    .await
    .ok()?;
    if state["state"].as_str() == Some("Failed") {
        Some(
            state["details"]["error"]
                .as_str()
                .unwrap_or("(no error)")
                .to_string(),
        )
    } else {
        None
    }
}

/// After the call under test, confirm the dispatcher still answers. A slow
/// request is tolerable; one that leaves every later call timing out until
/// cancel/disconnect is not.
async fn dispatcher_still_responsive(tools: &ToolsHandler, session_id: &str) -> Result<(), String> {
    let started = Instant::now();
    let res = tool(
        tools,
        "debugger_stack_trace",
        json!({"sessionId": session_id, "levels": 3}),
    )
    .await;
    match res {
        Ok(_) => Ok(()),
        Err(e) => Err(format!(
            "follow-up stack_trace failed after {:?}: {e}",
            started.elapsed()
        )),
    }
}

// ============================================================================
// Scope-level enumeration of a heavy sync frame.
//
// `debugger_get_variables({frameId})` asks for top-level locals only, but
// CodeLLDB computes a summary string per Variable and those summaries walk each
// container of `&mut self` — so "top-level only" is not cheap by itself.
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn get_variables_on_heavy_sync_frame_does_not_wedge() {
    if !check_prerequisites() {
        return;
    }
    let a = match arrive_at_heavy_frame().await {
        Ok(v) => v,
        Err(e) => panic!("setup: {e}"),
    };

    let started = Instant::now();
    let result = tool(
        &a.tools,
        "debugger_get_variables",
        json!({"sessionId": &a.session_id, "frameId": a.frame_id, "maxCount": 30}),
    )
    .await;
    let elapsed = started.elapsed();
    eprintln!("get_variables(frameId) took {elapsed:?}: {result:?}");

    let responsive = dispatcher_still_responsive(&a.tools, &a.session_id).await;
    let failed = session_failed(&a.tools, &a.session_id).await;
    let _ = tool(
        &a.tools,
        "debugger_disconnect",
        json!({"sessionId": &a.session_id}),
    )
    .await;

    if let Some(err) = failed {
        panic!(
            "session moved to Failed (supervisor kill) after scope-level get_variables: {err}\n\
             --- source ---\n{}",
            a.src
        );
    }
    let vars = result.unwrap_or_else(|e| {
        panic!("scope-level get_variables on heavy sync frame failed after {elapsed:?}: {e}")
    });
    responsive.unwrap_or_else(|e| panic!("dispatcher wedged after get_variables: {e}"));

    let names: Vec<&str> = vars["variables"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v["name"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        names.contains(&"node_id"),
        "expected the in-scope `node_id` parameter in locals, got {names:?}"
    );
}

// ============================================================================
// `noSynthetic: true` is documented on `debugger_get_variables` without
// qualification, so it has to work in frameId mode and not only when drilling
// into a variablesReference.
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn no_synthetic_is_honored_on_scope_level_get_variables() {
    if !check_prerequisites() {
        return;
    }
    let a = match arrive_at_heavy_frame().await {
        Ok(v) => v,
        Err(e) => panic!("setup: {e}"),
    };

    let started = Instant::now();
    let result = tool(
        &a.tools,
        "debugger_get_variables",
        json!({
            "sessionId": &a.session_id,
            "frameId": a.frame_id,
            "maxCount": 30,
            "noSynthetic": true,
        }),
    )
    .await;
    let elapsed = started.elapsed();
    eprintln!("get_variables(frameId, noSynthetic) took {elapsed:?}: {result:?}");

    let failed = session_failed(&a.tools, &a.session_id).await;
    let _ = tool(
        &a.tools,
        "debugger_disconnect",
        json!({"sessionId": &a.session_id}),
    )
    .await;

    if let Some(err) = failed {
        panic!("session moved to Failed with noSynthetic:true set: {err}");
    }
    let vars = result.unwrap_or_else(|e| {
        panic!("get_variables(noSynthetic:true) on heavy sync frame failed after {elapsed:?}: {e}")
    });
    let names: Vec<&str> = vars["variables"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v["name"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        names.contains(&"node_id"),
        "expected `node_id` with noSynthetic:true, got {names:?}"
    );
}

// ============================================================================
// Candidate mitigation probe: `format.showRaw` on the `variables` request is a
// no-op in CodeLLDB 1.11, so `noSynthetic` cannot rescue scope enumeration.
// LLDB's own `target.enable-synthetic-value` switch is the other lever — it
// turns synthetic providers off inside LLDB itself, below the DAP layer. This
// measures whether flipping it around the enumeration is enough, which decides
// whether the fix can be a targeted toggle or has to be an adaptive budget.
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn disabling_synthetic_values_makes_heavy_frame_enumeration_cheap() {
    if !check_prerequisites() {
        return;
    }
    let a = match arrive_at_heavy_frame().await {
        Ok(v) => v,
        Err(e) => panic!("setup: {e}"),
    };

    let toggle = tool(
        &a.tools,
        "debugger_evaluate",
        json!({
            "sessionId": &a.session_id,
            "frameId": a.frame_id,
            "expression": "settings set target.enable-synthetic-value false",
            "context": "repl",
        }),
    )
    .await;
    eprintln!("toggle -> {toggle:?}");

    let started = Instant::now();
    let result = tool(
        &a.tools,
        "debugger_get_variables",
        json!({"sessionId": &a.session_id, "frameId": a.frame_id, "maxCount": 30}),
    )
    .await;
    let elapsed = started.elapsed();
    eprintln!("get_variables with synthetics disabled took {elapsed:?}: {result:?}");

    let failed = session_failed(&a.tools, &a.session_id).await;
    let _ = tool(
        &a.tools,
        "debugger_disconnect",
        json!({"sessionId": &a.session_id}),
    )
    .await;

    if let Some(err) = failed {
        panic!("session Failed even with synthetics disabled: {err}");
    }
    let vars = result
        .unwrap_or_else(|e| panic!("enumeration still failed after {elapsed:?} with synthetics disabled: {e}"));
    let names: Vec<&str> = vars["variables"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v["name"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        names.contains(&"node_id"),
        "expected `node_id` with synthetics disabled, got {names:?}"
    );
}

// ============================================================================
// REPL passthrough must surface a command's whole output, first line included.
// A one-line command is the sharp case: lose that line and an in-scope variable
// reads back as `empty: true`, indistinguishable from "no such variable".
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn repl_passthrough_keeps_the_first_output_line() {
    if !check_prerequisites() {
        return;
    }
    let a = match arrive_at_heavy_frame().await {
        Ok(v) => v,
        Err(e) => panic!("setup: {e}"),
    };

    let mut failures = Vec::new();
    for (expr, expected) in [
        ("frame variable node_id", "node_id"),
        ("frame variable weight", "weight"),
        // `self` is the first line LLDB prints for the no-argument form.
        ("frame variable", "self"),
        ("expression node_id", "= 3"),
    ] {
        let r = tool(
            &a.tools,
            "debugger_evaluate",
            json!({
                "sessionId": &a.session_id,
                "frameId": a.frame_id,
                "expression": expr,
                "context": "repl",
            }),
        )
        .await;
        match r {
            Ok(v) => {
                let combined = format!(
                    "{}{}",
                    v["result"].as_str().unwrap_or(""),
                    v["output"].as_str().unwrap_or("")
                );
                if !combined.contains(expected) {
                    failures.push(format!("`{expr}` lost {expected:?}: {v}"));
                }
            }
            Err(e) => failures.push(format!("`{expr}` failed: {e}")),
        }
    }

    let _ = tool(
        &a.tools,
        "debugger_disconnect",
        json!({"sessionId": &a.session_id}),
    )
    .await;

    assert!(failures.is_empty(), "REPL passthrough losses:\n{}", failures.join("\n"));
}

// ============================================================================
// Why scope enumeration must never issue the pretty request on a fat frame.
//
// A synthetic walk already under way inside LLDB does not stop when the DAP
// request that started it is abandoned or cancelled. It runs to completion,
// holding the adapter's dispatcher and climbing in RSS, so anything issued
// afterwards — including the `settings set` that would have made the retry
// cheap — queues behind it. There is no recovery-after-the-fact, only
// avoidance.
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn abandoned_synthetic_walk_keeps_running() {
    if !check_prerequisites() {
        return;
    }
    let a = match arrive_at_heavy_frame().await {
        Ok(v) => v,
        Err(e) => panic!("setup: {e}"),
    };

    let scope = tool(
        &a.tools,
        "debugger_get_variables",
        json!({"sessionId": &a.session_id, "frameId": a.frame_id, "maxCount": 30}),
    )
    .await
    .unwrap_or_else(|e| panic!("scope enumeration: {e}"));
    let self_ref = scope["variables"]
        .as_array()
        .and_then(|arr| arr.iter().find(|v| v["name"] == "self"))
        .and_then(|v| v["variablesReference"].as_i64())
        .unwrap_or_else(|| panic!("no `self` variablesReference in {scope}"));

    let started = Instant::now();
    let drill = tool(
        &a.tools,
        "debugger_get_variables",
        json!({"sessionId": &a.session_id, "variablesReference": self_ref, "maxCount": 5}),
    )
    .await;
    eprintln!("pretty drill into self took {:?}: {drill:?}", started.elapsed());

    let after = Instant::now();
    let settings = tool(
        &a.tools,
        "debugger_evaluate",
        json!({
            "sessionId": &a.session_id,
            "expression": "settings set target.enable-synthetic-value false",
            "context": "repl",
        }),
    )
    .await;
    eprintln!("settings set after the walk took {:?}: {settings:?}", after.elapsed());

    let state = tool(
        &a.tools,
        "debugger_session_state",
        json!({"sessionId": &a.session_id}),
    )
    .await;
    eprintln!("session state afterwards: {state:?}");

    let _ = tool(
        &a.tools,
        "debugger_disconnect",
        json!({"sessionId": &a.session_id}),
    )
    .await;
}

// ============================================================================
// Bare identifier + `context: "repl"`, with and without `noSynthetic`.
//
// The tool prepends `?` for noSynthetic; dap::client strips it back off and
// moves it to `format.showRaw`, so LLDB receives the bare name as a REPL
// *command line* and answers "'node_id' is not a valid command." The caller
// gets no hint that the flag combination is what broke it.
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn repl_bare_identifier_is_rejected_with_actionable_guidance() {
    if !check_prerequisites() {
        return;
    }
    let a = match arrive_at_heavy_frame().await {
        Ok(v) => v,
        Err(e) => panic!("setup: {e}"),
    };

    let mut errors = Vec::new();
    for no_synthetic in [false, true] {
        let result = tool(
            &a.tools,
            "debugger_evaluate",
            json!({
                "sessionId": &a.session_id,
                "frameId": a.frame_id,
                "expression": "node_id",
                "context": "repl",
                "noSynthetic": no_synthetic,
            }),
        )
        .await;
        eprintln!("evaluate(node_id, repl, noSynthetic={no_synthetic}) -> {result:?}");
        match result {
            Ok(v) => panic!("expected a rejection, got {v}"),
            Err(e) => errors.push(e),
        }
    }

    let _ = tool(
        &a.tools,
        "debugger_disconnect",
        json!({"sessionId": &a.session_id}),
    )
    .await;

    // The rejection has to carry the way out, or the caller learns nothing the
    // raw LLDB "not a valid command" didn't already tell them.
    for e in &errors {
        assert!(
            !e.contains("not a valid command"),
            "LLDB's command-interpreter error reached the caller: {e}"
        );
        assert!(
            e.contains("context: \"watch\"") && e.contains("frame variable node_id"),
            "rejection must name both working forms, got: {e}"
        );
    }
}

// ============================================================================
// `frame variable <name>` through the REPL must return the value. An empty
// response is indistinguishable from "no such variable" at the call site.
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn repl_frame_variable_returns_the_value_not_empty() {
    if !check_prerequisites() {
        return;
    }
    let a = match arrive_at_heavy_frame().await {
        Ok(v) => v,
        Err(e) => panic!("setup: {e}"),
    };

    let result = tool(
        &a.tools,
        "debugger_evaluate",
        json!({
            "sessionId": &a.session_id,
            "frameId": a.frame_id,
            "expression": "frame variable node_id",
            "context": "repl",
        }),
    )
    .await;
    eprintln!("evaluate(frame variable node_id, repl) -> {result:?}");

    let _ = tool(
        &a.tools,
        "debugger_disconnect",
        json!({"sessionId": &a.session_id}),
    )
    .await;

    let v = result.unwrap_or_else(|e| panic!("repl `frame variable node_id` failed: {e}"));
    assert!(
        v["empty"].as_bool() != Some(true),
        "repl `frame variable node_id` returned empty for an in-scope parameter: {v}"
    );
    let combined = format!(
        "{}{}",
        v["result"].as_str().unwrap_or(""),
        v["output"].as_str().unwrap_or("")
    );
    assert!(
        combined.contains("node_id"),
        "expected `node_id` in the passthrough output, got {v}"
    );
}

// ============================================================================
// Multi-name `frame variable` through the REPL. Names only: `row` is a `&Row`,
// which DWARF describes as a pointer, so a `row.rowid` field path would test
// LLDB's syntax error instead of the multi-name read.
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn repl_multi_name_frame_variable_does_not_wedge() {
    if !check_prerequisites() {
        return;
    }
    let a = match arrive_at_heavy_frame().await {
        Ok(v) => v,
        Err(e) => panic!("setup: {e}"),
    };

    let started = Instant::now();
    let result = tool(
        &a.tools,
        "debugger_evaluate",
        json!({
            "sessionId": &a.session_id,
            "frameId": a.frame_id,
            "expression": "frame variable node_id weight current_idx",
            "context": "repl",
        }),
    )
    .await;
    let elapsed = started.elapsed();
    eprintln!("evaluate(multi-name frame variable) took {elapsed:?}: {result:?}");

    let responsive = dispatcher_still_responsive(&a.tools, &a.session_id).await;
    let failed = session_failed(&a.tools, &a.session_id).await;
    let _ = tool(
        &a.tools,
        "debugger_disconnect",
        json!({"sessionId": &a.session_id}),
    )
    .await;

    if let Some(err) = failed {
        panic!("session moved to Failed after multi-name frame variable: {err}");
    }
    let v =
        result.unwrap_or_else(|e| panic!("multi-name `frame variable` failed after {elapsed:?}: {e}"));
    responsive
        .unwrap_or_else(|e| panic!("dispatcher wedged after multi-name frame variable: {e}"));

    // LLDB prints one line per requested name; anything missing was dropped
    // between LLDB and the caller.
    let combined = format!(
        "{}{}",
        v["result"].as_str().unwrap_or(""),
        v["output"].as_str().unwrap_or("")
    );
    let missing: Vec<&str> = ["node_id", "weight", "current_idx"]
        .into_iter()
        .filter(|n| !combined.contains(n))
        .collect();
    assert!(
        missing.is_empty(),
        "REPL passthrough dropped {missing:?} from a 3-name `frame variable`; got {v}"
    );
}
