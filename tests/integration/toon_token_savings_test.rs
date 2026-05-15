//! Measures how many tokens TOON encoding saves over the historical
//! pretty-printed JSON for representative `debugger_*` tool outputs.
//!
//! Both encoders are exercised through the exact same path the server uses
//! (`OutputFormat::encode`), and token counts use the o200k_base tokenizer
//! (GPT-4o) — the public proxy the TOON project uses for its own benchmarks.
//!
//! Run with:
//!   cargo test --test toon_token_savings_test -- --nocapture | tee toon_savings.txt

use debugger_mcp::mcp::output_format::OutputFormat;
use serde_json::{json, Value};
use tiktoken_rs::o200k_base;

/// Representative tool-result payloads, mirroring the shapes the tool handlers
/// in `src/mcp/tools/mod.rs` actually return.
fn samples() -> Vec<(&'static str, Value)> {
    vec![
        (
            "session_state",
            json!({
                "sessionId": "3cee125e-d377-49c0-b368-8fd9daeb9af0",
                "state": "Stopped",
                "details": { "reason": "breakpoint", "threadId": 1 }
            }),
        ),
        (
            "stack_trace",
            json!({
                "stackTrace": [
                    {"id": 1000, "name": "fizzbuzz", "line": 3, "column": 5, "file": "/home/u/src/lib.rs"},
                    {"id": 1001, "name": "main", "line": 42, "column": 9, "file": "/home/u/src/main.rs"},
                    {"id": 1002, "name": "std::rt::lang_start", "line": 145, "column": 20, "file": "/rust/library/std/src/rt.rs"},
                    {"id": 1003, "name": "std::rt::lang_start_internal", "line": 148, "column": 48, "file": "/rust/library/std/src/rt.rs"},
                    {"id": 1004, "name": "core::ops::function::FnOnce::call_once", "line": 250, "column": 5, "file": "/rust/library/core/src/ops/function.rs"}
                ],
                "localVariables": ["n", "result", "counter", "acc"],
                "sourceContext": "  1 | fn fizzbuzz(n: u32) -> String {\n  2 |     let result = String::new();\n> 3 |     if n % 15 == 0 {\n  4 |         return \"FizzBuzz\".to_string();\n  5 |     }"
            }),
        ),
        (
            "list_breakpoints",
            json!({
                "breakpoints": [
                    {"id": 1, "file": "/home/u/src/main.rs", "line": 10, "verified": true, "condition": null},
                    {"id": 2, "file": "/home/u/src/main.rs", "line": 25, "verified": true, "condition": "n > 5"},
                    {"id": 3, "file": "/home/u/src/lib.rs", "line": 3, "verified": true, "condition": null},
                    {"id": 4, "file": "/home/u/src/lib.rs", "line": 88, "verified": false, "condition": null}
                ]
            }),
        ),
        (
            "get_variables",
            json!({
                "variables": [
                    {"name": "n", "value": "15", "type": "u32", "variablesReference": 0},
                    {"name": "result", "value": "\"\"", "type": "alloc::string::String", "variablesReference": 0},
                    {"name": "counter", "value": "0", "type": "usize", "variablesReference": 0},
                    {"name": "items", "value": "Vec(size=3)", "type": "alloc::vec::Vec<i32>", "variablesReference": 42},
                    {"name": "acc", "value": "120", "type": "i64", "variablesReference": 0}
                ]
            }),
        ),
        (
            "evaluate",
            json!({
                "result": "FizzBuzz",
                "type": "alloc::string::String",
                "variablesReference": 0
            }),
        ),
        (
            "wait_for_stop_enriched",
            json!({
                "state": "Stopped",
                "reason": "breakpoint",
                "threadId": 1,
                "stackTrace": [
                    {"id": 2000, "name": "fizzbuzz", "line": 3, "file": "/home/u/src/lib.rs"},
                    {"id": 2001, "name": "main", "line": 42, "file": "/home/u/src/main.rs"}
                ],
                "topFrame": {"id": 2000, "name": "fizzbuzz", "line": 3, "file": "/home/u/src/lib.rs"},
                "localVariables": ["n", "result", "counter"],
                "sourceContext": "  2 |     let result = String::new();\n> 3 |     if n % 15 == 0 {\n  4 |         return \"FizzBuzz\".to_string();"
            }),
        ),
    ]
}

fn count_tokens(bpe: &tiktoken_rs::CoreBPE, s: &str) -> usize {
    bpe.encode_with_special_tokens(s).len()
}

#[test]
fn toon_saves_tokens_vs_json() {
    let bpe = o200k_base().expect("load o200k_base tokenizer");

    let mut total_json = 0usize;
    let mut total_toon = 0usize;

    println!(
        "\n{:<26} {:>10} {:>10} {:>10} {:>9}",
        "tool output", "json tok", "toon tok", "saved", "saved %"
    );
    println!("{}", "-".repeat(70));

    for (name, value) in samples() {
        let json_text = OutputFormat::Json.encode(&value);
        let toon_text = OutputFormat::Toon.encode(&value);

        // TOON must be a lossless re-encoding of the same JSON model.
        let decoded: Value =
            toon_format::decode_default(&toon_text).expect("TOON output must decode back to JSON");
        assert_eq!(decoded, value, "TOON round-trip changed the data for {name}");

        let jt = count_tokens(&bpe, &json_text);
        let tt = count_tokens(&bpe, &toon_text);
        total_json += jt;
        total_toon += tt;

        let saved = jt.saturating_sub(tt);
        let pct = if jt > 0 { saved as f64 / jt as f64 * 100.0 } else { 0.0 };
        println!("{name:<26} {jt:>10} {tt:>10} {saved:>10} {pct:>8.1}%");
    }

    println!("{}", "-".repeat(70));
    let total_saved = total_json.saturating_sub(total_toon);
    let total_pct = total_saved as f64 / total_json as f64 * 100.0;
    println!(
        "{:<26} {:>10} {:>10} {:>10} {:>8.1}%\n",
        "TOTAL", total_json, total_toon, total_saved, total_pct
    );

    // TOON should be a clear win across the representative corpus.
    assert!(
        total_toon < total_json,
        "expected TOON ({total_toon}) to use fewer tokens than JSON ({total_json})"
    );
}
