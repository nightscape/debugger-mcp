use debugger_mcp::debug::SessionManager;
use debugger_mcp::mcp::tools::ToolsHandler;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{timeout, Duration};

const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(10);
const START_TIMEOUT: Duration = Duration::from_secs(30);

pub struct TestHarness {
    pub tools: ToolsHandler,
    pub session_manager: Arc<RwLock<SessionManager>>,
    pub source_path: PathBuf,
    pub binary_path: PathBuf,
}

/// Check that codelldb, lldb, and rustc are available. Returns false if any is missing.
pub fn check_prerequisites() -> bool {
    // codelldb doesn't support --version, just check it exists via --help
    for (cmd, args) in [("codelldb", "--help"), ("lldb", "--version"), ("rustc", "--version")] {
        match Command::new(cmd).arg(args).output() {
            Ok(output) if output.status.success() => {}
            _ => {
                println!("Skipping test: {} not available", cmd);
                return false;
            }
        }
    }
    true
}

/// Compile the debug_exercise.rs fixture with debug symbols and no optimizations.
pub fn compile_fixture() -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let source_path = PathBuf::from(&manifest_dir).join("tests/fixtures/debug_exercise.rs");
    let output_dir = PathBuf::from(&manifest_dir).join("tests/fixtures/target");
    fs::create_dir_all(&output_dir).unwrap();

    let binary_path = output_dir.join("debug_exercise");

    if binary_path.exists() {
        fs::remove_file(&binary_path).unwrap();
    }

    let result = Command::new("rustc")
        .arg(&source_path)
        .arg("-g")
        .arg("-C").arg("opt-level=0")
        .arg("-o").arg(&binary_path)
        .output()
        .unwrap();

    assert!(
        result.status.success(),
        "Compilation failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );

    binary_path
}

/// Create the full test harness: compile fixture, create session manager + tools handler.
pub fn setup() -> TestHarness {
    let binary_path = compile_fixture();
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let source_path = PathBuf::from(&manifest_dir).join("tests/fixtures/debug_exercise.rs");

    let session_manager = Arc::new(RwLock::new(SessionManager::new()));
    let tools = ToolsHandler::new(Arc::clone(&session_manager));

    TestHarness {
        tools,
        session_manager,
        source_path,
        binary_path,
    }
}

/// Call an MCP tool with a timeout. Panics on timeout (turns hangs into failures).
pub async fn tool_call(
    tools: &ToolsHandler,
    name: &str,
    args: Value,
) -> Result<Value, String> {
    tool_call_with_timeout(tools, name, args, DEFAULT_TOOL_TIMEOUT).await
}

pub async fn tool_call_with_timeout(
    tools: &ToolsHandler,
    name: &str,
    args: Value,
    dur: Duration,
) -> Result<Value, String> {
    match timeout(dur, tools.handle_tool(name, args)).await {
        Ok(Ok(val)) => Ok(val),
        Ok(Err(e)) => Err(format!("{e}")),
        Err(_) => panic!("{name} timed out after {dur:?}"),
    }
}

/// Start a debug session with stopOnEntry, wait for the initial stop, return session ID.
pub async fn start_session(harness: &TestHarness) -> String {
    let cwd = harness.binary_path.parent().unwrap().to_string_lossy().to_string();
    let start_args = json!({
        "language": "rust",
        "program": harness.binary_path.to_string_lossy(),
        "args": [],
        "stopOnEntry": true,
        "cwd": cwd
    });

    let result = tool_call_with_timeout(&harness.tools, "debugger_start", start_args, START_TIMEOUT)
        .await
        .expect("debugger_start failed");

    let session_id = result["sessionId"].as_str().unwrap().to_string();

    // Wait for the initial entry stop
    tokio::time::sleep(Duration::from_millis(100)).await;

    let wait_args = json!({
        "sessionId": &session_id,
        "timeoutMs": 10000
    });
    let stop = tool_call(&harness.tools, "debugger_wait_for_stop", wait_args)
        .await
        .expect("wait_for_stop after start failed");

    assert_eq!(
        stop["state"].as_str().unwrap_or(""),
        "Stopped",
        "Expected Stopped after stopOnEntry, got: {stop}"
    );

    session_id
}

/// Assert the session is stopped at a specific line.
pub async fn assert_stopped_at(tools: &ToolsHandler, session_id: &str, expected_line: i32) {
    let stack = tool_call(tools, "debugger_stack_trace", json!({"sessionId": session_id, "format": "json"}))
        .await
        .expect("stack_trace failed");

    let frames = stack["stackFrames"].as_array().expect("no stackFrames");
    assert!(!frames.is_empty(), "stack trace is empty");

    let top_line = frames[0]["line"].as_i64().unwrap() as i32;
    assert_eq!(
        top_line, expected_line,
        "Expected stop at line {expected_line}, got line {top_line}. Full frame: {}",
        frames[0]
    );
}

/// Assert the session is stopped in a specific function.
pub async fn assert_in_function(tools: &ToolsHandler, session_id: &str, expected_fn: &str) {
    let stack = tool_call(tools, "debugger_stack_trace", json!({"sessionId": session_id, "format": "json"}))
        .await
        .expect("stack_trace failed");

    let frames = stack["stackFrames"].as_array().expect("no stackFrames");
    assert!(!frames.is_empty(), "stack trace is empty");

    let top_fn = frames[0]["name"].as_str().unwrap_or("");
    assert!(
        top_fn.contains(expected_fn),
        "Expected function containing '{expected_fn}', got '{top_fn}'"
    );
}

/// Evaluate an expression and return the result string.
pub async fn evaluate(tools: &ToolsHandler, session_id: &str, expr: &str) -> Result<String, String> {
    let result = tool_call(
        tools,
        "debugger_evaluate",
        json!({"sessionId": session_id, "expression": expr}),
    )
    .await?;

    Ok(result["result"].as_str().unwrap_or("").to_string())
}

/// Assert a variable has a specific value (via evaluate).
pub async fn assert_variable(
    tools: &ToolsHandler,
    session_id: &str,
    name: &str,
    expected_value: &str,
) {
    let result = evaluate(tools, session_id, name)
        .await
        .unwrap_or_else(|e| panic!("evaluate '{name}' failed: {e}"));

    assert!(
        result.contains(expected_value),
        "Variable '{name}': expected value containing '{expected_value}', got '{result}'"
    );
}

/// Get the current stack depth.
pub async fn stack_depth(tools: &ToolsHandler, session_id: &str) -> usize {
    let stack = tool_call(tools, "debugger_stack_trace", json!({"sessionId": session_id, "format": "json"}))
        .await
        .expect("stack_trace failed");

    stack["stackFrames"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0)
}

/// Step over and wait for the stop.
pub async fn step_over(tools: &ToolsHandler, session_id: &str) {
    tool_call(tools, "debugger_step_over", json!({"sessionId": session_id}))
        .await
        .expect("step_over failed");
}

/// Step into and wait for the stop.
pub async fn step_into(tools: &ToolsHandler, session_id: &str) {
    tool_call(tools, "debugger_step_into", json!({"sessionId": session_id}))
        .await
        .expect("step_into failed");
}

/// Step out and wait for the stop.
pub async fn step_out(tools: &ToolsHandler, session_id: &str) {
    tool_call(tools, "debugger_step_out", json!({"sessionId": session_id}))
        .await
        .expect("step_out failed");
}

/// Continue execution and wait for a stop (breakpoint or termination).
pub async fn continue_and_wait(tools: &ToolsHandler, session_id: &str) -> Value {
    tool_call(
        tools,
        "debugger_continue",
        json!({"sessionId": session_id}),
    )
    .await
    .expect("continue failed");

    let wait = tool_call(
        tools,
        "debugger_wait_for_stop",
        json!({"sessionId": session_id, "timeoutMs": 10000, "enrichLocals": true}),
    )
    .await;

    match wait {
        Ok(v) => v,
        Err(e) => json!({"state": "Terminated", "error": e}),
    }
}

/// Disconnect the session.
pub async fn disconnect(tools: &ToolsHandler, session_id: &str) {
    let _ = tool_call(
        tools,
        "debugger_disconnect",
        json!({"sessionId": session_id}),
    )
    .await;
}

/// Get the current line from the top stack frame.
pub async fn current_line(tools: &ToolsHandler, session_id: &str) -> i32 {
    let stack = tool_call(tools, "debugger_stack_trace", json!({"sessionId": session_id, "format": "json"}))
        .await
        .expect("stack_trace failed");

    let frames = stack["stackFrames"].as_array().expect("no stackFrames");
    assert!(!frames.is_empty(), "stack trace is empty");
    frames[0]["line"].as_i64().unwrap() as i32
}
