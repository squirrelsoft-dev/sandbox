//! SQLite-backed record of every sandbox and its lifecycle events.

use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use sandbox_core::api::{Sandbox, SandboxState};

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS sandboxes (
                 id         TEXT PRIMARY KEY,
                 state      TEXT NOT NULL,
                 record     TEXT NOT NULL,
                 created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS events (
                 seq        INTEGER PRIMARY KEY AUTOINCREMENT,
                 sandbox_id TEXT NOT NULL,
                 ts         TEXT NOT NULL,
                 kind       TEXT NOT NULL,
                 detail     TEXT NOT NULL
             );",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn put(&self, sb: &Sandbox) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.lock().unwrap().execute(
            "INSERT INTO sandboxes (id, state, record, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET state = ?2, record = ?3, updated_at = ?5",
            params![sb.id, sb.state.to_string(), serde_json::to_string(sb)?, sb.created_at.to_rfc3339(), now],
        )?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Option<Sandbox>> {
        let record: Option<String> = self
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT record FROM sandboxes WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        Ok(record.map(|r| serde_json::from_str(&r)).transpose()?)
    }

    /// Every sandbox that hasn't been fully destroyed.
    pub fn live(&self) -> Result<Vec<Sandbox>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT record FROM sandboxes WHERE state != ?1 ORDER BY created_at",
        )?;
        let rows = stmt.query_map([SandboxState::Destroyed.to_string()], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row?)?);
        }
        Ok(out)
    }

    pub fn event(&self, id: &str, kind: &str, detail: &str) {
        let res = self.conn.lock().unwrap().execute(
            "INSERT INTO events (sandbox_id, ts, kind, detail) VALUES (?1, ?2, ?3, ?4)",
            params![id, chrono::Utc::now().to_rfc3339(), kind, detail],
        );
        if let Err(e) = res {
            tracing::warn!(error = %e, "failed to record event");
        }
    }
}
