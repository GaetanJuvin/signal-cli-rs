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
        if let Some(f) = filter {
            let pattern = format!("%{}%", f);
            let mut stmt = self.conn.prepare(
                "SELECT uuid, phone, name FROM contacts
                 WHERE name LIKE ?1 OR phone LIKE ?1
                 ORDER BY name"
            )?;
            let rows = stmt.query_map([&pattern], |row| {
                Ok(Contact {
                    uuid: row.get(0)?,
                    phone: row.get(1)?,
                    name: row.get(2)?,
                })
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT uuid, phone, name FROM contacts ORDER BY name"
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(Contact {
                    uuid: row.get(0)?,
                    phone: row.get(1)?,
                    name: row.get(2)?,
                })
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        }
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
