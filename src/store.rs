use crate::usage::{METRICS, UsageEvent, metric_values};
use rusqlite::{Connection as Sqlite, params, params_from_iter, types::Value as Sql};
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
        create_usage_schema(&db)?;
        Ok(Self { db: Mutex::new(db) })
    }
    /// Runs `f` with the database (usage queries). Keep `f` short: it holds the store lock.
    pub(crate) fn with_db<T>(
        &self,
        f: impl FnOnce(&Sqlite) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        f(&self.db.lock().expect("database lock"))
    }
    /// Replaces every key-value row of `kind` in one transaction.
    pub fn replace_kind(
        &self,
        kind: &str,
        items: &[(String, Value)],
    ) -> Result<(), rusqlite::Error> {
        let mut db = self.db.lock().expect("database lock");
        let tx = db.transaction()?;
        tx.execute("DELETE FROM kv WHERE kind=?", [kind])?;
        for (id, value) in items {
            tx.execute(
                "INSERT INTO kv(kind,id,value) VALUES(?,?,?)",
                params![kind, id, value.to_string()],
            )?;
        }
        tx.commit()
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
        self.record_with_usage(record, &[])
    }
    /// Writes a request-log row, its counters and its usage ledger units in one transaction.
    /// Ledger units are idempotent and outlive the 1000-row request-log cap.
    pub fn record_with_usage(
        &self,
        record: &RequestRecord,
        usage: &[UsageEvent],
    ) -> Result<(), rusqlite::Error> {
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
        for e in usage {
            insert_usage(&tx, e)?;
        }
        tx.commit()
    }
    /// Writes usage ledger units (WebSocket turns, external usage) in one transaction. A unit
    /// whose `event_id` is already recorded is ignored. Returns how many units were new.
    pub fn record_usage(&self, usage: &[UsageEvent]) -> Result<usize, rusqlite::Error> {
        let mut db = self.db.lock().expect("database lock");
        let tx = db.transaction()?;
        let mut added = 0;
        for e in usage {
            added += usize::from(insert_usage(&tx, e)?);
        }
        tx.commit()?;
        Ok(added)
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

/// Raw ledger units are kept this long; daily aggregates are kept indefinitely.
pub const USAGE_EVENT_RETENTION_DAYS: i64 = 400;
/// Hourly aggregates (rolling 24h/7d/30d windows) are kept this long.
pub const USAGE_HOURLY_RETENTION_DAYS: i64 = 40;

/// Aggregate dimension columns, in key order. Unknown values are stored as `''`.
const DIMENSIONS: [&str; 7] = [
    "bucket",
    "source",
    "connection_id",
    "provider",
    "model",
    "client_key_id",
    "billing",
];

fn create_usage_schema(db: &Sqlite) -> rusqlite::Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS usage_events (
            event_id TEXT PRIMARY KEY,
            ts INTEGER NOT NULL,
            source TEXT NOT NULL,
            response_id TEXT,
            value TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS usage_events_ts ON usage_events(ts);
        CREATE INDEX IF NOT EXISTS usage_events_response ON usage_events(response_id)
            WHERE response_id IS NOT NULL;
        CREATE TABLE IF NOT EXISTS usage_dedup (
            event_id TEXT PRIMARY KEY, ts INTEGER NOT NULL, source TEXT NOT NULL,
            response_id TEXT);
        CREATE INDEX IF NOT EXISTS usage_dedup_response ON usage_dedup(response_id)
            WHERE response_id IS NOT NULL;
        INSERT OR IGNORE INTO usage_dedup(event_id, ts, source, response_id)
            SELECT event_id, ts, source, response_id FROM usage_events;",
    )?;
    let metrics: String = METRICS
        .iter()
        .map(|m| format!(", {m} INTEGER NOT NULL DEFAULT 0"))
        .collect();
    for table in ["usage_hourly", "usage_daily"] {
        db.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS {table} (
                bucket INTEGER NOT NULL, source TEXT NOT NULL, connection_id TEXT NOT NULL,
                provider TEXT NOT NULL, model TEXT NOT NULL, client_key_id TEXT NOT NULL,
                billing TEXT NOT NULL, connection_name TEXT NOT NULL, client_key_name TEXT NOT NULL
                {metrics},
                PRIMARY KEY ({}));
            CREATE INDEX IF NOT EXISTS {table}_source_bucket ON {table}(source, bucket);",
            DIMENSIONS.join(",")
        ))?;
        let columns: Vec<String> = db
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |row| row.get(1))?
            .collect::<rusqlite::Result<_>>()?;
        if !columns.iter().any(|name| name == "api_priced") {
            db.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN api_priced INTEGER NOT NULL DEFAULT 0;
                ALTER TABLE {table} ADD COLUMN sub_priced INTEGER NOT NULL DEFAULT 0;
                UPDATE {table} SET api_priced=CASE WHEN billing='api_key' THEN priced ELSE 0 END,
                    sub_priced=CASE WHEN billing='subscription' THEN priced ELSE 0 END,
                    cost_api=CASE WHEN billing='api_key' THEN cost_sum ELSE 0 END,
                    cost_sub=CASE WHEN billing='subscription' THEN cost_sum ELSE 0 END;
                UPDATE {table} SET succeeded=0, failed=0, cancelled=0 WHERE source<>'gateway';"
            ))?;
        }
    }
    Ok(())
}

fn upsert_sql(table: &str) -> String {
    let columns: Vec<&str> = DIMENSIONS
        .iter()
        .copied()
        .chain(["connection_name", "client_key_name"])
        .chain(METRICS.iter().copied())
        .collect();
    let updates: Vec<String> = METRICS
        .iter()
        .map(|m| {
            if *m == "lat_max" {
                "lat_max = MAX(lat_max, excluded.lat_max)".to_string()
            } else {
                format!("{m} = {m} + excluded.{m}")
            }
        })
        .collect();
    format!(
        "INSERT INTO {table} ({}) VALUES ({}) ON CONFLICT ({}) DO UPDATE SET \
         connection_name = excluded.connection_name, client_key_name = excluded.client_key_name, {}",
        columns.join(","),
        vec!["?"; columns.len()].join(","),
        DIMENSIONS.join(","),
        updates.join(", ")
    )
}

/// Inserts one ledger unit and, only if it is new, adds it to the hourly and daily aggregates.
fn insert_usage(tx: &rusqlite::Transaction<'_>, e: &UsageEvent) -> rusqlite::Result<bool> {
    // Keep compact idempotency keys after detailed rows expire. Old native imports must
    // never increment the indefinitely retained daily aggregates twice.
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO usage_dedup(event_id, ts, source, response_id) VALUES(?,?,?,?)",
        params![e.event_id, e.ts_ms, e.source, e.response_id],
    )?;
    if inserted == 0 {
        return Ok(false);
    }
    tx.execute(
        "INSERT OR IGNORE INTO usage_events(event_id, ts, source, response_id, value) VALUES(?,?,?,?,?)",
        params![
            e.event_id,
            e.ts_ms,
            e.source,
            e.response_id,
            serde_json::to_string(e).expect("usage event")
        ],
    )?;
    let text = |v: &Option<String>| Sql::Text(v.clone().unwrap_or_default());
    let values = metric_values(e);
    for (table, bucket) in [
        ("usage_hourly", e.ts_ms.div_euclid(3_600_000)),
        ("usage_daily", e.ts_ms.div_euclid(86_400_000)),
    ] {
        let mut params = vec![
            Sql::Integer(bucket),
            Sql::Text(e.source.clone()),
            text(&e.connection_id),
            text(&e.provider),
            Sql::Text(e.model.clone()),
            text(&e.client_key_id),
            Sql::Text(e.billing.clone()),
            text(&e.connection_name),
            text(&e.client_key_name),
        ];
        params.extend(values.iter().map(|v| Sql::Integer(*v)));
        tx.prepare_cached(&upsert_sql(table))?
            .execute(params_from_iter(params))?;
    }
    tx.execute(
        "INSERT INTO counters(name, value) VALUES(?, ?) ON CONFLICT(name) DO UPDATE SET value = MIN(value, excluded.value)",
        params![format!("usage_started_ms:{}", e.source), e.ts_ms],
    )?;
    let now = chrono::Utc::now().timestamp_millis();
    tx.execute(
        "DELETE FROM usage_events WHERE ts < ?",
        [now - USAGE_EVENT_RETENTION_DAYS * 86_400_000],
    )?;
    tx.execute(
        "DELETE FROM usage_hourly WHERE bucket < ?",
        [(now - USAGE_HOURLY_RETENTION_DAYS * 86_400_000).div_euclid(3_600_000)],
    )?;
    Ok(true)
}
