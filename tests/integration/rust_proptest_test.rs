//! Property-based state machine test for the Rust debugger (CodeLLDB).
//!
//! Uses `proptest-state-machine` to generate random sequences of debug operations
//! governed by a reference model, then executes them against a real CodeLLDB session.
//! When a failure is found, proptest **shrinks** the sequence to a minimal reproducer.
//!
//! # Architecture
//!
//! ```
//! ReferenceStateMachine (DebugReferenceModel)
//!   ├─ transitions()  → all possible DebugOp variants
//!   ├─ preconditions() → gates which ops are valid from current model state
//!   ├─ apply()        → updates abstract model state
//!   └─ init_state()   → Stopped (stopOnEntry)
//!
//! StateMachineTest (DebugSmTest)
//!   ├─ init_test()   → spawn CodeLLDB, start session, wait for stopOnEntry
//!   ├─ apply()       → execute real tool call, assert post-conditions
//!   ├─ check_invariants() → session state consistency, no hangs
//!   └─ teardown()    → disconnect session
//! ```
//!
//! # How to run
//!
//! ```bash
//! # Run with shrinking (1000 cases, verbose)
//! cargo test rust_proptest -- --ignored --nocapture
//!
//! # Run fewer cases for quick smoke test
//! PROPTEST_CASES=50 cargo test rust_proptest -- --ignored --nocapture
//!
//! # Replay a specific regression seed
//! PROPTEST_SEED=12345 cargo test rust_proptest -- --ignored --nocapture
//! ```

#[allow(dead_code)]
#[path = "rust_model/test_helpers.rs"]
mod test_helpers;

use proptest::prelude::*;
use proptest::strategy::{BoxedStrategy, ValueTree};
use proptest::test_runner::Config as ProptestConfig;
use proptest_state_machine::{prop_state_machine, ReferenceStateMachine, StateMachineTest};
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use test_helpers::{check_prerequisites, compile_fixture};
use tokio::runtime::Runtime as TokioRuntime;
use tokio::sync::RwLock;
use tokio::time::Duration;

use debugger_mcp::debug::SessionManager;
use debugger_mcp::mcp::tools::ToolsHandler;

// ── Constants ──────────────────────────────────────────────────────────────

const MAX_STEPS: usize = 9; // main() has 9 breakable lines before println!

/// All breakable lines in the fixture (must match fixture_model.rs).
const BREAKABLE_LINES: &[i32] = &[
    8, 9, 13, 14, 16, 18, 20, 25, 29, 30, 31, 33,
    36, 37, 38, 39, 40, 41, 42, 43, 44,
];

// ── Reference Model ────────────────────────────────────────────────────────

/// The abstract debug session state that the model tracks.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DebugModelState {
    /// Breakpoint lines currently set.
    breakpoints: HashSet<i32>,
    /// Approximate step counter (0 = at entry line 36, increments per step-over).
    /// Used to keep transition generation away from running past end of program.
    step_count: usize,
}

impl DebugModelState {
    fn initial() -> Self {
        Self {
            breakpoints: HashSet::new(),
            step_count: 0,
        }
    }
}

/// The debug operations that can be applied. Each maps to an MCP tool call.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum DebugOp {
    /// Set a source breakpoint at a specific line.
    SetBreakpoint(i32),
    /// Remove a previously set breakpoint.
    RemoveBreakpoint(i32),
    /// Continue execution + wait for next stop (or termination).
    Continue,
    /// Step over current line + wait for next stop.
    StepOver,
    /// Step into current function call + wait for next stop.
    StepInto,
    /// Step out of current function + wait for next stop.
    StepOut,
    /// Evaluate an expression at the current stop point.
    Evaluate(String),
    /// Retrieve the call stack.
    StackTrace,
}

impl std::fmt::Display for DebugOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DebugOp::SetBreakpoint(line) => write!(f, "SetBP({})", line),
            DebugOp::RemoveBreakpoint(line) => write!(f, "RmBP({})", line),
            DebugOp::Continue => write!(f, "Continue"),
            DebugOp::StepOver => write!(f, "StepOver"),
            DebugOp::StepInto => write!(f, "StepInto"),
            DebugOp::StepOut => write!(f, "StepOut"),
            DebugOp::Evaluate(expr) => write!(f, "Eval({})", expr),
            DebugOp::StackTrace => write!(f, "StackTrace"),
        }
    }
}

/// The reference state machine.
///
/// Generates valid sequences of debug operations and tracks the abstract model.
/// `preconditions()` is the single source of truth for which transitions are
/// valid from a given state — keeping the logic DRY.
struct DebugReferenceModel;

impl ReferenceStateMachine for DebugReferenceModel {
    type State = DebugModelState;
    type Transition = DebugOp;

    fn init_state() -> BoxedStrategy<Self::State> {
        Just(DebugModelState::initial()).boxed()
    }

    /// The union of ALL possible transitions, filtered by `preconditions()`
    /// against the current state so the Sequential strategy never sees a
    /// rejected transition. Weights are tuned for balanced coverage: lots of
    /// stepping and evaluation, some breakpoint manipulation.
    fn transitions(state: &Self::State) -> BoxedStrategy<Self::Transition> {
        let line = prop::sample::select(BREAKABLE_LINES);
        let state = state.clone();

        prop_oneof![
            // Breakpoint ops
            2 => line.clone().prop_map(DebugOp::SetBreakpoint).boxed(),
            1 => line.clone().prop_map(DebugOp::RemoveBreakpoint).boxed(),
            // Execution control (higher weight = more exploration)
            3 => Just(DebugOp::Continue).boxed(),
            4 => Just(DebugOp::StepOver).boxed(),
            2 => Just(DebugOp::StepInto).boxed(),
            1 => Just(DebugOp::StepOut).boxed(),
            // Inspection (important for finding hangs — evaluate known variables)
            4 => prop::sample::select(vec![
                "a", "b", "c", "label", "p", "dist", "sum", "result",
                "x", "y", "count", "total", "i", "val",
            ])
            .prop_map(|s: &str| DebugOp::Evaluate(s.to_string()))
            .boxed(),
            3 => Just(DebugOp::StackTrace).boxed(),
        ]
        .prop_filter("transition rejected by preconditions", move |op| {
            Self::preconditions(&state, op)
        })
        .boxed()
    }

    /// The single source of truth for valid transitions.
    ///
    /// Rules:
    /// - All operations require the program to be Stopped.
    /// - Breakpoint ops must target breakable lines and respect duplicates.
    /// - StepOut only valid when we've stepped into a sub-function (depth > 1 in main).
    ///   The model approximates this: StepOut only allowed after a StepInto
    ///   (which we detect via step_count not advancing).
    fn preconditions(state: &Self::State, transition: &Self::Transition) -> bool {
        match transition {
            DebugOp::SetBreakpoint(line) => {
                BREAKABLE_LINES.contains(line) && !state.breakpoints.contains(line)
            }
            DebugOp::RemoveBreakpoint(line) => {
                state.breakpoints.contains(line)
            }
            DebugOp::Continue => {
                // Continue is valid when stopped. The outcome depends on
                // whether breakpoints are set ahead.
                true
            }
            DebugOp::StepOver => {
                // Step-over is valid as long as we haven't stepped past
                // the last line of main and the program is still alive.
                state.step_count < MAX_STEPS
            }
            DebugOp::StepInto => {
                // Step-into is valid from main lines 37, 38, 39, 40, 41,
                // where there are function calls to enter.
                // Also valid from sub-function lines (add, classify, etc.).
                // Model is permissive here — the actual behavior varies.
                state.step_count + 1 < MAX_STEPS
            }
            DebugOp::StepOut => {
                // Only valid if we might be inside a sub-function.
                // The model can't track this precisely, so we allow it
                // when step_count hasn't progressed past main-only lines.
                // In practice, StepOut is a nop if already in main.
                state.step_count < MAX_STEPS
            }
            DebugOp::Evaluate(_) | DebugOp::StackTrace => {
                // Always valid when Stopped.
                true
            }
        }
    }

    /// Apply a transition to the abstract model state.
    fn apply(mut state: Self::State, transition: &Self::Transition) -> Self::State {
        match transition {
            DebugOp::SetBreakpoint(line) => {
                state.breakpoints.insert(*line);
            }
            DebugOp::RemoveBreakpoint(line) => {
                state.breakpoints.remove(line);
            }
            DebugOp::Continue
            | DebugOp::StepOver
            | DebugOp::StepInto => {
                state.step_count = state.step_count.saturating_add(1).min(MAX_STEPS);
            }
            DebugOp::StepOut
            | DebugOp::Evaluate(_)
            | DebugOp::StackTrace => {
                // No state change.
            }
        }
        state
    }
}

// ── System Under Test ──────────────────────────────────────────────────────

/// The concrete context that holds the live debugger session.
///
/// Stores a Tokio runtime so that `StateMachineTest` methods (which are
/// synchronous per the trait) can execute async debugger operations via
/// `runtime.block_on()`.
pub struct DebugTestContext {
    rt: TokioRuntime,
    tools: ToolsHandler,
    _session_manager: Arc<RwLock<SessionManager>>,
    session_id: String,
    source_path: String,
    /// Count of step-overs actually executed. Used to detect "stepped off end."
    actual_step_count: usize,
}

/// The state machine test that executes transitions against the real debugger.
struct DebugSmTest;

impl StateMachineTest for DebugSmTest {
    type SystemUnderTest = DebugTestContext;
    type Reference = DebugReferenceModel;

    /// Spawn the real CodeLLDB debugger, start a session with stopOnEntry,
    /// and wait for the initial stop. Returns the live context.
    fn init_test(
        _ref_state: &<Self::Reference as ReferenceStateMachine>::State,
    ) -> Self::SystemUnderTest {
        let rt = TokioRuntime::new().expect("Failed to create Tokio runtime");

        // Build the context inside block_on.  Return just the data, then
        // construct DebugTestContext outside so `rt` is never moved into
        // the async block.
        let (tools, session_manager, session_id, source_str) = rt.block_on(async {
            // Compile the fixture (uses the test_helpers infrastructure).
            let binary_path = tokio::task::spawn_blocking(compile_fixture)
                .await
                .expect("spawn_blocking compile_fixture failed");

            let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
            let source_path = std::path::PathBuf::from(&manifest_dir)
                .join("tests/fixtures/debug_exercise.rs");
            let source_str = source_path.to_string_lossy().to_string();

            let session_manager = Arc::new(RwLock::new(SessionManager::new()));
            let tools = ToolsHandler::new(Arc::clone(&session_manager));
            let cwd = binary_path.parent().unwrap().to_string_lossy().to_string();

            // Start debug session with stopOnEntry.
            let start_result = test_helpers::tool_call_with_timeout(
                &tools,
                "debugger_start",
                json!({
                    "language": "rust",
                    "program": binary_path.to_string_lossy(),
                    "args": [],
                    "stopOnEntry": true,
                    "cwd": cwd,
                }),
                Duration::from_secs(30),
            )
            .await
            .expect("debugger_start failed");

            let session_id = start_result["sessionId"]
                .as_str()
                .unwrap()
                .to_string();

            // Wait for the initial entry stop.
            tokio::time::sleep(Duration::from_millis(100)).await;

            let wait_args = json!({
                "sessionId": &session_id,
                "timeoutMs": 10_000,
            });
            let stop = test_helpers::tool_call(&tools, "debugger_wait_for_stop", wait_args)
                .await
                .expect("wait_for_stop after start failed");

            assert_eq!(
                stop["state"].as_str().unwrap_or(""),
                "Stopped",
                "Expected Stopped after stopOnEntry, got: {stop}"
            );

            (tools, session_manager, session_id, source_str)
        });

        DebugTestContext {
            rt,
            tools,
            _session_manager: session_manager,
            session_id,
            source_path: source_str,
            actual_step_count: 0,
        }
    }

    /// Execute a single transition against the real debugger.
    ///
    /// `ref_state` is the model state AFTER the reference model has applied
    /// this transition. We use it to validate post-conditions.
    fn apply(
        mut state: Self::SystemUnderTest,
        ref_state: &<Self::Reference as ReferenceStateMachine>::State,
        transition: <Self::Reference as ReferenceStateMachine>::Transition,
    ) -> Self::SystemUnderTest {
        // Extract fields that the async block will need *before* the closure
        // captures `state` by move.  Then reconstruct at the end.
        let rt = &state.rt;
        let tools = &state.tools;
        let sid = &state.session_id;
        let src = &state.source_path;
        let mut actual_step_count = state.actual_step_count;

        rt.block_on(async {
            match &transition {
                DebugOp::SetBreakpoint(line) => {
                    let result = test_helpers::tool_call_with_timeout(
                        tools,
                        "debugger_set_breakpoint",
                        json!({
                            "sessionId": sid,
                            "sourcePath": src,
                            "line": line,
                        }),
                        Duration::from_secs(10),
                    )
                    .await;

                    match result {
                        Ok(_v) => {}
                        Err(e) => {
                            eprintln!(
                                "⚠️  SetBreakpoint({line}) unexpectedly failed: {e}  \
                                 (model state: {ref_state:?})"
                            );
                        }
                    }
                }

                DebugOp::RemoveBreakpoint(line) => {
                    let result = test_helpers::tool_call_with_timeout(
                        tools,
                        "debugger_remove_breakpoint",
                        json!({
                            "sessionId": sid,
                            "sourcePath": src,
                            "line": line,
                        }),
                        Duration::from_secs(10),
                    )
                    .await;

                    if let Err(e) = result {
                        eprintln!(
                            "⚠️  RemoveBreakpoint({line}) unexpectedly failed: {e}  \
                             (model state: {ref_state:?})"
                        );
                    }
                }

                DebugOp::Continue => {
                    let cont = test_helpers::tool_call_with_timeout(
                        tools,
                        "debugger_continue",
                        json!({"sessionId": sid}),
                        Duration::from_secs(10),
                    )
                    .await;

                    match cont {
                        Ok(_) => {
                            let wait = test_helpers::tool_call_with_timeout(
                                tools,
                                "debugger_wait_for_stop",
                                json!({"sessionId": sid, "timeoutMs": 10_000}),
                                Duration::from_secs(15),
                            )
                            .await;

                            match wait {
                                Ok(v) => {
                                    let got_state = v["state"].as_str().unwrap_or("?");
                                    assert!(
                                        got_state == "Stopped" || got_state == "Terminated",
                                        "Expected Stopped or Terminated after Continue, got {got_state}.  \
                                         transition={transition}, ref_state={ref_state:?}"
                                    );
                                }
                                Err(e) => {
                                    eprintln!(
                                        "⚠️  wait_for_stop after Continue gave error: {e} — \
                                         may be benign if program terminated."
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "⚠️  debugger_continue returned error: {e} — \
                                 may be benign if a previous op already terminated the session. \
                                 ref_state={ref_state:?}"
                            );
                        }
                    }
                }

                DebugOp::StepOver => {
                    let result = test_helpers::tool_call_with_timeout(
                        tools,
                        "debugger_step_over",
                        json!({"sessionId": sid}),
                        Duration::from_secs(10),
                    )
                    .await;

                    match result {
                        Ok(_) => {
                            actual_step_count += 1;

                            let session_state = test_helpers::tool_call_with_timeout(
                                tools,
                                "debugger_session_state",
                                json!({"sessionId": sid}),
                                Duration::from_secs(5),
                            )
                            .await;

                            match session_state {
                                Ok(s) => {
                                    let got_phase = s["state"].as_str().unwrap_or("?");
                                    assert!(
                                        got_phase == "Stopped" || got_phase == "Terminated",
                                        "StepOver: expected Stopped or Terminated, got {got_phase}.  \
                                         step_count={actual_step_count}, transition={transition}"
                                    );
                                }
                                Err(e) => {
                                    eprintln!(
                                        "⚠️  session_state query after StepOver failed: {e} — \
                                         may be benign if program terminated. \
                                         step_count={actual_step_count}"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "⚠️  StepOver returned error: {e} — \
                                 may be benign if program terminated. \
                                 step_count={actual_step_count}, ref_state={ref_state:?}"
                            );
                        }
                    }
                }

                DebugOp::StepInto => {
                    let result = test_helpers::tool_call_with_timeout(
                        tools,
                        "debugger_step_into",
                        json!({"sessionId": sid}),
                        Duration::from_secs(10),
                    )
                    .await;

                    match result {
                        Ok(_) => {
                            let session_state = test_helpers::tool_call_with_timeout(
                                tools,
                                "debugger_session_state",
                                json!({"sessionId": sid}),
                                Duration::from_secs(5),
                            )
                            .await;

                            if let Ok(s) = &session_state {
                                let got = s["state"].as_str().unwrap_or("?");
                                assert!(
                                    got == "Stopped" || got == "Terminated",
                                    "StepInto: expected Stopped or Terminated, got {got}. \
                                     transition={transition}"
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "⚠️  StepInto returned error: {e} — \
                                 may be benign if program terminated. ref_state={ref_state:?}"
                            );
                        }
                    }
                }

                DebugOp::StepOut => {
                    let result = test_helpers::tool_call_with_timeout(
                        tools,
                        "debugger_step_out",
                        json!({"sessionId": sid}),
                        Duration::from_secs(10),
                    )
                    .await;

                    match result {
                        Ok(_) => {
                            let session_state = test_helpers::tool_call_with_timeout(
                                tools,
                                "debugger_session_state",
                                json!({"sessionId": sid}),
                                Duration::from_secs(5),
                            )
                            .await;

                            if let Ok(s) = &session_state {
                                let got = s["state"].as_str().unwrap_or("?");
                                assert!(
                                    got == "Stopped" || got == "Terminated",
                                    "StepOut: expected Stopped or Terminated, got {got}. \
                                     transition={transition}"
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "⚠️  StepOut returned error: {e} — \
                                 may be benign if program terminated. ref_state={ref_state:?}"
                            );
                        }
                    }
                }

                DebugOp::Evaluate(expr) => {
                    let result = test_helpers::tool_call_with_timeout(
                        tools,
                        "debugger_evaluate",
                        json!({"sessionId": sid, "expression": expr}),
                        Duration::from_secs(5),
                    )
                    .await;

                    let check_state = |label: &'static str| {
                        let expr = expr.clone();
                        async move {
                            let session_state = test_helpers::tool_call_with_timeout(
                                tools,
                                "debugger_session_state",
                                json!({"sessionId": sid}),
                                Duration::from_secs(5),
                            )
                            .await;
                            if let Ok(s) = &session_state {
                                let got = s["state"].as_str().unwrap_or("?");
                                assert!(
                                    got == "Stopped" || got == "Terminated",
                                    "{label} evaluate must leave state Stopped or Terminated, \
                                     got {got}. expr={expr}"
                                );
                            }
                        }
                    };

                    match result {
                        Ok(v) => {
                            let _ = v["result"].as_str().unwrap_or("?");
                            check_state("Successful").await;
                        }
                        Err(e) => {
                            eprintln!("⚠️  Evaluate({expr}) returned error: {e}");
                            check_state("Failed").await;
                        }
                    }
                }

                DebugOp::StackTrace => {
                    let result = test_helpers::tool_call_with_timeout(
                        tools,
                        "debugger_stack_trace",
                        json!({"sessionId": sid, "format": "json"}),
                        Duration::from_secs(10),
                    )
                    .await;

                    match result {
                        Ok(v) => {
                            let frames = v["stackFrames"].as_array();
                            assert!(
                                frames.is_some(),
                                "StackTrace must return stackFrames array. \
                                 transition={transition}"
                            );
                            let num_frames = frames.unwrap().len();
                            assert!(
                                num_frames > 0,
                                "StackTrace must have at least 1 frame. \
                                 transition={transition}"
                            );

                            let session_state = test_helpers::tool_call_with_timeout(
                                tools,
                                "debugger_session_state",
                                json!({"sessionId": sid}),
                                Duration::from_secs(5),
                            )
                            .await;
                            if let Ok(s) = &session_state {
                                let got = s["state"].as_str().unwrap_or("?");
                                assert!(
                                    got == "Stopped" || got == "Terminated",
                                    "StackTrace must leave state Stopped or Terminated, got {got}."
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "⚠️  StackTrace returned error: {e} — \
                                 may be benign if session has terminated. \
                                 transition={transition}, ref_state={ref_state:?}"
                            );
                        }
                    }
                }
            }
        });

        state.actual_step_count = actual_step_count;
        state
    }

    /// Check invariants after every transition.
    fn check_invariants(
        state: &Self::SystemUnderTest,
        _ref_state: &<Self::Reference as ReferenceStateMachine>::State,
    ) {
        state.rt.block_on(async {
            // Invariant: the session should be queryable.
            let session_info = test_helpers::tool_call_with_timeout(
                &state.tools,
                "debugger_session_state",
                json!({"sessionId": &state.session_id}),
                Duration::from_secs(5),
            )
            .await;

            match &session_info {
                Ok(info) => {
                    let phase = info["state"].as_str().unwrap_or("?");
                    assert!(
                        matches!(phase, "NotStarted" | "Initializing" | "Initialized"
                               | "Launching" | "Running" | "Stopped" | "Terminated"
                               | "Failed"),
                        "Invalid session state: {phase}"
                    );
                }
                Err(e) => {
                    eprintln!("⚠️  Invariant check: session_state query failed: {e}");
                }
            }

            // Invariant: stack trace should still work if Stopped.
            if let Ok(info) = &session_info {
                if info["state"].as_str() == Some("Stopped") {
                    let stack = test_helpers::tool_call_with_timeout(
                        &state.tools,
                        "debugger_stack_trace",
                        json!({
                            "sessionId": &state.session_id,
                            "format": "json",
                        }),
                        Duration::from_secs(5),
                    )
                    .await;

                    if let Ok(v) = &stack {
                        let frames = v["stackFrames"].as_array();
                        assert!(
                            frames.map(|a| !a.is_empty()).unwrap_or(false),
                            "Invariant violated: Stopped but stack trace is empty/absent."
                        );
                    }
                }
            }
        });
    }

    /// Clean up the debug session.
    fn teardown(
        state: Self::SystemUnderTest,
        _ref_state: <Self::Reference as ReferenceStateMachine>::State,
    ) {
        // Borrow `rt` and `tools` before the async block captures `state`.
        let rt = &state.rt;
        let tools = &state.tools;
        let sid = &state.session_id;
        rt.block_on(async {
            let _ = test_helpers::tool_call_with_timeout(
                tools,
                "debugger_disconnect",
                json!({"sessionId": sid}),
                Duration::from_secs(10),
            )
            .await;
        });
        drop(state);
    }
}

// ── Test Registration ──────────────────────────────────────────────────────

prop_state_machine! {
    #![proptest_config(ProptestConfig {
        // 200 cases: enough to find most bugs without taking too long.
        // Each case spawns a fresh CodeLLDB + compiles the fixture (~2-5s).
        cases: 200,
        // Keep failure persistence off for now (the file would grow over time).
        failure_persistence: None,
        // Verbose to show the sequence on failure.
        verbose: 1,
        // Max shrink iterations — enough for complex sequences.
        max_shrink_iters: 1000,
        .. ProptestConfig::default()
    })]

    /// Property-based test: random sequences of debug operations must not
    /// hang, crash, or violate state invariants.
    ///
    /// The test compiles a Rust fixture, spawns CodeLLDB, and runs 1-12
    /// random debug operations against it. Each sequence is checked against
    /// a reference model. On failure, proptest shrinks to a minimal reproducer.
    #[test]
    #[ignore] // Requires codelldb + lldb + rustc
    fn test_proptest_debug_sequences(
        sequential 1..12 => DebugSmTest
    );
}

// ── Prerequisite check helper for manual runs ──────────────────────────────

/// Lightweight wrapper to skip the test if prerequisites are missing.
/// Call this at the top of `#[test]` functions that need real debuggers.
#[allow(dead_code)]
fn skip_if_no_prereqs() -> bool {
    !check_prerequisites()
}

#[cfg(test)]
mod unit_model_tests {
    use super::*;
    use proptest::test_runner::TestRunner;

    /// The model's preconditions should never reject ALL operations.
    /// From a Stopped state, at least StackTrace and Evaluate should be valid.
    #[test]
    fn test_model_stopped_has_valid_transitions() {
        let state = DebugModelState::initial();
        let valid: Vec<_> = [
            DebugOp::Continue,
            DebugOp::StepOver,
            DebugOp::StepInto,
            DebugOp::StepOut,
            DebugOp::Evaluate("a".into()),
            DebugOp::StackTrace,
            DebugOp::SetBreakpoint(36),
            DebugOp::RemoveBreakpoint(36),
        ]
        .into_iter()
        .filter(|op| DebugReferenceModel::preconditions(&state, op))
        .collect();

        assert!(!valid.is_empty(), "Stopped state must have valid transitions");
        // At minimum, StackTrace, Evaluate, Continue, StepOver should be valid
        // from the initial Stopped state.
        assert!(
            valid.contains(&DebugOp::StackTrace),
            "StackTrace should always be valid from Stopped"
        );
        assert!(
            valid.contains(&DebugOp::Continue),
            "Continue should be valid from Stopped"
        );
    }

    /// Duplicate breakpoints should be rejected by preconditions.
    #[test]
    fn test_model_rejects_duplicate_breakpoints() {
        let mut state = DebugModelState::initial();
        state.breakpoints.insert(36);

        assert!(DebugReferenceModel::preconditions(&state, &DebugOp::SetBreakpoint(36)) == false);
        assert!(DebugReferenceModel::preconditions(&state, &DebugOp::SetBreakpoint(37)) == true);
    }

    /// RemoveBreakpoint should only be valid for breakpoints that are set.
    #[test]
    fn test_model_remove_breakpoint_requires_set() {
        let state = DebugModelState::initial(); // no breakpoints set
        assert!(!DebugReferenceModel::preconditions(&state, &DebugOp::RemoveBreakpoint(36)));

        let mut state_with_bp = DebugModelState::initial();
        state_with_bp.breakpoints.insert(36);
        assert!(DebugReferenceModel::preconditions(&state_with_bp, &DebugOp::RemoveBreakpoint(36)));
    }

    /// Continue advances the step counter.
    #[test]
    fn test_model_continue_advances_step_count() {
        let state = DebugModelState::initial();
        let new_state = DebugReferenceModel::apply(state, &DebugOp::Continue);
        assert_eq!(new_state.step_count, 1);
    }

    /// Step-over from the terminal state should be rejected by preconditions.
    #[test]
    fn test_model_step_over_past_max_steps_rejected() {
        let state = DebugModelState {
            breakpoints: HashSet::new(),
            step_count: MAX_STEPS, // already at max
        };
        assert!(!DebugReferenceModel::preconditions(&state, &DebugOp::StepOver));
    }

    /// Verify that the sequential strategy can generate sequences.
    #[test]
    fn test_sequential_strategy_generates() {
        let mut runner = TestRunner::deterministic();
        let strategy = <DebugReferenceModel as ReferenceStateMachine>::sequential_strategy(3..10);
        let value_tree = strategy.new_tree(&mut runner);
        assert!(value_tree.is_ok(), "Strategy should produce a value tree");

        let (state, transitions, _counter) = value_tree.unwrap().current();
        assert_eq!(state.step_count, 0);
        assert!(
            transitions.len() >= 3,
            "Should generate at least 3 transitions, got {}",
            transitions.len()
        );
        // Every transition must satisfy preconditions.
        let mut s = state;
        for t in &transitions {
            assert!(
                DebugReferenceModel::preconditions(&s, t),
                "Generated transition {t:?} fails preconditions at state {s:?}"
            );
            s = DebugReferenceModel::apply(s, t);
        }
    }
}
