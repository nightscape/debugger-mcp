use debugger_mcp::debug::SessionManager;
use std::collections::HashMap;

#[tokio::test]
async fn test_rust_init_surfaces_errors() {
    // Initialize tracing so we see logs
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    let binary = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/target/debug_exercise"
    );

    if !std::path::Path::new(binary).exists() {
        println!("Skipping: binary not found at {}", binary);
        return;
    }

    let manager = SessionManager::new();

    println!("Creating Rust debug session for: {}", binary);
    let session_id = manager
        .create_session("rust", binary.to_string(), vec![], None, true, HashMap::new(), None)
        .await
        .expect("create_session should succeed");

    println!("Session created: {}", session_id);

    // Poll state for up to 15s — the 7s timeout should kick in
    for i in 0..30 {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        let session = manager.get_session(&session_id).await.expect("session should exist");
        let state = session.get_state().await;
        let elapsed = (i + 1) as f32 * 0.5;
        println!("[{:.1}s] State: {:?}", elapsed, state);

        let state_str = format!("{:?}", state);
        if state_str.contains("Stopped") || state_str.contains("Running") {
            println!("SUCCESS: Session reached {:?}", state);
            return;
        }
        if state_str.contains("Failed") {
            println!("FAILED: {}", state_str);
            panic!("Session failed: {}", state_str);
        }
        if state_str.contains("Terminated") {
            println!("TERMINATED early");
            panic!("Session terminated before reaching Stopped");
        }
    }

    panic!("Session stuck at Initializing for 15s — background task likely panicked");
}
