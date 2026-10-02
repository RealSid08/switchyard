//! Read-only, incremental import of usage that native apps recorded on this machine or in their
//! account: OpenCode's SQLite history, Codex session logs, Claude Code project logs and Cursor's
//! account usage events.
//!
//! Rules:
//! - Opt-in per source (`POST /api/usage/native/import`); nothing is read before that.
//! - Only counts, model and provider names and timestamps are read into memory and stored.
//!   Prompts, outputs and tool arguments are never stored. Chosen import roots remain private checkpoint state. OpenCode rows are read with
//!   `json_extract` so message content never leaves SQLite.
//! - Native files are opened read-only and are never written.
//! - Work is bounded: each chunk has a deadline and byte/row/page budgets, and a checkpoint lets
//!   the next chunk continue. The job holds only a weak app reference and stops with the app.
//! - Events go to the usage ledger through [`crate::usage::record_external`], which is
//!   idempotent on a stable native id, so overlapping rescans never double count.
//! - Native history does not know which gateway account served it, so it is labelled by device
//!   or by the identity the record itself carries, never by the current login. Traffic that is
//!   identifiably the gateway's own (a provider named `switchyard`) is excluded; everything else is
//!   marked as possibly overlapping (`disjoint: false`), so totals stay separate from gateway totals.
//!
//! Cursor event paging follows the MIT-licensed CodexBar (Copyright (c) 2026 Peter Steinberger) as
//! a protocol reference; see docs/usage-sources.md.
use crate::{
    app::{ApiError, App},
    store::{hash, now},
    usage::{ExternalUsage, Tokens, record_external, tokens_from_usage},
    usage_sources::{self, Collector, FetchError, Monitor, num, open_readonly, send_json},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path as FsPath, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const CHECKPOINT_KIND: &str = "native_checkpoint";
/// Budget for one chunk of one source.
const CHUNK_DEADLINE: Duration = Duration::from_secs(20);
const CHUNK_BYTES: u64 = 64 * 1024 * 1024;
const CHUNK_ROWS: usize = 20_000;
/// Lines longer than this are skipped (they are prompt payloads, never usage records).
const MAX_LINE: usize = 32 * 1024 * 1024;
const MAX_FILES: usize = 50_000;
/// A job runs at most this many chunks before yielding to the next scheduled resume.
const MAX_CHUNKS: usize = 60;
const BATCH: usize = 1000;
const CURSOR_PAGE_SIZE: u32 = 100;
const CURSOR_PAGES_PER_CHUNK: u32 = 50;
/// First Cursor import reaches back this far; later runs continue from the last import.
const CURSOR_INITIAL_DAYS: i64 = 30;
const CURSOR_OVERLAP_MS: i64 = 3_600_000;

#[derive(Default)]
pub(crate) struct NativeState {
    running: AtomicBool,
    current: Mutex<Option<(String, String)>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct FileCursor {
    offset: u64,
    len: u64,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    last_total: Option<u64>,
    #[serde(default)]
    excluded: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct CursorWindow {
    start_ms: i64,
    end_ms: i64,
    page: u32,
    total: Option<u64>,
    seen: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Checkpoint {
    #[serde(default)]
    generation: String,
    source: String,
    enabled: bool,
    /// User-chosen root (private; never returned).
    root: Option<String>,
    imported: u64,
    excluded: u64,
    first_event_ms: Option<i64>,
    last_event_ms: Option<i64>,
    last_run_at: Option<String>,
    /// `pending`, `running`, `partial`, `complete`, `error` or `not_found`.
    status: String,
    message: Option<String>,
    #[serde(default)]
    files: HashMap<String, FileCursor>,
    #[serde(default)]
    sqlite: Option<(i64, String)>,
    #[serde(default)]
    cursor: Option<CursorWindow>,
    #[serde(default)]
    cursor_done_until_ms: Option<i64>,
    #[serde(default)]
    identity: Option<String>,
}
impl Checkpoint {
    fn note(&mut self, ts_ms: i64) {
        self.first_event_ms = Some(self.first_event_ms.map_or(ts_ms, |f| f.min(ts_ms)));
        self.last_event_ms = Some(self.last_event_ms.map_or(ts_ms, |l| l.max(ts_ms)));
    }
}

fn iso_ms(ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}
fn ms_of(v: &Value) -> Option<i64> {
    match v {
        Value::String(s) => s.trim().parse::<i64>().ok().or_else(|| {
            chrono::DateTime::parse_from_rfc3339(s.trim())
                .ok()
                .map(|t| t.timestamp_millis())
        }),
        Value::Number(n) => n.as_i64(),
        _ => None,
    }
    .filter(|ms| *ms > 0)
    .map(|ms| if ms < 100_000_000_000 { ms * 1000 } else { ms })
}

// ---------------------------------------------------------------------------------------------
// Source roots
// ---------------------------------------------------------------------------------------------

fn root_for(cp: &Checkpoint) -> Option<PathBuf> {
    if let Some(r) = &cp.root {
        return Some(PathBuf::from(r));
    }
    match cp.source.as_str() {
        "opencode" => usage_sources::opencode_data_dir(),
        "codex" => usage_sources::codex_home(),
        "claude" => usage_sources::claude_home(),
        _ => None,
    }
}
fn opencode_db(root: &FsPath) -> PathBuf {
    if root.extension().is_some_and(|e| e == "db") {
        root.to_path_buf()
    } else {
        root.join("opencode.db")
    }
}

/// Lists `*.jsonl` files under `dir` up to `depth` levels, sorted, bounded.
fn jsonl_files(dir: &FsPath, depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 || out.len() >= MAX_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let Ok(t) = e.file_type() else { continue };
        let p = e.path();
        if t.is_dir() {
            jsonl_files(&p, depth - 1, out);
        } else if t.is_file() && p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
            if out.len() >= MAX_FILES {
                return;
            }
        }
    }
}

struct Budget {
    started: Instant,
    bytes: u64,
    rows: usize,
}
impl Budget {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            bytes: 0,
            rows: 0,
        }
    }
    fn exhausted(&self) -> bool {
        self.started.elapsed() >= CHUNK_DEADLINE
            || self.bytes >= CHUNK_BYTES
            || self.rows >= CHUNK_ROWS
    }
}

/// Reads complete lines from `offset`, calling `f` for each. Returns the new offset (an
/// incomplete trailing line is left for the next run).
fn read_lines(
    path: &FsPath,
    offset: u64,
    budget: &mut Budget,
    mut f: impl FnMut(&[u8]) -> bool,
) -> std::io::Result<u64> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::with_capacity(256 * 1024, file);
    let mut pos = offset;
    let mut line = Vec::new();
    loop {
        if budget.exhausted() {
            break;
        }
        line.clear();
        let n = (&mut reader)
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            if n > MAX_LINE {
                // Oversized line: skip through its end.
                let mut skip = Vec::new();
                let rest = reader.read_until(b'\n', &mut skip)?;
                if skip.last() != Some(&b'\n') {
                    break;
                }
                pos += (n + rest) as u64;
                budget.bytes += (n + rest) as u64;
                continue;
            }
            break;
        }
        pos += n as u64;
        budget.bytes += n as u64;
        if f(&line) {
            budget.rows += 1;
        }
    }
    Ok(pos)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

// ---------------------------------------------------------------------------------------------
// Codex sessions
// ---------------------------------------------------------------------------------------------

fn codex_label(account: Option<&str>) -> String {
    match account {
        Some(a) => format!("Codex workspace {}", &hash(a)[..8]),
        None => "Codex on this device".into(),
    }
}

/// Codex `token_count` usage: `input_tokens` includes cached (and cache-write) input and
/// `output_tokens` includes reasoning.
pub fn codex_tokens(u: &Value) -> Option<Tokens> {
    let input = u["input_tokens"].as_u64()?;
    let cached = u["cached_input_tokens"].as_u64();
    let write = u["cache_write_input_tokens"].as_u64();
    Some(Tokens {
        input: Some(input.saturating_sub(cached.unwrap_or(0).saturating_add(write.unwrap_or(0)))),
        cache_read: cached,
        cache_write: write,
        output: u["output_tokens"].as_u64(),
        reasoning: u["reasoning_output_tokens"].as_u64(),
        ..Default::default()
    })
}

fn scan_codex(
    cp: &mut Checkpoint,
    root: &FsPath,
    budget: &mut Budget,
) -> (Vec<ExternalUsage>, bool) {
    let mut files = Vec::new();
    for sub in ["sessions", "archived_sessions"] {
        jsonl_files(&root.join(sub), 5, &mut files);
    }
    let mut out = Vec::new();
    let mut more = false;
    for path in files {
        if budget.exhausted() {
            more = true;
            break;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let key = hash(&rel)[..24].to_string();
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let mut fc = cp.files.get(&key).cloned().unwrap_or_default();
        if fc.offset == meta.len() {
            continue;
        }
        if meta.len() < fc.offset {
            fc = FileCursor::default();
        }
        let session_fallback = key.clone();
        let mut excluded = 0u64;
        let result = read_lines(&path, fc.offset, budget, |line| {
            if contains(line, b"\"session_meta\"") {
                if let Ok(v) = serde_json::from_slice::<Value>(line) {
                    let p = &v["payload"];
                    fc.session = p["id"].as_str().map(String::from).or(fc.session.take());
                    fc.provider = p["model_provider"]
                        .as_str()
                        .map(|s| s.chars().take(64).collect());
                    fc.account = p["creator_account_id"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(String::from);
                    fc.excluded = fc
                        .provider
                        .as_deref()
                        .is_some_and(|p| p.to_ascii_lowercase().contains("switchyard"));
                }
            } else if contains(line, b"\"turn_context\"") {
                if let Ok(v) = serde_json::from_slice::<Value>(line)
                    && let Some(m) = v["payload"]["model"].as_str()
                {
                    fc.model = Some(m.chars().take(120).collect());
                }
            } else if contains(line, b"\"token_count\"")
                && let Ok(v) = serde_json::from_slice::<Value>(line)
            {
                let info = &v["payload"]["info"];
                let Some(total) = info["total_token_usage"]["total_tokens"].as_u64() else {
                    return false;
                };
                if fc.last_total == Some(total) {
                    return false; // repeated report of the same cumulative usage
                }
                fc.last_total = Some(total);
                let Some(tokens) = codex_tokens(&info["last_token_usage"]) else {
                    return false;
                };
                let Some(ts_ms) = ms_of(&v["timestamp"]) else {
                    return false;
                };
                if fc.excluded {
                    excluded += 1;
                    return false;
                }
                let session = fc
                    .session
                    .clone()
                    .unwrap_or_else(|| session_fallback.clone());
                out.push(ExternalUsage {
                    collector: "native_codex".into(),
                    client_id: Some("external:codex_cli".into()),
                    client_name: Some("Codex CLI".into()),
                    native_id: format!("codex:{session}:{total}"),
                    ts_ms,
                    provider: fc.provider.clone().unwrap_or_else(|| "openai".into()),
                    model: fc.model.clone().unwrap_or_else(|| "unknown".into()),
                    account_label: Some(codex_label(fc.account.as_deref())),
                    billing: "unknown".into(),
                    tokens,
                    estimated_cost_micros: None,
                    reported_cost_micros: None,
                    disjoint: false,
                });
                return true;
            }
            false
        });
        cp.excluded += excluded;
        match result {
            Ok(pos) => {
                fc.offset = pos;
                fc.len = meta.len();
                if pos < meta.len() {
                    more = more || budget.exhausted();
                }
            }
            Err(_) => continue,
        }
        cp.files.insert(key, fc);
    }
    (out, more)
}

// ---------------------------------------------------------------------------------------------
// Claude Code projects
// ---------------------------------------------------------------------------------------------

fn scan_claude(
    cp: &mut Checkpoint,
    root: &FsPath,
    budget: &mut Budget,
) -> (Vec<ExternalUsage>, bool) {
    let mut files = Vec::new();
    jsonl_files(&root.join("projects"), 4, &mut files);
    let mut out = Vec::new();
    let mut more = false;
    for path in files {
        if budget.exhausted() {
            more = true;
            break;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let key = hash(&rel)[..24].to_string();
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let mut fc = cp.files.get(&key).cloned().unwrap_or_default();
        if fc.offset == meta.len() {
            continue;
        }
        if meta.len() < fc.offset {
            fc = FileCursor::default();
        }
        let result = read_lines(&path, fc.offset, budget, |line| {
            if !contains(line, b"\"assistant\"") || !contains(line, b"\"usage\"") {
                return false;
            }
            let Ok(v) = serde_json::from_slice::<Value>(line) else {
                return false;
            };
            if v["type"] != "assistant" || v["isApiErrorMessage"] == true {
                return false;
            }
            let msg = &v["message"];
            let model = msg["model"].as_str().unwrap_or("unknown");
            if model == "<synthetic>" {
                return false;
            }
            let Some(message_id) = msg["id"].as_str() else {
                return false;
            };
            let Some((tokens, _)) = tokens_from_usage(msg, "anthropic") else {
                return false;
            };
            let Some(ts_ms) = ms_of(&v["timestamp"]) else {
                return false;
            };
            // Claude Code writes one line per content block with the same message and request
            // id, and copies history into resumed sessions; the id pair dedups both.
            out.push(ExternalUsage {
                collector: "native_claude".into(),
                client_id: Some("external:claude_code".into()),
                client_name: Some("Claude Code".into()),
                native_id: format!(
                    "claude:{message_id}:{}",
                    v["requestId"].as_str().unwrap_or("")
                ),
                ts_ms,
                provider: "anthropic".into(),
                model: model.chars().take(120).collect(),
                account_label: Some("Claude Code on this device".into()),
                billing: "unknown".into(),
                tokens,
                estimated_cost_micros: None,
                reported_cost_micros: None,
                disjoint: false,
            });
            true
        });
        if let Ok(pos) = result {
            fc.offset = pos;
            fc.len = meta.len();
            cp.files.insert(key, fc);
        }
    }
    (out, more)
}

// ---------------------------------------------------------------------------------------------
// OpenCode SQLite
// ---------------------------------------------------------------------------------------------

/// OpenCode token record: `input` excludes cache, `output` excludes reasoning (observed in
/// OpenCode 2.0.21: reasoning can exceed output), so reasoning is added to output here.
pub fn opencode_tokens(
    input: Option<i64>,
    output: Option<i64>,
    reasoning: Option<i64>,
    read: Option<i64>,
    write: Option<i64>,
) -> Tokens {
    let u = |x: Option<i64>| x.filter(|v| *v >= 0).map(|v| v as u64);
    let (output, reasoning) = (u(output), u(reasoning));
    Tokens {
        input: u(input),
        cache_read: u(read),
        cache_write: u(write),
        output: match (output, reasoning) {
            (Some(o), Some(r)) => Some(o.saturating_add(r)),
            (o, _) => o,
        },
        reasoning,
        ..Default::default()
    }
}

fn scan_opencode(
    cp: &mut Checkpoint,
    root: &FsPath,
    budget: &mut Budget,
) -> Result<(Vec<ExternalUsage>, bool), FetchError> {
    let db = open_readonly(&opencode_db(root))?;
    let has = |t: &str| {
        db.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
            [t],
            |_| Ok(()),
        )
        .is_ok()
    };
    let sql = if has("session_message") {
        "SELECT id, time_updated, json_extract(data,'$.model.providerID'), json_extract(data,'$.model.id'),
            json_extract(data,'$.tokens.input'), json_extract(data,'$.tokens.output'), json_extract(data,'$.tokens.reasoning'),
            json_extract(data,'$.tokens.cache.read'), json_extract(data,'$.tokens.cache.write'),
            COALESCE(json_extract(data,'$.time.created'), time_created), json_extract(data,'$.time.completed'), json_extract(data,'$.cost')
         FROM session_message WHERE type='assistant' AND (time_updated > ?1 OR (time_updated = ?1 AND id > ?2))
         ORDER BY time_updated, id LIMIT ?3"
    } else if has("message") {
        "SELECT id, time_updated, json_extract(data,'$.providerID'), json_extract(data,'$.modelID'),
            json_extract(data,'$.tokens.input'), json_extract(data,'$.tokens.output'), json_extract(data,'$.tokens.reasoning'),
            json_extract(data,'$.tokens.cache.read'), json_extract(data,'$.tokens.cache.write'),
            COALESCE(json_extract(data,'$.time.created'), time_created), json_extract(data,'$.time.completed'), json_extract(data,'$.cost')
         FROM message WHERE json_extract(data,'$.role')='assistant' AND (time_updated > ?1 OR (time_updated = ?1 AND id > ?2))
         ORDER BY time_updated, id LIMIT ?3"
    } else {
        return Err(FetchError::Parse);
    };
    let mut stmt = db.prepare(sql).map_err(|_| FetchError::Parse)?;
    let mut out = Vec::new();
    let mut more = false;
    let (mut after_ts, mut after_id) = cp.sqlite.clone().unwrap_or((0, String::new()));
    loop {
        if budget.exhausted() {
            more = true;
            break;
        }
        let limit = 2000i64;
        let rows: Vec<_> = stmt
            .query_map(rusqlite::params![after_ts, after_id, limit], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    [
                        r.get::<_, Option<i64>>(4)?,
                        r.get::<_, Option<i64>>(5)?,
                        r.get::<_, Option<i64>>(6)?,
                        r.get::<_, Option<i64>>(7)?,
                        r.get::<_, Option<i64>>(8)?,
                    ],
                    r.get::<_, Option<i64>>(9)?,
                    r.get::<_, Option<i64>>(10)?,
                    r.get::<_, Option<f64>>(11)?,
                ))
            })
            .map_err(|_| FetchError::Parse)?
            .filter_map(Result::ok)
            .collect();
        let n = rows.len();
        for (id, updated, provider, model, t, created, completed, cost) in rows {
            after_ts = updated;
            after_id = id.clone();
            budget.rows += 1;
            // Unfinished messages come back with a newer `time_updated` once completed.
            if completed.is_none() {
                continue;
            }
            let provider = provider.unwrap_or_else(|| "unknown".into());
            let provider = if provider == "opencode-go" {
                "opencode_go".into()
            } else {
                provider
            };
            if provider.to_ascii_lowercase().contains("switchyard") {
                cp.excluded += 1;
                continue;
            }
            let tokens = opencode_tokens(t[0], t[1], t[2], t[3], t[4]);
            if !tokens.reported() {
                continue;
            }
            let ts_ms = created
                .or(completed)
                .map(|ms| if ms < 100_000_000_000 { ms * 1000 } else { ms })
                .unwrap_or(updated);
            out.push(ExternalUsage {
                collector: "native_opencode".into(),
                client_id: Some("external:opencode".into()),
                client_name: Some("OpenCode".into()),
                native_id: format!("opencode:{id}"),
                ts_ms,
                provider: provider.chars().take(64).collect(),
                model: model
                    .unwrap_or_else(|| "unknown".into())
                    .chars()
                    .take(120)
                    .collect(),
                account_label: Some("OpenCode on this device".into()),
                billing: if provider == "opencode_go" {
                    "subscription"
                } else {
                    "unknown"
                }
                .into(),
                tokens,
                // OpenCode's `cost` is its own list-price estimate, not a billed amount.
                estimated_cost_micros: cost
                    .filter(|amount| {
                        amount.is_finite()
                            && *amount >= 0.0
                            && *amount <= u64::MAX as f64 / 1_000_000.0
                    })
                    .map(|amount| (amount * 1_000_000.0).round() as u64),
                reported_cost_micros: None,
                disjoint: false,
            });
        }
        if (n as i64) < limit {
            break;
        }
    }
    cp.sqlite = Some((after_ts, after_id));
    Ok((out, more))
}

// ---------------------------------------------------------------------------------------------
// Cursor account usage events
// ---------------------------------------------------------------------------------------------

/// One Cursor dashboard usage event. Token counters are disjoint; `chargedCents` is what the
/// plan actually deducted.
pub fn cursor_event(e: &Value, account: &str, label: &str) -> Option<ExternalUsage> {
    let ts_ms = ms_of(&e["timestamp"])?;
    let model = e["model"].as_str().unwrap_or("unknown");
    let kind = e["kind"].as_str().unwrap_or("");
    let t = &e["tokenUsage"];
    let tokens = if t.is_object() {
        let n = |k: &str| num(&t[k]).filter(|v| *v >= 0.0).map(|v| v as u64);
        Tokens {
            input: n("inputTokens"),
            output: n("outputTokens"),
            cache_read: n("cacheReadTokens"),
            cache_write: n("cacheWriteTokens"),
            ..Default::default()
        }
    } else {
        Tokens::default()
    };
    let charged = num(&e["chargedCents"])
        .filter(|c| *c >= 0.0)
        .map(|c| (c * 10_000.0).round() as u64);
    let fingerprint = hash(&format!(
        "{model}|{kind}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        tokens.input,
        tokens.output,
        tokens.cache_read,
        tokens.cache_write,
        charged,
        num(&e["requestsCosts"])
    ));
    // Events billed to the user's own API key may have gone through a custom base URL.
    let byok = kind.to_ascii_lowercase().contains("api key")
        || kind.to_ascii_lowercase().contains("api_key");
    let kind = kind.to_ascii_lowercase();
    let invoice_charge = !byok && matches!(kind.as_str(), "usage_based" | "on_demand");
    let plan_usage = matches!(kind.as_str(), "included_in_pro" | "included");
    Some(ExternalUsage {
        collector: "cursor_events".into(),
        client_id: Some("external:cursor".into()),
        client_name: Some("Cursor".into()),
        native_id: format!("cursor:{account}:{ts_ms}:{}", &fingerprint[..16]),
        ts_ms,
        provider: "cursor".into(),
        model: model.chars().take(120).collect(),
        account_label: Some(label.chars().take(120).collect()),
        billing: if byok {
            "api_key"
        } else if invoice_charge || plan_usage {
            "subscription"
        } else {
            "unknown"
        }
        .into(),
        tokens,
        estimated_cost_micros: if !byok && !invoice_charge {
            charged
        } else {
            None
        },
        reported_cost_micros: if invoice_charge { charged } else { None },
        disjoint: !byok
            && matches!(
                kind.to_ascii_lowercase().as_str(),
                "included_in_pro" | "usage_based" | "max_mode" | "included" | "on_demand"
            ),
    })
}

async fn scan_cursor(
    app: &App,
    c: &Collector,
    cp: &mut Checkpoint,
    monitor_id: &str,
) -> Result<(Vec<ExternalUsage>, bool), FetchError> {
    let m: Monitor = app
        .store
        .get(usage_sources::MONITOR_KIND, monitor_id)
        .filter(|m: &Monitor| {
            m.enabled && m.provider == "cursor" && m.credential_source != "api_key"
        })
        .ok_or(FetchError::NotFound)?;
    let (cookie, identity) = usage_sources::cursor_cookie(&m).await?;
    match &cp.identity {
        Some(i) if *i != identity => return Err(FetchError::IdentityChanged),
        None => cp.identity = Some(identity.clone()),
        _ => {}
    }
    let account = &identity[..16];
    let label = format!("Cursor: {} (account {})", m.name, &identity[..8]);
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut w = cp.cursor.clone().unwrap_or_else(|| CursorWindow {
        start_ms: cp
            .cursor_done_until_ms
            .map(|d| d - CURSOR_OVERLAP_MS)
            .unwrap_or(now_ms - CURSOR_INITIAL_DAYS * 86_400_000),
        end_ms: now_ms,
        page: 1,
        total: None,
        seen: 0,
    });
    let base = usage_sources::endpoint(c, "cursor");
    let started = Instant::now();
    let mut out = Vec::new();
    for _ in 0..CURSOR_PAGES_PER_CHUNK {
        if started.elapsed() >= CHUNK_DEADLINE {
            cp.cursor = Some(w);
            return Ok((out, true));
        }
        let rb = app
            .client
            .post(format!("{base}/api/dashboard/get-filtered-usage-events"))
            .header("cookie", &cookie)
            .header("origin", "https://cursor.com")
            .header("accept", "application/json")
            .json(&json!({"page": w.page, "pageSize": CURSOR_PAGE_SIZE, "startDate": w.start_ms.to_string(), "endDate": w.end_ms.to_string()}));
        if !app
            .store
            .get::<Checkpoint>(CHECKPOINT_KIND, &cp.source)
            .is_some_and(|current| current.enabled && current.generation == cp.generation)
        {
            return Ok((Vec::new(), false));
        }
        let remaining = CHUNK_DEADLINE
            .min(app.timeout)
            .saturating_sub(started.elapsed());
        let v = match tokio::time::timeout(remaining, send_json(rb)).await {
            Ok(result) => result?,
            Err(_) => {
                // Retain completed pages, but retry the unfinished page next chunk.
                cp.cursor = Some(w);
                return Ok((out, true));
            }
        };
        if !v.is_object() {
            return Err(FetchError::Parse);
        }
        let total = v["totalUsageEventsCount"]
            .as_u64()
            .or(num(&v["totalUsageEventsCount"]).map(|n| n as u64));
        let events = match v.get("usageEventsDisplay") {
            Some(Value::Array(a)) => a.clone(),
            None => Vec::new(),
            Some(_) => return Err(FetchError::Parse),
        };
        w.total = total.or(w.total);
        w.seen += events.len() as u64;
        out.extend(
            events
                .iter()
                .filter_map(|e| cursor_event(e, account, &label)),
        );
        let done = (events.len() as u32) < CURSOR_PAGE_SIZE || w.total.is_some_and(|t| w.seen >= t);
        if done {
            cp.cursor = None;
            cp.cursor_done_until_ms = Some(w.end_ms);
            if w.total.is_some_and(|t| w.seen < t) {
                cp.message = Some(
                    "Cursor returned fewer events than it reported; history may be incomplete."
                        .into(),
                );
            }
            return Ok((out, false));
        }
        w.page += 1;
    }
    cp.cursor = Some(w);
    Ok((out, true))
}

// ---------------------------------------------------------------------------------------------
// Job
// ---------------------------------------------------------------------------------------------

fn label_for(source: &str) -> &'static str {
    match source {
        "opencode" => "OpenCode history on this device",
        "codex" => "Codex CLI sessions on this device",
        "claude" => "Claude Code projects on this device",
        _ => "Cursor account usage events",
    }
}
/// Called under the collector write lock so in-flight jobs cannot resurrect this source.
pub(crate) fn forget_monitor(app: &App, monitor_id: &str) -> rusqlite::Result<()> {
    app.store
        .delete(CHECKPOINT_KIND, &format!("cursor:{monitor_id}"))
}

fn valid_source(app: &App, source: &str) -> bool {
    match source.strip_prefix("cursor:") {
        Some(mid) => app
            .store
            .get::<Monitor>(usage_sources::MONITOR_KIND, mid)
            .is_some_and(|m| {
                m.enabled && m.provider == "cursor" && m.credential_source != "api_key"
            }),
        None => matches!(source, "opencode" | "codex" | "claude"),
    }
}

/// Imports one chunk of `cp`'s source. Returns true when more work remains.
async fn run_source(app: &App, c: &Collector, cp: &mut Checkpoint) -> bool {
    let result: Result<(Vec<ExternalUsage>, bool), FetchError> =
        if let Some(mid) = cp.source.strip_prefix("cursor:") {
            let mid = mid.to_string();
            scan_cursor(app, c, cp, &mid).await
        } else {
            match root_for(cp) {
                None => Err(FetchError::NotFound),
                Some(root) => {
                    let mut owned = cp.clone();
                    let joined = tokio::task::spawn_blocking(move || {
                        let mut budget = Budget::new();
                        let r = match owned.source.as_str() {
                            "codex" if root.join("sessions").is_dir() => {
                                Ok(scan_codex(&mut owned, &root, &mut budget))
                            }
                            "claude" if root.join("projects").is_dir() => {
                                Ok(scan_claude(&mut owned, &root, &mut budget))
                            }
                            "opencode" => scan_opencode(&mut owned, &root, &mut budget),
                            _ => Err(FetchError::NotFound),
                        };
                        (owned, r)
                    })
                    .await;
                    match joined {
                        Ok((owned, r)) => {
                            *cp = owned;
                            r
                        }
                        Err(_) => Err(FetchError::Parse),
                    }
                }
            }
        };
    cp.last_run_at = Some(now());
    match result {
        Ok((mut items, more)) => {
            let _write = c.write.lock().await;
            if !app
                .store
                .get::<Checkpoint>(CHECKPOINT_KIND, &cp.source)
                .is_some_and(|current| current.enabled && current.generation == cp.generation)
            {
                return false;
            }
            // Anthropic's message id is shared by gateway responses and native Claude logs.
            // Only exclude exact matches; other sources retain explicit unknown-overlap scope.
            let ids: Vec<String> = items
                .iter()
                .filter(|event| event.collector == "native_claude")
                .filter_map(|event| event.native_id.strip_prefix("claude:"))
                .filter_map(|key| key.split(':').next())
                .map(str::to_owned)
                .collect();
            let known: std::collections::HashSet<String> = ids
                .chunks(1000)
                .flat_map(|chunk| crate::usage::known_response_ids(&app.store, chunk))
                .collect();
            let before = items.len();
            items.retain(|event| {
                event.collector != "native_claude"
                    || !event
                        .native_id
                        .strip_prefix("claude:")
                        .and_then(|key| key.split(':').next())
                        .is_some_and(|id| known.contains(id))
            });
            cp.excluded += (before - items.len()) as u64;
            let mut stored = 0usize;
            for batch in items.chunks(BATCH) {
                match record_external(&app.store, batch) {
                    Ok(n) => stored += n,
                    Err(_) => {
                        cp.status = "error".into();
                        cp.message = Some("Could not save imported usage.".into());
                        return false;
                    }
                }
            }
            for e in &items {
                cp.note(e.ts_ms);
            }
            cp.imported += stored as u64;
            cp.status = if more { "partial" } else { "complete" }.into();
            if !more
                && !cp
                    .message
                    .as_deref()
                    .is_some_and(|m| m.starts_with("Cursor returned"))
            {
                cp.message = None;
            }
            more
        }
        Err(e) => {
            cp.status = if matches!(e, FetchError::NotFound) {
                "not_found"
            } else {
                "error"
            }
            .into();
            cp.message = Some(e.message(false).into());
            false
        }
    }
}

struct Running(Arc<Collector>, std::sync::Weak<crate::app::AppState>);
impl Drop for Running {
    fn drop(&mut self) {
        *self.0.native.current.lock().expect("native current") = None;
        self.0.native.running.store(false, Ordering::SeqCst);
        // Close the admission race: a request can queue just as the worker finishes.
        // Only new pending imports wake immediately; partial scans retain their bounded cadence.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let weak = self.1.clone();
            handle.spawn(async move {
                tokio::task::yield_now().await;
                if let Some(app) = weak.upgrade()
                    && app
                        .store
                        .list::<Checkpoint>(CHECKPOINT_KIND)
                        .iter()
                        .any(|cp| {
                            cp.enabled && cp.status == "pending" && valid_source(&app, &cp.source)
                        })
                {
                    start_job(&app);
                }
            });
        }
    }
}

/// Starts the background import job when it is not already running.
pub fn start_job(app: &App) -> bool {
    let c = usage_sources::collector(app);
    if c.native.running.swap(true, Ordering::SeqCst) {
        return false;
    }
    let weak = Arc::downgrade(app);
    tokio::spawn(async move {
        let _running = Running(c.clone(), weak.clone());
        let mut pending: Vec<String> = Vec::new();
        let mut visited = std::collections::HashSet::new();
        for _ in 0..MAX_CHUNKS {
            let Some(app) = weak.upgrade() else { return };
            // Admit newly queued sources between chunks so a large history cannot starve them.
            let queued: Vec<Checkpoint> = app.store.list(CHECKPOINT_KIND);
            for cp in queued {
                if cp.enabled
                    && cp.status == "pending"
                    && valid_source(&app, &cp.source)
                    && !pending.contains(&cp.source)
                {
                    pending.push(cp.source);
                }
            }
            if pending.is_empty() {
                let mut all: Vec<Checkpoint> = app.store.list(CHECKPOINT_KIND);
                all.retain(|cp| {
                    cp.enabled
                        && valid_source(&app, &cp.source)
                        && (!visited.contains(&cp.source) || cp.status == "pending")
                });
                pending = all.into_iter().map(|cp| cp.source).collect();
                if pending.is_empty() {
                    return;
                }
            }
            let source = pending.remove(0);
            visited.insert(source.clone());
            let Some(mut cp) = app
                .store
                .get::<Checkpoint>(CHECKPOINT_KIND, &source)
                .filter(|cp| cp.enabled)
            else {
                continue;
            };
            *c.native.current.lock().expect("native current") = Some((source.clone(), now()));
            let more = run_source(&app, &c, &mut cp).await;
            // Do not resurrect a source disabled or removed while this chunk ran.
            let _guard = c.write.lock().await;
            if app
                .store
                .get::<Checkpoint>(CHECKPOINT_KIND, &source)
                .is_some_and(|x| x.enabled && x.generation == cp.generation)
            {
                let _ = app.store.put(CHECKPOINT_KIND, &source, &cp);
            }
            drop(_guard);
            if more {
                pending.push(source);
            }
            drop(app);
            tokio::task::yield_now().await;
        }
    });
    true
}

/// Continues enabled imports (called by the poller).
pub fn resume(app: &App) {
    let any = app
        .store
        .list::<Checkpoint>(CHECKPOINT_KIND)
        .iter()
        .any(|cp| cp.enabled);
    if any {
        start_job(app);
    }
}

// ---------------------------------------------------------------------------------------------
// HTTP API
// ---------------------------------------------------------------------------------------------

pub(crate) fn router() -> Router<App> {
    Router::new()
        .route("/api/usage/native", get(status))
        .route("/api/usage/native/import", post(import))
        .route("/api/usage/native/{source}", delete(remove))
}

fn public(app: &App, source: &str, cp: Option<&Checkpoint>) -> Value {
    let available = match source {
        "opencode" => {
            usage_sources::opencode_data_dir().is_some_and(|d| d.join("opencode.db").is_file())
        }
        "codex" => usage_sources::codex_home().is_some_and(|d| d.join("sessions").is_dir()),
        "claude" => usage_sources::claude_home().is_some_and(|d| d.join("projects").is_dir()),
        _ => valid_source(app, source),
    };
    let provider = source.split(':').next().unwrap_or(source);
    json!({
        "id": source, "provider": provider, "label": label_for(source), "available": available,
        "enabled": cp.is_some_and(|c| c.enabled),
        "status": cp.map(|c| c.status.clone()).unwrap_or_else(|| "not_imported".into()),
        "message": cp.and_then(|c| c.message.clone()),
        "imported_events": cp.map_or(0, |c| c.imported),
        "excluded_gateway_events": cp.map_or(0, |c| c.excluded),
        "first_event_at": cp.and_then(|c| c.first_event_ms).and_then(iso_ms),
        "last_event_at": cp.and_then(|c| c.last_event_ms).and_then(iso_ms),
        "last_run_at": cp.and_then(|c| c.last_run_at.clone()),
        "custom_path": cp.is_some_and(|c| c.root.is_some()),
        "overlap": if provider == "cursor" { "disjoint_except_own_api_keys" } else { "unknown" },
        "account_attribution": if provider == "codex" { "workspace_from_record" } else if provider == "cursor" { "monitor_account" } else { "device" },
    })
}

async fn status(State(app): State<App>) -> Json<Value> {
    let c = usage_sources::collector(&app);
    let cps: HashMap<String, Checkpoint> = app
        .store
        .list::<Checkpoint>(CHECKPOINT_KIND)
        .into_iter()
        .map(|cp| (cp.source.clone(), cp))
        .collect();
    let mut ids: Vec<String> = vec!["opencode".into(), "codex".into(), "claude".into()];
    let mut monitors: Vec<Monitor> = app.store.list(usage_sources::MONITOR_KIND);
    monitors.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    ids.extend(
        monitors
            .iter()
            .filter(|m| m.provider == "cursor" && m.credential_source != "api_key")
            .map(|m| format!("cursor:{}", m.id)),
    );
    let current = c.native.current.lock().expect("native current").clone();
    Json(json!({
        "sources": ids.iter().map(|id| public(&app, id, cps.get(id))).collect::<Vec<_>>(),
        "job": {"running": c.native.running.load(Ordering::SeqCst), "source": current.as_ref().map(|x| x.0.clone()), "started_at": current.map(|x| x.1)},
        "notes": [
            "Native history is read-only and labelled by device or by the account the record names, never by the current sign-in.",
            "Native totals may include traffic the gateway also counted, so Usage shows them separately from gateway totals."
        ],
        "generated_at": now(),
    }))
}

#[derive(Deserialize)]
struct ImportBody {
    source: String,
    #[serde(default)]
    path: Option<String>,
}

async fn import(
    State(app): State<App>,
    Json(body): Json<ImportBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let source = body.source.trim().to_string();
    if source
        .strip_prefix("cursor:")
        .and_then(|id| app.store.get::<Monitor>(usage_sources::MONITOR_KIND, id))
        .is_some_and(|m| !m.enabled)
    {
        return Err(ApiError::bad(
            "Enable the Cursor watcher before importing its history.",
        ));
    }
    if !valid_source(&app, &source) {
        return Err(ApiError::bad(
            "source must be opencode, codex, claude or cursor:<monitor id>",
        ));
    }
    let path = body
        .path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(String::from);
    if let Some(p) = &path
        && (p.len() > 1024 || !FsPath::new(p).is_absolute() || source.starts_with("cursor:"))
    {
        return Err(ApiError::bad(
            "path must be an absolute path to the app's data directory",
        ));
    }
    let c = usage_sources::collector(&app);
    {
        let _guard = c.write.lock().await;
        let mut cp = app
            .store
            .get::<Checkpoint>(CHECKPOINT_KIND, &source)
            .unwrap_or_default();
        if cp.root != path {
            // A different root is a different history: start over (the ledger dedups re-reads).
            cp = Checkpoint::default();
        }
        cp.source = source.clone();
        cp.generation = crate::store::id();
        cp.enabled = true;
        cp.root = path;
        cp.status = "pending".into();
        cp.message = None;
        if cp.status.is_empty() || cp.status == "not_found" || cp.status == "error" {
            cp.status = "pending".into();
            cp.message = None;
        }
        app.store
            .put(CHECKPOINT_KIND, &source, &cp)
            .map_err(ApiError::db)?;
    }
    let started = start_job(&app);
    Ok((
        StatusCode::ACCEPTED,
        Json(
            json!({"accepted": true, "message": if started { "Import started. Native files are read-only; poll /api/usage/native for progress." } else { "Import queued behind the running import." }}),
        ),
    ))
}

async fn remove(
    State(app): State<App>,
    Path(source): Path<String>,
) -> Result<StatusCode, ApiError> {
    let c = usage_sources::collector(&app);
    let _guard = c.write.lock().await;
    if app
        .store
        .get::<Checkpoint>(CHECKPOINT_KIND, &source)
        .is_none()
    {
        return Err(ApiError::new(404, "Native source not imported"));
    }
    app.store
        .delete(CHECKPOINT_KIND, &source)
        .map_err(ApiError::db)?;
    Ok(StatusCode::NO_CONTENT)
}
