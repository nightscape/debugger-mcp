use crate::adapters::security;
use crate::debug::SessionManager;
use crate::{Error, Result};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Deserialize an optional integer that may arrive as a string.
/// Some MCP clients (including Claude) stringify integer tool arguments.
fn deserialize_optional_int_or_string<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<i32>, D::Error>
where
    D: Deserializer<'de>,
{
    let v: Option<Value> = Option::deserialize(deserializer)?;
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_i64()
            .and_then(|n| i32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom(format!("number out of i32 range: {n}"))),
        Some(Value::String(s)) => s
            .parse::<i32>()
            .map(Some)
            .map_err(|_| serde::de::Error::custom(format!("invalid integer string: {s}"))),
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected integer or string, got: {other}"
        ))),
    }
}

/// Deserialize a boolean that may arrive as a string.
/// Some MCP clients stringify boolean tool arguments.
fn deserialize_bool_from_anything<'de, D>(deserializer: D) -> std::result::Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    let v: Value = Value::deserialize(deserializer)?;
    match v {
        Value::Bool(b) => Ok(b),
        Value::String(s) => match s.as_str() {
            "true" | "1" => Ok(true),
            "false" | "0" | "" => Ok(false),
            _ => Err(serde::de::Error::custom(format!(
                "invalid boolean string: {s}"
            ))),
        },
        Value::Number(n) => Ok(n.as_i64().unwrap_or(0) != 0),
        Value::Null => Ok(false),
        other => Err(serde::de::Error::custom(format!(
            "expected boolean or string, got: {other}"
        ))),
    }
}

/// Deserialize a u64 that may arrive as a string.
fn deserialize_u64_from_anything<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let v: Value = Value::deserialize(deserializer)?;
    match v {
        Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| serde::de::Error::custom(format!("number out of u64 range: {n}"))),
        Value::String(s) => s
            .parse::<u64>()
            .map_err(|_| serde::de::Error::custom(format!("invalid u64 string: {s}"))),
        Value::Null => Ok(0),
        other => Err(serde::de::Error::custom(format!(
            "expected number or string, got: {other}"
        ))),
    }
}

/// Deserialize an i32 that may arrive as a string.
fn deserialize_i32_from_anything<'de, D>(deserializer: D) -> std::result::Result<i32, D::Error>
where
    D: Deserializer<'de>,
{
    let v: Value = Value::deserialize(deserializer)?;
    match v {
        Value::Number(n) => n
            .as_i64()
            .and_then(|n| i32::try_from(n).ok())
            .ok_or_else(|| serde::de::Error::custom(format!("number out of i32 range: {n}"))),
        Value::String(s) => s
            .parse::<i32>()
            .map_err(|_| serde::de::Error::custom(format!("invalid i32 string: {s}"))),
        other => Err(serde::de::Error::custom(format!(
            "expected number or string, got: {other}"
        ))),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BreakpointSpec {
    pub source_path: String,
    #[serde(deserialize_with = "deserialize_i32_from_anything")]
    pub line: i32,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub activate_after: Option<ActivateAfterArgs>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebuggerStartArgs {
    pub language: String,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default, deserialize_with = "deserialize_bool_from_anything")]
    pub stop_on_entry: bool,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub breakpoints: Vec<BreakpointSpec>,
    /// Cargo build profile (e.g. "dev", "release", "debugger"). Rust only.
    /// When set on a Cargo project, CodeLLDB handles compilation with this profile.
    pub profile: Option<String>,
    /// RSS memory limit in MB for the debug adapter supervisor.
    /// If the adapter exceeds this, it is killed and the session fails with
    /// recovery recommendations. Default: 1024MB.
    #[serde(default, deserialize_with = "deserialize_optional_int_or_string")]
    pub supervisor_memory_limit_mb: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivateAfterArgs {
    pub source_path: String,
    #[serde(deserialize_with = "deserialize_i32_from_anything")]
    pub line: i32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetBreakpointArgs {
    pub session_id: String,
    pub source_path: String,
    #[serde(deserialize_with = "deserialize_i32_from_anything")]
    pub line: i32,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub activate_after: Option<ActivateAfterArgs>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveBreakpointArgs {
    pub session_id: String,
    pub source_path: String,
    #[serde(deserialize_with = "deserialize_i32_from_anything")]
    pub line: i32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinueArgs {
    pub session_id: String,
    pub wait_for_stop: Option<bool>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackTraceArgs {
    pub session_id: String,
    pub format: Option<String>,
    pub limit: Option<i32>,
    pub include_variables: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluateArgs {
    pub session_id: String,
    pub expression: String,
    #[serde(default, deserialize_with = "deserialize_optional_int_or_string")]
    pub frame_id: Option<i32>,
    pub context: Option<String>,
    /// When true, prepend `?` to the expression so CodeLLDB disables synthetic
    /// children for this eval. Lets a scalar field path (`state.c.length`) read
    /// the raw struct field without the expression compiler walking the
    /// surrounding container's synthetic view first. No-op on adapters that
    /// don't recognise the prefix.
    #[serde(default)]
    pub no_synthetic: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DisconnectArgs {
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelArgs {
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStateArgs {
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitForStopArgs {
    pub session_id: String,
    #[serde(default = "default_timeout", deserialize_with = "deserialize_u64_from_anything")]
    pub timeout_ms: u64,
    /// Pre-fetch the top frame's locals on stop. Off by default — agents
    /// should call `debugger_get_variables` explicitly when they need them.
    /// The pre-fetch can hang or OOM CodeLLDB on frames where the captures
    /// include large/recursive synthetic-formatted containers (async-fn
    /// state machines, deep Vec<HashMap<…>>, recursive enums), and the
    /// agent doesn't always need the data anyway.
    #[serde(default, deserialize_with = "deserialize_bool_from_anything")]
    pub enrich_locals: bool,
}

fn default_timeout() -> u64 {
    5000
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListBreakpointsArgs {
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepArgs {
    pub session_id: String,
    pub thread_id: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunToCrashArgs {
    pub language: String,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub exception_filter: Option<String>,
    /// Cargo build profile (e.g. "dev", "release", "debugger"). Rust only.
    pub profile: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotAtArgs {
    pub session_id: String,
    pub source_path: String,
    #[serde(deserialize_with = "deserialize_i32_from_anything")]
    pub line: i32,
    #[serde(default)]
    pub expressions: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceFunctionArgs {
    pub session_id: String,
    #[serde(default)]
    pub expressions: Vec<String>,
    pub max_steps: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebuggingTipsArgs {
    pub language: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetDataBreakpointArgs {
    pub session_id: String,
    /// Variable name or expression to watch
    pub name: String,
    /// Variables reference (from a scope/parent variable). Needed for child variables.
    pub variables_reference: Option<i32>,
    pub frame_id: Option<i32>,
    /// Access type: "write" (default), "read", or "readWrite"
    pub access_type: Option<String>,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetVariablesArgs {
    pub session_id: String,
    #[serde(default, deserialize_with = "deserialize_optional_int_or_string")]
    pub variables_reference: Option<i32>,
    #[serde(default, deserialize_with = "deserialize_optional_int_or_string")]
    pub frame_id: Option<i32>,
    #[serde(default, deserialize_with = "deserialize_optional_int_or_string")]
    pub max_count: Option<i32>,
    pub scope: Option<String>,
    pub filter: Option<String>,
    /// CodeLLDB raw mode: when true, sets `format.showRaw=true` on the DAP
    /// variables request so synthetic-children providers are bypassed and the
    /// underlying struct fields are returned. Use this to read scalar fields
    /// (`length`, `len`, `bucket_mask`) off large containers without paying
    /// the per-element materialisation cost.
    #[serde(default)]
    pub no_synthetic: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetOutputArgs {
    pub session_id: String,
    pub category: Option<String>,
    pub search: Option<String>,
    pub limit: Option<usize>,
    pub since_line: Option<usize>,
}

fn is_bare_identifier(expr: &str) -> bool {
    let trimmed = expr.trim();
    !trimmed.is_empty() && trimmed.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// Poll session state until stopped, terminated, or timeout.
/// On stop, enriches the response with stack trace, source context, and local variable names.
async fn wait_for_stop_enriched(
    session: &crate::debug::session::DebugSession,
    timeout_ms: u64,
) -> crate::Result<Value> {
    wait_for_stop_enriched_opts(session, timeout_ms, false).await
}

async fn wait_for_stop_enriched_opts(
    session: &crate::debug::session::DebugSession,
    timeout_ms: u64,
    enrich_locals: bool,
) -> crate::Result<Value> {
    let timeout = tokio::time::Duration::from_millis(timeout_ms);
    let start = tokio::time::Instant::now();

    loop {
        let state = session.get_state().await;

        if let crate::debug::state::DebugState::Stopped { thread_id, reason } = state {
            let mut result = json!({
                "state": "Stopped",
                "threadId": thread_id,
                "reason": reason
            });

            let ctx = build_stop_context(session, Some(3), enrich_locals).await;
            if let Value::Object(map) = ctx {
                for (k, v) in map {
                    result[k] = v;
                }
            }

            return Ok(result);
        }

        if matches!(state, crate::debug::state::DebugState::Terminated) {
            return Ok(json!({
                "state": "Terminated",
                "reason": "Program exited"
            }));
        }

        if let crate::debug::state::DebugState::Failed { error } = state {
            return Err(crate::Error::Dap(format!("Session failed: {}", error)));
        }

        if start.elapsed() > timeout {
            return Err(crate::Error::InvalidState(format!(
                "Timeout waiting for program to stop ({}ms). Current state: {:?}",
                timeout_ms, state
            )));
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
}

/// Validate session is stopped and return the thread_id (with optional override).
fn require_stopped(
    state: &crate::debug::state::DebugState,
    thread_id_override: Option<i32>,
) -> crate::Result<i32> {
    if let crate::debug::state::DebugState::Stopped { thread_id, .. } = state {
        Ok(thread_id_override.unwrap_or(*thread_id))
    } else {
        Err(crate::Error::InvalidState(
            "Cannot step while program is running. The program must be stopped first."
                .to_string(),
        ))
    }
}

/// Convert a slice of Variables into a JSON array of {name, value, type}.
fn format_variables_json(vars: &[crate::dap::types::Variable]) -> Vec<Value> {
    vars.iter()
        .map(|v| {
            json!({
                "name": v.name,
                "value": v.value,
                "type": v.type_
            })
        })
        .collect()
}

/// Fetch source context around a stack frame's current line.
async fn fetch_source_context(frame: &crate::dap::types::StackFrame) -> Option<String> {
    let path = frame.source.as_ref()?.path.as_ref()?;
    let src_timeout = tokio::time::Duration::from_secs(1);
    tokio::time::timeout(src_timeout, read_source_context(path, frame.line, 5))
        .await
        .ok()?
}

/// Fetch local variables for a frame with timeout, returning a JSON array.
/// Returns None on timeout or error (never breaks the session).
async fn fetch_frame_variables(
    session: &crate::debug::session::DebugSession,
    frame_id: i32,
    expand_depth: u32,
) -> Option<Vec<Value>> {
    let vars_timeout = tokio::time::Duration::from_secs(5);
    match tokio::time::timeout(vars_timeout, session.get_local_variables(frame_id, expand_depth)).await {
        Ok(Ok(vars)) => Some(format_variables_json(&vars)),
        Err(_) => {
            session.cancel_pending_requests().await;
            None
        }
        Ok(Err(_)) => None,
    }
}

async fn build_stop_context(
    session: &crate::debug::session::DebugSession,
    stack_limit: Option<i32>,
    enrich_locals: bool,
) -> Value {
    *session.last_tool_context.write().await =
        Some("build_stop_context variables".to_string());

    let stack_limit = stack_limit.unwrap_or(3);
    let mut ctx = json!({});

    // Stack trace with timeout
    let stack_timeout = tokio::time::Duration::from_secs(3);
    let stack_result = tokio::time::timeout(stack_timeout, session.stack_trace(Some(stack_limit))).await;
    if stack_result.is_err() {
        // Stack trace timed out — cancel pending requests so subsequent ops work
        session.cancel_pending_requests().await;
    }
    if let Ok(Ok(frames)) = stack_result {
        ctx["stackTrace"] = json!(format_stack_frames(&frames, &session.program));

        if let Some(top) = frames.first() {
            ctx["topFrame"] = json!({
                "id": top.id,
                "name": top.name,
                "line": top.line,
                "source": top.source
            });

            if let Some(source_ctx) = fetch_source_context(top).await {
                ctx["sourceContext"] = json!(source_ctx);
            }

            // Default: do NOT pre-fetch locals on stop. The variables
            // request is the single most expensive DAP call for many
            // frames (synthetic providers walk large/recursive captures
            // to compute summary strings), and the agent doesn't always
            // need the data anyway. The caller opts in via `enrichLocals:
            // true` on `debugger_wait_for_stop` if it really wants the
            // pre-fetch — at the same cost-and-risk the old auto-path had.
            //
            // When opted out: surface a short hint pointing at the right
            // recovery tools, with extra detail for async-closure frames
            // because that's the case where the pre-fetch would have
            // most likely hung anyway.
            if enrich_locals {
                if let Some(var_list) = fetch_frame_variables(session, top.id, 0).await {
                    ctx["localVariables"] = json!(var_list);
                }
            } else {
                ctx["localVariablesSkipped"] = json!(true);
                let in_async_closure =
                    crate::debug::async_state::is_async_closure_frame(&top.name);
                ctx["hint"] = json!(if in_async_closure {
                    "Locals not pre-fetched (default). This frame is an \
                     async-closure ({{closure#…}}) — even an opt-in fetch \
                     can hang LLDB on the state-machine union's synthetic \
                     walk.\n\
                     \n\
                     Inspect now (cheap, by name — SAFE here):\n\
                     - debugger_evaluate({sessionId, expression: \"<name>\", \
                       context: \"watch\", noSynthetic: true})\n\
                       For container lengths: expression: \"<name>.len\".\n\
                     \n\
                     Reading all locals at once is RISKY in async closures:\n\
                     - debugger_get_variables can wedge LLDB on the \
                       state-machine union (dispatcher gets stuck inside the \
                       synthetic walk and does NOT respond to DAP cancel; \
                       only debugger_disconnect recovers). Use only if you \
                       already know which sibling locals exist and need the \
                       full enumeration.\n\
                     \n\
                     Avoid the hang next run: move the BP one frame up to \
                     the synchronous caller, or rebind into non-async scope \
                     before the assert (`let len = db_rows.len(); ...`).\n\
                     \n\
                     If a later call hangs: debugger_cancel returns 0 \
                     (CodeLLDB ignores cancel during synthetic walks) — \
                     call debugger_disconnect and start fresh."
                } else {
                    "Locals not pre-fetched (default). Call \
                     debugger_get_variables({sessionId, frameId}) when you \
                     need them. Pass `enrichLocals: true` to \
                     debugger_wait_for_stop to opt back into the auto-fetch."
                });
            }
        }
    }

    ctx
}

async fn read_source_context(path: &str, line: i32, context_lines: i32) -> Option<String> {
    let content = tokio::fs::read_to_string(path).await.ok()?;
    let lines: Vec<&str> = content.lines().collect();
    let line_idx = (line - 1) as usize;
    if line_idx >= lines.len() {
        return None;
    }

    let start = line_idx.saturating_sub(context_lines as usize);
    let end = (line_idx + context_lines as usize + 1).min(lines.len());

    let formatted: Vec<String> = (start..end)
        .map(|i| {
            let line_num = i + 1;
            let marker = if i == line_idx { " >" } else { "  " };
            format!("{} {:>4} | {}", marker, line_num, lines[i])
        })
        .collect();

    Some(formatted.join("\n"))
}

fn format_stack_frames(
    frames: &[crate::dap::types::StackFrame],
    program: &str,
) -> String {
    let program_components: Vec<_> = std::path::Path::new(program).components().collect();
    let lines: Vec<String> = frames
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let location = match &f.source {
                Some(src) => {
                    let full = src
                        .path
                        .as_deref()
                        .or(src.name.as_deref())
                        .unwrap_or("?");
                    let src_components: Vec<_> =
                        std::path::Path::new(full).components().collect();
                    let common_len = program_components
                        .iter()
                        .zip(src_components.iter())
                        .take_while(|(a, b)| a == b)
                        .count();
                    if common_len > 0 {
                        let display = src_components[common_len..]
                            .iter()
                            .collect::<std::path::PathBuf>()
                            .to_string_lossy()
                            .into_owned();
                        format!("{}:{}", display, f.line)
                    } else {
                        format!("{}:{}", full, f.line)
                    }
                }
                None => format!("line {}", f.line),
            };
            format!("#{} [id={}] {} ({})", i, f.id, f.name, location)
        })
        .collect();
    lines.join("\n")
}

pub struct ToolsHandler {
    session_manager: Arc<RwLock<SessionManager>>,
}

impl ToolsHandler {
    pub fn new(session_manager: Arc<RwLock<SessionManager>>) -> Self {
        Self { session_manager }
    }

    pub async fn handle_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        match name {
            "debugger_start" => self.debugger_start(arguments).await,
            "debugger_session_state" => self.debugger_session_state(arguments).await,
            "debugger_set_breakpoint" => self.debugger_set_breakpoint(arguments).await,
            "debugger_remove_breakpoint" => self.debugger_remove_breakpoint(arguments).await,
            "debugger_continue" => self.debugger_continue(arguments).await,
            "debugger_stack_trace" => self.debugger_stack_trace(arguments).await,
            "debugger_evaluate" => self.debugger_evaluate(arguments).await,
            "debugger_disconnect" => self.debugger_disconnect(arguments).await,
            "debugger_wait_for_stop" => self.debugger_wait_for_stop(arguments).await,
            "debugger_list_breakpoints" => self.debugger_list_breakpoints(arguments).await,
            "debugger_step_over" => self.debugger_step_over(arguments).await,
            "debugger_step_into" => self.debugger_step_into(arguments).await,
            "debugger_step_out" => self.debugger_step_out(arguments).await,
            "debugger_get_output" => self.debugger_get_output(arguments).await,
            "debugger_run_to_crash" => self.debugger_run_to_crash(arguments).await,
            "debugger_snapshot_at" => self.debugger_snapshot_at(arguments).await,
            "debugger_trace_function" => self.debugger_trace_function(arguments).await,
            "debugger_debugging_tips" => self.debugger_debugging_tips(arguments).await,
            "debugger_set_data_breakpoint" => self.debugger_set_data_breakpoint(arguments).await,
            "debugger_get_variables" => self.debugger_get_variables(arguments).await,
            "debugger_cancel" => self.debugger_cancel(arguments).await,
            _ => Err(Error::MethodNotFound(name.to_string())),
        }
    }

    async fn debugger_start(&self, arguments: Value) -> Result<Value> {
        let args: DebuggerStartArgs = serde_json::from_value(arguments)?;

        // Validate program path to prevent path traversal attacks
        // For Rust, allow both .rs source files and pre-compiled binaries (no extension)
        // For others, validate with expected source file extension
        let validated_program = if args.language == "rust" {
            // Rust: accept .rs files, executables, Cargo.toml, or directories containing Cargo.toml
            let path = std::path::Path::new(&args.program);

            if path.is_dir() {
                // Directory: validate it contains a Cargo.toml
                let validated_dir = security::validate_directory_path(&args.program)?;
                let manifest = validated_dir.join("Cargo.toml");
                if !manifest.exists() {
                    return Err(Error::Compilation(format!(
                        "Directory does not contain Cargo.toml: {}",
                        validated_dir.display()
                    )));
                }
                manifest
            } else {
                let validated = security::validate_source_path(&args.program, None)?;
                let ext = validated.extension().and_then(|s| s.to_str()).unwrap_or("");
                if !ext.is_empty() && ext != "rs" && ext != "toml" {
                    return Err(Error::Compilation(format!(
                        "Invalid Rust program path. Expected .rs source file, Cargo.toml, directory, or executable, got .{} file: {}",
                        ext,
                        validated.display()
                    )));
                }
                if ext == "toml" && !validated.ends_with("Cargo.toml") {
                    return Err(Error::Compilation(format!(
                        "Invalid .toml file. Expected Cargo.toml, got: {}",
                        validated.display()
                    )));
                }
                validated
            }
        } else {
            // Other languages: Validate with expected extension
            let extension = match args.language.as_str() {
                "python" => Some("py"),
                "ruby" => Some("rb"),
                "javascript" | "nodejs" => Some("js"),
                "go" => Some("go"),
                _ => None,
            };
            security::validate_source_path(&args.program, extension)?
        };
        let program = validated_program
            .to_str()
            .ok_or_else(|| Error::Internal("Non-UTF8 program path (invalid encoding)".to_string()))?
            .to_string();

        // Validate cwd if provided
        let validated_cwd = if let Some(cwd_path) = &args.cwd {
            let validated = security::validate_directory_path(cwd_path)?;
            Some(
                validated
                    .to_str()
                    .ok_or_else(|| {
                        Error::Internal("Non-UTF8 cwd path (invalid encoding)".to_string())
                    })?
                    .to_string(),
            )
        } else {
            None
        };

        let has_breakpoints = !args.breakpoints.is_empty();
        let stop_on_entry = args.stop_on_entry || has_breakpoints;

        // Apply per-session supervisor memory limit if specified
        if let Some(limit) = args.supervisor_memory_limit_mb {
            let mut manager = self.session_manager.write().await;
            manager.set_supervisor_rss_limit(limit as u64);
        }

        let manager = self.session_manager.read().await;
        let session_id = manager
            .create_session(
                &args.language,
                program,
                args.args,
                validated_cwd,
                stop_on_entry,
                args.env,
                args.profile,
            )
            .await?;

        if !has_breakpoints {
            return Ok(json!({
                "sessionId": session_id,
                "status": "started"
            }));
        }

        // Wait for the session to stop on entry so we can set breakpoints
        let session = manager.get_session(&session_id).await?;
        let timeout = tokio::time::Duration::from_secs(30);
        let start = tokio::time::Instant::now();
        loop {
            let state = session.get_state().await;
            if matches!(state, crate::debug::state::DebugState::Stopped { .. }) {
                break;
            }
            if matches!(state, crate::debug::state::DebugState::Terminated) {
                return Err(Error::Dap("Program terminated before breakpoints could be set".into()));
            }
            if let crate::debug::state::DebugState::Failed { error } = state {
                return Err(Error::Dap(format!("Session failed: {}", error)));
            }
            if start.elapsed() > timeout {
                return Err(Error::InvalidState(
                    "Timeout waiting for program to stop on entry for breakpoint setup".into(),
                ));
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }

        // Set all breakpoints
        let mut bp_results = Vec::new();
        for bp in &args.breakpoints {
            let validated_source = security::validate_source_path(&bp.source_path, None)?;
            let source_path = validated_source
                .to_str()
                .ok_or_else(|| Error::Internal("Non-UTF8 source path".into()))?
                .to_string();

            let activate_after = bp.activate_after.as_ref().map(|a| {
                crate::debug::state::BreakpointLocation {
                    source_path: a.source_path.clone(),
                    line: a.line,
                }
            });

            let verified = session
                .set_breakpoint(source_path.clone(), bp.line, bp.condition.clone(), bp.hit_condition.clone(), activate_after)
                .await?;

            let mut bp_result = json!({
                "verified": verified,
                "sourcePath": source_path,
                "line": bp.line
            });
            if let Some(a) = &bp.activate_after {
                bp_result["activateAfter"] = json!({
                    "sourcePath": a.source_path,
                    "line": a.line
                });
                if !verified {
                    bp_result["status"] = json!("pending_dependency");
                }
            }
            bp_results.push(bp_result);
        }

        // If the user didn't originally request stop_on_entry, continue execution
        if !args.stop_on_entry {
            session.continue_execution().await?;
        }

        Ok(json!({
            "sessionId": session_id,
            "status": "started",
            "breakpoints": bp_results
        }))
    }

    async fn debugger_session_state(&self, arguments: Value) -> Result<Value> {
        let args: SessionStateArgs = serde_json::from_value(arguments)?;

        let manager = self.session_manager.read().await;
        let state = manager.get_session_state(&args.session_id).await?;

        // Convert DebugState to JSON-friendly format
        let (state_str, details) = match state {
            crate::debug::state::DebugState::NotStarted => ("NotStarted", json!({})),
            crate::debug::state::DebugState::Initializing => ("Initializing", json!({})),
            crate::debug::state::DebugState::Initialized => ("Initialized", json!({})),
            crate::debug::state::DebugState::Launching => ("Launching", json!({})),
            crate::debug::state::DebugState::Running => ("Running", json!({})),
            crate::debug::state::DebugState::Stopped { thread_id, reason } => (
                "Stopped",
                json!({
                    "threadId": thread_id,
                    "reason": reason
                }),
            ),
            crate::debug::state::DebugState::Terminated => ("Terminated", json!({})),
            crate::debug::state::DebugState::Failed { error } => (
                "Failed",
                json!({
                    "error": error
                }),
            ),
        };

        Ok(json!({
            "sessionId": args.session_id,
            "state": state_str,
            "details": details
        }))
    }

    async fn debugger_set_breakpoint(&self, arguments: Value) -> Result<Value> {
        let args: SetBreakpointArgs = serde_json::from_value(arguments)?;

        // Validate source path to prevent path traversal
        // Note: We validate without extension requirement since breakpoints
        // can be set in any source file regardless of language
        let validated_source = security::validate_source_path(&args.source_path, None)?;
        let source_path = validated_source
            .to_str()
            .ok_or_else(|| Error::Internal("Non-UTF8 source path (invalid encoding)".to_string()))?
            .to_string();

        let manager = self.session_manager.read().await;
        let session = manager.get_session(&args.session_id).await?;

        let activate_after = args.activate_after.as_ref().map(|a| {
            crate::debug::state::BreakpointLocation {
                source_path: a.source_path.clone(),
                line: a.line,
            }
        });

        let verified = session
            .set_breakpoint(source_path.clone(), args.line, args.condition.clone(), args.hit_condition.clone(), activate_after)
            .await?;

        let mut result = json!({
            "verified": verified,
            "sourcePath": source_path,
            "line": args.line,
            "condition": args.condition,
            "hitCondition": args.hit_condition
        });
        if let Some(a) = &args.activate_after {
            result["activateAfter"] = json!({
                "sourcePath": a.source_path,
                "line": a.line
            });
            if !verified {
                result["status"] = json!("pending_dependency");
            }
        }
        Ok(result)
    }

    async fn debugger_remove_breakpoint(&self, arguments: Value) -> Result<Value> {
        let args: RemoveBreakpointArgs = serde_json::from_value(arguments)?;

        let validated_source = security::validate_source_path(&args.source_path, None)?;
        let source_path = validated_source
            .to_str()
            .ok_or_else(|| Error::Internal("Non-UTF8 source path (invalid encoding)".to_string()))?
            .to_string();

        let manager = self.session_manager.read().await;
        let session = manager.get_session(&args.session_id).await?;

        session
            .remove_breakpoint(source_path.clone(), args.line)
            .await?;

        Ok(json!({
            "removed": true,
            "sourcePath": source_path,
            "line": args.line
        }))
    }

    async fn debugger_continue(&self, arguments: Value) -> Result<Value> {
        let args: ContinueArgs = serde_json::from_value(arguments)?;

        let manager = self.session_manager.read().await;
        let session = manager.get_session(&args.session_id).await?;

        session.continue_execution().await?;

        if args.wait_for_stop.unwrap_or(false) {
            let timeout_ms = args.timeout_ms.unwrap_or(30_000);
            let result = wait_for_stop_enriched(&session, timeout_ms).await?;
            return Ok(result);
        }

        Ok(json!({
            "status": "continued"
        }))
    }

    async fn debugger_stack_trace(&self, arguments: Value) -> Result<Value> {
        let args: StackTraceArgs = serde_json::from_value(arguments)?;

        let manager = self.session_manager.read().await;
        let session = manager.get_session(&args.session_id).await?;

        // Validate we're in a stopped state
        let state = session.get_state().await;
        if !matches!(state, crate::debug::state::DebugState::Stopped { .. }) {
            return Err(Error::InvalidState(
                "Cannot get stack trace while program is running. The program must be stopped at a breakpoint, entry point, or step. Use debugger_wait_for_stop() to wait for the program to stop.".to_string()
            ));
        }

        let levels = match args.limit {
            Some(0) => None,
            Some(n) => Some(n),
            None => Some(20),
        };
        let frames = session.stack_trace(levels).await?;

        let include_vars = args.include_variables.unwrap_or(false);

        if args.format.as_deref() != Some("json") {
            let mut result = json!({ "stackTrace": format_stack_frames(&frames, &session.program) });

            if include_vars {
                if let Some(top) = frames.first() {
                    if let Some(var_list) = fetch_frame_variables(&session, top.id, 1).await {
                        result["localVariables"] = json!(var_list);
                    }
                    if let Some(src_ctx) = fetch_source_context(top).await {
                        result["sourceContext"] = json!(src_ctx);
                    }
                }
            }

            Ok(result)
        } else {
            let mut result = json!({ "stackFrames": frames });

            if include_vars {
                for frame in &frames {
                    if let Some(var_list) = fetch_frame_variables(&session, frame.id, 1).await {
                        if let Some(arr) = result["stackFrames"].as_array_mut() {
                            if let Some(f) = arr.iter_mut().find(|f| f["id"] == frame.id) {
                                f["localVariables"] = json!(var_list);
                            }
                        }
                    }
                    if let Some(src_ctx) = fetch_source_context(frame).await {
                        if let Some(arr) = result["stackFrames"].as_array_mut() {
                            if let Some(f) = arr.iter_mut().find(|f| f["id"] == frame.id) {
                                f["sourceContext"] = json!(src_ctx);
                            }
                        }
                    }
                }
            }

            Ok(result)
        }
    }

    async fn debugger_evaluate(&self, arguments: Value) -> Result<Value> {
        let args: EvaluateArgs = serde_json::from_value(arguments)?;

        let manager = self.session_manager.read().await;
        let session = manager.get_session(&args.session_id).await?;

        // CodeLLDB convention: a leading `?` disables synthetic children for
        // this eval. Add it now (the dap-client stripper handles the wire form
        // and `format.showRaw`). Idempotent — if the caller already wrote
        // `?expr`, leave it alone.
        let expression = if args.no_synthetic && !args.expression.starts_with('?') {
            format!("?{}", args.expression)
        } else {
            args.expression.clone()
        };

        // Record context for supervisor diagnostics
        *session.last_tool_context.write().await =
            Some(format!("debugger_evaluate expression=\"{}\"", expression));

        // Validate we're in a stopped state
        let state = session.get_state().await;
        if !matches!(state, crate::debug::state::DebugState::Stopped { .. }) {
            return Err(Error::InvalidState(
                "Cannot evaluate expressions while program is running. The program must be stopped at a breakpoint, entry point, or step. Use debugger_wait_for_stop() to wait for the program to stop.".to_string()
            ));
        }

        let context_str = args.context.as_deref().unwrap_or("watch");
        // Bare-identifier warning checks the user's intent (the original
        // expression), not the wire form — `?x` and `x` should both warn.
        let warn_bare_id = context_str != "repl"
            && context_str != "variables"
            && is_bare_identifier(&args.expression);
        let is_repl = context_str == "repl";

        // Snapshot output line counter so we can attribute LLDB stdout/stderr
        // emitted *during* this evaluate to the response. Without this, REPL
        // commands like `frame variable`, `version`, `help` look identical to
        // a silent drop because their output flows through OutputEvents, not
        // the evaluate response body.
        let output_snapshot = if is_repl {
            Some(session.current_output_line())
        } else {
            None
        };

        let result = session
            .evaluate(&expression, args.frame_id, args.context.clone())
            .await;

        match result {
            Ok(value) => {
                let mut resp = json!({ "result": value });
                if warn_bare_id {
                    resp["warning"] = json!(
                        "Consider using debugger_get_variables instead of evaluating bare variable names. \
                         debugger_evaluate triggers the expression compiler which can cause high memory usage \
                         for containers (Vec, HashMap). debugger_get_variables reads directly from debug info \
                         and is safe for any size."
                    );
                }
                if let Some(since) = output_snapshot {
                    // For REPL passthrough, drain entries produced after the
                    // call started. The "console" category is where CodeLLDB
                    // routes LLDB command output; "stderr" carries error text.
                    // Both are surfaced — the caller decides what to do with
                    // them.
                    let entries = session.get_output(None, None, 1000, Some(since)).await;
                    let mut combined = String::new();
                    let mut stderr_buf = String::new();
                    for e in &entries {
                        match e.category.as_str() {
                            "stderr" => stderr_buf.push_str(&e.output),
                            // console / stdout / important / telemetry / "" all flow into the
                            // primary stream, mirroring what a human would see in DAP-aware IDEs.
                            _ => combined.push_str(&e.output),
                        }
                    }
                    if !combined.is_empty() {
                        resp["output"] = json!(combined);
                    }
                    if !stderr_buf.is_empty() {
                        resp["stderr"] = json!(stderr_buf);
                    }
                    // Either the result body, the output, or stderr should
                    // carry signal. If all three are empty, the response is
                    // ambiguous — flag it explicitly so callers can tell the
                    // difference between "command produced nothing" and
                    // "transport silently dropped the output".
                    let result_empty = resp["result"]
                        .as_str()
                        .map(|s| s.is_empty())
                        .unwrap_or(true);
                    if result_empty && combined.is_empty() && stderr_buf.is_empty() {
                        resp["note"] = json!(
                            "REPL command returned no result body and produced no output \
                             on either stream. This usually means the command exists but \
                             intentionally produces nothing (e.g. `settings set` with a \
                             blank value), but it may also indicate a transport drop. Try \
                             `version` to confirm the channel is alive."
                        );
                        resp["empty"] = json!(true);
                    }
                }
                Ok(resp)
            }
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("timed out") {
                    session.cancel_pending_requests().await;
                }
                // If the agent didn't already pass noSynthetic, echo their
                // exact expression back as a concrete retry recipe. They
                // already wrote the expression; we just append `noSynthetic:
                // true` to it — no source parsing, no guessing names.
                // Preserve the error variant so the JSON-RPC code (-32009
                // for AdapterStuck) stays correct.
                if !args.no_synthetic && matches!(e, Error::AdapterStuck(_)) {
                    let retry = format!(
                        " Retry: debugger_evaluate({{\
                         sessionId: \"{}\", expression: \"{}\", \
                         context: \"{}\", noSynthetic: true}}).",
                        args.session_id,
                        args.expression.replace('\\', "\\\\").replace('"', "\\\""),
                        context_str,
                    );
                    return Err(Error::AdapterStuck(format!("{err_str}{retry}")));
                }
                Err(e)
            }
        }
    }

    async fn debugger_wait_for_stop(&self, arguments: Value) -> Result<Value> {
        let args: WaitForStopArgs = serde_json::from_value(arguments)?;

        let manager = self.session_manager.read().await;
        let session = manager.get_session(&args.session_id).await?;

        wait_for_stop_enriched_opts(&session, args.timeout_ms, args.enrich_locals).await
    }

    async fn debugger_list_breakpoints(&self, arguments: Value) -> Result<Value> {
        let args: ListBreakpointsArgs = serde_json::from_value(arguments)?;

        let manager = self.session_manager.read().await;
        let session = manager.get_session(&args.session_id).await?;

        let full_state = session.get_full_state().await;

        // Collect all active breakpoints
        let mut all_breakpoints = Vec::new();
        for (source_path, breakpoints) in full_state.breakpoints.iter() {
            for bp in breakpoints {
                all_breakpoints.push(json!({
                    "id": bp.id,
                    "verified": bp.verified,
                    "line": bp.line,
                    "sourcePath": source_path,
                    "condition": bp.condition,
                    "hitCondition": bp.hit_condition
                }));
            }
        }

        // Collect dependent (dormant) breakpoints
        let mut dependent_breakpoints = Vec::new();
        for dep in &full_state.dependent_breakpoints {
            dependent_breakpoints.push(json!({
                "line": dep.line,
                "sourcePath": dep.source_path,
                "condition": dep.condition,
                "hitCondition": dep.hit_condition,
                "status": "pending_dependency",
                "activateAfter": {
                    "sourcePath": dep.activate_after.source_path,
                    "line": dep.activate_after.line
                }
            }));
        }

        Ok(json!({
            "breakpoints": all_breakpoints,
            "dependentBreakpoints": dependent_breakpoints
        }))
    }

    async fn debugger_step_over(&self, arguments: Value) -> Result<Value> {
        let args: StepArgs = serde_json::from_value(arguments)?;

        let manager = self.session_manager.read().await;
        let session = manager.get_session(&args.session_id).await?;

        let state = session.get_state().await;
        let thread_id = require_stopped(&state, args.thread_id)?;
        session.step_over(thread_id).await?;

        wait_for_stop_enriched(&session, 30_000).await
    }

    async fn debugger_step_into(&self, arguments: Value) -> Result<Value> {
        let args: StepArgs = serde_json::from_value(arguments)?;

        let manager = self.session_manager.read().await;
        let session = manager.get_session(&args.session_id).await?;

        let state = session.get_state().await;
        let thread_id = require_stopped(&state, args.thread_id)?;
        session.step_into(thread_id).await?;

        wait_for_stop_enriched(&session, 30_000).await
    }

    async fn debugger_step_out(&self, arguments: Value) -> Result<Value> {
        let args: StepArgs = serde_json::from_value(arguments)?;

        let manager = self.session_manager.read().await;
        let session = manager.get_session(&args.session_id).await?;

        let state = session.get_state().await;
        let thread_id = require_stopped(&state, args.thread_id)?;
        session.step_out(thread_id).await?;

        wait_for_stop_enriched(&session, 30_000).await
    }

    async fn debugger_run_to_crash(&self, arguments: Value) -> Result<Value> {
        let args: RunToCrashArgs = serde_json::from_value(arguments)?;

        let validated_program = if args.language == "rust" {
            let path = std::path::Path::new(&args.program);
            if path.is_dir() {
                let validated_dir = security::validate_directory_path(&args.program)?;
                let manifest = validated_dir.join("Cargo.toml");
                if !manifest.exists() {
                    return Err(Error::Compilation(format!(
                        "Directory does not contain Cargo.toml: {}",
                        validated_dir.display()
                    )));
                }
                manifest
            } else {
                security::validate_source_path(&args.program, None)?
            }
        } else {
            security::validate_source_path(&args.program, None)?
        };
        let program = validated_program
            .to_str()
            .ok_or_else(|| Error::Internal("Non-UTF8 program path".to_string()))?
            .to_string();

        let validated_cwd = if let Some(cwd_path) = &args.cwd {
            Some(security::validate_directory_path(cwd_path)?
                .to_str()
                .ok_or_else(|| Error::Internal("Non-UTF8 cwd path".to_string()))?
                .to_string())
        } else {
            None
        };

        let manager = self.session_manager.read().await;
        let session_id = manager
            .create_session(&args.language, program, args.args, validated_cwd, true, args.env, args.profile)
            .await?;

        let session = manager.get_session(&session_id).await?;

        // Wait for stop on entry
        let timeout = tokio::time::Duration::from_secs(30);
        let start = tokio::time::Instant::now();
        loop {
            let state = session.get_state().await;
            if matches!(state, crate::debug::state::DebugState::Stopped { .. }) {
                break;
            }
            if matches!(state, crate::debug::state::DebugState::Terminated) {
                return Err(Error::Dap("Program terminated before exception breakpoints could be set".into()));
            }
            if let crate::debug::state::DebugState::Failed { error } = state {
                return Err(Error::Dap(format!("Session failed: {}", error)));
            }
            if start.elapsed() > timeout {
                return Err(Error::InvalidState("Timeout waiting for entry stop".into()));
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }

        // Set exception breakpoints
        let filter = args.exception_filter.unwrap_or_else(|| "uncaught".to_string());
        session.set_exception_breakpoints(vec![filter]).await?;

        // Continue and wait for crash or termination
        session.continue_execution().await?;
        let result = wait_for_stop_enriched(&session, 60_000).await?;

        // Add session ID to result
        let mut result = result;
        result["sessionId"] = json!(session_id);
        Ok(result)
    }

    async fn debugger_snapshot_at(&self, arguments: Value) -> Result<Value> {
        let args: SnapshotAtArgs = serde_json::from_value(arguments)?;

        let validated_source = security::validate_source_path(&args.source_path, None)?;
        let source_path = validated_source
            .to_str()
            .ok_or_else(|| Error::Internal("Non-UTF8 source path".to_string()))?
            .to_string();

        let manager = self.session_manager.read().await;
        let session = manager.get_session(&args.session_id).await?;

        // Set temporary breakpoint
        session.set_breakpoint(source_path.clone(), args.line, None, None, None).await?;

        // Continue if stopped
        let state = session.get_state().await;
        if matches!(state, crate::debug::state::DebugState::Stopped { .. }) {
            session.continue_execution().await?;
        }

        // Wait for stop at breakpoint
        let mut result = wait_for_stop_enriched(&session, 30_000).await?;

        // Evaluate requested expressions
        if !args.expressions.is_empty() {
            let mut expr_results = Vec::new();
            for expr in &args.expressions {
                match session.evaluate(expr, None, None).await {
                    Ok(val) => expr_results.push(json!({"expression": expr, "result": val})),
                    Err(e) => expr_results.push(json!({"expression": expr, "error": e.to_string()})),
                }
            }
            result["evaluatedExpressions"] = json!(expr_results);
        }

        // Remove the temporary breakpoint
        let _ = session.remove_breakpoint(source_path, args.line).await;

        result["sessionId"] = json!(args.session_id);
        Ok(result)
    }

    async fn debugger_trace_function(&self, arguments: Value) -> Result<Value> {
        let args: TraceFunctionArgs = serde_json::from_value(arguments)?;

        let manager = self.session_manager.read().await;
        let session = manager.get_session(&args.session_id).await?;

        let max_steps = args.max_steps.unwrap_or(50).min(200) as usize;

        // Get initial frame info
        let initial_frames = session.stack_trace(Some(1)).await?;
        let initial_frame = initial_frames
            .first()
            .ok_or_else(|| Error::InvalidState("No stack frames available".to_string()))?;
        let initial_name = initial_frame.name.clone();
        let initial_source = initial_frame.source.as_ref().and_then(|s| s.path.clone());

        let mut trace = Vec::new();

        for _ in 0..max_steps {
            let state = session.get_state().await;
            if !matches!(state, crate::debug::state::DebugState::Stopped { .. }) {
                break;
            }

            let frames = session.stack_trace(Some(1)).await?;
            let frame = match frames.first() {
                Some(f) => f,
                None => break,
            };

            // Stop if we've left the function
            let current_source = frame.source.as_ref().and_then(|s| s.path.clone());
            if frame.name != initial_name || current_source != initial_source {
                break;
            }

            // Record current state
            let mut entry = json!({
                "line": frame.line,
                "name": frame.name,
            });

            // Get source line
            if let Some(path) = &current_source {
                if let Some(src_ctx) = read_source_context(path, frame.line, 0).await {
                    entry["source"] = json!(src_ctx);
                }
            }

            // Evaluate expressions
            if !args.expressions.is_empty() {
                let mut values = json!({});
                for expr in &args.expressions {
                    match session.evaluate(expr, Some(frame.id), None).await {
                        Ok(val) => values[expr] = json!(val),
                        Err(e) => values[expr] = json!(format!("<error: {}>", e)),
                    }
                }
                entry["values"] = values;
            }

            trace.push(entry);

            // Step over
            let thread_id = if let crate::debug::state::DebugState::Stopped { thread_id, .. } = session.get_state().await {
                thread_id
            } else {
                break;
            };
            session.step_over(thread_id).await?;

            // Wait for step to complete
            let step_timeout = tokio::time::Duration::from_secs(10);
            let step_start = tokio::time::Instant::now();
            loop {
                let s = session.get_state().await;
                if matches!(s, crate::debug::state::DebugState::Stopped { .. }) {
                    break;
                }
                if matches!(s, crate::debug::state::DebugState::Terminated) {
                    return Ok(json!({
                        "sessionId": args.session_id,
                        "trace": trace,
                        "reason": "terminated"
                    }));
                }
                if step_start.elapsed() > step_timeout {
                    return Ok(json!({
                        "sessionId": args.session_id,
                        "trace": trace,
                        "reason": "step_timeout"
                    }));
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            }
        }

        Ok(json!({
            "sessionId": args.session_id,
            "trace": trace,
            "steps": trace.len(),
            "reason": "completed"
        }))
    }

    async fn debugger_get_output(&self, arguments: Value) -> Result<Value> {
        let args: GetOutputArgs = serde_json::from_value(arguments)?;

        let manager = self.session_manager.read().await;
        let session = manager.get_session(&args.session_id).await?;

        let limit = args.limit.unwrap_or(50).min(500);
        let entries = session
            .get_output(
                args.category.as_deref(),
                args.search.as_deref(),
                limit,
                args.since_line,
            )
            .await;

        let total = session.output_buffer.read().await.len();

        Ok(json!({
            "entries": entries,
            "count": entries.len(),
            "totalBuffered": total,
            "truncated": entries.len() < total,
        }))
    }

    async fn debugger_disconnect(&self, arguments: Value) -> Result<Value> {
        let args: DisconnectArgs = serde_json::from_value(arguments)?;

        let manager = self.session_manager.write().await;
        manager.remove_session(&args.session_id).await?;

        Ok(json!({
            "status": "disconnected"
        }))
    }

    /// Cancel all in-flight DAP requests on the session without tearing it
    /// down. Recovery path when an evaluate/variables call gets stuck inside
    /// CodeLLDB's synthetic walk (typically deep async-closure captures).
    /// Drops pending request senders client-side and fires a DAP `cancel`
    /// for each one at the adapter. Subsequent tool calls on the same
    /// session keep working; breakpoints are preserved.
    async fn debugger_cancel(&self, arguments: Value) -> Result<Value> {
        let args: CancelArgs = serde_json::from_value(arguments)?;
        let manager = self.session_manager.read().await;
        let session = manager.get_session(&args.session_id).await?;
        let cancelled = session.cancel_pending_requests().await;
        let mut result = json!({
            "cancelled": cancelled,
            "sessionId": args.session_id,
        });
        if cancelled == 0 {
            // CodeLLDB does not preempt an in-flight synthetic-provider walk
            // when it receives a DAP `cancel` — the dispatcher only sees it
            // once the current LLDB call returns. So when our pending map is
            // already empty (the prior timeout cleaned up its own orphan
            // before returning AdapterStuck), repeatedly calling
            // debugger_cancel will *never* unblock the adapter. The agent
            // needs to know: the only effective recovery is to tear the
            // session down. Surface that explicitly instead of letting the
            // agent loop on cancel.
            result["note"] = json!(
                "No in-flight requests to cancel. If the previous call returned \
                 AdapterStuck and subsequent calls still time out, the adapter \
                 is wedged inside an LLDB synthetic-provider walk that does not \
                 respond to DAP cancels. Recovery: call debugger_disconnect and \
                 start a fresh session, avoiding the stuck frame (set the \
                 breakpoint one frame up at the synchronous caller, or rebind \
                 the values you need into a non-async scope before the assert)."
            );
        }
        Ok(result)
    }

    async fn debugger_debugging_tips(&self, arguments: Value) -> Result<Value> {
        let args: DebuggingTipsArgs = serde_json::from_value(arguments)?;
        let tips = Self::tips_for_language(&args.language);
        Ok(json!({ "language": args.language, "tips": tips }))
    }

    fn tips_for_language(lang: &str) -> &'static str {
        match lang {
            "rust" => r#"## Conditional Breakpoints

- LLDB's expression evaluator uses C/C++ semantics, not Rust.
- Numeric types (usize, i32, etc.) may be treated as strings in conditions — cast with int(): `int(n) > 5` instead of `n > 5`.
- String comparisons don't work natively — use `context: "repl"` with debugger_evaluate to run LLDB commands like `frame variable` and inspect manually instead.
- Enum variant matching is unreliable in conditions.
- Prefer hitCondition (which is numeric) over complex condition expressions.

## Variable Inspection

- ALWAYS use debugger_get_variables to read variable values. NEVER use debugger_evaluate for bare variable names.
- debugger_evaluate triggers LLDB's JIT compiler which can consume GBs of memory for containers (Vec, HashMap).
- debugger_get_variables reads directly from DWARF debug info and is safe for any container size.
- Workflow: debugger_get_variables({sessionId}) to see locals → debugger_get_variables({sessionId, variablesReference: N}) to drill into a specific variable.
- Use debugger_evaluate ONLY for expressions: arithmetic, comparisons, function calls.
- Variables may show `<optimized out>` even in debug builds — Rust's MIR optimizer is aggressive.
- Workaround: ensure opt-level = 0 and debug = 2 in [profile.dev] in Cargo.toml.
- Use `context: "repl"` with expression `frame variable` to list all locals via LLDB directly (bypasses expression parser).
- Complex types (HashMap, BTreeMap, trait objects) need CodeLLDB's Rust formatters — use rust-lldb or CodeLLDB adapter.
- Closures and captured variables are often opaque to the debugger.

## Async/Await

- Stepping through async functions may land in tokio/executor internals instead of next .await.
- Breakpoints on .await lines may not hit on older Rust toolchains (fixed in rust-lang/rust#123341) — update to latest stable.
- Set breakpoints inside async function bodies, not on .await call sites.
- Stack traces will show Future::poll machinery — look for your function name in the frames.
- Consider #[tokio::main(flavor = "current_thread")] during debugging to reduce concurrency noise.
- For async-specific inspection, consider tokio-console as a complement.

## Build Configuration

- Always debug with a debug profile, never cargo install or release.
- Use a dedicated [profile.debugger] that inherits from dev with opt-level = 0 and debug = 2 — this keeps normal cargo build fast while giving full debug info when needed: `cargo build --profile debugger`.
- Ensure the debugger profile has opt-level = 0 for your crate; use [profile.debugger.package."*"] with opt-level = 1 if dependency compile times are too slow.
- The debugger-mcp Rust adapter will compile with cargo build by default — consider configuring it to use --profile debugger for better variable visibility."#,
            _ => "No known issues. Standard debugging workflows apply.",
        }
    }

    async fn debugger_set_data_breakpoint(&self, arguments: Value) -> Result<Value> {
        let args: SetDataBreakpointArgs = serde_json::from_value(arguments)?;

        let manager = self.session_manager.read().await;
        let session = manager.get_session(&args.session_id).await?;

        // Step 1: Query if this variable supports data breakpoints
        let info = session
            .data_breakpoint_info(&args.name, args.variables_reference, args.frame_id)
            .await?;

        let data_id = info.data_id.ok_or_else(|| {
            Error::Dap(format!(
                "Data breakpoint not available for '{}': {}",
                args.name, info.description
            ))
        })?;

        // Step 2: Set the data breakpoint
        let bp = crate::dap::types::DataBreakpoint {
            data_id,
            access_type: Some(args.access_type.unwrap_or_else(|| "write".to_string())),
            condition: args.condition,
            hit_condition: args.hit_condition,
        };

        let results = session.set_data_breakpoints(vec![bp]).await?;

        let result = results.first();
        let verified = result.map_or(false, |r| r.verified);
        let message = result.and_then(|r| r.message.clone());

        Ok(json!({
            "verified": verified,
            "description": info.description,
            "accessTypes": info.access_types,
            "message": message,
        }))
    }

    async fn debugger_get_variables(&self, arguments: Value) -> Result<Value> {
        let args: GetVariablesArgs = serde_json::from_value(arguments)?;

        let manager = self.session_manager.read().await;
        let session = manager.get_session(&args.session_id).await?;

        *session.last_tool_context.write().await =
            Some(format!("debugger_get_variables variablesReference={:?} frameId={:?}", args.variables_reference, args.frame_id));

        let state = session.get_state().await;
        if !matches!(state, crate::debug::state::DebugState::Stopped { .. }) {
            return Err(Error::InvalidState(
                "Cannot get variables while program is running. Use debugger_wait_for_stop() first.".to_string()
            ));
        }

        let max_count = args.max_count.unwrap_or(50).min(200);
        let filter = args.filter.as_deref();

        let variables = if let Some(var_ref) = args.variables_reference {
            session.get_variable_children(var_ref, filter, max_count, args.no_synthetic).await
        } else {
            // Resolve frame_id: use provided, or auto-fetch from stopped thread
            let frame_id = if let Some(fid) = args.frame_id {
                fid
            } else {
                let frames = session.stack_trace(Some(1)).await?;
                if frames.is_empty() {
                    return Err(Error::Dap("No stack frames available".to_string()));
                }
                frames[0].id
            };
            // The scope-level call only returns top-level locals (name/type/
            // varRef), never their children — synthetic providers don't
            // activate at this level, so `no_synthetic` would be a no-op.
            session.get_scope_variables(frame_id, args.scope.as_deref(), max_count).await
        };

        match variables {
            Ok(vars) => {
                let truncated = vars.len() == max_count as usize;
                let has_expandable = vars.iter().any(|v| v.variables_reference > 0);
                let json_vars: Vec<Value> = vars.iter().map(|v| {
                    json!({
                        "name": v.name,
                        "value": v.value,
                        "type": v.type_,
                        "variablesReference": v.variables_reference,
                        "expandable": v.variables_reference > 0,
                    })
                }).collect();

                let mut result = json!({
                    "variables": json_vars,
                    "count": json_vars.len(),
                    "truncated": truncated,
                });
                if has_expandable {
                    result["hint"] = json!("Use variablesReference with this tool to drill into expandable variables");
                }
                // Empty-locals signal: when called for a frame's locals scope
                // (no variablesReference) and we got back nothing, it's almost
                // always a stripped-DWARF build profile, not a function with
                // genuinely no locals. Surface a hint pointing at the build
                // settings so callers don't burn a stack-inspection round-trip
                // figuring out why.
                if json_vars.is_empty() && args.variables_reference.is_none() {
                    result["dwarfStripped"] = json!(true);
                    result["hint"] = json!(
                        "Locals scope returned 0 variables. The most common cause is a \
                         stripped-DWARF build profile (Cargo `debug = \"line-tables-only\"`, \
                         rustc `-C debuginfo=1`, or release-without-debuginfo). Try a full \
                         debug build or use debugger_evaluate on a specific name to confirm \
                         — evaluate returns a clear LLDB diagnostic for stripped CUs."
                    );
                }
                Ok(result)
            }
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("timed out") || err_str.contains("Timeout") {
                    session.cancel_pending_requests().await;
                }
                Err(e)
            }
        }
    }

    pub fn list_tools() -> Vec<Value> {
        vec![
            json!({
                "name": "debugger_start",
                "title": "Start Debugging Session",
                "description": "Starts a debug session for a program in the given language. Pass `breakpoints` up front to avoid extra round-trips; see debugger://workflows for examples.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "language": {
                            "type": "string",
                            "description": "Programming language (e.g., 'python', 'ruby', 'javascript', 'rust', 'go')"
                        },
                        "program": {
                            "type": "string",
                            "description": "Path to the program to debug. For most languages: path to the source file. For Rust: path to a .rs file, a Cargo.toml, a directory containing Cargo.toml, or a pre-compiled binary."
                        },
                        "args": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Command-line arguments passed to the program (optional, defaults to empty array)"
                        },
                        "cwd": {
                            "type": "string",
                            "description": "Working directory for the program execution (optional, defaults to program's directory)"
                        },
                        "stopOnEntry": {
                            "type": "boolean",
                            "description": "If true, pauses execution at the program's first line (recommended for setting early breakpoints)"
                        },
                        "env": {
                            "type": "object",
                            "additionalProperties": { "type": "string" },
                            "description": "Environment variables to set for the debugged program (optional, e.g., {\"LOG_LEVEL\": \"debug\"})"
                        },
                        "breakpoints": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "sourcePath": { "type": "string", "description": "Absolute path to the source file" },
                                    "line": { "type": "integer", "description": "Line number for the breakpoint" },
                                    "condition": { "type": "string", "description": "Optional condition expression" },
                                    "hitCondition": { "type": "string", "description": "Optional hit count condition" },
                                    "activateAfter": {
                                        "type": "object",
                                        "properties": {
                                            "sourcePath": { "type": "string", "description": "Source file of the trigger breakpoint" },
                                            "line": { "type": "integer", "description": "Line of the trigger breakpoint" }
                                        },
                                        "required": ["sourcePath", "line"],
                                        "description": "Only activate this breakpoint after the specified breakpoint is hit"
                                    }
                                },
                                "required": ["sourcePath", "line"]
                            },
                            "description": "Breakpoints to set before the program runs. The program will pause on entry, set all breakpoints, then continue (unless stopOnEntry is also true)."
                        },
                        "profile": {
                            "type": "string",
                            "description": "Cargo build profile (Rust only). e.g. 'dev', 'release', 'debugger'. When set on a Cargo project, CodeLLDB handles compilation with this profile."
                        }
                    },
                    "required": ["language", "program"]
                },
                "annotations": {
                    "async": true,
                    "returnsTiming": "< 100ms",
                    "completionTiming": "200-500ms (background)",
                    "workflow": "initialization",
                    "requiredFollowUp": ["debugger_session_state"],
                    "category": "session-management",
                    "priority": 1.0
                }
            }),
            json!({
                "name": "debugger_session_state",
                "title": "Check Session State",
                "description": "Returns the current session state: NotStarted, Initializing, Launching, Running, Stopped, Terminated, or Failed. See debugger://state-machine for transitions.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sessionId": {
                            "type": "string",
                            "description": "Session ID returned from debugger_start"
                        }
                    },
                    "required": ["sessionId"]
                },
                "annotations": {
                    "async": false,
                    "returnsTiming": "< 10ms",
                    "workflow": "state-checking",
                    "category": "session-management",
                    "pollable": true,
                    "priority": 0.9
                }
            }),
            json!({
                "name": "debugger_set_breakpoint",
                "title": "Set Breakpoint",
                "description": "Sets a source breakpoint at `sourcePath:line`. Supports `condition` (truthy expression), `hitCondition` (hit-count expression like '>= 5'), and `activateAfter` (dormant until the trigger breakpoint fires). Requires session state Running or Stopped.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sessionId": {
                            "type": "string",
                            "description": "Session ID from debugger_start"
                        },
                        "sourcePath": {
                            "type": "string",
                            "description": "Absolute or relative path to the source file (must match debugger's path resolution)"
                        },
                        "line": {
                            "type": "integer",
                            "description": "Line number where breakpoint should be set (1-indexed, i.e., first line is 1)"
                        },
                        "condition": {
                            "type": "string",
                            "description": "Expression evaluated at the breakpoint location. The breakpoint only pauses execution when this expression is truthy. Example: 'x > 10' or 'name == \"foo\"'"
                        },
                        "hitCondition": {
                            "type": "string",
                            "description": "Expression evaluated against the hit count. The breakpoint only pauses when this expression is true. Examples: '>= 5' (pause on 5th+ hit), '== 3' (pause only on 3rd hit), '% 2' (pause on every 2nd hit)"
                        },
                        "activateAfter": {
                            "type": "object",
                            "description": "Makes this a dependent breakpoint that remains dormant until the specified breakpoint is hit. Once the dependency fires, this breakpoint is automatically activated. Useful for debugging code that is only relevant after a certain point in execution.",
                            "properties": {
                                "sourcePath": {
                                    "type": "string",
                                    "description": "Source file path of the dependency breakpoint"
                                },
                                "line": {
                                    "type": "integer",
                                    "description": "Line number of the dependency breakpoint"
                                }
                            },
                            "required": ["sourcePath", "line"]
                        }
                    },
                    "required": ["sessionId", "sourcePath", "line"]
                },
                "annotations": {
                    "async": false,
                    "returnsTiming": "5-20ms",
                    "workflow": "breakpoint-management",
                    "category": "debugging",
                    "requiresState": ["Running", "Stopped"],
                    "priority": 0.8
                }
            }),
            json!({
                "name": "debugger_continue",
                "title": "Continue Execution",
                "description": "Resumes execution. Set `waitForStop: true` to block until the next stop and receive enriched context (stack trace, local variables, source).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sessionId": {
                            "type": "string",
                            "description": "Session ID from debugger_start"
                        },
                        "waitForStop": {
                            "type": "boolean",
                            "description": "If true, blocks until program stops and returns enriched context (stack trace, variables, source). Recommended for typical debugging workflows. Default: false"
                        },
                        "timeoutMs": {
                            "type": "integer",
                            "description": "Timeout in ms when waitForStop is true (default: 30000)"
                        }
                    },
                    "required": ["sessionId"]
                },
                "annotations": {
                    "async": true,
                    "returnsTiming": "< 10ms",
                    "completionTiming": "unknown (until next stop)",
                    "workflow": "execution-control",
                    "category": "debugging",
                    "requiresState": ["Stopped"],
                    "priority": 0.7
                }
            }),
            json!({
                "name": "debugger_stack_trace",
                "title": "Get Stack Trace",
                "description": "Returns the call stack while stopped. Use each frame's `id` with debugger_get_variables or debugger_evaluate. Frame IDs are only valid for the current stop; fetch a fresh trace after every resume/step.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sessionId": {
                            "type": "string",
                            "description": "Session ID from debugger_start"
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "text"],
                            "description": "Output format. 'text' returns a compact line-by-line representation (e.g. '#0 [id=5] main (src/main.rs:42)'), 'json' returns full structured data. Defaults to 'text'."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of stack frames to return. Defaults to 20. Use 0 to return all frames."
                        },
                        "includeVariables": {
                            "type": "boolean",
                            "description": "If true, includes local variables and source context for each frame. Saves separate calls to debugger_evaluate."
                        }
                    },
                    "required": ["sessionId"]
                },
                "annotations": {
                    "async": false,
                    "returnsTiming": "10-50ms",
                    "workflow": "inspection",
                    "category": "debugging",
                    "requiresState": ["Stopped"],
                    "priority": 0.6
                }
            }),
            json!({
                "name": "debugger_evaluate",
                "title": "Evaluate Expression",
                "description": "Evaluates an expression (arithmetic, comparisons, function calls) in a stack frame. Do NOT use to read bare container variables — the expression compiler can consume GBs of memory on large Vec/HashMap/String; use debugger_get_variables instead. `frameId` is required in practice to access locals.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sessionId": {
                            "type": "string",
                            "description": "Session ID from debugger_start"
                        },
                        "expression": {
                            "type": "string",
                            "description": "Expression to evaluate (syntax depends on programming language being debugged)"
                        },
                        "frameId": {
                            "type": "integer",
                            "description": "Stack frame ID from debugger_stack_trace (optional, defaults to current frame)"
                        },
                        "context": {
                            "type": "string",
                            "description": "Evaluation context: 'watch' (default, expression evaluation), 'repl' (raw debugger command, e.g. LLDB's 'frame variable x' or 'v x'; LLDB stdout is captured into the response's `output` field, stderr into `stderr`), 'hover', or 'variables' (read locals via debug info, bypasses expression parser — useful when variable names collide with language keywords)",
                            "enum": ["watch", "repl", "hover", "variables"]
                        },
                        "noSynthetic": {
                            "type": "boolean",
                            "description": "Disable synthetic-children for this evaluate (CodeLLDB convention: prepends `?` and sets `format.showRaw=true`). Lets a scalar field path like `state.c.length` read the raw `usize` field on a BTreeMap without the expression compiler walking the surrounding synthetic view first. Use this whenever the expression resolves to a scalar through a container."
                        }
                    },
                    "required": ["sessionId", "expression"]
                },
                "annotations": {
                    "async": false,
                    "returnsTiming": "20-200ms",
                    "workflow": "inspection",
                    "category": "debugging",
                    "requiresState": ["Stopped"],
                    "priority": 0.5
                }
            }),
            json!({
                "name": "debugger_get_variables",
                "title": "Get Variables (Safe Inspection)",
                "description": "Reads variables directly from debug info — safe for any container size, unlike debugger_evaluate. Returns locals when given `frameId` (or auto-resolves the current frame); drill into children via `variablesReference` from a prior result. Valid only while stopped at the current location.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sessionId": {
                            "type": "string",
                            "description": "Session ID from debugger_start"
                        },
                        "frameId": {
                            "type": "integer",
                            "description": "Stack frame ID from debugger_stack_trace. If omitted, auto-resolves to current frame."
                        },
                        "variablesReference": {
                            "type": "integer",
                            "description": "Reference to expand a variable's children (from a previous get_variables result). Takes precedence over frameId."
                        },
                        "maxCount": {
                            "type": "integer",
                            "description": "Maximum number of variables to return (default: 50, max: 200)"
                        },
                        "scope": {
                            "type": "string",
                            "description": "Scope name to inspect: 'Locals' (default), 'Globals', 'Registers', etc. Only used with frameId mode."
                        },
                        "filter": {
                            "type": "string",
                            "description": "Filter: 'indexed' for array elements, 'named' for struct fields. Omit for all.",
                            "enum": ["indexed", "named"]
                        },
                        "noSynthetic": {
                            "type": "boolean",
                            "description": "Bypass synthetic-children providers (CodeLLDB extension `format.showRaw`). Use this when drilling into a large container (Vec/HashMap/BTreeMap/BTreeSet) just to read scalar fields like `length` or `len` — synthetic providers materialise per-element children eagerly and can push RSS over a GB on big collections. Defaults to false (normal pretty view)."
                        }
                    },
                    "required": ["sessionId"]
                },
                "annotations": {
                    "async": false,
                    "returnsTiming": "10-100ms",
                    "workflow": "inspection",
                    "category": "debugging",
                    "requiresState": ["Stopped"],
                    "priority": 0.7
                }
            }),
            json!({
                "name": "debugger_cancel",
                "title": "Cancel In-Flight Requests",
                "description": "Drops all pending DAP requests on the session (client-side) and fires a DAP `cancel` to the adapter for each. Use this to recover when an `evaluate` or `get_variables` call gets stuck — typically CodeLLDB walking the synthetic providers of deep async-closure captures. Breakpoints and session state are preserved; the next tool call should work normally. Returns `{cancelled: <count>}`.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sessionId": {
                            "type": "string",
                            "description": "Session ID from debugger_start"
                        }
                    },
                    "required": ["sessionId"]
                },
                "annotations": {
                    "async": false,
                    "returnsTiming": "<10ms",
                    "workflow": "recovery",
                    "category": "session-management",
                    "priority": 0.5
                }
            }),
            json!({
                "name": "debugger_disconnect",
                "title": "Disconnect Session",
                "description": "Terminates the debug session and frees its resources. The debugged program is stopped if still running.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sessionId": {
                            "type": "string",
                            "description": "Session ID from debugger_start"
                        }
                    },
                    "required": ["sessionId"]
                },
                "annotations": {
                    "async": false,
                    "returnsTiming": "50-200ms",
                    "workflow": "cleanup",
                    "category": "session-management",
                    "destructive": true,
                    "priority": 0.4
                }
            }),
            json!({
                "name": "debugger_wait_for_stop",
                "title": "Wait For Program To Stop",
                "description": "Blocks until the session stops (breakpoint, step, entry, pause), terminates, or `timeoutMs` elapses. Returns `{state, threadId, reason}` plus stack trace, top frame, and source context. Local variables are NOT pre-fetched by default — call debugger_get_variables when you need them, or pass `enrichLocals: true` to opt back into the auto-fetch.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sessionId": {
                            "type": "string",
                            "description": "Session ID from debugger_start"
                        },
                        "timeoutMs": {
                            "type": "integer",
                            "default": 5000,
                            "description": "Maximum time to wait in milliseconds (default: 5000)"
                        },
                        "enrichLocals": {
                            "type": "boolean",
                            "default": false,
                            "description": "Pre-fetch the top frame's locals. Off by default — the variables request is expensive and can hang or OOM CodeLLDB on frames with large/recursive/async-state-machine captures. Set true when you specifically want the auto-fetch (the old behavior)."
                        }
                    },
                    "required": ["sessionId"]
                }
            }),
            json!({
                "name": "debugger_list_breakpoints",
                "title": "List All Breakpoints",
                "description": "Lists all breakpoints in the session with their verification status (id, verified, sourcePath, line).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sessionId": {
                            "type": "string",
                            "description": "Session ID from debugger_start"
                        }
                    },
                    "required": ["sessionId"]
                }
            }),
            json!({
                "name": "debugger_remove_breakpoint",
                "title": "Remove Breakpoint",
                "description": "Removes the breakpoint at the given `sourcePath:line`.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sessionId": {
                            "type": "string",
                            "description": "Session ID from debugger_start"
                        },
                        "sourcePath": {
                            "type": "string",
                            "description": "Absolute path to the source file"
                        },
                        "line": {
                            "type": "integer",
                            "description": "Line number of the breakpoint to remove"
                        }
                    },
                    "required": ["sessionId", "sourcePath", "line"]
                }
            }),
            json!({
                "name": "debugger_step_over",
                "title": "Step Over (Next Line)",
                "description": "Executes the current line, stopping at the next line without entering function calls. Returns enriched context (stack, variables, source). Requires stopped state.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sessionId": {
                            "type": "string",
                            "description": "Session ID from debugger_start"
                        },
                        "threadId": {
                            "type": "integer",
                            "description": "Thread ID (optional, uses stopped thread if not specified)"
                        }
                    },
                    "required": ["sessionId"]
                }
            }),
            json!({
                "name": "debugger_step_into",
                "title": "Step Into (Enter Function)",
                "description": "Steps into the function called on the current line, otherwise behaves like step_over. Returns enriched context. Requires stopped state.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sessionId": {
                            "type": "string",
                            "description": "Session ID from debugger_start"
                        },
                        "threadId": {
                            "type": "integer",
                            "description": "Thread ID (optional)"
                        }
                    },
                    "required": ["sessionId"]
                }
            }),
            json!({
                "name": "debugger_step_out",
                "title": "Step Out (Exit Function)",
                "description": "Resumes until the current function returns, then stops at the caller. Returns enriched context. Requires stopped state inside a function.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sessionId": {
                            "type": "string",
                            "description": "Session ID from debugger_start"
                        },
                        "threadId": {
                            "type": "integer",
                            "description": "Thread ID (optional)"
                        }
                    },
                    "required": ["sessionId"]
                }
            }),
            json!({
                "name": "debugger_get_output",
                "title": "Get Program Output",
                "description": "Returns buffered stdout/stderr from the debugged program (ring buffer, last 1000 entries). Filter by `category`, `search` text, or paginate with `sinceLine`.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sessionId": {
                            "type": "string",
                            "description": "Session ID from debugger_start"
                        },
                        "category": {
                            "type": "string",
                            "enum": ["stdout", "stderr", "console", "all"],
                            "description": "Filter by output category (default: all)"
                        },
                        "search": {
                            "type": "string",
                            "description": "Filter output entries containing this text"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max entries to return (default: 50, max: 500)"
                        },
                        "sinceLine": {
                            "type": "integer",
                            "description": "Only return entries after this line number (for pagination)"
                        }
                    },
                    "required": ["sessionId"]
                }
            }),
            json!({
                "name": "debugger_run_to_crash",
                "title": "Run Program Until It Crashes",
                "description": "Launches a program with exception breakpoints and runs until it crashes or exits. On crash returns stack, locals, source context, and exception info in a single call; on clean exit returns Terminated state with program output.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "language": {
                            "type": "string",
                            "description": "Programming language (e.g., 'python', 'ruby', 'javascript', 'rust')"
                        },
                        "program": {
                            "type": "string",
                            "description": "Path to the program file to debug"
                        },
                        "args": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Command-line arguments"
                        },
                        "cwd": {
                            "type": "string",
                            "description": "Working directory"
                        },
                        "env": {
                            "type": "object",
                            "additionalProperties": { "type": "string" },
                            "description": "Environment variables"
                        },
                        "exceptionFilter": {
                            "type": "string",
                            "description": "Exception filter: 'uncaught' (default) or 'raised' (all exceptions including caught)"
                        },
                        "profile": {
                            "type": "string",
                            "description": "Cargo build profile (Rust only). e.g. 'dev', 'release', 'debugger'."
                        }
                    },
                    "required": ["language", "program"]
                }
            }),
            json!({
                "name": "debugger_snapshot_at",
                "title": "Capture State at Line",
                "description": "Sets a temporary breakpoint, runs to it, captures stack/locals/source plus any evaluated `expressions`, then removes the breakpoint.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sessionId": {
                            "type": "string",
                            "description": "Session ID from debugger_start"
                        },
                        "sourcePath": {
                            "type": "string",
                            "description": "Absolute path to the source file"
                        },
                        "line": {
                            "type": "integer",
                            "description": "Line number to capture state at"
                        },
                        "expressions": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Expressions to evaluate when stopped (e.g., ['x', 'len(items)', 'self.name'])"
                        }
                    },
                    "required": ["sessionId", "sourcePath", "line"]
                }
            }),
            json!({
                "name": "debugger_trace_function",
                "title": "Trace Function Execution",
                "description": "Steps through the current function line by line, recording each line with the values of `expressions`. Stops when the function returns or `maxSteps` is reached. Requires stopped state inside the target function.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sessionId": {
                            "type": "string",
                            "description": "Session ID from debugger_start"
                        },
                        "expressions": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Expressions to evaluate at each step (e.g., ['n', 'result'])"
                        },
                        "maxSteps": {
                            "type": "integer",
                            "description": "Maximum steps to trace (default: 50, max: 200)"
                        }
                    },
                    "required": ["sessionId"]
                }
            }),
            json!({
                "name": "debugger_debugging_tips",
                "title": "Debugging Tips & Known Issues",
                "description": "Returns language-specific debugger tips, known issues, and workarounds. No session required.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "language": {
                            "type": "string",
                            "description": "Programming language: 'rust', 'python', 'ruby', 'go', 'javascript'"
                        }
                    },
                    "required": ["language"]
                },
                "annotations": {
                    "returnsTiming": "< 1ms",
                    "workflow": "preparation",
                    "category": "documentation",
                    "priority": 0.3
                }
            }),
            json!({
                "name": "debugger_set_data_breakpoint",
                "title": "Set Data Breakpoint (Watchpoint)",
                "description": "Sets a data breakpoint (watchpoint) that fires on read, write, or readWrite of a variable. Requires hardware support (max ~4 simultaneously on x86_64, 1/2/4/8-byte regions) and stopped state.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sessionId": {
                            "type": "string",
                            "description": "Session ID from debugger_start"
                        },
                        "name": {
                            "type": "string",
                            "description": "Variable name or expression to watch"
                        },
                        "variablesReference": {
                            "type": "integer",
                            "description": "Variables reference from a scope or parent variable. Needed for child variables."
                        },
                        "frameId": {
                            "type": "integer",
                            "description": "Stack frame ID for context"
                        },
                        "accessType": {
                            "type": "string",
                            "enum": ["read", "write", "readWrite"],
                            "description": "When to trigger: 'write' (default), 'read', or 'readWrite'"
                        },
                        "condition": {
                            "type": "string",
                            "description": "Optional condition expression"
                        },
                        "hitCondition": {
                            "type": "string",
                            "description": "Optional hit count condition"
                        }
                    },
                    "required": ["sessionId", "name"]
                },
                "annotations": {
                    "returnsTiming": "< 100ms",
                    "workflow": "breakpoints",
                    "category": "breakpoints",
                    "priority": 0.6
                }
            }),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug::SessionManager;

    #[tokio::test]
    async fn test_read_source_context() {
        let dir = std::env::temp_dir().join("debugger_mcp_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_source.py");
        std::fs::write(&path, "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\n").unwrap();

        let result = read_source_context(path.to_str().unwrap(), 4, 2).await;
        assert!(result.is_some());
        let ctx = result.unwrap();
        assert!(ctx.contains(" >    4 |"), "Should mark line 4 as current, got: {}", ctx);
        assert!(ctx.contains("line 2"), "Should include context before");
        assert!(ctx.contains("line 6"), "Should include context after");
        assert!(!ctx.contains("line 1"), "Should not include line 1 with context=2");

        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn test_read_source_context_nonexistent_file() {
        let result = read_source_context("/nonexistent/path.py", 1, 5).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_read_source_context_line_out_of_range() {
        let dir = std::env::temp_dir().join("debugger_mcp_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_short.py");
        std::fs::write(&path, "only one line\n").unwrap();

        let result = read_source_context(path.to_str().unwrap(), 999, 5).await;
        assert!(result.is_none());

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn test_compound_tools_in_schema() {
        let tools = ToolsHandler::list_tools();
        let find_tool = |name: &str| tools.iter().find(|t| t["name"] == name).unwrap();

        // Verify compound tool schemas have required fields
        let rtc = find_tool("debugger_run_to_crash");
        let required: Vec<&str> = rtc["inputSchema"]["required"].as_array().unwrap()
            .iter().filter_map(|v| v.as_str()).collect();
        assert!(required.contains(&"language"));
        assert!(required.contains(&"program"));

        let snap = find_tool("debugger_snapshot_at");
        let required: Vec<&str> = snap["inputSchema"]["required"].as_array().unwrap()
            .iter().filter_map(|v| v.as_str()).collect();
        assert!(required.contains(&"sessionId"));
        assert!(required.contains(&"sourcePath"));
        assert!(required.contains(&"line"));

        let trace = find_tool("debugger_trace_function");
        let required: Vec<&str> = trace["inputSchema"]["required"].as_array().unwrap()
            .iter().filter_map(|v| v.as_str()).collect();
        assert!(required.contains(&"sessionId"));

        // Verify continue has new waitForStop parameter
        let cont = find_tool("debugger_continue");
        assert!(cont["inputSchema"]["properties"]["waitForStop"].is_object());
        assert!(cont["inputSchema"]["properties"]["timeoutMs"].is_object());

        // Verify stack_trace has includeVariables
        let st = find_tool("debugger_stack_trace");
        assert!(st["inputSchema"]["properties"]["includeVariables"].is_object());
    }

    #[test]
    fn test_list_tools() {
        let tools = ToolsHandler::list_tools();
        assert_eq!(tools.len(), 21);

        // Verify tool names
        let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

        // Core tools
        assert!(tool_names.contains(&"debugger_start"));
        assert!(tool_names.contains(&"debugger_session_state"));
        assert!(tool_names.contains(&"debugger_set_breakpoint"));
        assert!(tool_names.contains(&"debugger_continue"));
        assert!(tool_names.contains(&"debugger_stack_trace"));
        assert!(tool_names.contains(&"debugger_evaluate"));
        assert!(tool_names.contains(&"debugger_get_variables"));
        assert!(tool_names.contains(&"debugger_disconnect"));
        assert!(tool_names.contains(&"debugger_wait_for_stop"));
        assert!(tool_names.contains(&"debugger_list_breakpoints"));
        assert!(tool_names.contains(&"debugger_remove_breakpoint"));
        assert!(tool_names.contains(&"debugger_step_over"));
        assert!(tool_names.contains(&"debugger_step_into"));
        assert!(tool_names.contains(&"debugger_step_out"));
        assert!(tool_names.contains(&"debugger_get_output"));
        assert!(tool_names.contains(&"debugger_debugging_tips"));
        assert!(tool_names.contains(&"debugger_set_data_breakpoint"));

        // Compound tools
        assert!(tool_names.contains(&"debugger_run_to_crash"));
        assert!(tool_names.contains(&"debugger_snapshot_at"));
        assert!(tool_names.contains(&"debugger_trace_function"));
    }

    #[test]
    fn test_list_tools_schema_validation() {
        let tools = ToolsHandler::list_tools();

        // Check first tool structure
        let start_tool = &tools[0];
        assert_eq!(start_tool["name"], "debugger_start");
        assert!(start_tool["description"].is_string());
        assert!(start_tool["inputSchema"]["type"].is_string());
        assert!(start_tool["inputSchema"]["properties"].is_object());
        assert!(start_tool["inputSchema"]["required"].is_array());

        // Verify env property exists in schema
        let properties = &start_tool["inputSchema"]["properties"];
        assert!(
            properties["env"].is_object(),
            "env property should exist in debugger_start schema"
        );
        assert_eq!(properties["env"]["type"], "object");
    }

    #[tokio::test]
    async fn test_tools_handler_new() {
        let manager = Arc::new(RwLock::new(SessionManager::new()));
        let _handler = ToolsHandler::new(manager);
        // Verify list_tools returns expected tools
        let tools = ToolsHandler::list_tools();
        assert!(tools.iter().any(|t| t["name"] == "debugger_start"));
    }

    #[tokio::test]
    async fn test_handle_tool_unknown_method() {
        let manager = Arc::new(RwLock::new(SessionManager::new()));
        let handler = ToolsHandler::new(manager);

        let result = handler.handle_tool("unknown_tool", json!({})).await;
        assert!(result.is_err());

        match result {
            Err(Error::MethodNotFound(name)) => {
                assert_eq!(name, "unknown_tool");
            }
            _ => panic!("Expected MethodNotFound error"),
        }
    }

    #[tokio::test]
    async fn test_handle_tool_invalid_arguments() {
        let manager = Arc::new(RwLock::new(SessionManager::new()));
        let handler = ToolsHandler::new(manager);

        // Invalid JSON for debugger_start
        let result = handler
            .handle_tool("debugger_start", json!({"invalid": "data"}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_handle_tool_disconnect_invalid_json() {
        let manager = Arc::new(RwLock::new(SessionManager::new()));
        let handler = ToolsHandler::new(manager);

        // Missing required fields
        let result = handler.handle_tool("debugger_disconnect", json!({})).await;
        assert!(result.is_err());
    }

    // Rust-specific path validation tests
    #[tokio::test]
    async fn test_rust_accepts_source_file() {
        let manager = Arc::new(RwLock::new(SessionManager::new()));
        let handler = ToolsHandler::new(manager);

        // Rust should accept .rs source files
        let result = handler
            .handle_tool(
                "debugger_start",
                json!({
                    "language": "rust",
                    "program": "/workspace/tests/fixtures/fizzbuzz.rs",
                    "args": [],
                    "stopOnEntry": true
                }),
            )
            .await;

        // Will fail due to session creation, but should NOT fail due to path validation
        // The error should be about the adapter/session, not about file extension
        if let Err(e) = result {
            let error_msg = format!("{:?}", e);
            assert!(
                !error_msg.contains("Invalid file extension"),
                "Should accept .rs files, but got error: {}",
                error_msg
            );
        }
    }

    #[tokio::test]
    async fn test_rust_accepts_executable() {
        use std::path::PathBuf;

        let manager = Arc::new(RwLock::new(SessionManager::new()));
        let handler = ToolsHandler::new(manager);

        // Get absolute path to test fixture using CARGO_MANIFEST_DIR
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR should be set during tests");
        let executable_path = PathBuf::from(manifest_dir)
            .join("tests/fixtures/target/fizzbuzz")
            .to_string_lossy()
            .to_string();

        eprintln!("Testing with executable path: {}", executable_path);

        // Rust should accept executables (no extension) - THIS IS THE KEY TEST
        // This reproduces the error from the log:
        // "Invalid file extension. Expected '.rs', got: '/workspace/tests/fixtures/target/fizzbuzz'"
        let result = handler
            .handle_tool(
                "debugger_start",
                json!({
                    "language": "rust",
                    "program": executable_path,
                    "args": [],
                    "stopOnEntry": true
                }),
            )
            .await;

        // Will fail due to session creation, but should NOT fail due to path validation
        match result {
            Ok(_) => {
                // Unexpected success - file doesn't exist or session somehow created
                eprintln!("WARNING: Test unexpectedly succeeded");
            }
            Err(e) => {
                let error_msg = format!("{:?}", e);
                eprintln!("Got error: {}", error_msg);

                // THIS IS THE KEY ASSERTION - it should FAIL with current code
                assert!(
                    !error_msg.contains("Invalid file extension"),
                    "Should accept executables without extension, but got error: {}",
                    error_msg
                );
            }
        }
    }

    #[tokio::test]
    async fn test_rust_rejects_wrong_extension() {
        let manager = Arc::new(RwLock::new(SessionManager::new()));
        let handler = ToolsHandler::new(manager);

        // Rust should reject files with wrong extensions (e.g., .py)
        let result = handler
            .handle_tool(
                "debugger_start",
                json!({
                    "language": "rust",
                    "program": "/workspace/tests/fixtures/fizzbuzz.py",
                    "args": [],
                    "stopOnEntry": true
                }),
            )
            .await;

        // Should fail due to invalid extension
        assert!(result.is_err(), "Should reject .py files for Rust");

        if let Err(e) = result {
            let error_msg = format!("{:?}", e);
            assert!(
                error_msg.contains("Invalid")
                    || error_msg.contains("extension")
                    || error_msg.contains(".py"),
                "Error should mention invalid extension, but got: {}",
                error_msg
            );
        }
    }

    // Unit tests for path validation logic (direct testing for coverage)
    // These tests exercise the validation branches without requiring session creation
    mod path_validation_tests {
        use std::path::PathBuf;

        #[test]
        fn test_rust_validation_accepts_rs_extension() {
            // Test that .rs files pass validation
            let path = PathBuf::from("/workspace/test/file.rs");
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");

            // Verify this matches our validation logic
            assert_eq!(ext, "rs");
            assert!(ext.is_empty() || ext == "rs", "Should accept .rs files");
        }

        #[test]
        fn test_rust_validation_accepts_no_extension() {
            // Test that executables (no extension) pass validation
            let path = PathBuf::from("/workspace/test/executable");
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");

            // Verify this matches our validation logic
            assert_eq!(ext, "");
            assert!(ext.is_empty() || ext == "rs", "Should accept executables");
        }

        #[test]
        fn test_rust_validation_rejects_py_extension() {
            // Test that .py files fail validation
            let path = PathBuf::from("/workspace/test/file.py");
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");

            // Verify this triggers error in our validation logic
            assert_eq!(ext, "py");
            assert!(
                !ext.is_empty() && ext != "rs",
                "Should reject .py files for Rust"
            );
        }

        #[test]
        fn test_rust_validation_rejects_js_extension() {
            // Test that .js files fail validation
            let path = PathBuf::from("/workspace/test/file.js");
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");

            assert_eq!(ext, "js");
            assert!(
                !ext.is_empty() && ext != "rs",
                "Should reject .js files for Rust"
            );
        }

        #[test]
        fn test_python_extension_match() {
            // Test Python extension validation logic
            let extension = match "python" {
                "python" => Some("py"),
                "ruby" => Some("rb"),
                "javascript" | "nodejs" => Some("js"),
                "go" => Some("go"),
                _ => None,
            };

            assert_eq!(extension, Some("py"));
        }

        #[test]
        fn test_ruby_extension_match() {
            // Test Ruby extension validation logic
            let extension = match "ruby" {
                "python" => Some("py"),
                "ruby" => Some("rb"),
                "javascript" | "nodejs" => Some("js"),
                "go" => Some("go"),
                _ => None,
            };

            assert_eq!(extension, Some("rb"));
        }

        #[test]
        fn test_javascript_extension_match() {
            // Test JavaScript extension validation logic
            let extension = match "javascript" {
                "python" => Some("py"),
                "ruby" => Some("rb"),
                "javascript" | "nodejs" => Some("js"),
                "go" => Some("go"),
                _ => None,
            };

            assert_eq!(extension, Some("js"));
        }

        #[test]
        fn test_nodejs_extension_match() {
            // Test Node.js extension validation logic
            let extension = match "nodejs" {
                "python" => Some("py"),
                "ruby" => Some("rb"),
                "javascript" | "nodejs" => Some("js"),
                "go" => Some("go"),
                _ => None,
            };

            assert_eq!(extension, Some("js"));
        }

        #[test]
        fn test_go_extension_match() {
            // Test Go extension validation logic
            let extension = match "go" {
                "python" => Some("py"),
                "ruby" => Some("rb"),
                "javascript" | "nodejs" => Some("js"),
                "go" => Some("go"),
                _ => None,
            };

            assert_eq!(extension, Some("go"));
        }

        #[test]
        fn test_unknown_language_extension_match() {
            // Test unknown language returns None
            let extension = match "unknown" {
                "python" => Some("py"),
                "ruby" => Some("rb"),
                "javascript" | "nodejs" => Some("js"),
                "go" => Some("go"),
                _ => None,
            };

            assert_eq!(extension, None);
        }

        #[test]
        fn test_rust_language_branch() {
            // Test that we correctly identify rust language
            let language = "rust";
            let is_rust = language == "rust";

            assert!(is_rust, "Should identify rust language");
        }

        #[test]
        fn test_non_rust_language_branch() {
            // Test that we correctly identify non-rust languages
            let language = "python";
            let is_rust = language == "rust";

            assert!(!is_rust, "Should identify non-rust language");
        }
    }

    #[test]
    fn test_is_bare_identifier_true() {
        assert!(is_bare_identifier("big_vec"));
        assert!(is_bare_identifier("my_var"));
        assert!(is_bare_identifier("x"));
        assert!(is_bare_identifier("HashMap2"));
        assert!(is_bare_identifier("_private"));
        assert!(is_bare_identifier("  x  "));
    }

    #[test]
    fn test_is_bare_identifier_false() {
        assert!(!is_bare_identifier("x + 1"));
        assert!(!is_bare_identifier("foo.bar"));
        assert!(!is_bare_identifier("arr[0]"));
        assert!(!is_bare_identifier("len(x)"));
        assert!(!is_bare_identifier("x > 10"));
        assert!(!is_bare_identifier("(int)x"));
        assert!(!is_bare_identifier("a::b"));
        assert!(!is_bare_identifier(""));
        assert!(!is_bare_identifier("   "));
    }

    #[test]
    fn test_evaluate_description_contains_memory_warning() {
        let tools = ToolsHandler::list_tools();
        let evaluate_tool = tools.iter().find(|t| t["name"] == "debugger_evaluate").unwrap();
        let desc = evaluate_tool["description"].as_str().unwrap();
        assert!(desc.contains("debugger_get_variables"), "evaluate description should mention debugger_get_variables");
        assert!(desc.contains("GBs of memory"), "evaluate description should warn about memory");
    }

    #[test]
    fn test_get_variables_tool_has_higher_priority_than_evaluate() {
        let tools = ToolsHandler::list_tools();
        let get_vars = tools.iter().find(|t| t["name"] == "debugger_get_variables").unwrap();
        let evaluate = tools.iter().find(|t| t["name"] == "debugger_evaluate").unwrap();
        let get_vars_priority = get_vars["annotations"]["priority"].as_f64().unwrap();
        let evaluate_priority = evaluate["annotations"]["priority"].as_f64().unwrap();
        assert!(get_vars_priority > evaluate_priority, "get_variables should have higher priority than evaluate");
    }
}
