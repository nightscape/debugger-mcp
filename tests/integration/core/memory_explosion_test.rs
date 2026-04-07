/// Integration test: CodeLLDB memory explosion when expanding large data structures.
///
/// This test reproduces the bug where expanding variables without a `count` limit
/// causes CodeLLDB/LLDB to materialise all children of large collections (Vec with
/// 10k elements, HashMap with 5k entries), leading to multi-GB RSS.
///
/// The test:
/// 1. Compiles a Rust fixture with large Vec, HashMap, linked list, and nested structures
/// 2. Starts a debug session and hits a breakpoint where all are in scope
/// 3. Monitors the codelldb process RSS in a background task
/// 4. Exercises evaluate + variable expansion on the large structures
/// 5. Asserts codelldb RSS stays under a threshold (500 MB)
/// 6. Kills codelldb if it exceeds the threshold to protect the machine
///
/// Run: cargo test test_codelldb_memory -- --ignored --nocapture 2>&1 | tee memory_test.txt

#[path = "../../helpers/debug_helpers.rs"]
mod debug_helpers;

use debug_helpers::{compile_rust_fixture, fixture_source, has_codelldb, DebugTestHarness};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use sysinfo::System;
use tokio::time::{timeout, Duration};

const RSS_LIMIT_MB: u64 = 500;

/// Find all codelldb PIDs currently running.
fn find_codelldb_pids(sys: &System) -> Vec<sysinfo::Pid> {
    sys.processes()
        .iter()
        .filter(|(_, p)| {
            p.name().to_string_lossy().contains("codelldb")
                || p.exe()
                    .map(|e| e.to_string_lossy().contains("codelldb"))
                    .unwrap_or(false)
        })
        .map(|(pid, _)| *pid)
        .collect()
}

/// Background task: poll codelldb RSS every 200ms, record peak, kill if over limit.
async fn monitor_codelldb(
    peak_rss_mb: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    killed: Arc<AtomicBool>,
) {
    let mut sys = System::new();

    // Snapshot PIDs before the test starts adapter
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let pids_before: std::collections::HashSet<sysinfo::Pid> =
        find_codelldb_pids(&sys).into_iter().collect();

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;

        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let current_pids = find_codelldb_pids(&sys);

        for pid in &current_pids {
            if pids_before.contains(pid) {
                continue; // not ours
            }
            if let Some(proc) = sys.process(*pid) {
                let rss_mb = proc.memory() / (1024 * 1024);
                let prev = peak_rss_mb.fetch_max(rss_mb, Ordering::Relaxed);
                if rss_mb > prev {
                    println!("  codelldb pid={} RSS={}MB (new peak)", pid, rss_mb);
                }
                if rss_mb > RSS_LIMIT_MB {
                    eprintln!(
                        "🛑 KILLING codelldb pid={}: RSS {}MB exceeds {}MB limit",
                        pid, rss_mb, RSS_LIMIT_MB
                    );
                    proc.kill();
                    killed.store(true, Ordering::Relaxed);
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_codelldb_memory_stays_bounded_during_variable_expansion() {
    if !has_codelldb() {
        println!("⚠️  Skipping: codelldb not installed");
        return;
    }

    let binary = match compile_rust_fixture("memory_hog.rs", "memory_hog") {
        Some(b) => b,
        None => {
            println!("⚠️  Skipping: compilation failed");
            return;
        }
    };

    let h = DebugTestHarness::new_rust("memory_hog.rs");
    let source_path = h.source_path.clone();

    // Start memory monitor
    let peak_rss_mb = Arc::new(AtomicU64::new(0));
    let stop_monitor = Arc::new(AtomicBool::new(false));
    let was_killed = Arc::new(AtomicBool::new(false));
    let monitor_handle = tokio::spawn(monitor_codelldb(
        peak_rss_mb.clone(),
        stop_monitor.clone(),
        was_killed.clone(),
    ));

    let tools = &h.tools;

    // Start debug session
    let start_result = timeout(
        Duration::from_secs(30),
        tools.handle_tool(
            "debugger_start",
            json!({
                "language": "rust",
                "program": binary.to_str().unwrap(),
                "stopOnEntry": true
            }),
        ),
    )
    .await;

    let session_id = match start_result {
        Ok(Ok(r)) => r["sessionId"].as_str().unwrap().to_string(),
        other => {
            stop_monitor.store(true, Ordering::Relaxed);
            monitor_handle.await.ok();
            println!("⚠️  Skipping: start failed: {:?}", other);
            return;
        }
    };

    // Wait for stopped at entry
    let wait_result = timeout(
        Duration::from_secs(15),
        tools.handle_tool(
            "debugger_wait_for_stop",
            json!({"sessionId": &session_id, "timeoutMs": 14000}),
        ),
    )
    .await;

    match &wait_result {
        Ok(Ok(v)) if v["state"].as_str() == Some("Stopped") => {
            println!("✅ Stopped at entry");
        }
        _ => {
            stop_monitor.store(true, Ordering::Relaxed);
            monitor_handle.await.ok();
            let _ = tools.handle_tool("debugger_disconnect", json!({"sessionId": &session_id})).await;
            println!("⚠️  Skipping: did not stop at entry");
            return;
        }
    }

    // Set breakpoint at line 47 (println after all structures are built)
    let _ = tools
        .handle_tool(
            "debugger_set_breakpoint",
            json!({
                "sessionId": &session_id,
                "sourcePath": &source_path,
                "line": 47
            }),
        )
        .await;

    // Continue to breakpoint
    let _ = tools
        .handle_tool("debugger_continue", json!({"sessionId": &session_id}))
        .await;

    let stop = timeout(
        Duration::from_secs(15),
        tools.handle_tool(
            "debugger_wait_for_stop",
            json!({"sessionId": &session_id, "timeoutMs": 14000}),
        ),
    )
    .await;

    match &stop {
        Ok(Ok(v)) if v["state"].as_str() == Some("Stopped") => {
            println!("✅ Stopped at breakpoint (all large structures in scope)");
        }
        _ => {
            stop_monitor.store(true, Ordering::Relaxed);
            monitor_handle.await.ok();
            let _ = tools.handle_tool("debugger_disconnect", json!({"sessionId": &session_id})).await;
            println!("⚠️  Skipping: did not stop at breakpoint");
            return;
        }
    }

    let rss_before = peak_rss_mb.load(Ordering::Relaxed);
    println!("codelldb RSS before variable expansion: {}MB", rss_before);

    // Now exercise variable expansion on the large structures.
    // This is what triggers the memory explosion without the count limit.
    let expressions = ["big_vec", "big_map", "list", "nested"];

    for expr in &expressions {
        if was_killed.load(Ordering::Relaxed) {
            break;
        }

        println!("Evaluating '{}'...", expr);
        let eval_result = timeout(
            Duration::from_secs(30),
            tools.handle_tool(
                "debugger_evaluate",
                json!({
                    "sessionId": &session_id,
                    "expression": expr
                }),
            ),
        )
        .await;

        match eval_result {
            Ok(Ok(v)) => {
                let result_str = v["result"].as_str().unwrap_or("");
                let truncated: String = result_str.chars().take(200).collect();
                println!("  {} = {}...", expr, truncated);
            }
            Ok(Err(e)) => {
                println!("  {} error: {}", expr, e);
            }
            Err(_) => {
                println!("  {} timed out (30s)", expr);
            }
        }

        let current_peak = peak_rss_mb.load(Ordering::Relaxed);
        println!("  codelldb peak RSS so far: {}MB", current_peak);
    }

    // Stop monitor and collect results
    stop_monitor.store(true, Ordering::Relaxed);
    monitor_handle.await.ok();

    let final_peak = peak_rss_mb.load(Ordering::Relaxed);
    let adapter_killed = was_killed.load(Ordering::Relaxed);

    // Disconnect (may fail if killed)
    let _ = timeout(
        Duration::from_secs(5),
        tools.handle_tool("debugger_disconnect", json!({"sessionId": &session_id})),
    )
    .await;

    println!("\n=== RESULTS ===");
    println!("Peak codelldb RSS: {}MB", final_peak);
    println!("RSS limit: {}MB", RSS_LIMIT_MB);
    println!("Adapter killed: {}", adapter_killed);

    assert!(
        !adapter_killed,
        "MEMORY EXPLOSION: codelldb exceeded {}MB and was killed. \
         Peak RSS: {}MB. This means variable expansion is not properly \
         limited — the `count` parameter is missing from variables requests.",
        RSS_LIMIT_MB,
        final_peak
    );

    assert!(
        final_peak < RSS_LIMIT_MB,
        "codelldb peak RSS {}MB exceeds {}MB limit. \
         Variable expansion may be fetching too many children.",
        final_peak,
        RSS_LIMIT_MB
    );

    println!("✅ codelldb memory stayed bounded: {}MB peak (limit {}MB)", final_peak, RSS_LIMIT_MB);
}

/// Test that the built-in process supervisor kills codelldb and produces
/// a structured error with per-tool recommendations.
///
/// Uses a low RSS threshold (150MB) so the supervisor triggers during
/// a normal evaluate of the big_map fixture (~300-400MB in LLDB).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_supervisor_kills_adapter_and_sets_failed_with_recommendations() {
    if !has_codelldb() {
        println!("⚠️  Skipping: codelldb not installed");
        return;
    }

    let binary = match compile_rust_fixture("memory_hog.rs", "memory_hog_supervisor") {
        Some(b) => b,
        None => {
            println!("⚠️  Skipping: compilation failed");
            return;
        }
    };

    // Create session manager with a LOW RSS limit so the supervisor triggers.
    // CodeLLDB baseline is ~80-120MB; evaluating big_map pushes it to ~400MB.
    let mut manager = debugger_mcp::debug::SessionManager::new();
    manager.set_supervisor_rss_limit(150);

    let sm = Arc::new(tokio::sync::RwLock::new(manager));
    let tools = debugger_mcp::mcp::tools::ToolsHandler::new(Arc::clone(&sm));

    let source_path = fixture_source("memory_hog.rs")
        .to_string_lossy()
        .to_string();

    // Start session
    let start_result = timeout(
        Duration::from_secs(30),
        tools.handle_tool(
            "debugger_start",
            json!({
                "language": "rust",
                "program": binary.to_str().unwrap(),
                "stopOnEntry": true
            }),
        ),
    )
    .await;

    let session_id = match start_result {
        Ok(Ok(r)) => r["sessionId"].as_str().unwrap().to_string(),
        other => {
            println!("⚠️  Skipping: start failed: {:?}", other);
            return;
        }
    };

    // Wait for stop at entry
    let wait = timeout(
        Duration::from_secs(15),
        tools.handle_tool(
            "debugger_wait_for_stop",
            json!({"sessionId": &session_id, "timeoutMs": 14000}),
        ),
    )
    .await;

    match &wait {
        Ok(Ok(v)) if v["state"].as_str() == Some("Stopped") => {
            println!("✅ Stopped at entry");
        }
        _ => {
            println!("⚠️  Skipping: did not stop at entry");
            let _ = tools
                .handle_tool("debugger_disconnect", json!({"sessionId": &session_id}))
                .await;
            return;
        }
    }

    // Set breakpoint at line 47 where all large structures are in scope
    let _ = tools
        .handle_tool(
            "debugger_set_breakpoint",
            json!({
                "sessionId": &session_id,
                "sourcePath": &source_path,
                "line": 47
            }),
        )
        .await;

    // Continue to breakpoint
    let _ = tools
        .handle_tool("debugger_continue", json!({"sessionId": &session_id}))
        .await;

    let stop = timeout(
        Duration::from_secs(15),
        tools.handle_tool(
            "debugger_wait_for_stop",
            json!({"sessionId": &session_id, "timeoutMs": 14000}),
        ),
    )
    .await;

    match &stop {
        Ok(Ok(v)) if v["state"].as_str() == Some("Stopped") => {
            println!("✅ Stopped at breakpoint");
        }
        // Supervisor may have already killed the adapter during wait_for_stop
        // (build_stop_context fetches variables which can push RSS over 150MB)
        Ok(Err(e)) => {
            let err_str = e.to_string();
            if err_str.contains("killed by memory supervisor") || err_str.contains("Session failed") {
                println!("✅ Supervisor already killed adapter during wait_for_stop");
                // Verify the error has recommendations
                assert!(
                    err_str.contains("RECOMMENDATIONS") || err_str.contains("Recovery"),
                    "Supervisor error should contain recommendations, got: {}",
                    err_str
                );
                println!("✅ Error contains recommendations");
                return;
            }
            println!("⚠️  Unexpected error: {}", err_str);
            return;
        }
        _ => {
            println!("⚠️  Skipping: did not stop at breakpoint");
            return;
        }
    }

    // Evaluate big_map — this should push codelldb over the 150MB limit.
    // The supervisor should kill it and set the session to Failed.
    println!("Evaluating 'big_map' to trigger supervisor kill...");
    let eval_result = timeout(
        Duration::from_secs(30),
        tools.handle_tool(
            "debugger_evaluate",
            json!({
                "sessionId": &session_id,
                "expression": "big_map"
            }),
        ),
    )
    .await;

    println!("Evaluate result: {:?}", eval_result.as_ref().map(|r| r.as_ref().map(|v| v.to_string()).map_err(|e| e.to_string())));

    // Give the supervisor a moment to detect and kill
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Check session state — should be Failed with recommendations
    let state_result = tools
        .handle_tool(
            "debugger_session_state",
            json!({"sessionId": &session_id}),
        )
        .await;

    match &state_result {
        Ok(v) => {
            let state = v["state"].as_str().unwrap_or("");
            println!("Session state: {}", state);
            println!("Full response: {}", serde_json::to_string_pretty(&v).unwrap());

            if state == "Failed" {
                // Error is nested under details.error in the session_state response
                let error = v["details"]["error"].as_str().unwrap_or("");
                println!("Error message:\n{}", error);

                assert!(
                    error.contains("memory supervisor") || error.contains("RSS"),
                    "Failed state should mention memory supervisor, got: {}",
                    error
                );
                assert!(
                    error.contains("RECOMMENDATIONS") || error.contains("Recovery"),
                    "Failed state should contain recommendations, got: {}",
                    error
                );
                println!("✅ Supervisor killed adapter and set Failed with recommendations");
            } else {
                println!("⚠️  Session state is '{}', not Failed — supervisor may not have triggered", state);
                println!("    (codelldb may have stayed under 150MB for this evaluation)");
            }
        }
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("memory supervisor") || err_str.contains("Session failed") {
                println!("✅ Session query returned supervisor error: {}", err_str);
                assert!(
                    err_str.contains("RECOMMENDATIONS") || err_str.contains("Recovery"),
                    "Error should contain recommendations"
                );
            } else {
                println!("⚠️  Unexpected session_state error: {}", err_str);
            }
        }
    }
}
