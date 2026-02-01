// src/server.rs
//! MCP server that handles JSON-RPC requests over a Unix socket.
//!
//! The server maintains a Signal connection and receives messages in the background,
//! storing them in the database. Clients connect via Unix socket and send JSON-RPC
//! requests to interact with Signal.

use anyhow::{Context, Result};
use daemonize::Daemonize;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, Mutex};

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
                .add_directive(tracing::Level::INFO.into()),
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

/// Command sent to the Signal manager task.
enum ManagerCommand {
    SendMessage {
        recipient_uuid: String,
        message: String,
        reply: oneshot::Sender<Result<u64, String>>,
    },
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
    pub async fn run_stdio(mut self) -> Result<()> {
        use tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncWriteExt, BufReader};

        let mut stdout = stdout();
        let mut stdin_reader = BufReader::new(stdin());
        let mut line = String::new();

        // Channel for manager commands (from request handler to main loop)
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<ManagerCommand>(100);

        loop {
            tokio::select! {
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
                                let response = match serde_json::from_str::<JsonRpcRequest>(trimmed) {
                                    Ok(request) => {
                                        // Handle send_message specially since it needs the manager
                                        if request.method == "send_message" {
                                            self.handle_send_message_request(request, &cmd_tx).await
                                        } else {
                                            handle_request(
                                                request,
                                                &self.storage,
                                                &cmd_tx,
                                                &self.account_uuid,
                                            ).await
                                        }
                                    }
                                    Err(e) => JsonRpcResponse::error(
                                        None,
                                        -32700,
                                        format!("Parse error: {}", e),
                                    ),
                                };

                                let response_json = serde_json::to_string(&response)?;
                                stdout.write_all(response_json.as_bytes()).await?;
                                stdout.write_all(b"\n").await?;
                                stdout.flush().await?;
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
                    }
                }
            }
        }

        Ok(())
    }

    /// Handle a send_message request directly (for stdio mode).
    async fn handle_send_message_request(
        &self,
        request: JsonRpcRequest,
        cmd_tx: &mpsc::Sender<ManagerCommand>,
    ) -> JsonRpcResponse {
        handle_request(request, &self.storage, cmd_tx, &self.account_uuid).await
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
        "get_messages" => rpc_get_messages(request.id, request.params, storage).await,
        "get_conversations" => rpc_get_conversations(request.id, request.params, storage).await,
        "get_status" => rpc_get_status(request.id, account_uuid),
        "tools/list" => rpc_tools_list(request.id),
        "tools/call" => rpc_tools_call(request.id, request.params, storage, cmd_tx, account_uuid).await,
        _ => JsonRpcResponse::error(
            request.id,
            -32601,
            format!("Method not found: {}", request.method),
        ),
    }
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

    // Resolve recipient to UUID
    let recipient_uuid = {
        let storage = storage.lock().await;
        match storage.resolve_recipient(&params.recipient) {
            Ok(Some(uuid)) => uuid,
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
        }
    };

    // Send command to manager task
    let (reply_tx, reply_rx) = oneshot::channel();
    let cmd = ManagerCommand::SendMessage {
        recipient_uuid: recipient_uuid.clone(),
        message: params.message.clone(),
        reply: reply_tx,
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

    // Store the outgoing message
    {
        let storage = storage.lock().await;
        if let Err(e) = storage.insert_message(&Message {
            id: 0,
            contact_uuid: recipient_uuid.clone(),
            direction: "out".to_string(),
            body: params.message,
            timestamp: timestamp as i64,
        }) {
            tracing::warn!("Failed to store outgoing message: {}", e);
        }
    }

    JsonRpcResponse::success(
        id,
        serde_json::json!({
            "success": true,
            "timestamp": timestamp,
            "recipient_uuid": recipient_uuid,
        }),
    )
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
