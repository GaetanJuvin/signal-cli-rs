// src/server.rs
//! MCP server that handles JSON-RPC requests over a Unix socket.
//!
//! The server maintains a Signal connection and receives messages in the background,
//! storing them in the database. Clients connect via Unix socket and send JSON-RPC
//! requests to interact with Signal.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::signal::{self, SignalEvent, SignalManager};
use crate::storage::{Contact, Message, Storage};

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
    pub async fn run(mut self) -> Result<()> {
        let listener = UnixListener::bind(&self.socket_path)?;
        println!("Server listening on {:?}", self.socket_path);
        println!("Connected as {}", self.account_uuid);

        // Channel for Signal events (from receiver to main loop)
        let (event_tx, mut event_rx) = mpsc::channel::<SignalEvent>(100);

        // Channel for manager commands (from clients to main loop)
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<ManagerCommand>(100);

        // Start receiving messages
        signal::receive_messages(&mut self.manager, event_tx).await;

        // Main event loop - handles everything on one task to avoid Send issues
        loop {
            tokio::select! {
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

                // Handle Signal events
                Some(event) = event_rx.recv() => {
                    if let Err(e) = self.handle_signal_event(event).await {
                        tracing::error!("Error handling signal event: {}", e);
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
        "get_status" => rpc_get_status(request.id, account_uuid),
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
