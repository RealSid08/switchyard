//! Usage ledger: durable, idempotent per-unit usage records, SQL aggregates and the usage API.
//!
//! An accounted *unit* is one HTTP/SSE request or one WebSocket turn. Each unit is written once
//! (primary key `event_id`) together with hourly and daily aggregates in one SQLite transaction,
//! so usage survives restarts and the 1000-row request-log cap. No prompts, outputs, tool
//! arguments or credentials are stored: only counts, timings, labels and public identifiers.
//!
//! Token dimensions are canonical and never overlap:
//! `total = input + cache_read + cache_write + output`, where `input` is uncached input,
//! `cache_write = cache_write_5m + cache_write_1h` when the TTL split is known, and `reasoning`
//! is a subset of `output`. Providers report differently and are normalized here:
//! - OpenAI (Responses and Chat): `input_tokens`/`prompt_tokens` INCLUDE cached and cache-write
//!   tokens (`*_tokens_details.cached_tokens` / `cache_write_tokens`); reasoning is included in
//!   output.
//! - Gemini (`usageMetadata`, also wrapped as `response.usageMetadata`): `promptTokenCount`
//!   INCLUDES `cachedContentTokenCount`; `thoughtsTokenCount` is reported apart from
//!   `candidatesTokenCount` and is added to output. Zero-valued fields are omitted by the API.
//! - Anthropic: `input_tokens`, `cache_read_input_tokens` and `cache_creation_input_tokens` are
//!   ADDITIVE; `cache_creation.ephemeral_{5m,1h}_input_tokens` gives the TTL split.
//!
//! A value the provider did not report stays `None` (unknown), never 0.
use std::collections::BTreeMap;

use axum::{
    Json, Router,
    extract::{Query, State},
    routing::{get, put},
};
use rusqlite::{Connection as Sqlite, params_from_iter, types::Value as Sql};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    app::{ApiError, App},
    pricing::{self, Override},
    store::{RequestRecord, Store, hash},
};

// ---------------------------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------------------------

/// Canonical, non-overlapping token counts for one unit. `None` = not reported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    /// Uncached input tokens.
    pub input: Option<u64>,
    pub cache_read: Option<u64>,
    /// All cache-write tokens (`cache_write_5m + cache_write_1h` when the split is known).
    pub cache_write: Option<u64>,
    pub cache_write_5m: Option<u64>,
    pub cache_write_1h: Option<u64>,
    /// All output tokens, including reasoning.
    pub output: Option<u64>,
    /// Reasoning/thinking tokens, a subset of `output`.
    pub reasoning: Option<u64>,
    /// Audio input tokens (Gemini), a subset of the input-side tokens; used only for pricing.
    #[serde(default)]
    pub audio_input: Option<u64>,
}
impl Tokens {
    /// True when the provider reported any input or output count.
    pub fn reported(&self) -> bool {
        self.input.is_some() || self.output.is_some()
    }
    /// Sum of the known dimensions.
    pub fn total(&self) -> u64 {
        [self.input, self.cache_read, self.cache_write, self.output]
            .iter()
            .flatten()
            .fold(0u64, |sum, value| sum.saturating_add(*value))
    }
}

/// How the reporting provider relates cache tokens to input tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Semantics {
    /// Cache tokens are a subset of the reported input (OpenAI, Gemini).
    Subset,
    /// Cache tokens are reported in addition to input (Anthropic).
    Additive,
}

fn n(v: &Value) -> Option<u64> {
    v.as_u64()
}

/// Normalizes a provider usage object into canonical [`Tokens`].
///
/// `v` may be a whole stream event or response (the usage object is located under `usage`,
/// `response.usage`, `message.usage`, `usageMetadata` or `response.usageMetadata`) or the usage
/// object itself. `provider_hint` (connection kind) decides ambiguous shapes. Returns `None`
/// when no usage is present. Exposed for native-log collectors so every source normalizes alike.
pub fn tokens_from_usage(v: &Value, provider_hint: &str) -> Option<(Tokens, Semantics)> {
    if let Some(m) = v
        .get("usageMetadata")
        .or_else(|| v.get("response").and_then(|r| r.get("usageMetadata")))
        .filter(|m| m.is_object())
    {
        return gemini(m).map(|t| (t, Semantics::Subset));
    }
    let u = v
        .get("usage")
        .or_else(|| v.get("response").and_then(|r| r.get("usage")))
        .or_else(|| v.get("message").and_then(|r| r.get("usage")))
        .filter(|u| u.is_object())
        .unwrap_or(v);
    if !u.is_object() {
        return None;
    }
    let has = |k: &str| u.get(k).is_some();
    if has("promptTokenCount") || has("candidatesTokenCount") {
        return gemini(u).map(|t| (t, Semantics::Subset));
    }
    if has("cache_creation_input_tokens") || has("cache_read_input_tokens") || has("cache_creation")
    {
        return anthropic(u).map(|t| (t, Semantics::Additive));
    }
    if has("prompt_tokens") || has("completion_tokens") {
        return openai(
            u,
            "prompt_tokens",
            "prompt_tokens_details",
            "completion_tokens",
            "completion_tokens_details",
        )
        .map(|t| (t, Semantics::Subset));
    }
    if !(has("input_tokens") || has("output_tokens")) {
        return None;
    }
    if provider_hint == "anthropic" && !has("input_tokens_details") && !has("output_tokens_details")
    {
        return anthropic(u).map(|t| (t, Semantics::Additive));
    }
    openai(
        u,
        "input_tokens",
        "input_tokens_details",
        "output_tokens",
        "output_tokens_details",
    )
    .map(|t| (t, Semantics::Subset))
}

fn openai(
    u: &Value,
    input: &str,
    input_details: &str,
    output: &str,
    output_details: &str,
) -> Option<Tokens> {
    let total_in = n(&u[input]);
    let out = n(&u[output]);
    if total_in.is_none() && out.is_none() {
        return None;
    }
    let mut t = Tokens {
        output: out,
        ..Tokens::default()
    };
    let details = &u[input_details];
    match total_in {
        Some(total) if details.is_object() => {
            let cached = n(&details["cached_tokens"]).unwrap_or(0);
            let write = n(&details["cache_write_tokens"]).unwrap_or(0);
            match total.checked_sub(cached).and_then(|r| r.checked_sub(write)) {
                Some(uncached) => {
                    t.input = Some(uncached);
                    t.cache_read = Some(cached);
                    t.cache_write = Some(write);
                }
                // Inconsistent split: keep the total, leave the cache dimensions unknown.
                None => t.input = Some(total),
            }
        }
        // No split reported: input may include unknown cached tokens; cache stays unknown.
        Some(total) => t.input = Some(total),
        None => {}
    }
    if out.is_some() {
        t.reasoning = match &u[output_details] {
            d if d.is_object() => Some(n(&d["reasoning_tokens"]).unwrap_or(0)),
            _ => None,
        };
    }
    Some(t)
}

fn anthropic(u: &Value) -> Option<Tokens> {
    let input = n(&u["input_tokens"]);
    let output = n(&u["output_tokens"]);
    let read = n(&u["cache_read_input_tokens"]);
    let write = n(&u["cache_creation_input_tokens"]);
    if input.is_none() && output.is_none() && read.is_none() && write.is_none() {
        return None;
    }
    let split = &u["cache_creation"];
    let (w5, w1) = if split.is_object() {
        (
            Some(n(&split["ephemeral_5m_input_tokens"]).unwrap_or(0)),
            Some(n(&split["ephemeral_1h_input_tokens"]).unwrap_or(0)),
        )
    } else {
        (None, None)
    };
    Some(Tokens {
        input,
        cache_read: read,
        cache_write: write,
        cache_write_5m: w5,
        cache_write_1h: w1,
        output,
        reasoning: None,
        audio_input: None,
    })
}

fn gemini(m: &Value) -> Option<Tokens> {
    // The Gemini API omits zero-valued counters, so an absent counter next to a present
    // promptTokenCount is 0, not unknown.
    let prompt = n(&m["promptTokenCount"])
        .map(|p| p.saturating_add(n(&m["toolUsePromptTokenCount"]).unwrap_or(0)));
    let candidates = n(&m["candidatesTokenCount"]);
    let thoughts = n(&m["thoughtsTokenCount"]);
    if prompt.is_none() && candidates.is_none() && thoughts.is_none() {
        return None;
    }
    let mut t = Tokens::default();
    if let Some(prompt) = prompt {
        let cached = n(&m["cachedContentTokenCount"]).unwrap_or(0);
        match prompt.checked_sub(cached) {
            Some(uncached) => {
                t.input = Some(uncached);
                t.cache_read = Some(cached);
                t.cache_write = Some(0);
            }
            None => t.input = Some(prompt),
        }
        let audio: u64 = ["promptTokensDetails", "cacheTokensDetails"]
            .iter()
            .flat_map(|k| m[*k].as_array().into_iter().flatten())
            .filter(|d| d["modality"] == "AUDIO")
            .filter_map(|d| n(&d["tokenCount"]))
            .fold(0u64, u64::saturating_add);
        t.audio_input = Some(audio);
        t.output = Some(
            candidates
                .unwrap_or(0)
                .saturating_add(thoughts.unwrap_or(0)),
        );
        t.reasoning = Some(thoughts.unwrap_or(0));
    } else {
        t.output = Some(
            candidates
                .unwrap_or(0)
                .saturating_add(thoughts.unwrap_or(0)),
        );
        t.reasoning = Some(thoughts.unwrap_or(0));
    }
    Some(t)
}

/// The provider response/message id carried by a response or stream event, if any:
/// `response.id` (OpenAI Responses), `message.id` (Anthropic `message_start`), a top-level
/// `id` (non-streaming bodies, Chat chunks) or `responseId` (Gemini). Bounded to 200 chars.
pub fn response_id_of(v: &Value) -> Option<String> {
    let id = v["response"]["id"]
        .as_str()
        .or(v["message"]["id"].as_str())
        .or(v["id"].as_str())
        .or(v["responseId"].as_str())
        .or(v["response"]["responseId"].as_str())?;
    (!id.is_empty() && id.len() <= 200).then(|| id.to_string())
}

/// A provider-reported cost in the usage object (`usage.cost`, USD), as micro-USD.
fn reported_cost(v: &Value) -> Option<u64> {
    let u = v
        .get("usage")
        .or_else(|| v.get("response").and_then(|r| r.get("usage")))?;
    let usd = u.get("cost")?.as_f64()?;
    (usd.is_finite() && (0.0..1_000_000.0).contains(&usd)).then(|| (usd * 1e6).round() as u64)
}

/// Accumulates repeated usage snapshots for one unit. Providers send cumulative snapshots
/// (Anthropic `message_start` then `message_delta`, Gemini on every chunk, OpenAI on the
/// terminal event, possibly twice), so newer values REPLACE older ones; nothing is summed.
/// For subset semantics the input side is replaced as a group so an earlier cache split never
/// mixes with a later unsplit total.
#[derive(Clone, Debug, Default)]
pub struct UsageAccumulator {
    pub tokens: Tokens,
    pub reported_cost_micros: Option<u64>,
    /// The provider's response/message id (first seen), used to deduplicate native logs.
    pub response_id: Option<String>,
}
impl UsageAccumulator {
    pub fn observe(&mut self, v: &Value, provider_hint: &str) {
        if self.response_id.is_none() {
            self.response_id = response_id_of(v);
        }
        if let Some(cost) = reported_cost(v) {
            self.reported_cost_micros = Some(cost);
        }
        let Some((newer, semantics)) = tokens_from_usage(v, provider_hint) else {
            return;
        };
        let t = &mut self.tokens;
        match semantics {
            Semantics::Additive => {
                let keep = |old: &mut Option<u64>, new: Option<u64>| {
                    if new.is_some() {
                        *old = new;
                    }
                };
                keep(&mut t.input, newer.input);
                keep(&mut t.cache_read, newer.cache_read);
                keep(&mut t.cache_write, newer.cache_write);
                keep(&mut t.cache_write_5m, newer.cache_write_5m);
                keep(&mut t.cache_write_1h, newer.cache_write_1h);
                keep(&mut t.output, newer.output);
                keep(&mut t.reasoning, newer.reasoning);
            }
            Semantics::Subset => {
                if newer.input.is_some() {
                    t.input = newer.input;
                    t.cache_read = newer.cache_read;
                    t.cache_write = newer.cache_write;
                    t.cache_write_5m = newer.cache_write_5m;
                    t.cache_write_1h = newer.cache_write_1h;
                    t.audio_input = newer.audio_input;
                }
                if newer.output.is_some() {
                    t.output = newer.output;
                    t.reasoning = newer.reasoning;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------------------------

/// Gateway traffic (default view).
pub const SOURCE_GATEWAY: &str = "gateway";
/// External/native usage whose overlap with gateway traffic is unknown.
pub const SOURCE_EXTERNAL: &str = "external";
/// External usage the collector proved does not overlap gateway traffic.
pub const SOURCE_EXTERNAL_DISJOINT: &str = "external_disjoint";

/// The public identity of the client key that made a request. Never the key itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientIdentity {
    pub id: String,
    pub name: String,
}
impl ClientIdentity {
    /// Requests made from the admin dashboard playground.
    pub fn playground() -> Self {
        Self {
            id: "playground".into(),
            name: "Playground".into(),
        }
    }
}

/// One accounted unit in the ledger.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageEvent {
    /// Idempotency key: request id, `ws:<connection>:<response id>`, `ws:<session>:<turn>` or
    /// `ext:<collector>:<hash>`.
    pub event_id: String,
    pub ts_ms: i64,
    pub source: String,
    pub request_id: Option<String>,
    /// WebSocket turn number (1-based) within its session.
    pub turn: Option<u32>,
    pub transport: String,
    pub connection_id: Option<String>,
    pub connection_name: Option<String>,
    pub provider: Option<String>,
    /// `api_key`, `subscription` or `unknown`.
    pub billing: String,
    pub requested_model: String,
    pub route: Option<String>,
    /// The model actually used upstream (route aliases resolved); prices use this.
    pub model: String,
    pub client_key_id: Option<String>,
    pub client_key_name: Option<String>,
    pub status: u16,
    /// `succeeded`, `failed` or `cancelled`.
    pub outcome: String,
    /// Constant label, never provider text.
    pub error: Option<String>,
    pub attempts: u32,
    pub failed_attempts: u32,
    pub failovers: u32,
    pub latency_ms: Option<u64>,
    pub ttfb_ms: Option<u64>,
    pub first_token_ms: Option<u64>,
    pub tokens: Tokens,
    /// Estimated list-price cost (micro-USD) frozen at record time; `None` when unpriced.
    pub cost_micros: Option<u64>,
    pub pricing_version: Option<String>,
    pub unpriced_reason: Option<String>,
    /// Cost reported by the provider itself (micro-USD), when it reports one.
    pub reported_cost_micros: Option<u64>,
    /// Provider response/message id, for deduplicating native logs against gateway traffic.
    #[serde(default)]
    pub response_id: Option<String>,
}

pub fn outcome_for(status: u16) -> &'static str {
    match status {
        499 => "cancelled",
        s if s < 400 => "succeeded",
        _ => "failed",
    }
}
/// `YYYY-MM-DD` for a Unix-millisecond timestamp (UTC).
pub fn day_string(ts_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ts_ms)
        .unwrap_or_default()
        .format("%Y-%m-%d")
        .to_string()
}
fn overrides(store: &Store) -> Vec<Override> {
    store.list(pricing::OVERRIDE_KIND)
}

/// Prices `e` in place with the card in force on its day (frozen into the event).
pub fn apply_price(e: &mut UsageEvent, overrides: &[Override]) {
    // Impossible per-request counters must not poison the integer ledger. Preserve the unit
    // and its outcome, but treat out-of-range dimensions as unreported rather than clamp them.
    for dimension in [
        &mut e.tokens.input,
        &mut e.tokens.cache_read,
        &mut e.tokens.cache_write,
        &mut e.tokens.cache_write_5m,
        &mut e.tokens.cache_write_1h,
        &mut e.tokens.output,
        &mut e.tokens.reasoning,
        &mut e.tokens.audio_input,
    ] {
        if dimension.is_some_and(|n| n > 1_000_000_000_000) {
            *dimension = None;
        }
    }

    let p = pricing::price_provider(
        e.provider.as_deref().unwrap_or(""),
        &e.model,
        e.ts_ms,
        &e.tokens,
        overrides,
    );
    e.cost_micros = p.cost_micros;
    e.pricing_version = p.version;
    e.unpriced_reason = p.reason.map(String::from);
}

/// Billing kind of a gateway connection.
pub fn billing_for(oauth: bool) -> &'static str {
    if oauth { "subscription" } else { "api_key" }
}

pub fn canonical_provider(provider: &str) -> &str {
    match provider {
        "opencode-go" => "opencode_go",
        _ => provider,
    }
}

/// A connection's protocol kind can differ from its vendor (Zen/Go speak several protocols).
pub fn provider_for(connection: &crate::store::Connection) -> &str {
    if let Ok(url) = url::Url::parse(&connection.base_url)
        && url.host_str() == Some("opencode.ai")
    {
        if url.path().starts_with("/zen/go/v1") {
            return "opencode_go";
        }
        if url.path().starts_with("/zen/v1") {
            return "opencode";
        }
    }
    &connection.kind
}
pub fn billing_for_connection(connection: &crate::store::Connection) -> &'static str {
    if provider_for(connection) == "opencode_go" {
        "subscription"
    } else {
        billing_for(connection.oauth)
    }
}

/// Facts the proxy knows about a unit beyond its request record.
pub struct UnitContext<'a> {
    pub provider: Option<&'a str>,
    pub billing: &'a str,
    pub upstream_model: Option<&'a str>,
    pub client: Option<&'a ClientIdentity>,
    pub usage: &'a UsageAccumulator,
}

/// Builds the ledger event for an HTTP/SSE request (or a WebSocket session that never got a
/// turn, such as a failed handshake) from its request record.
pub fn event_for_record(store: &Store, r: &RequestRecord, cx: &UnitContext<'_>) -> UsageEvent {
    let ts_ms = chrono::DateTime::parse_from_rfc3339(&r.timestamp)
        .map(|t| t.timestamp_millis())
        .unwrap_or_else(|_| chrono::Utc::now().timestamp_millis());
    let failed_attempts = r
        .attempts
        .iter()
        .filter(|a| a.error.is_some() || a.status == 0 || a.status >= 400)
        .count() as u32;
    let mut e = UsageEvent {
        event_id: r.id.clone(),
        ts_ms,
        source: SOURCE_GATEWAY.into(),
        request_id: Some(r.id.clone()),
        turn: None,
        transport: r.transport.clone(),
        connection_id: Some(r.connection_id.clone()),
        connection_name: Some(r.connection_name.clone()),
        provider: cx.provider.map(String::from),
        billing: cx.billing.into(),
        requested_model: r.model.clone(),
        route: r.route.clone(),
        model: cx.upstream_model.unwrap_or(&r.model).to_string(),
        client_key_id: cx.client.map(|c| c.id.clone()),
        client_key_name: cx.client.map(|c| c.name.clone()),
        status: r.status,
        outcome: outcome_for(r.status).into(),
        error: r.error.clone(),
        attempts: r.attempts.len() as u32,
        failed_attempts,
        failovers: r.failovers,
        latency_ms: Some(r.latency_ms),
        ttfb_ms: r.ttfb_ms,
        first_token_ms: r.first_token_ms,
        tokens: cx.usage.tokens,
        cost_micros: None,
        pricing_version: None,
        unpriced_reason: None,
        reported_cost_micros: cx.usage.reported_cost_micros,
        response_id: cx.usage.response_id.clone(),
    };
    apply_price(&mut e, &overrides(store));
    e
}

/// Usage reported by an external/native collector (for example a provider's own usage log).
///
/// Collectors must exclude traffic they can identify as gateway traffic. Mark `disjoint` only
/// when non-overlap with gateway traffic is proven; otherwise views keep the two separate.
#[derive(Clone, Debug)]
pub struct ExternalUsage {
    /// Collector name, e.g. `codex_sessions`.
    pub collector: String,
    /// Stable native identifier (provider response/message id plus native event key).
    pub native_id: String,
    pub ts_ms: i64,
    pub provider: String,
    pub model: String,
    /// Account or device label as known to the native source (never a credential).
    pub account_label: Option<String>,
    /// Client attribution; defaults to `external:<collector>`.
    pub client_id: Option<String>,
    pub client_name: Option<String>,
    /// `api_key`, `subscription` or `unknown`.
    pub billing: String,
    pub tokens: Tokens,
    /// Native SDK/list-price estimate, distinct from charged money.
    pub estimated_cost_micros: Option<u64>,
    pub reported_cost_micros: Option<u64>,
    pub disjoint: bool,
}

/// Records external usage idempotently. Returns how many units were new.
pub fn record_external(store: &Store, items: &[ExternalUsage]) -> Result<usize, rusqlite::Error> {
    let overrides = overrides(store);
    let events: Vec<UsageEvent> = items
        .iter()
        .map(|x| {
            let mut e = UsageEvent {
                event_id: format!("ext:{}:{}", x.collector, hash(&x.native_id)),
                ts_ms: x.ts_ms,
                source: if x.disjoint {
                    SOURCE_EXTERNAL_DISJOINT
                } else {
                    SOURCE_EXTERNAL
                }
                .into(),
                request_id: None,
                turn: None,
                transport: x.collector.clone(),
                connection_id: None,
                connection_name: x.account_label.clone(),
                provider: Some(canonical_provider(&x.provider).into()),
                billing: if canonical_provider(&x.provider) == "opencode_go" {
                    "subscription".into()
                } else {
                    x.billing.clone()
                },
                requested_model: x.model.clone(),
                route: None,
                model: x.model.clone(),
                client_key_id: Some(
                    x.client_id
                        .clone()
                        .unwrap_or_else(|| format!("external:{}", x.collector)),
                ),
                client_key_name: Some(x.client_name.clone().unwrap_or_else(|| x.collector.clone())),
                status: 0,
                outcome: "unknown".into(),
                error: None,
                attempts: 0,
                failed_attempts: 0,
                failovers: 0,
                latency_ms: None,
                ttfb_ms: None,
                first_token_ms: None,
                tokens: x.tokens,
                cost_micros: None,
                pricing_version: None,
                unpriced_reason: None,
                reported_cost_micros: x.reported_cost_micros,
                response_id: None,
            };
            apply_price(&mut e, &overrides);
            if let Some(amount) = x.estimated_cost_micros {
                e.cost_micros = Some(amount);
                e.pricing_version = Some(format!("native-source-value:{}", x.collector));
                e.unpriced_reason = None;
            }
            e
        })
        .collect();
    store.record_usage(&events)
}

/// Which of `ids` (provider response/message ids) the gateway ledger has recorded. At most
/// 1000 ids are checked per call.
pub fn known_response_ids(store: &Store, ids: &[String]) -> Vec<String> {
    let ids: Vec<&String> = ids
        .iter()
        .filter(|i| !i.is_empty() && i.len() <= 200)
        .take(1000)
        .collect();
    if ids.is_empty() {
        return Vec::new();
    }
    store
        .with_db(|db| {
            let mut found = Vec::new();
            for chunk in ids.chunks(200) {
                let sql = format!(
                    "SELECT DISTINCT response_id FROM usage_dedup WHERE source = 'gateway' AND response_id IN ({})",
                    vec!["?"; chunk.len()].join(",")
                );
                let mut stmt = db.prepare(&sql)?;
                let rows = stmt.query_map(params_from_iter(chunk.iter()), |r| r.get::<_, String>(0))?;
                for r in rows {
                    found.push(r?);
                }
            }
            Ok(found)
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// WebSocket turns
// ---------------------------------------------------------------------------------------------

/// At most this many turns can be open at once in one session (pipelined `response.create`).
const MAX_OPEN_TURNS: usize = 16;
/// Response ids remembered per session to ignore repeated terminal events.
const MAX_ACCOUNTED_IDS: usize = 4096;

/// One open WebSocket turn.
#[derive(Debug)]
struct Turn {
    n: u32,
    started: std::time::Instant,
    response_id: Option<String>,
    usage: UsageAccumulator,
    ttfb_ms: Option<u64>,
    first_token_ms: Option<u64>,
}

/// A finished turn, ready to become a ledger unit.
#[derive(Debug)]
pub struct ClosedTurn {
    pub n: u32,
    pub response_id: Option<String>,
    pub usage: UsageAccumulator,
    pub status: u16,
    pub latency_ms: u64,
    pub ttfb_ms: Option<u64>,
    pub first_token_ms: Option<u64>,
}

#[derive(Debug, PartialEq)]
enum Pending {
    /// The last frame was a terminal event for this response id (or the oldest open turn).
    Terminal(Option<String>),
    /// The last frame repeated a terminal event that was already accounted.
    Duplicate,
}

/// Per-turn accounting for one persistent Responses WebSocket session.
///
/// Turn 1 opens with the session; each forwarded `response.create` opens another. Frames are
/// matched to a turn by `response.id` (assigned from `response.created`), else to the oldest
/// open turn. A terminal event (`response.completed`/`incomplete`/`failed`/`error`) closes
/// exactly its turn; a repeated terminal for an already-accounted response id closes nothing.
/// A session-level end (client gone, idle, upstream lost) closes every open turn.
#[derive(Debug)]
pub struct WsTurns {
    open: std::collections::VecDeque<Turn>,
    next: u32,
    accounted: std::collections::HashSet<String>,
    pending: Option<Pending>,
}
impl Default for WsTurns {
    fn default() -> Self {
        Self::new()
    }
}
impl WsTurns {
    /// A session with turn 1 open.
    pub fn new() -> Self {
        let mut t = Self {
            open: Default::default(),
            next: 1,
            accounted: Default::default(),
            pending: None,
        };
        t.begin();
        t
    }
    /// Opens the next turn. Returns a turn closed early as cancelled when the open limit is hit.
    pub fn begin(&mut self) -> Option<ClosedTurn> {
        let evicted = if self.open.len() >= MAX_OPEN_TURNS {
            self.open.pop_front().map(|t| close_turn(t, 499))
        } else {
            None
        };
        self.open.push_back(Turn {
            n: self.next,
            started: std::time::Instant::now(),
            response_id: None,
            usage: UsageAccumulator::default(),
            ttfb_ms: None,
            first_token_ms: None,
        });
        self.next += 1;
        evicted
    }
    pub fn has_open(&self) -> bool {
        !self.open.is_empty()
    }
    fn target(&mut self, id: Option<&str>) -> Option<&mut Turn> {
        let at = id
            .and_then(|id| {
                self.open
                    .iter()
                    .position(|t| t.response_id.as_deref() == Some(id))
            })
            .unwrap_or(0);
        self.open.get_mut(at)
    }
    /// Observes one upstream frame (JSON).
    pub fn frame(&mut self, v: &Value, provider_hint: &str) {
        self.pending = None;
        let id = v["response"]["id"].as_str().filter(|s| !s.is_empty());
        if v["type"] == "response.created"
            && let Some(id) = id
            && !self
                .open
                .iter()
                .any(|t| t.response_id.as_deref() == Some(id))
            && let Some(t) = self.open.iter_mut().find(|t| t.response_id.is_none())
        {
            t.response_id = Some(id.to_string());
        }
        let terminal = matches!(
            v["type"].as_str(),
            Some("response.completed" | "response.incomplete" | "response.failed" | "error")
        );
        if terminal
            && let Some(id) = id
            && self.accounted.contains(id)
            && !self
                .open
                .iter()
                .any(|t| t.response_id.as_deref() == Some(id))
        {
            self.pending = Some(Pending::Duplicate);
            return;
        }
        if let Some(t) = self.target(id) {
            t.usage.observe(v, provider_hint);
        }
        if terminal {
            self.pending = Some(Pending::Terminal(id.map(String::from)));
        }
    }
    /// The first frame of the turn arrived `ms` after the session started; recorded relative to
    /// the turn's own start.
    pub fn first_byte(&mut self) {
        if let Some(t) = self.open.front_mut()
            && t.ttfb_ms.is_none()
        {
            t.ttfb_ms = Some(t.started.elapsed().as_millis() as u64);
        }
    }
    /// Notes a non-empty output delta for its turn.
    pub fn output(&mut self, v: &Value, is_delta: bool) {
        if !is_delta {
            return;
        }
        let id = v["response"]["id"].as_str().or(v["response_id"].as_str());
        if let Some(t) = self.target(id)
            && t.first_token_ms.is_none()
        {
            t.first_token_ms = Some(t.started.elapsed().as_millis() as u64);
        }
    }
    /// Closes the turn(s) the session status applies to: the turn of the terminal frame just
    /// observed, nothing for a duplicate terminal, or every open turn for a session-level end.
    pub fn close(&mut self, status: u16) -> Vec<ClosedTurn> {
        let closed: Vec<Turn> = match self.pending.take() {
            Some(Pending::Duplicate) => Vec::new(),
            Some(Pending::Terminal(id)) => {
                let at = id
                    .as_deref()
                    .and_then(|id| {
                        self.open
                            .iter()
                            .position(|t| t.response_id.as_deref() == Some(id))
                    })
                    .unwrap_or(0);
                self.open.remove(at).into_iter().collect()
            }
            None => self.open.drain(..).collect(),
        };
        closed
            .into_iter()
            .map(|t| {
                if let Some(id) = &t.response_id {
                    if self.accounted.len() >= MAX_ACCOUNTED_IDS {
                        self.accounted.clear();
                    }
                    self.accounted.insert(id.clone());
                }
                close_turn(t, status)
            })
            .collect()
    }
}
fn close_turn(t: Turn, status: u16) -> ClosedTurn {
    ClosedTurn {
        n: t.n,
        response_id: t.response_id.or(t.usage.response_id.clone()),
        latency_ms: t.started.elapsed().as_millis() as u64,
        usage: t.usage,
        status,
        ttfb_ms: t.ttfb_ms,
        first_token_ms: t.first_token_ms,
    }
}

/// Builds the ledger unit for a WebSocket turn of `session`. Turn 1 carries the session's
/// handshake attempts and failovers; later turns made no new upstream attempts.
pub fn event_for_turn(
    store: &Store,
    session: &RequestRecord,
    turn: ClosedTurn,
    cx: &UnitContext<'_>,
) -> UsageEvent {
    let event_id = match &turn.response_id {
        Some(rid) => format!("ws:{}:{rid}", session.connection_id),
        None => format!("ws:{}:{}", session.id, turn.n),
    };
    let mut e = event_for_record(store, session, cx);
    e.event_id = event_id;
    e.ts_ms = chrono::Utc::now().timestamp_millis() - turn.latency_ms as i64;
    e.turn = Some(turn.n);
    e.status = turn.status;
    e.outcome = outcome_for(turn.status).into();
    if turn.status < 400 {
        e.error = None;
    }
    if turn.n != 1 {
        e.attempts = 0;
        e.failed_attempts = 0;
        e.failovers = 0;
    }
    e.latency_ms = Some(turn.latency_ms);
    e.ttfb_ms = turn.ttfb_ms;
    e.first_token_ms = turn.first_token_ms;
    e.tokens = turn.usage.tokens;
    e.reported_cost_micros = turn.usage.reported_cost_micros;
    e.response_id = turn.response_id;
    apply_price(&mut e, &overrides(store));
    e
}

// ---------------------------------------------------------------------------------------------
// Aggregate columns
// ---------------------------------------------------------------------------------------------

/// Aggregate metric columns (all summed, except `lat_max`, which keeps the maximum).
pub(crate) const METRICS: &[&str] = &[
    "units",
    "succeeded",
    "failed",
    "cancelled",
    "attempts",
    "failed_attempts",
    "failovers",
    "in_sum",
    "in_n",
    "cr_sum",
    "cr_n",
    "cw_sum",
    "cw_n",
    "cw5_sum",
    "cw1h_sum",
    "out_sum",
    "out_n",
    "rs_sum",
    "rs_n",
    "usage_n",
    "ce_n",
    "ce_total",
    "ce_read",
    "cost_sum",
    "cost_api",
    "cost_sub",
    "api_priced",
    "sub_priced",
    "priced",
    "unpriced",
    "rep_sum",
    "rep_n",
    "lat_sum",
    "lat_n",
    "lat_max",
    "ttfb_sum",
    "ttfb_n",
    "ftk_sum",
    "ftk_n",
    "gen_out",
    "gen_ms",
];
/// Index of a metric column in [`METRICS`].
fn col(name: &str) -> usize {
    METRICS
        .iter()
        .position(|m| *m == name)
        .expect("metric column")
}

fn i(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// The aggregate contribution of one event, in [`METRICS`] order.
pub(crate) fn metric_values(e: &UsageEvent) -> Vec<i64> {
    let mut v = vec![0i64; METRICS.len()];
    let mut set = |name: &str, value: i64| v[col(name)] = value;
    let t = &e.tokens;
    set("units", 1);
    if ["succeeded", "failed", "cancelled"].contains(&e.outcome.as_str()) {
        set(&e.outcome, 1);
    }
    set("attempts", i64::from(e.attempts));
    set("failed_attempts", i64::from(e.failed_attempts));
    set("failovers", i64::from(e.failovers));
    for (sum, count, value) in [
        ("in_sum", "in_n", t.input),
        ("cr_sum", "cr_n", t.cache_read),
        ("cw_sum", "cw_n", t.cache_write),
        ("out_sum", "out_n", t.output),
        ("rs_sum", "rs_n", t.reasoning),
    ] {
        if let Some(x) = value {
            set(sum, i(x));
            set(count, 1);
        }
    }
    if let (Some(w5), Some(w1)) = (t.cache_write_5m, t.cache_write_1h) {
        set("cw5_sum", i(w5));
        set("cw1h_sum", i(w1));
    }
    if t.reported() {
        set("usage_n", 1);
    }
    if let (Some(a), Some(b), Some(c)) = (t.input, t.cache_read, t.cache_write) {
        set("ce_n", 1);
        set("ce_total", i(a.saturating_add(b).saturating_add(c)));
        set("ce_read", i(b));
    }
    match e.cost_micros {
        Some(c) => {
            set("cost_sum", i(c));
            match e.billing.as_str() {
                "subscription" => {
                    set("cost_sub", i(c));
                    set("sub_priced", 1);
                }
                "api_key" => {
                    set("cost_api", i(c));
                    set("api_priced", 1);
                }
                _ => {}
            }
            set("priced", 1);
        }
        None => set("unpriced", 1),
    }
    if let Some(c) = e.reported_cost_micros {
        set("rep_sum", i(c));
        set("rep_n", 1);
    }
    if let Some(l) = e.latency_ms {
        set("lat_sum", i(l));
        set("lat_n", 1);
        set("lat_max", i(l));
    }
    if let Some(x) = e.ttfb_ms {
        set("ttfb_sum", i(x));
        set("ttfb_n", 1);
    }
    if let Some(x) = e.first_token_ms {
        set("ftk_sum", i(x));
        set("ftk_n", 1);
        if let (Some(out), Some(l)) = (t.output, e.latency_ms)
            && l > x
            && out > 0
        {
            set("gen_out", i(out));
            set("gen_ms", i(l - x));
        }
    }
    v
}

// ---------------------------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------------------------

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;
const MAX_CUSTOM_DAYS: i64 = 366;
const MAX_MONTHS: i64 = 120;
const MAX_GROUP_ROWS: usize = 100;

#[derive(Debug, Default, Deserialize)]
pub struct UsageQuery {
    pub window: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub connection_id: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub client_key_id: Option<String>,
    pub source: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Granularity {
    Hour,
    Day,
    Month,
}

/// A resolved query window over one aggregate table.
#[derive(Debug)]
struct Window {
    key: &'static str,
    table: &'static str,
    /// Inclusive bucket range in the table's units (hours or days since the epoch).
    lo: i64,
    hi: i64,
    granularity: Granularity,
    from_ms: Option<i64>,
    to_ms: i64,
}

fn parse_day(s: &str) -> Result<i64, ApiError> {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map(|d| {
            d.and_hms_opt(0, 0, 0)
                .expect("midnight")
                .and_utc()
                .timestamp_millis()
                / DAY_MS
        })
        .map_err(|_| ApiError::bad("from and to must be UTC dates as YYYY-MM-DD"))
}

fn resolve_window(q: &UsageQuery, now_ms: i64) -> Result<Window, ApiError> {
    if q.from.is_some() || q.to.is_some() {
        let (Some(from), Some(to)) = (&q.from, &q.to) else {
            return Err(ApiError::bad("A custom window needs both from and to"));
        };
        let (lo, hi) = (parse_day(from)?, parse_day(to)?);
        if hi < lo {
            return Err(ApiError::bad("to must not be before from"));
        }
        if hi - lo + 1 > MAX_CUSTOM_DAYS {
            return Err(ApiError::bad("A custom window can span at most 366 days"));
        }
        return Ok(Window {
            key: "custom",
            table: "usage_daily",
            lo,
            hi,
            granularity: Granularity::Day,
            from_ms: Some(lo * DAY_MS),
            to_ms: ((hi + 1) * DAY_MS).min(now_ms),
        });
    }
    let hour = now_ms.div_euclid(HOUR_MS);
    let rolling = |key, hours: i64, granularity| Window {
        key,
        table: "usage_hourly",
        lo: hour - hours + 1,
        hi: hour,
        granularity,
        from_ms: Some((hour - hours + 1) * HOUR_MS),
        to_ms: now_ms,
    };
    match q.window.as_deref().unwrap_or("24h") {
        "24h" => Ok(rolling("24h", 24, Granularity::Hour)),
        "7d" => Ok(rolling("7d", 7 * 24, Granularity::Day)),
        "30d" => Ok(rolling("30d", 30 * 24, Granularity::Day)),
        "all" => Ok(Window {
            key: "all",
            table: "usage_daily",
            lo: i64::MIN,
            hi: now_ms.div_euclid(DAY_MS),
            granularity: Granularity::Month,
            from_ms: None,
            to_ms: now_ms,
        }),
        _ => Err(ApiError::bad("window must be 24h, 7d, 30d or all")),
    }
}

/// Source values matched by a `source` filter.
fn sources_for(source: &str) -> Result<&'static [&'static str], ApiError> {
    match source {
        "gateway" => Ok(&[SOURCE_GATEWAY]),
        "external" => Ok(&[SOURCE_EXTERNAL, SOURCE_EXTERNAL_DISJOINT]),
        "all" => Ok(&[SOURCE_GATEWAY, SOURCE_EXTERNAL, SOURCE_EXTERNAL_DISJOINT]),
        _ => Err(ApiError::bad("source must be gateway, external or all")),
    }
}

/// Summed metric columns.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Agg(pub Vec<i64>);
impl Agg {
    fn zero() -> Self {
        Self(vec![0; METRICS.len()])
    }
    fn get(&self, name: &str) -> i64 {
        self.0[col(name)]
    }
    fn add(&mut self, other: &Agg) {
        let max = col("lat_max");
        for (k, (a, b)) in self.0.iter_mut().zip(&other.0).enumerate() {
            *a = if k == max {
                (*a).max(*b)
            } else {
                a.saturating_add(*b)
            };
        }
    }
}

struct Filters<'a> {
    sources: &'a [&'a str],
    connection_id: Option<&'a str>,
    provider: Option<&'a str>,
    model: Option<&'a str>,
    client_key_id: Option<&'a str>,
}

fn where_clause(w: &Window, f: &Filters<'_>) -> (String, Vec<Sql>) {
    let mut sql = String::from("bucket >= ? AND bucket <= ?");
    let mut params = vec![Sql::Integer(w.lo), Sql::Integer(w.hi)];
    sql.push_str(&format!(
        " AND source IN ({})",
        vec!["?"; f.sources.len()].join(",")
    ));
    params.extend(f.sources.iter().map(|s| Sql::Text((*s).into())));
    for (column, value) in [
        ("connection_id", f.connection_id),
        ("provider", f.provider),
        ("model", f.model),
        ("client_key_id", f.client_key_id),
    ] {
        if let Some(value) = value {
            sql.push_str(&format!(" AND {column} = ?"));
            params.push(Sql::Text(value.into()));
        }
    }
    (sql, params)
}

fn select_metrics() -> String {
    METRICS
        .iter()
        .map(|m| {
            if *m == "lat_max" {
                "MAX(lat_max)".to_string()
            } else {
                format!("SUM({m})")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Grouped aggregate query. `keys` are grouping expressions; `labels` are aggregate label
/// expressions (for example the latest display name). Both are returned as text, keys first.
fn grouped(
    db: &Sqlite,
    w: &Window,
    f: &Filters<'_>,
    keys: &[&str],
    labels: &[&str],
    order_limit: Option<usize>,
) -> rusqlite::Result<Vec<(Vec<String>, Agg)>> {
    let (cond, params) = where_clause(w, f);
    let columns: Vec<&str> = keys.iter().chain(labels).copied().collect();
    let width = columns.len();
    let key_sql = if columns.is_empty() {
        String::new()
    } else {
        format!("{},", columns.join(","))
    };
    let mut sql = format!(
        "SELECT {key_sql}{} FROM {} WHERE {cond}",
        select_metrics(),
        w.table
    );
    if !keys.is_empty() {
        let group: Vec<String> = (1..=keys.len()).map(|k| k.to_string()).collect();
        sql.push_str(&format!(" GROUP BY {}", group.join(",")));
    }
    if let Some(limit) = order_limit {
        sql.push_str(&format!(
            " ORDER BY SUM(cost_sum) DESC, SUM(units) DESC LIMIT {limit}"
        ));
    }
    let mut stmt = db.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(params), |row| {
        let mut key = Vec::with_capacity(width);
        for k in 0..width {
            let v: Sql = row.get(k)?;
            key.push(match v {
                Sql::Text(s) => s,
                Sql::Integer(x) => x.to_string(),
                _ => String::new(),
            });
        }
        let mut values = Vec::with_capacity(METRICS.len());
        for k in 0..METRICS.len() {
            values.push(row.get::<_, Option<i64>>(width + k)?.unwrap_or(0));
        }
        Ok((key, Agg(values)))
    })?;
    rows.collect()
}

fn opt(s: &str) -> Value {
    if s.is_empty() { Value::Null } else { json!(s) }
}
fn ratio(num: i64, den: i64) -> Value {
    if den > 0 {
        json!(num as f64 / den as f64)
    } else {
        Value::Null
    }
}
fn avg(sum: i64, count: i64) -> Value {
    if count > 0 {
        json!((sum as f64 / count as f64).round() as i64)
    } else {
        Value::Null
    }
}
fn money(micros: i64, known: bool) -> (Value, Value) {
    if known {
        let m = u64::try_from(micros).unwrap_or(0);
        (json!(m), json!(pricing::usd_string(m)))
    } else {
        (Value::Null, Value::Null)
    }
}

/// Public JSON for summed metrics (the `Metrics` object of the contract).
pub(crate) fn metrics_json(a: &Agg) -> Value {
    let g = |m| a.get(m);
    let units = g("units");
    let tokens_total = g("in_sum")
        .saturating_add(g("cr_sum"))
        .saturating_add(g("cw_sum"))
        .saturating_add(g("out_sum"));
    let (est, est_usd) = money(g("cost_sum"), g("priced") > 0);
    let (rep, rep_usd) = money(g("rep_sum"), g("rep_n") > 0);
    let mut unit_counts = json!({"total": units, "succeeded": g("succeeded"), "failed": g("failed"), "cancelled": g("cancelled")});
    let unknown_outcomes = units - g("succeeded") - g("failed") - g("cancelled");
    if unknown_outcomes > 0 {
        unit_counts["unknown"] = json!(unknown_outcomes);
    }
    json!({
        "units": unit_counts,
        "success_rate": ratio(g("succeeded"), g("succeeded") + g("failed")),
        "attempts": {"total": g("attempts"), "failed": g("failed_attempts"), "failovers": g("failovers")},
        "tokens": {
            "input": g("in_sum"), "cache_read": g("cr_sum"), "cache_write": g("cw_sum"),
            "cache_write_5m": g("cw5_sum"), "cache_write_1h": g("cw1h_sum"),
            "output": g("out_sum"), "reasoning": g("rs_sum"), "total": tokens_total,
            "known_units": {"input": g("in_n"), "cache_read": g("cr_n"), "cache_write": g("cw_n"), "output": g("out_n"), "reasoning": g("rs_n")},
            "usage_reported_units": g("usage_n"),
            "usage_missing_units": units - g("usage_n"),
        },
        "cache": {"read_ratio": ratio(g("ce_read"), g("ce_total")), "eligible_units": g("ce_n")},
        "cost": {
            "estimated_micros": est, "estimated_usd": est_usd,
            "api_estimated_micros": if g("api_priced") > 0 { json!(g("cost_api")) } else { Value::Null },
            "subscription_equivalent_micros": if g("sub_priced") > 0 { json!(g("cost_sub")) } else { Value::Null },
            "unknown_billing_micros": if g("priced") > g("api_priced") + g("sub_priced") { json!(g("cost_sum") - g("cost_api") - g("cost_sub")) } else { Value::Null },
            "unknown_billing_units": g("priced") - g("api_priced") - g("sub_priced"),
            "reported_micros": rep, "reported_usd": rep_usd,
            "priced_units": g("priced"), "unpriced_units": g("unpriced"),
        },
        "latency_ms": {"avg": avg(g("lat_sum"), g("lat_n")), "max": if g("lat_n") > 0 { json!(g("lat_max")) } else { Value::Null }, "samples": g("lat_n")},
        "ttfb_ms": {"avg": avg(g("ttfb_sum"), g("ttfb_n")), "samples": g("ttfb_n")},
        "first_token_ms": {"avg": avg(g("ftk_sum"), g("ftk_n")), "samples": g("ftk_n")},
        "throughput": {
            "output_tokens_per_second": if g("gen_ms") > 0 { json!(g("gen_out") as f64 * 1000.0 / g("gen_ms") as f64) } else { Value::Null },
            "samples": if g("gen_ms") > 0 { g("ftk_n").min(g("out_n")) } else { 0 },
        },
    })
}

fn iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// Month index (months since 1970-01) of a day index.
fn month_of_day(day: i64) -> i64 {
    use chrono::Datelike;
    let d = chrono::DateTime::from_timestamp_millis(day * DAY_MS).unwrap_or_default();
    i64::from(d.year() - 1970) * 12 + i64::from(d.month0())
}
fn month_start_ms(month: i64) -> i64 {
    let year = 1970 + month.div_euclid(12);
    let m = month.rem_euclid(12) as u32 + 1;
    chrono::NaiveDate::from_ymd_opt(year as i32, m, 1)
        .map(|d| {
            d.and_hms_opt(0, 0, 0)
                .expect("midnight")
                .and_utc()
                .timestamp_millis()
        })
        .unwrap_or_default()
}

/// Series buckets for the window: `(bucket key, bucket start ms)`, ascending.
fn series_buckets(w: &Window, first_data_day: Option<i64>) -> Vec<(i64, i64)> {
    match (w.granularity, w.table) {
        (Granularity::Hour, _) => (w.lo..=w.hi).map(|h| (h, h * HOUR_MS)).collect(),
        (Granularity::Day, "usage_hourly") => {
            let (lo, hi) = (
                (w.lo * HOUR_MS).div_euclid(DAY_MS),
                (w.hi * HOUR_MS).div_euclid(DAY_MS),
            );
            (lo..=hi).map(|d| (d, d * DAY_MS)).collect()
        }
        (Granularity::Day, _) => (w.lo..=w.hi).map(|d| (d, d * DAY_MS)).collect(),
        (Granularity::Month, _) => {
            let Some(first) = first_data_day else {
                return Vec::new();
            };
            let hi = month_of_day(w.hi);
            let lo = month_of_day(first).max(hi - MAX_MONTHS + 1);
            (lo..=hi).map(|m| (m, month_start_ms(m))).collect()
        }
    }
}

/// Usage summary for the `GET /api/usage` contract.
pub fn summary(store: &Store, q: &UsageQuery, now_ms: i64) -> Result<Value, ApiError> {
    let w = resolve_window(q, now_ms)?;
    let source = q.source.as_deref().unwrap_or("gateway");
    let sources = sources_for(source)?;
    for v in [&q.connection_id, &q.provider, &q.model, &q.client_key_id]
        .into_iter()
        .flatten()
    {
        if v.is_empty() || v.len() > 200 {
            return Err(ApiError::bad("Filters must be 1-200 characters"));
        }
    }
    let f = Filters {
        sources,
        connection_id: q.connection_id.as_deref(),
        provider: q.provider.as_deref(),
        model: q.model.as_deref(),
        client_key_id: q.client_key_id.as_deref(),
    };
    // Store uses a non-reentrant mutex. Load auxiliary state before holding its SQL lock.
    let snapshot = SummarySnapshot {
        overrides: overrides(store),
        requests: store.requests(1000),
    };
    store
        .with_db(|db| build_summary(db, &snapshot, &w, &f, source, now_ms))
        .map_err(ApiError::db)
}

struct SummarySnapshot {
    overrides: Vec<Override>,
    requests: Vec<RequestRecord>,
}

fn build_summary(
    db: &Sqlite,
    snapshot: &SummarySnapshot,
    w: &Window,
    f: &Filters<'_>,
    source: &str,
    now_ms: i64,
) -> rusqlite::Result<Value> {
    // Per-source totals decide whether a combined total is honest.
    let by_source: BTreeMap<String, Agg> = grouped(db, w, f, &["source"], &[], None)?
        .into_iter()
        .map(|(k, a)| (k[0].clone(), a))
        .collect();
    let unknown_overlap = by_source
        .get(SOURCE_EXTERNAL)
        .is_some_and(|a| a.get("units") > 0)
        && by_source
            .get(SOURCE_GATEWAY)
            .is_some_and(|a| a.get("units") > 0);
    let combine = !(source == "all" && unknown_overlap);
    let mut totals = Agg::zero();
    for a in by_source.values() {
        totals.add(a);
    }

    // Series.
    let bucket_expr = match (w.granularity, w.table) {
        (Granularity::Day, "usage_hourly") => "(bucket * 3600000) / 86400000",
        _ => "bucket",
    };
    let first_day: Option<i64> = if w.granularity == Granularity::Month {
        let (cond, params) = where_clause(w, f);
        db.query_row(
            &format!("SELECT MIN(bucket) FROM usage_daily WHERE {cond}"),
            params_from_iter(params),
            |r| r.get(0),
        )?
    } else {
        None
    };
    let keys: &[&str] = if combine {
        &[bucket_expr]
    } else {
        &[bucket_expr, "source"]
    };
    let mut series_rows: BTreeMap<i64, BTreeMap<String, Agg>> = BTreeMap::new();
    for (k, a) in grouped(db, w, f, keys, &[], None)? {
        let mut bucket: i64 = k[0].parse().unwrap_or(0);
        if w.granularity == Granularity::Month {
            bucket = month_of_day(bucket);
        }
        let src = if combine { String::new() } else { k[1].clone() };
        series_rows
            .entry(bucket)
            .or_default()
            .entry(src)
            .or_insert_with(Agg::zero)
            .add(&a);
    }
    let series: Vec<Value> = series_buckets(w, first_day)
        .into_iter()
        .map(|(b, start)| {
            let row = series_rows.remove(&b).unwrap_or_default();
            if combine {
                let a = row.get("").cloned().unwrap_or_else(Agg::zero);
                json!({"bucket_start": iso(start), "metrics": metrics_json(&a), "by_source": Value::Null})
            } else {
                let per: serde_json::Map<String, Value> = row
                    .iter()
                    .map(|(s, a)| (s.clone(), metrics_json(a)))
                    .collect();
                json!({"bucket_start": iso(start), "metrics": Value::Null, "by_source": per})
            }
        })
        .collect();

    // Breakdowns. With unknown overlap, rows also carry their source and are never merged.
    let extra = if combine { vec![] } else { vec!["source"] };
    let with = |base: &[&'static str]| -> Vec<&'static str> {
        let mut v = base.to_vec();
        v.extend(extra.iter().copied());
        v
    };
    let src_of =
        |k: &[String], at: usize| -> Value { if combine { Value::Null } else { json!(k[at]) } };
    let origin_of = |k: &[String], at: usize| -> &'static str {
        let s = if combine { source } else { k[at].as_str() };
        if s == SOURCE_GATEWAY {
            "gateway"
        } else {
            "external"
        }
    };
    let by_account: Vec<Value> = grouped(
        db,
        w,
        f,
        &with(&["connection_id", "provider", "billing"]),
        &["MAX(connection_name)"],
        Some(MAX_GROUP_ROWS),
    )?
    .into_iter()
    .map(|(k, a)| json!({"connection_id": opt(&k[0]), "connection_name": opt(k.last().expect("label")), "provider": opt(&k[1]), "billing": k[2], "source": src_of(&k, 3), "origin": origin_of(&k, 3), "metrics": metrics_json(&a)}))
    .collect();
    let by_provider: Vec<Value> = grouped(db, w, f, &with(&["provider"]), &[], Some(MAX_GROUP_ROWS))?
        .into_iter()
        .map(|(k, a)| json!({"provider": opt(&k[0]), "source": src_of(&k, 1), "metrics": metrics_json(&a)}))
        .collect();
    let today = day_string(now_ms);
    let overrides = &snapshot.overrides;
    let by_model: Vec<Value> = grouped(db, w, f, &with(&["model", "provider"]), &[], Some(MAX_GROUP_ROWS))?
        .into_iter()
        .map(|(k, a)| {
            let priced = pricing::find(&k[0], &today, overrides).is_some();
            json!({"model": k[0], "provider": opt(&k[1]), "priced": priced, "source": src_of(&k, 2), "metrics": metrics_json(&a)})
        })
        .collect();
    let by_client: Vec<Value> = grouped(
        db,
        w,
        f,
        &with(&["client_key_id"]),
        &["MAX(client_key_name)"],
        Some(MAX_GROUP_ROWS),
    )?
    .into_iter()
    .map(|(k, a)| json!({"client_key_id": opt(&k[0]), "client_key_name": opt(k.last().expect("label")), "source": src_of(&k, 1), "origin": origin_of(&k, 1), "metrics": metrics_json(&a)}))
    .collect();

    // Facets: filter options for the window, ignoring the account/provider/model/client filters.
    let unfiltered = Filters {
        sources: f.sources,
        connection_id: None,
        provider: None,
        model: None,
        client_key_id: None,
    };
    let facet = |keys: &[&str], labels: &[&str]| {
        grouped(db, w, &unfiltered, keys, labels, Some(MAX_GROUP_ROWS))
    };
    let facets = json!({
        "accounts": facet(&["connection_id", "provider"], &["MAX(connection_name)"])?
            .into_iter()
            .filter(|(k, _)| !k[0].is_empty())
            .map(|(k, _)| json!({"connection_id": k[0], "provider": opt(&k[1]), "name": opt(&k[2])}))
            .collect::<Vec<_>>(),
        "providers": facet(&["provider"], &[])?
            .into_iter()
            .filter(|(k, _)| !k[0].is_empty())
            .map(|(k, _)| json!(k[0]))
            .collect::<Vec<_>>(),
        "models": facet(&["model"], &[])?
            .into_iter()
            .map(|(k, _)| json!(k[0]))
            .collect::<Vec<_>>(),
        "clients": facet(&["client_key_id"], &["MAX(client_key_name)"])?
            .into_iter()
            .filter(|(k, _)| !k[0].is_empty())
            .map(|(k, _)| json!({"client_key_id": k[0], "name": opt(&k[1])}))
            .collect::<Vec<_>>(),
    });

    // Coverage.
    let started = |s: &str| -> Option<i64> {
        db.query_row(
            "SELECT value FROM counters WHERE name = ?",
            [format!("usage_started_ms:{s}")],
            |r| r.get(0),
        )
        .ok()
    };
    let gateway_started = started(SOURCE_GATEWAY);
    let legacy = match gateway_started {
        Some(start) => snapshot
            .requests
            .iter()
            .filter(|r| {
                chrono::DateTime::parse_from_rfc3339(&r.timestamp)
                    .is_ok_and(|t| t.timestamp_millis() < start)
            })
            .count(),
        None => snapshot.requests.len(),
    };
    let complete = match (gateway_started, w.from_ms) {
        (Some(start), Some(from)) => start <= from,
        (Some(_), None) => legacy == 0,
        (None, _) => legacy == 0,
    };
    let message = if complete {
        Value::Null
    } else {
        json!(match gateway_started {
            Some(start) => format!(
                "Usage tracking started {}. Earlier gateway traffic is not included{}.",
                iso(start),
                if legacy > 0 {
                    format!(" ({legacy} older requests remain in the activity log only)")
                } else {
                    String::new()
                }
            ),
            None => "No usage has been recorded yet.".to_string(),
        })
    };
    let mut warnings = Vec::new();
    if source == "all" && unknown_overlap {
        warnings.push("External usage may include traffic the gateway also counted, so gateway and external totals are shown separately and not added together.".to_string());
    }
    if source != "gateway" && by_source.keys().all(|s| s == SOURCE_GATEWAY) {
        warnings.push("No external usage has been collected for this window.".to_string());
    }
    let window_from = match w.from_ms {
        Some(ms) => json!(iso(ms)),
        None => first_day
            .map(|d| json!(iso(d * DAY_MS)))
            .unwrap_or(Value::Null),
    };
    Ok(json!({
        "generated_at": iso(now_ms),
        "window": {
            "key": w.key,
            "from": window_from,
            "to": iso(w.to_ms),
            "granularity": match w.granularity { Granularity::Hour => "hour", Granularity::Day => "day", Granularity::Month => "month" },
            "timezone": "UTC",
        },
        "filters": {
            "connection_id": f.connection_id, "provider": f.provider, "model": f.model,
            "client_key_id": f.client_key_id, "source": source,
        },
        "coverage": {
            "ledger_started_at": gateway_started.map(iso),
            "external_started_at": started(SOURCE_EXTERNAL).or(started(SOURCE_EXTERNAL_DISJOINT)).map(iso),
            "complete_for_window": complete,
            "message": message,
            "excluded_legacy_requests": legacy,
            "overlap": if source == "gateway" || !by_source.keys().any(|s| s != SOURCE_GATEWAY) {
                "none"
            } else if unknown_overlap {
                "possible"
            } else {
                "deduplicated"
            },
        },
        "facets": facets,
        "combined": combine,
        "totals": if combine { metrics_json(&totals) } else { Value::Null },
        "by_source": by_source.iter().map(|(s, a)| json!({"source": s, "metrics": metrics_json(a)})).collect::<Vec<_>>(),
        "series": series,
        "by_account": by_account,
        "by_provider": by_provider,
        "by_model": by_model,
        "by_client": by_client,
        "warnings": warnings,
        "pricing": {"version": pricing::OFFICIAL_VERSION, "as_of": pricing::OFFICIAL_AS_OF, "overrides": overrides.len()},
    }))
}

/// Pricing view for `GET /api/usage/pricing`.
pub fn pricing_view(store: &Store, now_ms: i64) -> Result<Value, ApiError> {
    let today = day_string(now_ms);
    let overrides = overrides(store);
    let mut rates: Vec<Value> = pricing::official_cards()
        .iter()
        .filter(|c| {
            c.from_day.as_deref().is_none_or(|f| today.as_str() >= f)
                && c.until_day.as_deref().is_none_or(|u| today.as_str() <= u)
        })
        .map(pricing::card_json)
        .collect();
    let scheduled: Vec<Value> = pricing::official_cards()
        .iter()
        .filter(|c| c.from_day.as_deref().is_some_and(|f| today.as_str() < f))
        .map(pricing::card_json)
        .collect();
    rates.extend(overrides.iter().map(|o| {
        let mut v = pricing::card_json(
            &pricing::find(&o.model, &today, std::slice::from_ref(o)).expect("override card"),
        );
        v["updated_at"] = json!(o.updated_at);
        v
    }));
    let unpriced = store
        .with_db(|db| {
            let mut stmt = db.prepare(
                "SELECT model, MAX(provider), SUM(units), MAX(bucket) FROM usage_daily GROUP BY model ORDER BY SUM(units) DESC LIMIT 500",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(ApiError::db)?
        .into_iter()
        .filter(|(m, ..)| !m.is_empty() && pricing::find(m, &today, &overrides).is_none())
        .take(100)
        .map(|(m, p, units, day)| json!({"model": m, "provider": opt(&p), "units": units, "last_seen_day": day_string(day * DAY_MS)}))
        .collect::<Vec<_>>();
    Ok(json!({
        "version": pricing::OFFICIAL_VERSION,
        "as_of": pricing::OFFICIAL_AS_OF,
        "sources": pricing::SOURCES.iter().map(|(p, u)| json!({"provider": p, "url": u, "retrieved": pricing::OFFICIAL_AS_OF})).collect::<Vec<_>>(),
        "rates": rates,
        "scheduled": scheduled,
        "overrides": overrides.iter().map(|o| json!({"model": o.model, "updated_at": o.updated_at})).collect::<Vec<_>>(),
        "unpriced_models": unpriced,
        "scope": "Standard synchronous list prices. Not modelled: batch, priority/flex tiers, Anthropic fast mode and US data residency, server-tool fees, Gemini cache storage. Subscription (OAuth) usage is shown as an API-equivalent value, not money charged.",
    }))
}

/// Validates and replaces all overrides (`PUT /api/usage/pricing/overrides`).
pub fn replace_overrides(store: &Store, body: &Value) -> Result<(), ApiError> {
    let list = body["overrides"]
        .as_array()
        .ok_or(ApiError::bad("Body must be {\"overrides\": [...]}"))?;
    if list.len() > pricing::MAX_OVERRIDES {
        return Err(ApiError::bad("At most 200 price overrides"));
    }
    let now = crate::store::now();
    let mut items: Vec<Override> = Vec::with_capacity(list.len());
    for o in list {
        let model = o["model"]
            .as_str()
            .map(str::trim)
            .filter(|m| !m.is_empty() && m.len() <= 200)
            .ok_or(ApiError::bad(
                "Each override needs a model name of 1-200 characters",
            ))?;
        if items.iter().any(|x| x.model == model) {
            return Err(ApiError::bad("Each model can have only one override"));
        }
        let r = &o["usd_per_mtok"];
        let field = |k: &str| {
            pricing::parse_rate(&r[k])
                .map_err(|e| ApiError::new(400, &format!("{model}: {k}: {e}")))
        };
        let rate = pricing::Rate {
            input: field("input")?,
            output: field("output")?,
            cache_read: field("cache_read")?,
            cache_write: field("cache_write")?,
            cache_write_5m: field("cache_write_5m")?,
            cache_write_1h: field("cache_write_1h")?,
        };
        if rate.input.is_none() || rate.output.is_none() {
            return Err(ApiError::new(
                400,
                &format!("{model}: input and output rates are required"),
            ));
        }
        items.push(Override {
            model: model.to_string(),
            rate,
            updated_at: now.clone(),
        });
    }
    let values: Vec<(String, Value)> = items
        .iter()
        .map(|o| (o.model.clone(), serde_json::to_value(o).expect("override")))
        .collect();
    store
        .replace_kind(pricing::OVERRIDE_KIND, &values)
        .map_err(ApiError::db)
}

// ---------------------------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------------------------

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn get_usage(
    State(app): State<App>,
    Query(q): Query<UsageQuery>,
) -> Result<Json<Value>, ApiError> {
    summary(&app.store, &q, now_ms()).map(Json)
}
async fn get_pricing(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    pricing_view(&app.store, now_ms()).map(Json)
}
async fn put_overrides(
    State(app): State<App>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    replace_overrides(&app.store, &body)?;
    pricing_view(&app.store, now_ms()).map(Json)
}

/// Usage API routes. Admin-only: merge into the admin router before its auth `route_layer`.
pub fn router() -> Router<App> {
    Router::new()
        .route("/api/usage", get(get_usage))
        .route("/api/usage/pricing", get(get_pricing))
        .route("/api/usage/pricing/overrides", put(put_overrides))
}
