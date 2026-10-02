use rusqlite::{Connection as Sqlite, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Mutex};
use uuid::Uuid;

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
pub fn id() -> String {
    Uuid::new_v4().to_string()
}
pub fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Connection {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub enabled: bool,
    pub models: Vec<String>,
    pub supports_websocket: bool,
    pub created_at: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_at: i64,
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub oauth: bool,
    #[serde(default)]
    pub credential_source: String,
    #[serde(default)]
    pub source_path: String,
    #[serde(default)]
    pub account_identity: String,
}
impl Connection {
    pub fn public(&self) -> Value {
        json!({"id":self.id,"name":self.name,"kind":self.kind,"base_url":self.base_url,"enabled":self.enabled,"models":self.models,"supports_websocket":self.supports_websocket,"created_at":self.created_at,"credential_present":!self.api_key.is_empty(),"credential_source":self.credential_source})
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Target {
    pub connection_id: String,
    pub model: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Route {
    pub model: String,
    pub targets: Vec<Target>,
    pub strategy: String,
}
/// At most this many upstream attempts are kept per request record (20 accounts, each with at
/// most one same-account credential retry).
pub const MAX_ATTEMPTS: usize = 40;

/// One upstream attempt made while serving a request. Never contains prompts, bodies, tokens or
/// provider-supplied text.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RequestAttempt {
    pub connection_id: String,
    pub connection_name: String,
    /// The model name sent upstream (after route mapping).
    pub model: String,
    /// Upstream HTTP status. `101` is an accepted WebSocket handshake; `0` means no HTTP
    /// response was received (credential unavailable, connection failed, or timed out).
    pub status: u16,
    /// From the start of the attempt (including any credential refresh) until it was decided:
    /// response headers received, handshake finished, or the failure was observed.
    pub duration_ms: u64,
    /// A constant label from a fixed vocabulary (for example `rate_limited`), never provider text.
    #[serde(default)]
    pub error: Option<String>,
}

/// One row of the request log. Fields added after the first release are `serde(default)` so
/// older rows still load. A WebSocket session is a single row: its timings describe the first
/// turn of the session.
#[derive(Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    pub id: String,
    pub timestamp: String,
    pub model: String,
    /// The connection that served (or last attempted) the request.
    pub connection_id: String,
    pub connection_name: String,
    pub transport: String,
    pub status: u16,
    pub latency_ms: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub error: Option<String>,
    /// The requested model when it is a stored route alias; `None` for direct model names.
    #[serde(default)]
    pub route: Option<String>,
    /// How many times the request moved to a different account. A same-account retry after a
    /// credential refresh is an attempt, not a failover.
    #[serde(default)]
    pub failovers: u32,
    /// Upstream attempts in order, bounded by [`MAX_ATTEMPTS`].
    #[serde(default)]
    pub attempts: Vec<RequestAttempt>,
    /// Milliseconds from the start of the request (including earlier failed attempts) until the
    /// first body byte of the successful upstream response; for WebSockets, the first upstream
    /// frame after the first `response.create`. `None` if no successful body arrived.
    #[serde(default)]
    pub ttfb_ms: Option<u64>,
    /// Milliseconds from the start of the request until the first non-empty output delta: text,
    /// refusal, tool-call arguments or reasoning/thinking. `None` for responses delivered as a
    /// single JSON document or when no output was produced.
    #[serde(default)]
    pub first_token_ms: Option<u64>,
}
pub struct Store {
    db: Mutex<Sqlite>,
}
impl Store {
    pub fn open(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let db = Sqlite::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
        CREATE TABLE IF NOT EXISTS kv (kind TEXT NOT NULL, id TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(kind,id));
        CREATE TABLE IF NOT EXISTS requests (seq INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS counters (name TEXT PRIMARY KEY, value INTEGER NOT NULL);")?;
        Ok(Self { db: Mutex::new(db) })
    }
    pub fn list<T: serde::de::DeserializeOwned>(&self, kind: &str) -> Vec<T> {
        let db = self.db.lock().expect("database lock");
        let mut stmt = db
            .prepare("SELECT value FROM kv WHERE kind=? ORDER BY id")
            .expect("valid schema");
        stmt.query_map([kind], |row| row.get::<_, String>(0))
            .expect("valid schema")
            .filter_map(|r| r.ok().and_then(|s| serde_json::from_str(&s).ok()))
            .collect()
    }
    pub fn get<T: serde::de::DeserializeOwned>(&self, kind: &str, id: &str) -> Option<T> {
        let db = self.db.lock().expect("database lock");
        db.query_row(
            "SELECT value FROM kv WHERE kind=? AND id=?",
            params![kind, id],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
    }
    pub fn put<T: Serialize>(
        &self,
        kind: &str,
        id: &str,
        value: &T,
    ) -> Result<(), rusqlite::Error> {
        self.db.lock().expect("database lock").execute("INSERT INTO kv(kind,id,value) VALUES(?,?,?) ON CONFLICT(kind,id) DO UPDATE SET value=excluded.value",params![kind,id,serde_json::to_string(value).expect("serializable value")])?;
        Ok(())
    }
    pub fn delete(&self, kind: &str, id: &str) -> Result<(), rusqlite::Error> {
        self.db
            .lock()
            .expect("database lock")
            .execute("DELETE FROM kv WHERE kind=? AND id=?", params![kind, id])?;
        Ok(())
    }
    pub fn record(&self, record: &RequestRecord) -> Result<(), rusqlite::Error> {
        let mut db = self.db.lock().expect("database lock");
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO requests(value) VALUES(?)",
            [serde_json::to_string(record).expect("record")],
        )?;
        for name in [
            "total",
            if record.status < 400 {
                "success"
            } else {
                "failed"
            },
            record.transport.as_str(),
        ] {
            tx.execute("INSERT INTO counters(name,value) VALUES(?,1) ON CONFLICT(name) DO UPDATE SET value=value+1",[name])?;
        }
        tx.execute(
            "DELETE FROM requests WHERE seq <= (SELECT COALESCE(MAX(seq),0)-1000 FROM requests)",
            [],
        )?;
        tx.commit()
    }
    pub fn requests(&self, limit: usize) -> Vec<RequestRecord> {
        let db = self.db.lock().expect("database lock");
        let mut stmt = db
            .prepare("SELECT value FROM requests ORDER BY seq DESC LIMIT ?")
            .expect("schema");
        stmt.query_map([limit.min(1000) as i64], |r| r.get::<_, String>(0))
            .expect("schema")
            .filter_map(|r| r.ok().and_then(|s| serde_json::from_str(&s).ok()))
            .collect()
    }
    pub fn counter(&self, name: &str) -> u64 {
        self.db
            .lock()
            .expect("database lock")
            .query_row("SELECT value FROM counters WHERE name=?", [name], |r| {
                r.get(0)
            })
            .unwrap_or(0)
    }
}
