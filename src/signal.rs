//! Signal module wrapping presage for Signal operations.
//!
//! This module provides a simplified interface for interacting with Signal
//! through the presage library.

use anyhow::{anyhow, Context, Result};
use futures::{channel::oneshot, future, StreamExt};
use presage::libsignal_service::configuration::SignalServers;
use presage::libsignal_service::content::{Content, ContentBody, DataMessage};
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::ServiceId;
use presage::manager::Registered;
use presage::model::messages::Received;
use presage::store::StateStore;
use presage::Manager;
use presage_store_sqlite::{OnNewIdentity, SqliteStore};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use url::Url;

/// Type alias for a registered Signal manager using SQLite storage.
pub type SignalManager = Manager<SqliteStore, Registered>;

/// Events emitted by the Signal message receiver.
#[derive(Debug, Clone)]
pub enum SignalEvent {
    /// A text message was received.
    Message {
        /// UUID of the sender.
        sender_uuid: String,
        /// Message body text.
        body: String,
        /// Unix timestamp in milliseconds.
        timestamp: u64,
    },
    /// A contact was synchronized.
    Contact {
        /// UUID of the contact.
        uuid: String,
        /// Phone number if available.
        phone: Option<String>,
        /// Display name if available.
        name: Option<String>,
    },
    /// The message queue is empty (initial sync complete).
    QueueEmpty,
    /// Contacts have been synchronized.
    ContactsSync,
}

/// Open the SQLite store at the given path.
///
/// Creates the database if it does not exist.
pub async fn open_store(store_path: &Path) -> Result<SqliteStore> {
    let db_path = store_path.to_string_lossy();
    SqliteStore::open(&db_path, OnNewIdentity::Trust)
        .await
        .with_context(|| format!("Failed to open store at {}", db_path))
}

/// Link this device as a secondary device to an existing Signal account.
///
/// This function will:
/// 1. Generate a provisioning URL
/// 2. Call `on_url` with the URL (for QR code display)
/// 3. Wait for the primary device to scan the QR code
/// 4. Complete the linking process
///
/// # Arguments
///
/// * `store_path` - Path to the SQLite database file
/// * `device_name` - Name to display for this device in Signal
/// * `on_url` - Callback that receives the provisioning URL for QR code display
///
/// # Returns
///
/// The registered manager on success.
pub async fn link_device(
    store_path: &Path,
    device_name: &str,
    on_url: impl FnOnce(Url) + Send + 'static,
) -> Result<SignalManager> {
    let store = open_store(store_path).await?;

    let (provisioning_link_tx, provisioning_link_rx) = oneshot::channel();

    let (manager_result, _) = future::join(
        Manager::link_secondary_device(
            store,
            SignalServers::Production,
            device_name.to_string(),
            provisioning_link_tx,
        ),
        async move {
            match provisioning_link_rx.await {
                Ok(url) => on_url(url),
                Err(e) => tracing::error!("Failed to receive provisioning URL: {}", e),
            }
        },
    )
    .await;

    manager_result.map_err(|e| anyhow!("Failed to link device: {}", e))
}

/// Load an already-registered manager from the store.
///
/// # Arguments
///
/// * `store_path` - Path to the SQLite database file
///
/// # Returns
///
/// The registered manager, or an error if not yet registered.
pub async fn open_manager(store_path: &Path) -> Result<SignalManager> {
    let store = open_store(store_path).await?;
    Manager::load_registered(store)
        .await
        .map_err(|e| anyhow!("Failed to load registered manager: {}", e))
}

/// Check if the device is registered (linked).
///
/// # Arguments
///
/// * `store_path` - Path to the SQLite database file
///
/// # Returns
///
/// `true` if the device is registered, `false` otherwise.
pub async fn is_registered(store_path: &Path) -> Result<bool> {
    let store = open_store(store_path).await?;
    Ok(store.is_registered().await)
}

/// Get the account's UUID (ACI).
///
/// # Arguments
///
/// * `manager` - The registered Signal manager
///
/// # Returns
///
/// The account's UUID as a string.
pub fn get_account_uuid(manager: &SignalManager) -> String {
    manager.registration_data().service_ids.aci.to_string()
}

/// Send a text message to a recipient identified by UUID.
///
/// # Arguments
///
/// * `manager` - The registered Signal manager
/// * `recipient_uuid` - UUID of the recipient
/// * `message` - The message text to send
///
/// # Returns
///
/// The timestamp of the sent message.
pub async fn send_message(
    manager: &mut SignalManager,
    recipient_uuid: &str,
    message: &str,
) -> Result<u64> {
    let uuid: Uuid = recipient_uuid
        .parse()
        .with_context(|| format!("Invalid UUID: {}", recipient_uuid))?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
        .as_millis() as u64;

    let data_message = DataMessage {
        body: Some(message.to_string()),
        timestamp: Some(timestamp),
        ..Default::default()
    };

    manager
        .send_message(ServiceId::Aci(uuid.into()), data_message, timestamp)
        .await
        .map_err(|e| anyhow!("Failed to send message: {}", e))?;

    Ok(timestamp)
}

/// Start receiving messages and send events through a channel.
///
/// This function will:
/// 1. Connect to the Signal websocket
/// 2. Receive messages continuously
/// 3. Parse incoming messages and send SignalEvents through the channel
///
/// The function runs until the channel is closed or an error occurs.
///
/// # Arguments
///
/// * `manager` - The registered Signal manager
/// * `tx` - Channel sender for SignalEvents
pub async fn receive_messages(manager: &mut SignalManager, tx: mpsc::Sender<SignalEvent>) {
    let messages = match manager.receive_messages().await {
        Ok(stream) => stream,
        Err(e) => {
            tracing::error!("Failed to start receiving messages: {}", e);
            return;
        }
    };

    futures::pin_mut!(messages);

    while let Some(received) = messages.next().await {
        let event = match received {
            Received::QueueEmpty => Some(SignalEvent::QueueEmpty),
            Received::Contacts => Some(SignalEvent::ContactsSync),
            Received::Content(content) => content_to_event(&content),
        };

        if let Some(event) = event {
            if tx.send(event).await.is_err() {
                tracing::debug!("Channel closed, stopping message receiver");
                break;
            }
        }
    }
}

/// Convert a Content message to a SignalEvent if it contains a text message.
fn content_to_event(content: &Content) -> Option<SignalEvent> {
    let sender_uuid = content.metadata.sender.raw_uuid().to_string();
    let timestamp = content.metadata.timestamp;

    match &content.body {
        ContentBody::DataMessage(DataMessage {
            body: Some(text), ..
        }) => Some(SignalEvent::Message {
            sender_uuid,
            body: text.clone(),
            timestamp,
        }),
        ContentBody::SynchronizeMessage(sync_msg) => {
            // Handle sent messages (from another device)
            if let Some(sent) = &sync_msg.sent {
                if let Some(DataMessage {
                    body: Some(text), ..
                }) = &sent.message
                {
                    return Some(SignalEvent::Message {
                        sender_uuid,
                        body: text.clone(),
                        timestamp: sent.timestamp.unwrap_or(timestamp),
                    });
                }
            }
            None
        }
        _ => None,
    }
}

/// Request contacts sync from the primary device.
///
/// This is useful for secondary devices to get the contact list.
///
/// # Arguments
///
/// * `manager` - The registered Signal manager
pub async fn request_contacts_sync(manager: &mut SignalManager) -> Result<()> {
    manager
        .request_contacts()
        .await
        .map_err(|e| anyhow!("Failed to request contacts: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_to_event_returns_none_for_empty() {
        // This is a basic test structure; full testing would require mocking Content
    }
}
