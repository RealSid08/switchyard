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
#[derive(Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    pub id: String,
    pub timestamp: String,
    pub model: String,
    pub connection_id: String,
    pub connection_name: String,
    pub transport: String,
    pub status: u16,
    pub latency_ms: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub error: Option<String>,
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
