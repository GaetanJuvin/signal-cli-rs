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
}
