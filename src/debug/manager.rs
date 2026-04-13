use super::session::DebugSession;
use crate::adapters::golang::GoAdapter;
use crate::adapters::logging::DebugAdapterLogger;
use crate::adapters::nodejs::NodeJsAdapter;
use crate::adapters::python::PythonAdapter;
use crate::adapters::ruby::RubyAdapter;
use crate::adapters::rust::RustAdapter;
use crate::dap::client::DapClient;
use crate::{Error, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info};

/// Session Manager - manages multiple debug sessions
pub struct SessionManager {
    sessions: Arc<RwLock<HashMap<String, Arc<DebugSession>>>>,
    /// RSS limit in MB for the process supervisor (None = default 512MB).
    supervisor_rss_limit_mb: Option<u64>,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            supervisor_rss_limit_mb: None,
        }
    }

    /// Set a custom RSS limit for the process supervisor (in MB).
    /// Mainly useful for testing.
    pub fn set_supervisor_rss_limit(&mut self, limit_mb: u64) {
        self.supervisor_rss_limit_mb = Some(limit_mb);
    }

    pub async fn create_session(
        &self,
        language: &str,
        program: String,
        args: Vec<String>,
        cwd: Option<String>,
        stop_on_entry: bool,
        env: std::collections::HashMap<String, String>,
        profile: Option<String>,
    ) -> Result<String> {
        // Type alias for STDIO adapter tuple: (command, args, adapter_id, launch_args, adapter_for_logging)
        type StdioAdapterTuple<'a> = (
            String,
            Vec<String>,
            &'a str,
            serde_json::Value,
            Box<dyn DebugAdapterLogger + 'a>,
        );

        let (command, adapter_args, adapter_id, launch_args, adapter): StdioAdapterTuple =
            match language {
                "python" => {
                    // Create adapter instance for logging
                    let adapter = PythonAdapter;

                    // Log adapter selection
                    adapter.log_selection();

                    let cmd = PythonAdapter::command();
                    let adapter_args = PythonAdapter::args();
                    let adapter_id = PythonAdapter::adapter_id();
                    let launch_args = PythonAdapter::launch_args_with_options(
                        &program,
                        &args,
                        cwd.as_deref(),
                        stop_on_entry,
                        &env,
                    );

                    // Log transport initialization
                    adapter.log_transport_init();

                    (
                        cmd,
                        adapter_args,
                        adapter_id,
                        launch_args,
                        Box::new(adapter),
                    )
                }
                "ruby" => {
                    // Create adapter instance for logging
                    let adapter = RubyAdapter;

                    // Log adapter selection
                    adapter.log_selection();

                    // Log transport initialization
                    adapter.log_transport_init();

                    // Ruby uses socket-based communication, not stdio
                    // Spawn rdbg and connect to socket
                    adapter.log_spawn_attempt();
                    let ruby_session = RubyAdapter::spawn(&program, &args, stop_on_entry)
                        .await
                        .inspect_err(|e| {
                            adapter.log_spawn_error(e);
                        })?;

                    // Log successful connection with Ruby-specific details
                    ruby_session.log_connection_success_with_port();

                    let adapter_id = RubyAdapter::adapter_id();
                    let launch_args = RubyAdapter::launch_args_with_options(
                        &program,
                        &args,
                        cwd.as_deref(),
                        stop_on_entry,
                        &env,
                    );

                    // Create DAP client from socket
                    let client = DapClient::from_socket(ruby_session.socket)
                        .await
                        .inspect_err(|e| {
                            adapter.log_connection_error(e);
                        })?;

                    // Create session
                    let session =
                        DebugSession::new(language.to_string(), program.clone(), client).await?;
                    let session_id = session.id.clone();

                    // Store session immediately
                    let session_arc = Arc::new(session);
                    {
                        let mut sessions = self.sessions.write().await;
                        sessions.insert(session_id.clone(), session_arc.clone());
                    }

                    // Log workaround application (Ruby requires entry breakpoint workaround)
                    adapter.log_workaround_applied();

                    // Initialize and launch in the background
                    tokio::spawn(
                        session_arc
                            .initialize_and_launch_async(adapter_id.to_string(), launch_args),
                    );

                    return Ok(session_id);
                }
                "nodejs" => {
                    // Create adapter instance for logging
                    let adapter = NodeJsAdapter;

                    // Log adapter selection
                    adapter.log_selection();

                    // Log transport initialization
                    adapter.log_transport_init();

                    // Node.js uses socket-based communication with vscode-js-debug DAP server
                    // Spawn vscode-js-debug and connect to socket
                    adapter.log_spawn_attempt();
                    let nodejs_session =
                        NodeJsAdapter::spawn_dap_server().await.inspect_err(|e| {
                            adapter.log_spawn_error(e);
                        })?;

                    // Log successful connection with Node.js-specific details
                    nodejs_session.log_connection_success_with_details();

                    let adapter_id = NodeJsAdapter::adapter_id();
                    let launch_args = NodeJsAdapter::launch_config(
                        &program,
                        &args,
                        cwd.as_deref(),
                        stop_on_entry,
                        &env,
                    );

                    // Create DAP client from socket (parent session)
                    let parent_client = DapClient::from_socket(nodejs_session.socket)
                        .await
                        .inspect_err(|e| {
                            adapter.log_connection_error(e);
                        })?;

                    info!("🔄 [NODEJS] Creating multi-session manager for parent session");

                    // Create session with multi-session mode
                    use super::multi_session::MultiSessionManager;
                    use super::session::SessionMode;

                    let session_id = uuid::Uuid::new_v4().to_string();
                    let multi_session_manager = MultiSessionManager::new(session_id.clone());

                    let session_mode = SessionMode::MultiSession {
                        parent_client: Arc::new(RwLock::new(parent_client)),
                        multi_session_manager: multi_session_manager.clone(),
                        vscode_js_debug_port: nodejs_session.port,
                    };

                    let session = DebugSession::new_with_mode(
                        language.to_string(),
                        program.clone(),
                        session_mode,
                    )
                    .await?;

                    // Store session immediately
                    let session_arc = Arc::new(session);
                    {
                        let mut sessions = self.sessions.write().await;
                        sessions.insert(session_id.clone(), session_arc.clone());
                    }

                    // Register child session spawn callback on parent client
                    info!("🔄 [NODEJS] Registering child session spawn callback");
                    let session_clone = session_arc.clone();
                    if let SessionMode::MultiSession { parent_client, .. } =
                        &session_arc.session_mode
                    {
                        let parent = parent_client.read().await;
                        parent
                        .on_child_session_spawn(move |target_id| {
                            let session = session_clone.clone();
                            Box::pin(async move {
                                info!("🎯 [NODEJS] Child session spawn callback invoked for target_id: {}", target_id);
                                if let Err(e) = session.spawn_child_session(target_id).await {
                                    error!("❌ [NODEJS] Failed to spawn child session: {}", e);
                                } else {
                                    info!("✅ [NODEJS] Child session spawned successfully");
                                }
                            })
                        })
                        .await;
                    }

                    // Log workaround application (Node.js uses multi-session for stopOnEntry)
                    adapter.log_workaround_applied();

                    // Initialize and launch in the background
                    // This will trigger the parent session, which will send startDebugging reverse request
                    tokio::spawn(
                        session_arc
                            .initialize_and_launch_async(adapter_id.to_string(), launch_args),
                    );

                    return Ok(session_id);
                }
                "go" => {
                    // Create adapter instance for logging
                    let adapter = GoAdapter;

                    // Log adapter selection
                    adapter.log_selection();

                    // Log transport initialization
                    adapter.log_transport_init();

                    // Go uses socket-based communication with Delve DAP server
                    // Spawn dlv dap and connect to socket
                    adapter.log_spawn_attempt();
                    let go_session = GoAdapter::spawn(&program, &args, stop_on_entry)
                        .await
                        .inspect_err(|e| {
                            adapter.log_spawn_error(e);
                        })?;

                    // Log successful connection with Go-specific details
                    go_session.log_connection_success_with_port();

                    let adapter_id = GoAdapter::adapter_id();
                    let launch_args = GoAdapter::launch_args_with_options(
                        &program,
                        &args,
                        cwd.as_deref(),
                        stop_on_entry,
                        &env,
                    );

                    // Create DAP client from socket
                    let client = DapClient::from_socket(go_session.socket)
                        .await
                        .inspect_err(|e| {
                            adapter.log_connection_error(e);
                        })?;

                    // Create session
                    let session =
                        DebugSession::new(language.to_string(), program.clone(), client).await?;
                    let session_id = session.id.clone();

                    // Store session immediately
                    let session_arc = Arc::new(session);
                    {
                        let mut sessions = self.sessions.write().await;
                        sessions.insert(session_id.clone(), session_arc.clone());
                    }

                    // Log workaround application (if any Go-specific workarounds needed)
                    adapter.log_workaround_applied();

                    // Initialize and launch in the background
                    tokio::spawn(
                        session_arc
                            .initialize_and_launch_async(adapter_id.to_string(), launch_args),
                    );

                    return Ok(session_id);
                }
                "rust" => {
                    // Create adapter instance for logging
                    let adapter = RustAdapter;

                    // Log adapter selection
                    adapter.log_selection();

                    // Detect project type and decide compilation strategy
                    // Accept Cargo.toml directly or .rs files inside a Cargo project
                    let use_cargo_integration = if program.ends_with("Cargo.toml") {
                        true
                    } else if program.ends_with(".rs") {
                        matches!(
                            RustAdapter::detect_project_type(&program),
                            Ok(crate::adapters::rust::RustProjectType::CargoProject { .. })
                        )
                    } else {
                        false
                    };

                    let (binary_path, launch_args) = if use_cargo_integration {
                        // Cargo project: let CodeLLDB handle compilation via its cargo integration
                        let cargo_root = if program.ends_with("Cargo.toml") {
                            // Cargo.toml passed directly — use its parent directory
                            std::path::Path::new(&program)
                                .parent()
                                .ok_or_else(|| Error::Process("Cargo.toml has no parent directory".to_string()))?
                                .to_str()
                                .ok_or_else(|| Error::Process("Non-UTF8 Cargo root path".to_string()))?
                                .to_string()
                        } else {
                            // .rs file inside a Cargo project — detect the root
                            match RustAdapter::detect_project_type(&program)? {
                                crate::adapters::rust::RustProjectType::CargoProject { root, .. } => {
                                    root.to_str()
                                        .ok_or_else(|| Error::Process("Non-UTF8 Cargo root path".to_string()))?
                                        .to_string()
                                }
                                _ => unreachable!(),
                            }
                        };
                        info!("📦 [RUST] Using CodeLLDB Cargo integration for: {}", cargo_root);

                        let launch = RustAdapter::cargo_launch_args(
                            &cargo_root,
                            &args,
                            stop_on_entry,
                            &env,
                            profile.as_deref(),
                        );
                        // CodeLLDB resolves the binary path from cargo output
                        (program.clone(), launch)
                    } else if program.ends_with(".rs") {
                        // Single .rs file: compile with rustc ourselves
                        info!("🔨 [RUST] Compiling single Rust file before debugging");
                        RustAdapter::log_compilation_start(&program, false);
                        let binary = RustAdapter::compile(&program, false)
                            .await
                            .inspect_err(|e| {
                                RustAdapter::log_compilation_error(e);
                            })?;
                        RustAdapter::log_compilation_success(&binary);

                        let launch = RustAdapter::launch_args(
                            &binary,
                            &args,
                            cwd.as_deref(),
                            stop_on_entry,
                            &env,
                        );
                        (binary, launch)
                    } else {
                        // Pre-compiled binary
                        info!("🎯 [RUST] Using pre-compiled binary: {}", program);
                        let launch = RustAdapter::launch_args(
                            &program,
                            &args,
                            cwd.as_deref(),
                            stop_on_entry,
                            &env,
                        );
                        (program.clone(), launch)
                    };

                    // Log transport initialization
                    adapter.log_transport_init();

                    // Spawn CodeLLDB in TCP mode
                    adapter.log_spawn_attempt();
                    let rust_session = RustAdapter::spawn(&binary_path, &args, stop_on_entry)
                        .await
                        .inspect_err(|e| {
                            adapter.log_spawn_error(e);
                        })?;

                    // Log successful connection with Rust-specific details
                    rust_session.log_connection_success_with_port();

                    let adapter_id = RustAdapter::adapter_id();

                    // Capture adapter PID before consuming the socket
                    let adapter_pid = rust_session.process.id();

                    // Create DAP client from socket (like Ruby/Go)
                    let client = DapClient::from_socket(rust_session.socket)
                        .await
                        .inspect_err(|e| {
                            adapter.log_connection_error(e);
                        })?;

                    // Create session with adapter PID for supervisor monitoring
                    let mut session =
                        DebugSession::new(language.to_string(), program.clone(), client).await?;
                    session.adapter_pid = adapter_pid;
                    let session_id = session.id.clone();

                    // Store session immediately
                    let session_arc = Arc::new(session);
                    {
                        let mut sessions = self.sessions.write().await;
                        sessions.insert(session_id.clone(), session_arc.clone());
                    }

                    // Spawn memory supervisor for the adapter process
                    if let Some(pid) = adapter_pid {
                        super::supervisor::spawn_supervisor(
                            pid,
                            session_arc.state.clone(),
                            session_arc.last_tool_context.clone(),
                            self.supervisor_rss_limit_mb,
                        );
                    }

                    // Log workaround application (Rust doesn't require workarounds)
                    adapter.log_workaround_applied();

                    // Initialize and launch in the background
                    tokio::spawn(
                        session_arc
                            .initialize_and_launch_async(adapter_id.to_string(), launch_args),
                    );

                    return Ok(session_id);
                }
                _ => return Err(Error::AdapterNotFound(language.to_string())),
            };

        // Spawn DAP client (Python path - uses STDIO transport)
        // Adapter instance is passed from match arm above for language-specific logging
        adapter.log_spawn_attempt();
        let client = DapClient::spawn(&command, &adapter_args)
            .await
            .inspect_err(|e| {
                adapter.log_spawn_error(e);
            })?;

        // Log successful connection
        adapter.log_connection_success();

        // Create session
        let session = DebugSession::new(language.to_string(), program, client).await?;
        let session_id = session.id.clone();

        // Store session immediately
        let session_arc = Arc::new(session);
        {
            let mut sessions = self.sessions.write().await;
            sessions.insert(session_id.clone(), session_arc.clone());
        }

        // Log workaround if needed (Python doesn't require workarounds)
        adapter.log_workaround_applied();

        // Initialize and launch in the background
        tokio::spawn(session_arc.initialize_and_launch_async(adapter_id.to_string(), launch_args));

        Ok(session_id)
    }

    pub async fn get_session(&self, session_id: &str) -> Result<Arc<DebugSession>> {
        let sessions = self.sessions.read().await;
        sessions
            .get(session_id)
            .cloned()
            .ok_or_else(|| Error::SessionNotFound(session_id.to_string()))
    }

    pub async fn get_session_state(
        &self,
        session_id: &str,
    ) -> Result<crate::debug::state::DebugState> {
        let session = self.get_session(session_id).await?;
        Ok(session.get_state().await)
    }

    pub async fn list_sessions(&self) -> Vec<String> {
        let sessions = self.sessions.read().await;
        sessions.keys().cloned().collect()
    }

    pub async fn remove_session(&self, session_id: &str) -> Result<()> {
        // Disconnect the session first
        if let Ok(session) = self.get_session(session_id).await {
            let _ = session.disconnect().await;
        }

        let mut sessions = self.sessions.write().await;
        sessions
            .remove(session_id)
            .ok_or_else(|| Error::SessionNotFound(session_id.to_string()))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_session_manager_new() {
        let manager = SessionManager::new();
        let sessions = manager.list_sessions().await;
        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn test_list_sessions_empty() {
        let manager = SessionManager::new();
        let sessions = manager.list_sessions().await;
        assert_eq!(sessions.len(), 0);
    }

    #[tokio::test]
    async fn test_get_session_not_found() {
        let manager = SessionManager::new();
        let result = manager.get_session("nonexistent").await;
        assert!(result.is_err());

        match result {
            Err(Error::SessionNotFound(id)) => {
                assert_eq!(id, "nonexistent");
            }
            _ => panic!("Expected SessionNotFound error"),
        }
    }

    #[tokio::test]
    async fn test_remove_session_not_found() {
        let manager = SessionManager::new();
        let result = manager.remove_session("nonexistent").await;
        assert!(result.is_err());

        match result {
            Err(Error::SessionNotFound(id)) => {
                assert_eq!(id, "nonexistent");
            }
            _ => panic!("Expected SessionNotFound error"),
        }
    }

    #[tokio::test]
    async fn test_create_session_unknown_language() {
        let manager = SessionManager::new();
        // Use a truly unsupported language (ruby is now supported!)
        let result = manager
            .create_session(
                "javascript",
                "test.js".to_string(),
                vec![],
                None,
                false,
                std::collections::HashMap::new(),
                None,
            )
            .await;

        assert!(result.is_err());
        match result {
            Err(Error::AdapterNotFound(lang)) => {
                assert_eq!(lang, "javascript");
            }
            _ => panic!("Expected AdapterNotFound error"),
        }
    }
}
