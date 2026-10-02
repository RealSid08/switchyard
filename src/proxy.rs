//! Inference proxy: account selection, request shaping, upstream calls, stream integrity and the
//! Responses WebSocket bridge.
//!
//! Replay policy. A request is sent again (to another account, or to the same account after a
//! credential refresh) only when the provider definitely did not run it: the connection failed
//! before dispatch, or the provider answered with a non-2xx status before any output reached the
//! client. After output has been forwarded, or after an ambiguous transport failure once the
//! request was sent, nothing is replayed; the client receives an explicit partial-output error.
use crate::{
    app::{ApiError, App},
    credentials,
    store::{Connection, RequestRecord, Route, id, now},
};
use axum::{
    Json,
    body::{Body, Bytes},
    extract::{
        Path, Query, State,
        ws::{Message as AxMessage, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, Stream, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};
use tokio::sync::OwnedSemaphorePermit;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

/// First system block required by Anthropic for Claude subscription (OAuth) tokens. Mirrors the
/// identity CLIProxyAPI sends (internal/runtime/executor/claude_executor_cloaking.go).
pub const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const MAX_ERROR_BODY: usize = 64 * 1024;
const MAX_EVENT: usize = 16 * 1024 * 1024;
const MAX_COLLECTED: usize = 16 * 1024 * 1024;
const ERROR_BODY_TIMEOUT: Duration = Duration::from_secs(20);
const WS_FIRST_FRAME_DEADLINE: Duration = Duration::from_secs(30);
const WS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------------------------
// Account selection
// ---------------------------------------------------------------------------------------------

pub fn candidates(app: &App, model: &str, ws: bool) -> Result<Vec<(Connection, String)>, ApiError> {
    let connections: Vec<Connection> = app.store.list("connection");
    let route = app.store.get::<Route>("route", model);
    let mut targets: Vec<(Connection, String)> = if let Some(r) = &route {
        r.targets
            .iter()
            .filter_map(|t| {
                connections
                    .iter()
                    .find(|c| c.id == t.connection_id && c.enabled && (!ws || c.supports_websocket))
                    .map(|c| (c.clone(), t.model.clone()))
            })
            .collect()
    } else {
        connections
            .into_iter()
            .filter(|c| {
                c.enabled && c.models.iter().any(|m| m == model) && (!ws || c.supports_websocket)
            })
            .map(|c| (c, model.into()))
            .collect()
    };
    if targets.is_empty() {
        return Err(ApiError::new(
            404,
            if ws {
                "No enabled WebSocket connection for this model"
            } else {
                "No enabled connection for this model. Add a provider or route in the dashboard."
            },
        ));
    }
    if route.as_ref().is_none_or(|r| r.strategy == "round_robin") {
        let n = app.cursor.fetch_add(1, Ordering::Relaxed) % targets.len();
        targets.rotate_left(n);
    }
    let mut health = app.resilience.lock().expect("resilience lock");
    let ready: Vec<_> = targets
        .iter()
        .filter(|(c, m)| health.remaining(&c.id, m) == 0)
        .cloned()
        .collect();
    if ready.is_empty() {
        return Err(ApiError::new(
            429,
            "All matching accounts are cooling down. Retry after their provider limit resets.",
        )
        .retry_after(
            targets
                .iter()
                .map(|(c, m)| health.remaining(&c.id, m))
                .min()
                .unwrap_or(1),
        ));
    }
    Ok(ready.into_iter().take(20).collect())
}

/// Keeps only the account that owns `previous_response_id`, if any. Unknown ids are refused in
/// multi-account pools because guessing would continue the conversation on the wrong account.
fn apply_affinity(
    app: &App,
    previous: Option<&str>,
    candidates: &mut Vec<(Connection, String)>,
) -> Result<(), &'static str> {
    let Some(previous) = previous else {
        return Ok(());
    };
    let owner = app
        .resilience
        .lock()
        .expect("resilience lock")
        .owner(previous);
    match owner {
        Some(owner) => {
            candidates.retain(|(c, _)| c.id == owner);
            if candidates.is_empty() {
                return Err(
                    "The account owning previous_response_id is disabled, cooling down or no longer routed. Restore that account or start a new conversation.",
                );
            }
        }
        None if candidates.len() > 1 => {
            return Err(
                "Unknown previous_response_id in a multi-account route. Start a new conversation; account affinity cannot be guessed safely.",
            );
        }
        None => {}
    }
    Ok(())
}

// The resilience mutex is synchronous: these helpers lock and release it without awaiting.
fn cool_model(app: &App, connection: &str, model: &str, seconds: u64) {
    app.resilience
        .lock()
        .expect("resilience lock")
        .cool(connection, model, seconds);
}
fn cool_account(app: &App, connection: &str, seconds: u64) {
    app.resilience
        .lock()
        .expect("resilience lock")
        .cool_account(connection, seconds);
}
fn remember_response(app: &App, response_id: &str, connection: &str) {
    app.resilience
        .lock()
        .expect("resilience lock")
        .remember(response_id, connection);
}
/// Cooldown after a provider status that says "not this account right now".
fn cool_for_status(app: &App, connection: &str, model: &str, status: u16, seconds: u64) {
    if matches!(status, 401 | 403) {
        cool_account(app, connection, seconds);
    } else {
        cool_model(app, connection, model, seconds);
    }
}

// ---------------------------------------------------------------------------------------------
// Upstream headers
// ---------------------------------------------------------------------------------------------

pub fn upstream_headers(req: reqwest::RequestBuilder, c: &Connection) -> reqwest::RequestBuilder {
    let req = match c.kind.as_str() {
        "anthropic" if !c.oauth => req.header("x-api-key", &c.api_key),
        "gemini" => req.header("x-goog-api-key", &c.api_key),
        _ => req.bearer_auth(&c.api_key),
    };
    let req = if c.kind == "anthropic" {
        let req = req.header("anthropic-version", ANTHROPIC_VERSION);
        if c.oauth {
            req.header("anthropic-beta", ANTHROPIC_OAUTH_BETA)
                .header("user-agent", "claude-cli/2.1.287 (external, cli)")
        } else {
            req
        }
    } else {
        req
    };
    if c.kind == "codex" {
        let req = req
            .header("originator", "codex_cli_rs")
            .header("user-agent", "codex_cli_rs/0.159.3");
        if !c.account_id.is_empty() {
            req.header("chatgpt-account-id", &c.account_id)
        } else {
            req
        }
    } else {
        req
    }
}

/// Client protocol headers that are safe and meaningful to forward. Returned as a map whose
/// entries replace (not append to) the defaults set by `upstream_headers`, so each header is
/// sent exactly once. Client authentication and arbitrary headers never reach upstream.
fn protocol_headers(h: &HeaderMap, c: &Connection) -> HeaderMap {
    let mut out = HeaderMap::new();
    if let Some(v) = h.get("idempotency-key") {
        out.insert("idempotency-key", v.clone());
    }
    match c.kind.as_str() {
        "anthropic" => {
            if let Some(v) = h.get("anthropic-version") {
                out.insert("anthropic-version", v.clone());
            }
            if let Some(betas) = anthropic_betas(h, c.oauth) {
                out.insert("anthropic-beta", betas);
            }
        }
        "openai" | "codex" => {
            if let Some(v) = h.get("openai-beta") {
                out.insert("openai-beta", v.clone());
            }
        }
        _ => {}
    }
    out
}

/// Merges every `anthropic-beta` header line (HTTP lists may be split over lines) into one
/// de-duplicated value, adding the OAuth beta for subscription tokens.
fn anthropic_betas(h: &HeaderMap, oauth: bool) -> Option<HeaderValue> {
    let mut betas: Vec<String> = Vec::new();
    for value in h.get_all("anthropic-beta") {
        for beta in value.to_str().unwrap_or("").split(',') {
            let beta = beta.trim();
            if !beta.is_empty() && !betas.iter().any(|b| b == beta) {
                betas.push(beta.to_string());
            }
        }
    }
    if betas.is_empty() {
        return None; // upstream_headers already set the OAuth beta when needed
    }
    if oauth && !betas.iter().any(|b| b == ANTHROPIC_OAUTH_BETA) {
        betas.insert(0, ANTHROPIC_OAUTH_BETA.to_string());
    }
    betas.join(",").parse().ok()
}

// ---------------------------------------------------------------------------------------------
// Request records
// ---------------------------------------------------------------------------------------------

/// One request-log row, written when dropped. Until `finish` is called the row reads as a client
/// disconnect (499), which is exactly what happens if the response future is dropped early.
struct RecordGuard {
    app: App,
    record: Option<RequestRecord>,
    start: Instant,
    settled: bool,
    _permit: OwnedSemaphorePermit,
}
impl RecordGuard {
    fn new(
        app: App,
        c: &Connection,
        model: &str,
        transport: &str,
        permit: OwnedSemaphorePermit,
    ) -> Self {
        app.active.fetch_add(1, Ordering::Relaxed);
        Self {
            app,
            record: Some(RequestRecord {
                id: id(),
                timestamp: now(),
                model: model.into(),
                connection_id: c.id.clone(),
                connection_name: c.name.clone(),
                transport: transport.into(),
                status: 499,
                latency_ms: 0,
                input_tokens: None,
                output_tokens: None,
                error: Some("Client disconnected before completion".into()),
            }),
            start: Instant::now(),
            settled: false,
            _permit: permit,
        }
    }
    fn attribute(&mut self, c: &Connection) {
        if let Some(r) = &mut self.record {
            r.connection_id = c.id.clone();
            r.connection_name = c.name.clone();
        }
    }
    fn finish(&mut self, status: u16, error: Option<&str>) {
        self.settled = true;
        if let Some(r) = &mut self.record {
            r.status = status;
            r.error = error.map(String::from);
        }
    }
    /// A new WebSocket turn starts: until it ends the session reads as in flight.
    fn reopen(&mut self) {
        self.finish(499, Some("Client disconnected before completion"));
        self.settled = false;
    }
    fn id(&self) -> String {
        self.record
            .as_ref()
            .map(|r| r.id.clone())
            .unwrap_or_default()
    }
    fn usage(&mut self, v: &Value) {
        let u = v
            .get("usage")
            .or_else(|| v.get("response").and_then(|r| r.get("usage")))
            .or_else(|| v.get("message").and_then(|r| r.get("usage")))
            .or_else(|| v.get("usageMetadata"));
        if let (Some(r), Some(u)) = (&mut self.record, u) {
            r.input_tokens = u["input_tokens"]
                .as_u64()
                .or(u["prompt_tokens"].as_u64())
                .or(u["promptTokenCount"].as_u64())
                .or(r.input_tokens);
            r.output_tokens = u["output_tokens"]
                .as_u64()
                .or(u["completion_tokens"].as_u64())
                .or(u["candidatesTokenCount"].as_u64())
                .or(r.output_tokens);
        }
    }
}
impl Drop for RecordGuard {
    fn drop(&mut self) {
        self.app.active.fetch_sub(1, Ordering::Relaxed);
        if let Some(mut r) = self.record.take() {
            r.latency_ms = self.start.elapsed().as_millis() as u64;
            if self.app.store.record(&r).is_err() {
                tracing::error!("Could not persist request metrics");
            }
            let _ = self.app.events.send(json!({"type":"request","data":r}));
        }
    }
}
fn permit(app: &App) -> Result<OwnedSemaphorePermit, ApiError> {
    if app.paused.load(Ordering::Relaxed) {
        return Err(ApiError::new(503, "Gateway is paused"));
    }
    app.permits.clone().try_acquire_owned().map_err(|_| {
        ApiError::new(429, "Gateway concurrency limit reached; retry later").retry_after(1)
    })
}
fn compatible(c: &Connection, endpoint: &str) -> bool {
    match endpoint {
        "responses" | "chat/completions" => ["openai", "codex"].contains(&c.kind.as_str()),
        "messages" | "messages/count_tokens" => c.kind == "anthropic",
        "gemini" => c.kind == "gemini",
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Payload shaping
// ---------------------------------------------------------------------------------------------

fn codex_payload(mut v: Value) -> Value {
    v["store"] = json!(false);
    v["stream"] = json!(true);
    if v["instructions"]
        .as_str()
        .is_none_or(|s| s.trim().is_empty())
    {
        v["instructions"] = json!("You are a helpful coding assistant.");
    }
    if let Some(s) = v["input"].as_str() {
        v["input"] = json!([{"role":"user","content":[{"type":"input_text","text":s}]}]);
    }
    if let Some(o) = v.as_object_mut() {
        for k in [
            "max_output_tokens",
            "max_tokens",
            "temperature",
            "top_p",
            "stream_options",
        ] {
            o.remove(k);
        }
    }
    v
}

/// Claude subscription tokens are only accepted for requests that identify as Claude Code. The
/// identity block is prepended; the caller's own system prompt is kept, in order, after it.
fn ensure_claude_code_identity(payload: &mut Value) {
    let identity = json!({"type":"text","text":CLAUDE_CODE_IDENTITY});
    let is_identity = |text: &str| text.trim_start().starts_with(CLAUDE_CODE_IDENTITY);
    match payload.get_mut("system") {
        None | Some(Value::Null) => payload["system"] = json!([identity]),
        Some(Value::String(s)) if is_identity(s) => {}
        Some(Value::String(s)) if s.trim().is_empty() => payload["system"] = json!([identity]),
        Some(Value::String(s)) => {
            let user = json!({"type":"text","text":s});
            payload["system"] = json!([identity, user]);
        }
        Some(Value::Array(blocks)) => {
            // Claude Code itself may send a billing block first and the identity second.
            if !blocks
                .iter()
                .any(|b| b["text"].as_str().is_some_and(is_identity))
            {
                blocks.insert(0, identity);
            }
        }
        Some(_) => {} // malformed; let the provider report it
    }
}

fn chat_content_part(part: &Value, role: &str) -> Value {
    match part["type"].as_str().unwrap_or("") {
        "text" => json!({
            "type": if role == "assistant" { "output_text" } else { "input_text" },
            "text": part["text"],
        }),
        "image_url" => {
            let mut image = json!({"type":"input_image","image_url":part["image_url"]["url"]});
            if !part["image_url"]["detail"].is_null() {
                image["detail"] = part["image_url"]["detail"].clone();
            }
            image
        }
        _ => part.clone(),
    }
}

fn chat_to_responses(v: &Value) -> Value {
    let mut input = Vec::new();
    let mut instructions = Vec::new();
    for m in v["messages"].as_array().into_iter().flatten() {
        let role = m["role"].as_str().unwrap_or("user");
        if ["system", "developer"].contains(&role) {
            instructions.push(match m["content"].as_str() {
                Some(text) => text.to_string(),
                None => m["content"]
                    .as_array()
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|p| p["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default(),
            });
            continue;
        }
        if role == "tool" {
            input.push(json!({"type":"function_call_output","call_id":m["tool_call_id"],"output":m["content"]}));
            continue;
        }
        if !m["content"].is_null() {
            let content = match m["content"].as_array() {
                Some(parts) => {
                    Value::Array(parts.iter().map(|p| chat_content_part(p, role)).collect())
                }
                None => m["content"].clone(),
            };
            input.push(json!({"role":role,"content":content}));
        }
        for call in m["tool_calls"].as_array().into_iter().flatten() {
            input.push(json!({"type":"function_call","call_id":call["id"],"name":call["function"]["name"],"arguments":call["function"]["arguments"]}));
        }
    }
    let mut result = json!({"model":v["model"],"input":input,"instructions":instructions.join("\n"),"stream":v["stream"].as_bool().unwrap_or(false)});
    if let Some(tools) = v["tools"].as_array() {
        let tools: Vec<Value> = tools
            .iter()
            .map(|t| {
                if t["type"] == "function" {
                    let mut f = t["function"].clone();
                    f["type"] = json!("function");
                    f
                } else {
                    t.clone()
                }
            })
            .collect();
        result["tools"] = json!(tools);
    }
    for k in ["parallel_tool_calls", "reasoning", "temperature", "top_p"] {
        if !v[k].is_null() {
            result[k] = v[k].clone();
        }
    }
    if !v["tool_choice"].is_null() {
        result["tool_choice"] = if v["tool_choice"]["type"] == "function" {
            json!({"type":"function","name":v["tool_choice"]["function"]["name"]})
        } else {
            v["tool_choice"].clone()
        };
    }
    if let Some(effort) = v["reasoning_effort"].as_str() {
        result["reasoning"] = json!({"effort":effort});
    }
    let format = &v["response_format"];
    if !format.is_null() {
        let format = if format["type"] == "json_schema" {
            let mut schema = format["json_schema"].clone();
            schema["type"] = json!("json_schema");
            schema
        } else {
            format.clone()
        };
        result["text"] = json!({"format":format});
    }
    if let Some(max) = v["max_completion_tokens"]
        .as_u64()
        .or(v["max_tokens"].as_u64())
    {
        result["max_output_tokens"] = json!(max);
    }
    result
}

/// Responses usage in Chat Completions field names. `total_tokens` is derived when the
/// provider omits it.
fn chat_usage(u: &Value) -> Value {
    if !u.is_object() {
        return Value::Null;
    }
    let prompt = u["input_tokens"].as_u64().or(u["prompt_tokens"].as_u64());
    let completion = u["output_tokens"]
        .as_u64()
        .or(u["completion_tokens"].as_u64());
    let total = u["total_tokens"].as_u64().or(match (prompt, completion) {
        (Some(p), Some(c)) => Some(p + c),
        _ => None,
    });
    json!({"prompt_tokens":prompt,"completion_tokens":completion,"total_tokens":total})
}

fn chat_finish_reason(response: &Value, has_tool_calls: bool) -> &'static str {
    if response["status"] == "incomplete" {
        "length"
    } else if has_tool_calls {
        "tool_calls"
    } else {
        "stop"
    }
}

fn response_to_chat(v: &Value, model: &str) -> Value {
    let mut text = String::new();
    let mut calls = Vec::new();
    for item in v["output"].as_array().into_iter().flatten() {
        if item["type"] == "function_call" {
            calls.push(json!({"id":item["call_id"],"type":"function","function":{"name":item["name"],"arguments":item["arguments"]}}));
        }
        if item["type"] == "message" {
            for part in item["content"].as_array().into_iter().flatten() {
                if part["type"] == "output_text"
                    && let Some(s) = part["text"].as_str()
                {
                    text.push_str(s);
                }
            }
        }
    }
    let mut message = json!({"role":"assistant","content":text});
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
    }
    let finish = chat_finish_reason(v, !calls.is_empty());
    json!({
        "id": v["id"],
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{"index":0,"message":message,"finish_reason":finish}],
        "usage": chat_usage(&v["usage"]),
    })
}

/// The ChatGPT Codex backend may send `response.completed` with an empty `output` and deliver the
/// items only as `response.output_item.done` events. Rebuild the output from those events.
fn patch_completed_output(response: &mut Value, items: BTreeMap<u64, Value>, extra: Vec<Value>) {
    let empty = response["output"].as_array().is_none_or(|o| o.is_empty());
    if empty && (!items.is_empty() || !extra.is_empty()) {
        let output: Vec<Value> = items.into_values().chain(extra).collect();
        response["output"] = json!(output);
    }
}

// ---------------------------------------------------------------------------------------------
// Provider errors: native codes preserved, credentials redacted
// ---------------------------------------------------------------------------------------------

/// Removes credentials from provider text before it reaches clients or logs.
struct Redactor {
    secrets: Vec<String>,
}
impl Redactor {
    fn new(c: &Connection, h: &HeaderMap) -> Self {
        let mut secrets: Vec<String> = [&c.api_key, &c.refresh_token, &c.account_id]
            .into_iter()
            .cloned()
            .collect();
        for name in [
            header::AUTHORIZATION.as_str(),
            "x-api-key",
            "x-goog-api-key",
        ] {
            for v in h.get_all(name) {
                if let Ok(v) = v.to_str() {
                    secrets.push(v.to_string());
                    if let Some((_, token)) = v.split_once(' ') {
                        secrets.push(token.trim().to_string());
                    }
                }
            }
        }
        // Short values would redact ordinary words; longest first so prefixes do not leak tails.
        secrets.retain(|s| s.len() >= 6);
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        secrets.dedup();
        Self { secrets }
    }
    fn text(&self, s: &str) -> String {
        let mut out = s.to_string();
        for secret in &self.secrets {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), "[redacted]");
            }
        }
        redact_token_shapes(&out)
    }
    /// Redacts every string in a JSON value, including reflected structured fields.
    fn value(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.text(s)),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.value(x)).collect()),
            Value::Object(o) => {
                Value::Object(o.iter().map(|(k, x)| (k.clone(), self.value(x))).collect())
            }
            other => other.clone(),
        }
    }
}

/// Redacts anything shaped like a provider or gateway credential, even when it is not one of the
/// connection's own secrets: `sk-…`, `sy_…`, JWTs (`eyJ…`) and `Bearer …` values.
fn redact_token_shapes(s: &str) -> String {
    const PREFIXES: [&str; 5] = ["sk-", "sy_", "eyJ", "Bearer ", "bearer "];
    let token_char = |c: char| c.is_ascii_alphanumeric() || "-_.~+/=".contains(c);
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    'scan: while !rest.is_empty() {
        for prefix in PREFIXES {
            let at_boundary = out.chars().last().is_none_or(|c| !token_char(c));
            if at_boundary && rest.starts_with(prefix) {
                let body = &rest[prefix.len()..];
                let len = body.find(|c: char| !token_char(c)).unwrap_or(body.len());
                if len >= 8 {
                    if prefix.ends_with(' ') {
                        out.push_str(prefix);
                    }
                    out.push_str("[redacted]");
                    rest = &body[len..];
                    continue 'scan;
                }
            }
        }
        let ch = rest.chars().next().expect("non-empty");
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

/// Reads a small JSON body (errors, token counts): at most 64 KiB and 20 s, else `Null`.
async fn read_json_bounded(res: reqwest::Response) -> Value {
    let read = async {
        let mut stream = res.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(Ok(b)) = stream.next().await {
            if bytes.len() + b.len() > MAX_ERROR_BODY {
                return Value::Null;
            }
            bytes.extend_from_slice(&b);
        }
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    tokio::time::timeout(ERROR_BODY_TIMEOUT, read)
        .await
        .unwrap_or(Value::Null)
}

/// Seconds until an account may be retried, from Retry-After (seconds or HTTP date), Codex
/// `resets_in_seconds` / `resets_at`, or Anthropic's unified rate-limit reset header.
fn retry_seconds(headers: &HeaderMap, body: &Value, status: u16) -> u64 {
    let now = chrono::Utc::now().timestamp();
    let until = |t: i64| (t - now).max(1) as u64;
    let header_str = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let error = &body["error"];
    let retry = header_str("retry-after")
        .and_then(|s| {
            s.trim().parse::<u64>().ok().or_else(|| {
                chrono::DateTime::parse_from_rfc2822(s)
                    .ok()
                    .map(|t| until(t.timestamp()))
            })
        })
        .or_else(|| error["resets_in_seconds"].as_u64())
        .or_else(|| error["resets_at"].as_i64().map(until))
        .or_else(|| {
            header_str("anthropic-ratelimit-unified-reset")
                .and_then(|s| s.trim().parse::<i64>().ok())
                .map(until)
        })
        .unwrap_or(if status == 429 { 60 } else { 10 });
    retry.clamp(1, 3600)
}

fn anthropic_error_type(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ => "api_error",
    }
}

/// A short, log-safe label for a provider error: the native code or type when it is a plain
/// identifier, never free text.
fn error_label(body: &Value) -> String {
    let error = body.get("error").unwrap_or(body);
    let code = error["code"]
        .as_str()
        .or(error["type"].as_str())
        .unwrap_or("");
    let safe = !code.is_empty()
        && code.len() <= 64
        && code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.');
    if safe {
        format!("Provider rejected request ({code})")
    } else {
        "Provider rejected request".into()
    }
}

/// The provider's error in the client's native envelope. All structured fields (codes, params,
/// reset times, plan types) are preserved; every string is redacted.
fn provider_error(
    status: StatusCode,
    body: &Value,
    redactor: &Redactor,
    endpoint: &str,
) -> Response {
    let source = body.get("error").unwrap_or(body);
    let mut error = match source {
        Value::Object(o) => match redactor.value(&Value::Object(o.clone())) {
            Value::Object(o) => o,
            _ => serde_json::Map::new(),
        },
        _ => serde_json::Map::new(),
    };
    let detail = match source {
        Value::String(s) => Some(s.as_str()),
        _ => source["message"].as_str(),
    }
    .map(|s| redactor.text(s))
    .unwrap_or_else(|| "Check credentials, account quota and model availability.".into());
    error.insert(
        "message".into(),
        json!(format!("Provider rejected the request: {detail}")),
    );
    if !error.get("type").is_some_and(Value::is_string) {
        let fallback = if endpoint == "messages" {
            anthropic_error_type(status.as_u16())
        } else {
            "gateway_error"
        };
        error.insert("type".into(), json!(fallback));
    }
    let v = if endpoint == "messages" {
        json!({"type":"error","error":error})
    } else {
        json!({"error":error})
    };
    (status, Json(v)).into_response()
}

// ---------------------------------------------------------------------------------------------
// Server-sent events
// ---------------------------------------------------------------------------------------------

/// Error event appended when a stream ends without a protocol terminal event. The leading blank
/// lines terminate any half-written upstream event so the client can parse this one.
fn stream_error(message: &str) -> Bytes {
    Bytes::from(format!(
        "\n\nevent: error\ndata: {}\n\n",
        json!({"type":"error","error":{"type":"upstream_interrupted","message":message,"retryable":true,"partial_output":true}})
    ))
}

/// Incremental WHATWG SSE parser: CRLF/LF lines, multi-line `data:`, comments and other fields
/// ignored, `[DONE]` surfaced as `{"_done":true}`. A single event is bounded to 16 MiB.
struct SseParser {
    pending: Vec<u8>,
    data: Vec<u8>,
    scanned: usize,
}
impl SseParser {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
            data: Vec::new(),
            scanned: 0,
        }
    }
    fn push(&mut self, b: &[u8]) -> Result<Vec<Value>, ApiError> {
        self.pending.extend_from_slice(b);
        let mut events = Vec::new();
        loop {
            let Some(offset) = self.pending[self.scanned..]
                .iter()
                .position(|x| *x == b'\n')
            else {
                self.scanned = self.pending.len();
                break;
            };
            let end = self.scanned + offset;
            self.scanned = 0;
            let raw: Vec<u8> = self.pending.drain(..=end).collect();
            let line = raw.strip_suffix(b"\n").unwrap_or(&raw);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() {
                self.dispatch(&mut events);
            } else if let Some(data) = line.strip_prefix(b"data:") {
                let data = data.strip_prefix(b" ").unwrap_or(data);
                if !self.data.is_empty() {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(data);
            }
            if self.data.len() > MAX_EVENT {
                return Err(ApiError::upstream("Provider event exceeds 16 MiB"));
            }
        }
        if self.pending.len() + self.data.len() > MAX_EVENT {
            return Err(ApiError::upstream("Provider event exceeds 16 MiB"));
        }
        Ok(events)
    }
    fn dispatch(&mut self, events: &mut Vec<Value>) {
        if self.data.is_empty() {
            return;
        }
        let data = std::mem::take(&mut self.data);
        let is_done = data
            .iter()
            .copied()
            .filter(|b| !b.is_ascii_whitespace())
            .eq(b"[DONE]".iter().copied());
        if is_done {
            events.push(json!({"_done":true}));
        } else if let Ok(v) = serde_json::from_slice::<Value>(&data) {
            events.push(v);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Terminal {
    /// The provider finished the response (including truncation such as max tokens).
    Success,
    /// The provider reported a failure in-band.
    Failure,
}

/// Protocol terminal events for every native stream format the gateway relays.
fn terminal_event(e: &Value) -> Option<Terminal> {
    match e["type"].as_str() {
        Some("response.completed" | "response.incomplete" | "message_stop") => {
            return Some(Terminal::Success);
        }
        Some("response.failed" | "error") => return Some(Terminal::Failure),
        _ => {}
    }
    if e["_done"] == true {
        return Some(Terminal::Success);
    }
    if e["error"].is_object() {
        return Some(Terminal::Failure); // Chat Completions and Gemini in-stream errors
    }
    let chat_finished = e["choices"]
        .as_array()
        .is_some_and(|cs| cs.iter().any(|c| c["finish_reason"].is_string()));
    let gemini_finished = e["candidates"]
        .as_array()
        .is_some_and(|cs| cs.iter().any(|c| c["finishReason"].is_string()))
        || e["promptFeedback"]["blockReason"].is_string();
    (chat_finished || gemini_finished).then_some(Terminal::Success)
}

/// Translates Responses stream events into Chat Completions chunks.
struct ChatTranslator {
    id: String,
    model: String,
    created: i64,
    role_sent: bool,
    tool_indices: HashMap<String, usize>,
    finished: bool,
}
impl ChatTranslator {
    fn new(id: String, model: String) -> Self {
        Self {
            id,
            model,
            created: chrono::Utc::now().timestamp(),
            role_sent: false,
            tool_indices: HashMap::new(),
            finished: false,
        }
    }
    fn chunk(&self, delta: Value, finish_reason: Value, usage: Option<Value>) -> Bytes {
        let mut chunk = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{"index":0,"delta":delta,"finish_reason":finish_reason}],
        });
        if let Some(usage) = usage {
            chunk["usage"] = usage;
        }
        Bytes::from(format!("data: {chunk}\n\n"))
    }
    fn translate(&mut self, e: &Value, redactor: &Redactor) -> Vec<Bytes> {
        let mut out = Vec::new();
        if self.finished || e["_done"] == true {
            return out;
        }
        let kind = e["type"].as_str().unwrap_or("");
        if kind == "response.created"
            && let Some(id) = e["response"]["id"].as_str()
        {
            self.id = id.into();
        }
        if !self.role_sent {
            self.role_sent = true;
            out.push(self.chunk(json!({"role":"assistant","content":""}), Value::Null, None));
        }
        let delta = match kind {
            "response.output_text.delta" => Some(json!({"content":e["delta"]})),
            "response.refusal.delta" => Some(json!({"refusal":e["delta"]})),
            "response.output_item.added" if e["item"]["type"] == "function_call" => {
                let index = self.tool_indices.len();
                if let Some(item) = e["item"]["id"].as_str() {
                    self.tool_indices.insert(item.into(), index);
                }
                Some(
                    json!({"tool_calls":[{"index":index,"id":e["item"]["call_id"],"type":"function","function":{"name":e["item"]["name"],"arguments":""}}]}),
                )
            }
            "response.function_call_arguments.delta" => {
                let item = e["item_id"].as_str().unwrap_or("");
                let index = self.tool_indices.get(item).copied().unwrap_or(0);
                Some(json!({"tool_calls":[{"index":index,"function":{"arguments":e["delta"]}}]}))
            }
            _ => None,
        };
        if let Some(delta) = delta {
            out.push(self.chunk(delta, Value::Null, None));
        }
        match kind {
            "response.completed" | "response.incomplete" => {
                self.finished = true;
                let response = &e["response"];
                let finish = chat_finish_reason(response, !self.tool_indices.is_empty());
                let usage = chat_usage(&response["usage"]);
                out.push(self.chunk(json!({}), json!(finish), Some(usage)));
                out.push(Bytes::from_static(b"data: [DONE]\n\n"));
            }
            "response.failed" | "error" => {
                self.finished = true;
                let error = if kind == "response.failed" {
                    &e["response"]["error"]
                } else {
                    &e["error"]
                };
                let error = if error.is_object() {
                    redactor.value(error)
                } else {
                    json!({"type":"upstream_error","message":"Provider stream failed"})
                };
                out.push(Bytes::from(format!("data: {}\n\n", json!({"error":error}))));
                out.push(Bytes::from_static(b"data: [DONE]\n\n"));
            }
            _ => {}
        }
        out
    }
}

/// Relays an upstream SSE body. Bytes are forwarded unchanged unless Chat translation is on.
/// The request record reflects the protocol outcome, not merely the HTTP status.
fn relay_stream(
    app: App,
    mut guard: RecordGuard,
    mut upstream: impl Stream<Item = reqwest::Result<Bytes>> + Unpin + Send + 'static,
    connection: String,
    status: u16,
    mut translator: Option<ChatTranslator>,
    redactor: Redactor,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> {
    async_stream::stream! {
        let mut parser = SseParser::new();
        let mut outcome: Option<Terminal> = None;
        loop {
            let bytes = match upstream.next().await {
                None => break,
                Some(Ok(bytes)) => bytes,
                Some(Err(_)) => {
                    // After a terminal event the response is complete; a dropped connection is
                    // not a failure. Before it, output is partial and must not be replayed.
                    if outcome.is_none() {
                        guard.finish(502, Some("Upstream stream interrupted"));
                        yield Ok(stream_error("Upstream stream interrupted. Output may be partial; do not replay tool actions automatically."));
                    }
                    return;
                }
            };
            let events = match parser.push(&bytes) {
                Ok(events) => events,
                Err(_) => {
                    guard.finish(502, Some("Oversized upstream event"));
                    yield Err(std::io::Error::other("Oversized upstream event"));
                    return;
                }
            };
            for e in &events {
                guard.usage(e);
                if outcome.is_some() {
                    continue;
                }
                let Some(terminal) = terminal_event(e) else { continue };
                outcome = Some(terminal);
                match terminal {
                    Terminal::Success => {
                        guard.finish(status, None);
                        if let Some(response_id) = e["response"]["id"].as_str() {
                            remember_response(&app, response_id, &connection);
                        }
                    }
                    Terminal::Failure => {
                        guard.finish(502, Some(&format!("{} in stream", error_label(e))));
                    }
                }
            }
            match translator.as_mut() {
                Some(t) => {
                    for e in &events {
                        for chunk in t.translate(e, &redactor) {
                            yield Ok(chunk);
                        }
                    }
                }
                None => yield Ok(bytes),
            }
        }
        if outcome.is_none() {
            guard.finish(502, Some("Provider stream ended without completion"));
            yield Ok(stream_error("Provider stream ended without completion. Output may be partial."));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// HTTP endpoints
// ---------------------------------------------------------------------------------------------

pub async fn responses(
    State(app): State<App>,
    h: HeaderMap,
    Json(v): Json<Value>,
) -> Result<Response, ApiError> {
    execute(app, "responses", v, h).await
}
pub async fn chat(
    State(app): State<App>,
    h: HeaderMap,
    Json(v): Json<Value>,
) -> Result<Response, ApiError> {
    execute(app, "chat/completions", v, h).await
}
pub async fn messages(
    State(app): State<App>,
    h: HeaderMap,
    Json(v): Json<Value>,
) -> Result<Response, ApiError> {
    execute(app, "messages", v, h).await
}
/// Native Anthropic `POST /v1/messages/count_tokens`, used by Claude Code and the Anthropic
/// SDKs. The request is shaped exactly like a message request for the same account (credential,
/// Claude Code identity block, merged betas), so the count matches what would be sent. Counting is
/// not inference: it is not written to the request log and does not cool accounts down.
pub async fn count_tokens(
    State(app): State<App>,
    h: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    const ENDPOINT: &str = "messages/count_tokens";
    let model = body["model"]
        .as_str()
        .ok_or(ApiError::bad("model is required"))?
        .to_string();
    let candidates: Vec<_> = candidates(&app, &model, false)?
        .into_iter()
        .filter(|(c, _)| compatible(c, ENDPOINT))
        .collect();
    if candidates.is_empty() {
        return Err(ApiError::bad(
            "Token counting requires an Anthropic connection for this model.",
        ));
    }
    let _permit = permit(&app)?;
    let count = candidates.len();
    for (i, (mut c, target_model)) in candidates.into_iter().enumerate() {
        let last = i + 1 == count;
        if let Err(e) = credentials::refresh(&app, &mut c).await {
            if last {
                return Err(e);
            }
            continue;
        }
        let plan = plan_request(ENDPOINT, &body, &c, &target_model, false);
        let res = match send_with_auth_retry(&app, &mut c, &plan, &h, false).await {
            Ok(res) => res,
            Err(e) if e.is_connect() && !last => continue,
            Err(_) => {
                return Err(ApiError::upstream(
                    "Provider connection failed or timed out",
                ));
            }
        };
        let status = res.status();
        let headers = res.headers().clone();
        let value = read_json_bounded(res).await;
        if status.is_success() {
            if !value["input_tokens"].is_u64() {
                return Err(ApiError::upstream("Invalid provider token count response"));
            }
            return Ok((status, Json(value)).into_response());
        }
        if matches!(status.as_u16(), 401 | 403 | 429 | 502 | 503 | 504) && !last {
            continue;
        }
        let redactor = Redactor::new(&c, &h);
        let mut response = provider_error(status, &value, &redactor, "messages");
        if let Some(v) = headers.get("retry-after") {
            response.headers_mut().insert("retry-after", v.clone());
        }
        return Ok(response);
    }
    Err(ApiError::new(503, "All routes unavailable"))
}
pub async fn gemini(
    State(app): State<App>,
    Path(action): Path<String>,
    h: HeaderMap,
    Json(mut v): Json<Value>,
) -> Result<Response, ApiError> {
    let (model, method) = action.rsplit_once(':').ok_or(ApiError::bad(
        "Expected model:generateContent or model:streamGenerateContent",
    ))?;
    if !["generateContent", "streamGenerateContent"].contains(&method) {
        return Err(ApiError::bad("Unsupported Gemini method"));
    }
    v["model"] = json!(model);
    v["stream"] = json!(method == "streamGenerateContent");
    execute(app, "gemini", v, h).await
}

/// The upstream URL and body for one account.
struct Plan {
    url: String,
    payload: Value,
    chat_translate: bool,
}
fn plan_request(
    endpoint: &str,
    body: &Value,
    c: &Connection,
    target_model: &str,
    stream: bool,
) -> Plan {
    let chat_translate = endpoint == "chat/completions" && c.kind == "codex";
    let mut payload = if chat_translate {
        chat_to_responses(body)
    } else {
        body.clone()
    };
    payload["model"] = json!(target_model);
    let path = if c.kind == "codex" {
        payload = codex_payload(payload);
        "responses".to_string()
    } else if endpoint == "gemini" {
        if let Some(o) = payload.as_object_mut() {
            o.remove("model");
            o.remove("stream");
        }
        let encoded: String =
            url::form_urlencoded::byte_serialize(target_model.as_bytes()).collect();
        if stream {
            format!("models/{encoded}:streamGenerateContent?alt=sse")
        } else {
            format!("models/{encoded}:generateContent")
        }
    } else {
        endpoint.to_string()
    };
    // Token counts must see the same system prompt the message request will carry.
    if endpoint.starts_with("messages") && c.kind == "anthropic" && c.oauth {
        ensure_claude_code_identity(&mut payload);
    }
    Plan {
        url: format!("{}/{}", c.base_url, path),
        payload,
        chat_translate,
    }
}
fn build_request(
    app: &App,
    c: &Connection,
    plan: &Plan,
    h: &HeaderMap,
    stream: bool,
) -> reqwest::RequestBuilder {
    let mut req = upstream_headers(app.client.post(&plan.url).json(&plan.payload), c)
        .headers(protocol_headers(h, c));
    if c.kind == "codex" {
        req = req.header(header::ACCEPT, "text/event-stream");
    }
    // Streams (and Codex, which always streams upstream) are bounded by the client's read
    // timeout between chunks instead of a total deadline.
    if !stream && c.kind != "codex" {
        req = req.timeout(app.timeout);
    }
    req
}

/// Sends the request. After a 401 for a subscription account, adopts or refreshes the credential
/// once and resends to the same account only if the token actually changed: a 401 means the
/// provider did not run the request, so this is not a replay of inference.
async fn send_with_auth_retry(
    app: &App,
    c: &mut Connection,
    plan: &Plan,
    h: &HeaderMap,
    stream: bool,
) -> reqwest::Result<reqwest::Response> {
    let res = build_request(app, c, plan, h, stream).send().await?;
    if res.status() != StatusCode::UNAUTHORIZED || !c.oauth {
        return Ok(res);
    }
    let rejected = c.api_key.clone();
    match credentials::refresh_forced(app, c).await {
        Ok(()) if c.api_key != rejected => {
            drop(res);
            build_request(app, c, plan, h, stream).send().await
        }
        _ => Ok(res),
    }
}

/// What a non-streaming collection produced.
enum Collected {
    Value(Value),
    /// The provider failed in-band; this response is final.
    Failed(Response),
}

/// Reads a complete non-streaming response, bounded in size. SSE bodies (Codex always streams)
/// are reduced to their final `response` object.
async fn collect(
    res: reqwest::Response,
    sse: bool,
    guard: &mut RecordGuard,
    redactor: &Redactor,
    endpoint: &str,
) -> Result<Collected, ApiError> {
    let mut upstream = res.bytes_stream();
    let mut bytes = Vec::new();
    let mut parser = SseParser::new();
    let mut final_response: Option<Value> = None;
    let mut items: BTreeMap<u64, Value> = BTreeMap::new();
    let mut unindexed: Vec<Value> = Vec::new();
    while let Some(chunk) = upstream.next().await {
        let b = chunk.map_err(|_| {
            guard.finish(502, Some("Upstream response interrupted"));
            ApiError::upstream("Upstream response interrupted")
        })?;
        if !sse {
            bytes.extend_from_slice(&b);
            if bytes.len() > MAX_COLLECTED {
                guard.finish(502, Some("Response exceeds 16 MiB"));
                return Err(ApiError::upstream("Response exceeds 16 MiB"));
            }
            continue;
        }
        let events = parser.push(&b).inspect_err(|_| {
            guard.finish(502, Some("Oversized upstream event"));
        })?;
        for v in events {
            guard.usage(&v);
            match v["type"].as_str().unwrap_or("") {
                "response.output_item.done" if v["item"].is_object() => {
                    match v["output_index"].as_u64() {
                        Some(i) => {
                            items.insert(i, v["item"].clone());
                        }
                        None => unindexed.push(v["item"].clone()),
                    }
                }
                "response.completed" | "response.incomplete" => {
                    final_response = Some(v["response"].clone());
                }
                "response.failed" | "error" => {
                    let error = if v["type"] == "response.failed" {
                        json!({"error": v["response"]["error"]})
                    } else {
                        v.clone()
                    };
                    guard.finish(502, Some(&format!("{} in stream", error_label(&error))));
                    let response =
                        provider_error(StatusCode::BAD_GATEWAY, &error, redactor, endpoint);
                    return Ok(Collected::Failed(response));
                }
                _ => {}
            }
        }
    }
    if !sse {
        return serde_json::from_slice::<Value>(&bytes)
            .map(Collected::Value)
            .map_err(|_| {
                guard.finish(502, Some("Invalid provider JSON response"));
                ApiError::upstream("Invalid provider JSON response")
            });
    }
    let mut response = final_response.ok_or_else(|| {
        guard.finish(502, Some("Provider ended without a completed response"));
        ApiError::upstream("Provider ended without a completed response")
    })?;
    patch_completed_output(&mut response, items, unindexed);
    Ok(Collected::Value(response))
}

pub async fn execute(
    app: App,
    endpoint: &str,
    body: Value,
    h: HeaderMap,
) -> Result<Response, ApiError> {
    let model = body["model"]
        .as_str()
        .ok_or(ApiError::bad("model is required"))?
        .to_string();
    let wants_stream = body["stream"].as_bool().unwrap_or(false);
    let mut candidates: Vec<_> = candidates(&app, &model, false)?
        .into_iter()
        .filter(|(c, _)| compatible(c, endpoint))
        .collect();
    apply_affinity(&app, body["previous_response_id"].as_str(), &mut candidates)
        .map_err(|m| ApiError::new(409, m))?;
    if candidates.is_empty() {
        return Err(ApiError::bad(
            "This model requires its native provider endpoint. Use /v1/messages for Anthropic, /v1beta/models for Gemini, or /v1/responses for Codex.",
        ));
    }
    let permit = permit(&app)?;
    let transport = if wants_stream { "sse" } else { "http" };
    let mut guard = RecordGuard::new(app.clone(), &candidates[0].0, &model, transport, permit);
    let count = candidates.len();
    for (i, (mut c, target_model)) in candidates.into_iter().enumerate() {
        let last = i + 1 == count;
        guard.attribute(&c);
        if let Err(e) = credentials::refresh(&app, &mut c).await {
            guard.finish(e.status.as_u16(), Some("Credential refresh failed"));
            if !last {
                cool_account(&app, &c.id, 30);
                continue;
            }
            return Err(e);
        }
        let plan = plan_request(endpoint, &body, &c, &target_model, wants_stream);
        let res = match send_with_auth_retry(&app, &mut c, &plan, &h, wants_stream).await {
            Ok(res) => res,
            Err(e) => {
                let never_connected = e.is_connect();
                tracing::warn!(error = %e.without_url(), "Upstream HTTP connection failed");
                // Only a connection that was never established is safe to send elsewhere.
                if never_connected && !last {
                    cool_model(&app, &c.id, &target_model, 10);
                    continue;
                }
                guard.finish(502, Some("Provider connection failed or timed out"));
                return Err(ApiError::upstream(
                    "Provider connection failed or timed out",
                ));
            }
        };
        let status = res.status();
        let redactor = Redactor::new(&c, &h);
        if !status.is_success() {
            let headers = res.headers().clone();
            let error_body = read_json_bounded(res).await;
            let code = status.as_u16();
            let retry = retry_seconds(&headers, &error_body, code);
            if matches!(code, 401 | 403 | 429 | 502 | 503 | 504) {
                cool_for_status(&app, &c.id, &target_model, code, retry);
                if !last {
                    continue;
                }
            }
            guard.finish(code, Some(&error_label(&error_body)));
            let mut response = provider_error(status, &error_body, &redactor, endpoint);
            if let Some(v) = headers.get("retry-after") {
                response.headers_mut().insert("retry-after", v.clone());
            } else if code == 429 {
                response
                    .headers_mut()
                    .insert("retry-after", HeaderValue::from(retry));
            }
            return Ok(response);
        }
        let sse = c.kind == "codex"
            || res
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains("text/event-stream"));
        if wants_stream && sse {
            let translator = plan
                .chat_translate
                .then(|| ChatTranslator::new(guard.id(), model.clone()));
            let stream = relay_stream(
                app.clone(),
                guard,
                res.bytes_stream(),
                c.id.clone(),
                status.as_u16(),
                translator,
                redactor,
            );
            return Ok(Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .header(header::CACHE_CONTROL, "no-cache")
                .header("x-accel-buffering", "no")
                .body(Body::from_stream(stream))
                .expect("valid streaming response"));
        }
        let value = match collect(res, sse, &mut guard, &redactor, endpoint).await? {
            Collected::Value(v) => v,
            Collected::Failed(response) => return Ok(response),
        };
        guard.usage(&value);
        if let Some(response_id) = value["id"].as_str() {
            remember_response(&app, response_id, &c.id);
        }
        guard.finish(status.as_u16(), None);
        let value = if plan.chat_translate {
            response_to_chat(&value, &model)
        } else {
            value
        };
        return Ok((status, Json(value)).into_response());
    }
    guard.finish(503, Some("All routes unavailable"));
    Err(ApiError::new(503, "All routes unavailable"))
}

// ---------------------------------------------------------------------------------------------
// Responses WebSocket bridge
// ---------------------------------------------------------------------------------------------

#[derive(serde::Deserialize)]
pub struct WsQuery {
    model: Option<String>,
}
pub async fn responses_ws(
    State(app): State<App>,
    h: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let permit = permit(&app)?;
    Ok(ws
        .max_message_size(64 * 1024 * 1024)
        .on_upgrade(move |socket| bridge(app, socket, h, None, permit)))
}
pub async fn playground_ws(
    State(app): State<App>,
    h: HeaderMap,
    Query(q): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    if let Some(model) = &q.model {
        candidates(&app, model, true)?;
    }
    let permit = permit(&app)?;
    Ok(ws
        .max_message_size(64 * 1024 * 1024)
        .on_upgrade(move |socket| bridge(app, socket, h, q.model, permit)))
}

fn ws_error_frame(kind: &str, message: &str) -> AxMessage {
    AxMessage::Text(
        json!({"type":"error","error":{"type":kind,"message":message}})
            .to_string()
            .into(),
    )
}
fn ws_interrupted_frame(message: &str) -> AxMessage {
    AxMessage::Text(
        json!({"type":"error","error":{"type":"upstream_interrupted","message":message,"retryable":true,"partial_output":true}})
            .to_string()
            .into(),
    )
}
async fn ws_error(socket: &mut WebSocket, message: &str) {
    let _ = socket.send(ws_error_frame("gateway_error", message)).await;
    let _ = socket.close().await;
}

/// Waits for the first text frame. Control frames are answered or ignored, but the 30 s deadline
/// is absolute so a client cannot hold a permit by pinging. `Err(None)` means the client left.
async fn first_text_frame(socket: &mut WebSocket) -> Result<String, Option<&'static str>> {
    let deadline = tokio::time::Instant::now() + WS_FIRST_FRAME_DEADLINE;
    loop {
        let frame = match tokio::time::timeout_at(deadline, socket.recv()).await {
            Err(_) => return Err(Some("Send response.create within 30 seconds")),
            Ok(None | Some(Err(_))) => return Err(None),
            Ok(Some(Ok(frame))) => frame,
        };
        match frame {
            AxMessage::Text(t) => return Ok(t.to_string()),
            AxMessage::Ping(p) => {
                if socket.send(AxMessage::Pong(p)).await.is_err() {
                    return Err(None);
                }
            }
            AxMessage::Pong(_) => {}
            AxMessage::Close(_) => return Err(None),
            AxMessage::Binary(_) => {
                return Err(Some(
                    "The first frame must be a response.create JSON text frame",
                ));
            }
        }
    }
}

type Upstream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Why an upstream WebSocket handshake failed.
enum Handshake {
    /// The provider answered the upgrade with an HTTP status.
    Rejected { status: u16, retry: u64 },
    /// Connect error, timeout or invalid URL.
    Unreachable,
}

async fn ws_handshake(c: &Connection, h: &HeaderMap) -> Result<Upstream, Handshake> {
    let mut url = url::Url::parse(&format!("{}/responses", c.base_url))
        .map_err(|_| Handshake::Unreachable)?;
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme).map_err(|_| Handshake::Unreachable)?;
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|_| Handshake::Unreachable)?;
    let headers = request.headers_mut();
    if let Ok(v) = format!("Bearer {}", c.api_key).parse() {
        headers.insert(header::AUTHORIZATION, v);
    }
    if !c.account_id.is_empty()
        && let Ok(v) = c.account_id.parse()
    {
        headers.insert("chatgpt-account-id", v);
    }
    let beta = h
        .get("openai-beta")
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static("responses_websockets=2026-02-06"));
    headers.insert("openai-beta", beta);
    if c.kind == "codex" {
        headers.insert("originator", HeaderValue::from_static("codex_cli_rs"));
        headers.insert(
            "user-agent",
            HeaderValue::from_static("codex_cli_rs/0.159.3"),
        );
    }
    match tokio::time::timeout(
        WS_HANDSHAKE_TIMEOUT,
        tokio_tungstenite::connect_async(request),
    )
    .await
    {
        Ok(Ok((upstream, _))) => Ok(upstream),
        Ok(Err(tokio_tungstenite::tungstenite::Error::Http(response))) => {
            let status = response.status().as_u16();
            let retry = retry_seconds(response.headers(), &Value::Null, status);
            Err(Handshake::Rejected { status, retry })
        }
        _ => Err(Handshake::Unreachable),
    }
}

/// Opens one upstream socket, trying accounts in order. Rotation is safe here: the handshake is a
/// GET and no inference frame has been sent to any account yet.
async fn connect_upstream(
    app: &App,
    h: &HeaderMap,
    candidates: Vec<(Connection, String)>,
    guard: &mut RecordGuard,
) -> Option<(Connection, String, Upstream)> {
    for (mut c, target) in candidates {
        guard.attribute(&c);
        if credentials::refresh(app, &mut c).await.is_err() {
            cool_account(app, &c.id, 30);
            continue;
        }
        let mut result = ws_handshake(&c, h).await;
        if let Err(Handshake::Rejected { status: 401, .. }) = result
            && c.oauth
        {
            let rejected = c.api_key.clone();
            if credentials::refresh_forced(app, &mut c).await.is_ok() && c.api_key != rejected {
                result = ws_handshake(&c, h).await;
            }
        }
        match result {
            Ok(upstream) => return Some((c, target, upstream)),
            Err(Handshake::Rejected { status, retry }) => {
                cool_for_status(app, &c.id, &target, status, retry);
            }
            Err(Handshake::Unreachable) => cool_model(app, &c.id, &target, 10),
        }
    }
    None
}

/// Cooldown implied by an in-band WebSocket error (`error` or `response.failed`), if any.
fn ws_error_cooldown(v: &Value) -> Option<(u16, u64)> {
    let error = if v["type"] == "response.failed" {
        &v["response"]["error"]
    } else {
        &v["error"]
    };
    let status = v["status"]
        .as_u64()
        .or(v["status_code"].as_u64())
        .or(error["status"].as_u64())
        .unwrap_or(0) as u16;
    let names = [error["type"].as_str(), error["code"].as_str()];
    let named = |list: &[&str]| names.iter().flatten().any(|n| list.contains(n));
    let body = json!({"error": error});
    if status == 429
        || named(&[
            "rate_limit_exceeded",
            "rate_limit_error",
            "usage_limit_reached",
            "insufficient_quota",
        ])
    {
        return Some((429, retry_seconds(&HeaderMap::new(), &body, 429)));
    }
    if matches!(status, 401 | 403)
        || named(&[
            "invalid_api_key",
            "authentication_error",
            "permission_error",
            "token_expired",
        ])
    {
        return Some((401, 30));
    }
    None
}

async fn bridge(
    app: App,
    mut socket: WebSocket,
    h: HeaderMap,
    selected: Option<String>,
    permit: OwnedSemaphorePermit,
) {
    let first = match first_text_frame(&mut socket).await {
        Ok(text) => text,
        Err(Some(message)) => return ws_error(&mut socket, message).await,
        Err(None) => return,
    };
    let Ok(mut event) = serde_json::from_str::<Value>(&first) else {
        return ws_error(&mut socket, "Invalid JSON frame").await;
    };
    if event["type"] != "response.create" {
        return ws_error(&mut socket, "First frame must be response.create").await;
    }
    let model = selected
        .or_else(|| event["response"]["model"].as_str().map(String::from))
        .or_else(|| event["model"].as_str().map(String::from));
    let Some(model) = model else {
        return ws_error(&mut socket, "model is required").await;
    };
    let mut cs = match candidates(&app, &model, true) {
        Ok(cs) => cs,
        Err(e) => return ws_error(&mut socket, &e.message).await,
    };
    let previous = event["response"]["previous_response_id"]
        .as_str()
        .or(event["previous_response_id"].as_str());
    if let Err(message) = apply_affinity(&app, previous, &mut cs) {
        return ws_error(&mut socket, message).await;
    }
    let mut guard = RecordGuard::new(app.clone(), &cs[0].0, &model, "websocket", permit);
    let Some((c, target, mut upstream)) = connect_upstream(&app, &h, cs, &mut guard).await else {
        guard.finish(502, Some("All upstream WebSocket handshakes failed"));
        return ws_error(
            &mut socket,
            "All upstream WebSocket handshakes failed. Check account status and retry after cooldown.",
        )
        .await;
    };
    rewrite_ws(&mut event, &c, &target);
    if upstream
        .send(Message::Text(event.to_string().into()))
        .await
        .is_err()
    {
        guard.finish(502, Some("Upstream WebSocket send failed"));
        return ws_error(
            &mut socket,
            "Upstream WebSocket send failed before any output. Retry.",
        )
        .await;
    }
    relay_ws(app, guard, socket, upstream, c, target, model).await;
}

/// Relays one session. A single upstream socket serves the whole session: frames are never
/// replayed and the account never changes, so previous_response_id and tool calls stay on their
/// origin.
async fn relay_ws(
    app: App,
    mut guard: RecordGuard,
    socket: WebSocket,
    upstream: Upstream,
    c: Connection,
    target: String,
    model: String,
) {
    let (mut client_tx, mut client_rx) = socket.split();
    let (mut up_tx, mut up_rx) = upstream.split();
    let idle = tokio::time::sleep(app.timeout);
    tokio::pin!(idle);
    // A response is in flight between response.create and its terminal event.
    let mut active = true;
    loop {
        tokio::select! {
            _ = &mut idle => {
                guard.finish(504, Some("WebSocket idle timeout"));
                let _ = client_tx.send(AxMessage::Close(None)).await;
                let _ = up_tx.send(Message::Close(None)).await;
                break;
            }
            incoming = client_rx.next() => match incoming {
                Some(Ok(AxMessage::Text(text))) => {
                    idle.as_mut().reset(tokio::time::Instant::now() + app.timeout);
                    let Ok(mut v) = serde_json::from_str::<Value>(&text) else {
                        let frame = ws_error_frame("invalid_request_error", "Ignored a frame that is not valid JSON. The session is still open.");
                        if client_tx.send(frame).await.is_err() {
                            break;
                        }
                        continue;
                    };
                    if v["type"] == "response.create" {
                        let requested = v["response"]["model"].as_str().or(v["model"].as_str()).unwrap_or(&model);
                        if requested != model && requested != target {
                            let frame = ws_error_frame("invalid_request_error", "Changing models requires a new WebSocket session");
                            if client_tx.send(frame).await.is_err() {
                                break;
                            }
                            continue;
                        }
                        rewrite_ws(&mut v, &c, &target);
                        guard.reopen();
                        active = true;
                    }
                    if up_tx.send(Message::Text(v.to_string().into())).await.is_err() {
                        guard.finish(502, Some("Upstream send failed"));
                        let _ = client_tx.send(ws_interrupted_frame("Upstream connection lost; the frame was not delivered. Reconnect and retry.")).await;
                        let _ = client_tx.send(AxMessage::Close(None)).await;
                        break;
                    }
                }
                Some(Ok(AxMessage::Binary(b))) => {
                    idle.as_mut().reset(tokio::time::Instant::now() + app.timeout);
                    if up_tx.send(Message::Binary(b)).await.is_err() {
                        guard.finish(502, Some("Upstream send failed"));
                        break;
                    }
                }
                Some(Ok(AxMessage::Ping(p))) => {
                    if client_tx.send(AxMessage::Pong(p)).await.is_err() {
                        break;
                    }
                }
                Some(Ok(AxMessage::Pong(_))) => {}
                Some(Ok(AxMessage::Close(_))) | None | Some(Err(_)) => {
                    if active {
                        guard.finish(499, Some("Client disconnected before completion"));
                    }
                    let _ = up_tx.send(Message::Close(None)).await;
                    break;
                }
            },
            incoming = up_rx.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    idle.as_mut().reset(tokio::time::Instant::now() + app.timeout);
                    if let Ok(v) = serde_json::from_str::<Value>(&text) {
                        guard.usage(&v);
                        match v["type"].as_str().unwrap_or("") {
                            "response.completed" | "response.incomplete" => {
                                active = false;
                                guard.finish(200, None);
                                if let Some(response_id) = v["response"]["id"].as_str() {
                                    remember_response(&app, response_id, &c.id);
                                }
                            }
                            "response.failed" | "error" => {
                                active = false;
                                guard.finish(502, Some(&format!("{} on WebSocket", error_label(&v))));
                                if let Some((status, retry)) = ws_error_cooldown(&v) {
                                    cool_for_status(&app, &c.id, &target, status, retry);
                                }
                            }
                            _ => {}
                        }
                    }
                    if client_tx.send(AxMessage::Text(text.to_string().into())).await.is_err() {
                        if active {
                            guard.finish(499, Some("Client disconnected before completion"));
                        }
                        let _ = up_tx.send(Message::Close(None)).await;
                        break;
                    }
                }
                Some(Ok(Message::Binary(b))) => {
                    if client_tx.send(AxMessage::Binary(b)).await.is_err() {
                        if active {
                            guard.finish(499, Some("Client disconnected before completion"));
                        }
                        break;
                    }
                }
                Some(Ok(Message::Ping(p))) => {
                    let _ = up_tx.send(Message::Pong(p)).await;
                }
                Some(Ok(Message::Close(_))) | None => {
                    if active {
                        guard.finish(502, Some("Upstream closed before completion"));
                        let _ = client_tx.send(ws_interrupted_frame("Upstream closed before completion; output may be partial.")).await;
                    }
                    let _ = client_tx.send(AxMessage::Close(None)).await;
                    break;
                }
                Some(Err(_)) => {
                    if active {
                        guard.finish(502, Some("Upstream WebSocket interrupted"));
                        let _ = client_tx.send(ws_interrupted_frame("Connection interrupted; output may be partial. Reconnect on the same account or start a new conversation.")).await;
                    }
                    let _ = client_tx.send(AxMessage::Close(None)).await;
                    break;
                }
                Some(Ok(_)) => {}
            }
        }
    }
}

fn rewrite_ws(v: &mut Value, c: &Connection, target: &str) {
    // Accept the playground's nested convenience envelope, but emit the native Responses
    // wire format: response.create fields are top-level (unlike Realtime API events).
    let mut payload = if v["response"].is_object() {
        v["response"].clone()
    } else {
        v.clone()
    };
    if let Some(stream_id) = v.get("stream_id") {
        payload["stream_id"] = stream_id.clone();
    }
    payload["model"] = json!(target);
    if c.kind == "codex" {
        payload = codex_payload(payload);
    }
    if let Some(o) = payload.as_object_mut() {
        o.remove("stream");
        o.remove("background");
    }
    payload["type"] = json!("response.create");
    *v = payload;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_shapes_are_redacted_without_eating_words() {
        let s = "key sk-abc12345678 and Bearer eyJhbGciOi.payload.sig; sky-high sk-short";
        let r = redact_token_shapes(s);
        assert_eq!(r, "key [redacted] and Bearer [redacted]; sky-high sk-short");
    }

    #[test]
    fn identity_is_prepended_once_and_user_system_is_kept() {
        let mut p = json!({"system":"Be terse."});
        ensure_claude_code_identity(&mut p);
        assert_eq!(p["system"][0]["text"], CLAUDE_CODE_IDENTITY);
        assert_eq!(p["system"][1]["text"], "Be terse.");
        let again = p.clone();
        ensure_claude_code_identity(&mut p);
        assert_eq!(p, again);
    }

    #[test]
    fn sse_parser_handles_long_lines_split_in_small_chunks() {
        let mut parser = SseParser::new();
        let event = format!("data: {}\n\n", json!({"x":"y".repeat(10_000)}));
        let mut events = Vec::new();
        for chunk in event.as_bytes().chunks(7) {
            events.extend(parser.push(chunk).unwrap());
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["x"].as_str().unwrap().len(), 10_000);
    }
}
