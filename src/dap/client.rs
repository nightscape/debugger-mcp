use super::transport::DapTransport;
use super::transport_trait::DapTransportTrait;
use super::types::*;
use crate::{Error, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex, Notify, RwLock};
use tracing::{debug, error, info, warn};

type ResponseSender = oneshot::Sender<Response>;
type EventNotifier = Arc<Notify>;
type EventCallback = Arc<dyn Fn(Event) + Send + Sync>;
type ChildSessionSpawnCallback = Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync,
>;

/// DAP Client with event-driven architecture
pub struct DapClient {
    transport: Arc<Mutex<Box<dyn DapTransportTrait>>>,
    seq_counter: Arc<AtomicI32>,
    pending_requests: Arc<RwLock<HashMap<i32, ResponseSender>>>,
    #[allow(dead_code)] // Reserved for future event handling
    event_tx: mpsc::UnboundedSender<Event>,
    // For backward compatibility with wait_for_event
    event_notifiers: Arc<RwLock<HashMap<String, EventNotifier>>>,
    // New: Event callbacks (can have multiple callbacks per event)
    event_callbacks: Arc<RwLock<HashMap<String, Vec<EventCallback>>>>,
    // Callback for child session spawning (Node.js multi-session)
    child_session_spawn_callback: Arc<RwLock<Option<ChildSessionSpawnCallback>>>,
    // Channel for sending write requests to avoid lock contention
    write_tx: mpsc::UnboundedSender<Message>,
    // Set to false when the adapter dies (write_message fails or stream closes).
    // Used to distinguish "MCP client bug closed the channel" from "the debug
    // adapter process exited" — the latter is what users actually hit when
    // LLDB or the debugged program crashes mid-evaluate.
    adapter_alive: Arc<AtomicBool>,
    _child: Option<Child>,
}

impl DapClient {
    /// Spawn a DAP adapter via stdio (for Python/debugpy)
    pub async fn spawn(command: &str, args: &[String]) -> Result<Self> {
        info!("Spawning DAP client: {} {:?}", command, args);

        let mut child = Command::new(command)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Error::Process(format!("Failed to spawn debug adapter: {}", e)))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Process("Failed to get stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Process("Failed to get stdout".to_string()))?;

        let transport: Box<dyn DapTransportTrait> = Box::new(DapTransport::new(stdin, stdout));
        Self::new_with_transport(transport, Some(child)).await
    }

    /// Create DAP client from TCP socket (for Ruby/rdbg)
    pub async fn from_socket(socket: tokio::net::TcpStream) -> Result<Self> {
        info!("Creating DAP client from socket: {:?}", socket.peer_addr());

        let transport: Box<dyn DapTransportTrait> = Box::new(DapTransport::new_socket(socket));
        Self::new_with_transport(transport, None).await
    }

    /// Create a new DAP client with a custom transport (for testing)
    pub async fn new_with_transport(
        transport: Box<dyn DapTransportTrait>,
        child: Option<Child>,
    ) -> Result<Self> {
        let transport = Arc::new(Mutex::new(transport));
        let seq_counter = Arc::new(AtomicI32::new(1));
        let pending_requests = Arc::new(RwLock::new(HashMap::new()));
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (write_tx, write_rx) = mpsc::unbounded_channel();

        let event_notifiers = Arc::new(RwLock::new(HashMap::new()));
        let event_callbacks = Arc::new(RwLock::new(HashMap::new()));
        let child_session_spawn_callback = Arc::new(RwLock::new(None));
        let adapter_alive = Arc::new(AtomicBool::new(true));

        let client = Self {
            transport: transport.clone(),
            seq_counter: seq_counter.clone(),
            pending_requests: pending_requests.clone(),
            event_tx,
            event_notifiers: event_notifiers.clone(),
            event_callbacks: event_callbacks.clone(),
            child_session_spawn_callback: child_session_spawn_callback.clone(),
            write_tx: write_tx.clone(),
            adapter_alive: adapter_alive.clone(),
            _child: child,
        };

        // Spawn message reader handler
        tokio::spawn(Self::message_reader(
            transport.clone(),
            pending_requests.clone(),
            event_notifiers.clone(),
            event_callbacks.clone(),
            child_session_spawn_callback.clone(),
            event_rx,
            adapter_alive.clone(),
        ));

        // Spawn message writer handler
        tokio::spawn(Self::message_writer(
            transport.clone(),
            write_rx,
            adapter_alive.clone(),
        ));

        Ok(client)
    }

    /// Message reader task - reads messages from transport and dispatches them
    async fn message_reader(
        transport: Arc<Mutex<Box<dyn DapTransportTrait>>>,
        pending_requests: Arc<RwLock<HashMap<i32, ResponseSender>>>,
        event_notifiers: Arc<RwLock<HashMap<String, EventNotifier>>>,
        event_callbacks: Arc<RwLock<HashMap<String, Vec<EventCallback>>>>,
        child_session_spawn_callback: Arc<RwLock<Option<ChildSessionSpawnCallback>>>,
        mut _event_rx: mpsc::UnboundedReceiver<Event>,
        adapter_alive: Arc<AtomicBool>,
    ) {
        loop {
            debug!("📖 message_reader: Attempting to acquire transport lock");

            // Try to read a message with the lock, but release it if no message is ready
            let msg_result = {
                let mut transport = transport.lock().await;
                debug!("📖 message_reader: Lock acquired, checking for message");

                // Use select with timeout to avoid holding lock indefinitely
                let read_future = transport.read_message();
                tokio::select! {
                    result = read_future => {
                        debug!("📖 message_reader: Message read attempt completed");
                        Some(result)
                    }
                    _ = tokio::time::sleep(tokio::time::Duration::from_millis(50)) => {
                        debug!("📖 message_reader: Read timeout, releasing lock");
                        None
                    }
                }
            };
            debug!("📖 message_reader: Lock released");

            // If we didn't get a message, continue loop (will retry)
            let msg = match msg_result {
                None => {
                    tokio::time::sleep(tokio::time::Duration::from_micros(100)).await;
                    continue;
                }
                Some(Ok(msg)) => msg,
                Some(Err(e)) => {
                    error!("📖 message_reader: Failed to read DAP message: {}", e);
                    adapter_alive.store(false, Ordering::SeqCst);
                    break;
                }
            };

            match msg {
                Message::Response(resp) => {
                    debug!("Received response for seq {}", resp.request_seq);
                    let mut pending = pending_requests.write().await;
                    if let Some(sender) = pending.remove(&resp.request_seq) {
                        if sender.send(resp).is_err() {
                            warn!("Failed to send response to waiting request");
                        }
                    } else {
                        warn!(
                            "Received response for unknown request: {}",
                            resp.request_seq
                        );
                    }
                }
                Message::Event(event) => {
                    info!(
                        "🎯 EVENT RECEIVED: '{}' with body: {:?}",
                        event.event, event.body
                    );

                    // 1. Notify anyone waiting for this specific event (legacy wait_for_event)
                    let notifiers = event_notifiers.read().await;
                    if let Some(notifier) = notifiers.get(&event.event) {
                        info!("  Notifying waiters for event '{}'", event.event);
                        notifier.notify_waiters();
                    }
                    drop(notifiers);

                    // 2. Invoke registered event callbacks
                    let callbacks = event_callbacks.read().await;
                    if let Some(handlers) = callbacks.get(&event.event) {
                        info!(
                            "  Found {} callback(s) for event '{}'",
                            handlers.len(),
                            event.event
                        );
                        for (idx, callback) in handlers.iter().enumerate() {
                            info!("  Invoking callback {} for event '{}'", idx, event.event);
                            // Invoke callback with cloned event
                            callback(event.clone());
                            info!("  Callback {} completed for event '{}'", idx, event.event);
                        }
                    } else {
                        info!("  No callbacks registered for event '{}'", event.event);
                    }
                }
                Message::Request(req) => {
                    info!(
                        "🔄 REVERSE REQUEST received: '{}' (seq {})",
                        req.command, req.seq
                    );
                    info!("   Arguments: {:?}", req.arguments);

                    // vscode-js-debug sends reverse requests like 'startDebugging' or 'attachedChildSession'
                    // We need to respond to these requests or the adapter will hang

                    // Handle startDebugging - extract __pendingTargetId and spawn child
                    if req.command == "startDebugging" {
                        info!(
                            "   🎯 startDebugging request detected - extracting __pendingTargetId"
                        );
                        if let Some(args) = &req.arguments {
                            if let Some(config) = args.get("configuration") {
                                if let Some(target_id) = config.get("__pendingTargetId") {
                                    if let Some(target_id_str) = target_id.as_str() {
                                        info!("   ✅ Found __pendingTargetId: {}", target_id_str);

                                        // Invoke callback to spawn child session
                                        let callback_guard =
                                            child_session_spawn_callback.read().await;
                                        if let Some(callback) = callback_guard.as_ref() {
                                            info!("   📞 Invoking child session spawn callback with target_id: {}", target_id_str);
                                            let fut = callback(target_id_str.to_string());
                                            drop(callback_guard);
                                            tokio::spawn(fut);
                                        } else {
                                            warn!(
                                                "   ⚠️  No child session spawn callback registered"
                                            );
                                        }
                                    } else {
                                        warn!(
                                            "   ⚠️  __pendingTargetId is not a string: {:?}",
                                            target_id
                                        );
                                    }
                                } else {
                                    warn!("   ⚠️  No __pendingTargetId in configuration");
                                }
                            } else {
                                warn!("   ⚠️  No configuration in startDebugging arguments");
                            }
                        }
                    }

                    // Send success response
                    let response = Response {
                        seq: 0, // Response seq (incremental, but not critical for reverse requests)
                        request_seq: req.seq,
                        success: true,
                        command: req.command.clone(),
                        message: None,
                        body: None,
                    };

                    info!(
                        "   Sending success response to reverse request '{}'",
                        req.command
                    );

                    // Send response back via transport (don't use write channel to avoid deadlock)
                    let transport_clone = transport.clone();
                    tokio::spawn(async move {
                        let mut transport = transport_clone.lock().await;
                        if let Err(e) = transport.write_message(&Message::Response(response)).await
                        {
                            error!("Failed to send reverse request response: {}", e);
                        }
                    });
                }
            }

            // Small sleep to let other tasks run (e.g., configurationDone sender)
            // This is necessary because read_message() blocks holding the lock
            tokio::time::sleep(tokio::time::Duration::from_micros(100)).await;
        }
    }

    /// Message writer task - writes messages to transport from a channel
    /// This avoids lock contention between reader and writers
    async fn message_writer(
        transport: Arc<Mutex<Box<dyn DapTransportTrait>>>,
        mut write_rx: mpsc::UnboundedReceiver<Message>,
        adapter_alive: Arc<AtomicBool>,
    ) {
        info!("📝 message_writer: Task started");
        while let Some(message) = write_rx.recv().await {
            let msg_type = match &message {
                Message::Request(req) => format!("Request({})", req.command),
                Message::Response(resp) => format!("Response(seq {})", resp.seq),
                Message::Event(evt) => format!("Event({})", evt.event),
            };
            info!("📝 message_writer: Received {} from channel", msg_type);
            info!("📝 message_writer: Attempting to acquire transport lock");
            let mut transport = transport.lock().await;
            info!("📝 message_writer: Lock acquired, writing message");
            if let Err(e) = transport.write_message(&message).await {
                error!("📝 message_writer: Failed to write DAP message: {}", e);
                adapter_alive.store(false, Ordering::SeqCst);
                break;
            }
            info!("📝 message_writer: Message written successfully, releasing lock");
            drop(transport);
            info!("📝 message_writer: Lock released");
        }
        info!("📝 message_writer: Task exiting");
    }

    /// Register a callback for a specific DAP event
    /// The callback will be invoked every time the event is received
    pub async fn on_event<F>(&self, event_name: &str, callback: F)
    where
        F: Fn(Event) + Send + Sync + 'static,
    {
        let mut callbacks = self.event_callbacks.write().await;
        callbacks
            .entry(event_name.to_string())
            .or_insert_with(Vec::new)
            .push(Arc::new(callback));
    }

    /// Remove all callbacks for a specific event
    pub async fn remove_event_handlers(&self, event_name: &str) {
        let mut callbacks = self.event_callbacks.write().await;
        callbacks.remove(event_name);
    }

    /// Register a callback for child session spawning (multi-session debugging)
    ///
    /// This callback will be invoked when a reverse request (like `startDebugging`)
    /// contains a child session port. The callback should spawn the child session
    /// and connect to it.
    ///
    /// # Arguments
    ///
    /// * `callback` - Async function that takes a port number and spawns a child session
    ///
    /// # Example
    ///
    /// ```ignore
    /// let session_arc = Arc::new(session);
    /// let session_clone = session_arc.clone();
    /// parent_client.on_child_session_spawn(move |target_id| {
    ///     let session = session_clone.clone();
    ///     Box::pin(async move {
    ///         if let Err(e) = session.spawn_child_session(target_id).await {
    ///             error!("Failed to spawn child session: {}", e);
    ///         }
    ///     })
    /// }).await;
    /// ```
    pub async fn on_child_session_spawn<F>(&self, callback: F)
    where
        F: Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            + Send
            + Sync
            + 'static,
    {
        let mut callback_guard = self.child_session_spawn_callback.write().await;
        *callback_guard = Some(Arc::new(callback));
        info!("✅ Child session spawn callback registered");
    }

    /// Wait for a specific DAP event with a timeout (legacy method)
    pub async fn wait_for_event(
        &self,
        event_name: &str,
        timeout: tokio::time::Duration,
    ) -> Result<()> {
        let notifier = {
            let mut notifiers = self.event_notifiers.write().await;
            // Create or get existing notifier for this event
            notifiers
                .entry(event_name.to_string())
                .or_insert_with(|| Arc::new(Notify::new()))
                .clone()
        };

        // Wait for notification or timeout
        tokio::select! {
            _ = notifier.notified() => {
                debug!("Received '{}' event", event_name);
                Ok(())
            }
            _ = tokio::time::sleep(timeout) => {
                Err(Error::Dap(format!("Timeout waiting for '{}' event after {:?}", event_name, timeout)))
            }
        }
    }

    /// Send a request without waiting for response (fire-and-forget)
    /// Useful when you'll handle the response via another mechanism
    pub async fn send_request_nowait(
        &self,
        command: &str,
        arguments: Option<Value>,
    ) -> Result<i32> {
        debug!("send_request_nowait: Starting for command '{}'", command);
        let seq = self.seq_counter.fetch_add(1, Ordering::SeqCst);

        let request = Request {
            seq,
            command: command.to_string(),
            arguments,
        };

        debug!(
            "send_request_nowait: Acquiring transport lock for command '{}'",
            command
        );
        let mut transport = self.transport.lock().await;
        debug!(
            "send_request_nowait: Writing {} request (seq {})",
            command, seq
        );
        transport.write_message(&Message::Request(request)).await?;
        debug!("send_request_nowait: Message written successfully");
        drop(transport);

        Ok(seq)
    }

    /// Send a request and wait for response (blocking)
    pub async fn send_request(&self, command: &str, arguments: Option<Value>) -> Result<Response> {
        let seq = self.seq_counter.fetch_add(1, Ordering::SeqCst);

        info!(
            "✉️  send_request: Sending '{}' request (seq {})",
            command, seq
        );

        let request = Request {
            seq,
            command: command.to_string(),
            arguments,
        };

        let (tx, rx) = oneshot::channel();

        {
            let mut pending = self.pending_requests.write().await;
            pending.insert(seq, tx);
            info!(
                "✉️  send_request: Registered pending request for seq {}",
                seq
            );
        }

        info!("✉️  send_request: Sending message to write channel");
        self.write_tx
            .send(Message::Request(request))
            .map_err(|_| self.write_channel_error())?;

        info!("✉️  send_request: Waiting for response to seq {}", seq);
        let response = rx
            .await
            .map_err(|_| Error::Dap("Request cancelled or connection closed".to_string()))?;

        info!(
            "✅ send_request: Received response for '{}' (seq {}), success: {}",
            command, seq, response.success
        );
        Ok(response)
    }

    /// Send a request with a timeout.
    /// On timeout, removes the orphaned entry from pending_requests to prevent
    /// stale requests from blocking subsequent operations.
    ///
    /// Before sending, cancels any orphaned pending requests from previous
    /// timed-out operations. This is critical for recovery: if a previous
    /// evaluate/variables request timed out while the adapter was still
    /// processing it, the adapter may have since finished and sent a response
    /// that went to a dropped receiver. Cleaning up first ensures we start
    /// with a clean request pipeline.
    pub async fn send_request_with_timeout(
        &self,
        command: &str,
        arguments: Option<Value>,
        timeout: std::time::Duration,
    ) -> Result<Response> {
        // Cancel any orphaned pending requests from previous timeouts.
        // DAP adapters process requests serially, so a stuck previous request
        // blocks all subsequent ones. By cleaning up orphans, we give the
        // adapter a chance to process our new request.
        {
            let pending = self.pending_requests.read().await;
            if !pending.is_empty() {
                drop(pending);
                let cancelled = self.cancel_pending_requests().await;
                if cancelled > 0 {
                    warn!(
                        "⏱️  send_request_with_timeout: Cleaned up {} orphaned pending request(s) before sending '{}'",
                        cancelled, command
                    );
                }
            }
        }

        info!(
            "⏱️  send_request_with_timeout: '{}' with timeout {:?}",
            command, timeout
        );

        let seq = self.seq_counter.fetch_add(1, Ordering::SeqCst);

        let request = Request {
            seq,
            command: command.to_string(),
            arguments,
        };

        let (tx, rx) = oneshot::channel();

        {
            let mut pending = self.pending_requests.write().await;
            pending.insert(seq, tx);
        }

        self.write_tx
            .send(Message::Request(request))
            .map_err(|_| self.write_channel_error())?;

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => {
                info!(
                    "✅ send_request_with_timeout: Received response for '{}' (seq {}), success: {}",
                    command, seq, response.success
                );
                Ok(response)
            }
            Ok(Err(_)) => {
                self.pending_requests.write().await.remove(&seq);
                if !self.adapter_alive.load(Ordering::SeqCst) {
                    return Err(self.write_channel_error());
                }
                Err(Error::Dap("Request cancelled or connection closed".to_string()))
            }
            Err(_) => {
                self.pending_requests.write().await.remove(&seq);
                // If the adapter died while we were waiting, the timeout is a
                // misleading symptom — surface the real cause so the caller
                // knows to restart instead of retrying.
                if !self.adapter_alive.load(Ordering::SeqCst) {
                    return Err(self.write_channel_error());
                }
                warn!(
                    "⏱️  send_request_with_timeout: '{}' (seq {}) timed out after {:?}, sending DAP cancel and pinging",
                    command, seq, timeout
                );
                self.send_cancel(seq).await;

                // Stuck-vs-dead disambiguation. The adapter process is alive
                // (adapter_alive check above), but the request did not return.
                // Two sub-cases:
                //   (a) the adapter's dispatcher is free — our original request
                //       was inside an LLDB call that we just cancelled, and the
                //       next request can be served. A `threads` ping returns
                //       quickly.
                //   (b) the adapter is wedged inside LLDB (synthetic walk on a
                //       large container is the common cause) and won't dispatch
                //       new requests either — the ping also times out.
                // Both cases need a different recovery from `AdapterExited`:
                // `debugger_cancel` may free (a); (b) typically needs
                // `debugger_disconnect` + restart. We surface them as one error
                // variant (`AdapterStuck`) with a sub-classifier in the message
                // so the agent can pick the right tool.
                const PING_DEADLINE: std::time::Duration = std::time::Duration::from_millis(1000);
                let responsive = self.ping_threads(PING_DEADLINE).await;
                let sub = if responsive {
                    "adapter responsive after cancel"
                } else {
                    "adapter dispatcher wedged"
                };
                let hint = if responsive {
                    "Recovery: retry the request with `noSynthetic: true`, evaluate a scalar field (e.g. `name.len`) instead of the container, or call `debugger_cancel` if subsequent calls hang."
                } else {
                    "Recovery: call `debugger_cancel` first (may free the dispatcher); if that does not unblock, `debugger_disconnect` and start fresh — avoid the stuck frame or set the breakpoint one frame up. The memory supervisor may kill the adapter if RSS keeps climbing."
                };
                Err(Error::AdapterStuck(format!(
                    "Request '{command}' timed out after {timeout:?} ({sub}). {hint}"
                )))
            }
        }
    }

    /// Send a request with a callback for the response
    pub async fn send_request_async<F>(
        &self,
        command: &str,
        arguments: Option<Value>,
        callback: F,
    ) -> Result<i32>
    where
        F: FnOnce(Result<Response>) + Send + 'static,
    {
        debug!("send_request_async: Starting for command '{}'", command);
        let seq = self.seq_counter.fetch_add(1, Ordering::SeqCst);

        let request = Request {
            seq,
            command: command.to_string(),
            arguments,
        };

        let (tx, rx) = oneshot::channel();

        debug!("send_request_async: Registering pending request");
        {
            let mut pending = self.pending_requests.write().await;
            pending.insert(seq, tx);
        }

        debug!(
            "send_request_async: Sending {} request (seq {}) to write channel",
            command, seq
        );
        self.write_tx
            .send(Message::Request(request))
            .map_err(|_| self.write_channel_error())?;
        debug!("send_request_async: Request queued");

        // Spawn task to wait for response and invoke callback
        tokio::spawn(async move {
            debug!(
                "send_request_async callback task: Waiting for response seq {}",
                seq
            );
            match rx.await {
                Ok(response) => {
                    debug!(
                        "send_request_async callback task: Got response for seq {}",
                        seq
                    );
                    callback(Ok(response))
                }
                Err(_) => callback(Err(Error::Dap(
                    "Request cancelled or connection closed".to_string(),
                ))),
            }
        });

        debug!("send_request_async: Returning seq {}", seq);
        Ok(seq)
    }

    pub async fn initialize(&self, adapter_id: &str) -> Result<Capabilities> {
        let args = InitializeRequestArguments {
            client_id: Some("debugger_mcp".to_string()),
            client_name: Some("debugger_mcp".to_string()),
            adapter_id: adapter_id.to_string(),
            locale: Some("en-US".to_string()),
            lines_start_at_1: Some(true),
            columns_start_at_1: Some(true),
            path_format: Some("path".to_string()),
            supports_variable_paging: Some(true),
        };

        let response = self
            .send_request("initialize", Some(serde_json::to_value(args)?))
            .await?;

        if !response.success {
            return Err(Error::Dap(format!(
                "Initialize failed: {:?}",
                response.message
            )));
        }

        let caps: Capabilities = response
            .body
            .ok_or_else(|| Error::Dap("No capabilities in initialize response".to_string()))
            .and_then(|v| {
                serde_json::from_value(v)
                    .map_err(|e| Error::Dap(format!("Failed to parse capabilities: {}", e)))
            })?;

        Ok(caps)
    }

    pub async fn launch(&self, args: Value) -> Result<()> {
        let response = self.send_request("launch", Some(args)).await?;

        if !response.success {
            return Err(Error::Dap(format!("Launch failed: {:?}", response.message)));
        }

        Ok(())
    }

    /// Proper DAP initialization and launch sequence following the specification
    /// This method implements the correct async flow:
    /// 1. Send initialize, get response
    /// 2. Register 'initialized' event handler (just signals, doesn't call methods)
    /// 3. Send launch (triggers 'initialized' event)
    /// 4. Wait for 'initialized' signal, then send configurationDone from main context
    /// 5. Wait for launch response
    pub async fn initialize_and_launch(
        &self,
        adapter_id: &str,
        launch_args: Value,
        adapter_type: Option<&str>,
    ) -> Result<()> {
        self.initialize_and_launch_with_pending(
            adapter_id,
            launch_args,
            adapter_type,
            HashMap::new(),
        )
        .await
    }

    pub async fn initialize_and_launch_with_pending(
        &self,
        adapter_id: &str,
        launch_args: Value,
        adapter_type: Option<&str>,
        pending_breakpoints: HashMap<String, Vec<SourceBreakpoint>>,
    ) -> Result<()> {
        // Step 1: Send initialize request and get capabilities
        info!("Sending initialize request to adapter");
        let capabilities = self.initialize(adapter_id).await?;
        debug!(
            "Adapter capabilities: supportsConfigurationDoneRequest={:?}",
            capabilities.supports_configuration_done_request
        );

        let config_done_supported = capabilities
            .supports_configuration_done_request
            .unwrap_or(false);

        // Check if we need stopOnEntry workaround (Ruby and Node.js)
        // - Ruby (rdbg): Doesn't honor --stop-at-load in socket mode
        // - Node.js (vscode-js-debug): Uses multi-session architecture, parent doesn't send stopped events
        // Workaround: Set entry breakpoint at first executable line, then continue
        // See: docs/RUBY_STOPENTRY_FIX.md, docs/NODEJS_STOPONENTRY_ANALYSIS.md

        // Debug logging to trace workaround detection
        info!("🔍 Workaround detection: adapter_type = {:?}", adapter_type);
        info!(
            "🔍 Launch args: {}",
            serde_json::to_string_pretty(&launch_args)
                .unwrap_or_else(|_| "invalid json".to_string())
        );

        let adapter_type_str = adapter_type.unwrap_or("");
        // NOTE: Go/Delve does NOT use entry breakpoint workaround
        // Delve's setBreakpoints request clears ALL previous breakpoints, so the entry breakpoint
        // would be removed when the user sets their first breakpoint
        let needs_entry_breakpoint = adapter_type_str == "ruby" || adapter_type_str == "nodejs";
        let stop_on_entry = launch_args
            .get("stopOnEntry")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let needs_workaround = needs_entry_breakpoint && stop_on_entry;

        info!("🔍 adapter_type_str = '{}', needs_entry_breakpoint = {}, stop_on_entry = {}, needs_workaround = {}",
              adapter_type_str, needs_entry_breakpoint, stop_on_entry, needs_workaround);

        if needs_workaround {
            info!(
                "🔧 {} stopOnEntry workaround will be applied (entry breakpoint)",
                adapter_type_str
            );
        }

        // Extract program path before moving launch_args (for entry breakpoint workaround)
        let program_path_for_breakpoint = if needs_workaround {
            launch_args
                .get("program")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        } else {
            None
        };

        // Modify launch_args for entry breakpoint workaround:
        // Change stopOnEntry to false so the program runs and hits our breakpoint
        let mut launch_args = launch_args;
        if needs_workaround {
            if let Some(obj) = launch_args.as_object_mut() {
                obj.insert("stopOnEntry".to_string(), serde_json::Value::Bool(false));
                info!("🔧 Workaround: Changed stopOnEntry to false in launch args");
            }
        }

        // Step 2: Register 'initialized' event handler BEFORE sending launch
        // This handler will be invoked DURING launch processing
        // We use a simple signal approach - the handler just notifies, doesn't send messages
        let (init_tx, init_rx) = oneshot::channel();
        let init_tx = Arc::new(tokio::sync::Mutex::new(Some(init_tx)));

        self.on_event("initialized", move |_event| {
            info!("Received 'initialized' event - signaling");
            let tx = init_tx.clone();
            // Just signal - don't call any async methods from here
            // This keeps the event handler fast (< 0.1ms like Python standalone test)
            tokio::spawn(async move {
                if let Some(sender) = tx.lock().await.take() {
                    let _ = sender.send(());
                }
            });
        })
        .await;

        // Step 3: Send launch request (doesn't wait for response yet)
        info!("Sending launch request with args: {:?}", launch_args);
        let launch_seq = self
            .send_request_nowait("launch", Some(launch_args))
            .await?;
        info!("Launch request sent with seq {}", launch_seq);

        // Step 4: Wait for 'initialized' event signal
        if config_done_supported {
            info!("Waiting for 'initialized' event (timeout: 5s)...");
            match tokio::time::timeout(tokio::time::Duration::from_secs(5), init_rx).await {
                Ok(Ok(())) => {
                    info!("✅ Received 'initialized' event signal");

                    // Apply pending breakpoints BEFORE configurationDone (correct DAP sequence)
                    if !pending_breakpoints.is_empty() {
                        info!(
                            "🔧 Applying {} pending breakpoints before configurationDone",
                            pending_breakpoints.len()
                        );
                        for (source_path, breakpoints) in &pending_breakpoints {
                            info!(
                                "  Setting {} breakpoints for {}",
                                breakpoints.len(),
                                source_path
                            );
                            let source = Source {
                                path: Some(source_path.clone()),
                                name: None,
                                source_reference: None,
                            };
                            match self.set_breakpoints(source, breakpoints.clone()).await {
                                Ok(bps) => {
                                    info!("  ✅ Set {} breakpoints for {}", bps.len(), source_path);
                                    for bp in bps {
                                        if bp.verified {
                                            info!("    Line {}: verified", bp.line.unwrap_or(0));
                                        } else {
                                            warn!(
                                                "    Line {}: NOT verified",
                                                bp.line.unwrap_or(0)
                                            );
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        "  ⚠️  Failed to set breakpoints for {}: {}",
                                        source_path, e
                                    );
                                }
                            }
                        }
                    }

                    // Entry breakpoint workaround: Set breakpoint BEFORE configurationDone
                    // This follows the correct DAP sequence (setBreakpoints must be before configurationDone)
                    if needs_workaround {
                        info!(
                            "🔧 Applying {} stopOnEntry workaround: setting entry breakpoint",
                            adapter_type_str
                        );
                        info!(
                            "   (Per DAP spec: breakpoints must be set BEFORE configurationDone)"
                        );

                        match program_path_for_breakpoint.as_deref() {
                            Some(path) => {
                                // Find first executable line based on language
                                let entry_line = if adapter_type_str == "ruby" {
                                    Self::find_first_executable_line_ruby(path)
                                } else if adapter_type_str == "go" {
                                    Self::find_first_executable_line_go(path)
                                } else {
                                    Self::find_first_executable_line_javascript(path)
                                };
                                info!("  Entry breakpoint will be set at line {}", entry_line);

                                // Create breakpoint at entry line
                                let source = Source {
                                    path: Some(path.to_string()),
                                    name: None,
                                    source_reference: None,
                                };

                                let breakpoint = SourceBreakpoint {
                                    line: entry_line as i32,
                                    column: None,
                                    condition: None,
                                    hit_condition: None,
                                };

                                // Set breakpoint BEFORE configurationDone (per DAP spec)
                                match self.set_breakpoints(source, vec![breakpoint]).await {
                                    Ok(bps) => {
                                        if let Some(bp) = bps.first() {
                                            if bp.verified {
                                                info!(
                                                    "✅ Entry breakpoint set at line {} (verified)",
                                                    entry_line
                                                );
                                            } else {
                                                warn!(
                                                    "⚠️  Entry breakpoint not verified at line {}",
                                                    entry_line
                                                );
                                                warn!("   Program may not stop - check if line is executable");
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        warn!("⚠️  Failed to set entry breakpoint: {}", e);
                                        warn!("   Continuing anyway - program might not stop at entry");
                                    }
                                }
                            }
                            None => {
                                warn!("⚠️  No program path in launch args - cannot set entry breakpoint");
                                warn!("   {} stopOnEntry may not work", adapter_type_str);
                            }
                        }
                    }
                }
                Ok(Err(_)) => {
                    error!("❌ 'initialized' event signal was cancelled");
                    return Err(Error::Dap(
                        "'initialized' event signal was cancelled".to_string(),
                    ));
                }
                Err(_) => {
                    error!("❌ Timeout waiting for 'initialized' event (5s)");
                    error!("   This usually means:");
                    error!("   1. The program path is invalid or not found");
                    error!("   2. The Python environment doesn't have the target program");
                    error!("   3. The program has a syntax error preventing launch");
                    error!("   4. debugpy couldn't start the target program");
                    error!("   Check that the program path exists and is executable");
                    return Err(Error::Dap("Timeout waiting for 'initialized' event (5s). Program may not exist or has errors.".to_string()));
                }
            }

            // Step 5: Now send configurationDone from main context (not from event handler)
            info!("Sending configurationDone");
            self.configuration_done().await?;
            info!("configurationDone completed");
        }

        // Step 6: Wait for launch response (using wait_for_event on the response)
        // The launch response should arrive shortly after configurationDone
        info!("Waiting for launch to complete");
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        info!("Launch sequence completed successfully");
        Ok(())
    }

    /// Helper to clone the client for use in callbacks
    /// Returns an Arc-wrapped clone of the necessary fields
    #[allow(dead_code)]
    fn clone_for_callback(&self) -> Self {
        Self {
            transport: self.transport.clone(),
            seq_counter: self.seq_counter.clone(),
            pending_requests: self.pending_requests.clone(),
            event_tx: self.event_tx.clone(),
            event_notifiers: self.event_notifiers.clone(),
            event_callbacks: self.event_callbacks.clone(),
            child_session_spawn_callback: self.child_session_spawn_callback.clone(),
            write_tx: self.write_tx.clone(),
            adapter_alive: self.adapter_alive.clone(),
            _child: None, // Don't clone the child process
        }
    }

    /// Map a write_tx send failure into the right error.
    ///
    /// If the adapter has died (writer task exited because `write_message`
    /// errored, or reader task exited because the stream closed), surface
    /// `Error::AdapterExited` so the caller knows the process is gone and
    /// the only recovery is to restart the session. Otherwise the channel
    /// closing is an internal bug — keep the old "Write channel closed"
    /// string so it's grep-able in logs.
    fn write_channel_error(&self) -> Error {
        if !self.adapter_alive.load(Ordering::SeqCst) {
            Error::AdapterExited(
                "the debug adapter process exited (likely caused by the debugged \
                 program crashing or LLDB itself running out of memory). Restart \
                 the debug session to recover."
                    .to_string(),
            )
        } else {
            Error::Dap("Write channel closed".to_string())
        }
    }

    /// Send a DAP `cancel` request for a timed-out request.
    /// This tells the adapter to abort the stuck operation (e.g., a hanging LLDB evaluate).
    /// Fire-and-forget: we don't wait for a response since the adapter may be unresponsive.
    async fn send_cancel(&self, request_seq: i32) {
        let cancel_args = serde_json::json!({ "requestId": request_seq });
        let cancel_seq = self.seq_counter.fetch_add(1, Ordering::SeqCst);
        let cancel_request = Request {
            seq: cancel_seq,
            command: "cancel".to_string(),
            arguments: Some(cancel_args),
        };
        if let Err(e) = self.write_tx.send(Message::Request(cancel_request)) {
            warn!("Failed to send DAP cancel request: {}", e);
        } else {
            info!(
                "🚫 Sent DAP cancel for timed-out request seq {} (cancel seq {})",
                request_seq, cancel_seq
            );
        }
    }

    /// Fire a `threads` request with a short deadline as a liveness probe.
    /// Used post-timeout to distinguish "adapter dispatcher is free, just
    /// this request was stuck inside LLDB" from "adapter is wedged and
    /// won't dispatch anything new". Does not surface errors — returns
    /// `true` iff a response came back inside `deadline`. On false, the
    /// ping's pending-request entry stays orphaned; the next
    /// `send_request_with_timeout` will clean it up via
    /// `cancel_pending_requests`.
    async fn ping_threads(&self, deadline: std::time::Duration) -> bool {
        let seq = self.seq_counter.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending_requests.write().await;
            pending.insert(seq, tx);
        }
        let req = Request {
            seq,
            command: "threads".to_string(),
            arguments: None,
        };
        if self.write_tx.send(Message::Request(req)).is_err() {
            self.pending_requests.write().await.remove(&seq);
            return false;
        }
        match tokio::time::timeout(deadline, rx).await {
            Ok(Ok(_)) => true,
            _ => {
                self.pending_requests.write().await.remove(&seq);
                false
            }
        }
    }

    /// Cancel all pending requests by removing them from the tracking map and
    /// dropping the senders (which causes waiting receivers to get a channel-closed error).
    /// Returns the number of cancelled requests.
    ///
    /// This is used before execution control commands (continue, step) to ensure
    /// a stuck evaluate doesn't prevent the adapter from processing new requests.
    pub async fn cancel_pending_requests(&self) -> usize {
        let mut pending = self.pending_requests.write().await;
        let count = pending.len();
        if count > 0 {
            let seqs: Vec<i32> = pending.keys().copied().collect();
            warn!(
                "🧹 cancel_pending_requests: Dropping {} orphaned pending request(s): {:?}",
                count, seqs
            );
            pending.clear();
            drop(pending);
            for seq in seqs {
                self.send_cancel(seq).await;
            }
        }
        count
    }

    pub async fn configuration_done(&self) -> Result<()> {
        let response = self.send_request("configurationDone", None).await?;

        if !response.success {
            return Err(Error::Dap(format!(
                "ConfigurationDone failed: {:?}",
                response.message
            )));
        }

        Ok(())
    }

    pub async fn set_breakpoints(
        &self,
        source: Source,
        breakpoints: Vec<SourceBreakpoint>,
    ) -> Result<Vec<Breakpoint>> {
        info!(
            "🔧 set_breakpoints: Starting for source {:?}, {} breakpoints",
            source.path,
            breakpoints.len()
        );
        for (i, bp) in breakpoints.iter().enumerate() {
            info!(
                "  Breakpoint {}: line {}, condition: {:?}",
                i, bp.line, bp.condition
            );
        }

        let args = SetBreakpointsArguments {
            source,
            breakpoints: Some(breakpoints),
            source_modified: Some(false),
        };

        info!("🔧 set_breakpoints: Sending setBreakpoints request...");
        let response = self
            .send_request("setBreakpoints", Some(serde_json::to_value(args)?))
            .await?;
        info!(
            "🔧 set_breakpoints: Received response, success: {}",
            response.success
        );

        if !response.success {
            return Err(Error::Dap(format!(
                "SetBreakpoints failed: {:?}",
                response.message
            )));
        }

        #[derive(serde::Deserialize)]
        struct SetBreakpointsResponse {
            breakpoints: Vec<Breakpoint>,
        }

        let body: SetBreakpointsResponse = response
            .body
            .ok_or_else(|| Error::Dap("No breakpoints in response".to_string()))
            .and_then(|v| {
                serde_json::from_value(v)
                    .map_err(|e| Error::Dap(format!("Failed to parse breakpoints: {}", e)))
            })?;

        info!(
            "✅ set_breakpoints: Success, {} breakpoints verified",
            body.breakpoints.len()
        );
        for (i, bp) in body.breakpoints.iter().enumerate() {
            info!(
                "  Breakpoint {}: id={:?}, verified={}, line={:?}",
                i, bp.id, bp.verified, bp.line
            );
        }

        Ok(body.breakpoints)
    }

    pub async fn continue_execution(&self, thread_id: i32) -> Result<()> {
        let args = ContinueArguments { thread_id };

        let response = self
            .send_request_with_timeout(
                "continue",
                Some(serde_json::to_value(args)?),
                std::time::Duration::from_secs(30),
            )
            .await?;

        if !response.success {
            return Err(Error::Dap(format!(
                "Continue failed: {:?}",
                response.message
            )));
        }

        Ok(())
    }

    /// Find the first executable line in a Ruby source file
    ///
    /// Skips comments, empty lines, requires, and class/module definitions
    /// to find the first actual executable line.
    ///
    /// Returns line number (1-indexed) or 1 as fallback.
    fn find_first_executable_line_ruby(program_path: &str) -> usize {
        use std::fs;

        let content = match fs::read_to_string(program_path) {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    "Could not read {} for line detection: {}, using line 1",
                    program_path, e
                );
                return 1;
            }
        };

        for (line_num, line) in content.lines().enumerate() {
            let trimmed = line.trim();

            // Skip empty lines
            if trimmed.is_empty() {
                continue;
            }

            // Skip shebang
            if line_num == 0 && trimmed.starts_with("#!") {
                continue;
            }

            // Skip comments
            if trimmed.starts_with('#') {
                continue;
            }

            // Skip requires/loads (not executable, just declarations)
            if trimmed.starts_with("require") || trimmed.starts_with("load") {
                continue;
            }

            // Skip class/module/def declarations (not entry point)
            if trimmed.starts_with("class ") || trimmed.starts_with("module ") {
                // Continue looking inside the class for executable code
                continue;
            }

            // Found first executable line!
            info!("  First executable line detected: {}", line_num + 1);
            return line_num + 1; // DAP uses 1-indexed lines
        }

        // Fallback: No executable line found, use line 1
        warn!("No executable line found in {}, using line 1", program_path);
        1
    }

    /// Find the first executable line in a Go source file
    ///
    /// Skips package declarations, imports, comments, and function signatures
    /// to find the first actual executable line (typically inside main function).
    ///
    /// Returns line number (1-indexed) or 1 as fallback.
    fn find_first_executable_line_go(program_path: &str) -> usize {
        use std::fs;

        let content = match fs::read_to_string(program_path) {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    "Could not read {} for line detection: {}, using line 1",
                    program_path, e
                );
                return 1;
            }
        };

        let mut in_func_main = false;

        for (line_num, line) in content.lines().enumerate() {
            let trimmed = line.trim();

            // Skip empty lines
            if trimmed.is_empty() {
                continue;
            }

            // Skip package declaration
            if trimmed.starts_with("package ") {
                continue;
            }

            // Skip import statements (single line and multi-line)
            if trimmed.starts_with("import ") || trimmed.starts_with("import(") {
                continue;
            }

            // Skip comments (// and /*)
            if trimmed.starts_with("//") || trimmed.starts_with("/*") {
                continue;
            }

            // Detect main function
            if trimmed.starts_with("func main()") {
                in_func_main = true;
                continue;
            }

            // If we're inside main function, find first executable line
            if in_func_main {
                // Skip opening brace, comments, and variable declarations without initialization
                if trimmed == "{" || trimmed.starts_with("//") || trimmed.starts_with("var ") {
                    continue;
                }

                // Found first executable line inside main!
                info!(
                    "  First executable line detected (Go main): {}",
                    line_num + 1
                );
                return line_num + 1; // DAP uses 1-indexed lines
            }
        }

        // Fallback: No executable line found, use line 1
        warn!("No executable line found in {}, using line 1", program_path);
        1
    }

    /// Find the first executable line in a JavaScript/TypeScript source file
    ///
    /// Skips comments, empty lines, imports, and type declarations
    /// to find the first actual executable line.
    ///
    /// Returns line number (1-indexed) or 1 as fallback.
    pub fn find_first_executable_line_javascript(program_path: &str) -> usize {
        use std::fs;

        let content = match fs::read_to_string(program_path) {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    "Could not read {} for line detection: {}, using line 1",
                    program_path, e
                );
                return 1;
            }
        };

        let mut in_multiline_comment = false;

        for (line_num, line) in content.lines().enumerate() {
            let trimmed = line.trim();

            // Handle multiline comments
            if in_multiline_comment {
                if trimmed.contains("*/") {
                    in_multiline_comment = false;
                }
                continue;
            }

            if trimmed.contains("/*") {
                in_multiline_comment = true;
                continue;
            }

            // Skip empty lines
            if trimmed.is_empty() {
                continue;
            }

            // Skip shebang
            if line_num == 0 && trimmed.starts_with("#!") {
                continue;
            }

            // Skip single-line comments
            if trimmed.starts_with("//") {
                continue;
            }

            // Skip import/require statements (not executable, just declarations)
            if trimmed.starts_with("import ")
                || trimmed.starts_with("export ")
                || trimmed.starts_with("require(")
                || trimmed.starts_with("const ") && trimmed.contains("require(")
            {
                continue;
            }

            // Skip type declarations (TypeScript)
            if trimmed.starts_with("type ") || trimmed.starts_with("interface ") {
                continue;
            }

            // Skip function declarations (look for first line OUTSIDE function)
            if trimmed.starts_with("function ")
                || trimmed.starts_with("const ") && trimmed.contains("=>")
            {
                // Skip function declarations - we want module-level code
                continue;
            }

            // Skip class declarations
            if trimmed.starts_with("class ") {
                continue;
            }

            // Skip variable declarations without initialization
            if (trimmed.starts_with("let ")
                || trimmed.starts_with("var ")
                || trimmed.starts_with("const "))
                && !trimmed.contains('=')
            {
                continue;
            }

            // Skip indented lines (likely inside function/class bodies)
            // Module-level executable code should start at column 0
            if line.starts_with(' ') || line.starts_with('\t') {
                continue;
            }

            // Found first executable line at module level!
            info!("  First executable line detected: {}", line_num + 1);
            return line_num + 1; // DAP uses 1-indexed lines
        }

        // Fallback: No executable line found, use line 1
        warn!("No executable line found in {}, using line 1", program_path);
        1
    }

    pub async fn next(&self, thread_id: i32) -> Result<()> {
        let args = NextArguments { thread_id };

        let response = self
            .send_request_with_timeout(
                "next",
                Some(serde_json::to_value(args)?),
                std::time::Duration::from_secs(30),
            )
            .await?;

        if !response.success {
            return Err(Error::Dap(format!(
                "Next (step over) failed: {:?}",
                response.message
            )));
        }

        Ok(())
    }

    pub async fn step_in(&self, thread_id: i32) -> Result<()> {
        let args = StepInArguments { thread_id };

        let response = self
            .send_request_with_timeout(
                "stepIn",
                Some(serde_json::to_value(args)?),
                std::time::Duration::from_secs(30),
            )
            .await?;

        if !response.success {
            return Err(Error::Dap(format!("StepIn failed: {:?}", response.message)));
        }

        Ok(())
    }

    pub async fn step_out(&self, thread_id: i32) -> Result<()> {
        let args = StepOutArguments { thread_id };

        let response = self
            .send_request_with_timeout(
                "stepOut",
                Some(serde_json::to_value(args)?),
                std::time::Duration::from_secs(30),
            )
            .await?;

        if !response.success {
            return Err(Error::Dap(format!(
                "StepOut failed: {:?}",
                response.message
            )));
        }

        Ok(())
    }

    pub async fn stack_trace(&self, thread_id: i32, levels: Option<i32>) -> Result<Vec<StackFrame>> {
        let args = StackTraceArguments {
            thread_id,
            start_frame: None,
            levels,
        };

        let timeout = super::timeouts::stack_trace();
        let response = self
            .send_request_with_timeout("stackTrace", Some(serde_json::to_value(args)?), timeout)
            .await
            .map_err(|e| Error::Dap(format!(
                "stackTrace timed out after {timeout:?} for thread {thread_id}. \
                 The debug adapter may be unresponsive. Try: debugger_disconnect and restart the session. \
                 Original error: {e}"
            )))?;

        if !response.success {
            return Err(Error::Dap(format!(
                "StackTrace failed: {:?}",
                response.message
            )));
        }

        #[derive(serde::Deserialize)]
        struct StackTraceResponse {
            #[serde(rename = "stackFrames")]
            stack_frames: Vec<StackFrame>,
        }

        let body: StackTraceResponse = response
            .body
            .ok_or_else(|| Error::Dap("No stack frames in response".to_string()))
            .and_then(|v| {
                serde_json::from_value(v)
                    .map_err(|e| Error::Dap(format!("Failed to parse stack frames: {}", e)))
            })?;

        Ok(body.stack_frames)
    }

    pub async fn scopes(&self, frame_id: i32) -> Result<Vec<Scope>> {
        let args = ScopesArguments { frame_id };

        let timeout = super::timeouts::scopes();
        let response = self
            .send_request_with_timeout("scopes", Some(serde_json::to_value(args)?), timeout)
            .await?;

        if !response.success {
            return Err(Error::Dap(format!(
                "Scopes failed: {:?}",
                response.message
            )));
        }

        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct ScopesResponse {
            scopes: Vec<Scope>,
        }

        let body: ScopesResponse = response
            .body
            .ok_or_else(|| Error::Dap("No body in scopes response".to_string()))
            .and_then(|v| {
                serde_json::from_value(v)
                    .map_err(|e| Error::Dap(format!("Failed to parse scopes: {}", e)))
            })?;

        Ok(body.scopes)
    }

    /// Fetch variables with optional count limit and filter.
    /// When `max_count` is Some, uses DAP pagination to request at most that many children.
    /// This prevents CodeLLDB/LLDB from materializing huge collections (Vec with 100k elements)
    /// which can cause multi-GB memory usage.
    /// `filter`: "indexed" for array elements, "named" for struct fields, None for all.
    /// `no_synthetic`: when true, sets `format.showRaw=true` (CodeLLDB extension) so the
    /// adapter exposes raw struct fields instead of synthetic children. Cheap path for
    /// reading length/scalar fields off large containers without materialising elements.
    pub async fn variables_limited(
        &self,
        variables_reference: i32,
        max_count: Option<i32>,
        filter: Option<&str>,
        no_synthetic: bool,
    ) -> Result<Vec<Variable>> {
        let args = VariablesArguments {
            variables_reference,
            filter: filter.map(|s| s.to_string()),
            start: Some(0),
            count: max_count,
            format: if no_synthetic {
                Some(crate::dap::types::ValueFormat {
                    show_raw: Some(true),
                    ..Default::default()
                })
            } else {
                None
            },
        };

        // Default 4s — chosen so the request is cancelled before the supervisor
        // (which polls every 500ms with a low RSS limit in tests) has time to
        // kill the adapter. Override with DAP_VARIABLES_TIMEOUT_MS for large
        // Rust binaries where DWARF lookups are slow.
        let timeout = super::timeouts::variables_limited();
        let response = self
            .send_request_with_timeout("variables", Some(serde_json::to_value(args)?), timeout)
            .await
            .map_err(|e| Error::Dap(format!(
                "variables request timed out after {timeout:?} (variablesReference={variables_reference}). \
                 This often happens with large or deeply nested types (e.g. HashMap, Vec with many elements). \
                 Pass `noSynthetic: true` to bypass synthetic providers, or evaluate a specific field instead. \
                 Original error: {e}"
            )))?;

        if !response.success {
            return Err(Error::Dap(format!(
                "Variables failed: {:?}",
                response.message
            )));
        }

        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct VariablesResponse {
            variables: Vec<Variable>,
        }

        let body: VariablesResponse = response
            .body
            .ok_or_else(|| Error::Dap("No body in variables response".to_string()))
            .and_then(|v| {
                serde_json::from_value(v)
                    .map_err(|e| Error::Dap(format!("Failed to parse variables: {}", e)))
            })?;

        Ok(body.variables)
    }

    pub async fn variables(&self, variables_reference: i32) -> Result<Vec<Variable>> {
        let args = VariablesArguments {
            variables_reference,
            filter: None,
            start: None,
            count: None,
            format: None,
        };

        let timeout = super::timeouts::variables_full();
        let response = self
            .send_request_with_timeout("variables", Some(serde_json::to_value(args)?), timeout)
            .await
            .map_err(|e| Error::Dap(format!(
                "variables request timed out after {timeout:?} (variablesReference={variables_reference}). \
                 This often happens with large or deeply nested types (e.g. HashMap, Vec with many elements). \
                 Try evaluating a specific field instead of the whole variable. \
                 Original error: {e}"
            )))?;

        if !response.success {
            return Err(Error::Dap(format!(
                "Variables failed: {:?}",
                response.message
            )));
        }

        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct VariablesResponse {
            variables: Vec<Variable>,
        }

        let body: VariablesResponse = response
            .body
            .ok_or_else(|| Error::Dap("No body in variables response".to_string()))
            .and_then(|v| {
                serde_json::from_value(v)
                    .map_err(|e| Error::Dap(format!("Failed to parse variables: {}", e)))
            })?;

        Ok(body.variables)
    }

    pub async fn set_exception_breakpoints(&self, filters: Vec<String>) -> Result<()> {
        let args = SetExceptionBreakpointsArguments {
            filters,
        };

        let timeout = std::time::Duration::from_secs(10);
        let response = self
            .send_request_with_timeout("setExceptionBreakpoints", Some(serde_json::to_value(args)?), timeout)
            .await?;

        if !response.success {
            return Err(Error::Dap(format!(
                "setExceptionBreakpoints failed: {:?}",
                response.message
            )));
        }

        Ok(())
    }

    /// Query data breakpoint info for a variable. Returns the opaque dataId
    /// needed by setDataBreakpoints, plus the supported access types.
    pub async fn data_breakpoint_info(
        &self,
        name: &str,
        variables_reference: Option<i32>,
        frame_id: Option<i32>,
    ) -> Result<DataBreakpointInfoBody> {
        let args = DataBreakpointInfoArguments {
            name: name.to_string(),
            variables_reference,
            frame_id,
        };

        let timeout = std::time::Duration::from_secs(10);
        let response = self
            .send_request_with_timeout(
                "dataBreakpointInfo",
                Some(serde_json::to_value(args)?),
                timeout,
            )
            .await?;

        if !response.success {
            return Err(Error::Dap(format!(
                "dataBreakpointInfo failed: {:?}",
                response.message
            )));
        }

        let body: DataBreakpointInfoBody = serde_json::from_value(
            response
                .body
                .ok_or_else(|| Error::Dap("No body in dataBreakpointInfo response".to_string()))?,
        )
        .map_err(|e| Error::Dap(format!("Failed to parse dataBreakpointInfo response: {}", e)))?;

        Ok(body)
    }

    /// Set data breakpoints (watchpoints). Replaces all current data breakpoints.
    pub async fn set_data_breakpoints(
        &self,
        breakpoints: Vec<DataBreakpoint>,
    ) -> Result<Vec<Breakpoint>> {
        let args = SetDataBreakpointsArguments { breakpoints };

        let timeout = std::time::Duration::from_secs(10);
        let response = self
            .send_request_with_timeout(
                "setDataBreakpoints",
                Some(serde_json::to_value(args)?),
                timeout,
            )
            .await?;

        if !response.success {
            return Err(Error::Dap(format!(
                "setDataBreakpoints failed: {:?}",
                response.message
            )));
        }

        let body = response
            .body
            .ok_or_else(|| Error::Dap("No body in setDataBreakpoints response".to_string()))?;
        let breakpoints: Vec<Breakpoint> =
            serde_json::from_value(body["breakpoints"].clone()).unwrap_or_default();

        Ok(breakpoints)
    }

    pub async fn evaluate(
        &self,
        expression: &str,
        frame_id: Option<i32>,
        context: Option<String>,
    ) -> Result<String> {
        let (result, _variables_reference) =
            self.evaluate_raw(expression, frame_id, context).await?;

        // Do NOT auto-expand children via the `variables` DAP request.
        // Many adapters (including CodeLLDB/LLDB) ignore the `count` pagination
        // parameter and materialise ALL children in memory.  For a Vec with 10k
        // elements or a HashMap with 5k entries this causes multi-GB RSS.
        //
        // The evaluate response already contains a summary string (e.g.
        // "size=10000" for Vec, "size=5000" for HashMap).  If the caller needs
        // to see individual elements, they should evaluate a specific index
        // (e.g. "big_vec[0]") or field (e.g. "big_map[\"key\"]").

        Ok(result)
    }

    /// Raw evaluate returning both result string and variablesReference
    pub async fn evaluate_raw(
        &self,
        expression: &str,
        frame_id: Option<i32>,
        context: Option<String>,
    ) -> Result<(String, i32)> {
        // If frame_id is None, get the top frame from stack trace
        let frame_id = if let Some(id) = frame_id {
            Some(id)
        } else {
            // Get current thread (assume thread 0 for simplicity)
            match self.stack_trace(0, Some(1)).await {
                Ok(frames) if !frames.is_empty() => {
                    info!("📍 Auto-fetched frame_id {} for evaluate", frames[0].id);
                    Some(frames[0].id)
                }
                Ok(_) => {
                    warn!("⚠️  No stack frames available for evaluate");
                    None
                }
                Err(e) => {
                    warn!("⚠️  Failed to get stack trace for evaluate: {}", e);
                    None
                }
            }
        };

        // CodeLLDB convention: a leading `?` on the expression disables synthetic
        // children for the eval. Strip it from the wire expression and route it
        // through `format.showRaw` instead — same effect, but works on adapters
        // that don't recognise the prefix and on DAP-spec consumers that
        // reflect arguments back to the user.
        let (wire_expression, no_synthetic) = match expression.strip_prefix('?') {
            Some(rest) => (rest.to_string(), true),
            None => (expression.to_string(), false),
        };

        let args = EvaluateArguments {
            expression: wire_expression,
            frame_id,
            context: Some(context.unwrap_or_else(|| "watch".to_string())),
            format: if no_synthetic {
                Some(crate::dap::types::ValueFormat {
                    show_raw: Some(true),
                    ..Default::default()
                })
            } else {
                None
            },
        };

        // Default 8s. Override via DAP_EVAL_TIMEOUT_MS for huge Rust binaries
        // where first-touch DWARF lookups for a single name can take >30s.
        // Tight default is intentional: a fast failure with a structured error
        // is more useful than a long block — extend only when you know the
        // binary actually needs it.
        let timeout = super::timeouts::evaluate();
        let response = self
            .send_request_with_timeout("evaluate", Some(serde_json::to_value(args)?), timeout)
            .await
            .map_err(|e| Error::Dap(format!(
                "evaluate timed out after {timeout:?} for expression '{}'. \
                 This can happen when: (1) the expression triggers expensive computation in the debugger, \
                 (2) the type is large/deeply nested (e.g. HashMap, Vec with many elements), or \
                 (3) the debug adapter is stuck. \
                 Try: prefix the expression with `?` to disable synthetic children (CodeLLDB convention), \
                 use context=\"repl\" with an LLDB command like 'frame variable <name>' for faster inspection, \
                 or evaluate a specific field (e.g. 'obj.field') instead of the whole object. \
                 Original error: {e}",
                expression
            )))?;

        if !response.success {
            return Err(Error::Dap(format!(
                "Evaluate failed: {:?}",
                response.message
            )));
        }

        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct EvaluateResponse {
            result: String,
            #[serde(default)]
            variables_reference: i32,
        }

        let body: EvaluateResponse = response
            .body
            .ok_or_else(|| Error::Dap("No result in evaluate response".to_string()))
            .and_then(|v| {
                serde_json::from_value(v)
                    .map_err(|e| Error::Dap(format!("Failed to parse evaluate result: {}", e)))
            })?;

        Ok((body.result, body.variables_reference))
    }

    /// Maximum children to fetch per variable expansion level.
    /// Prevents CodeLLDB/LLDB from materializing huge collections in memory.
    /// A Vec<T> with 100k elements would otherwise cause multi-GB memory usage.
    const MAX_CHILDREN_PER_LEVEL: i32 = 50;

    /// Recursively expand a variablesReference into a readable string.
    /// Caps children per level to MAX_CHILDREN_PER_LEVEL to prevent
    /// memory explosion in the debug adapter.
    pub(crate) fn expand_variable(
        &self,
        variables_reference: i32,
        max_depth: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + '_>> {
        Box::pin(async move {
            if max_depth == 0 || variables_reference <= 0 {
                return Ok(String::new());
            }

            let children = self.variables_limited(
                variables_reference,
                Some(Self::MAX_CHILDREN_PER_LEVEL),
                None,
                false,
            ).await?;
            if children.is_empty() {
                return Ok(String::new());
            }

            let total_children = children.len();
            let mut lines = Vec::new();
            for child in &children {
                // Skip [[raw]] entries — internal LLDB synthetic provider detail
                if child.name == "[raw]" {
                    continue;
                }
                // Skip character-by-character string expansion (unsigned char children)
                let type_str = child.type_.as_deref().unwrap_or("");
                if type_str == "unsigned char" || type_str == "char" {
                    continue;
                }

                let child_expansion = if child.variables_reference > 0 && max_depth > 1 {
                    match self
                        .expand_variable(child.variables_reference, max_depth - 1)
                        .await
                    {
                        Ok(s) if !s.is_empty() => {
                            let indented: String = s
                                .lines()
                                .map(|l| format!("  {}", l))
                                .collect::<Vec<_>>()
                                .join("\n");
                            format!("\n{}", indented)
                        }
                        _ => String::new(),
                    }
                } else if child.variables_reference > 0 {
                    " {…}".to_string()
                } else {
                    String::new()
                };

                if type_str.is_empty() {
                    lines.push(format!(
                        "  [{}]: {}{}",
                        child.name, child.value, child_expansion
                    ));
                } else {
                    lines.push(format!(
                        "  [{}]: {} ({}){}",
                        child.name, child.value, type_str, child_expansion
                    ));
                }
            }

            if total_children as i32 >= Self::MAX_CHILDREN_PER_LEVEL {
                lines.push(format!(
                    "  ... (showing first {} children, use specific field access for more)",
                    Self::MAX_CHILDREN_PER_LEVEL
                ));
            }

            Ok(lines.join("\n"))
        })
    }

    pub async fn disconnect(&self) -> Result<()> {
        let response = self.send_request("disconnect", None).await?;

        if !response.success {
            warn!("Disconnect failed: {:?}", response.message);
        }

        Ok(())
    }

    // === Timeout Wrappers (Aggressive Timeouts) ===

    /// Initialize with 2 second timeout
    /// DAP init takes ~100ms normally, 2s = 20x safety margin
    pub async fn initialize_with_timeout(&self, adapter_id: &str) -> Result<Capabilities> {
        let timeout = std::time::Duration::from_secs(2);
        info!("⏱️  initialize_with_timeout: Starting with 2s timeout");

        tokio::time::timeout(timeout, self.initialize(adapter_id))
            .await
            .map_err(|_| Error::Dap(format!("Initialize timed out after {:?}", timeout)))?
    }

    /// Launch with 5 second timeout
    /// Launch is more complex and may involve file loading
    pub async fn launch_with_timeout(&self, args: Value) -> Result<()> {
        let timeout = std::time::Duration::from_secs(5);
        info!("⏱️  launch_with_timeout: Starting with 5s timeout");

        tokio::time::timeout(timeout, self.launch(args))
            .await
            .map_err(|_| Error::Dap(format!("Launch timed out after {:?}", timeout)))?
    }

    /// Disconnect with 2 second timeout (force cleanup)
    /// If disconnect hangs, we want to fail fast and let process cleanup handle it
    pub async fn disconnect_with_timeout(&self) -> Result<()> {
        let timeout = std::time::Duration::from_secs(2);
        info!("⏱️  disconnect_with_timeout: Starting with 2s timeout");

        tokio::time::timeout(timeout, self.disconnect())
            .await
            .map_err(|_| {
                warn!(
                    "Disconnect timed out after {:?}, proceeding anyway",
                    timeout
                );
                Error::Dap(format!("Disconnect timed out after {:?}", timeout))
            })?
    }

    /// Initialize and launch with combined timeout (2s + 5s = 7s total)
    /// This wraps the entire sequence with aggressive timeouts
    pub async fn initialize_and_launch_with_timeout(
        &self,
        adapter_id: &str,
        launch_args: Value,
        adapter_type: Option<&str>,
    ) -> Result<()> {
        self.initialize_and_launch_with_timeout_and_pending(
            adapter_id,
            launch_args,
            adapter_type,
            HashMap::new(),
        )
        .await
    }

    pub async fn initialize_and_launch_with_timeout_and_pending(
        &self,
        adapter_id: &str,
        launch_args: Value,
        adapter_type: Option<&str>,
        pending_breakpoints: HashMap<String, Vec<SourceBreakpoint>>,
    ) -> Result<()> {
        let timeout = std::time::Duration::from_secs(7);
        info!("⏱️  initialize_and_launch_with_timeout: Starting with 7s timeout");
        if let Some(atype) = adapter_type {
            info!("   Adapter type: {}", atype);
        }

        tokio::time::timeout(
            timeout,
            self.initialize_and_launch_with_pending(
                adapter_id,
                launch_args,
                adapter_type,
                pending_breakpoints,
            ),
        )
        .await
        .map_err(|_| {
            Error::Dap(format!(
                "Initialize and launch timed out after {:?}",
                timeout
            ))
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::super::transport_trait::DapTransportTrait;
    use super::*;
    use mockall::mock;
    use serde_json::json;

    mock! {
        pub TestTransport {}

        #[async_trait::async_trait]
        impl DapTransportTrait for TestTransport {
            async fn read_message(&mut self) -> Result<Message>;
            async fn write_message(&mut self, msg: &Message) -> Result<()>;
        }
    }

    // Helper to create a mock that responds once then errors
    fn create_mock_with_response(response: Response) -> MockTestTransport {
        let mut mock = MockTestTransport::new();

        // Expect write
        mock.expect_write_message().times(1).returning(|_| Ok(()));

        // Return response once
        mock.expect_read_message()
            .times(1)
            .return_once(move || Ok(Message::Response(response)));

        // Then error to stop message loop
        mock.expect_read_message()
            .returning(|| Err(Error::Dap("Connection closed".to_string())));

        mock
    }

    #[tokio::test]
    async fn test_dap_client_initialize() {
        let mut mock_transport = MockTestTransport::new();

        // Expect write of initialize request
        mock_transport
            .expect_write_message()
            .times(1)
            .returning(|_| Ok(()));

        // Return initialize response, then error to stop message loop
        mock_transport.expect_read_message().times(1).returning(|| {
            Ok(Message::Response(Response {
                seq: 1,
                request_seq: 1,
                command: "initialize".to_string(),
                success: true,
                message: None,
                body: Some(json!({
                    "supportsConfigurationDoneRequest": true,
                    "supportsFunctionBreakpoints": false,
                    "supportsConditionalBreakpoints": true,
                })),
            }))
        });

        // Second read returns error to stop background task
        mock_transport
            .expect_read_message()
            .returning(|| Err(Error::Dap("Connection closed".to_string())));

        let client = DapClient::new_with_transport(Box::new(mock_transport), None)
            .await
            .unwrap();

        let caps = client.initialize("test-adapter").await.unwrap();

        assert!(caps.supports_configuration_done_request.unwrap_or(false));
        assert!(!caps.supports_function_breakpoints.unwrap_or(true));
        assert!(caps.supports_conditional_breakpoints.unwrap_or(false));
    }

    #[tokio::test]
    async fn test_dap_client_launch_success() {
        let mock_transport = create_mock_with_response(Response {
            seq: 1,
            request_seq: 1,
            command: "launch".to_string(),
            success: true,
            message: None,
            body: None,
        });

        let client = DapClient::new_with_transport(Box::new(mock_transport), None)
            .await
            .unwrap();

        let launch_args = json!({"program": "test.py"});
        client.launch(launch_args).await.unwrap();
    }

    #[tokio::test]
    async fn test_dap_client_launch_failure() {
        let mock_transport = create_mock_with_response(Response {
            seq: 1,
            request_seq: 1,
            command: "launch".to_string(),
            success: false,
            message: Some("Failed to start program".to_string()),
            body: None,
        });

        let client = DapClient::new_with_transport(Box::new(mock_transport), None)
            .await
            .unwrap();

        let launch_args = json!({"program": "test.py"});
        let result = client.launch(launch_args).await;

        assert!(result.is_err());
        match result {
            Err(Error::Dap(msg)) => assert!(msg.contains("Launch failed")),
            _ => panic!("Expected Dap error"),
        }
    }

    #[tokio::test]
    async fn test_dap_client_set_breakpoints() {
        let mock_transport = create_mock_with_response(Response {
            seq: 1,
            request_seq: 1,
            command: "setBreakpoints".to_string(),
            success: true,
            message: None,
            body: Some(json!({
                "breakpoints": [
                    {
                        "id": 1,
                        "verified": true,
                        "line": 10
                    }
                ]
            })),
        });

        let client = DapClient::new_with_transport(Box::new(mock_transport), None)
            .await
            .unwrap();

        let source = Source {
            name: Some("test.py".to_string()),
            path: Some("/path/to/test.py".to_string()),
            source_reference: None,
        };

        let breakpoints = vec![SourceBreakpoint {
            line: 10,
            column: None,
            condition: None,
            hit_condition: None,
        }];

        let result = client.set_breakpoints(source, breakpoints).await.unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, Some(1));
        assert!(result[0].verified);
    }

    #[tokio::test]
    async fn test_dap_client_continue_execution() {
        let mock_transport = create_mock_with_response(Response {
            seq: 1,
            request_seq: 1,
            command: "continue".to_string(),
            success: true,
            message: None,
            body: None,
        });

        let client = DapClient::new_with_transport(Box::new(mock_transport), None)
            .await
            .unwrap();

        client.continue_execution(1).await.unwrap();
    }

    #[tokio::test]
    async fn test_dap_client_stack_trace() {
        let mock_transport = create_mock_with_response(Response {
            seq: 1,
            request_seq: 1,
            command: "stackTrace".to_string(),
            success: true,
            message: None,
            body: Some(json!({
                "stackFrames": [
                    {
                        "id": 1,
                        "name": "main",
                        "line": 42,
                        "column": 10
                    }
                ]
            })),
        });

        let client = DapClient::new_with_transport(Box::new(mock_transport), None)
            .await
            .unwrap();

        let frames = client.stack_trace(1, None).await.unwrap();

        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].name, "main");
        assert_eq!(frames[0].line, 42);
    }

    #[tokio::test]
    async fn test_dap_client_evaluate() {
        let mock_transport = create_mock_with_response(Response {
            seq: 1,
            request_seq: 1,
            command: "evaluate".to_string(),
            success: true,
            message: None,
            body: Some(json!({
                "result": "42"
            })),
        });

        let client = DapClient::new_with_transport(Box::new(mock_transport), None)
            .await
            .unwrap();

        let result = client.evaluate("x + y", Some(1), None).await.unwrap();

        assert_eq!(result, "42");
    }

    #[tokio::test]
    async fn test_dap_client_configuration_done() {
        let mock_transport = create_mock_with_response(Response {
            seq: 1,
            request_seq: 1,
            command: "configurationDone".to_string(),
            success: true,
            message: None,
            body: None,
        });

        let client = DapClient::new_with_transport(Box::new(mock_transport), None)
            .await
            .unwrap();

        client.configuration_done().await.unwrap();
    }

    #[tokio::test]
    async fn test_dap_client_disconnect() {
        let mock_transport = create_mock_with_response(Response {
            seq: 1,
            request_seq: 1,
            command: "disconnect".to_string(),
            success: true,
            message: None,
            body: None,
        });

        let client = DapClient::new_with_transport(Box::new(mock_transport), None)
            .await
            .unwrap();

        client.disconnect().await.unwrap();
    }

    // ========================================================================
    // Channel-based mock transport for complex interaction testing
    // ========================================================================

    /// A mock transport where requests go out via a channel and responses
    /// come back via another channel.  This lets a test task sit in the
    /// middle, inspect each request, and decide what/when to respond.
    struct ChannelTransport {
        /// Requests written by the client arrive here (test reads them)
        outgoing_tx: tokio::sync::mpsc::UnboundedSender<Message>,
        /// Responses fed by the test arrive here (client reads them)
        incoming_rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Message>>,
    }

    #[async_trait::async_trait]
    impl DapTransportTrait for ChannelTransport {
        async fn read_message(&mut self) -> Result<Message> {
            let mut rx = self.incoming_rx.lock().await;
            rx.recv()
                .await
                .ok_or_else(|| Error::Dap("Channel closed".to_string()))
        }
        async fn write_message(&mut self, msg: &Message) -> Result<()> {
            self.outgoing_tx
                .send(msg.clone())
                .map_err(|_| Error::Dap("Channel closed".to_string()))
        }
    }

    struct ChannelTransportPair {
        /// Send responses/events TO the client
        incoming_tx: tokio::sync::mpsc::UnboundedSender<Message>,
        /// Receive requests FROM the client
        outgoing_rx: tokio::sync::mpsc::UnboundedReceiver<Message>,
    }

    fn create_channel_transport() -> (Box<ChannelTransport>, ChannelTransportPair) {
        let (outgoing_tx, outgoing_rx) = tokio::sync::mpsc::unbounded_channel();
        let (incoming_tx, incoming_rx) = tokio::sync::mpsc::unbounded_channel();
        let transport = Box::new(ChannelTransport {
            outgoing_tx,
            incoming_rx: tokio::sync::Mutex::new(incoming_rx),
        });
        let pair = ChannelTransportPair {
            incoming_tx,
            outgoing_rx,
        };
        (transport, pair)
    }

    /// Helper: receive the next request from the client, extracting the seq
    async fn recv_request(pair: &mut ChannelTransportPair) -> (i32, String, Option<Value>) {
        let msg = pair.outgoing_rx.recv().await.expect("channel closed");
        match msg {
            Message::Request(req) => (req.seq, req.command, req.arguments),
            other => panic!("Expected request, got {:?}", other),
        }
    }

    /// Like `recv_request`, but transparently skips DAP `cancel` requests
    /// (fired async after a prior timeout) and post-timeout `threads` pings
    /// (fired by `send_request_with_timeout` for stuck-vs-dead disambiguation).
    /// Tests that fire multiple requests in sequence need this so they see
    /// the next *user* request, not the recovery housekeeping.
    async fn recv_request_skipping_cancels(
        pair: &mut ChannelTransportPair,
    ) -> (i32, String, Option<Value>) {
        loop {
            let msg = pair.outgoing_rx.recv().await.expect("channel closed");
            match msg {
                Message::Request(req)
                    if req.command == "cancel" || req.command == "threads" =>
                {
                    continue
                }
                Message::Request(req) => return (req.seq, req.command, req.arguments),
                other => panic!("Expected request, got {:?}", other),
            }
        }
    }

    /// Helper: send a success response back to the client
    fn send_response(pair: &ChannelTransportPair, request_seq: i32, command: &str, body: Option<Value>) {
        pair.incoming_tx
            .send(Message::Response(Response {
                seq: 0,
                request_seq,
                command: command.to_string(),
                success: true,
                message: None,
                body,
            }))
            .expect("send failed");
    }

    // ========================================================================
    // Bug reproduction: expand_variable requests unlimited children
    // ========================================================================

    /// Reproduce: expand_variable must send `count` in the variables request
    /// to prevent LLDB from materialising huge collections.
    ///
    /// The test creates a client whose "evaluate" returns a variablesReference,
    /// then checks that the subsequent "variables" request includes a `count`
    /// field (i.e. uses variables_limited, not the unbounded variables).
    #[tokio::test]
    async fn test_expand_variable_limits_children_count() {
        let (transport, mut pair) = create_channel_transport();
        let client = DapClient::new_with_transport(transport, None)
            .await
            .unwrap();

        // Run expand_variable(42, 1) in a task
        let client2 = client.clone_for_callback();
        let expand_handle = tokio::spawn(async move {
            client2.expand_variable(42, 1).await
        });

        // The client should send a "variables" request for ref 42
        let (seq, cmd, args) = recv_request(&mut pair).await;
        assert_eq!(cmd, "variables");

        // KEY ASSERTION: the request must include a `count` field
        let args = args.expect("variables should have arguments");
        assert!(
            args.get("count").is_some() && !args["count"].is_null(),
            "BUG REPRODUCED: variables request has no 'count' limit — \
             this causes LLDB to materialise all children of large collections, \
             leading to multi-GB memory usage. Got args: {}",
            args
        );

        let count = args["count"].as_i64().unwrap();
        assert!(
            count <= 100,
            "count should be reasonable (≤100), got {}",
            count
        );

        // Respond with one child (no further expansion needed)
        send_response(&pair, seq, "variables", Some(json!({
            "variables": [{
                "name": "x",
                "value": "42",
                "type": "i32",
                "variablesReference": 0
            }]
        })));

        let result = expand_handle.await.unwrap().unwrap();
        assert!(result.contains("42"));
    }

    // ========================================================================
    // Bug reproduction: evaluate timeout breaks session
    // ========================================================================

    /// Reproduce: after an evaluate times out, the adapter sends a late
    /// response.  Without the fix, the orphaned pending entry has been removed
    /// (good), but using `send_request` (no timeout) for the original request
    /// leaves it permanently in pending.  This test uses `send_request` to
    /// create a true orphan that `cancel_pending_requests` must clean up.
    ///
    /// Scenario:
    /// 1. Client sends request via `send_request` (no timeout — hangs forever)
    /// 2. Another task calls `cancel_pending_requests` (simulating recovery)
    /// 3. Client sends a new request — it must succeed
    ///
    /// Without the `cancel_pending_requests` call, the new request would
    /// work at the transport level but the orphaned entry would never be
    /// cleaned, slowly leaking memory and potentially confusing seq matching.
    #[tokio::test]
    async fn test_session_recovers_after_request_timeout() {
        let (transport, mut pair) = create_channel_transport();
        let client = DapClient::new_with_transport(transport, None)
            .await
            .unwrap();

        // 1. Send a request that will never get a response (simulates hung evaluate)
        let client_clone = client.clone_for_callback();
        let _orphan_handle = tokio::spawn(async move {
            let _ = client_clone
                .send_request("evaluate", Some(json!({"expression": "slow"})))
                .await;
        });

        // Consume the evaluate request
        let (_seq, cmd, _) = recv_request(&mut pair).await;
        assert_eq!(cmd, "evaluate");

        // Give pending entry time to register
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Verify the orphan exists
        let pending_before = client.pending_requests.read().await.len();
        assert_eq!(pending_before, 1, "Should have 1 pending request (the orphan)");

        // 2. Cancel pending requests (simulates the recovery path)
        let cancelled = client.cancel_pending_requests().await;
        assert_eq!(cancelled, 1, "Should cancel the orphaned evaluate");

        // Drain any cancel requests sent
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        while pair.outgoing_rx.try_recv().is_ok() {}

        // 3. Send a NEW request — must succeed
        let client_clone2 = client.clone_for_callback();
        let recovery_handle = tokio::spawn(async move {
            client_clone2
                .send_request_with_timeout(
                    "stackTrace",
                    Some(json!({"threadId": 1})),
                    std::time::Duration::from_secs(5),
                )
                .await
        });

        // Receive the stackTrace request
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        let received = tokio::time::timeout_at(deadline, recv_request(&mut pair)).await;

        assert!(
            received.is_ok(),
            "stackTrace request should arrive after cleanup"
        );

        let (seq2, cmd2, _) = received.unwrap();
        assert_eq!(cmd2, "stackTrace");

        send_response(&pair, seq2, "stackTrace", Some(json!({
            "stackFrames": [{"id": 1, "name": "main", "line": 10, "column": 0}]
        })));

        let recovery_result: Result<Response> = recovery_handle.await.unwrap();
        assert!(
            recovery_result.is_ok(),
            "stackTrace should succeed after cancel_pending_requests, got: {:?}",
            recovery_result.err()
        );

        // Verify no orphans remain
        let pending_after = client.pending_requests.read().await.len();
        assert_eq!(pending_after, 0, "No orphans should remain");
    }

    // ========================================================================
    // Bug reproduction: cancel_pending_requests cleans up orphans
    // ========================================================================

    /// Verify that cancel_pending_requests drops orphaned entries
    /// so they don't interfere with new requests.
    #[tokio::test]
    async fn test_cancel_pending_requests_cleans_orphans() {
        let (transport, mut pair) = create_channel_transport();
        let client = DapClient::new_with_transport(transport, None)
            .await
            .unwrap();

        // Send a request that will never get a response
        let client_clone = client.clone_for_callback();
        let _orphan_handle = tokio::spawn(async move {
            // This will hang forever since we won't respond
            let _ = client_clone
                .send_request("evaluate", Some(json!({"expression": "hang"})))
                .await;
        });

        // Consume the request
        let (_seq, cmd, _) = recv_request(&mut pair).await;
        assert_eq!(cmd, "evaluate");

        // Give the pending entry time to be registered
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Now cancel_pending_requests should clean it up
        let cancelled = client.cancel_pending_requests().await;
        assert_eq!(cancelled, 1, "Should have cancelled 1 orphaned request");

        // Consume the cancel request if sent
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        while pair.outgoing_rx.try_recv().is_ok() {}

        // Verify pending map is now empty
        let pending_count = client.pending_requests.read().await.len();
        assert_eq!(pending_count, 0, "Pending requests should be empty after cancel");
    }

    // ========================================================================
    // Bug reproduction: holon-style timeout cascade — distinguishes a slow
    // adapter (recoverable) from a dead adapter (must restart).
    // ========================================================================

    /// A 200ms evaluate timeout against a never-responding transport produces
    /// a structured `Request '...' timed out after ...` error, cleans up the
    /// orphaned pending entry, and leaves `adapter_alive` true so subsequent
    /// requests can still try. This mirrors the 8s evaluate timeout that hit
    /// in the holon incident, just compressed so the test runs fast.
    #[tokio::test]
    async fn test_tiny_eval_timeout_cleans_up_without_poisoning() {
        let (transport, mut pair) = create_channel_transport();
        let client = DapClient::new_with_transport(transport, None)
            .await
            .unwrap();

        let client_clone = client.clone_for_callback();
        let handle = tokio::spawn(async move {
            client_clone
                .send_request_with_timeout(
                    "evaluate",
                    Some(json!({"expression": "resolved_id"})),
                    std::time::Duration::from_millis(200),
                )
                .await
        });

        let (_seq, cmd, _) = recv_request(&mut pair).await;
        assert_eq!(cmd, "evaluate");

        // send_request_with_timeout: 200ms wait → DAP cancel → 1s threads
        // ping (never answered here, since this test stubs the transport) →
        // return AdapterStuck. We just await the eventual result.
        let result = handle.await.unwrap();
        let err = result.expect_err("evaluate should time out");
        match err {
            Error::AdapterStuck(msg) => {
                assert!(
                    msg.contains("timed out"),
                    "expected timeout message, got: {msg}"
                );
                assert!(
                    msg.contains("wedged"),
                    "ping was unanswered, expected 'dispatcher wedged' classifier, got: {msg}"
                );
            }
            other => panic!("expected Error::AdapterStuck timeout, got {other:?}"),
        }

        // Drain the cancel + ping that send_request_with_timeout fired.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        while pair.outgoing_rx.try_recv().is_ok() {}

        // Adapter is still alive — we just timed out. Pending map is clean.
        assert!(
            client.adapter_alive.load(Ordering::SeqCst),
            "timeout alone should not mark adapter dead"
        );
        assert_eq!(
            client.pending_requests.read().await.len(),
            0,
            "orphan must be cleaned"
        );
    }

    /// When the adapter's stdout/socket closes mid-session (process died), the
    /// reader task sets `adapter_alive=false`. A pending request that was
    /// waiting for a response surfaces this as `Error::AdapterExited` instead
    /// of the misleading `"Request cancelled or connection closed"` or the
    /// hard-to-act-on `"Write channel closed"` we used to return.
    ///
    /// This is the exact symptom from the holon incident: after evaluate
    /// timed out, `continue` returned "Write channel closed" — what we
    /// actually wanted to tell the user was "the debug adapter died".
    #[tokio::test]
    async fn test_adapter_death_surfaces_as_adapter_exited() {
        let (transport, mut pair) = create_channel_transport();
        let client = DapClient::new_with_transport(transport, None)
            .await
            .unwrap();

        // Kick off a request. Don't respond — the reader will eventually
        // see the channel close.
        let client_clone = client.clone_for_callback();
        let handle = tokio::spawn(async move {
            client_clone
                .send_request_with_timeout(
                    "continue",
                    Some(json!({"threadId": 1})),
                    std::time::Duration::from_secs(5),
                )
                .await
        });

        let (_seq, cmd, _) = recv_request(&mut pair).await;
        assert_eq!(cmd, "continue");

        // Simulate adapter death: drop both ends of the transport. The reader
        // sees the incoming channel close, the writer sees outgoing closed —
        // both flip `adapter_alive` to false.
        drop(pair);

        let result = handle.await.unwrap();
        let err = result.expect_err("continue should fail once adapter dies");
        assert!(
            matches!(err, Error::AdapterExited(_)),
            "expected AdapterExited, got: {err:?}"
        );

        // A second request after death also returns AdapterExited — not
        // a stale "Write channel closed" string.
        let second = client
            .send_request_with_timeout(
                "continue",
                Some(json!({"threadId": 1})),
                std::time::Duration::from_millis(500),
            )
            .await
            .expect_err("second request should fail with AdapterExited");
        assert!(
            matches!(second, Error::AdapterExited(_)),
            "second request should also surface AdapterExited, got: {second:?}"
        );
    }

    /// Stuck-vs-dead disambiguation: if the post-timeout `threads` ping is
    /// answered, the error classifier reports "adapter responsive after
    /// cancel" — not "wedged". Mirrors the case where the original request
    /// was stuck inside a single LLDB call that the cancel cleared, after
    /// which the adapter dispatcher is free to serve new requests.
    #[tokio::test]
    async fn test_timeout_ping_responsive_classifier() {
        let (transport, mut pair) = create_channel_transport();
        let client = DapClient::new_with_transport(transport, None)
            .await
            .unwrap();

        let client_clone = client.clone_for_callback();
        let handle = tokio::spawn(async move {
            client_clone
                .send_request_with_timeout(
                    "evaluate",
                    Some(json!({"expression": "x"})),
                    std::time::Duration::from_millis(150),
                )
                .await
        });

        // Drain the initial evaluate — don't respond, force a timeout.
        let (_seq, cmd, _) = recv_request(&mut pair).await;
        assert_eq!(cmd, "evaluate");

        // After timeout the client fires a DAP `cancel` then a `threads` ping.
        // recv_request_skipping_cancels also skips threads (for tests that fire
        // multiple user requests in a row) — here we want the threads ping,
        // so consume requests directly and look for it.
        let mut ping_seq = None;
        for _ in 0..4 {
            let (seq, cmd, _) = recv_request(&mut pair).await;
            if cmd == "threads" {
                ping_seq = Some(seq);
                break;
            }
            // "cancel" or anything else — ignore and keep waiting.
        }
        let ping_seq = ping_seq.expect("did not see threads ping after timeout");
        send_response(&pair, ping_seq, "threads", Some(json!({"threads": []})));

        let err = handle.await.unwrap().expect_err("evaluate should timeout");
        match err {
            Error::AdapterStuck(msg) => {
                assert!(
                    msg.contains("responsive after cancel"),
                    "expected 'responsive after cancel' classifier, got: {msg}"
                );
            }
            other => panic!("expected AdapterStuck, got {other:?}"),
        }
    }

    /// Multiple timeouts in a row, then a successful request — the holon
    /// "after 3 timeouts every request fails" symptom is *not* present when
    /// the adapter is still alive. This locks in the diagnosis: the cascade
    /// failure was adapter death, not a session-poisoning bug in MCP.
    #[tokio::test]
    async fn test_repeated_timeouts_do_not_poison_session() {
        let (transport, mut pair) = create_channel_transport();
        let client = DapClient::new_with_transport(transport, None)
            .await
            .unwrap();

        for _ in 0..3 {
            let client_clone = client.clone_for_callback();
            let handle = tokio::spawn(async move {
                client_clone
                    .send_request_with_timeout(
                        "evaluate",
                        Some(json!({"expression": "slow"})),
                        std::time::Duration::from_millis(150),
                    )
                    .await
            });
            let (_seq, cmd, _) = recv_request_skipping_cancels(&mut pair).await;
            assert_eq!(cmd, "evaluate");
            let res = handle.await.unwrap();
            assert!(res.is_err(), "each evaluate should time out");
        }

        // 4th request: respond to it successfully.
        let client_clone = client.clone_for_callback();
        let success_handle = tokio::spawn(async move {
            client_clone
                .send_request_with_timeout(
                    "continue",
                    Some(json!({"threadId": 1})),
                    std::time::Duration::from_secs(2),
                )
                .await
        });

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        let (seq, cmd, _) =
            tokio::time::timeout_at(deadline, recv_request_skipping_cancels(&mut pair))
                .await
                .expect("continue request must arrive after the timeouts");
        assert_eq!(cmd, "continue");
        send_response(&pair, seq, "continue", None);

        let res = success_handle.await.unwrap();
        assert!(
            res.is_ok(),
            "continue must succeed after prior timeouts — got {:?}",
            res.err()
        );
        assert!(
            client.adapter_alive.load(Ordering::SeqCst),
            "adapter should still be alive"
        );
    }
}
