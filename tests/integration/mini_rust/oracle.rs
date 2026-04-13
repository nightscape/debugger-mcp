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
    fn parse_indexed_name_filters_aux_rows() {
        assert_eq!(parse_indexed_name("[0]"), Some(0));
        assert_eq!(parse_indexed_name("[42]"), Some(42));
        assert_eq!(parse_indexed_name("[raw]"), None);
        assert_eq!(parse_indexed_name("length"), None);
        assert_eq!(parse_indexed_name(""), None);
    }
}
