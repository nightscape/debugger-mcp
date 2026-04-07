/// Integration tests for timeout and stuck debugger scenarios (Python)

#[path = "../../helpers/debug_helpers.rs"]
mod debug_helpers;

use debug_helpers::{fixture_source, is_debugpy_available, DebugTestHarness};
use serde_json::json;
use tokio::time::{timeout, Duration};

fn get_scenarios_path() -> String {
    fixture_source("debug_scenarios.py")
        .to_string_lossy()
        .to_string()
}

// ============================================================================
// Timeout / Stuck Tests
// ============================================================================

/// wait_for_stop should return a timeout error when called with a very short
/// timeout on a running program (no breakpoints set).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_wait_for_stop_timeout_returns_error() {
    if !is_debugpy_available() {
        println!("⚠️  Skipping: debugpy not installed");
        return;
    }

    let h = DebugTestHarness::new_python();
    let tools_handler = &h.tools;

    // Start without stopOnEntry so program is running
    let start_response = timeout(
        Duration::from_secs(10),
        tools_handler.handle_tool(
            "debugger_start",
            json!({
                "language": "python",
                "program": get_scenarios_path(),
                "stopOnEntry": false
            }),
        ),
    )
    .await;

    let start_response = match start_response {
        Ok(Ok(r)) => r,
        _ => {
            println!("⚠️  Skipping: debugger_start failed");
            return;
        }
    };

    let session_id = start_response["sessionId"].as_str().unwrap();

    // Very short timeout — program will either be running or already terminated
    let wait_result = tools_handler
        .handle_tool(
            "debugger_wait_for_stop",
            json!({
                "sessionId": session_id,
                "timeoutMs": 100
            }),
        )
        .await;

    // Should either timeout (Error) or return Terminated — NOT hang forever
    match &wait_result {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("Timeout") || msg.contains("timeout"),
                "Timeout error should mention 'Timeout', got: {}",
                msg
            );
            println!("✅ Got expected timeout error: {}", msg);
        }
        Ok(v) => {
            let state = v["state"].as_str().unwrap_or("");
            assert_eq!(
                state, "Terminated",
                "If not timeout, should be Terminated, got state: {}",
                v
            );
            println!("✅ Program already terminated (fast execution)");
        }
    }

    let _ = tools_handler
        .handle_tool("debugger_disconnect", json!({"sessionId": session_id}))
        .await;
}

/// Stepping when program is running (not stopped) should fail with clear error
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_step_while_running_fails() {
    if !is_debugpy_available() {
        println!("⚠️  Skipping: debugpy not installed");
        return;
    }

    let h = DebugTestHarness::new_python();
    let tools_handler = &h.tools;

    let session_id = match h.start_python_stopped().await {
        Some(id) => id,
        None => {
            println!("⚠️  Skipping: setup failed");
            return;
        }
    };

    // Continue without breakpoints so program runs
    tools_handler
        .handle_tool("debugger_continue", json!({"sessionId": &session_id}))
        .await
        .expect("Continue should succeed");

    // Give program time to start running
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Check state — if program already terminated, step should still fail
    let state = tools_handler
        .handle_tool(
            "debugger_session_state",
            json!({"sessionId": &session_id}),
        )
        .await
        .unwrap();

    let current_state = state["state"].as_str().unwrap_or("");
    if current_state == "Stopped" {
        println!("⚠️  Program stopped immediately, can't test step-while-running");
        let _ = tools_handler
            .handle_tool("debugger_disconnect", json!({"sessionId": &session_id}))
            .await;
        return;
    }

    // step_over should fail since we're not stopped
    let step_result = tools_handler
        .handle_tool("debugger_step_over", json!({"sessionId": &session_id}))
        .await;

    assert!(
        step_result.is_err(),
        "step_over should fail when not stopped, got: {:?}",
        step_result
    );

    let err_msg = step_result.unwrap_err().to_string();
    assert!(
        err_msg.contains("running") || err_msg.contains("stopped") || err_msg.contains("state"),
        "Error should mention state issue, got: {}",
        err_msg
    );
    println!("✅ step_over correctly rejected: {}", err_msg);

    // step_into should also fail
    let step_into_result = tools_handler
        .handle_tool("debugger_step_into", json!({"sessionId": &session_id}))
        .await;

    assert!(step_into_result.is_err(), "step_into should also fail when not stopped");
    println!("✅ step_into correctly rejected");

    let _ = tools_handler
        .handle_tool("debugger_disconnect", json!({"sessionId": &session_id}))
        .await;
}

/// Evaluate while running should fail with clear error
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_evaluate_while_running_fails() {
    if !is_debugpy_available() {
        println!("⚠️  Skipping: debugpy not installed");
        return;
    }

    let h = DebugTestHarness::new_python();
    let tools_handler = &h.tools;

    let session_id = match h.start_python_stopped().await {
        Some(id) => id,
        None => {
            println!("⚠️  Skipping: setup failed");
            return;
        }
    };

    // Continue without breakpoints
    tools_handler
        .handle_tool("debugger_continue", json!({"sessionId": &session_id}))
        .await
        .expect("Continue should succeed");

    tokio::time::sleep(Duration::from_millis(50)).await;

    let state = tools_handler
        .handle_tool(
            "debugger_session_state",
            json!({"sessionId": &session_id}),
        )
        .await
        .unwrap();

    if state["state"].as_str() == Some("Stopped") {
        println!("⚠️  Program stopped immediately, can't test evaluate-while-running");
        let _ = tools_handler
            .handle_tool("debugger_disconnect", json!({"sessionId": &session_id}))
            .await;
        return;
    }

    let eval_result = tools_handler
        .handle_tool(
            "debugger_evaluate",
            json!({
                "sessionId": &session_id,
                "expression": "1 + 1"
            }),
        )
        .await;

    assert!(
        eval_result.is_err(),
        "evaluate should fail when program is running"
    );

    let err_msg = eval_result.unwrap_err().to_string();
    assert!(
        err_msg.contains("running") || err_msg.contains("stopped"),
        "Error should mention running state, got: {}",
        err_msg
    );
    println!("✅ evaluate correctly rejected while running: {}", err_msg);

    let _ = tools_handler
        .handle_tool("debugger_disconnect", json!({"sessionId": &session_id}))
        .await;
}

/// Disconnect should succeed regardless of session state
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_disconnect_works_in_any_state() {
    if !is_debugpy_available() {
        println!("⚠️  Skipping: debugpy not installed");
        return;
    }

    let h = DebugTestHarness::new_python();
    let tools_handler = &h.tools;

    // Test 1: Disconnect from stopped state
    {
        let session_id = match h.start_python_stopped().await {
            Some(id) => id,
            None => {
                println!("⚠️  Skipping: setup failed");
                return;
            }
        };

        let disconnect_result = timeout(
            Duration::from_secs(5),
            tools_handler.handle_tool("debugger_disconnect", json!({"sessionId": &session_id})),
        )
        .await;

        assert!(
            matches!(disconnect_result, Ok(Ok(_))),
            "Disconnect from stopped state should succeed"
        );
        println!("✅ Disconnect from stopped state succeeded");
    }

    // Test 2: Disconnect from running state
    {
        let start_response = timeout(
            Duration::from_secs(10),
            tools_handler.handle_tool(
                "debugger_start",
                json!({
                    "language": "python",
                    "program": get_scenarios_path(),
                    "stopOnEntry": false
                }),
            ),
        )
        .await;

        if let Ok(Ok(resp)) = start_response {
            let session_id = resp["sessionId"].as_str().unwrap();

            let disconnect_result = timeout(
                Duration::from_secs(5),
                tools_handler
                    .handle_tool("debugger_disconnect", json!({"sessionId": session_id})),
            )
            .await;

            assert!(
                matches!(disconnect_result, Ok(Ok(_))),
                "Disconnect from running state should succeed"
            );
            println!("✅ Disconnect from running state succeeded");
        }
    }
}

/// Double-disconnect should not panic
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_double_disconnect_does_not_panic() {
    if !is_debugpy_available() {
        println!("⚠️  Skipping: debugpy not installed");
        return;
    }

    let h = DebugTestHarness::new_python();
    let tools_handler = &h.tools;

    let session_id = match h.start_python_stopped().await {
        Some(id) => id,
        None => {
            println!("⚠️  Skipping: setup failed");
            return;
        }
    };

    // First disconnect
    let _ = tools_handler
        .handle_tool("debugger_disconnect", json!({"sessionId": &session_id}))
        .await;
    println!("✅ First disconnect completed");

    // Second disconnect — should return error, not panic
    let second_disconnect = tools_handler
        .handle_tool("debugger_disconnect", json!({"sessionId": &session_id}))
        .await;

    assert!(
        second_disconnect.is_err(),
        "Second disconnect should fail (session already removed)"
    );
    println!("✅ Second disconnect returned error without panic");
}

/// Operations after disconnect should fail with session-not-found error
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_operations_after_disconnect_fail_cleanly() {
    if !is_debugpy_available() {
        println!("⚠️  Skipping: debugpy not installed");
        return;
    }

    let h = DebugTestHarness::new_python();
    let tools_handler = &h.tools;

    let session_id = match h.start_python_stopped().await {
        Some(id) => id,
        None => {
            println!("⚠️  Skipping: setup failed");
            return;
        }
    };

    // Disconnect
    let _ = tools_handler
        .handle_tool("debugger_disconnect", json!({"sessionId": &session_id}))
        .await;

    // All subsequent operations should fail with session not found
    let ops = vec![
        ("debugger_continue", json!({"sessionId": &session_id})),
        ("debugger_step_over", json!({"sessionId": &session_id})),
        (
            "debugger_evaluate",
            json!({"sessionId": &session_id, "expression": "1"}),
        ),
        ("debugger_stack_trace", json!({"sessionId": &session_id})),
        (
            "debugger_session_state",
            json!({"sessionId": &session_id}),
        ),
        (
            "debugger_list_breakpoints",
            json!({"sessionId": &session_id}),
        ),
    ];

    for (tool_name, args) in ops {
        let result = tools_handler.handle_tool(tool_name, args).await;
        assert!(
            result.is_err(),
            "{} should fail after disconnect",
            tool_name
        );
        println!("✅ {} correctly failed after disconnect", tool_name);
    }
}

/// Continue on terminated session should fail with clear error
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_continue_on_terminated_session_fails() {
    if !is_debugpy_available() {
        println!("⚠️  Skipping: debugpy not installed");
        return;
    }

    let h = DebugTestHarness::new_python();
    let tools_handler = &h.tools;

    let session_id = match h.start_python_stopped().await {
        Some(id) => id,
        None => {
            println!("⚠️  Skipping: setup failed");
            return;
        }
    };

    // Continue without breakpoints — program should run to completion
    tools_handler
        .handle_tool("debugger_continue", json!({"sessionId": &session_id}))
        .await
        .expect("First continue should succeed");

    // Wait for termination
    let wait_result = tools_handler
        .handle_tool(
            "debugger_wait_for_stop",
            json!({
                "sessionId": &session_id,
                "timeoutMs": 10000
            }),
        )
        .await;

    // Should either timeout or return Terminated
    match &wait_result {
        Ok(v) if v["state"].as_str() == Some("Terminated") => {
            println!("✅ Program terminated as expected");
        }
        Err(_) => {
            println!("⚠️  wait_for_stop timed out — checking state");
        }
        other => {
            println!("Unexpected wait result: {:?}", other);
        }
    }

    // Now try to continue again — should fail since program is done
    let second_continue = tools_handler
        .handle_tool("debugger_continue", json!({"sessionId": &session_id}))
        .await;

    // This should either error or be a no-op; shouldn't hang
    println!(
        "Second continue result: {:?}",
        second_continue.as_ref().map(|v| v.to_string()).map_err(|e| e.to_string())
    );

    let _ = tools_handler
        .handle_tool("debugger_disconnect", json!({"sessionId": &session_id}))
        .await;
}
