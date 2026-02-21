// src/server.rs
//! MCP server that handles JSON-RPC requests over a Unix socket.
//!
//! The server maintains a Signal connection and receives messages in the background,
//! storing them in the database. Clients connect via Unix socket and send JSON-RPC
//! requests to interact with Signal.

use anyhow::{Context, Result};
use daemonize::Daemonize;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, Mutex};

use futures::StreamExt;

use crate::signal::{self, SignalEvent, SignalManager};
use crate::storage::{Contact, Message, Storage};

/// Global variable to hold the logging guard (keeps logging active)
static LOGGING_GUARD: std::sync::OnceLock<tracing_appender::non_blocking::WorkerGuard> =
    std::sync::OnceLock::new();

/// Daemonize the current process.
///
/// This function:
/// 1. Sets up file logging to `{config_dir}/signal-mcp.log`
/// 2. Writes a PID file to `{config_dir}/signal-mcp.pid`
/// 3. Forks to background and detaches from terminal
/// 4. Sets up SIGTERM handler for graceful shutdown
pub fn daemonize(config_dir: &Path) -> Result<()> {
    let pid_file = config_dir.join("signal-mcp.pid");
    let log_file = config_dir.join("signal-mcp.log");

    // Check if daemon is already running
    if pid_file.exists() {
        if let Ok(pid_str) = fs::read_to_string(&pid_file) {
            if let Ok(pid) = pid_str.trim().parse::<i32>() {
                // Check if process is still running
                if unsafe { libc::kill(pid, 0) } == 0 {
                    anyhow::bail!(
                        "Daemon already running with PID {}. Use 'signal-mcp stop' to stop it.",
                        pid
                    );
                }
            }
        }
        // Stale PID file, remove it
        let _ = fs::remove_file(&pid_file);
    }

    // Open log file for appending
    let log_file_handle = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)
        .with_context(|| format!("Failed to open log file: {:?}", log_file))?;

    let stdout = log_file_handle.try_clone()?;
    let stderr = log_file_handle;

    let daemon = Daemonize::new()
        .pid_file(&pid_file)
        .chown_pid_file(true)
        .working_directory(config_dir)
        .stdout(stdout)
        .stderr(stderr);

    daemon.start().with_context(|| "Failed to daemonize")?;

    // Set up file-based logging after daemonization
    setup_daemon_logging(config_dir);

    Ok(())
}

/// Set up logging to a file for daemon mode.
fn setup_daemon_logging(config_dir: &Path) {
    let log_file = config_dir.join("signal-mcp.log");

    let file_appender = tracing_appender::rolling::never(config_dir, "signal-mcp.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    // Store the guard in a global to keep it alive for the duration of the process
    let _ = LOGGING_GUARD.set(guard);

    // Note: We need to set a new subscriber since the default one was already set in main
    // Use try_init to avoid panic if already set
    let subscriber = tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::DEBUG.into()),
        )
        .with_ansi(false)
        .finish();

    // Set as the global default (this will replace the previous one)
    let _ = tracing::subscriber::set_global_default(subscriber);

    tracing::info!("Daemon started, logging to {:?}", log_file);
}

/// Stop a running daemon.
///
/// Reads the PID file and sends SIGTERM to the daemon process.
pub fn stop_daemon(config_dir: &Path) -> Result<()> {
    let pid_file = config_dir.join("signal-mcp.pid");

    if !pid_file.exists() {
        println!("No daemon running (PID file not found)");
        return Ok(());
    }

    let pid_str = fs::read_to_string(&pid_file)
        .with_context(|| format!("Failed to read PID file: {:?}", pid_file))?;

    let pid: i32 = pid_str
        .trim()
        .parse()
        .with_context(|| format!("Invalid PID in file: {}", pid_str))?;

    // Check if process is running
    if unsafe { libc::kill(pid, 0) } != 0 {
        println!("Daemon not running (stale PID file), cleaning up");
        let _ = fs::remove_file(&pid_file);
        return Ok(());
    }

    // Send SIGTERM
    println!("Sending SIGTERM to daemon (PID {})", pid);
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        anyhow::bail!("Failed to send SIGTERM to PID {}", pid);
    }

    // Wait for process to terminate (with timeout)
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if unsafe { libc::kill(pid, 0) } != 0 {
            println!("Daemon stopped");
            let _ = fs::remove_file(&pid_file);
            return Ok(());
        }
    }

    println!("Daemon did not stop within 5 seconds, you may need to kill it manually");
    Ok(()
    )
}

/// JSON-RPC request structure.
#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Option<serde_json::Value>,
    method: String,
    params: Option<serde_json::Value>,
}

/// JSON-RPC response structure.
#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

/// JSON-RPC error structure.
#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

impl JsonRpcResponse {
    fn success(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    fn error(id: Option<serde_json::Value>, code: i32, message: String) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError { code, message }),
        }
    }
}

/// Status response for get_status RPC.
#[derive(Debug, Serialize)]
struct StatusResponse {
    connected: bool,
    account_uuid: String,
}

/// Parameters for send_message RPC.
#[derive(Debug, Deserialize)]
struct SendMessageParams {
    recipient: String,
    message: String,
}

/// Parameters for list_contacts RPC.
#[derive(Debug, Deserialize)]
struct ListContactsParams {
    filter: Option<String>,
}

/// Parameters for get_messages RPC.
#[derive(Debug, Deserialize)]
struct GetMessagesParams {
    contact: String,
    limit: Option<i64>,
}

/// Parameters for list_groups RPC.
#[derive(Debug, Deserialize)]
struct ListGroupsParams {
    filter: Option<String>,
}

/// Result of a contacts sync operation.
#[derive(Debug, Clone)]
pub struct SyncResult {
    pub contacts_count: usize,
    pub groups_count: usize,
}

/// Command sent to the Signal manager task.
enum ManagerCommand {
    SendMessage {
        recipient_uuid: String,
        message: String,
        reply: oneshot::Sender<Result<u64, String>>,
    },
    SendGroupMessage {
        group_id: String,
        message: String,
        reply: oneshot::Sender<Result<u64, String>>,
    },
    SyncContacts {
        storage: Arc<Mutex<Storage>>,
        reply: oneshot::Sender<Result<SyncResult, String>>,
    },
}

/// Handle the MCP initialize handshake on stdio before the heavy Signal setup.
///
/// Claude Code sends `initialize` immediately and expects a fast response.
/// This handles `initialize` and `notifications/initialized` synchronously
/// on stdin/stdout so the server is registered before `Server::new()` runs.
pub async fn mcp_handshake_stdio() -> Result<BufReader<tokio::io::Stdin>> {
    use tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut stdout = stdout();
    let mut stdin_reader = BufReader::new(stdin());
    let mut line = String::new();

    // Read lines until we've handled initialize + notifications/initialized
    let mut initialized = false;
    let mut notified = false;

    while !initialized || !notified {
        line.clear();
        let bytes_read = stdin_reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            anyhow::bail!("EOF before MCP handshake completed");
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(request) = serde_json::from_str::<JsonRpcRequest>(trimmed) {
            match request.method.as_str() {
                "initialize" => {
                    let response = rpc_initialize(request.id);
                    let json = serde_json::to_string(&response)?;
                    stdout.write_all(json.as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                    stdout.flush().await?;
                    initialized = true;
                }
                "notifications/initialized" => {
                    // Notification — no response needed
                    notified = true;
                }
                _ => {
                    // Unexpected method during handshake — respond with error
                    let response = JsonRpcResponse::error(
                        request.id,
                        -32601,
                        format!("Server initializing, method not available yet: {}", request.method),
                    );
                    let json = serde_json::to_string(&response)?;
                    stdout.write_all(json.as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                    stdout.flush().await?;
                }
            }
        }
    }

    // Return the BufReader so run_stdio can reuse it (avoids losing buffered data)
    Ok(stdin_reader)
}

/// The MCP server.
pub struct Server {
    socket_path: std::path::PathBuf,
    storage: Arc<Mutex<Storage>>,
    manager: SignalManager,
    account_uuid: String,
}

impl Server {
    /// Create a new server instance.
    ///
    /// # Arguments
    ///
    /// * `config_dir` - The configuration directory containing signal.db and signal-mcp.db
    pub async fn new(config_dir: &Path) -> Result<Self> {
        let store_path = config_dir.join("signal.db");
        let db_path = config_dir.join("signal-mcp.db");
        let socket_path = config_dir.join("signal-mcp.sock");

        // Clean up stale socket
        let _ = std::fs::remove_file(&socket_path);

        let storage = Storage::open(&db_path)?;
        let manager = signal::open_manager(&store_path).await?;
        let account_uuid = signal::get_account_uuid(&manager);

        Ok(Self {
            socket_path,
            storage: Arc::new(Mutex::new(storage)),
            manager,
            account_uuid,
        })
    }

    /// Run the server main loop.
    ///
    /// This will:
    /// 1. Listen on the Unix socket for client connections
    /// 2. Spawn a background task to receive Signal messages
    /// 3. Handle both client requests and Signal events concurrently
    /// 4. Handle SIGTERM for graceful shutdown (daemon mode)
    pub async fn run(mut self) -> Result<()> {
        let listener = UnixListener::bind(&self.socket_path)?;
        tracing::info!("Server listening on {:?}", self.socket_path);
        tracing::info!("Connected as {}", self.account_uuid);

        // Channel for manager commands (from clients to main loop)
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<ManagerCommand>(100);

        // Channel for incoming Signal events from the receive task
        let (event_tx, mut event_rx) = mpsc::channel::<SignalEvent>(100);

        // Spawn receive loop on a dedicated thread (stream is !Send)
        // Auto-reconnects with exponential backoff on disconnect.
        let mut receive_manager = self.manager.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to build receive runtime");
            rt.block_on(async move {
                let mut backoff_secs = 1u64;
                loop {
                    tracing::info!("Starting Signal message receive loop...");
                    match signal::start_receiving(&mut receive_manager).await {
                        Ok(mut stream) => {
                            backoff_secs = 1; // reset on successful connect
                            loop {
                                match stream.next().await {
                                    Some(event) => {
                                        if event_tx.send(event).await.is_err() {
                                            tracing::debug!("Event channel closed, stopping receive loop");
                                            return;
                                        }
                                    }
                                    None => {
                                        tracing::warn!("Signal receive stream ended, reconnecting in {}s...", backoff_secs);
                                        break;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to start receiving: {}, retrying in {}s...", e, backoff_secs);
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(60);
                }
            });
        });

        // Set up SIGTERM handler for graceful shutdown
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

        // Main event loop
        loop {
            tokio::select! {
                // Handle SIGTERM (graceful shutdown)
                _ = sigterm.recv() => {
                    tracing::info!("Received SIGTERM, shutting down gracefully...");
                    break;
                }

                // Handle incoming Signal events
                Some(event) = event_rx.recv() => {
                    if let Err(e) = self.handle_signal_event(event).await {
                        tracing::error!("Error handling signal event: {}", e);
                    }
                }

                // Handle incoming client connections
                result = listener.accept() => {
                    match result {
                        Ok((stream, _)) => {
                            let storage = Arc::clone(&self.storage);
                            let cmd_tx = cmd_tx.clone();
                            let account_uuid = self.account_uuid.clone();

                            tokio::spawn(async move {
                                if let Err(e) =
                                    handle_client(stream, storage, cmd_tx, account_uuid).await
                                {
                                    tracing::error!("Client handler error: {}", e);
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Accept error: {}", e);
                        }
                    }
                }

                // Handle manager commands from clients
                Some(cmd) = cmd_rx.recv() => {
                    match cmd {
                        ManagerCommand::SendMessage { recipient_uuid, message, reply } => {
                            let result = signal::send_message(&mut self.manager, &recipient_uuid, &message).await;
                            let _ = reply.send(result.map_err(|e| e.to_string()));
                        }
                        ManagerCommand::SendGroupMessage { group_id, message, reply } => {
                            let result = signal::send_group_message(&mut self.manager, &group_id, &message).await;
                            let _ = reply.send(result.map_err(|e| e.to_string()));
                        }
                        ManagerCommand::SyncContacts { storage, reply } => {
                            tracing::info!("Starting contacts sync...");
                            let result = signal::request_contacts_sync(&mut self.manager, &storage).await;
                            if let Ok((c, g)) = &result {
                                tracing::info!("Sync completed: {} contacts, {} groups", c, g);
                            }
                            let _ = reply.send(result.map(|(contacts_count, groups_count)| {
                                SyncResult { contacts_count, groups_count }
                            }).map_err(|e| e.to_string()));
                        }
                    }
                }
            }
        }

        // Clean up socket file on shutdown
        let _ = std::fs::remove_file(&self.socket_path);
        tracing::info!("Server shutdown complete");
        Ok(())
    }

    /// Run the server in stdio mode for MCP clients.
    ///
    /// This reads JSON-RPC requests from stdin and writes responses to stdout,
    /// instead of using a Unix socket. This is the standard transport for MCP
    /// (Model Context Protocol) clients like Claude.
    pub async fn run_stdio(mut self, stdin_reader: Option<BufReader<tokio::io::Stdin>>) -> Result<()> {
        use tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncWriteExt, BufReader};

        let mut stdout = stdout();
        let mut stdin_reader = stdin_reader.unwrap_or_else(|| BufReader::new(stdin()));
        let mut line = String::new();

        // Channel for manager commands (from request handler to main loop)
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<ManagerCommand>(100);

        // Channel for incoming Signal events from the receive task
        let (event_tx, mut event_rx) = mpsc::channel::<SignalEvent>(100);

        // Spawn receive loop on a dedicated thread (stream is !Send)
        // Auto-reconnects with exponential backoff on disconnect.
        let mut receive_manager = self.manager.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to build receive runtime");
            rt.block_on(async move {
                let mut backoff_secs = 1u64;
                loop {
                    tracing::info!("Starting Signal message receive loop (stdio)...");
                    match signal::start_receiving(&mut receive_manager).await {
                        Ok(mut stream) => {
                            backoff_secs = 1;
                            loop {
                                match stream.next().await {
                                    Some(event) => {
                                        if event_tx.send(event).await.is_err() {
                                            tracing::debug!("Event channel closed, stopping receive loop");
                                            return;
                                        }
                                    }
                                    None => {
                                        tracing::warn!("Signal receive stream ended, reconnecting in {}s...", backoff_secs);
                                        break;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to start receiving: {}, retrying in {}s...", e, backoff_secs);
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(60);
                }
            });
        });

        loop {
            tokio::select! {
                // Handle incoming Signal events
                Some(event) = event_rx.recv() => {
                    if let Err(e) = self.handle_signal_event(event).await {
                        tracing::error!("Error handling signal event: {}", e);
                    }
                }

                // Handle stdin input
                result = stdin_reader.read_line(&mut line) => {
                    match result {
                        Ok(0) => {
                            // EOF - stdin closed
                            break;
                        }
                        Ok(_) => {
                            let trimmed = line.trim();
                            if !trimmed.is_empty() {
                                match serde_json::from_str::<JsonRpcRequest>(trimmed) {
                                    Ok(request) => {
                                        // Notifications have no id — don't send a response
                                        let is_notification = request.id.is_none();

                                        // In stdio mode, handle manager commands
                                        // directly to avoid deadlock — the channel
                                        // consumer is in this same select loop, so
                                        // sending a command and awaiting the reply
                                        // would block forever.
                                        let needs_manager = Self::request_needs_manager(&request);
                                        let response = if needs_manager {
                                            self.handle_manager_request_direct(request).await
                                        } else {
                                            handle_request(
                                                request,
                                                &self.storage,
                                                &cmd_tx,
                                                &self.account_uuid,
                                            ).await
                                        };

                                        if !is_notification {
                                            let response_json = serde_json::to_string(&response)?;
                                            stdout.write_all(response_json.as_bytes()).await?;
                                            stdout.write_all(b"\n").await?;
                                            stdout.flush().await?;
                                        }
                                    }
                                    Err(e) => {
                                        let response = JsonRpcResponse::error(
                                            None,
                                            -32700,
                                            format!("Parse error: {}", e),
                                        );
                                        let response_json = serde_json::to_string(&response)?;
                                        stdout.write_all(response_json.as_bytes()).await?;
                                        stdout.write_all(b"\n").await?;
                                        stdout.flush().await?;
                                    }
                                };
                            }
                            line.clear();
                        }
                        Err(e) => {
                            tracing::error!("Error reading stdin: {}", e);
                            break;
                        }
                    }
                }

                // Handle manager commands
                Some(cmd) = cmd_rx.recv() => {
                    match cmd {
                        ManagerCommand::SendMessage { recipient_uuid, message, reply } => {
                            let result = signal::send_message(&mut self.manager, &recipient_uuid, &message).await;
                            let _ = reply.send(result.map_err(|e| e.to_string()));
                        }
                        ManagerCommand::SendGroupMessage { group_id, message, reply } => {
                            let result = signal::send_group_message(&mut self.manager, &group_id, &message).await;
                            let _ = reply.send(result.map_err(|e| e.to_string()));
                        }
                        ManagerCommand::SyncContacts { storage, reply } => {
                            let result = signal::request_contacts_sync(&mut self.manager, &storage).await;
                            let _ = reply.send(result.map(|(contacts_count, groups_count)| {
                                SyncResult { contacts_count, groups_count }
                            }).map_err(|e| e.to_string()));
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Check if a request needs the Signal manager (and would deadlock via channel).
    fn request_needs_manager(request: &JsonRpcRequest) -> bool {
        if request.method == "send_message" || request.method == "sync_contacts" {
            return true;
        }
        if request.method == "tools/call" {
            if let Some(params) = &request.params {
                if let Some(name) = params.get("name").and_then(|v| v.as_str()) {
                    return matches!(name, "send_message" | "sync_contacts");
                }
            }
        }
        false
    }

    /// Handle requests that need the Signal manager directly (for stdio mode).
    ///
    /// This bypasses the channel to avoid deadlock — in stdio mode the channel
    /// consumer lives in the same select loop as the stdin handler, so sending
    /// a command and awaiting its reply would block forever.
    async fn handle_manager_request_direct(
        &mut self,
        request: JsonRpcRequest,
    ) -> JsonRpcResponse {
        // Extract the actual tool name and arguments
        let (tool_name, arguments, request_id) = if request.method == "tools/call" {
            // MCP tools/call — extract name and arguments from params
            let params = match &request.params {
                Some(p) => p.clone(),
                None => {
                    return JsonRpcResponse::error(
                        request.id,
                        -32602,
                        "Missing params".to_string(),
                    )
                }
            };
            let name = match params.get("name").and_then(|v| v.as_str()) {
                Some(n) => n.to_string(),
                None => {
                    return JsonRpcResponse::error(
                        request.id,
                        -32602,
                        "Missing tool name".to_string(),
                    )
                }
            };
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            (name, args, request.id)
        } else {
            // Direct RPC call (e.g. method="send_message")
            let args = request.params.unwrap_or(serde_json::json!({}));
            (request.method, args, request.id)
        };

        tracing::info!("handle_manager_request_direct: {}", tool_name);

        match tool_name.as_str() {
            "send_message" => {
                self.send_message_direct(request_id, arguments).await
            }
            "sync_contacts" => {
                self.sync_contacts_direct(request_id).await
            }
            _ => JsonRpcResponse::error(
                request_id,
                -32601,
                format!("Unknown manager method: {}", tool_name),
            ),
        }
    }

    /// Send a message directly using the manager (no channel).
    async fn send_message_direct(
        &mut self,
        id: Option<serde_json::Value>,
        arguments: serde_json::Value,
    ) -> JsonRpcResponse {
        let params: SendMessageParams = match serde_json::from_value(arguments) {
            Ok(params) => params,
            Err(e) => {
                return JsonRpcResponse::error(id, -32602, format!("Invalid params: {}", e))
            }
        };

        // Resolve recipient
        let resolved = {
            let storage = self.storage.lock().await;
            match storage.resolve_recipient(&params.recipient) {
                Ok(Some(uuid)) => ResolvedRecipient::Contact(uuid),
                Ok(None) => match storage.find_group(&params.recipient) {
                    Ok(Some(group_id)) => ResolvedRecipient::Group(group_id),
                    Ok(None) => {
                        return JsonRpcResponse::error(
                            id,
                            -32000,
                            format!("Could not resolve recipient: {}", params.recipient),
                        )
                    }
                    Err(e) => {
                        return JsonRpcResponse::error(
                            id,
                            -32000,
                            format!("Error resolving recipient: {}", e),
                        )
                    }
                },
                Err(e) => {
                    return JsonRpcResponse::error(
                        id,
                        -32000,
                        format!("Error resolving recipient: {}", e),
                    )
                }
            }
        };

        // Send directly using the manager
        let (timestamp, recipient_id, is_group) = match &resolved {
            ResolvedRecipient::Contact(uuid) => {
                match signal::send_message(&mut self.manager, uuid, &params.message).await {
                    Ok(ts) => (ts, uuid.clone(), false),
                    Err(e) => {
                        return JsonRpcResponse::error(
                            id,
                            -32000,
                            format!("Failed to send message: {}", e),
                        )
                    }
                }
            }
            ResolvedRecipient::Group(group_id) => {
                match signal::send_group_message(&mut self.manager, group_id, &params.message).await
                {
                    Ok(ts) => (ts, group_id.clone(), true),
                    Err(e) => {
                        return JsonRpcResponse::error(
                            id,
                            -32000,
                            format!("Failed to send group message: {}", e),
                        )
                    }
                }
            }
        };

        // Store the outgoing message
        if !is_group {
            let storage = self.storage.lock().await;
            if let Err(e) = storage.insert_message(&Message {
                id: 0,
                contact_uuid: recipient_id.clone(),
                direction: "out".to_string(),
                body: params.message.clone(),
                timestamp: timestamp as i64,
            }) {
                tracing::warn!("Failed to store outgoing message: {}", e);
            }
        }

        // For tools/call, wrap in MCP content array format
        let result = serde_json::json!({
            "content": [{
                "type": "text",
                "text": serde_json::json!({
                    "success": true,
                    "timestamp": timestamp,
                    "recipient": recipient_id,
                }).to_string()
            }]
        });

        JsonRpcResponse::success(id, result)
    }

    /// Sync contacts directly using the manager (no channel).
    async fn sync_contacts_direct(
        &mut self,
        id: Option<serde_json::Value>,
    ) -> JsonRpcResponse {
        let storage = self.storage.clone();
        match signal::request_contacts_sync(&mut self.manager, &storage).await {
            Ok((contacts_count, groups_count)) => {
                let result = serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": serde_json::json!({
                            "success": true,
                            "contacts_synced": contacts_count,
                            "groups_synced": groups_count,
                        }).to_string()
                    }]
                });
                JsonRpcResponse::success(id, result)
            }
            Err(e) => JsonRpcResponse::error(
                id,
                -32000,
                format!("Failed to sync contacts: {}", e),
            ),
        }
    }

    /// Handle Signal events by storing them in the database.
    async fn handle_signal_event(&self, event: SignalEvent) -> Result<()> {
        match event {
            SignalEvent::Message {
                sender_uuid,
                body,
                timestamp,
            } => {
                tracing::info!("Received message from {}: {}", sender_uuid, body);

                let storage = self.storage.lock().await;

                // Ensure sender exists as a contact
                storage.upsert_contact(&Contact {
                    uuid: sender_uuid.clone(),
                    phone: None,
                    name: None,
                })?;

                // Store the message
                storage.insert_message(&Message {
                    id: 0, // Will be assigned by the database
                    contact_uuid: sender_uuid,
                    direction: "in".to_string(),
                    body,
                    timestamp: timestamp as i64,
                })?;
            }
            SignalEvent::Contact { uuid, phone, name } => {
                tracing::info!("Contact sync: {} ({:?})", uuid, name);

                let storage = self.storage.lock().await;
                storage.upsert_contact(&Contact { uuid, phone, name })?;
            }
            SignalEvent::QueueEmpty => {
                tracing::info!("Message queue empty (initial sync complete)");
            }
            SignalEvent::ContactsSync => {
                tracing::info!("Contacts synchronized");
            }
        }

        Ok(())
    }
}

/// Handle a client connection.
async fn handle_client(
    stream: UnixStream,
    storage: Arc<Mutex<Storage>>,
    cmd_tx: mpsc::Sender<ManagerCommand>,
    account_uuid: String,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line).await?;

        if bytes_read == 0 {
            // Client disconnected
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<JsonRpcRequest>(trimmed) {
            Ok(request) => handle_request(request, &storage, &cmd_tx, &account_uuid).await,
            Err(e) => JsonRpcResponse::error(None, -32700, format!("Parse error: {}", e)),
        };

        let response_json = serde_json::to_string(&response)?;
        writer.write_all(response_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }

    Ok(())
}

/// Handle a JSON-RPC request.
async fn handle_request(
    request: JsonRpcRequest,
    storage: &Arc<Mutex<Storage>>,
    cmd_tx: &mpsc::Sender<ManagerCommand>,
    account_uuid: &str,
) -> JsonRpcResponse {
    // Validate JSON-RPC version
    if request.jsonrpc != "2.0" {
        return JsonRpcResponse::error(
            request.id,
            -32600,
            "Invalid Request: jsonrpc must be \"2.0\"".to_string(),
        );
    }

    match request.method.as_str() {
        "send_message" => rpc_send_message(request.id, request.params, storage, cmd_tx).await,
        "list_contacts" => rpc_list_contacts(request.id, request.params, storage).await,
        "list_groups" => rpc_list_groups(request.id, request.params, storage).await,
        "sync_contacts" => rpc_sync_contacts(request.id, storage, cmd_tx).await,
        "get_messages" => rpc_get_messages(request.id, request.params, storage).await,
        "get_conversations" => rpc_get_conversations(request.id, request.params, storage).await,
        "get_status" => rpc_get_status(request.id, account_uuid),
        "initialize" => rpc_initialize(request.id),
        "notifications/initialized" => JsonRpcResponse::success(request.id, serde_json::json!({})),
        "tools/list" => rpc_tools_list(request.id),
        "tools/call" => rpc_tools_call(request.id, request.params, storage, cmd_tx, account_uuid).await,
        _ => JsonRpcResponse::error(
            request.id,
            -32601,
            format!("Method not found: {}", request.method),
        ),
    }
}

/// Resolved recipient - either a contact UUID or a group ID.
enum ResolvedRecipient {
    Contact(String),
    Group(String),
}

/// RPC: send_message - Send a message via Signal.
async fn rpc_send_message(
    id: Option<serde_json::Value>,
    params: Option<serde_json::Value>,
    storage: &Arc<Mutex<Storage>>,
    cmd_tx: &mpsc::Sender<ManagerCommand>,
) -> JsonRpcResponse {
    let params: SendMessageParams = match params {
        Some(p) => match serde_json::from_value(p) {
            Ok(params) => params,
            Err(e) => {
                return JsonRpcResponse::error(id, -32602, format!("Invalid params: {}", e))
            }
        },
        None => return JsonRpcResponse::error(id, -32602, "Missing params".to_string()),
    };

    // Resolve recipient - try contact first, then group
    let resolved = {
        let storage = storage.lock().await;

        // Try to resolve as contact (UUID, phone, name)
        tracing::debug!("Resolving recipient: {}", params.recipient);
        match storage.resolve_recipient(&params.recipient) {
            Ok(Some(uuid)) => {
                tracing::debug!("Resolved as contact: {}", uuid);
                ResolvedRecipient::Contact(uuid)
            }
            Ok(None) => {
                tracing::debug!("Not a contact, trying group...");
                // Try to resolve as group (ID or name)
                match storage.find_group(&params.recipient) {
                    Ok(Some(group_id)) => {
                        tracing::debug!("Resolved as group: {}", group_id);
                        ResolvedRecipient::Group(group_id)
                    }
                    Ok(None) => {
                        tracing::debug!("Not found as contact or group");
                        return JsonRpcResponse::error(
                            id,
                            -32000,
                            format!("Could not resolve recipient: {}", params.recipient),
                        )
                    }
                    Err(e) => {
                        return JsonRpcResponse::error(
                            id,
                            -32000,
                            format!("Error resolving recipient: {}", e),
                        )
                    }
                }
            }
            Err(e) => {
                return JsonRpcResponse::error(
                    id,
                    -32000,
                    format!("Error resolving recipient: {}", e),
                )
            }
        }
    };

    // Send command to manager task based on recipient type
    let (reply_tx, reply_rx) = oneshot::channel();
    let (cmd, recipient_id, is_group) = match &resolved {
        ResolvedRecipient::Contact(uuid) => (
            ManagerCommand::SendMessage {
                recipient_uuid: uuid.clone(),
                message: params.message.clone(),
                reply: reply_tx,
            },
            uuid.clone(),
            false,
        ),
        ResolvedRecipient::Group(group_id) => (
            ManagerCommand::SendGroupMessage {
                group_id: group_id.clone(),
                message: params.message.clone(),
                reply: reply_tx,
            },
            group_id.clone(),
            true,
        ),
    };

    if cmd_tx.send(cmd).await.is_err() {
        return JsonRpcResponse::error(id, -32000, "Server shutting down".to_string());
    }

    let timestamp = match reply_rx.await {
        Ok(Ok(ts)) => ts,
        Ok(Err(e)) => {
            return JsonRpcResponse::error(id, -32000, format!("Failed to send message: {}", e))
        }
        Err(_) => {
            return JsonRpcResponse::error(id, -32000, "Server error: channel closed".to_string())
        }
    };

    // Store the outgoing message (only for contacts, not groups for now)
    if !is_group {
        let storage = storage.lock().await;
        if let Err(e) = storage.insert_message(&Message {
            id: 0,
            contact_uuid: recipient_id.clone(),
            direction: "out".to_string(),
            body: params.message,
            timestamp: timestamp as i64,
        }) {
            tracing::warn!("Failed to store outgoing message: {}", e);
        }
    }

    let mut response = serde_json::json!({
        "success": true,
        "timestamp": timestamp,
    });

    if is_group {
        response["group_id"] = serde_json::json!(recipient_id);
    } else {
        response["recipient_uuid"] = serde_json::json!(recipient_id);
    }

    JsonRpcResponse::success(id, response)
}

/// RPC: list_contacts - List contacts from storage.
async fn rpc_list_contacts(
    id: Option<serde_json::Value>,
    params: Option<serde_json::Value>,
    storage: &Arc<Mutex<Storage>>,
) -> JsonRpcResponse {
    let filter = params
        .and_then(|p| serde_json::from_value::<ListContactsParams>(p).ok())
        .and_then(|p| p.filter);

    let storage = storage.lock().await;
    match storage.list_contacts(filter.as_deref()) {
        Ok(contacts) => JsonRpcResponse::success(id, serde_json::to_value(contacts).unwrap()),
        Err(e) => JsonRpcResponse::error(id, -32000, format!("Failed to list contacts: {}", e)),
    }
}

/// RPC: list_groups - List groups from storage.
async fn rpc_list_groups(
    id: Option<serde_json::Value>,
    params: Option<serde_json::Value>,
    storage: &Arc<Mutex<Storage>>,
) -> JsonRpcResponse {
    let filter = params
        .and_then(|p| serde_json::from_value::<ListGroupsParams>(p).ok())
        .and_then(|p| p.filter);

    let storage = storage.lock().await;
    match storage.list_groups(filter.as_deref()) {
        Ok(groups) => JsonRpcResponse::success(id, serde_json::to_value(groups).unwrap()),
        Err(e) => JsonRpcResponse::error(id, -32000, format!("Failed to list groups: {}", e)),
    }
}

/// RPC: sync_contacts - Request contacts sync from primary device.
async fn rpc_sync_contacts(
    id: Option<serde_json::Value>,
    storage: &Arc<Mutex<Storage>>,
    cmd_tx: &mpsc::Sender<ManagerCommand>,
) -> JsonRpcResponse {
    let (reply_tx, reply_rx) = oneshot::channel();
    let cmd = ManagerCommand::SyncContacts {
        storage: Arc::clone(storage),
        reply: reply_tx,
    };

    if cmd_tx.send(cmd).await.is_err() {
        return JsonRpcResponse::error(id, -32000, "Server shutting down".to_string());
    }

    match reply_rx.await {
        Ok(Ok(result)) => JsonRpcResponse::success(
            id,
            serde_json::json!({
                "success": true,
                "contacts_count": result.contacts_count,
                "groups_count": result.groups_count,
                "message": format!("Synced {} contacts and {} groups", result.contacts_count, result.groups_count)
            }),
        ),
        Ok(Err(e)) => {
            JsonRpcResponse::error(id, -32000, format!("Failed to request contacts sync: {}", e))
        }
        Err(_) => {
            JsonRpcResponse::error(id, -32000, "Server error: channel closed".to_string())
        }
    }
}

/// RPC: get_messages - Get message history for a contact.
async fn rpc_get_messages(
    id: Option<serde_json::Value>,
    params: Option<serde_json::Value>,
    storage: &Arc<Mutex<Storage>>,
) -> JsonRpcResponse {
    let params: GetMessagesParams = match params {
        Some(p) => match serde_json::from_value(p) {
            Ok(params) => params,
            Err(e) => {
                return JsonRpcResponse::error(id, -32602, format!("Invalid params: {}", e))
            }
        },
        None => return JsonRpcResponse::error(id, -32602, "Missing params".to_string()),
    };

    let limit = params.limit.unwrap_or(50);

    let storage = storage.lock().await;

    // Resolve contact to UUID
    let contact_uuid = match storage.resolve_recipient(&params.contact) {
        Ok(Some(uuid)) => uuid,
        Ok(None) => {
            return JsonRpcResponse::error(
                id,
                -32000,
                format!("Could not resolve contact: {}", params.contact),
            )
        }
        Err(e) => {
            return JsonRpcResponse::error(id, -32000, format!("Error resolving contact: {}", e))
        }
    };

    match storage.get_messages(&contact_uuid, limit) {
        Ok(messages) => JsonRpcResponse::success(id, serde_json::to_value(messages).unwrap()),
        Err(e) => JsonRpcResponse::error(id, -32000, format!("Failed to get messages: {}", e)),
    }
}

/// RPC: get_status - Get server connection status.
fn rpc_get_status(id: Option<serde_json::Value>, account_uuid: &str) -> JsonRpcResponse {
    let status = StatusResponse {
        connected: true,
        account_uuid: account_uuid.to_string(),
    };

    JsonRpcResponse::success(id, serde_json::to_value(status).unwrap())
}

/// RPC: get_conversations - List recent conversations.
async fn rpc_get_conversations(
    id: Option<serde_json::Value>,
    params: Option<serde_json::Value>,
    storage: &Arc<Mutex<Storage>>,
) -> JsonRpcResponse {
    #[derive(Deserialize, Default)]
    struct Params {
        #[serde(default = "default_conv_limit")]
        limit: i64,
    }
    fn default_conv_limit() -> i64 { 20 }

    let params: Params = params
        .map(serde_json::from_value)
        .transpose()
        .ok()
        .flatten()
        .unwrap_or_default();

    let storage = storage.lock().await;
    match storage.get_conversations(params.limit) {
        Ok(conversations) => JsonRpcResponse::success(id, serde_json::to_value(conversations).unwrap()),
        Err(e) => JsonRpcResponse::error(id, -32000, format!("Failed to get conversations: {}", e)),
    }
}

/// RPC: initialize - MCP protocol handshake.
fn rpc_initialize(id: Option<serde_json::Value>) -> JsonRpcResponse {
    JsonRpcResponse::success(id, serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "signal-mcp", "version": "0.1.0" }
    }))
}

/// RPC: tools/list - List available MCP tools.
fn rpc_tools_list(id: Option<serde_json::Value>) -> JsonRpcResponse {
    let tools = serde_json::json!({
        "tools": [
            {
                "name": "send_message",
                "description": "Send a Signal message to a contact",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "recipient": {
                            "type": "string",
                            "description": "Phone number (+1234...), contact name, or UUID"
                        },
                        "message": {
                            "type": "string",
                            "description": "Message text to send"
                        }
                    },
                    "required": ["recipient", "message"]
                }
            },
            {
                "name": "list_contacts",
                "description": "List Signal contacts",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "filter": {
                            "type": "string",
                            "description": "Optional search filter for name or phone"
                        }
                    }
                }
            },
            {
                "name": "get_messages",
                "description": "Get message history with a contact",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "contact": {
                            "type": "string",
                            "description": "Phone number, contact name, or UUID"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max messages to return (default 50)"
                        }
                    },
                    "required": ["contact"]
                }
            },
            {
                "name": "get_conversations",
                "description": "List recent conversations",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "limit": {
                            "type": "integer",
                            "description": "Max conversations to return (default 20)"
                        }
                    }
                }
            },
            {
                "name": "get_status",
                "description": "Get server connection status",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "sync_contacts",
                "description": "Request contacts sync from the primary device",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "list_groups",
                "description": "List Signal groups",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "filter": {
                            "type": "string",
                            "description": "Optional search filter for group name"
                        }
                    }
                }
            }
        ]
    });

    JsonRpcResponse::success(id, tools)
}

/// RPC: tools/call - Invoke an MCP tool.
async fn rpc_tools_call(
    id: Option<serde_json::Value>,
    params: Option<serde_json::Value>,
    storage: &Arc<Mutex<Storage>>,
    cmd_tx: &mpsc::Sender<ManagerCommand>,
    account_uuid: &str,
) -> JsonRpcResponse {
    #[derive(Deserialize)]
    struct Params {
        name: String,
        arguments: Option<serde_json::Value>,
    }

    let params: Params = match params {
        Some(p) => match serde_json::from_value(p) {
            Ok(params) => params,
            Err(e) => {
                return JsonRpcResponse::error(id, -32602, format!("Invalid params: {}", e))
            }
        },
        None => return JsonRpcResponse::error(id, -32602, "Missing params".to_string()),
    };

    let arguments = params.arguments.unwrap_or(serde_json::json!({}));

    let result = match params.name.as_str() {
        "send_message" => {
            let response = rpc_send_message(None, Some(arguments), storage, cmd_tx).await;
            match response.result {
                Some(r) => r,
                None => return JsonRpcResponse::error(
                    id,
                    -32000,
                    response.error.map(|e| e.message).unwrap_or_else(|| "Unknown error".to_string()),
                ),
            }
        }
        "list_contacts" => {
            let response = rpc_list_contacts(None, Some(arguments), storage).await;
            match response.result {
                Some(r) => r,
                None => return JsonRpcResponse::error(
                    id,
                    -32000,
                    response.error.map(|e| e.message).unwrap_or_else(|| "Unknown error".to_string()),
                ),
            }
        }
        "get_messages" => {
            let response = rpc_get_messages(None, Some(arguments), storage).await;
            match response.result {
                Some(r) => r,
                None => return JsonRpcResponse::error(
                    id,
                    -32000,
                    response.error.map(|e| e.message).unwrap_or_else(|| "Unknown error".to_string()),
                ),
            }
        }
        "get_conversations" => {
            let response = rpc_get_conversations(None, Some(arguments), storage).await;
            match response.result {
                Some(r) => r,
                None => return JsonRpcResponse::error(
                    id,
                    -32000,
                    response.error.map(|e| e.message).unwrap_or_else(|| "Unknown error".to_string()),
                ),
            }
        }
        "get_status" => {
            let response = rpc_get_status(None, account_uuid);
            match response.result {
                Some(r) => r,
                None => return JsonRpcResponse::error(
                    id,
                    -32000,
                    response.error.map(|e| e.message).unwrap_or_else(|| "Unknown error".to_string()),
                ),
            }
        }
        "sync_contacts" => {
            let response = rpc_sync_contacts(None, storage, cmd_tx).await;
            match response.result {
                Some(r) => r,
                None => return JsonRpcResponse::error(
                    id,
                    -32000,
                    response.error.map(|e| e.message).unwrap_or_else(|| "Unknown error".to_string()),
                ),
            }
        }
        "list_groups" => {
            let response = rpc_list_groups(None, Some(arguments), storage).await;
            match response.result {
                Some(r) => r,
                None => return JsonRpcResponse::error(
                    id,
                    -32000,
                    response.error.map(|e| e.message).unwrap_or_else(|| "Unknown error".to_string()),
                ),
            }
        }
        _ => return JsonRpcResponse::error(id, -32000, format!("Unknown tool: {}", params.name)),
    };

    // MCP tools/call returns content array
    let content = serde_json::json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string())
        }]
    });

    JsonRpcResponse::success(id, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_rpc_response_success() {
        let response = JsonRpcResponse::success(
            Some(serde_json::json!(1)),
            serde_json::json!({"ok": true}),
        );
        assert_eq!(response.jsonrpc, "2.0");
        assert!(response.result.is_some());
        assert!(response.error.is_none());
    }

    #[test]
    fn test_json_rpc_response_error() {
        let response = JsonRpcResponse::error(
            Some(serde_json::json!(1)),
            -32600,
            "Invalid Request".to_string(),
        );
        assert_eq!(response.jsonrpc, "2.0");
        assert!(response.result.is_none());
        assert!(response.error.is_some());
        assert_eq!(response.error.as_ref().unwrap().code, -32600);
    }
}
