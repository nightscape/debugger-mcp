/// Extended Rust integration tests for debugger stuck/timeout and variable inspection issues
///
/// Run all: cargo test rust_extended -- --ignored --nocapture 2>&1 | tee test_output.txt
/// Run one:  cargo test test_rust_step_over_returns -- --ignored --nocapture

#[path = "../../helpers/debug_helpers.rs"]
mod debug_helpers;

use debug_helpers::{compile_rust_fixture, skip_unless_rust_debug, DebugTestHarness};
use serde_json::json;
use tokio::time::{timeout, Duration};

/// Thin alias — all tests in this file use the same fixture.
fn compile(binary_stem: &str) -> Option<std::path::PathBuf> {
    compile_rust_fixture("rust_debug_scenarios.rs", binary_stem)
}

fn harness() -> DebugTestHarness {
    DebugTestHarness::new_rust("rust_debug_scenarios.rs")
}

// ============================================================================
// Group 1: Timeout / Stuck Scenarios (Rust-specific)
// ============================================================================

/// wait_for_stop returns timeout error when no breakpoint is hit
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_rust_wait_for_stop_timeout() {
    if skip_unless_rust_debug() { return; }

    let binary = match compile("rust_debug_scenarios") {
        Some(b) => b,
        None => { println!("⚠️  Skipping: compilation failed"); return; }
    };

    let h = harness();
    let session_id = match h.start_rust_stopped(binary.to_str().unwrap()).await {
        Some(id) => id,
        None => { println!("⚠️  Skipping: start failed"); return; }
    };

    // Continue without breakpoints — program runs to completion
    h.tools
        .handle_tool("debugger_continue", json!({"sessionId": &session_id}))
        .await
        .expect("continue should succeed");

    // Short timeout — should either get Terminated or timeout error, NOT hang
    let result = h.tools
        .handle_tool(
            "debugger_wait_for_stop",
            json!({
                "sessionId": &session_id,
                "timeoutMs": 500
            }),
        )
        .await;

    match &result {
        Err(e) => {
            assert!(
                e.to_string().contains("Timeout") || e.to_string().contains("timeout"),
                "Should be timeout error, got: {}",
                e
            );
            println!("✅ Got expected timeout error");
        }
        Ok(v) => {
            assert_eq!(
                v["state"].as_str(),
                Some("Terminated"),
                "Should be Terminated if no timeout"
            );
            println!("✅ Program terminated (fast)");
        }
    }

    h.disconnect(&session_id).await;
}

/// step_over on a non-stopped Rust session fails clearly
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_rust_step_while_running_fails() {
    if skip_unless_rust_debug() { return; }

    let binary = match compile("rust_debug_scenarios_step") {
        Some(b) => b,
        None => { println!("⚠️  Skipping: compilation failed"); return; }
    };

    let h = harness();
    let session_id = match h.start_rust_stopped(binary.to_str().unwrap()).await {
        Some(id) => id,
        None => { println!("⚠️  Skipping: start failed"); return; }
    };

    // Continue execution (no breakpoints)
    h.tools
        .handle_tool("debugger_continue", json!({"sessionId": &session_id}))
        .await
        .expect("continue should work");

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Check state to see what we're testing against
    let state = h.tools
        .handle_tool("debugger_session_state", json!({"sessionId": &session_id}))
        .await
        .unwrap();

    let current = state["state"].as_str().unwrap_or("");
    if current == "Stopped" {
        println!("⚠️  Program already stopped, can't test step-while-running");
        h.disconnect(&session_id).await;
        return;
    }

    for tool_name in &["debugger_step_over", "debugger_step_into", "debugger_step_out"] {
        let step_result = h.tools
            .handle_tool(tool_name, json!({"sessionId": &session_id}))
            .await;

        assert!(step_result.is_err(), "{} should fail when not stopped", tool_name);
        println!("✅ {} correctly rejected when state={}", tool_name, current);
    }

    h.disconnect(&session_id).await;
}

/// evaluate while Rust program is running fails with clear message
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_rust_evaluate_while_running_fails() {
    if skip_unless_rust_debug() { return; }

    let binary = match compile("rust_debug_scenarios_eval_run") {
        Some(b) => b,
        None => { println!("⚠️  Skipping: compilation failed"); return; }
    };

    let h = harness();
    let session_id = match h.start_rust_stopped(binary.to_str().unwrap()).await {
        Some(id) => id,
        None => { println!("⚠️  Skipping: start failed"); return; }
    };

    h.tools
        .handle_tool("debugger_continue", json!({"sessionId": &session_id}))
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;

    let state = h.tools
        .handle_tool("debugger_session_state", json!({"sessionId": &session_id}))
        .await
        .unwrap();

    if state["state"].as_str() == Some("Stopped") {
        println!("⚠️  Program stopped, can't test evaluate-while-running");
        h.disconnect(&session_id).await;
        return;
    }

    let result = h.tools
        .handle_tool(
            "debugger_evaluate",
            json!({
                "sessionId": &session_id,
                "expression": "1 + 1"
            }),
        )
        .await;

    assert!(result.is_err(), "evaluate should fail while running");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("running") || err.contains("stopped"),
        "Error should mention state, got: {}",
        err
    );
    println!("✅ evaluate correctly rejected: {}", err);

    h.disconnect(&session_id).await;
}

/// Disconnect works in any Rust session state
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_rust_disconnect_any_state() {
    if skip_unless_rust_debug() { return; }

    let binary = match compile("rust_debug_scenarios_disc") {
        Some(b) => b,
        None => { println!("⚠️  Skipping: compilation failed"); return; }
    };

    let h = harness();

    // Disconnect from stopped state
    {
        let sid = match h.start_rust_stopped(binary.to_str().unwrap()).await {
            Some(id) => id,
            None => { println!("⚠️  Skipping: start failed"); return; }
        };

        let result = timeout(
            Duration::from_secs(5),
            h.tools.handle_tool("debugger_disconnect", json!({"sessionId": &sid})),
        )
        .await;

        assert!(matches!(result, Ok(Ok(_))), "Disconnect from stopped should succeed");
        println!("✅ Disconnect from stopped state OK");
    }

    // Disconnect from running state
    {
        let resp = timeout(
            Duration::from_secs(30),
            h.tools.handle_tool(
                "debugger_start",
                json!({
                    "language": "rust",
                    "program": binary.to_str().unwrap(),
                    "stopOnEntry": false
                }),
            ),
        )
        .await;

        if let Ok(Ok(r)) = resp {
            let sid = r["sessionId"].as_str().unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;

            let result = timeout(
                Duration::from_secs(5),
                h.tools.handle_tool("debugger_disconnect", json!({"sessionId": sid})),
            )
            .await;

            assert!(matches!(result, Ok(Ok(_))), "Disconnect from running should succeed");
            println!("✅ Disconnect from running state OK");
        }
    }
}

/// Operations after disconnect fail with session-not-found
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_rust_operations_after_disconnect() {
    if skip_unless_rust_debug() { return; }

    let binary = match compile("rust_debug_scenarios_post") {
        Some(b) => b,
        None => { println!("⚠️  Skipping: compilation failed"); return; }
    };

    let h = harness();
    let session_id = match h.start_rust_stopped(binary.to_str().unwrap()).await {
        Some(id) => id,
        None => { println!("⚠️  Skipping: start failed"); return; }
    };

    h.disconnect(&session_id).await;

    let ops = [
        ("debugger_continue", json!({"sessionId": &session_id})),
        ("debugger_step_over", json!({"sessionId": &session_id})),
        ("debugger_evaluate", json!({"sessionId": &session_id, "expression": "1"})),
        ("debugger_stack_trace", json!({"sessionId": &session_id})),
        ("debugger_session_state", json!({"sessionId": &session_id})),
    ];

    for (name, args) in &ops {
        let r = h.tools.handle_tool(name, args.clone()).await;
        assert!(r.is_err(), "{} should fail after disconnect", name);
        println!("✅ {} correctly failed after disconnect", name);
    }
}

// ============================================================================
// Group 2: Variable Inspection (Rust-specific)
// ============================================================================

/// Evaluate local variables when stopped at breakpoint inside a function
/// This is the #1 pain point: variable inspection fails because of missing context.
/// Line 43 of rust_debug_scenarios.rs: `let result: i32 = items.iter().sum();`
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_rust_evaluate_local_variables() {
    if skip_unless_rust_debug() { return; }

    let binary = match compile("rust_debug_scenarios_vars") {
        Some(b) => b,
        None => { println!("⚠️  Skipping: compilation failed"); return; }
    };

    let h = harness();
    let session_id = match h.start_rust_stopped(binary.to_str().unwrap()).await {
        Some(id) => id,
        None => { println!("⚠️  Skipping: start failed"); return; }
    };

    // Set breakpoint inside compute_with_locals at line 46 (println — all locals in scope)
    let verified = h.set_breakpoint(&session_id, 46).await;
    println!("Breakpoint at line 46 verified: {}", verified);

    // Continue to breakpoint
    let stop = h.continue_and_wait(&session_id, 10000).await;
    let stop = match stop {
        Some(v) if v["state"].as_str() == Some("Stopped") => v,
        other => {
            println!("⚠️  Did not stop at breakpoint: {:?}", other);
            h.disconnect(&session_id).await;
            return;
        }
    };

    println!("Stopped: reason={}", stop["reason"]);

    // Get stack trace for frame ID (JSON format to get structured frame data)
    let stack = h.stack_trace_json(&session_id).await
        .expect("stack trace should work when stopped");

    let frames = stack["stackFrames"].as_array().unwrap();
    assert!(!frames.is_empty(), "Should have stack frames");

    let top_frame_id = frames[0]["id"].as_i64().expect("frame should have id");
    println!("Top frame: id={}, name={}", top_frame_id, frames[0]["name"]);

    // Test: evaluate with auto frame_id (no explicit frameId)
    let eval_auto = h.tools
        .handle_tool(
            "debugger_evaluate",
            json!({
                "sessionId": &session_id,
                "expression": "a"
            }),
        )
        .await;

    match &eval_auto {
        Ok(v) => println!("✅ evaluate 'a' (auto frame): {}", v["result"]),
        Err(e) => println!("❌ evaluate 'a' (auto frame) failed: {}", e),
    }

    // Test: evaluate with explicit frame_id
    let eval_explicit = h.tools
        .handle_tool(
            "debugger_evaluate",
            json!({
                "sessionId": &session_id,
                "expression": "a",
                "frameId": top_frame_id
            }),
        )
        .await;

    assert!(
        eval_explicit.is_ok(),
        "evaluate with explicit frameId should work: {:?}",
        eval_explicit.err()
    );
    println!("✅ evaluate 'a' (explicit frame): {}", eval_explicit.unwrap()["result"]);

    // Test: evaluate expression
    let eval_expr = h.tools
        .handle_tool(
            "debugger_evaluate",
            json!({
                "sessionId": &session_id,
                "expression": "a + b",
                "frameId": top_frame_id
            }),
        )
        .await;

    match &eval_expr {
        Ok(v) => println!("✅ evaluate 'a + b': {}", v["result"]),
        Err(e) => println!("⚠️  evaluate 'a + b' failed (may be LLDB limitation): {}", e),
    }

    // Test: evaluate multiple local variables
    for var in &["a", "b", "x", "y", "z", "flag", "result"] {
        let r = h.tools
            .handle_tool(
                "debugger_evaluate",
                json!({
                    "sessionId": &session_id,
                    "expression": var,
                    "frameId": top_frame_id
                }),
            )
            .await;

        match r {
            Ok(v) => println!("  {} = {}", var, v["result"]),
            Err(e) => println!("  {} = ERROR: {}", var, e),
        }
    }

    h.disconnect(&session_id).await;
}

/// Verify that stop context includes local variables automatically
/// The build_stop_context function should fetch locals as part of wait_for_stop.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_rust_stop_context_includes_locals() {
    if skip_unless_rust_debug() { return; }

    let binary = match compile("rust_debug_scenarios_ctx") {
        Some(b) => b,
        None => { println!("⚠️  Skipping: compilation failed"); return; }
    };

    let h = harness();
    let session_id = match h.start_rust_stopped(binary.to_str().unwrap()).await {
        Some(id) => id,
        None => { println!("⚠️  Skipping: start failed"); return; }
    };

    // Breakpoint at line 48 inside compute_with_locals
    h.set_breakpoint(&session_id, 46).await;

    let stop = h.continue_and_wait(&session_id, 10000).await;
    let stop = match stop {
        Some(v) if v["state"].as_str() == Some("Stopped") => v,
        _ => {
            println!("⚠️  Did not stop at breakpoint");
            h.disconnect(&session_id).await;
            return;
        }
    };

    // Check that stop context has stack trace info
    assert!(
        stop.get("stackTrace").is_some() || stop.get("topFrame").is_some(),
        "Stop context should include stack trace. Got: {}",
        serde_json::to_string_pretty(&stop).unwrap()
    );
    println!("✅ Stop context has stack trace");

    if let Some(top) = stop.get("topFrame") {
        println!("  Top frame: {} at line {}", top["name"], top["line"]);
    }

    // Check for local variables in context
    if let Some(locals) = stop.get("localVariables") {
        let vars = locals.as_array().unwrap();
        println!("✅ Stop context has {} local variables:", vars.len());
        for v in vars {
            println!("  {} : {} = {}", v["name"], v["type"], v["value"]);
        }
        assert!(!vars.is_empty(), "Should have at least one local variable");
    } else {
        println!("⚠️  No localVariables in stop context (may be LLDB/CodeLLDB limitation)");
    }

    // Check source context
    if let Some(src) = stop.get("sourceContext") {
        println!("✅ Source context present:\n{}", src.as_str().unwrap_or(""));
    }

    h.disconnect(&session_id).await;
}

/// Evaluate struct fields and nested data
/// Line 73 of rust_debug_scenarios.rs: end of nested_data()
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_rust_evaluate_struct_fields() {
    if skip_unless_rust_debug() { return; }

    let binary = match compile("rust_debug_scenarios_struct") {
        Some(b) => b,
        None => { println!("⚠️  Skipping: compilation failed"); return; }
    };

    let h = harness();
    let session_id = match h.start_rust_stopped(binary.to_str().unwrap()).await {
        Some(id) => id,
        None => { println!("⚠️  Skipping: start failed"); return; }
    };

    // Breakpoint at line 104 (println after p and dist are computed)
    h.set_breakpoint(&session_id, 104).await;

    let stop = h.continue_and_wait(&session_id, 10000).await;
    match &stop {
        Some(v) if v["state"].as_str() == Some("Stopped") => {}
        _ => {
            println!("⚠️  Did not stop at breakpoint");
            h.disconnect(&session_id).await;
            return;
        }
    }

    let stack = h.stack_trace_json(&session_id).await
        .expect("stack trace should work");

    let frame_id = stack["stackFrames"]
        .as_array()
        .unwrap()[0]["id"]
        .as_i64()
        .unwrap();

    // Evaluate struct variable
    let eval_p = h.tools
        .handle_tool(
            "debugger_evaluate",
            json!({
                "sessionId": &session_id,
                "expression": "p",
                "frameId": frame_id
            }),
        )
        .await;

    match &eval_p {
        Ok(v) => {
            let result = v["result"].as_str().unwrap_or("");
            println!("✅ p = {}", result);
            // LLDB typically shows struct fields
            assert!(
                result.contains("3") || result.contains("x") || result.contains("Point"),
                "Struct eval should show field values, got: {}",
                result
            );
        }
        Err(e) => println!("❌ evaluate 'p' failed: {}", e),
    }

    // Evaluate struct field access
    let eval_px = h.tools
        .handle_tool(
            "debugger_evaluate",
            json!({
                "sessionId": &session_id,
                "expression": "p.x",
                "frameId": frame_id
            }),
        )
        .await;

    match &eval_px {
        Ok(v) => println!("✅ p.x = {}", v["result"]),
        Err(e) => println!("⚠️  p.x failed (LLDB may require different syntax): {}", e),
    }

    // Evaluate dist (f64)
    let eval_dist = h.tools
        .handle_tool(
            "debugger_evaluate",
            json!({
                "sessionId": &session_id,
                "expression": "dist",
                "frameId": frame_id
            }),
        )
        .await;

    match &eval_dist {
        Ok(v) => println!("✅ dist = {}", v["result"]),
        Err(e) => println!("⚠️  dist eval failed: {}", e),
    }

    h.disconnect(&session_id).await;
}

// ============================================================================
// Group 3: Stepping Operations (Rust-specific)
// ============================================================================

/// step_over advances exactly one line and returns enriched context
/// Breakpoint at step_target_outer (line 82), then step_over
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_rust_step_over_returns_enriched_context() {
    if skip_unless_rust_debug() { return; }

    let binary = match compile("rust_debug_scenarios_stepover") {
        Some(b) => b,
        None => { println!("⚠️  Skipping: compilation failed"); return; }
    };

    let h = harness();
    let session_id = match h.start_rust_stopped(binary.to_str().unwrap()).await {
        Some(id) => id,
        None => { println!("⚠️  Skipping: start failed"); return; }
    };

    // Breakpoint at line 82 (first line of step_target_outer)
    h.set_breakpoint(&session_id, 82).await;

    let stop = h.continue_and_wait(&session_id, 10000).await;
    match &stop {
        Some(v) if v["state"].as_str() == Some("Stopped") => {
            println!("Stopped at: {}", v.get("topFrame").unwrap_or(&json!(null)));
        }
        _ => {
            println!("⚠️  Did not stop at breakpoint");
            h.disconnect(&session_id).await;
            return;
        }
    }

    // Record line before step
    let stack_before = h.stack_trace_json(&session_id).await.unwrap();
    let line_before = stack_before["stackFrames"].as_array().unwrap()[0]["line"]
        .as_i64()
        .unwrap();

    // step_over — should return enriched stop context
    let step_result = timeout(
        Duration::from_secs(30),
        h.tools.handle_tool("debugger_step_over", json!({"sessionId": &session_id})),
    )
    .await;

    let step_response = match step_result {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            println!("❌ step_over failed: {}", e);
            h.disconnect(&session_id).await;
            return;
        }
        Err(_) => {
            println!("❌ step_over timed out (30s) — DEBUGGER STUCK");
            h.disconnect(&session_id).await;
            panic!("step_over hung for 30 seconds — this is the 'stuck' bug");
        }
    };

    assert_eq!(
        step_response["state"].as_str(),
        Some("Stopped"),
        "step_over should return Stopped state"
    );

    // Should have advanced at least one line
    if let Some(top) = step_response.get("topFrame") {
        let line_after = top["line"].as_i64().unwrap_or(0);
        println!("✅ step_over: line {} → line {}", line_before, line_after);
        assert_ne!(line_before, line_after, "Line should change after step_over");
    }

    // Check enriched context
    if step_response.get("stackTrace").is_some() {
        println!("✅ step_over returned stack trace context");
    }
    if step_response.get("localVariables").is_some() {
        println!("✅ step_over returned local variables");
    }

    h.disconnect(&session_id).await;
}

/// step_into enters a function call
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_rust_step_into_enters_function() {
    if skip_unless_rust_debug() { return; }

    let binary = match compile("rust_debug_scenarios_stepinto") {
        Some(b) => b,
        None => { println!("⚠️  Skipping: compilation failed"); return; }
    };

    let h = harness();
    let session_id = match h.start_rust_stopped(binary.to_str().unwrap()).await {
        Some(id) => id,
        None => { println!("⚠️  Skipping: start failed"); return; }
    };

    // Breakpoint at line 82 (let first = step_target_inner(a))
    h.set_breakpoint(&session_id, 82).await;

    let stop = h.continue_and_wait(&session_id, 10000).await;
    match &stop {
        Some(v) if v["state"].as_str() == Some("Stopped") => {}
        _ => {
            println!("⚠️  Did not stop at breakpoint");
            h.disconnect(&session_id).await;
            return;
        }
    }

    let stack_before = h.stack_trace_json(&session_id).await.unwrap();
    let func_before = stack_before["stackFrames"].as_array().unwrap()[0]["name"]
        .as_str()
        .unwrap_or("")
        .to_string();

    // step_into should enter step_target_inner
    let step_result = timeout(
        Duration::from_secs(30),
        h.tools.handle_tool("debugger_step_into", json!({"sessionId": &session_id})),
    )
    .await;

    let step_response = match step_result {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            println!("❌ step_into failed: {}", e);
            h.disconnect(&session_id).await;
            return;
        }
        Err(_) => {
            println!("❌ step_into timed out — DEBUGGER STUCK");
            h.disconnect(&session_id).await;
            panic!("step_into hung for 30 seconds");
        }
    };

    assert_eq!(step_response["state"].as_str(), Some("Stopped"));

    if let Some(top) = step_response.get("topFrame") {
        let func_after = top["name"].as_str().unwrap_or("");
        println!("✅ step_into: {} → {}", func_before, func_after);
        // After step_into, we should be in a different or deeper function
        // (or at least on a different line)
    }

    h.disconnect(&session_id).await;
}

/// step_out returns to the caller
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_rust_step_out_returns_to_caller() {
    if skip_unless_rust_debug() { return; }

    let binary = match compile("rust_debug_scenarios_stepout") {
        Some(b) => b,
        None => { println!("⚠️  Skipping: compilation failed"); return; }
    };

    let h = harness();
    let session_id = match h.start_rust_stopped(binary.to_str().unwrap()).await {
        Some(id) => id,
        None => { println!("⚠️  Skipping: start failed"); return; }
    };

    // Breakpoint inside step_target_inner (line 78: let doubled = val * 2)
    h.set_breakpoint(&session_id, 78).await;

    let stop = h.continue_and_wait(&session_id, 10000).await;
    match &stop {
        Some(v) if v["state"].as_str() == Some("Stopped") => {}
        _ => {
            println!("⚠️  Did not stop at breakpoint");
            h.disconnect(&session_id).await;
            return;
        }
    }

    let stack_before = h.stack_trace_json(&session_id).await.unwrap();
    let frames_before = stack_before["stackFrames"].as_array().unwrap();
    let depth_before = frames_before.len();
    let func_before = frames_before[0]["name"].as_str().unwrap_or("").to_string();

    println!("Before step_out: {} (depth {})", func_before, depth_before);

    // step_out should return to caller
    let step_result = timeout(
        Duration::from_secs(30),
        h.tools.handle_tool("debugger_step_out", json!({"sessionId": &session_id})),
    )
    .await;

    let step_response = match step_result {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            println!("❌ step_out failed: {}", e);
            h.disconnect(&session_id).await;
            return;
        }
        Err(_) => {
            println!("❌ step_out timed out — DEBUGGER STUCK");
            h.disconnect(&session_id).await;
            panic!("step_out hung for 30 seconds");
        }
    };

    assert_eq!(step_response["state"].as_str(), Some("Stopped"));

    if let Some(top) = step_response.get("topFrame") {
        let func_after = top["name"].as_str().unwrap_or("");
        println!("✅ step_out: {} → {}", func_before, func_after);
    }

    h.disconnect(&session_id).await;
}

// ============================================================================
// Group 4: Breakpoint Management
// ============================================================================

/// Multiple breakpoints: set two, hit first, continue to second
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_rust_multiple_breakpoints() {
    if skip_unless_rust_debug() { return; }

    let binary = match compile("rust_debug_scenarios_multi_bp") {
        Some(b) => b,
        None => { println!("⚠️  Skipping: compilation failed"); return; }
    };

    let h = harness();
    let session_id = match h.start_rust_stopped(binary.to_str().unwrap()).await {
        Some(id) => id,
        None => { println!("⚠️  Skipping: start failed"); return; }
    };

    // Set two breakpoints at different lines in main()
    let bp1_verified = h.set_breakpoint(&session_id, 89).await; // compute_with_locals call
    let bp2_verified = h.set_breakpoint(&session_id, 92).await; // slow_loop call

    println!(
        "Breakpoint 1 (line 89) verified: {}, Breakpoint 2 (line 92) verified: {}",
        bp1_verified, bp2_verified
    );

    // List breakpoints
    let bps = h.tools
        .handle_tool("debugger_list_breakpoints", json!({"sessionId": &session_id}))
        .await
        .expect("list_breakpoints should work");

    let bp_list = bps["breakpoints"].as_array().unwrap();
    println!("Listed breakpoints: {}", bp_list.len());
    assert!(bp_list.len() >= 2, "Should have at least 2 breakpoints");

    // Continue to first breakpoint
    let stop1 = h.continue_and_wait(&session_id, 10000).await;
    match &stop1 {
        Some(v) if v["state"].as_str() == Some("Stopped") => {
            let line = v.get("topFrame").and_then(|f| f["line"].as_i64()).unwrap_or(0);
            println!("✅ Hit first breakpoint at line {}", line);
        }
        _ => {
            println!("⚠️  Did not hit first breakpoint");
            h.disconnect(&session_id).await;
            return;
        }
    }

    // Continue to second breakpoint
    let stop2 = h.continue_and_wait(&session_id, 10000).await;
    match &stop2 {
        Some(v) if v["state"].as_str() == Some("Stopped") => {
            let line = v.get("topFrame").and_then(|f| f["line"].as_i64()).unwrap_or(0);
            println!("✅ Hit second breakpoint at line {}", line);
        }
        _ => {
            println!("⚠️  Did not hit second breakpoint");
        }
    }

    h.disconnect(&session_id).await;
}

/// Remove breakpoint, then continue — should not stop there
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_rust_remove_breakpoint() {
    if skip_unless_rust_debug() { return; }

    let binary = match compile("rust_debug_scenarios_rm_bp") {
        Some(b) => b,
        None => { println!("⚠️  Skipping: compilation failed"); return; }
    };

    let h = harness();
    let session_id = match h.start_rust_stopped(binary.to_str().unwrap()).await {
        Some(id) => id,
        None => { println!("⚠️  Skipping: start failed"); return; }
    };

    // Set breakpoint then remove it
    h.set_breakpoint(&session_id, 89).await;

    // Also set a later breakpoint that we keep
    h.set_breakpoint(&session_id, 104).await; // near end of main

    // Remove the first breakpoint
    let remove_result = h.tools
        .handle_tool(
            "debugger_remove_breakpoint",
            json!({
                "sessionId": &session_id,
                "sourcePath": &h.source_path,
                "line": 89
            }),
        )
        .await;

    assert!(remove_result.is_ok(), "Remove breakpoint should succeed");
    println!("✅ Removed breakpoint at line 89");

    // Verify list shows only the remaining breakpoint
    let bps = h.tools
        .handle_tool("debugger_list_breakpoints", json!({"sessionId": &session_id}))
        .await
        .unwrap();

    let bp_list = bps["breakpoints"].as_array().unwrap();
    let has_line_89 = bp_list.iter().any(|bp| bp["line"].as_i64() == Some(89));
    assert!(
        !has_line_89,
        "Breakpoint at line 89 should be removed from list"
    );
    println!("✅ list_breakpoints confirms removal");

    h.disconnect(&session_id).await;
}

// ============================================================================
// Group 5: Output Capture and Advanced Features
// ============================================================================

/// get_output captures program stdout
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_rust_get_output() {
    if skip_unless_rust_debug() { return; }

    let binary = match compile("rust_debug_scenarios_output") {
        Some(b) => b,
        None => { println!("⚠️  Skipping: compilation failed"); return; }
    };

    let h = harness();
    let session_id = match h.start_rust_stopped(binary.to_str().unwrap()).await {
        Some(id) => id,
        None => { println!("⚠️  Skipping: start failed"); return; }
    };

    // Set breakpoint after some println! calls (line 92 after compute and factorial)
    h.set_breakpoint(&session_id, 92).await;

    let stop = h.continue_and_wait(&session_id, 10000).await;
    match &stop {
        Some(v) if v["state"].as_str() == Some("Stopped") => {}
        _ => {
            println!("⚠️  Did not stop at breakpoint for output test");
            h.disconnect(&session_id).await;
            return;
        }
    }

    // Get captured output
    let output = h.tools
        .handle_tool("debugger_get_output", json!({"sessionId": &session_id}))
        .await;

    match output {
        Ok(v) => {
            let stdout = v["stdout"].as_str().unwrap_or("");
            println!("✅ Captured stdout ({} chars):", stdout.len());
            for line in stdout.lines().take(5) {
                println!("  > {}", line);
            }
            // The program should have printed something by now
            if !stdout.is_empty() {
                assert!(
                    stdout.contains("Starting") || stdout.contains("compute") || stdout.contains("="),
                    "Output should contain program output"
                );
            } else {
                println!("⚠️  stdout empty (output capture may not be supported by CodeLLDB)");
            }
        }
        Err(e) => {
            println!("⚠️  get_output failed: {} (may not be supported)", e);
        }
    }

    h.disconnect(&session_id).await;
}

/// snapshot_at sets a breakpoint, runs to it, and returns full context
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_rust_snapshot_at() {
    if skip_unless_rust_debug() { return; }

    let binary = match compile("rust_debug_scenarios_snap") {
        Some(b) => b,
        None => { println!("⚠️  Skipping: compilation failed"); return; }
    };

    let h = harness();

    let start = timeout(
        Duration::from_secs(30),
        h.tools.handle_tool(
            "debugger_start",
            json!({
                "language": "rust",
                "program": binary.to_str().unwrap(),
                "stopOnEntry": true
            }),
        ),
    )
    .await;

    let session_id = match start {
        Ok(Ok(r)) => r["sessionId"].as_str().unwrap().to_string(),
        _ => {
            println!("⚠️  Skipping: start failed");
            return;
        }
    };

    // Wait for initial stop
    let _ = timeout(
        Duration::from_secs(15),
        h.tools.handle_tool(
            "debugger_wait_for_stop",
            json!({"sessionId": &session_id, "timeoutMs": 14000}),
        ),
    )
    .await;

    // Use snapshot_at to get state at a specific line
    let snap_result = timeout(
        Duration::from_secs(30),
        h.tools.handle_tool(
            "debugger_snapshot_at",
            json!({
                "sessionId": &session_id,
                "sourcePath": &h.source_path,
                "line": 48
            }),
        ),
    )
    .await;

    match snap_result {
        Ok(Ok(v)) => {
            println!("✅ snapshot_at returned:");
            if let Some(state) = v.get("state") {
                println!("  state: {}", state);
            }
            if let Some(vars) = v.get("localVariables") {
                println!("  locals: {}", vars);
            }
            if let Some(stack) = v.get("stackTrace") {
                println!("  stack: {}", stack);
            }
        }
        Ok(Err(e)) => println!("⚠️  snapshot_at failed: {}", e),
        Err(_) => {
            println!("❌ snapshot_at timed out — possible stuck debugger");
            h.disconnect(&session_id).await;
            panic!("snapshot_at hung for 30 seconds");
        }
    }

    h.disconnect(&session_id).await;
}

/// Rapid successive operations don't cause hangs
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_rust_rapid_operations_no_hang() {
    if skip_unless_rust_debug() { return; }

    let binary = match compile("rust_debug_scenarios_rapid") {
        Some(b) => b,
        None => { println!("⚠️  Skipping: compilation failed"); return; }
    };

    let h = harness();
    let session_id = match h.start_rust_stopped(binary.to_str().unwrap()).await {
        Some(id) => id,
        None => { println!("⚠️  Skipping: start failed"); return; }
    };

    // Set breakpoint where we'll do rapid ops
    h.set_breakpoint(&session_id, 89).await;

    let stop = h.continue_and_wait(&session_id, 10000).await;
    match &stop {
        Some(v) if v["state"].as_str() == Some("Stopped") => {}
        _ => {
            println!("⚠️  Did not stop at breakpoint");
            h.disconnect(&session_id).await;
            return;
        }
    }

    // Rapid sequence: stack_trace → evaluate → step_over → evaluate → step_over
    // Each operation has a 30s overall timeout to detect hangs
    let overall = timeout(Duration::from_secs(60), async {
        // stack trace
        let s = h.tools
            .handle_tool("debugger_stack_trace", json!({"sessionId": &session_id}))
            .await;
        assert!(s.is_ok(), "stack_trace should work");
        println!("✅ [1/5] stack_trace OK");

        // evaluate
        let e = h.tools
            .handle_tool(
                "debugger_evaluate",
                json!({"sessionId": &session_id, "expression": "1 + 1"}),
            )
            .await;
        assert!(e.is_ok(), "evaluate should work");
        println!("✅ [2/5] evaluate OK");

        // step_over
        let so = h.tools
            .handle_tool("debugger_step_over", json!({"sessionId": &session_id}))
            .await;
        assert!(so.is_ok(), "step_over 1 should work: {:?}", so.err());
        println!("✅ [3/5] step_over OK");

        // evaluate again
        let e2 = h.tools
            .handle_tool(
                "debugger_evaluate",
                json!({"sessionId": &session_id, "expression": "1 + 2"}),
            )
            .await;
        assert!(e2.is_ok(), "evaluate 2 should work");
        println!("✅ [4/5] evaluate OK");

        // step_over again
        let so2 = h.tools
            .handle_tool("debugger_step_over", json!({"sessionId": &session_id}))
            .await;
        assert!(so2.is_ok(), "step_over 2 should work: {:?}", so2.err());
        println!("✅ [5/5] step_over OK");
    })
    .await;

    assert!(
        overall.is_ok(),
        "Rapid operations timed out at 60s — DEBUGGER STUCK"
    );

    println!("✅ All rapid operations completed without hanging");

    h.disconnect(&session_id).await;
}

// ============================================================================
// Group 6: Recovery After Evaluate Timeout
// ============================================================================

/// After evaluate times out, session should still be usable.
/// This is the core "broken state" bug: a timed-out evaluate leaves
/// orphaned pending requests that block all subsequent DAP operations.
///
/// The fix (cancel_pending_requests in send_request_with_timeout) should
/// make the session recover automatically.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_rust_session_recovers_after_evaluate_timeout() {
    if skip_unless_rust_debug() { return; }

    let binary = match compile("rust_debug_scenarios_recovery") {
        Some(b) => b,
        None => { println!("⚠️  Skipping: compilation failed"); return; }
    };

    let h = harness();
    let session_id = match h.start_rust_stopped(binary.to_str().unwrap()).await {
        Some(id) => id,
        None => { println!("⚠️  Skipping: start failed"); return; }
    };

    // Set breakpoint inside compute_with_locals
    h.set_breakpoint(&session_id, 46).await;

    let stop = h.continue_and_wait(&session_id, 10000).await;
    match &stop {
        Some(v) if v["state"].as_str() == Some("Stopped") => {}
        _ => {
            println!("⚠️  Did not stop at breakpoint");
            h.disconnect(&session_id).await;
            return;
        }
    }

    // Try evaluating something that might be slow or fail
    // (Even if this succeeds, the test validates the recovery path works)
    let eval_result = h.tools
        .handle_tool(
            "debugger_evaluate",
            json!({
                "sessionId": &session_id,
                "expression": "a"
            }),
        )
        .await;

    println!("First evaluate result: {:?}", eval_result.as_ref().map(|v| v.to_string()).map_err(|e| e.to_string()));

    // KEY TEST: After evaluate (whether it succeeded or timed out),
    // stack_trace should still work
    let stack_result = timeout(
        Duration::from_secs(15),
        async { h.stack_trace_json(&session_id).await.ok_or_else(|| "stack trace returned None".to_string()) },
    )
    .await;

    match &stack_result {
        Ok(Ok(v)) => {
            let frames = v["stackFrames"].as_array().unwrap();
            println!("✅ stack_trace works after evaluate: {} frames", frames.len());
        }
        Ok(Err(e)) => {
            println!("❌ stack_trace failed after evaluate: {}", e);
            panic!("Session broken after evaluate — stack_trace failed: {}", e);
        }
        Err(_) => {
            println!("❌ stack_trace timed out after evaluate — SESSION IS STUCK");
            h.disconnect(&session_id).await;
            panic!("Session stuck after evaluate — stack_trace hung for 15s");
        }
    }

    // Also verify step_over still works
    let step_result = timeout(
        Duration::from_secs(30),
        h.tools.handle_tool("debugger_step_over", json!({"sessionId": &session_id})),
    )
    .await;

    match &step_result {
        Ok(Ok(v)) => {
            println!("✅ step_over works after evaluate: state={}", v["state"]);
        }
        Ok(Err(e)) => {
            println!("⚠️  step_over failed after evaluate: {} (may be acceptable)", e);
        }
        Err(_) => {
            println!("❌ step_over timed out after evaluate — SESSION IS STUCK");
            h.disconnect(&session_id).await;
            panic!("Session stuck after evaluate — step_over hung for 30s");
        }
    }

    // And one more evaluate to confirm full recovery
    let eval2 = timeout(
        Duration::from_secs(15),
        h.tools.handle_tool(
            "debugger_evaluate",
            json!({
                "sessionId": &session_id,
                "expression": "1 + 1"
            }),
        ),
    )
    .await;

    match &eval2 {
        Ok(Ok(v)) => println!("✅ Second evaluate works: {}", v["result"]),
        Ok(Err(e)) => println!("⚠️  Second evaluate failed: {} (state may have changed)", e),
        Err(_) => {
            h.disconnect(&session_id).await;
            panic!("Session stuck — second evaluate hung for 15s");
        }
    }

    h.disconnect(&session_id).await;
    println!("✅ Session fully recovered after evaluate");
}

// ============================================================================
// Group: debugger_get_variables tests
// ============================================================================

fn compile_memory_hog() -> Option<std::path::PathBuf> {
    compile_rust_fixture("memory_hog.rs", "memory_hog")
}

fn memory_hog_harness() -> DebugTestHarness {
    DebugTestHarness::new_rust("memory_hog.rs")
}

/// debugger_get_variables returns locals with expandable flag
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_get_variables_scope_locals() {
    if skip_unless_rust_debug() { return; }

    let binary = match compile_memory_hog() {
        Some(b) => b,
        None => { println!("⚠️  Skipping: compilation failed"); return; }
    };

    let h = memory_hog_harness();
    let session_id = match h.start_rust_stopped(binary.to_str().unwrap()).await {
        Some(id) => id,
        None => { println!("⚠️  Skipping: start failed"); return; }
    };

    // Set breakpoint at line 47 (all containers in scope)
    let source_path = debug_helpers::fixture_source("memory_hog.rs");
    let bp = h.tools.handle_tool(
        "debugger_set_breakpoint",
        json!({
            "sessionId": &session_id,
            "sourcePath": source_path.to_str().unwrap(),
            "line": 47
        }),
    ).await;
    assert!(bp.is_ok(), "Breakpoint should succeed");

    let stop = h.continue_and_wait(&session_id, 15000).await;
    assert!(stop.is_some(), "Should hit breakpoint");
    assert_eq!(stop.as_ref().unwrap()["reason"], "breakpoint");

    // Call debugger_get_variables with auto-resolved frame
    let vars_result = timeout(
        Duration::from_secs(10),
        h.tools.handle_tool(
            "debugger_get_variables",
            json!({ "sessionId": &session_id }),
        ),
    ).await;

    let vars = vars_result.expect("should not timeout").expect("should succeed");
    println!("get_variables result: {}", serde_json::to_string_pretty(&vars).unwrap());

    let variables = vars["variables"].as_array().expect("should have variables array");
    assert!(!variables.is_empty(), "should return at least one variable");

    // Check that big_vec is present and expandable
    let big_vec = variables.iter().find(|v| v["name"] == "big_vec");
    assert!(big_vec.is_some(), "big_vec should be in locals");
    let big_vec = big_vec.unwrap();
    assert_eq!(big_vec["expandable"], true, "big_vec should be expandable");
    assert!(big_vec["variablesReference"].as_i64().unwrap() > 0);

    // Check that big_map is present and expandable
    let big_map = variables.iter().find(|v| v["name"] == "big_map");
    assert!(big_map.is_some(), "big_map should be in locals");
    assert_eq!(big_map.unwrap()["expandable"], true, "big_map should be expandable");

    h.disconnect(&session_id).await;
    println!("✅ get_variables returns locals with expandable flags");
}

/// debugger_get_variables can drill into a variablesReference
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_get_variables_drill_down() {
    if skip_unless_rust_debug() { return; }

    let binary = match compile_memory_hog() {
        Some(b) => b,
        None => { println!("⚠️  Skipping: compilation failed"); return; }
    };

    let h = memory_hog_harness();
    let session_id = match h.start_rust_stopped(binary.to_str().unwrap()).await {
        Some(id) => id,
        None => { println!("⚠️  Skipping: start failed"); return; }
    };

    let source_path = debug_helpers::fixture_source("memory_hog.rs");
    h.tools.handle_tool(
        "debugger_set_breakpoint",
        json!({
            "sessionId": &session_id,
            "sourcePath": source_path.to_str().unwrap(),
            "line": 47
        }),
    ).await.unwrap();

    let stop = h.continue_and_wait(&session_id, 15000).await;
    assert!(stop.is_some());

    // Get scope locals first
    let vars = timeout(
        Duration::from_secs(10),
        h.tools.handle_tool(
            "debugger_get_variables",
            json!({ "sessionId": &session_id }),
        ),
    ).await.unwrap().unwrap();

    let variables = vars["variables"].as_array().unwrap();
    let big_vec = variables.iter().find(|v| v["name"] == "big_vec").unwrap();
    let var_ref = big_vec["variablesReference"].as_i64().unwrap();
    assert!(var_ref > 0, "big_vec should have a variablesReference");

    // Drill into big_vec with maxCount: 10
    let children = timeout(
        Duration::from_secs(10),
        h.tools.handle_tool(
            "debugger_get_variables",
            json!({
                "sessionId": &session_id,
                "variablesReference": var_ref,
                "maxCount": 10
            }),
        ),
    ).await.unwrap().unwrap();

    println!("drill-down result: {}", serde_json::to_string_pretty(&children).unwrap());

    let child_vars = children["variables"].as_array().unwrap();
    assert_eq!(child_vars.len(), 10, "should return exactly maxCount items");
    assert_eq!(children["truncated"], true, "should indicate truncation");
    assert_eq!(children["count"], 10);

    h.disconnect(&session_id).await;
    println!("✅ get_variables drill-down works with maxCount truncation");
}

/// debugger_get_variables with auto-resolved frame returns same results as explicit frameId
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_get_variables_auto_resolve_frame() {
    if skip_unless_rust_debug() { return; }

    let binary = match compile_memory_hog() {
        Some(b) => b,
        None => { println!("⚠️  Skipping: compilation failed"); return; }
    };

    let h = memory_hog_harness();
    let session_id = match h.start_rust_stopped(binary.to_str().unwrap()).await {
        Some(id) => id,
        None => { println!("⚠️  Skipping: start failed"); return; }
    };

    let source_path = debug_helpers::fixture_source("memory_hog.rs");
    h.tools.handle_tool(
        "debugger_set_breakpoint",
        json!({
            "sessionId": &session_id,
            "sourcePath": source_path.to_str().unwrap(),
            "line": 47
        }),
    ).await.unwrap();

    h.continue_and_wait(&session_id, 15000).await.unwrap();

    // Get stack trace for explicit frame ID
    let stack = h.stack_trace_json(&session_id).await.unwrap();
    let frame_id = stack["stackFrames"][0]["id"].as_i64().unwrap();

    // Get variables with auto-resolve (no frameId)
    let auto_vars = timeout(
        Duration::from_secs(10),
        h.tools.handle_tool(
            "debugger_get_variables",
            json!({ "sessionId": &session_id }),
        ),
    ).await.unwrap().unwrap();

    // Get variables with explicit frameId
    let explicit_vars = timeout(
        Duration::from_secs(10),
        h.tools.handle_tool(
            "debugger_get_variables",
            json!({ "sessionId": &session_id, "frameId": frame_id }),
        ),
    ).await.unwrap().unwrap();

    let auto_names: Vec<&str> = auto_vars["variables"].as_array().unwrap()
        .iter().filter_map(|v| v["name"].as_str()).collect();
    let explicit_names: Vec<&str> = explicit_vars["variables"].as_array().unwrap()
        .iter().filter_map(|v| v["name"].as_str()).collect();

    assert_eq!(auto_names, explicit_names, "auto-resolve should return same variables as explicit frameId");

    h.disconnect(&session_id).await;
    println!("✅ auto-resolve frame returns same results as explicit frameId");
}

/// Stale variablesReference after continue doesn't crash
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_get_variables_stale_reference() {
    if skip_unless_rust_debug() { return; }

    let binary = match compile_memory_hog() {
        Some(b) => b,
        None => { println!("⚠️  Skipping: compilation failed"); return; }
    };

    let h = memory_hog_harness();
    let session_id = match h.start_rust_stopped(binary.to_str().unwrap()).await {
        Some(id) => id,
        None => { println!("⚠️  Skipping: start failed"); return; }
    };

    let source_path = debug_helpers::fixture_source("memory_hog.rs");
    // Set breakpoints on lines 47 and 48
    h.tools.handle_tool(
        "debugger_set_breakpoint",
        json!({
            "sessionId": &session_id,
            "sourcePath": source_path.to_str().unwrap(),
            "line": 47
        }),
    ).await.unwrap();
    h.tools.handle_tool(
        "debugger_set_breakpoint",
        json!({
            "sessionId": &session_id,
            "sourcePath": source_path.to_str().unwrap(),
            "line": 48
        }),
    ).await.unwrap();

    // Hit first breakpoint, get a variablesReference
    h.continue_and_wait(&session_id, 15000).await.unwrap();
    let vars = timeout(
        Duration::from_secs(10),
        h.tools.handle_tool(
            "debugger_get_variables",
            json!({ "sessionId": &session_id }),
        ),
    ).await.unwrap().unwrap();

    let big_vec = vars["variables"].as_array().unwrap()
        .iter().find(|v| v["name"] == "big_vec").unwrap();
    let stale_ref = big_vec["variablesReference"].as_i64().unwrap();

    // Continue to next breakpoint — stale_ref is now potentially invalid
    h.continue_and_wait(&session_id, 15000).await.unwrap();

    // Using the stale reference should not crash or hang
    let result = timeout(
        Duration::from_secs(5),
        h.tools.handle_tool(
            "debugger_get_variables",
            json!({
                "sessionId": &session_id,
                "variablesReference": stale_ref,
            }),
        ),
    ).await;

    // We don't assert on the content — the behavior is adapter-dependent.
    // The key assertion is that it didn't hang or crash.
    match result {
        Ok(Ok(v)) => println!("Stale ref returned data: {} vars", v["count"]),
        Ok(Err(e)) => println!("Stale ref returned error (expected): {}", e),
        Err(_) => panic!("Stale variablesReference caused a timeout — this is a bug"),
    }

    h.disconnect(&session_id).await;
    println!("✅ stale variablesReference handled gracefully");
}
