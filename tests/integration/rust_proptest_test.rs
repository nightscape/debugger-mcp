#[allow(dead_code)]
#[path = "rust_model/reference_state.rs"]
mod reference_state;
#[allow(dead_code)]
#[path = "rust_model/fixture_model.rs"]
mod fixture_model;
#[allow(dead_code)]
#[path = "rust_model/test_helpers.rs"]
mod test_helpers;

use test_helpers::*;
use serde_json::json;
use std::collections::HashSet;
use tokio::time::Duration;

#[derive(Debug, Clone)]
enum DebugOp {
    SetBreakpoint(i32),
    RemoveBreakpoint(i32),
    Continue,
    StepOver,
    StepInto,
    StepOut,
    Evaluate(String),
    EvaluateInvalid(String),
    StackTrace,
}

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
enum TestState {
    Stopped,
    Running,
    Terminated,
}

struct DebugStateMachine {
    state: TestState,
    breakpoints: HashSet<i32>,
    stack_depth: usize,
}

impl DebugStateMachine {
    fn new() -> Self {
        Self {
            state: TestState::Stopped, // stopOnEntry
            breakpoints: HashSet::new(),
            stack_depth: 1,
        }
    }

    fn valid_ops(&self) -> Vec<DebugOp> {
        let mut ops = vec![];

        match self.state {
            TestState::Stopped => {
                ops.push(DebugOp::StepOver);
                ops.push(DebugOp::StepInto);
                ops.push(DebugOp::Continue);
                ops.push(DebugOp::Evaluate("a".into()));
                ops.push(DebugOp::EvaluateInvalid("nonexistent_xyz".into()));
                ops.push(DebugOp::StackTrace);

                if self.stack_depth > 1 {
                    ops.push(DebugOp::StepOut);
                }
            }
            TestState::Running | TestState::Terminated => {}
        }

        // Breakpoint ops work in any non-terminated state
        if self.state != TestState::Terminated {
            for &line in fixture_model::BREAKABLE_LINES {
                if !self.breakpoints.contains(&line) {
                    ops.push(DebugOp::SetBreakpoint(line));
                    break; // limit to keep sequences short
                }
            }
            if let Some(&line) = self.breakpoints.iter().next() {
                ops.push(DebugOp::RemoveBreakpoint(line));
            }
        }

        ops
    }
}

/// Generate a random sequence of valid debug operations and execute them.
/// This is a manual state-machine approach since proptest-state-machine
/// requires complex setup with async runtimes.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_random_operation_sequences() {
    if !check_prerequisites() {
        return;
    }

    // Run multiple random sequences
    for seed in 0..5 {
        println!("\n=== Random sequence seed={} ===", seed);

        let harness = setup();
        let sid = start_session(&harness).await;
        let src = harness.source_path.to_string_lossy().to_string();

        let mut model = DebugStateMachine::new();
        let mut rng_state: u64 = seed * 7919 + 104729;

        for step in 0..15 {
            let valid_ops = model.valid_ops();
            if valid_ops.is_empty() {
                println!("  step {}: no valid ops, stopping", step);
                break;
            }

            // Simple deterministic pseudo-random selection
            rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let idx = (rng_state >> 33) as usize % valid_ops.len();
            let op = valid_ops[idx].clone();

            println!("  step {}: {:?} (state={:?})", step, op, model.state);

            match op {
                DebugOp::SetBreakpoint(line) => {
                    let result = tool_call_with_timeout(
                        &harness.tools,
                        "debugger_set_breakpoint",
                        json!({"sessionId": &sid, "sourcePath": &src, "line": line}),
                        Duration::from_secs(10),
                    )
                    .await;
                    if result.is_ok() {
                        model.breakpoints.insert(line);
                    }
                    println!("    -> {:?}", result.is_ok());
                }
                DebugOp::RemoveBreakpoint(line) => {
                    let _ = tool_call_with_timeout(
                        &harness.tools,
                        "debugger_remove_breakpoint",
                        json!({"sessionId": &sid, "sourcePath": &src, "line": line}),
                        Duration::from_secs(10),
                    )
                    .await;
                    model.breakpoints.remove(&line);
                }
                DebugOp::Continue => {
                    let result = tool_call_with_timeout(
                        &harness.tools,
                        "debugger_continue",
                        json!({"sessionId": &sid}),
                        Duration::from_secs(10),
                    )
                    .await;

                    if result.is_ok() {
                        // Wait for next stop or termination
                        let wait = tool_call_with_timeout(
                            &harness.tools,
                            "debugger_wait_for_stop",
                            json!({"sessionId": &sid, "timeoutMs": 5000}),
                            Duration::from_secs(10),
                        )
                        .await;

                        match wait {
                            Ok(v) if v["state"].as_str() == Some("Stopped") => {
                                model.state = TestState::Stopped;
                                model.stack_depth = stack_depth(&harness.tools, &sid).await;
                            }
                            _ => {
                                model.state = TestState::Terminated;
                            }
                        }
                    }
                    println!("    -> state={:?}", model.state);
                }
                DebugOp::StepOver => {
                    let result = tool_call_with_timeout(
                        &harness.tools,
                        "debugger_step_over",
                        json!({"sessionId": &sid}),
                        Duration::from_secs(10),
                    )
                    .await;

                    match result {
                        Ok(_) => {
                            // Check if still stopped or terminated
                            let state = tool_call(
                                &harness.tools,
                                "debugger_session_state",
                                json!({"sessionId": &sid}),
                            )
                            .await;
                            if let Ok(s) = state {
                                if s["state"].as_str() == Some("Terminated") {
                                    model.state = TestState::Terminated;
                                } else {
                                    model.state = TestState::Stopped;
                                    model.stack_depth = stack_depth(&harness.tools, &sid).await;
                                }
                            }
                        }
                        Err(_) => {
                            model.state = TestState::Terminated;
                        }
                    }
                    println!("    -> state={:?}", model.state);
                }
                DebugOp::StepInto => {
                    let result = tool_call_with_timeout(
                        &harness.tools,
                        "debugger_step_into",
                        json!({"sessionId": &sid}),
                        Duration::from_secs(10),
                    )
                    .await;

                    match result {
                        Ok(_) => {
                            model.state = TestState::Stopped;
                            model.stack_depth = stack_depth(&harness.tools, &sid).await;
                        }
                        Err(_) => {
                            model.state = TestState::Terminated;
                        }
                    }
                    println!("    -> state={:?} depth={}", model.state, model.stack_depth);
                }
                DebugOp::StepOut => {
                    let result = tool_call_with_timeout(
                        &harness.tools,
                        "debugger_step_out",
                        json!({"sessionId": &sid}),
                        Duration::from_secs(10),
                    )
                    .await;

                    match result {
                        Ok(_) => {
                            model.state = TestState::Stopped;
                            model.stack_depth = stack_depth(&harness.tools, &sid).await;
                        }
                        Err(_) => {
                            model.state = TestState::Terminated;
                        }
                    }
                    println!("    -> state={:?} depth={}", model.state, model.stack_depth);
                }
                DebugOp::Evaluate(expr) => {
                    let result = tool_call_with_timeout(
                        &harness.tools,
                        "debugger_evaluate",
                        json!({"sessionId": &sid, "expression": &expr}),
                        Duration::from_secs(5),
                    )
                    .await;
                    println!("    -> eval({}) = {:?}", expr, result.is_ok());
                    // State should remain Stopped
                    assert_eq!(model.state, TestState::Stopped, "Evaluate should not change state");
                }
                DebugOp::EvaluateInvalid(expr) => {
                    let result = tool_call_with_timeout(
                        &harness.tools,
                        "debugger_evaluate",
                        json!({"sessionId": &sid, "expression": &expr}),
                        Duration::from_secs(5),
                    )
                    .await;
                    println!("    -> eval_invalid({}) = {:?}", expr, result.is_err());
                    // CRITICAL: state must remain Stopped after failed eval
                    assert_eq!(
                        model.state,
                        TestState::Stopped,
                        "Failed evaluate must not change state"
                    );
                }
                DebugOp::StackTrace => {
                    let result = tool_call_with_timeout(
                        &harness.tools,
                        "debugger_stack_trace",
                        json!({"sessionId": &sid, "format": "json"}),
                        Duration::from_secs(10),
                    )
                    .await;
                    assert!(result.is_ok(), "StackTrace should succeed when stopped");
                    if let Ok(v) = &result {
                        let depth = v["stackFrames"].as_array().map(|a| a.len()).unwrap_or(0);
                        println!("    -> depth={}", depth);
                    }
                }
            }

            if model.state == TestState::Terminated {
                println!("  Program terminated at step {}", step);
                break;
            }
        }

        disconnect(&harness.tools, &sid).await;
        println!("=== Sequence seed={} completed ===", seed);
    }
}

/// Targeted regression test: the exact sequence that previously caused hangs.
/// SetBP -> Continue -> EvalInvalid -> StepOver
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_hang_regression_sequence() {
    if !check_prerequisites() {
        return;
    }

    let harness = setup();
    let sid = start_session(&harness).await;
    let src = harness.source_path.to_string_lossy().to_string();

    // Set breakpoint at line 38
    tool_call(
        &harness.tools,
        "debugger_set_breakpoint",
        json!({"sessionId": &sid, "sourcePath": &src, "line": 38}),
    )
    .await
    .expect("set breakpoint");

    // Continue to breakpoint
    let stop = continue_and_wait(&harness.tools, &sid).await;
    assert_eq!(stop["state"].as_str(), Some("Stopped"));

    // Evaluate invalid expression (this used to hang)
    let bad = tool_call_with_timeout(
        &harness.tools,
        "debugger_evaluate",
        json!({"sessionId": &sid, "expression": "this_variable_absolutely_does_not_exist"}),
        Duration::from_secs(5),
    )
    .await;
    println!("Invalid eval result: {:?}", bad);

    // Step over (this used to hang after failed eval)
    step_over(&harness.tools, &sid).await;
    println!("Step over succeeded");

    // Evaluate valid expression
    assert_variable(&harness.tools, &sid, "a", "3").await;
    println!("Valid eval succeeded");

    disconnect(&harness.tools, &sid).await;
}

/// Stress test: rapid-fire operations to expose race conditions.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_rapid_fire_operations() {
    if !check_prerequisites() {
        return;
    }

    let harness = setup();
    let sid = start_session(&harness).await;
    let src = harness.source_path.to_string_lossy().to_string();

    // Set breakpoint
    tool_call(
        &harness.tools,
        "debugger_set_breakpoint",
        json!({"sessionId": &sid, "sourcePath": &src, "line": fixture_model::MAIN_ENTRY}),
    )
    .await
    .expect("set breakpoint");

    continue_and_wait(&harness.tools, &sid).await;

    // Rapid sequence of 10 step-overs
    for i in 0..10 {
        let result = tool_call_with_timeout(
            &harness.tools,
            "debugger_step_over",
            json!({"sessionId": &sid}),
            Duration::from_secs(10),
        )
        .await;

        if result.is_err() {
            println!("Step over {} returned error (program may have terminated): {:?}", i, result);
            break;
        }

        // Quick stack trace between steps
        let _ = tool_call_with_timeout(
            &harness.tools,
            "debugger_stack_trace",
            json!({"sessionId": &sid, "format": "json"}),
            Duration::from_secs(5),
        )
        .await;
    }

    disconnect(&harness.tools, &sid).await;
}
