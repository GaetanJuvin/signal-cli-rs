// src/client.rs
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
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
        self.call_with_timeout(method, params, Duration::from_secs(30)).await
    }

    async fn call_with_timeout(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value> {
        let connect_timeout = Duration::from_secs(5);
        let stream = tokio::time::timeout(connect_timeout, UnixStream::connect(&self.socket_path))
            .await
            .map_err(|_| anyhow!("Connection timeout. Server may be unresponsive."))?
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

        tokio::time::timeout(timeout, reader.read_line(&mut line))
            .await
            .map_err(|_| anyhow!("Server response timeout. Try 'signal-mcp stop' and restart the server."))??;

        let response: JsonRpcResponse = serde_json::from_str(&line)?;

        if let Some(error) = response.error {
            return Err(anyhow!("{}", error.message));
        }

        response.result.ok_or_else(|| anyhow!("Empty response"))
    }

    pub async fn send_message(&self, recipient: &str, message: &str) -> Result<()> {
        // Longer timeout for send - includes multi-device sync which can be slow
        let result = self.call_with_timeout(
            "send_message",
            serde_json::json!({
                "recipient": recipient,
                "message": message
            }),
            Duration::from_secs(120),
        ).await?;

        println!("Sent! Timestamp: {}", result["timestamp"]);
        Ok(())
    }

    pub async fn status(&self) -> Result<()> {
        let result = self.call("get_status", serde_json::json!({})).await?;

        let account = result["account_uuid"].as_str().unwrap_or("unknown");
        let connected = result["connected"].as_bool().unwrap_or(false);

        if connected {
            println!("Connected as {}", account);
        } else {
            println!("Not connected (account: {})", account);
        }
        Ok(())
    }

    pub async fn list_contacts(&self, filter: Option<&str>) -> Result<()> {
        let result = self.call("list_contacts", serde_json::json!({
            "filter": filter
        })).await?;

        // Pretty print contacts
        if let Some(contacts) = result.as_array() {
            if contacts.is_empty() {
                println!("No contacts found.");
            } else {
                println!("{:<20} {:<15} {}", "NAME", "PHONE", "UUID");
                println!("{}", "-".repeat(70));
                for contact in contacts {
                    let name = contact["name"].as_str().unwrap_or("-");
                    let phone = contact["phone"].as_str().unwrap_or("-");
                    let uuid = contact["uuid"].as_str().unwrap_or("-");
                    println!("{:<20} {:<15} {}", name, phone, uuid);
                }
                println!("\nTotal: {} contact(s)", contacts.len());
            }
        }
        Ok(())
    }

    pub async fn list_groups(&self, filter: Option<&str>) -> Result<()> {
        let result = self.call("list_groups", serde_json::json!({
            "filter": filter
        })).await?;

        // Pretty print groups
        if let Some(groups) = result.as_array() {
            if groups.is_empty() {
                println!("No groups found.");
            } else {
                println!("{:<30} {:<10} {}", "NAME", "MEMBERS", "ID");
                println!("{}", "-".repeat(70));
                for group in groups {
                    let name = group["name"].as_str().unwrap_or("-");
                    let members = group["members_count"].as_u64().unwrap_or(0);
                    let id = group["id"].as_str().unwrap_or("-");
                    println!("{:<30} {:<10} {}", name, members, id);
                }
                println!("\nTotal: {} group(s)", groups.len());
            }
        }
        Ok(())
    }

    pub async fn sync_contacts(&self) -> Result<()> {
        println!("Requesting contacts sync from primary device...");
        let result = self.call("sync_contacts", serde_json::json!({})).await?;

        let contacts_count = result["contacts_count"].as_u64().unwrap_or(0);
        let groups_count = result["groups_count"].as_u64().unwrap_or(0);

        println!("Synced {} contacts and {} groups", contacts_count, groups_count);
        Ok(())
    }
}
