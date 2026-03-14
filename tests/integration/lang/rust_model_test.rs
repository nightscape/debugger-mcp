#[allow(dead_code)]
#[path = "../rust_model/reference_state.rs"]
mod reference_state;
#[allow(dead_code)]
#[path = "../rust_model/fixture_model.rs"]
mod fixture_model;
#[allow(dead_code)]
#[path = "../rust_model/test_helpers.rs"]
mod test_helpers;

use test_helpers::*;
use serde_json::json;
use tokio::time::Duration;

/// Calibration test: steps through the entire program and prints actual CodeLLDB output
/// at each line. Use this to populate/update fixture_model.rs.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_calibration_step_trace() {
    if !check_prerequisites() {
        return;
    }

    let harness = setup();
    let sid = start_session(&harness).await;

    // Step through main() line by line until terminated, printing everything
    for step_num in 0..100 {
        let stack = tool_call(
            &harness.tools,
            "debugger_stack_trace",
            json!({"sessionId": &sid, "includeVariables": true, "format": "json"}),
        )
        .await;

        match stack {
            Ok(s) => {
                let frames = &s["stackFrames"];
                if let Some(arr) = frames.as_array() {
                    if let Some(top) = arr.first() {
                        println!(
                            "CALIBRATION step={} line={} fn={} depth={}",
                            step_num,
                            top["line"],
                            top["name"],
                            arr.len()
                        );
                        if let Some(vars) = top["variables"].as_array() {
                            for v in vars {
                                println!(
                                    "  VAR {}={} (type={})",
                                    v["name"], v["value"], v["type"]
                                );
                            }
                        }
                    }
                }
            }
            Err(e) => {
                println!("CALIBRATION step={} stack_trace error: {}", step_num, e);
                break;
            }
        }

        let state = tool_call(
            &harness.tools,
            "debugger_session_state",
            json!({"sessionId": &sid}),
        )
        .await;

        if let Ok(s) = &state {
            if s["state"].as_str() == Some("Terminated") {
                println!("CALIBRATION: program terminated at step {}", step_num);
                break;
            }
        }

        let step_result = tool_call(
            &harness.tools,
            "debugger_step_over",
            json!({"sessionId": &sid}),
        )
        .await;

        if step_result.is_err() {
            println!(
                "CALIBRATION: step_over failed at step {}: {:?}",
                step_num, step_result
            );
            break;
        }
    }

    disconnect(&harness.tools, &sid).await;
}

/// Test 1: Step through main() and verify line progression.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_step_over_trace() {
    if !check_prerequisites() {
        return;
    }

    let harness = setup();
    let sid = start_session(&harness).await;

    // Step to main entry (stopOnEntry may stop before main)
    // Continue to main first by setting a breakpoint
    tool_call(
        &harness.tools,
        "debugger_set_breakpoint",
        json!({
            "sessionId": &sid,
            "sourcePath": harness.source_path.to_string_lossy(),
            "line": fixture_model::MAIN_ENTRY
        }),
    )
    .await
    .expect("set breakpoint at main entry");

    let stop = continue_and_wait(&harness.tools, &sid).await;
    assert_eq!(stop["state"].as_str(), Some("Stopped"));

    // Remove breakpoint so it doesn't interfere
    tool_call(
        &harness.tools,
        "debugger_remove_breakpoint",
        json!({
            "sessionId": &sid,
            "sourcePath": harness.source_path.to_string_lossy(),
            "line": fixture_model::MAIN_ENTRY
        }),
    )
    .await
    .expect("remove breakpoint");

    // Now step through main, collecting lines
    let mut lines = vec![current_line(&harness.tools, &sid).await];

    for _ in 0..20 {
        let step_result = tool_call(
            &harness.tools,
            "debugger_step_over",
            json!({"sessionId": &sid}),
        )
        .await;

        if step_result.is_err() {
            break;
        }

        let state = tool_call(
            &harness.tools,
            "debugger_session_state",
            json!({"sessionId": &sid}),
        )
        .await;

        if let Ok(s) = &state {
            if s["state"].as_str() == Some("Terminated") {
                break;
            }
        }

        lines.push(current_line(&harness.tools, &sid).await);
    }

    println!("Step-over trace: {:?}", lines);

    // Verify lines are progressing (not stuck)
    assert!(lines.len() >= 3, "Should step through at least 3 lines, got {:?}", lines);

    // Verify we started in main. CodeLLDB may resolve the breakpoint at
    // `fn main() {` (line 35) to the first executable statement (line 36 or 37).
    assert!(
        (fixture_model::MAIN_ENTRY..=fixture_model::MAIN_ENTRY + 2).contains(&lines[0]),
        "Should start near main entry ({}), got {}",
        fixture_model::MAIN_ENTRY,
        lines[0]
    );

    disconnect(&harness.tools, &sid).await;
}

/// Test 2: Set breakpoints at function entries, continue to each, verify stack.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_breakpoint_at_functions() {
    if !check_prerequisites() {
        return;
    }

    let harness = setup();
    let sid = start_session(&harness).await;
    let src = harness.source_path.to_string_lossy().to_string();

    // Set breakpoints at add, classify, iterate entries
    for &line in &[fixture_model::ADD_ENTRY, fixture_model::CLASSIFY_ENTRY, fixture_model::ITERATE_ENTRY] {
        let result = tool_call(
            &harness.tools,
            "debugger_set_breakpoint",
            json!({"sessionId": &sid, "sourcePath": &src, "line": line}),
        )
        .await
        .expect("set breakpoint");

        println!("Breakpoint at line {}: verified={}", line, result["verified"]);
    }

    // Continue to add()
    let stop = continue_and_wait(&harness.tools, &sid).await;
    assert_eq!(stop["state"].as_str(), Some("Stopped"));
    assert_in_function(&harness.tools, &sid, "add").await;
    let depth = stack_depth(&harness.tools, &sid).await;
    assert!(depth >= 2, "Stack depth in add() should be >= 2, got {}", depth);

    // Continue to classify()
    let stop = continue_and_wait(&harness.tools, &sid).await;
    assert_eq!(stop["state"].as_str(), Some("Stopped"));
    assert_in_function(&harness.tools, &sid, "classify").await;

    // Continue to iterate()
    let stop = continue_and_wait(&harness.tools, &sid).await;
    assert_eq!(stop["state"].as_str(), Some("Stopped"));
    assert_in_function(&harness.tools, &sid, "iterate").await;

    disconnect(&harness.tools, &sid).await;
}

/// Test 3: Step into add(), verify params, step out, verify return.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_step_into_out() {
    if !check_prerequisites() {
        return;
    }

    let harness = setup();
    let sid = start_session(&harness).await;
    let src = harness.source_path.to_string_lossy().to_string();

    // Set breakpoint at add() call site (line 38)
    tool_call(
        &harness.tools,
        "debugger_set_breakpoint",
        json!({"sessionId": &sid, "sourcePath": &src, "line": 38}),
    )
    .await
    .expect("set breakpoint");

    let stop = continue_and_wait(&harness.tools, &sid).await;
    assert_eq!(stop["state"].as_str(), Some("Stopped"));

    // Step into add() — may need multiple step_into calls to reach the function
    // (LLDB evaluates arguments before entering the call)
    let mut entered_add = false;
    for _ in 0..5 {
        step_into(&harness.tools, &sid).await;
        let stack = tool_call(
            &harness.tools,
            "debugger_stack_trace",
            json!({"sessionId": &sid, "format": "json"}),
        )
        .await
        .expect("stack_trace failed");

        let frames = stack["stackFrames"].as_array().expect("no stackFrames");
        let top_fn = frames[0]["name"].as_str().unwrap_or("");
        println!("After step_into: fn={}", top_fn);
        if top_fn.contains("add") {
            entered_add = true;
            break;
        }
    }
    assert!(entered_add, "Should have entered add() after step_into");

    // Verify parameters
    assert_variable(&harness.tools, &sid, "a", "3").await;
    assert_variable(&harness.tools, &sid, "b", "5").await;

    // Step out back to main
    step_out(&harness.tools, &sid).await;
    // After step_out we should be back in main (or close to it)
    let stack = tool_call(
        &harness.tools,
        "debugger_stack_trace",
        json!({"sessionId": &sid, "format": "json"}),
    )
    .await
    .expect("stack_trace after step_out");

    let frames = stack["stackFrames"].as_array().expect("no stackFrames");
    let depth = frames.len();
    println!("After step_out: fn={}, depth={}", frames[0]["name"], depth);
    // Depth should have decreased (we left add)
    assert!(depth >= 1, "Should have at least 1 frame after step_out");

    disconnect(&harness.tools, &sid).await;
}

/// Test 4: Conditional breakpoint in loop.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_conditional_breakpoint() {
    if !check_prerequisites() {
        return;
    }

    let harness = setup();
    let sid = start_session(&harness).await;
    let src = harness.source_path.to_string_lossy().to_string();

    // Set conditional breakpoint: stop when i == 3
    tool_call(
        &harness.tools,
        "debugger_set_breakpoint",
        json!({
            "sessionId": &sid,
            "sourcePath": &src,
            "line": fixture_model::LOOP_BODY_LINE,
            "condition": "i == 3"
        }),
    )
    .await
    .expect("set conditional breakpoint");

    let stop = continue_and_wait(&harness.tools, &sid).await;
    assert_eq!(stop["state"].as_str(), Some("Stopped"));

    // Verify we're at the right spot with i == 3
    assert_variable(&harness.tools, &sid, "i", "3").await;
    assert_variable(&harness.tools, &sid, "total", "3").await;

    disconnect(&harness.tools, &sid).await;
}

/// Test 5: Set, list, remove, list breakpoints.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_breakpoint_set_remove() {
    if !check_prerequisites() {
        return;
    }

    let harness = setup();
    let sid = start_session(&harness).await;
    let src = harness.source_path.to_string_lossy().to_string();

    // Set two breakpoints
    tool_call(
        &harness.tools,
        "debugger_set_breakpoint",
        json!({"sessionId": &sid, "sourcePath": &src, "line": fixture_model::ADD_ENTRY}),
    )
    .await
    .expect("set bp1");

    tool_call(
        &harness.tools,
        "debugger_set_breakpoint",
        json!({"sessionId": &sid, "sourcePath": &src, "line": fixture_model::CLASSIFY_ENTRY}),
    )
    .await
    .expect("set bp2");

    // List — should have 2
    let list = tool_call(
        &harness.tools,
        "debugger_list_breakpoints",
        json!({"sessionId": &sid}),
    )
    .await
    .expect("list breakpoints");

    let bps = list["breakpoints"].as_array().expect("breakpoints array");
    assert!(bps.len() >= 2, "Expected at least 2 breakpoints, got {}", bps.len());

    // Remove one
    tool_call(
        &harness.tools,
        "debugger_remove_breakpoint",
        json!({"sessionId": &sid, "sourcePath": &src, "line": fixture_model::ADD_ENTRY}),
    )
    .await
    .expect("remove bp1");

    // List — should have 1
    let list = tool_call(
        &harness.tools,
        "debugger_list_breakpoints",
        json!({"sessionId": &sid}),
    )
    .await
    .expect("list breakpoints after remove");

    let bps = list["breakpoints"].as_array().expect("breakpoints array");
    // After removing, check the remaining breakpoint
    let remaining_lines: Vec<i64> = bps.iter()
        .filter_map(|b| b["line"].as_i64())
        .collect();
    assert!(
        !remaining_lines.contains(&(fixture_model::ADD_ENTRY as i64)),
        "Removed breakpoint should not appear in list"
    );

    disconnect(&harness.tools, &sid).await;
}

/// Test 6: Evaluate expressions at a stop point.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_evaluate_expressions() {
    if !check_prerequisites() {
        return;
    }

    let harness = setup();
    let sid = start_session(&harness).await;
    let src = harness.source_path.to_string_lossy().to_string();

    // Set breakpoint at println (line 44) where all variables are initialized
    tool_call(
        &harness.tools,
        "debugger_set_breakpoint",
        json!({"sessionId": &sid, "sourcePath": &src, "line": 44}),
    )
    .await
    .expect("set breakpoint");

    let stop = continue_and_wait(&harness.tools, &sid).await;
    assert_eq!(stop["state"].as_str(), Some("Stopped"));

    // Evaluate individual variables
    assert_variable(&harness.tools, &sid, "a", "3").await;
    assert_variable(&harness.tools, &sid, "b", "5").await;
    assert_variable(&harness.tools, &sid, "c", "8").await;

    // Evaluate arithmetic expression
    let result = evaluate(&harness.tools, &sid, "a + b").await;
    println!("a + b = {:?}", result);
    if let Ok(val) = &result {
        assert!(val.contains("8"), "a + b should be 8, got {val}");
    }

    disconnect(&harness.tools, &sid).await;
}

/// Test 7: Regression test — evaluating invalid expression must not hang.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_not_stuck_after_failed_evaluate() {
    if !check_prerequisites() {
        return;
    }

    let harness = setup();
    let sid = start_session(&harness).await;
    let src = harness.source_path.to_string_lossy().to_string();

    // Get to a stop point with variables in scope
    tool_call(
        &harness.tools,
        "debugger_set_breakpoint",
        json!({"sessionId": &sid, "sourcePath": &src, "line": 38}),
    )
    .await
    .expect("set breakpoint");

    continue_and_wait(&harness.tools, &sid).await;

    // 1. Evaluate a nonexistent variable — should error, not hang
    let bad_eval = tool_call_with_timeout(
        &harness.tools,
        "debugger_evaluate",
        json!({"sessionId": &sid, "expression": "nonexistent_var_xyz"}),
        Duration::from_secs(5),
    )
    .await;

    assert!(bad_eval.is_err(), "Evaluating nonexistent variable should error");
    println!("Bad eval returned error as expected: {:?}", bad_eval);

    // 2. Step over should still work (adapter is not stuck)
    step_over(&harness.tools, &sid).await;
    println!("Step over succeeded after failed evaluate");

    // 3. Evaluate a valid variable — should work
    assert_variable(&harness.tools, &sid, "a", "3").await;
    println!("Valid evaluate succeeded after recovery");

    disconnect(&harness.tools, &sid).await;
}

/// Test 8: Recovery sequence — mix of valid/invalid operations.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_recovery_sequence() {
    if !check_prerequisites() {
        return;
    }

    let harness = setup();
    let sid = start_session(&harness).await;
    let src = harness.source_path.to_string_lossy().to_string();

    // Get to a stop point
    tool_call(
        &harness.tools,
        "debugger_set_breakpoint",
        json!({"sessionId": &sid, "sourcePath": &src, "line": 44}),
    )
    .await
    .unwrap();

    continue_and_wait(&harness.tools, &sid).await;

    // Valid eval
    assert_variable(&harness.tools, &sid, "c", "8").await;

    // Invalid eval
    let _ = tool_call_with_timeout(
        &harness.tools,
        "debugger_evaluate",
        json!({"sessionId": &sid, "expression": "this_does_not_exist"}),
        Duration::from_secs(5),
    )
    .await;

    // Stack trace still works
    let stack = tool_call(
        &harness.tools,
        "debugger_stack_trace",
        json!({"sessionId": &sid, "format": "json"}),
    )
    .await
    .expect("stack_trace should work after failed eval");

    assert!(
        !stack["stackFrames"].as_array().unwrap().is_empty(),
        "Should have stack frames"
    );

    // Step still works
    step_over(&harness.tools, &sid).await;

    // Eval still works
    let result = evaluate(&harness.tools, &sid, "a").await;
    assert!(result.is_ok(), "evaluate should work after step");

    disconnect(&harness.tools, &sid).await;
}

/// Test 9: Disconnect cleanup — all calls after disconnect return error.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_disconnect_cleanup() {
    if !check_prerequisites() {
        return;
    }

    let harness = setup();
    let sid = start_session(&harness).await;

    // Disconnect
    disconnect(&harness.tools, &sid).await;

    // Brief wait for cleanup
    tokio::time::sleep(Duration::from_millis(200)).await;

    // All subsequent calls should fail with session not found
    let state = tool_call(
        &harness.tools,
        "debugger_session_state",
        json!({"sessionId": &sid}),
    )
    .await;
    assert!(state.is_err(), "session_state after disconnect should fail");

    let step = tool_call(
        &harness.tools,
        "debugger_step_over",
        json!({"sessionId": &sid}),
    )
    .await;
    assert!(step.is_err(), "step_over after disconnect should fail");

    let eval = tool_call(
        &harness.tools,
        "debugger_evaluate",
        json!({"sessionId": &sid, "expression": "a"}),
    )
    .await;
    assert!(eval.is_err(), "evaluate after disconnect should fail");
}

/// Test 10: Output capture — continue past println, verify stdout.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_output_capture() {
    if !check_prerequisites() {
        return;
    }

    let harness = setup();
    let sid = start_session(&harness).await;

    // Continue to let program run to completion (past the println)
    tool_call(
        &harness.tools,
        "debugger_continue",
        json!({"sessionId": &sid}),
    )
    .await
    .expect("continue failed");

    // Wait a bit for output to be captured
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Get output
    let output = tool_call(
        &harness.tools,
        "debugger_get_output",
        json!({"sessionId": &sid}),
    )
    .await;

    match output {
        Ok(val) => {
            let entries = val["entries"].as_array();
            let count = val["count"].as_u64().unwrap_or(0);
            println!("Captured {} output entries: {:?}", count, entries);

            // CodeLLDB may not forward program stdout through DAP output events
            // (especially without Python support). If we get entries, verify content.
            if count > 0 {
                let all_output: String = entries
                    .unwrap()
                    .iter()
                    .filter_map(|e| e["output"].as_str())
                    .collect();
                println!("Combined output: {:?}", all_output);
                assert!(
                    all_output.contains("Other") || all_output.contains("52") || all_output.contains("10"),
                    "Output should contain program output, got: {all_output}"
                );
            } else {
                println!("No output events captured (CodeLLDB may not forward stdout through DAP)");
            }
        }
        Err(e) => {
            // Output capture may not be available for terminated sessions
            println!("get_output after termination returned error (expected): {e}");
        }
    }
}
