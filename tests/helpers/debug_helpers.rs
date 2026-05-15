/// Shared helpers for debug integration tests.
///
/// Include in test files via:
/// ```ignore
/// #[path = "../helpers/debug_helpers.rs"]
/// mod debug_helpers;
/// use debug_helpers::*;
/// ```
/// (adjust `../` depth to match test file location)
use debugger_mcp::debug::SessionManager;
use debugger_mcp::mcp::tools::ToolsHandler;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{timeout, Duration};

// ============================================================================
// Adapter detection
// ============================================================================

#[allow(dead_code)]
pub fn is_debugpy_available() -> bool {
    Command::new("python3")
        .args(["-c", "import debugpy"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[allow(dead_code)]
pub fn has_codelldb() -> bool {
    Command::new("codelldb").arg("--help").output().is_ok()
}

#[allow(dead_code)]
pub fn has_rustc() -> bool {
    Command::new("rustc")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[allow(dead_code)]
pub fn skip_unless_rust_debug() -> bool {
    if !has_rustc() {
        println!("⚠️  Skipping: rustc not installed");
        return true;
    }
    if !has_codelldb() {
        println!("⚠️  Skipping: codelldb not installed");
        return true;
    }
    false
}

// ============================================================================
// Fixture paths and compilation
// ============================================================================

#[allow(dead_code)]
pub fn manifest_dir() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
}

#[allow(dead_code)]
pub fn fixture_source(name: &str) -> PathBuf {
    manifest_dir().join("tests/fixtures").join(name)
}

#[allow(dead_code)]
pub fn get_fizzbuzz_py_path() -> String {
    fixture_source("fizzbuzz.py").to_string_lossy().to_string()
}

/// Compile a Rust fixture to a binary with debug symbols.
/// Returns the binary path on success, None on failure.
#[allow(dead_code)]
pub fn compile_rust_fixture(source_name: &str, binary_stem: &str) -> Option<PathBuf> {
    let src = fixture_source(source_name);
    let out = manifest_dir()
        .join("tests/fixtures/target")
        .join(binary_stem);

    std::fs::create_dir_all(out.parent().unwrap()).ok()?;
    if out.exists() {
        std::fs::remove_file(&out).ok();
    }

    let result = Command::new("rustc")
        .arg(&src)
        .arg("-g")
        .arg("-C")
        .arg("opt-level=0")
        .arg("-o")
        .arg(&out)
        .output()
        .ok()?;

    if !result.status.success() {
        eprintln!(
            "Compilation failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        return None;
    }
    Some(out)
}

// ============================================================================
// Session test harness
// ============================================================================

/// Reusable test harness for debugger integration tests.
/// Wraps SessionManager + ToolsHandler with convenience methods.
#[allow(dead_code)]
pub struct DebugTestHarness {
    pub tools: ToolsHandler,
    pub source_path: String,
    _session_manager: Arc<RwLock<SessionManager>>,
}

#[allow(dead_code)]
impl DebugTestHarness {
    pub fn new_rust(source_name: &str) -> Self {
        let sm = Arc::new(RwLock::new(SessionManager::new()));
        let tools = ToolsHandler::new(Arc::clone(&sm));
        let source_path = fixture_source(source_name)
            .to_string_lossy()
            .to_string();
        Self {
            tools,
            source_path,
            _session_manager: sm,
        }
    }

    pub fn new_python() -> Self {
        let sm = Arc::new(RwLock::new(SessionManager::new()));
        let tools = ToolsHandler::new(Arc::clone(&sm));
        let source_path = get_fizzbuzz_py_path();
        Self {
            tools,
            source_path,
            _session_manager: sm,
        }
    }

    /// Start a debug session with stopOnEntry and wait until stopped.
    pub async fn start_stopped(&self, language: &str, program: &str) -> Option<String> {
        let resp = timeout(
            Duration::from_secs(30),
            self.tools.handle_tool(
                "debugger_start",
                json!({
                    "language": language,
                    "program": program,
                    "stopOnEntry": true
                }),
            ),
        )
        .await
        .ok()?
        .ok()?;

        let session_id = resp["sessionId"].as_str()?.to_string();

        let wait = timeout(
            Duration::from_secs(15),
            self.tools.handle_tool(
                "debugger_wait_for_stop",
                json!({
                    "sessionId": &session_id,
                    "timeoutMs": 14000
                }),
            ),
        )
        .await;

        match wait {
            Ok(Ok(v)) if v["state"].as_str() == Some("Stopped") => Some(session_id),
            _ => {
                eprintln!("start_stopped: failed to reach Stopped state");
                None
            }
        }
    }

    /// Start a Rust debug session (shorthand).
    pub async fn start_rust_stopped(&self, binary: &str) -> Option<String> {
        self.start_stopped("rust", binary).await
    }

    /// Start a Python debug session (shorthand).
    pub async fn start_python_stopped(&self) -> Option<String> {
        self.start_stopped("python", &self.source_path.clone())
            .await
    }

    pub async fn set_breakpoint(&self, session_id: &str, line: i32) -> bool {
        let resp = self
            .tools
            .handle_tool(
                "debugger_set_breakpoint",
                json!({
                    "sessionId": session_id,
                    "sourcePath": &self.source_path,
                    "line": line
                }),
            )
            .await;

        match resp {
            Ok(v) => v["verified"].as_bool().unwrap_or(false),
            Err(e) => {
                eprintln!("set_breakpoint failed: {}", e);
                false
            }
        }
    }

    /// Continue execution and wait for stop. Opts into `enrichLocals: true`
    /// so existing tests that assert on `localVariables` keep working with
    /// the wrapper's new default (locals not pre-fetched).
    pub async fn continue_and_wait(&self, session_id: &str, timeout_ms: u64) -> Option<Value> {
        self.tools
            .handle_tool("debugger_continue", json!({"sessionId": session_id}))
            .await
            .ok()?;

        timeout(
            Duration::from_millis(timeout_ms + 1000),
            self.tools.handle_tool(
                "debugger_wait_for_stop",
                json!({
                    "sessionId": session_id,
                    "timeoutMs": timeout_ms,
                    "enrichLocals": true
                }),
            ),
        )
        .await
        .ok()?
        .ok()
    }

    /// Get JSON-formatted stack trace with structured frame data.
    pub async fn stack_trace_json(&self, session_id: &str) -> Option<Value> {
        self.tools
            .handle_tool(
                "debugger_stack_trace",
                json!({
                    "sessionId": session_id,
                    "format": "json"
                }),
            )
            .await
            .ok()
    }

    pub async fn disconnect(&self, session_id: &str) {
        let _ = timeout(
            Duration::from_secs(5),
            self.tools
                .handle_tool("debugger_disconnect", json!({"sessionId": session_id})),
        )
        .await;
    }
}
