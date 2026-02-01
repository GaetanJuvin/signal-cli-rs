# signal-mcp Design

**Date:** 2026-02-01
**Status:** Approved
**Goal:** Replace signal-cli-rb with a simple Pure Rust CLI + MCP server

## Overview

A complete rewrite focused on two use cases:
- **MCP Server**: Long-running process that maintains Signal connection, receives messages, exposes tools to AI
- **CLI**: Thin client that talks to the MCP server (plus standalone `link` command)

## Architecture

```
┌─────────────────────────────────────────────────┐
│           MCP Server (long-running)             │
│  - Maintains Signal websocket connection        │
│  - Receives messages                            │
│  - Exposes tools: send, contacts, history       │
│  - Stores conversation history                  │
└─────────────────────────────────────────────────┘
        ▲                           ▲
        │ Unix socket               │ stdio
   ┌────┴────┐                ┌─────┴─────┐
   │   CLI   │                │  Claude/  │
   │  client │                │    AI     │
   └─────────┘                └───────────┘
```

## Project Structure

```
signal-mcp/
├── Cargo.toml
├── src/
│   ├── main.rs          # Entry point: CLI or server mode
│   ├── cli.rs           # Thin CLI client (~50 lines)
│   ├── server.rs        # MCP server + Signal connection
│   ├── storage.rs       # SQLite: messages, contacts
│   └── signal.rs        # Presage wrapper (link, send, receive)
├── migrations/          # SQLite schema
└── README.md
```

**Single binary, two modes:**
- `signal-mcp server` — Start MCP server (long-running)
- `signal-mcp link` — Pair with phone (interactive, shows QR)
- `signal-mcp send <recipient> <message>` — Send via server
- `signal-mcp status` — Check if server running, show account info

**Dependencies:**
- `presage` + `presage-store-sqlite` (Signal protocol)
- `tokio` (async runtime)
- `rmcp` or raw JSON-RPC (MCP protocol)
- `rusqlite` (conversation history)
- `clap` (CLI parsing)
- `qrcode` (for link command)

**Estimated size:** ~800-1000 lines total

## MCP Server Tools

### `send_message`
```json
{
  "recipient": "+1234567890 or name or UUID",
  "message": "Hello!"
}
```
Returns: `{ "success": true, "timestamp": 1706812345 }`

### `list_contacts`
```json
{
  "filter": "optional search term"
}
```
Returns: `[{ "name": "Alice", "phone": "+1...", "uuid": "..." }, ...]`

### `get_conversations`
```json
{
  "limit": 20
}
```
Returns: List of recent conversations with last message preview

### `get_messages`
```json
{
  "contact": "+1234567890 or name or UUID",
  "limit": 50,
  "before": "optional timestamp"
}
```
Returns: Message history with that contact

### `get_status`
```json
{}
```
Returns: `{ "connected": true, "account": "+1...", "contacts_count": 42 }`

## Storage Schema

**Location:** `~/.config/signal-mcp/signal-mcp.db`

```sql
-- Contacts (synced from Signal)
CREATE TABLE contacts (
  uuid TEXT PRIMARY KEY,
  phone TEXT,
  name TEXT,
  profile_key BLOB,
  updated_at INTEGER
);

-- Messages (sent and received)
CREATE TABLE messages (
  id INTEGER PRIMARY KEY,
  contact_uuid TEXT NOT NULL,
  direction TEXT NOT NULL,  -- 'in' or 'out'
  body TEXT,
  timestamp INTEGER NOT NULL,
  read INTEGER DEFAULT 0,
  FOREIGN KEY (contact_uuid) REFERENCES contacts(uuid)
);

-- Indexes
CREATE INDEX idx_messages_contact ON messages(contact_uuid, timestamp);
CREATE INDEX idx_messages_timestamp ON messages(timestamp);
CREATE INDEX idx_contacts_phone ON contacts(phone);
CREATE INDEX idx_contacts_name ON contacts(name);
```

Presage's own SQLite store (separate file) handles Signal protocol state.

## CLI Interface

```bash
# Start server (foreground)
signal-mcp server

# Link device (standalone, doesn't need server)
signal-mcp link --name "my-laptop"

# Send message (via server)
signal-mcp send "+1234567890" "Hello!"
signal-mcp send "Alice" "Hello!"
signal-mcp send "a1b2c3d4-e5f6-..." "Hello!"

# Check status
signal-mcp status
```

**Recipient resolution order:**
1. UUID format (36 chars with dashes) → use directly
2. Starts with `+` → phone number lookup
3. Otherwise → contact name search (case-insensitive)

**Communication:** Unix socket at `~/.config/signal-mcp/signal-mcp.sock`

## Signal Connection

**Startup sequence:**
1. Load presage store
2. Verify linked (exit if not)
3. Open messages database
4. Start MCP server on Unix socket
5. Connect to Signal websocket
6. Enter receive loop

**Receive loop:**
- `SyncMessage(contacts)` → update contacts table
- `DataMessage(from, body, ts)` → insert into messages table
- `Receipt` → update delivery status (optional)
- `Typing` → ignore

**Resilience:**
- Auto-reconnect with exponential backoff (1s → 60s max)
- Graceful shutdown on SIGINT/SIGTERM
- 5s timeout for pending sends on shutdown

**Logging:**
- Default: errors only
- `RUST_LOG=info` for connection events
- `RUST_LOG=debug` for message details

## Migration from signal-cli-rb

The existing `vendor/presage` and `vendor/libsignal-service-rs` can be reused. The presage store format is compatible, so existing linked accounts will work.

## Non-Goals

- Full signal-cli compatibility (50+ commands)
- D-Bus interface
- JSON-RPC daemon mode (replaced by MCP)
- Ruby bindings
- Group messaging (maybe later)
- Attachments (maybe later)
