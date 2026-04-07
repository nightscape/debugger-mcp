//! Process supervisor for debug adapters.
//!
//! Monitors the RSS of a debug adapter process (e.g. CodeLLDB) and kills it
//! if memory usage exceeds a threshold.  When killed, the session is moved to
//! `Failed` with a structured error message that helps the LLM retry with a
//! safer strategy.

use super::state::DebugState;
use super::state::SessionState;
use std::sync::Arc;
use sysinfo::System;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Default RSS limit in megabytes.  CodeLLDB's baseline is ~80–120 MB;
/// anything above this threshold is almost certainly unbounded variable
/// expansion inside LLDB's synthetic providers.
const DEFAULT_RSS_LIMIT_MB: u64 = 4096;

/// How often to poll the process (ms).
const POLL_INTERVAL_MS: u64 = 500;

/// Spawn a background task that monitors the given PID's RSS.
///
/// When the limit is exceeded the process is killed and `session_state` is set
/// to `Failed` with a diagnostic message.  The task exits when:
/// - the monitored process disappears (normal exit / already killed)
/// - the session state becomes `Terminated` or `Failed`
/// - the process is killed by the supervisor
pub fn spawn_supervisor(
    pid: u32,
    session_state: Arc<RwLock<SessionState>>,
    last_tool_context: Arc<RwLock<Option<String>>>,
    rss_limit_mb: Option<u64>,
) -> tokio::task::JoinHandle<()> {
    let limit = rss_limit_mb.unwrap_or(DEFAULT_RSS_LIMIT_MB);

    tokio::spawn(async move {
        let mut sys = System::new();
        let sysinfo_pid = sysinfo::Pid::from_u32(pid);

        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(POLL_INTERVAL_MS)).await;

            // Check if session is already done — no point monitoring a dead session.
            {
                let state = session_state.read().await;
                match &state.state {
                    DebugState::Terminated | DebugState::Failed { .. } => {
                        info!("🛡️  Supervisor: session ended, stopping monitor for pid {}", pid);
                        return;
                    }
                    _ => {}
                }
            }

            sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[sysinfo_pid]), true);
            let proc = match sys.process(sysinfo_pid) {
                Some(p) => p,
                None => {
                    info!("🛡️  Supervisor: process {} gone, stopping monitor", pid);
                    return;
                }
            };

            let rss_mb = proc.memory() / (1024 * 1024);
            if rss_mb <= limit {
                continue;
            }

            // Over the limit — build a diagnostic, kill the process, fail the session.
            warn!(
                "🛡️  Supervisor: KILLING debug adapter pid {} — RSS {}MB exceeds {}MB limit",
                pid, rss_mb, limit
            );

            let tool_ctx = last_tool_context.read().await.clone();
            let recommendation = build_recommendation(tool_ctx.as_deref());

            proc.kill();

            let error_msg = format!(
                "Debug adapter killed by memory supervisor: RSS {rss_mb}MB exceeded {limit}MB limit.\n\
                 \n\
                 {recommendation}\n\
                 \n\
                 Recovery: call debugger_start to create a new session, then follow the recommendations above."
            );

            let mut state = session_state.write().await;
            state.set_state(DebugState::Failed { error: error_msg });

            return;
        }
    })
}

/// Produce a human/LLM-readable recommendation based on the tool call that
/// was in-flight when the adapter blew up.
fn build_recommendation(tool_context: Option<&str>) -> String {
    let ctx = tool_context.unwrap_or("");

    if ctx.contains("evaluate") {
        let expression = extract_quoted(ctx, "expression");
        return format!(
            "CAUSE: Evaluating '{expression}' caused LLDB to expand a large data structure.\n\
             RECOMMENDATIONS:\n\
             - Do NOT evaluate container variables (Vec, HashMap, BTreeMap) directly.\n\
             - Instead evaluate specific elements: e.g. `my_vec[0]`, `my_vec.len()`\n\
             - For HashMap: `my_map.len()` instead of `my_map`\n\
             - For structs with large fields: evaluate individual fields like `obj.field`"
        );
    }

    if ctx.contains("variables") || ctx.contains("stack_trace") || ctx.contains("wait_for_stop") {
        return
            "CAUSE: Automatic variable expansion triggered LLDB to materialise a large collection.\n\
             RECOMMENDATIONS:\n\
             - Avoid stopping where large containers (Vec, HashMap) are in scope if possible.\n\
             - Use debugger_evaluate with specific field access instead of inspecting all locals.\n\
             - Set breakpoints inside functions that only have small local variables."
                .to_string();
    }

    "CAUSE: The debug adapter consumed excessive memory (likely expanding large data structures).\n\
     RECOMMENDATIONS:\n\
     - Do NOT evaluate containers (Vec, HashMap, BTreeMap, large structs) directly.\n\
     - Use element access: `my_vec[0]`, `my_map.len()`, `obj.field`\n\
     - Prefer breakpoints in functions with few/small local variables."
        .to_string()
}

/// Extract the value for `key="value"` or `"key":"value"` from a context string.
fn extract_quoted(ctx: &str, key: &str) -> String {
    for pattern in [
        format!("{}=\"", key),
        format!("\"{}\":\"", key),
        format!("\"{}\": \"", key),
    ] {
        if let Some(start) = ctx.find(&pattern) {
            let after = &ctx[start + pattern.len()..];
            let end = after.find('"').unwrap_or(after.len().min(100));
            return after[..end].to_string();
        }
    }
    "<unknown>".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_recommendation_for_evaluate() {
        let ctx = r#"debugger_evaluate expression="big_map""#;
        let rec = build_recommendation(Some(ctx));
        assert!(rec.contains("big_map"));
        assert!(rec.contains("Do NOT evaluate container"));
    }

    #[test]
    fn test_recommendation_for_variables() {
        let rec = build_recommendation(Some("build_stop_context variables"));
        assert!(rec.contains("Automatic variable expansion"));
    }

    #[test]
    fn test_recommendation_generic() {
        let rec = build_recommendation(None);
        assert!(rec.contains("Do NOT evaluate containers"));
    }

    #[test]
    fn test_extract_quoted() {
        assert_eq!(extract_quoted(r#"expression="foo""#, "expression"), "foo");
        assert_eq!(
            extract_quoted(r#""expression":"bar","other":1"#, "expression"),
            "bar"
        );
        assert_eq!(extract_quoted("nothing here", "expression"), "<unknown>");
    }
}
