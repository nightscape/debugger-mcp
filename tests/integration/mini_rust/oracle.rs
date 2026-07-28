//! Two oracle paths:
//!
//! 1. `check_snapshot_via_eval` — drives a Rust-aware expression evaluator with
//!    Rust-flavored expressions (`v.len() as i64`, `name == "foo".to_string()`,
//!    method calls on `HashMap`). At call time it picks the first available
//!    tool from `EVAL_TOOL_CANDIDATES` (currently prefers `debugger_eval_rust`
//!    when registered, falling back to `debugger_evaluate`). The plain
//!    `debugger_evaluate` path will fail on Rust syntax — it's there only so
//!    the oracle is testable end-to-end before `debugger_eval_rust` lands.
//!    Not currently called from the PBT harness; reserved for future enabling.
//!
//! 2. `check_snapshot_via_variables` — uses `debugger_get_variables`, reading
//!    DWARF directly. This is the path the PBT exercises today. Drills into
//!    `Vec` and `HashMap` via `variablesReference` for element-level checks.
//!    `BTreeMap` is verified by length only (CodeLLDB exposes the B-tree's
//!    internal node graph, not synthesized entries). See `mini_rust_map_layout_probe`
//!    for the layout assumptions; rerun it when CodeLLDB is upgraded.

/// Tool names tried in order when running the eval oracle. The first one that
/// the running server registers wins. Update by *prepending* — preferred names
/// first.
const EVAL_TOOL_CANDIDATES: &[&str] = &["debugger_eval_rust", "debugger_evaluate"];

use serde_json::{json, Value as JsonValue};
use std::collections::HashMap;
use std::time::Duration;

use debugger_mcp::mcp::tools::ToolsHandler;
use tokio::time::timeout;

use super::interp::Snapshot;
use super::value::{PrimValue, Value};

const EVAL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum Mismatch {
    EvalFailed { expr: String, error: String },
    EvalUnparseable { expr: String, raw: String, expected: String },
    PrimMismatch { name: String, expected: PrimValue, raw: String },
    LenMismatch { name: String, expected: usize, actual: i64 },
    VecElemMismatch { name: String, idx: usize, expected: PrimValue, raw: String },
    MapKeyAbsent { name: String, key: PrimValue },
    MapValueMismatch { name: String, key: PrimValue, expected: PrimValue, raw: String },
    VarMissing { name: String },
    GetVarsFailed { error: String },
    /// One documented access path returned a value contradicting the interpreter.
    AccessPathMismatch {
        path: String,
        name: String,
        expected: PrimValue,
        raw: String,
        readings: String,
    },
    /// One documented access path failed outright — timeout, adapter-stuck,
    /// supervisor kill. Diagnosable, but still a violation: reading an in-scope
    /// scalar must not start failing because heavy state shares the frame.
    AccessPathFailed {
        path: String,
        name: String,
        detail: String,
        readings: String,
    },
    /// One documented access path returned neither a value nor a specific error
    /// — `{"empty": true}`, a blank `result` with only a generic `note`, or a
    /// scope enumeration that simply omitted the variable.
    AccessPathSilentEmpty {
        path: String,
        name: String,
        detail: String,
        readings: String,
    },
    /// The session stopped answering after an operation.
    SessionPoisoned { after: String, detail: String },
}

pub async fn check_snapshot_via_eval(
    tools: &ToolsHandler,
    session_id: &str,
    snap: &Snapshot,
) -> Vec<Mismatch> {
    let mut out = Vec::new();
    for (name, expected) in &snap.vars {
        check_var(tools, session_id, name, expected, &mut out).await;
    }
    out
}

async fn check_var(
    tools: &ToolsHandler,
    session_id: &str,
    name: &str,
    expected: &Value,
    out: &mut Vec<Mismatch>,
) {
    match expected {
        Value::Prim(p) => check_prim(tools, session_id, name, p, out).await,
        Value::Vec(items) => check_vec(tools, session_id, name, items, out).await,
        Value::Map { kind: _, entries } => check_map(tools, session_id, name, entries, out).await,
        Value::Unit => {} // ignored
    }
}

async fn check_prim(
    tools: &ToolsHandler,
    session_id: &str,
    name: &str,
    expected: &PrimValue,
    out: &mut Vec<Mismatch>,
) {
    let expr = compare_prim_expr(name, expected);
    match eval(tools, session_id, &expr).await {
        Ok(raw) => {
            match parse_bool(&raw) {
                Some(true) => {}
                Some(false) => out.push(Mismatch::PrimMismatch {
                    name: name.into(), expected: expected.clone(), raw,
                }),
                None => out.push(Mismatch::EvalUnparseable {
                    expr, raw, expected: format!("bool comparing {name} to {expected:?}"),
                }),
            }
        }
        Err(e) => out.push(Mismatch::EvalFailed { expr, error: e }),
    }
}

async fn check_vec(
    tools: &ToolsHandler,
    session_id: &str,
    name: &str,
    items: &[Value],
    out: &mut Vec<Mismatch>,
) {
    let len_expr = format!("({name}.len() as i64)");
    let actual_len = match eval(tools, session_id, &len_expr).await {
        Ok(raw) => match parse_i64(&raw) {
            Some(n) => n,
            None => {
                out.push(Mismatch::EvalUnparseable {
                    expr: len_expr, raw, expected: "i64".into(),
                });
                return;
            }
        },
        Err(e) => {
            out.push(Mismatch::EvalFailed { expr: len_expr, error: e });
            return;
        }
    };
    if actual_len as usize != items.len() {
        out.push(Mismatch::LenMismatch {
            name: name.into(), expected: items.len(), actual: actual_len,
        });
        return;
    }
    for (idx, item) in items.iter().enumerate() {
        let p = item.as_prim();
        let expr = compare_prim_expr(&format!("{name}[{idx}usize]"), p);
        match eval(tools, session_id, &expr).await {
            Ok(raw) => match parse_bool(&raw) {
                Some(true) => {}
                Some(false) | None => out.push(Mismatch::VecElemMismatch {
                    name: name.into(), idx, expected: p.clone(), raw,
                }),
            },
            Err(e) => out.push(Mismatch::EvalFailed { expr, error: e }),
        }
    }
}

async fn check_map(
    tools: &ToolsHandler,
    session_id: &str,
    name: &str,
    entries: &std::collections::BTreeMap<PrimValue, Value>,
    out: &mut Vec<Mismatch>,
) {
    let len_expr = format!("({name}.len() as i64)");
    let actual_len = match eval(tools, session_id, &len_expr).await {
        Ok(raw) => match parse_i64(&raw) {
            Some(n) => n,
            None => {
                out.push(Mismatch::EvalUnparseable {
                    expr: len_expr, raw, expected: "i64".into(),
                });
                return;
            }
        },
        Err(e) => {
            out.push(Mismatch::EvalFailed { expr: len_expr, error: e });
            return;
        }
    };
    if actual_len as usize != entries.len() {
        out.push(Mismatch::LenMismatch {
            name: name.into(), expected: entries.len(), actual: actual_len,
        });
        return;
    }
    for (k, v) in entries {
        let key_expr = prim_literal(k);
        let contains = format!("{name}.contains_key(&{key_expr})");
        match eval(tools, session_id, &contains).await {
            Ok(raw) => match parse_bool(&raw) {
                Some(true) => {}
                Some(false) | None => {
                    out.push(Mismatch::MapKeyAbsent { name: name.into(), key: k.clone() });
                    continue;
                }
            },
            Err(e) => {
                out.push(Mismatch::EvalFailed { expr: contains, error: e });
                continue;
            }
        }
        let v_prim = v.as_prim();
        let access = format!("(*{name}.get(&{key_expr}).unwrap())");
        let expr = compare_prim_expr(&access, v_prim);
        match eval(tools, session_id, &expr).await {
            Ok(raw) => match parse_bool(&raw) {
                Some(true) => {}
                Some(false) | None => out.push(Mismatch::MapValueMismatch {
                    name: name.into(), key: k.clone(), expected: v_prim.clone(), raw,
                }),
            },
            Err(e) => out.push(Mismatch::EvalFailed { expr, error: e }),
        }
    }
}

fn compare_prim_expr(target: &str, expected: &PrimValue) -> String {
    match expected {
        PrimValue::I64(n) => format!("({target} == {n}i64)"),
        PrimValue::Bool(b) => format!("({target} == {b})"),
        PrimValue::String(s) => format!("({target}.as_str() == {})", string_literal(s)),
    }
}

fn prim_literal(p: &PrimValue) -> String {
    match p {
        PrimValue::I64(n) => format!("{n}i64"),
        PrimValue::Bool(b) => b.to_string(),
        PrimValue::String(s) => format!("{}.to_string()", string_literal(s)),
    }
}

fn string_literal(s: &str) -> String { format!("{s:?}") }

/// Variables-based oracle: works today, no Rust-eval needed.
///
/// For each expected variable in the snapshot we check via `debugger_get_variables`:
///   * Primitives — compare formatted value string against expected.
///   * Vec — verify type, then drill in via `variablesReference` and compare each
///     element [i] to `items[i]`.
///   * Map — verify type only (per-element drill-in is Task #5; CodeLLDB child
///     layout varies across versions).
pub async fn check_snapshot_via_variables(
    tools: &ToolsHandler,
    session_id: &str,
    snap: &Snapshot,
) -> Vec<Mismatch> {
    let mut out = Vec::new();
    let resp = match get_variables(tools, session_id, None).await {
        Ok(v) => v,
        Err(e) => {
            out.push(Mismatch::GetVarsFailed { error: e });
            return out;
        }
    };
    let by_name: HashMap<String, &JsonValue> = resp.iter()
        .filter_map(|v| v.get("name").and_then(|n| n.as_str()).map(|n| (n.to_string(), v)))
        .collect();

    for (name, expected) in &snap.vars {
        let Some(var) = by_name.get(name) else {
            out.push(Mismatch::VarMissing { name: name.clone() });
            continue;
        };
        match expected {
            Value::Prim(p) => {
                let raw = var.get("value").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if !prim_value_str_matches(p, &raw) {
                    out.push(Mismatch::PrimMismatch {
                        name: name.clone(), expected: p.clone(), raw,
                    });
                }
            }
            Value::Vec(items) => {
                let ty = var.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if !(ty.contains("Vec") || ty.contains("alloc::vec")) {
                    out.push(Mismatch::PrimMismatch {
                        name: name.clone(),
                        expected: PrimValue::String(format!("Vec type, got `{ty}`")),
                        raw: ty.into(),
                    });
                    continue;
                }
                let var_ref = var.get("variablesReference").and_then(|v| v.as_i64()).unwrap_or(0);
                if var_ref == 0 {
                    // Empty Vec is sometimes returned non-expandable; only flag
                    // if we expected children.
                    if !items.is_empty() {
                        out.push(Mismatch::VarMissing {
                            name: format!("{name} children (variablesReference=0)"),
                        });
                    }
                    continue;
                }
                check_vec_children(tools, session_id, name, items, var_ref as i32, &mut out).await;
            }
            Value::Map { kind, entries } => {
                let ty = var.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let kind_ok = match kind {
                    super::value::MapKind::Hash =>
                        ty.contains("HashMap") || ty.contains("hashmap"),
                    super::value::MapKind::BTree =>
                        ty.contains("BTreeMap") || ty.contains("btree"),
                };
                if !kind_ok {
                    out.push(Mismatch::PrimMismatch {
                        name: name.clone(),
                        expected: PrimValue::String(format!("{kind:?} type, got `{ty}`")),
                        raw: ty.into(),
                    });
                    continue;
                }
                let var_ref = var.get("variablesReference").and_then(|v| v.as_i64()).unwrap_or(0);
                if var_ref == 0 {
                    if !entries.is_empty() {
                        out.push(Mismatch::VarMissing {
                            name: format!("{name} children (variablesReference=0)"),
                        });
                    }
                    continue;
                }
                check_map_children(tools, session_id, name, *kind, entries, var_ref as i32, &mut out).await;
            }
            Value::Unit => {}
        }
    }
    out
}

/// Drill into a Vec via its `variablesReference` and compare each element to
/// the interpreter's expected items.
///
/// CodeLLDB names Vec children `[0]`, `[1]`, … plus sometimes auxiliary entries
/// like `[raw]` for the underlying buffer. Children whose names don't match
/// `[<digits>]` are ignored (forward-compat with whatever extra rows CodeLLDB
/// chooses to surface). Length comparison uses *only* indexed children.
async fn check_vec_children(
    tools: &ToolsHandler,
    session_id: &str,
    name: &str,
    expected: &[Value],
    var_ref: i32,
    out: &mut Vec<Mismatch>,
) {
    let children = match get_variables(tools, session_id, Some(var_ref)).await {
        Ok(v) => v,
        Err(e) => {
            out.push(Mismatch::GetVarsFailed { error: format!("children of {name}: {e}") });
            return;
        }
    };

    let indexed: HashMap<usize, &JsonValue> = children.iter()
        .filter_map(|v| {
            let n = v.get("name").and_then(|n| n.as_str())?;
            parse_indexed_name(n).map(|i| (i, v))
        })
        .collect();

    if indexed.len() != expected.len() {
        out.push(Mismatch::LenMismatch {
            name: name.into(),
            expected: expected.len(),
            actual: indexed.len() as i64,
        });
        return;
    }

    for (i, want) in expected.iter().enumerate() {
        let Some(child) = indexed.get(&i) else {
            out.push(Mismatch::VecElemMismatch {
                name: name.into(),
                idx: i,
                expected: want.as_prim().clone(),
                raw: format!("(child [{i}] missing)"),
            });
            continue;
        };
        let raw = child.get("value").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let want_prim = want.as_prim();
        if !prim_value_str_matches(want_prim, &raw) {
            out.push(Mismatch::VecElemMismatch {
                name: name.into(),
                idx: i,
                expected: want_prim.clone(),
                raw,
            });
        }
    }
}

/// Parse a child name like `[3]` into the integer 3. Returns None for any name
/// that doesn't match `[<digits>]` so we can ignore CodeLLDB's auxiliary rows.
fn parse_indexed_name(name: &str) -> Option<usize> {
    let inner = name.strip_prefix('[')?.strip_suffix(']')?;
    inner.parse().ok()
}

/// Drill into a Map and verify contents.
///
/// CodeLLDB layouts (current observation, see `mini_rust_map_layout_probe`):
///
/// * **HashMap** — synthesized children `[0]`..`[n-1]` of type `(K, V)`. Each
///   pair drills into children named `"0"` (key) and `"1"` (value). Comparison
///   is **set-wise** because HashMap iteration order is non-deterministic.
///
/// * **BTreeMap** — *not* synthesized as indexed entries. CodeLLDB exposes the
///   B-tree's internal fields (`root`, `length`, `alloc`, `_marker`, `[raw]`).
///   We verify `length` against the expected entry count; per-entry comparison
///   would require walking the B-tree node graph, which is structurally
///   version-dependent and out of scope for now.
async fn check_map_children(
    tools: &ToolsHandler,
    session_id: &str,
    name: &str,
    kind: super::value::MapKind,
    expected: &std::collections::BTreeMap<PrimValue, Value>,
    var_ref: i32,
    out: &mut Vec<Mismatch>,
) {
    let children = match get_variables(tools, session_id, Some(var_ref)).await {
        Ok(v) => v,
        Err(e) => {
            out.push(Mismatch::GetVarsFailed { error: format!("children of {name}: {e}") });
            return;
        }
    };

    match kind {
        super::value::MapKind::Hash => {
            let pair_refs: Vec<(usize, i32)> = children.iter()
                .filter_map(|c| {
                    let n = c.get("name").and_then(|n| n.as_str())?;
                    let i = parse_indexed_name(n)?;
                    let r = c.get("variablesReference").and_then(|v| v.as_i64())? as i32;
                    Some((i, r))
                })
                .collect();

            if pair_refs.len() != expected.len() {
                out.push(Mismatch::LenMismatch {
                    name: name.into(),
                    expected: expected.len(),
                    actual: pair_refs.len() as i64,
                });
                return;
            }

            // Read each pair's key (child "0") and value (child "1").
            let mut observed: Vec<(String, String)> = Vec::with_capacity(pair_refs.len());
            for (i, pref) in pair_refs {
                let pair_kids = match get_variables(tools, session_id, Some(pref)).await {
                    Ok(v) => v,
                    Err(e) => {
                        out.push(Mismatch::GetVarsFailed {
                            error: format!("{name}[{i}]: {e}"),
                        });
                        return;
                    }
                };
                let key_str = pair_kids.iter()
                    .find(|c| c.get("name").and_then(|n| n.as_str()) == Some("0"))
                    .and_then(|c| c.get("value").and_then(|v| v.as_str()))
                    .unwrap_or("").to_string();
                let val_str = pair_kids.iter()
                    .find(|c| c.get("name").and_then(|n| n.as_str()) == Some("1"))
                    .and_then(|c| c.get("value").and_then(|v| v.as_str()))
                    .unwrap_or("").to_string();
                observed.push((key_str, val_str));
            }

            for (ek, ev) in expected {
                let ev_prim = ev.as_prim();
                let key_match = observed.iter().find(|(ok, _)| prim_value_str_matches(ek, ok));
                match key_match {
                    None => out.push(Mismatch::MapKeyAbsent {
                        name: name.into(), key: ek.clone(),
                    }),
                    Some((_, ov)) if !prim_value_str_matches(ev_prim, ov) => {
                        out.push(Mismatch::MapValueMismatch {
                            name: name.into(),
                            key: ek.clone(),
                            expected: ev_prim.clone(),
                            raw: ov.clone(),
                        });
                    }
                    Some(_) => {}
                }
            }
        }
        super::value::MapKind::BTree => {
            let length_str = children.iter()
                .find(|c| c.get("name").and_then(|n| n.as_str()) == Some("length"))
                .and_then(|c| c.get("value").and_then(|v| v.as_str()))
                .unwrap_or("");
            match parse_i64(length_str) {
                Some(n) if n as usize == expected.len() => {}
                Some(n) => out.push(Mismatch::LenMismatch {
                    name: name.into(),
                    expected: expected.len(),
                    actual: n,
                }),
                None => out.push(Mismatch::PrimMismatch {
                    name: format!("{name}.length"),
                    expected: PrimValue::I64(expected.len() as i64),
                    raw: length_str.into(),
                }),
            }
        }
    }
}

fn prim_value_str_matches(expected: &PrimValue, raw: &str) -> bool {
    match expected {
        PrimValue::I64(n) => parse_i64(raw) == Some(*n),
        PrimValue::Bool(b) => parse_bool(raw) == Some(*b),
        // CodeLLDB wraps a String's raw bytes in `"..."` — no escaping. A real
        // newline byte in the string stays a real newline in `raw`; an embedded
        // `"` stays a literal `"`. Strip the outer quotes and compare bytes.
        // Earlier versions used substring `contains` against `{s:?}`; that
        // produced both false negatives (because Rust debug-escaped `\n` while
        // LLDB kept the byte) and false positives (because "beta" matched
        // inside "alpha-beta-gamma…"). Strict equality is the right primitive.
        PrimValue::String(s) => strip_lldb_quotes(raw) == Some(s.as_str()),
    }
}

fn strip_lldb_quotes(raw: &str) -> Option<&str> {
    raw.strip_prefix('"').and_then(|r| r.strip_suffix('"'))
}

async fn get_variables(
    tools: &ToolsHandler,
    session_id: &str,
    var_ref: Option<i32>,
) -> Result<Vec<JsonValue>, String> {
    let mut args = json!({"sessionId": session_id, "maxCount": 200});
    if let Some(r) = var_ref {
        args["variablesReference"] = json!(r);
    }
    let fut = tools.handle_tool("debugger_get_variables", args);
    match timeout(EVAL_TIMEOUT, fut).await {
        Ok(Ok(v)) => Ok(v.get("variables").and_then(|x| x.as_array()).cloned().unwrap_or_default()),
        Ok(Err(e)) => Err(format!("{e}")),
        Err(_) => Err(format!("debugger_get_variables timed out after {EVAL_TIMEOUT:?}")),
    }
}

async fn eval(tools: &ToolsHandler, session_id: &str, expr: &str) -> Result<String, String> {
    // Route through whichever eval tool is registered. Preference order is
    // declared in `EVAL_TOOL_CANDIDATES`. The set of registered tools is fixed
    // for the life of the process, so it's safe to discover lazily once.
    let registered = registered_tool_names(tools);
    let tool_name = EVAL_TOOL_CANDIDATES.iter()
        .find(|n| registered.contains(**n))
        .copied()
        .ok_or_else(|| "no eval tool registered (looked for debugger_eval_rust, debugger_evaluate)".to_string())?;
    let args = json!({"sessionId": session_id, "expression": expr});
    let fut = tools.handle_tool(tool_name, args);
    match timeout(EVAL_TIMEOUT, fut).await {
        Ok(Ok(v)) => Ok(v.get("result").and_then(|r| r.as_str()).unwrap_or("").to_string()),
        Ok(Err(e)) => Err(format!("{e}")),
        Err(_) => Err(format!("{tool_name} timed out after {EVAL_TIMEOUT:?}: {expr}")),
    }
}

fn registered_tool_names(_tools: &ToolsHandler) -> std::collections::HashSet<&'static str> {
    // ToolsHandler::list_tools is a static descriptor of the registered tool
    // surface — same source the MCP `tools/list` endpoint reads from. Filter to
    // names we know about so we never accidentally call something else.
    let names: std::collections::HashSet<String> = ToolsHandler::list_tools().iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect();
    EVAL_TOOL_CANDIDATES.iter().copied()
        .filter(|c| names.contains(*c))
        .collect()
}

fn parse_bool(s: &str) -> Option<bool> {
    let lower = s.to_ascii_lowercase();
    if lower.contains("true") && !lower.contains("false") { return Some(true); }
    if lower.contains("false") && !lower.contains("true") { return Some(false); }
    None
}

fn parse_i64(s: &str) -> Option<i64> {
    // LLDB output is typically "(type) value" — the value is the last
    // parseable integer token. Take the last one to avoid the type prefix.
    s.split(|c: char| !c.is_ascii_digit() && c != '-')
        .filter(|tok| !tok.is_empty() && *tok != "-")
        .filter_map(|tok| tok.parse::<i64>().ok())
        .last()
}

// ============================================================================
// Access-path agreement oracle
//
// Every documented way to read one in-scope scalar must produce the same value,
// or fail loudly. The bug this pins: some paths answer with nothing at all —
// `{"empty": true}` or a blank `result` carrying only a generic `note` — which
// an agent cannot distinguish from "the variable is absent".
// ============================================================================

/// How a path's raw reading is compared against the expected value.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Cmp {
    /// The reading *is* the rendered value (`3`, `"foo"`, `true`) — strict
    /// equality via `prim_value_str_matches`.
    Strict,
    /// The reading is LLDB `frame variable` output text (`(long) node_id = 3`).
    /// The `name = value` field is extracted first, then compared strictly; the
    /// surrounding type prefix and line framing are LLDB's, not ours to pin.
    FrameVariableText,
}

#[derive(Debug, PartialEq)]
enum Reading {
    Value(String),
    /// Failed, but with a non-empty specific message — acceptable.
    LoudFailure(String),
    /// Neither a value nor a specific error — the violation.
    SilentEmpty(String),
}

/// Read one scalar through every documented access path and require agreement.
///
/// The contract for a plain scalar that is in scope at the current PC: *every*
/// documented path must return its correct value within `budget`. Three ways to
/// break it, all violations, differing only in diagnosability:
///
///   * `AccessPathMismatch` — answered, but with the wrong value.
///   * `AccessPathFailed` — failed loudly (timeout, adapter-stuck, supervisor
///     kill). Reading a small scalar must not start failing because unrelated
///     heavy state shares the frame.
///   * `AccessPathSilentEmpty` — answered nothing, with no error to act on.
///
/// `budget` is per tool call and deliberately *not* the shared `EVAL_TIMEOUT`:
/// under a heavy fixture a slow-but-correct read now fails the property, so the
/// harness sets the ceiling per tier.
pub async fn check_scalar_all_paths(
    tools: &ToolsHandler,
    session_id: &str,
    name: &str,
    expected: &PrimValue,
    budget: Duration,
) -> Vec<Mismatch> {
    let frame_id = match top_frame_id(tools, session_id, budget).await {
        Ok(f) => f,
        Err(e) => {
            return vec![Mismatch::SessionPoisoned {
                after: format!("resolving top frame to read `{name}`"),
                detail: e,
            }]
        }
    };

    let readings: Vec<(&str, Cmp, Reading)> = vec![
        (
            "get_variables(frameId)",
            Cmp::Strict,
            read_scope(tools, session_id, frame_id, name, false, budget).await,
        ),
        (
            "get_variables(frameId, noSynthetic)",
            Cmp::Strict,
            read_scope(tools, session_id, frame_id, name, true, budget).await,
        ),
        (
            "evaluate(watch)",
            Cmp::Strict,
            read_evaluate(tools, session_id, frame_id, name, "watch", false, budget).await,
        ),
        (
            "evaluate(watch, noSynthetic)",
            Cmp::Strict,
            read_evaluate(tools, session_id, frame_id, name, "watch", true, budget).await,
        ),
        (
            "evaluate(repl, `frame variable`)",
            Cmp::FrameVariableText,
            read_evaluate(
                tools,
                session_id,
                frame_id,
                &format!("frame variable {name}"),
                "repl",
                false,
                budget,
            )
            .await,
        ),
    ];

    verdicts(&readings, name, expected)
}

/// Pure verdict pass: every reading that is not the correct value is a
/// violation, carrying the full reading table so a divergence is legible from
/// any single Mismatch.
fn verdicts(
    readings: &[(&str, Cmp, Reading)],
    name: &str,
    expected: &PrimValue,
) -> Vec<Mismatch> {
    let rendered = render_readings(name, expected, readings);
    let mut out = Vec::new();
    for (path, cmp, reading) in readings {
        match reading {
            Reading::LoudFailure(detail) => out.push(Mismatch::AccessPathFailed {
                path: (*path).into(),
                name: name.into(),
                detail: detail.clone(),
                readings: rendered.clone(),
            }),
            Reading::SilentEmpty(detail) => out.push(Mismatch::AccessPathSilentEmpty {
                path: (*path).into(),
                name: name.into(),
                detail: detail.clone(),
                readings: rendered.clone(),
            }),
            Reading::Value(raw) => {
                if !value_matches(*cmp, raw, name, expected) {
                    out.push(Mismatch::AccessPathMismatch {
                        path: (*path).into(),
                        name: name.into(),
                        expected: expected.clone(),
                        raw: raw.clone(),
                        readings: rendered.clone(),
                    });
                }
            }
        }
    }
    out
}

/// One expensive request must not poison the session: a cheap stack trace and
/// the session state both have to keep answering afterwards.
pub async fn check_session_live(
    tools: &ToolsHandler,
    session_id: &str,
    after: &str,
) -> Vec<Mismatch> {
    let mut out = Vec::new();

    if let Err(e) = call(
        tools,
        "debugger_stack_trace",
        json!({"sessionId": session_id, "limit": 3}),
        EVAL_TIMEOUT,
    )
    .await
    {
        out.push(Mismatch::SessionPoisoned {
            after: after.into(),
            detail: format!("debugger_stack_trace: {e}"),
        });
    }

    match call(
        tools,
        "debugger_session_state",
        json!({"sessionId": session_id}),
        EVAL_TIMEOUT,
    )
    .await
    {
        Ok(v) if v["state"].as_str() == Some("Failed") => out.push(Mismatch::SessionPoisoned {
            after: after.into(),
            detail: format!(
                "session state Failed: {}",
                v["details"]["error"].as_str().unwrap_or("(no details.error)")
            ),
        }),
        Ok(_) => {}
        Err(e) => out.push(Mismatch::SessionPoisoned {
            after: after.into(),
            detail: format!("debugger_session_state: {e}"),
        }),
    }

    out
}

fn value_matches(cmp: Cmp, raw: &str, name: &str, expected: &PrimValue) -> bool {
    match cmp {
        Cmp::Strict => prim_value_str_matches(expected, raw),
        Cmp::FrameVariableText => match frame_variable_field(raw, name) {
            Some(v) => prim_value_str_matches(expected, v),
            None => false,
        },
    }
}

/// Extract `value` from an LLDB `frame variable` line such as
/// `(long) node_id = 3`. Requires a non-identifier character before `name` so
/// `other_node_id = 9` cannot answer a query for `node_id`.
fn frame_variable_field<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name} = ");
    text.lines().find_map(|line| {
        let idx = line.find(&needle)?;
        let before = line[..idx].chars().next_back();
        if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            return None;
        }
        // A value with expandable children opens a brace block on the same
        // line: `(alloc::string::String) s = "init" {`. The summary is what
        // precedes it.
        Some(
            line[idx + needle.len()..]
                .trim_end()
                .trim_end_matches('{')
                .trim_end(),
        )
    })
}

fn render_readings(
    name: &str,
    expected: &PrimValue,
    readings: &[(&str, Cmp, Reading)],
) -> String {
    let mut s = format!("readings of `{name}` (expected {expected:?}):\n");
    for (path, _, r) in readings {
        let body = match r {
            Reading::Value(v) => format!("value {v:?}"),
            Reading::LoudFailure(e) => format!("FAILED: {e}"),
            Reading::SilentEmpty(d) => format!("SILENT-EMPTY: {d}"),
        };
        s.push_str(&format!("  {path:<38} {body}\n"));
    }
    s
}

async fn read_scope(
    tools: &ToolsHandler,
    session_id: &str,
    frame_id: i32,
    name: &str,
    no_synthetic: bool,
    budget: Duration,
) -> Reading {
    let args = json!({
        "sessionId": session_id,
        "frameId": frame_id,
        "maxCount": 200,
        "noSynthetic": no_synthetic,
    });
    match call(tools, "debugger_get_variables", args, budget).await {
        Err(e) => classify_error(&e),
        Ok(v) => {
            let vars = v.get("variables").and_then(|x| x.as_array()).cloned().unwrap_or_default();
            match vars.iter().find(|x| x.get("name").and_then(|n| n.as_str()) == Some(name)) {
                Some(var) => match var.get("value").and_then(|x| x.as_str()) {
                    Some(s) if !s.is_empty() => Reading::Value(s.to_string()),
                    _ => Reading::SilentEmpty(format!("variable row without a value: {var}")),
                },
                None => {
                    let names: Vec<&str> =
                        vars.iter().filter_map(|x| x.get("name").and_then(|n| n.as_str())).collect();
                    Reading::SilentEmpty(format!(
                        "scope enumeration succeeded but omitted `{name}`; saw {names:?}"
                    ))
                }
            }
        }
    }
}

async fn read_evaluate(
    tools: &ToolsHandler,
    session_id: &str,
    frame_id: i32,
    expression: &str,
    context: &str,
    no_synthetic: bool,
    budget: Duration,
) -> Reading {
    let args = json!({
        "sessionId": session_id,
        "frameId": frame_id,
        "expression": expression,
        "context": context,
        "noSynthetic": no_synthetic,
    });
    match call(tools, "debugger_evaluate", args, budget).await {
        Err(e) => classify_error(&e),
        Ok(v) => classify_evaluate(&v),
    }
}

/// A failure is acceptable only if it says something specific.
fn classify_error(err: &str) -> Reading {
    if err.trim().is_empty() {
        Reading::SilentEmpty("failed with an empty error message".into())
    } else {
        Reading::LoudFailure(err.to_string())
    }
}

/// `result` first, then REPL passthrough `output`/`stderr`. Anything else is a
/// silent nothing — including the explicit `empty: true` marker.
fn classify_evaluate(resp: &JsonValue) -> Reading {
    if resp.get("empty").and_then(|e| e.as_bool()) == Some(true) {
        return Reading::SilentEmpty(format!("responded empty:true — {resp}"));
    }
    for field in ["result", "output", "stderr"] {
        if let Some(s) = resp.get(field).and_then(|v| v.as_str()) {
            if !s.trim().is_empty() {
                return Reading::Value(s.to_string());
            }
        }
    }
    Reading::SilentEmpty(format!("no result, output or stderr — {resp}"))
}

async fn top_frame_id(
    tools: &ToolsHandler,
    session_id: &str,
    budget: Duration,
) -> Result<i32, String> {
    let args = json!({"sessionId": session_id, "format": "json", "limit": 1});
    let v = call(tools, "debugger_stack_trace", args, budget).await?;
    v["stackFrames"][0]["id"]
        .as_i64()
        .map(|i| i as i32)
        .ok_or_else(|| format!("stack trace carried no frame id: {v}"))
}

async fn call(
    tools: &ToolsHandler,
    name: &str,
    args: JsonValue,
    budget: Duration,
) -> Result<JsonValue, String> {
    match timeout(budget, tools.handle_tool(name, args)).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(format!("{e}")),
        Err(_) => Err(format!("{name} timed out after {budget:?}")),
    }
}

#[cfg(test)]
mod parser_tests {
    use super::*;

    #[test]
    fn parses_bools() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("(bool) true"), Some(true));
        assert_eq!(parse_bool("false"), Some(false));
        assert_eq!(parse_bool("0x42"), None);
    }

    #[test]
    fn parses_i64s() {
        assert_eq!(parse_i64("3"), Some(3));
        assert_eq!(parse_i64("(i64) 42"), Some(42));
        assert_eq!(parse_i64("-7"), Some(-7));
        assert_eq!(parse_i64(""), None);
    }

    #[test]
    fn prim_string_matches_quoted_lldb_value() {
        // LLDB wraps the raw bytes; no escaping. These cases all came up live
        // during PBT runs, including the substring-confusion bug that prompted
        // the matcher rewrite.
        let foo = PrimValue::String("foo".into());
        assert!(prim_value_str_matches(&foo, "\"foo\""));
        assert!(!prim_value_str_matches(&foo, "\"foobar\"")); // not a substring
        assert!(!prim_value_str_matches(&foo, "\"alpha-foo-beta\"")); // ditto

        // Real newline byte (LLDB does not escape).
        let nl = PrimValue::String("with\nnewline".into());
        assert!(prim_value_str_matches(&nl, "\"with\nnewline\""));
        // Backslash-n in raw (Rust debug-escape form) must NOT match a real-newline
        // expected — that was the false-negative that masked Map mismatches.
        assert!(!prim_value_str_matches(&nl, "\"with\\nnewline\""));

        // Embedded literal quote — LLDB preserves it (broken-JSON shape).
        let q = PrimValue::String("with\"quote".into());
        assert!(prim_value_str_matches(&q, "\"with\"quote\""));

        let empty = PrimValue::String(String::new());
        assert!(prim_value_str_matches(&empty, "\"\""));
        assert!(!prim_value_str_matches(&empty, "anything"));
    }

    #[test]
    fn silent_empty_responses_are_violations() {
        // The exact reported shape: in-scope parameter, nothing back.
        let Reading::SilentEmpty(detail) = classify_evaluate(&json!({"result": "", "empty": true}))
        else {
            panic!("`empty: true` must be classified as a violation");
        };
        assert!(detail.contains("empty:true"), "{detail}");
        assert!(matches!(
            classify_evaluate(&json!({"result": "", "note": "may also indicate a transport drop"})),
            Reading::SilentEmpty(_)
        ));
        assert!(matches!(classify_evaluate(&json!({})), Reading::SilentEmpty(_)));
        assert!(matches!(classify_error("   "), Reading::SilentEmpty(_)));
    }

    /// A loud failure is diagnosable, not acceptable: for an in-scope scalar it
    /// is still a violation, reported separately from the silent-empty case.
    #[test]
    fn loud_failures_are_a_distinct_violation() {
        assert_eq!(
            classify_error("Evaluate failed: 'node_id' is not a valid command."),
            Reading::LoudFailure("Evaluate failed: 'node_id' is not a valid command.".into())
        );
        assert_eq!(
            classify_error("debugger_get_variables timed out after 4s"),
            Reading::LoudFailure("debugger_get_variables timed out after 4s".into())
        );

        let readings = vec![
            ("evaluate(watch)", Cmp::Strict, Reading::Value("3".into())),
            (
                "get_variables(frameId)",
                Cmp::Strict,
                Reading::LoudFailure("variables request timed out after 4s".into()),
            ),
            (
                "evaluate(repl, `frame variable`)",
                Cmp::FrameVariableText,
                Reading::SilentEmpty("responded empty:true".into()),
            ),
        ];
        let verdicts = verdicts(&readings, "node_id", &PrimValue::I64(3));
        assert_eq!(verdicts.len(), 2, "{verdicts:#?}");
        assert!(matches!(verdicts[0], Mismatch::AccessPathFailed { .. }));
        assert!(matches!(verdicts[1], Mismatch::AccessPathSilentEmpty { .. }));
    }

    #[test]
    fn correct_values_are_accepted() {
        assert_eq!(
            classify_evaluate(&json!({"result": "3"})),
            Reading::Value("3".into())
        );
        // REPL passthrough puts the text on `output`, with `result` blank.
        assert_eq!(
            classify_evaluate(&json!({"result": "", "output": "(long) node_id = 3\n"})),
            Reading::Value("(long) node_id = 3\n".into())
        );

        // All paths agreeing — the only outcome with no verdicts.
        let readings = vec![
            ("get_variables(frameId)", Cmp::Strict, Reading::Value("3".into())),
            ("evaluate(watch)", Cmp::Strict, Reading::Value("3".into())),
            (
                "evaluate(repl, `frame variable`)",
                Cmp::FrameVariableText,
                Reading::Value("(long) node_id = 3\n".into()),
            ),
        ];
        assert!(verdicts(&readings, "node_id", &PrimValue::I64(3)).is_empty());

        let disagreeing = vec![("evaluate(watch)", Cmp::Strict, Reading::Value("4".into()))];
        assert!(matches!(
            verdicts(&disagreeing, "node_id", &PrimValue::I64(3))[0],
            Mismatch::AccessPathMismatch { .. }
        ));
    }

    #[test]
    fn frame_variable_output_yields_the_value() {
        let out = "(long) node_id = 3\n";
        assert_eq!(frame_variable_field(out, "node_id"), Some("3"));
        assert!(value_matches(Cmp::FrameVariableText, out, "node_id", &PrimValue::I64(3)));
        assert!(!value_matches(Cmp::FrameVariableText, out, "node_id", &PrimValue::I64(4)));

        let multi = "(long) weight = -2\n(unsigned long) current_idx = 1\n";
        assert_eq!(frame_variable_field(multi, "current_idx"), Some("1"));
        assert_eq!(frame_variable_field(multi, "weight"), Some("-2"));

        let s = "(alloc::string::String) label = \"foo\"\n";
        assert!(value_matches(
            Cmp::FrameVariableText,
            s,
            "label",
            &PrimValue::String("foo".into())
        ));

        // An expandable value opens its children block on the summary line.
        let expandable = "(alloc::string::String) s = \"init\" {\n  vec = size=4 {\n  }\n}\n";
        assert_eq!(frame_variable_field(expandable, "s"), Some("\"init\""));
        assert!(value_matches(
            Cmp::FrameVariableText,
            expandable,
            "s",
            &PrimValue::String("init".into())
        ));

        // A different variable whose name ends in the queried one must not answer.
        assert_eq!(frame_variable_field("(long) other_node_id = 9\n", "node_id"), None);
        assert_eq!(frame_variable_field("error: no variable named 'node_id'", "node_id"), None);
    }

    #[test]
    fn parse_indexed_name_filters_aux_rows() {
        assert_eq!(parse_indexed_name("[0]"), Some(0));
        assert_eq!(parse_indexed_name("[42]"), Some(42));
        assert_eq!(parse_indexed_name("[raw]"), None);
        assert_eq!(parse_indexed_name("length"), None);
        assert_eq!(parse_indexed_name(""), None);
    }
}
