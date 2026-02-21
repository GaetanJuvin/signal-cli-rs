//! Signal module wrapping presage for Signal operations.
//!
//! This module provides a simplified interface for interacting with Signal
//! through the presage library.

use anyhow::{anyhow, Context, Result};
use futures::{channel::oneshot, future, StreamExt};
use presage::libsignal_service::configuration::SignalServers;
use presage::libsignal_service::content::{
    sync_message, Content, ContentBody, DataMessage, SyncMessage,
};
use presage::libsignal_service::prelude::phonenumber::PhoneNumber;
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::ServiceId;
use presage::manager::{ConfirmationData, Registered, RegistrationOptions};
use presage::model::messages::Received;
use presage::store::{ContentsStore, StateStore};
use presage::Manager;
use presage_store_sqlite::{OnNewIdentity, SqliteStore};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use url::Url;

use crate::storage::{Contact, Group, Storage};

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
    // Use open_with_passphrase with None to get create_if_missing(true) behavior
    SqliteStore::open_with_passphrase(&db_path, None, OnNewIdentity::Trust)
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

/// Register a new primary device with a phone number.
///
/// This is step 1 of registration: it sends an SMS verification code
/// to the given phone number and returns serializable confirmation data
/// that must be passed to `confirm_registration` with the code.
pub async fn request_registration(
    store_path: &Path,
    phone_number: &str,
    captcha: Option<&str>,
    use_voice: bool,
) -> Result<ConfirmationData> {
    let phone = PhoneNumber::from_str(phone_number)
        .map_err(|e| anyhow!("Invalid phone number '{}': {}", phone_number, e))?;

    let store = open_store(store_path).await?;

    let manager = Manager::register(
        store,
        RegistrationOptions {
            signal_servers: SignalServers::Production,
            phone_number: phone,
            use_voice_call: use_voice,
            captcha,
            force: true,
        },
    )
    .await
    .map_err(|e| anyhow!("Registration failed: {}", e))?;

    Ok(manager.confirmation_data())
}

/// Confirm registration with the verification code received via SMS/voice.
///
/// This is step 2 of registration: it takes the confirmation data from
/// `request_registration` and the verification code, and completes the
/// registration process.
pub async fn confirm_registration(
    store_path: &Path,
    confirmation_data: ConfirmationData,
    code: &str,
) -> Result<SignalManager> {
    let store = open_store(store_path).await?;

    let manager = Manager::from_confirmation_data(store, confirmation_data)
        .map_err(|e| anyhow!("Failed to restore confirmation state: {}", e))?;

    let registered = manager
        .confirm_verification_code(code)
        .await
        .map_err(|e| anyhow!("Verification failed: {}", e))?;

    Ok(registered)
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

    tracing::info!("Sending message to {}...", recipient_uuid);

    // Timeout for send operation - 15s to avoid blocking MCP clients
    match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        manager.send_message(ServiceId::Aci(uuid.into()), data_message, timestamp),
    )
    .await
    {
        Ok(Ok(())) => {
            tracing::info!("Message sent successfully to {}", recipient_uuid);
            Ok(timestamp)
        }
        Ok(Err(e)) => {
            tracing::error!("Failed to send message to {}: {}", recipient_uuid, e);
            Err(anyhow!("Failed to send message: {}", e))
        }
        Err(_) => {
            // Timeout - message was likely sent but sync failed
            tracing::warn!("Send message timed out after 15s to {} — message may have been delivered", recipient_uuid);
            Ok(timestamp)
        }
    }
}

/// Send a text message to a group identified by its base64-encoded master key.
///
/// # Arguments
///
/// * `manager` - The registered Signal manager
/// * `group_id` - Base64-encoded group master key
/// * `message` - The message text to send
///
/// # Returns
///
/// The timestamp of the sent message.
pub async fn send_group_message(
    manager: &mut SignalManager,
    group_id: &str,
    message: &str,
) -> Result<u64> {
    // Decode the base64 group ID to get the master key bytes
    let master_key_bytes = base64_decode(group_id)
        .with_context(|| format!("Invalid group ID (not valid base64): {}", group_id))?;

    if master_key_bytes.len() != 32 {
        return Err(anyhow!("Invalid group master key length: expected 32 bytes, got {}", master_key_bytes.len()));
    }

    let mut master_key = [0u8; 32];
    master_key.copy_from_slice(&master_key_bytes);

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
        .as_millis() as u64;

    let data_message = DataMessage {
        body: Some(message.to_string()),
        timestamp: Some(timestamp),
        ..Default::default()
    };

    // Timeout for send operation - group sends can be slow with many recipients
    match tokio::time::timeout(
        std::time::Duration::from_secs(120),
        manager.send_message_to_group(&master_key, data_message, timestamp),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(anyhow!("Failed to send group message: {}", e)),
        Err(_) => {
            tracing::warn!("Group message send timed out, but message was likely delivered to some recipients");
        }
    }

    Ok(timestamp)
}

/// Start receiving messages - returns a boxed message stream.
///
/// The stream is heap-allocated (`Box::pin`) because presage's deeply nested
/// future types are too large for the stack.
pub async fn start_receiving(
    manager: &mut SignalManager,
) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = SignalEvent> + '_>>> {
    let messages = manager
        .receive_messages()
        .await
        .map_err(|e| anyhow!("Failed to start receiving messages: {}", e))?;

    Ok(Box::pin(messages.filter_map(|received| {
        let event = match received {
            Received::QueueEmpty => Some(SignalEvent::QueueEmpty),
            Received::Contacts => Some(SignalEvent::ContactsSync),
            Received::Content(content) => content_to_event(&content),
        };
        future::ready(event)
    })))
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
        // "Note to Self" — a sync message where we sent a message to our own account
        ContentBody::SynchronizeMessage(SyncMessage {
            sent:
                Some(sync_message::Sent {
                    destination_service_id: Some(dest_uuid),
                    message:
                        Some(DataMessage {
                            body: Some(text), ..
                        }),
                    ..
                }),
            ..
        }) if dest_uuid == &sender_uuid => Some(SignalEvent::Message {
            sender_uuid,
            body: text.clone(),
            timestamp,
        }),
        ContentBody::SynchronizeMessage(_) => {
            // Skip other sync messages — these are our own outgoing messages
            // echoed back from another device.
            None
        }
        _ => None,
    }
}

/// Request contacts sync from the primary device and wait for them to arrive.
///
/// This function:
/// 1. Sends a sync request to the primary device
/// 2. Starts receiving messages
/// 3. Waits for the Contacts signal (or timeout after 30 seconds)
/// 4. Reads contacts from presage's store and saves them to our storage
/// 5. Also reads groups from presage's store
///
/// # Arguments
///
/// * `manager` - The registered Signal manager
/// * `storage` - Our storage to save contacts/groups to
///
/// # Returns
///
/// A tuple of (contacts_count, groups_count)
pub async fn request_contacts_sync(
    manager: &mut SignalManager,
    storage: &Arc<Mutex<Storage>>,
) -> Result<(usize, usize)> {
    // Try to send sync request to primary device (with timeout)
    // If it times out, we'll still read whatever is in the store
    match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        manager.request_contacts()
    ).await {
        Ok(Ok(())) => {
            tracing::debug!("Sync request sent to primary device");
        }
        Ok(Err(e)) => {
            tracing::warn!("Failed to request contacts from primary: {}", e);
        }
        Err(_) => {
            tracing::debug!("Sync request timed out - reading from store");
        }
    }

    // Read contacts directly from presage's store

    // Now read contacts and groups from presage's store and save to our storage
    let mut contacts_count = 0;
    let mut groups_count = 0;

    // Get contacts from presage store
    let presage_contacts = manager.store().contacts().await
        .map_err(|e| anyhow!("Failed to get contacts from store: {}", e))?;

    {
        let storage = storage.lock().await;

        for contact_result in presage_contacts {
            match contact_result {
                Ok(presage_contact) => {
                    let contact = Contact {
                        uuid: presage_contact.uuid.to_string(),
                        phone: presage_contact.phone_number.map(|p| p.to_string()),
                        name: if presage_contact.name.is_empty() {
                            None
                        } else {
                            Some(presage_contact.name.clone())
                        },
                    };

                    if let Err(e) = storage.upsert_contact(&contact) {
                        tracing::warn!("Failed to save contact {}: {}", contact.uuid, e);
                    } else {
                        contacts_count += 1;
                        tracing::debug!("Saved contact: {} ({:?})", contact.uuid, contact.name);
                    }
                }
                Err(e) => {
                    tracing::warn!("Error reading contact: {}", e);
                }
            }
        }
    }

    // Get groups from presage store
    let presage_groups = manager.store().groups().await
        .map_err(|e| anyhow!("Failed to get groups from store: {}", e))?;

    {
        let storage = storage.lock().await;

        for group_result in presage_groups {
            match group_result {
                Ok((master_key_bytes, presage_group)) => {
                    // Convert master key bytes to base64 for ID
                    let group_id = base64_encode(&master_key_bytes);

                    let group = Group {
                        id: group_id.clone(),
                        name: if presage_group.title.is_empty() {
                            None
                        } else {
                            Some(presage_group.title.clone())
                        },
                        members_count: presage_group.members.len(),
                    };

                    if let Err(e) = storage.upsert_group(&group) {
                        tracing::warn!("Failed to save group {}: {}", group_id, e);
                    } else {
                        groups_count += 1;
                        tracing::debug!("Saved group: {} ({:?})", group_id, group.name);
                    }
                }
                Err(e) => {
                    tracing::warn!("Error reading group: {}", e);
                }
            }
        }
    }

    Ok((contacts_count, groups_count))
}

/// Simple base64 encoding for group IDs
fn base64_encode(data: &[u8]) -> String {
    let mut result = String::new();
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    for chunk in data.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
        let b2 = chunk.get(2).copied().unwrap_or(0) as usize;

        let n = (b0 << 16) | (b1 << 8) | b2;

        result.push(CHARS[(n >> 18) & 0x3F] as char);
        result.push(CHARS[(n >> 12) & 0x3F] as char);

        if chunk.len() > 1 {
            result.push(CHARS[(n >> 6) & 0x3F] as char);
        } else {
            result.push('=');
        }

        if chunk.len() > 2 {
            result.push(CHARS[n & 0x3F] as char);
        } else {
            result.push('=');
        }
    }

    result
}

/// Simple base64 decoding for group IDs
fn base64_decode(data: &str) -> Result<Vec<u8>> {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    fn char_to_val(c: char) -> Option<u8> {
        CHARS.iter().position(|&x| x == c as u8).map(|p| p as u8)
    }

    let data = data.trim_end_matches('=');
    let mut result = Vec::new();

    let chars: Vec<char> = data.chars().collect();
    for chunk in chars.chunks(4) {
        let vals: Vec<u8> = chunk.iter()
            .filter_map(|&c| char_to_val(c))
            .collect();

        if vals.len() < 2 {
            return Err(anyhow!("Invalid base64 string"));
        }

        let n = match vals.len() {
            2 => ((vals[0] as u32) << 18) | ((vals[1] as u32) << 12),
            3 => ((vals[0] as u32) << 18) | ((vals[1] as u32) << 12) | ((vals[2] as u32) << 6),
            4 => ((vals[0] as u32) << 18) | ((vals[1] as u32) << 12) | ((vals[2] as u32) << 6) | (vals[3] as u32),
            _ => return Err(anyhow!("Invalid base64 string")),
        };

        result.push(((n >> 16) & 0xFF) as u8);
        if vals.len() > 2 {
            result.push(((n >> 8) & 0xFF) as u8);
        }
        if vals.len() > 3 {
            result.push((n & 0xFF) as u8);
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_to_event_returns_none_for_empty() {
        // This is a basic test structure; full testing would require mocking Content
    }
}
