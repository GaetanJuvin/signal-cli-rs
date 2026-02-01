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
- `signal.db` - Signal protocol state (keys, sessions)
- `signal-mcp.db` - Messages and contacts
- `signal-mcp.sock` - Unix socket for CLI communication
