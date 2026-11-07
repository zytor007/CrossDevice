//! Persistent state. One writer connection is expected; WAL keeps readers
//! unblocked.

use std::path::Path;

use cd_files::TransferState;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use thiserror::Error;

const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported schema version {0}")]
    UnsupportedSchema(i64),
    #[error("invalid transfer state `{0}`")]
    InvalidState(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRecord {
    pub id: String,
    pub name: String,
    pub cert_fingerprint: Vec<u8>,
    pub public_key: Vec<u8>,
    pub capabilities: u32,
    pub paired_at: i64,
    pub last_seen: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Incoming,
    Outgoing,
}

impl TransferDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Incoming => "incoming",
            Self::Outgoing => "outgoing",
        }
    }

    pub fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "incoming" => Ok(Self::Incoming),
            "outgoing" => Ok(Self::Outgoing),
            other => Err(StoreError::InvalidState(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferRecord {
    pub id: String,
    pub direction: TransferDirection,
    pub peer_id: String,
    pub root_name: String,
    pub total_bytes: u64,
    pub received_bytes: u64,
    pub state: TransferState,
    pub dest_path: String,
    pub created_at: i64,
    pub updated_at: i64,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    pub fn open_memory() -> Result<Self, StoreError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self, StoreError> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), StoreError> {
        let version: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchema(version));
        }
        if version < 1 {
            self.conn.execute_batch(SCHEMA_V1)?;
            self.conn
                .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(())
    }

    pub fn upsert_device(&self, device: &DeviceRecord) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO devices
             (id, name, cert_fingerprint, public_key, capabilities, paired_at, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
               name = excluded.name,
               cert_fingerprint = excluded.cert_fingerprint,
               public_key = excluded.public_key,
               capabilities = excluded.capabilities,
               last_seen = excluded.last_seen",
            params![
                device.id,
                device.name,
                device.cert_fingerprint,
                device.public_key,
                device.capabilities,
                device.paired_at,
                device.last_seen
            ],
        )?;
        Ok(())
    }

    pub fn device(&self, id: &str) -> Result<Option<DeviceRecord>, StoreError> {
        self.conn
            .query_row(
                "SELECT id, name, cert_fingerprint, public_key, capabilities, paired_at, last_seen
                 FROM devices WHERE id = ?1",
                params![id],
                map_device,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn devices(&self) -> Result<Vec<DeviceRecord>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT id, name, cert_fingerprint, public_key, capabilities, paired_at, last_seen
             FROM devices ORDER BY paired_at DESC",
        )?;
        let rows = statement.query_map([], map_device)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn remove_device(&self, id: &str) -> Result<bool, StoreError> {
        Ok(self
            .conn
            .execute("DELETE FROM devices WHERE id = ?1", params![id])?
            > 0)
    }

    pub fn set_setting(&self, key: &str, value: &Value) -> Result<(), StoreError> {
        let json = serde_json::to_string(value)?;
        self.conn.execute(
            "INSERT INTO settings (key, value_json) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json",
            params![key, json],
        )?;
        Ok(())
    }

    pub fn setting(&self, key: &str) -> Result<Option<Value>, StoreError> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT value_json FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        match json {
            Some(json) => Ok(Some(serde_json::from_str(&json)?)),
            None => Ok(None),
        }
    }

    pub fn save_transfer(&self, transfer: &TransferRecord) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO transfers
             (id, direction, peer_id, root_name, total_bytes, received_bytes, state,
              dest_path, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
               received_bytes = excluded.received_bytes,
               state = excluded.state,
               updated_at = excluded.updated_at",
            params![
                transfer.id,
                transfer.direction.as_str(),
                transfer.peer_id,
                transfer.root_name,
                transfer.total_bytes,
                transfer.received_bytes,
                state_str(transfer.state),
                transfer.dest_path,
                transfer.created_at,
                transfer.updated_at
            ],
        )?;
        Ok(())
    }

    pub fn transfer(&self, id: &str) -> Result<Option<TransferRecord>, StoreError> {
        self.conn
            .query_row(
                "SELECT id, direction, peer_id, root_name, total_bytes, received_bytes,
                        state, dest_path, created_at, updated_at
                 FROM transfers WHERE id = ?1",
                params![id],
                map_transfer,
            )
            .optional()
            .map_err(StoreError::from)?
            .transpose()
    }

    pub fn transfers(&self) -> Result<Vec<TransferRecord>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT id, direction, peer_id, root_name, total_bytes, received_bytes,
                    state, dest_path, created_at, updated_at
             FROM transfers ORDER BY updated_at DESC",
        )?;
        let rows = statement.query_map([], map_transfer)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)?
            .into_iter()
            .collect()
    }

    pub fn remove_transfer(&self, id: &str) -> Result<bool, StoreError> {
        Ok(self
            .conn
            .execute("DELETE FROM transfers WHERE id = ?1", params![id])?
            > 0)
    }
}

fn map_device(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceRecord> {
    Ok(DeviceRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        cert_fingerprint: row.get(2)?,
        public_key: row.get(3)?,
        capabilities: row.get(4)?,
        paired_at: row.get(5)?,
        last_seen: row.get(6)?,
    })
}

fn map_transfer(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<TransferRecord, StoreError>> {
    let direction: String = row.get(1)?;
    let state: String = row.get(6)?;
    Ok((|| {
        Ok(TransferRecord {
            id: row.get(0)?,
            direction: TransferDirection::parse(&direction)?,
            peer_id: row.get(2)?,
            root_name: row.get(3)?,
            total_bytes: row.get(4)?,
            received_bytes: row.get(5)?,
            state: parse_state(&state)?,
            dest_path: row.get(7)?,
            created_at: row.get(8)?,
            updated_at: row.get(9)?,
        })
    })())
}

fn state_str(state: TransferState) -> &'static str {
    match state {
        TransferState::Queued => "queued",
        TransferState::Running => "running",
        TransferState::Paused => "paused",
        TransferState::Completed => "completed",
        TransferState::Skipped => "skipped",
        TransferState::Cancelled => "cancelled",
        TransferState::Failed => "failed",
    }
}

fn parse_state(value: &str) -> Result<TransferState, StoreError> {
    match value {
        "queued" => Ok(TransferState::Queued),
        "running" => Ok(TransferState::Running),
        "paused" => Ok(TransferState::Paused),
        "completed" => Ok(TransferState::Completed),
        "skipped" => Ok(TransferState::Skipped),
        "cancelled" => Ok(TransferState::Cancelled),
        "failed" => Ok(TransferState::Failed),
        other => Err(StoreError::InvalidState(other.to_owned())),
    }
}

const SCHEMA_V1: &str = r#"
CREATE TABLE devices (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  cert_fingerprint BLOB NOT NULL,
  public_key BLOB NOT NULL,
  capabilities INTEGER NOT NULL,
  paired_at INTEGER NOT NULL,
  last_seen INTEGER
);

CREATE TABLE settings (
  key TEXT PRIMARY KEY,
  value_json TEXT NOT NULL
);

CREATE TABLE transfers (
  id TEXT PRIMARY KEY,
  direction TEXT NOT NULL,
  peer_id TEXT NOT NULL,
  root_name TEXT NOT NULL,
  total_bytes INTEGER NOT NULL,
  received_bytes INTEGER NOT NULL,
  state TEXT NOT NULL,
  dest_path TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> DeviceRecord {
        DeviceRecord {
            id: "macbook-air-13".to_owned(),
            name: "MacBook Air".to_owned(),
            cert_fingerprint: vec![1, 2, 3],
            public_key: vec![4, 5, 6],
            capabilities: 0b1011,
            paired_at: 100,
            last_seen: Some(200),
        }
    }

    fn transfer() -> TransferRecord {
        TransferRecord {
            id: "task-1".to_owned(),
            direction: TransferDirection::Outgoing,
            peer_id: "macbook-air-13".to_owned(),
            root_name: "assets".to_owned(),
            total_bytes: 1000,
            received_bytes: 400,
            state: TransferState::Paused,
            dest_path: "/tmp/assets".to_owned(),
            created_at: 100,
            updated_at: 200,
        }
    }

    #[test]
    fn devices_roundtrip_upsert_and_remove() {
        let store = Store::open_memory().unwrap();
        store.upsert_device(&device()).unwrap();
        assert_eq!(store.device("macbook-air-13").unwrap(), Some(device()));

        let updated = DeviceRecord {
            last_seen: Some(300),
            ..device()
        };
        store.upsert_device(&updated).unwrap();
        assert_eq!(store.device("macbook-air-13").unwrap(), Some(updated));
        assert_eq!(store.devices().unwrap().len(), 1);
        assert!(store.remove_device("macbook-air-13").unwrap());
        assert_eq!(store.device("macbook-air-13").unwrap(), None);
    }

    #[test]
    fn settings_store_arbitrary_json() {
        let store = Store::open_memory().unwrap();
        let value = serde_json::json!({"auto_fetch_bytes": 67108864, "excluded": ["1password"]});
        store.set_setting("clipboard", &value).unwrap();
        assert_eq!(store.setting("clipboard").unwrap(), Some(value));
        assert_eq!(store.setting("missing").unwrap(), None);
    }

    #[test]
    fn transfers_roundtrip_and_update_state() {
        let store = Store::open_memory().unwrap();
        let mut task = transfer();
        store.save_transfer(&task).unwrap();
        assert_eq!(store.transfer("task-1").unwrap(), Some(task.clone()));

        task.received_bytes = 1000;
        task.state = TransferState::Completed;
        task.updated_at = 300;
        store.save_transfer(&task).unwrap();
        assert_eq!(store.transfer("task-1").unwrap(), Some(task));
        assert_eq!(store.transfers().unwrap().len(), 1);
        assert!(store.remove_transfer("task-1").unwrap());
    }

    #[test]
    fn file_database_reopens_with_schema() {
        let path = std::env::temp_dir().join(format!(
            "crossdev-store-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        {
            let store = Store::open(&path).unwrap();
            store.upsert_device(&device()).unwrap();
        }
        {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.device("macbook-air-13").unwrap(), Some(device()));
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn direction_and_state_parsing_rejects_unknown_values() {
        assert_eq!(
            TransferDirection::parse("incoming").unwrap(),
            TransferDirection::Incoming
        );
        assert!(TransferDirection::parse("sideways").is_err());
        assert!(parse_state("exploded").is_err());
    }
}
