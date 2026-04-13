//! Pinned regression test for the CodeLLDB child layout the Map/Vec oracle
//! relies on. Compiles a tiny Rust program with a populated HashMap (incl.
//! edge keys with newline + embedded quote), BTreeMap, and Vec, then asserts
//! every layout assumption the oracle makes:
//!
//! * Vec exposes `[0]`..`[n-1]` indexed children with parseable values.
//! * HashMap exposes `[N]` pair children, each drilling to grandchildren named
//!   `0` (key) and `1` (value).
//! * BTreeMap does **not** synthesize indexed entries — it surfaces the B-tree's
//!   internal `length` field instead. (If this assumption ever breaks because
//!   CodeLLDB starts synthesizing entries, this test fails fast and the Map
//!   oracle's BTreeMap arm should be upgraded from length-only to drill-in.)
//! * String values are wrapped in `"..."` with no escaping — a real `\n` byte
//!   stays a real byte; an embedded `"` stays literal.
//!
//! Skipped automatically on machines without `codelldb` so it doesn't break
//! CI by default.

use serde_json::{json, Value as JsonValue};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Runtime;
use tokio::sync::RwLock;
use tokio::time::timeout;

use debugger_mcp::debug::SessionManager;
use debugger_mcp::mcp::tools::ToolsHandler;

const TOOL_TIMEOUT: Duration = Duration::from_secs(15);

const PROBE_SOURCE: &str = r#"
#![allow(unused)]
use std::collections::{HashMap, BTreeMap};

fn main() {
    let mut h: HashMap<String, i64> = HashMap::new();
    h.insert("alpha".to_string(), 1);
    h.insert("beta".to_string(), 2);
    h.insert("gamma".to_string(), 3);
    h.insert("with\nnewline".to_string(), 99);
    h.insert("with\"quote".to_string(), 88);

    let mut t: BTreeMap<i64, String> = BTreeMap::new();
    t.insert(10, "ten".to_string());
    t.insert(20, "twenty".to_string());
    t.insert(30, "thirty".to_string());

    let mut v: Vec<i64> = Vec::new();
    v.push(7);
    v.push(11);
    v.push(13);

    std::hint::black_box(()); // BREAKPOINT HERE
    let _ = (h, t, v);
}
"#;

fn codelldb_available() -> bool {
    Command::new("codelldb").arg("--help").output()
        .map(|o| o.status.success()).unwrap_or(false)
}

fn rustc_available() -> bool {
    Command::new("rustc").arg("--version").output()
        .map(|o| o.status.success()).unwrap_or(false)
}

async fn tool_call(
    tools: &ToolsHandler,
    name: &str,
    args: JsonValue,
) -> JsonValue {
    match timeout(TOOL_TIMEOUT, tools.handle_tool(name, args)).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => panic!("{name} failed: {e}"),
        Err(_) => panic!("{name} timed out after {TOOL_TIMEOUT:?}"),
    }
}

fn parse_indexed_name(name: &str) -> Option<usize> {
    name.strip_prefix('[')?.strip_suffix(']')?.parse().ok()
}

fn child_named<'a>(arr: &'a [JsonValue], wanted: &str) -> Option<&'a JsonValue> {
    arr.iter().find(|c| c.get("name").and_then(|n| n.as_str()) == Some(wanted))
}

fn name_of(v: &JsonValue) -> &str {
    v.get("name").and_then(|n| n.as_str()).unwrap_or("")
}

fn value_of(v: &JsonValue) -> &str {
    v.get("value").and_then(|n| n.as_str()).unwrap_or("")
}

fn var_ref_of(v: &JsonValue) -> i64 {
    v.get("variablesReference").and_then(|n| n.as_i64()).unwrap_or(0)
}

#[test]
fn codelldb_layout_assumptions() {
    if !codelldb_available() {
        eprintln!("skipping: codelldb not on PATH");
        return;
    }
    if !rustc_available() {
        eprintln!("skipping: rustc not on PATH");
        return;
    }

    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let out = PathBuf::from(&manifest).join("tests/fixtures/target/map_probe");
        std::fs::create_dir_all(&out).unwrap();
        let src = out.join("probe.rs");
        let bin = out.join("probe");
        std::fs::write(&src, PROBE_SOURCE).unwrap();

        let compile = Command::new("rustc")
            .arg(&src).arg("-g").arg("-C").arg("opt-level=0")
            .arg("-o").arg(&bin)
            .output().unwrap();
        assert!(compile.status.success(),
            "rustc: {}", String::from_utf8_lossy(&compile.stderr));

        let sm = Arc::new(RwLock::new(SessionManager::new()));
        let tools = ToolsHandler::new(Arc::clone(&sm));

        let start = tool_call(&tools, "debugger_start", json!({
            "language": "rust",
            "program": bin.to_string_lossy(),
            "args": [],
            "stopOnEntry": true,
            "cwd": out.to_string_lossy(),
        })).await;
        let sid = start["sessionId"].as_str().unwrap().to_string();

        tool_call(&tools, "debugger_wait_for_stop", json!({
            "sessionId": &sid, "timeoutMs": 10000,
        })).await;

        let bp_line = PROBE_SOURCE.lines()
            .position(|l| l.contains("BREAKPOINT HERE"))
            .map(|i| i as u32 + 1)
            .expect("breakpoint marker");
        tool_call(&tools, "debugger_set_breakpoint", json!({
            "sessionId": &sid,
            "sourcePath": src.to_string_lossy(),
            "line": bp_line,
        })).await;

        tool_call(&tools, "debugger_continue", json!({"sessionId": &sid})).await;
        tool_call(&tools, "debugger_wait_for_stop", json!({
            "sessionId": &sid, "timeoutMs": 10000,
        })).await;

        let locals = tool_call(&tools, "debugger_get_variables",
            json!({"sessionId": &sid, "maxCount": 100})).await;
        let local_arr = locals["variables"].as_array()
            .expect("locals.variables array");

        check_vec_layout(&tools, &sid, local_arr).await;
        check_hashmap_layout(&tools, &sid, local_arr).await;
        check_btreemap_layout(&tools, &sid, local_arr).await;

        let _ = tool_call(&tools, "debugger_disconnect",
            json!({"sessionId": &sid})).await;
    });
}

async fn check_vec_layout(tools: &ToolsHandler, sid: &str, locals: &[JsonValue]) {
    let v = child_named(locals, "v")
        .expect("local `v` (Vec<i64>) missing — generator changed?");
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    assert!(ty.contains("Vec") || ty.contains("alloc::vec"),
        "Vec type assumption broken: got `{ty}` for v");

    let var_ref = var_ref_of(v);
    assert!(var_ref > 0, "expected v expandable (variablesReference > 0), got {var_ref}");

    let kids = fetch_children(tools, sid, var_ref).await;
    let indexed: Vec<(usize, &JsonValue)> = kids.iter()
        .filter_map(|c| parse_indexed_name(name_of(c)).map(|i| (i, c)))
        .collect();
    assert_eq!(indexed.len(), 3,
        "expected 3 indexed Vec children, got {} from {:?}",
        indexed.len(),
        kids.iter().map(name_of).collect::<Vec<_>>());

    let mut by_idx: std::collections::BTreeMap<usize, i64> = Default::default();
    for (i, c) in indexed {
        let raw = value_of(c);
        let parsed: i64 = raw.parse()
            .unwrap_or_else(|_| panic!("Vec[{i}] value `{raw}` not parseable as i64"));
        by_idx.insert(i, parsed);
    }
    assert_eq!(by_idx.values().copied().collect::<Vec<_>>(), vec![7, 11, 13],
        "Vec contents diverged from program (saw {by_idx:?})");
}

async fn check_hashmap_layout(tools: &ToolsHandler, sid: &str, locals: &[JsonValue]) {
    let h = child_named(locals, "h")
        .expect("local `h` (HashMap) missing — generator changed?");
    let ty = h.get("type").and_then(|t| t.as_str()).unwrap_or("");
    assert!(ty.contains("HashMap") || ty.contains("hashmap"),
        "HashMap type assumption broken: got `{ty}` for h");

    let var_ref = var_ref_of(h);
    assert!(var_ref > 0, "expected h expandable, got variablesReference={var_ref}");

    let kids = fetch_children(tools, sid, var_ref).await;
    let pair_refs: Vec<(usize, i64)> = kids.iter()
        .filter_map(|c| {
            let i = parse_indexed_name(name_of(c))?;
            Some((i, var_ref_of(c)))
        })
        .collect();
    assert_eq!(pair_refs.len(), 5,
        "expected 5 HashMap pair children (one per insert), got {} from {:?}",
        pair_refs.len(),
        kids.iter().map(name_of).collect::<Vec<_>>());

    // Drill into every pair: each must produce children named "0" (key) and
    // "1" (value). Collect (key_raw, value_raw) so we can also assert the
    // string-format invariants.
    let mut entries: Vec<(String, String)> = Vec::new();
    for (i, pref) in &pair_refs {
        assert!(*pref > 0,
            "HashMap pair [{i}] not expandable (variablesReference=0) — drill-in oracle would break");
        let pair_kids = fetch_children(tools, sid, *pref).await;
        let key = child_named(&pair_kids, "0")
            .unwrap_or_else(|| panic!("pair [{i}] missing child `0` (key); saw {:?}",
                pair_kids.iter().map(name_of).collect::<Vec<_>>()));
        let val = child_named(&pair_kids, "1")
            .unwrap_or_else(|| panic!("pair [{i}] missing child `1` (value); saw {:?}",
                pair_kids.iter().map(name_of).collect::<Vec<_>>()));
        entries.push((value_of(key).into(), value_of(val).into()));
    }

    // Every key string is `"..."`-wrapped and stripping outer quotes yields
    // the original bytes verbatim — the property the oracle's matcher relies on.
    for (k_raw, _) in &entries {
        assert!(k_raw.starts_with('"') && k_raw.ends_with('"') && k_raw.len() >= 2,
            "HashMap key not quote-wrapped: `{k_raw}`");
    }

    // Newline byte preserved as raw byte (not escaped to `\n`).
    let nl_entry = entries.iter().find(|(k, _)| {
        strip_quotes(k) == Some("with\nnewline")
    });
    assert!(nl_entry.is_some(),
        "newline-key not found unescaped; saw keys {:?}",
        entries.iter().map(|(k, _)| k).collect::<Vec<_>>());

    // Embedded `"` preserved literally (broken-JSON shape: `"with"quote"`).
    let q_entry = entries.iter().find(|(k, _)| {
        strip_quotes(k) == Some("with\"quote")
    });
    assert!(q_entry.is_some(),
        "embedded-quote key not found; saw keys {:?}",
        entries.iter().map(|(k, _)| k).collect::<Vec<_>>());

    // Spot-check a value: numeric values are bare integers, no quotes.
    let alpha = entries.iter().find(|(k, _)| strip_quotes(k) == Some("alpha"));
    let alpha = alpha.expect("alpha key missing");
    assert_eq!(alpha.1, "1", "HashMap value format changed: got `{}`", alpha.1);
}

async fn check_btreemap_layout(tools: &ToolsHandler, sid: &str, locals: &[JsonValue]) {
    let t = child_named(locals, "t")
        .expect("local `t` (BTreeMap) missing — generator changed?");
    let ty = t.get("type").and_then(|t| t.as_str()).unwrap_or("");
    assert!(ty.contains("BTreeMap") || ty.contains("btree"),
        "BTreeMap type assumption broken: got `{ty}` for t");

    let var_ref = var_ref_of(t);
    assert!(var_ref > 0, "expected t expandable, got variablesReference={var_ref}");

    let kids = fetch_children(tools, sid, var_ref).await;

    // Hard assumption that gates the oracle's BTreeMap strategy: CodeLLDB
    // does NOT synthesize indexed entries here. If this ever changes, the
    // BTreeMap oracle should switch from length-only to drill-in (see
    // HANDOFF "P1 — BTreeMap entry walk").
    let indexed: Vec<&JsonValue> = kids.iter()
        .filter(|c| parse_indexed_name(name_of(c)).is_some())
        .collect();
    assert!(indexed.is_empty(),
        "BTreeMap now exposes synthesized [N] children — upgrade oracle.rs::check_map_children for BTree drill-in. Saw: {:?}",
        indexed.iter().map(|v| name_of(v)).collect::<Vec<_>>());

    // The oracle reads `length`. Verify it's there and parses to the right
    // value with the expected type.
    let length = child_named(&kids, "length")
        .unwrap_or_else(|| panic!("BTreeMap `length` child missing — oracle broken. Saw children: {:?}",
            kids.iter().map(name_of).collect::<Vec<_>>()));
    let len_val = value_of(length);
    let parsed: i64 = len_val.parse()
        .unwrap_or_else(|_| panic!("BTreeMap `length` value `{len_val}` not parseable as i64"));
    assert_eq!(parsed, 3, "BTreeMap length mismatch: program inserted 3, length reports {parsed}");
}

async fn fetch_children(tools: &ToolsHandler, sid: &str, var_ref: i64) -> Vec<JsonValue> {
    let resp = tool_call(tools, "debugger_get_variables", json!({
        "sessionId": sid,
        "variablesReference": var_ref,
        "maxCount": 100,
    })).await;
    resp.get("variables").and_then(|v| v.as_array()).cloned().unwrap_or_default()
}

fn strip_quotes(raw: &str) -> Option<&str> {
    raw.strip_prefix('"').and_then(|r| r.strip_suffix('"'))
}
