# signal-mcp Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a Pure Rust Signal MCP server with thin CLI client

**Architecture:** Single binary with two modes: `server` (long-running MCP server maintaining Signal websocket) and CLI commands (`link`, `send`, `status`) that talk to the server via Unix socket. SQLite stores messages and contacts.

**Tech Stack:** Rust, presage (vendored), tokio, clap, rusqlite, serde, qrcode-rs

---

## Task 0: Clean up and scaffold

**Files:**
- Delete: Everything except `vendor/presage`, `vendor/libsignal-service-rs`, `docs/`
- Create: `Cargo.toml`, `src/main.rs`, `.gitignore`

**Step 1: Remove old implementation**

```bash
cd /Users/gaetanjuvin/Project/signal-cli-rb
rm -rf bin lib data native scripts spec tools .bundle
rm -f Gemfile Gemfile.lock signal-cli-rb.gemspec CONTEXT.md README.md USAGE.md
rm -rf vendor/signal-cli
```

**Step 2: Create Cargo.toml**

```toml
[package]
name = "signal-mcp"
version = "0.1.0"
edition = "2021"

[dependencies]
# Signal protocol
presage = { path = "vendor/presage/presage" }
presage-store-sled = { path = "vendor/presage/presage-store-sled" }

# Async runtime
tokio = { version = "1", features = ["full"] }
futures = "0.3"

# CLI
clap = { version = "4", features = ["derive"] }

# Storage
rusqlite = { version = "0.31", features = ["bundled"] }

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# QR code for linking
qrcode = "0.14"

# Logging
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# Error handling
anyhow = "1"
thiserror = "1"
```

**Step 3: Create .gitignore**

```
/target
*.db
*.db-journal
*.sock
```

**Step 4: Create minimal src/main.rs**

```rust
fn main() {
    println!("signal-mcp");
}
```

**Step 5: Verify it compiles**

Run: `cargo build`
Expected: Compiles (may take a while for presage deps)

**Step 6: Commit**

```bash
git add -A
git commit -m "chore: scaffold signal-mcp, remove old Ruby implementation"
```

---

## Task 1: CLI argument parsing

**Files:**
- Modify: `src/main.rs`

**Step 1: Add CLI structure**

```rust
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "signal-mcp")]
#[command(about = "Signal MCP server and CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the MCP server
    Server,
    /// Link this device to your Signal account
    Link {
        /// Device name shown in Signal
        #[arg(short, long, default_value = "signal-mcp")]
        name: String,
    },
    /// Send a message (via server)
    Send {
        /// Recipient: phone number, name, or UUID
        recipient: String,
        /// Message text
        message: String,
    },
    /// Check server status
    Status,
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Server => println!("Starting server..."),
        Commands::Link { name } => println!("Linking as '{}'...", name),
        Commands::Send { recipient, message } => {
            println!("Sending to {}: {}", recipient, message)
        }
        Commands::Status => println!("Checking status..."),
    }
}
```

**Step 2: Test CLI parsing**

Run: `cargo run -- --help`
Expected: Shows help with server, link, send, status commands

Run: `cargo run -- send "+1234" "hello"`
Expected: Prints "Sending to +1234: hello"

**Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat: add CLI argument parsing with clap"
```

---

## Task 2: Storage module

**Files:**
- Create: `src/storage.rs`
- Modify: `src/main.rs` (add mod)

**Step 1: Create storage module**

```rust
// src/storage.rs
use anyhow::Result;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contact {
    pub uuid: String,
    pub phone: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub contact_uuid: String,
    pub direction: String, // "in" or "out"
    pub body: String,
    pub timestamp: i64,
}

pub struct Storage {
    conn: Connection,
}

impl Storage {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        let storage = Self { conn };
        storage.init_schema()?;
        Ok(storage)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS contacts (
                uuid TEXT PRIMARY KEY,
                phone TEXT,
                name TEXT,
                updated_at INTEGER
            );

            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY,
                contact_uuid TEXT NOT NULL,
                direction TEXT NOT NULL,
                body TEXT,
                timestamp INTEGER NOT NULL,
                FOREIGN KEY (contact_uuid) REFERENCES contacts(uuid)
            );

            CREATE INDEX IF NOT EXISTS idx_messages_contact
                ON messages(contact_uuid, timestamp);
            CREATE INDEX IF NOT EXISTS idx_messages_timestamp
                ON messages(timestamp);
            CREATE INDEX IF NOT EXISTS idx_contacts_phone
                ON contacts(phone);
            CREATE INDEX IF NOT EXISTS idx_contacts_name
                ON contacts(name);
            "
        )?;
        Ok(())
    }

    pub fn upsert_contact(&self, contact: &Contact) -> Result<()> {
        self.conn.execute(
            "INSERT INTO contacts (uuid, phone, name, updated_at)
             VALUES (?1, ?2, ?3, strftime('%s', 'now'))
             ON CONFLICT(uuid) DO UPDATE SET
                phone = COALESCE(?2, phone),
                name = COALESCE(?3, name),
                updated_at = strftime('%s', 'now')",
            params![contact.uuid, contact.phone, contact.name],
        )?;
        Ok(())
    }

    pub fn insert_message(&self, msg: &Message) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO messages (contact_uuid, direction, body, timestamp)
             VALUES (?1, ?2, ?3, ?4)",
            params![msg.contact_uuid, msg.direction, msg.body, msg.timestamp],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn list_contacts(&self, filter: Option<&str>) -> Result<Vec<Contact>> {
        let mut stmt = if let Some(f) = filter {
            let pattern = format!("%{}%", f);
            let mut s = self.conn.prepare(
                "SELECT uuid, phone, name FROM contacts
                 WHERE name LIKE ?1 OR phone LIKE ?1
                 ORDER BY name"
            )?;
            let rows = s.query_map([pattern], |row| {
                Ok(Contact {
                    uuid: row.get(0)?,
                    phone: row.get(1)?,
                    name: row.get(2)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        } else {
            let mut s = self.conn.prepare(
                "SELECT uuid, phone, name FROM contacts ORDER BY name"
            )?;
            let rows = s.query_map([], |row| {
                Ok(Contact {
                    uuid: row.get(0)?,
                    phone: row.get(1)?,
                    name: row.get(2)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        Ok(stmt)
    }

    pub fn get_messages(&self, contact_uuid: &str, limit: i64) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, contact_uuid, direction, body, timestamp
             FROM messages
             WHERE contact_uuid = ?1
             ORDER BY timestamp DESC
             LIMIT ?2"
        )?;
        let rows = stmt.query_map(params![contact_uuid, limit], |row| {
            Ok(Message {
                id: row.get(0)?,
                contact_uuid: row.get(1)?,
                direction: row.get(2)?,
                body: row.get(3)?,
                timestamp: row.get(4)?,
            })
        })?;
        let mut msgs: Vec<_> = rows.collect::<Result<Vec<_>, _>>()?;
        msgs.reverse(); // Return in chronological order
        Ok(msgs)
    }

    pub fn resolve_recipient(&self, recipient: &str) -> Result<Option<String>> {
        // UUID format: 8-4-4-4-12 hex chars with dashes
        if recipient.len() == 36 && recipient.chars().filter(|c| *c == '-').count() == 4 {
            return Ok(Some(recipient.to_string()));
        }

        // Phone number
        if recipient.starts_with('+') {
            let mut stmt = self.conn.prepare(
                "SELECT uuid FROM contacts WHERE phone = ?1"
            )?;
            if let Ok(uuid) = stmt.query_row([recipient], |row| row.get::<_, String>(0)) {
                return Ok(Some(uuid));
            }
        }

        // Name search (case-insensitive)
        let mut stmt = self.conn.prepare(
            "SELECT uuid FROM contacts WHERE LOWER(name) = LOWER(?1)"
        )?;
        if let Ok(uuid) = stmt.query_row([recipient], |row| row.get::<_, String>(0)) {
            return Ok(Some(uuid));
        }

        Ok(None)
    }
}
```

**Step 2: Add module to main.rs**

Add at top of `src/main.rs`:
```rust
mod storage;
```

**Step 3: Verify it compiles**

Run: `cargo build`
Expected: Compiles without errors

**Step 4: Commit**

```bash
git add src/storage.rs src/main.rs
git commit -m "feat: add SQLite storage for contacts and messages"
```

---

## Task 3: Signal module (presage wrapper)

**Files:**
- Create: `src/signal.rs`
- Modify: `src/main.rs` (add mod)

**Step 1: Create signal module**

```rust
// src/signal.rs
use anyhow::{anyhow, Result};
use futures::StreamExt;
use presage::libsignal_service::prelude::*;
use presage::manager::ReceivingMode;
use presage::store::ContentsStore;
use presage::{Manager, manager::Registered};
use presage_store_sled::SledStore;
use std::path::Path;
use tokio::sync::mpsc;

pub type SignalManager = Manager<SledStore, Registered>;

#[derive(Debug, Clone)]
pub enum SignalEvent {
    Message {
        sender_uuid: String,
        body: String,
        timestamp: u64,
    },
    Contact {
        uuid: String,
        phone: Option<String>,
        name: Option<String>,
    },
}

pub async fn link_device(
    store_path: &Path,
    device_name: &str,
    on_qr: impl FnOnce(&str),
) -> Result<()> {
    let config_store = SledStore::open(store_path, None, None)?;

    let (tx, mut rx) = mpsc::channel(1);

    let link_future = Manager::link_secondary_device(
        config_store,
        presage::manager::LinkingVersion::V2,
        device_name.to_string(),
        tx,
    );

    // Get the provisioning URL and show QR
    tokio::select! {
        url = rx.recv() => {
            if let Some(url) = url {
                on_qr(&url.to_string());
            }
        }
        result = link_future => {
            result?;
            return Ok(());
        }
    }

    // Wait for linking to complete
    link_future.await?;
    Ok(())
}

pub async fn open_manager(store_path: &Path) -> Result<SignalManager> {
    let config_store = SledStore::open(store_path, None, None)?;
    let manager = Manager::load_registered(config_store)
        .await
        .map_err(|e| anyhow!("Not linked. Run 'signal-mcp link' first. Error: {}", e))?;
    Ok(manager)
}

pub async fn send_message(
    manager: &mut SignalManager,
    recipient_uuid: &str,
    message: &str,
) -> Result<u64> {
    let uuid = recipient_uuid.parse::<uuid::Uuid>()
        .map_err(|_| anyhow!("Invalid UUID: {}", recipient_uuid))?;

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as u64;

    manager
        .send_message(
            ServiceAddress { uuid },
            presage::libsignal_service::content::ContentBody::DataMessage(
                presage::libsignal_service::proto::DataMessage {
                    body: Some(message.to_string()),
                    timestamp: Some(timestamp),
                    ..Default::default()
                },
            ),
            timestamp,
        )
        .await?;

    Ok(timestamp)
}

pub async fn receive_messages(
    manager: &mut SignalManager,
    event_tx: mpsc::Sender<SignalEvent>,
) -> Result<()> {
    let mut messages = manager.receive_messages(ReceivingMode::Forever).await?;

    while let Some(content) = messages.next().await {
        match content {
            Ok(content) => {
                if let Some(msg) = content.body.data_message() {
                    if let Some(body) = &msg.body {
                        let sender_uuid = content.metadata.sender.uuid.to_string();
                        let _ = event_tx.send(SignalEvent::Message {
                            sender_uuid,
                            body: body.clone(),
                            timestamp: msg.timestamp.unwrap_or(0),
                        }).await;
                    }
                }
                if let Some(sync) = content.body.synchronize_message() {
                    if let Some(contacts) = &sync.contacts {
                        // TODO: Parse contacts blob
                        tracing::info!("Received contacts sync");
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Error receiving message: {}", e);
            }
        }
    }

    Ok(())
}

pub fn get_account_uuid(manager: &SignalManager) -> String {
    manager.state().service_ids.aci.to_string()
}
```

**Step 2: Add module to main.rs**

Add at top of `src/main.rs`:
```rust
mod signal;
```

**Step 3: Verify it compiles**

Run: `cargo build`
Expected: Compiles (may have warnings about unused code, that's fine)

**Step 4: Commit**

```bash
git add src/signal.rs src/main.rs
git commit -m "feat: add Signal module wrapping presage"
```

---

## Task 4: Link command

**Files:**
- Modify: `src/main.rs`

**Step 1: Implement link command**

Replace the main function in `src/main.rs`:

```rust
use clap::{Parser, Subcommand};
use anyhow::Result;
use std::path::PathBuf;

mod signal;
mod storage;

#[derive(Parser)]
#[command(name = "signal-mcp")]
#[command(about = "Signal MCP server and CLI")]
struct Cli {
    /// Config directory
    #[arg(long, default_value = "~/.config/signal-mcp")]
    config: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the MCP server
    Server,
    /// Link this device to your Signal account
    Link {
        /// Device name shown in Signal
        #[arg(short, long, default_value = "signal-mcp")]
        name: String,
    },
    /// Send a message (via server)
    Send {
        /// Recipient: phone number, name, or UUID
        recipient: String,
        /// Message text
        message: String,
    },
    /// Check server status
    Status,
}

fn expand_path(path: &str) -> PathBuf {
    if path.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(&path[2..]);
        }
    }
    PathBuf::from(path)
}

fn print_qr(url: &str) {
    use qrcode::QrCode;
    use qrcode::render::unicode;

    let code = QrCode::new(url).unwrap();
    let image = code
        .render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Light)
        .light_color(unicode::Dense1x2::Dark)
        .build();

    println!("\nScan this QR code with Signal app:\n");
    println!("{}", image);
    println!("\nOr use this URL: {}\n", url);
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::WARN.into())
        )
        .init();

    let cli = Cli::parse();
    let config_dir = expand_path(&cli.config);
    std::fs::create_dir_all(&config_dir)?;

    match cli.command {
        Commands::Server => {
            println!("Starting server...");
            // TODO: implement
        }
        Commands::Link { name } => {
            println!("Linking device as '{}'...", name);
            let store_path = config_dir.join("signal-store");
            signal::link_device(&store_path, &name, print_qr).await?;
            println!("Successfully linked!");
        }
        Commands::Send { recipient, message } => {
            println!("Sending to {}: {}", recipient, message);
            // TODO: send via server
        }
        Commands::Status => {
            println!("Checking status...");
            // TODO: check server status
        }
    }

    Ok(())
}
```

**Step 2: Add dirs dependency**

Add to `Cargo.toml` under `[dependencies]`:
```toml
dirs = "5"
```

**Step 3: Test link command**

Run: `cargo run -- link --name "test-device"`
Expected: Shows QR code in terminal (linking will fail without scanning, but QR should display)

**Step 4: Commit**

```bash
git add Cargo.toml src/main.rs
git commit -m "feat: implement link command with QR display"
```

---

## Task 5: MCP server skeleton

**Files:**
- Create: `src/server.rs`
- Modify: `src/main.rs`

**Step 1: Create server module**

```rust
// src/server.rs
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use crate::signal::{self, SignalEvent, SignalManager};
use crate::storage::Storage;

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Option<serde_json::Value>,
    method: String,
    params: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

pub struct Server {
    socket_path: std::path::PathBuf,
    storage: Storage,
    manager: SignalManager,
}

impl Server {
    pub async fn new(config_dir: &Path) -> Result<Self> {
        let store_path = config_dir.join("signal-store");
        let db_path = config_dir.join("signal-mcp.db");
        let socket_path = config_dir.join("signal-mcp.sock");

        // Clean up stale socket
        let _ = std::fs::remove_file(&socket_path);

        let storage = Storage::open(&db_path)?;
        let manager = signal::open_manager(&store_path).await?;

        Ok(Self {
            socket_path,
            storage,
            manager,
        })
    }

    pub async fn run(mut self) -> Result<()> {
        let listener = UnixListener::bind(&self.socket_path)?;
        println!("Server listening on {:?}", self.socket_path);

        let account = signal::get_account_uuid(&self.manager);
        println!("Connected as {}", account);

        // Channel for Signal events
        let (event_tx, mut event_rx) = mpsc::channel::<SignalEvent>(100);

        // Spawn message receiver
        let mut manager_clone = self.manager.clone();
        tokio::spawn(async move {
            if let Err(e) = signal::receive_messages(&mut manager_clone, event_tx).await {
                tracing::error!("Receive loop error: {}", e);
            }
        });

        loop {
            tokio::select! {
                // Handle incoming client connections
                result = listener.accept() => {
                    match result {
                        Ok((stream, _)) => {
                            self.handle_client(stream).await?;
                        }
                        Err(e) => {
                            tracing::error!("Accept error: {}", e);
                        }
                    }
                }
                // Handle Signal events
                Some(event) = event_rx.recv() => {
                    self.handle_signal_event(event)?;
                }
            }
        }
    }

    async fn handle_client(&mut self, stream: UnixStream) -> Result<()> {
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();

        while reader.read_line(&mut line).await? > 0 {
            let response = match serde_json::from_str::<JsonRpcRequest>(&line) {
                Ok(req) => self.handle_request(req).await,
                Err(e) => JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: None,
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32700,
                        message: format!("Parse error: {}", e),
                    }),
                },
            };

            let response_str = serde_json::to_string(&response)? + "\n";
            writer.write_all(response_str.as_bytes()).await?;
            line.clear();
        }

        Ok(())
    }

    async fn handle_request(&mut self, req: JsonRpcRequest) -> JsonRpcResponse {
        let result = match req.method.as_str() {
            "send_message" => self.rpc_send_message(req.params).await,
            "list_contacts" => self.rpc_list_contacts(req.params),
            "get_messages" => self.rpc_get_messages(req.params),
            "get_status" => self.rpc_get_status(),
            _ => Err(anyhow::anyhow!("Unknown method: {}", req.method)),
        };

        match result {
            Ok(value) => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: req.id,
                result: Some(value),
                error: None,
            },
            Err(e) => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: req.id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32000,
                    message: e.to_string(),
                }),
            },
        }
    }

    async fn rpc_send_message(&mut self, params: Option<serde_json::Value>) -> Result<serde_json::Value> {
        #[derive(Deserialize)]
        struct Params { recipient: String, message: String }

        let params: Params = serde_json::from_value(
            params.ok_or_else(|| anyhow::anyhow!("Missing params"))?
        )?;

        let uuid = self.storage.resolve_recipient(&params.recipient)?
            .ok_or_else(|| anyhow::anyhow!("Recipient not found: {}", params.recipient))?;

        let timestamp = signal::send_message(&mut self.manager, &uuid, &params.message).await?;

        // Store outgoing message
        self.storage.insert_message(&crate::storage::Message {
            id: 0,
            contact_uuid: uuid,
            direction: "out".to_string(),
            body: params.message,
            timestamp: timestamp as i64,
        })?;

        Ok(serde_json::json!({ "success": true, "timestamp": timestamp }))
    }

    fn rpc_list_contacts(&self, params: Option<serde_json::Value>) -> Result<serde_json::Value> {
        #[derive(Deserialize, Default)]
        struct Params { filter: Option<String> }

        let params: Params = params
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();

        let contacts = self.storage.list_contacts(params.filter.as_deref())?;
        Ok(serde_json::to_value(contacts)?)
    }

    fn rpc_get_messages(&self, params: Option<serde_json::Value>) -> Result<serde_json::Value> {
        #[derive(Deserialize)]
        struct Params {
            contact: String,
            #[serde(default = "default_limit")]
            limit: i64,
        }
        fn default_limit() -> i64 { 50 }

        let params: Params = serde_json::from_value(
            params.ok_or_else(|| anyhow::anyhow!("Missing params"))?
        )?;

        let uuid = self.storage.resolve_recipient(&params.contact)?
            .ok_or_else(|| anyhow::anyhow!("Contact not found: {}", params.contact))?;

        let messages = self.storage.get_messages(&uuid, params.limit)?;
        Ok(serde_json::to_value(messages)?)
    }

    fn rpc_get_status(&self) -> Result<serde_json::Value> {
        let account = signal::get_account_uuid(&self.manager);
        let contacts = self.storage.list_contacts(None)?;
        Ok(serde_json::json!({
            "connected": true,
            "account": account,
            "contacts_count": contacts.len()
        }))
    }

    fn handle_signal_event(&mut self, event: SignalEvent) -> Result<()> {
        match event {
            SignalEvent::Message { sender_uuid, body, timestamp } => {
                println!("Message from {}: {}", sender_uuid, body);
                self.storage.insert_message(&crate::storage::Message {
                    id: 0,
                    contact_uuid: sender_uuid,
                    direction: "in".to_string(),
                    body,
                    timestamp: timestamp as i64,
                })?;
            }
            SignalEvent::Contact { uuid, phone, name } => {
                self.storage.upsert_contact(&crate::storage::Contact { uuid, phone, name })?;
            }
        }
        Ok(())
    }
}
```

**Step 2: Wire up server command in main.rs**

Update the `Commands::Server` match arm:

```rust
Commands::Server => {
    let server = crate::server::Server::new(&config_dir).await?;
    server.run().await?;
}
```

And add `mod server;` at the top.

**Step 3: Verify it compiles**

Run: `cargo build`
Expected: Compiles

**Step 4: Commit**

```bash
git add src/server.rs src/main.rs
git commit -m "feat: add MCP server with JSON-RPC over Unix socket"
```

---

## Task 6: CLI client (send/status)

**Files:**
- Create: `src/client.rs`
- Modify: `src/main.rs`

**Step 1: Create client module**

```rust
// src/client.rs
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Serialize)]
struct JsonRpcRequest<'a> {
    jsonrpc: &'static str,
    id: u32,
    method: &'a str,
    params: serde_json::Value,
}

#[derive(Deserialize)]
struct JsonRpcResponse {
    result: Option<serde_json::Value>,
    error: Option<JsonRpcError>,
}

#[derive(Deserialize)]
struct JsonRpcError {
    message: String,
}

pub struct Client {
    socket_path: std::path::PathBuf,
}

impl Client {
    pub fn new(config_dir: &Path) -> Self {
        Self {
            socket_path: config_dir.join("signal-mcp.sock"),
        }
    }

    async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let stream = UnixStream::connect(&self.socket_path).await
            .map_err(|_| anyhow!("Server not running. Start with: signal-mcp server"))?;

        let (reader, mut writer) = stream.into_split();

        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method,
            params,
        };

        let request_str = serde_json::to_string(&request)? + "\n";
        writer.write_all(request_str.as_bytes()).await?;

        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).await?;

        let response: JsonRpcResponse = serde_json::from_str(&line)?;

        if let Some(error) = response.error {
            return Err(anyhow!("{}", error.message));
        }

        response.result.ok_or_else(|| anyhow!("Empty response"))
    }

    pub async fn send_message(&self, recipient: &str, message: &str) -> Result<()> {
        let result = self.call("send_message", serde_json::json!({
            "recipient": recipient,
            "message": message
        })).await?;

        println!("Sent! Timestamp: {}", result["timestamp"]);
        Ok(())
    }

    pub async fn status(&self) -> Result<()> {
        let result = self.call("get_status", serde_json::json!({})).await?;

        println!("Connected as {}", result["account"]);
        println!("Contacts: {}", result["contacts_count"]);
        Ok(())
    }
}
```

**Step 2: Wire up send and status commands in main.rs**

Update the match arms:

```rust
Commands::Send { recipient, message } => {
    let client = crate::client::Client::new(&config_dir);
    client.send_message(&recipient, &message).await?;
}
Commands::Status => {
    let client = crate::client::Client::new(&config_dir);
    client.status().await?;
}
```

And add `mod client;` at the top.

**Step 3: Test**

Run: `cargo run -- status`
Expected: "Server not running. Start with: signal-mcp server"

**Step 4: Commit**

```bash
git add src/client.rs src/main.rs
git commit -m "feat: add CLI client for send and status commands"
```

---

## Task 7: MCP protocol (stdio transport for AI)

**Files:**
- Modify: `src/server.rs`
- Modify: `src/main.rs`

**Step 1: Add stdio mode to server**

The MCP protocol uses stdio for AI clients. Add a `--stdio` flag to server mode.

Update `Commands::Server` in main.rs:
```rust
/// Start the MCP server
Server {
    /// Use stdio instead of Unix socket (for MCP clients)
    #[arg(long)]
    stdio: bool,
},
```

**Step 2: Add stdio handling to server**

Add to `src/server.rs`:

```rust
pub async fn run_stdio(mut self) -> Result<()> {
    use tokio::io::{stdin, stdout, BufReader, AsyncBufReadExt, AsyncWriteExt};

    // Send MCP initialization
    let init = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "signal-mcp",
                "version": env!("CARGO_PKG_VERSION")
            }
        }
    });

    let mut stdout = stdout();
    stdout.write_all(serde_json::to_string(&init)?.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;

    let mut stdin = BufReader::new(stdin());
    let mut line = String::new();

    // Spawn message receiver
    let (event_tx, mut event_rx) = mpsc::channel::<SignalEvent>(100);
    let mut manager_clone = self.manager.clone();
    tokio::spawn(async move {
        if let Err(e) = signal::receive_messages(&mut manager_clone, event_tx).await {
            tracing::error!("Receive loop error: {}", e);
        }
    });

    loop {
        tokio::select! {
            result = stdin.read_line(&mut line) => {
                if result? == 0 {
                    break; // EOF
                }

                if let Ok(req) = serde_json::from_str::<JsonRpcRequest>(&line) {
                    let response = self.handle_request(req).await;
                    stdout.write_all(serde_json::to_string(&response)?.as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                    stdout.flush().await?;
                }
                line.clear();
            }
            Some(event) = event_rx.recv() => {
                self.handle_signal_event(event)?;
            }
        }
    }

    Ok(())
}
```

**Step 3: Wire up in main.rs**

```rust
Commands::Server { stdio } => {
    let server = crate::server::Server::new(&config_dir).await?;
    if stdio {
        server.run_stdio().await?;
    } else {
        server.run().await?;
    }
}
```

**Step 4: Commit**

```bash
git add src/server.rs src/main.rs
git commit -m "feat: add stdio transport for MCP protocol"
```

---

## Task 8: MCP tools declaration

**Files:**
- Modify: `src/server.rs`

**Step 1: Add tools/list handler**

MCP clients call `tools/list` to discover available tools. Add this method handler:

```rust
"tools/list" => self.rpc_tools_list(),
```

And the implementation:

```rust
fn rpc_tools_list(&self) -> Result<serde_json::Value> {
    Ok(serde_json::json!({
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
    }))
}
```

**Step 2: Add tools/call handler**

MCP clients call `tools/call` to invoke tools:

```rust
"tools/call" => self.rpc_tools_call(req.params).await,
```

```rust
async fn rpc_tools_call(&mut self, params: Option<serde_json::Value>) -> Result<serde_json::Value> {
    #[derive(Deserialize)]
    struct Params {
        name: String,
        arguments: serde_json::Value,
    }

    let params: Params = serde_json::from_value(
        params.ok_or_else(|| anyhow::anyhow!("Missing params"))?
    )?;

    let result = match params.name.as_str() {
        "send_message" => self.rpc_send_message(Some(params.arguments)).await?,
        "list_contacts" => self.rpc_list_contacts(Some(params.arguments))?,
        "get_messages" => self.rpc_get_messages(Some(params.arguments))?,
        "get_conversations" => self.rpc_get_conversations(Some(params.arguments))?,
        "get_status" => self.rpc_get_status()?,
        _ => return Err(anyhow::anyhow!("Unknown tool: {}", params.name)),
    };

    Ok(serde_json::json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&result)?
        }]
    }))
}
```

**Step 3: Add get_conversations**

```rust
fn rpc_get_conversations(&self, params: Option<serde_json::Value>) -> Result<serde_json::Value> {
    #[derive(Deserialize, Default)]
    struct Params {
        #[serde(default = "default_conv_limit")]
        limit: i64,
    }
    fn default_conv_limit() -> i64 { 20 }

    let params: Params = params
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();

    let conversations = self.storage.get_conversations(params.limit)?;
    Ok(serde_json::to_value(conversations)?)
}
```

Add to `storage.rs`:

```rust
#[derive(Debug, Clone, Serialize)]
pub struct Conversation {
    pub contact: Contact,
    pub last_message: String,
    pub last_timestamp: i64,
    pub unread_count: i64,
}

pub fn get_conversations(&self, limit: i64) -> Result<Vec<Conversation>> {
    let mut stmt = self.conn.prepare(
        "SELECT c.uuid, c.phone, c.name, m.body, m.timestamp,
                (SELECT COUNT(*) FROM messages m2
                 WHERE m2.contact_uuid = c.uuid AND m2.direction = 'in' AND m2.read = 0)
         FROM contacts c
         JOIN messages m ON m.contact_uuid = c.uuid
         WHERE m.timestamp = (
             SELECT MAX(timestamp) FROM messages WHERE contact_uuid = c.uuid
         )
         ORDER BY m.timestamp DESC
         LIMIT ?1"
    )?;

    let rows = stmt.query_map([limit], |row| {
        Ok(Conversation {
            contact: Contact {
                uuid: row.get(0)?,
                phone: row.get(1)?,
                name: row.get(2)?,
            },
            last_message: row.get(3)?,
            last_timestamp: row.get(4)?,
            unread_count: row.get(5)?,
        })
    })?;

    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}
```

**Step 4: Commit**

```bash
git add src/server.rs src/storage.rs
git commit -m "feat: add MCP tools declaration and call handlers"
```

---

## Task 9: README and cleanup

**Files:**
- Create: `README.md`

**Step 1: Write README**

```markdown
# signal-mcp

A Signal client as an MCP server. Send and receive Signal messages from Claude or any MCP-compatible AI.

## Quick Start

```bash
# Build
cargo build --release

# Link to your Signal account (scan QR with Signal app)
./target/release/signal-mcp link

# Start the server
./target/release/signal-mcp server

# Send a message (in another terminal)
./target/release/signal-mcp send "+1234567890" "Hello from CLI!"
./target/release/signal-mcp send "Alice" "Hello by name!"
```

## MCP Integration

Add to your Claude config:

```json
{
  "mcpServers": {
    "signal": {
      "command": "/path/to/signal-mcp",
      "args": ["server", "--stdio"]
    }
  }
}
```

## Available Tools

- **send_message** - Send a message to a contact
- **list_contacts** - List your Signal contacts
- **get_messages** - Get message history with a contact
- **get_conversations** - List recent conversations
- **get_status** - Check connection status

## CLI Commands

- `signal-mcp link [--name NAME]` - Link device to Signal account
- `signal-mcp server [--stdio]` - Start the server
- `signal-mcp send RECIPIENT MESSAGE` - Send a message
- `signal-mcp status` - Check server status

## Config

Data stored in `~/.config/signal-mcp/`:
- `signal-store/` - Signal protocol state (keys, sessions)
- `signal-mcp.db` - Messages and contacts
- `signal-mcp.sock` - Unix socket for CLI communication
```

**Step 2: Commit**

```bash
git add README.md
git commit -m "docs: add README"
```

---

## Summary

**Total estimated lines:** ~800-1000 (vs 2,955 before)

**Files:**
- `src/main.rs` (~100 lines) - CLI entry point
- `src/client.rs` (~60 lines) - CLI client
- `src/server.rs` (~300 lines) - MCP server
- `src/storage.rs` (~200 lines) - SQLite storage
- `src/signal.rs` (~150 lines) - Presage wrapper

**After completion:** Working Signal MCP server that Claude can use to send/receive messages.
