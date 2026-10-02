//! Translation between client protocols and Antigravity's Gemini dialect.
//!
//! Supported client protocols: native Gemini `generateContent` (passthrough), OpenAI Chat
//! Completions and Anthropic Messages. OpenAI Responses is not supported for Antigravity accounts;
//! [`supports`] reports that so the gateway can answer with a clear 400.
//!
//! Anything a client sends that Antigravity cannot carry (remote image URLs, audio, server tools,
//! `n > 1`, log probabilities, redacted thinking) is rejected with a 400 that names the field,
//! rather than silently dropped. Fields with no upstream meaning that do not change the result
//! (`user`, `metadata`, `store`, `stream_options`, `parallel_tool_calls`, cache-control hints)
//! are accepted and ignored; docs/providers.md lists them.
//!
//! Thought signatures: Claude models need the signature on the thinking part itself, and
//! Antigravity rejects dummy signatures. Gemini models carry the signature on the function call
//! or text part that follows the thought, and accept Google's documented
//! `skip_thought_signature_validator` when the original is unavailable. Anthropic clients carry
//! signatures in thinking blocks; Chat clients cannot, so signatures are kept in a bounded
//! in-memory cache keyed by tool call id (opaque signatures only, never text).
use super::{EVENT_MAX, is_claude_model, schema, wrap};
use crate::app::ApiError;
use axum::body::Bytes;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeSet, HashMap},
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

/// Client protocol of an incoming request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// Gemini `generateContent` / `streamGenerateContent` bodies.
    Gemini,
    /// OpenAI Chat Completions.
    Chat,
    /// Anthropic Messages.
    Messages,
}

/// Whether a gateway endpoint can be served by an Antigravity account.
pub fn supports(endpoint: &str) -> Option<Protocol> {
    match endpoint {
        "gemini" => Some(Protocol::Gemini),
        "chat/completions" => Some(Protocol::Chat),
        "messages" => Some(Protocol::Messages),
        _ => None,
    }
}

/// Google's documented placeholder for a Gemini function call whose signature is unavailable.
pub const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";

fn bad(message: impl AsRef<str>) -> ApiError {
    ApiError::bad(message.as_ref())
}

// ---------------------------------------------------------------------------------------------
// Signature cache (Chat clients only)
// ---------------------------------------------------------------------------------------------

const SIGNATURE_TTL: Duration = Duration::from_secs(3600);
const SIGNATURE_MAX: usize = 4096;
static SIGNATURES: LazyLock<Mutex<HashMap<String, (String, Instant)>>> =
    LazyLock::new(Default::default);

fn remember_signature(call_id: &str, signature: &str) {
    if call_id.is_empty() || signature.is_empty() || signature.len() > 64 * 1024 {
        return;
    }
    let mut cache = SIGNATURES.lock().expect("signature cache");
    let now = Instant::now();
    cache.retain(|_, (_, at)| now.duration_since(*at) < SIGNATURE_TTL);
    if cache.len() >= SIGNATURE_MAX
        && let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, (_, at))| *at)
            .map(|(k, _)| k.clone())
    {
        cache.remove(&oldest);
    }
    cache.insert(call_id.to_string(), (signature.to_string(), now));
}
fn recall_signature(call_id: &str) -> Option<String> {
    let cache = SIGNATURES.lock().expect("signature cache");
    cache
        .get(call_id)
        .filter(|(_, at)| at.elapsed() < SIGNATURE_TTL)
        .map(|(s, _)| s.clone())
}

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

/// The planned upstream request plus what the response translator needs to know.
pub struct Plan {
    /// The full `v1internal` envelope.
    pub body: Value,
    /// Tools whose schema received the Claude placeholder argument.
    pub placeholder_tools: BTreeSet<String>,
}

/// A `data:` URL split into MIME type and base64 payload.
fn data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let mime = meta.strip_suffix(";base64")?;
    (!mime.is_empty() && !data.is_empty()).then(|| (mime.to_string(), data.to_string()))
}
fn inline(mime: &str, data: &str) -> Value {
    json!({"inlineData": {"mimeType": mime, "data": data}})
}
fn text_part(text: &str) -> Value {
    json!({"text": text})
}

/// Appends `parts` as a content of `role`, merging into the previous content when the role
/// repeats (Gemini expects one turn per role, and parallel tool results must share a turn).
fn push_content(contents: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    if let Some(last) = contents.last_mut()
        && last["role"] == role
        && let Some(existing) = last["parts"].as_array_mut()
    {
        existing.extend(parts);
        return;
    }
    contents.push(json!({"role": role, "parts": parts}));
}

/// A function declaration for `model`: Gemini models keep the client's JSON Schema in
/// `parametersJsonSchema`; Claude models get a cleaned `parameters` schema.
fn declaration(
    model: &str,
    name: &str,
    description: Option<&str>,
    params: Option<&Value>,
    placeholders: &mut BTreeSet<String>,
) -> Result<Value, ApiError> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
    {
        return Err(bad(format!(
            "Tool name {name:?} is not valid for Antigravity (letters, digits, _ - . : up to 128)."
        )));
    }
    let mut d = json!({"name": name});
    if let Some(desc) = description.filter(|d| !d.is_empty()) {
        d["description"] = json!(desc);
    }
    let params = params
        .cloned()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    if is_claude_model(model) {
        let cleaned = schema::clean_for_claude(&params);
        if cleaned.placeholder {
            placeholders.insert(name.to_string());
        }
        d["parameters"] = cleaned.schema;
    } else {
        d["parametersJsonSchema"] = params;
    }
    Ok(d)
}

fn number(v: &Value, field: &str) -> Result<Option<Value>, ApiError> {
    match v {
        Value::Null => Ok(None),
        Value::Number(_) => Ok(Some(v.clone())),
        _ => Err(bad(format!("{field} must be a number"))),
    }
}

/// Rejects a field that Antigravity cannot honour.
fn reject_if(present: bool, field: &str) -> Result<(), ApiError> {
    if present {
        Err(bad(format!(
            "{field} is not supported for Antigravity accounts."
        )))
    } else {
        Ok(())
    }
}

/// Splits a Gemini response into its first candidate's parts, finish reason and usage.
fn candidate(v: &Value) -> (&[Value], Option<&str>, &Value) {
    let c = &v["candidates"][0];
    let parts = c["content"]["parts"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let finish = c["finishReason"].as_str().or_else(|| {
        v["promptFeedback"]["blockReason"]
            .as_str()
            .map(|_| "SAFETY")
    });
    (parts, finish, &v["usageMetadata"])
}

fn tokens(usage: &Value, key: &str) -> u64 {
    usage[key].as_u64().unwrap_or(0)
}

fn new_call_id(prefix: &str) -> String {
    format!("{prefix}{}", uuid::Uuid::new_v4().simple())
}

// ---------------------------------------------------------------------------------------------
// Gemini passthrough
// ---------------------------------------------------------------------------------------------

/// Plans a native Gemini request. The body is forwarded as sent, except routing fields; Claude
/// models get their tool schemas cleaned.
pub fn plan_gemini(client: &Value, model: &str, project: &str) -> Result<Plan, ApiError> {
    let mut request = client.clone();
    reject_if(!request["cachedContent"].is_null(), "cachedContent")?;
    let mut placeholders = BTreeSet::new();
    if is_claude_model(model)
        && let Some(tools) = request["tools"].as_array_mut()
    {
        for tool in tools {
            for key in ["functionDeclarations", "function_declarations"] {
                if let Some(decls) = tool[key].as_array_mut() {
                    for d in decls {
                        let name = d["name"].as_str().unwrap_or("").to_string();
                        let params = d.as_object_mut().and_then(|o| {
                            o.remove("parametersJsonSchema")
                                .or_else(|| o.remove("parameters_json_schema"))
                                .or_else(|| o.remove("parameters"))
                        });
                        let cleaned = schema::clean_for_claude(
                            &params.unwrap_or_else(|| json!({"type": "object"})),
                        );
                        if cleaned.placeholder {
                            placeholders.insert(name);
                        }
                        d["parameters"] = cleaned.schema;
                    }
                }
            }
        }
    }
    Ok(Plan {
        body: wrap(request, model, project),
        placeholder_tools: placeholders,
    })
}

// ---------------------------------------------------------------------------------------------
// OpenAI Chat Completions -> Gemini
// ---------------------------------------------------------------------------------------------

fn chat_text(content: &Value, role: &str) -> Result<String, ApiError> {
    match content {
        Value::Null => Ok(String::new()),
        Value::String(s) => Ok(s.clone()),
        Value::Array(parts) => {
            let mut out = Vec::new();
            for p in parts {
                match p["type"].as_str() {
                    Some("text") => out.push(p["text"].as_str().unwrap_or("").to_string()),
                    Some("refusal") => out.push(p["refusal"].as_str().unwrap_or("").to_string()),
                    other => {
                        return Err(bad(format!(
                            "{role} message content part {:?} is not supported for Antigravity accounts.",
                            other.unwrap_or("?")
                        )));
                    }
                }
            }
            Ok(out.join("\n"))
        }
        _ => Err(bad(format!(
            "{role} message content must be a string or an array"
        ))),
    }
}

fn chat_user_parts(content: &Value) -> Result<Vec<Value>, ApiError> {
    match content {
        Value::String(s) => Ok(vec![text_part(s)]),
        Value::Array(items) => items
            .iter()
            .map(|p| match p["type"].as_str() {
                Some("text") => Ok(text_part(p["text"].as_str().unwrap_or(""))),
                Some("image_url") => {
                    let url = p["image_url"]["url"].as_str().or(p["image_url"].as_str()).unwrap_or("");
                    let (mime, data) = data_url(url).ok_or_else(|| bad("Antigravity accepts images as data: URLs only; remote image URLs are not supported."))?;
                    Ok(inline(&mime, &data))
                }
                Some("file") => {
                    let url = p["file"]["file_data"].as_str().unwrap_or("");
                    let (mime, data) = data_url(url).ok_or_else(|| bad("Antigravity accepts files as data: URLs in file.file_data only; file ids are not supported."))?;
                    Ok(inline(&mime, &data))
                }
                other => Err(bad(format!("Content part {:?} is not supported for Antigravity accounts.", other.unwrap_or("?")))),
            })
            .collect(),
        Value::Null => Ok(Vec::new()),
        _ => Err(bad("user message content must be a string or an array")),
    }
}

fn effort_budget(effort: &str) -> Option<u32> {
    match effort {
        "minimal" | "low" => Some(1024),
        "medium" => Some(8192),
        "high" | "xhigh" => Some(24576),
        _ => None,
    }
}

/// Plans an OpenAI Chat Completions request for an Antigravity model.
pub fn plan_chat(client: &Value, model: &str, project: &str) -> Result<Plan, ApiError> {
    reject_if(client["n"].as_u64().is_some_and(|n| n > 1), "n > 1")?;
    reject_if(
        client["logprobs"] == true || !client["top_logprobs"].is_null(),
        "logprobs",
    )?;
    reject_if(!client["audio"].is_null(), "audio output")?;
    reject_if(
        client["modalities"]
            .as_array()
            .is_some_and(|m| m.iter().any(|x| x != "text")),
        "non-text modalities",
    )?;
    reject_if(!client["prediction"].is_null(), "prediction")?;
    reject_if(
        client["logit_bias"]
            .as_object()
            .is_some_and(|m| !m.is_empty()),
        "logit_bias",
    )?;
    reject_if(
        !client["web_search_options"].is_null(),
        "web_search_options",
    )?;
    let messages = client["messages"]
        .as_array()
        .ok_or_else(|| bad("messages is required"))?;
    let claude = is_claude_model(model);

    let mut system: Vec<Value> = Vec::new();
    let mut contents: Vec<Value> = Vec::new();
    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut history_has_tool_calls = false;
    for m in messages {
        let role = m["role"].as_str().unwrap_or("");
        match role {
            "system" | "developer" => {
                let t = chat_text(&m["content"], role)?;
                if !t.is_empty() {
                    system.push(text_part(&t));
                }
            }
            "user" => push_content(&mut contents, "user", chat_user_parts(&m["content"])?),
            "assistant" => {
                let mut parts = Vec::new();
                let t = chat_text(&m["content"], role)?;
                if !t.is_empty() {
                    parts.push(text_part(&t));
                }
                for call in m["tool_calls"].as_array().into_iter().flatten() {
                    if call["type"].as_str().is_some_and(|t| t != "function") {
                        return Err(bad(
                            "Only function tool calls are supported for Antigravity accounts.",
                        ));
                    }
                    let id = call["id"].as_str().unwrap_or("").to_string();
                    let name = call["function"]["name"].as_str().unwrap_or("").to_string();
                    let raw = call["function"]["arguments"].as_str().unwrap_or("{}");
                    let args: Value = if raw.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(raw).map_err(|_| {
                            bad(format!("Tool call {id:?} arguments are not valid JSON."))
                        })?
                    };
                    let mut part = json!({"functionCall": {"name": name, "args": args}});
                    if !id.is_empty() {
                        part["functionCall"]["id"] = json!(id);
                        tool_names.insert(id.clone(), name.clone());
                    }
                    if !claude {
                        part["thoughtSignature"] =
                            json!(recall_signature(&id).unwrap_or_else(|| SKIP_SIGNATURE.into()));
                    }
                    history_has_tool_calls = true;
                    parts.push(part);
                }
                push_content(&mut contents, "model", parts);
            }
            "tool" => {
                let id = m["tool_call_id"].as_str().unwrap_or("");
                let name = tool_names.get(id).cloned().ok_or_else(|| {
                    bad(format!(
                        "Tool result {id:?} does not match an earlier tool call."
                    ))
                })?;
                let mut images = Vec::new();
                let result = match &m["content"] {
                    Value::String(s) => s.clone(),
                    Value::Array(items) => {
                        let mut texts = Vec::new();
                        for p in items {
                            match p["type"].as_str() {
                                Some("text") => {
                                    texts.push(p["text"].as_str().unwrap_or("").to_string())
                                }
                                Some("image_url") => {
                                    let url = p["image_url"]["url"].as_str().unwrap_or("");
                                    let (mime, data) = data_url(url).ok_or_else(|| {
                                        bad("Tool result images must be data: URLs.")
                                    })?;
                                    images.push(inline(&mime, &data));
                                }
                                other => {
                                    return Err(bad(format!(
                                        "Tool result part {:?} is not supported.",
                                        other.unwrap_or("?")
                                    )));
                                }
                            }
                        }
                        texts.join("\n")
                    }
                    Value::Null => String::new(),
                    _ => return Err(bad("tool message content must be a string or an array")),
                };
                let mut fr = json!({"id": id, "name": name, "response": {"result": result}});
                if !images.is_empty() {
                    fr["parts"] = Value::Array(images);
                }
                push_content(&mut contents, "user", vec![json!({"functionResponse": fr})]);
            }
            "function" => {
                return Err(bad(
                    "The legacy function role is not supported; use tool messages.",
                ));
            }
            other => return Err(bad(format!("Message role {other:?} is not supported."))),
        }
    }

    let mut request = json!({"contents": contents});
    if !system.is_empty() {
        request["systemInstruction"] = json!({"role": "user", "parts": system});
    }
    let mut placeholders = BTreeSet::new();
    let mut decls = Vec::new();
    for t in client["tools"].as_array().into_iter().flatten() {
        if t["type"] != "function" {
            return Err(bad(format!(
                "Tool type {:?} is not supported for Antigravity accounts.",
                t["type"].as_str().unwrap_or("?")
            )));
        }
        let f = &t["function"];
        decls.push(declaration(
            model,
            f["name"].as_str().unwrap_or(""),
            f["description"].as_str(),
            f.get("parameters"),
            &mut placeholders,
        )?);
    }
    let mut tool_mode = None;
    match &client["tool_choice"] {
        Value::Null => {}
        Value::String(s) if s == "none" => tool_mode = Some(json!({"mode": "NONE"})),
        Value::String(s) if s == "auto" => tool_mode = Some(json!({"mode": "AUTO"})),
        Value::String(s) if s == "required" => tool_mode = Some(json!({"mode": "ANY"})),
        Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("function") => {
            let name = o["function"]["name"].as_str().unwrap_or("");
            tool_mode = Some(json!({"mode": "ANY", "allowedFunctionNames": [name]}));
        }
        _ => return Err(bad("tool_choice is not valid")),
    }
    if !decls.is_empty() {
        request["tools"] = json!([{"functionDeclarations": decls}]);
    }
    if let Some(mode) = tool_mode {
        request["toolConfig"] = json!({"functionCallingConfig": mode});
    }

    let mut config_map = Map::new();
    if let Some(v) = number(&client["temperature"], "temperature")? {
        config_map.insert("temperature".into(), v);
    }
    if let Some(v) = number(&client["top_p"], "top_p")? {
        config_map.insert("topP".into(), v);
    }
    if let Some(v) = number(&client["presence_penalty"], "presence_penalty")? {
        config_map.insert("presencePenalty".into(), v);
    }
    if let Some(v) = number(&client["frequency_penalty"], "frequency_penalty")? {
        config_map.insert("frequencyPenalty".into(), v);
    }
    if let Some(v) = number(&client["seed"], "seed")? {
        config_map.insert("seed".into(), v);
    }
    if let Some(v) = number(&client["max_completion_tokens"], "max_completion_tokens")?
        .or(number(&client["max_tokens"], "max_tokens")?)
    {
        config_map.insert("maxOutputTokens".into(), v);
    }
    match &client["stop"] {
        Value::Null => {}
        Value::String(s) => {
            config_map.insert("stopSequences".into(), json!([s]));
        }
        Value::Array(a) => {
            config_map.insert("stopSequences".into(), Value::Array(a.clone()));
        }
        _ => return Err(bad("stop must be a string or an array")),
    }
    match client["response_format"]["type"].as_str() {
        None | Some("text") => {}
        Some("json_object") => {
            config_map.insert("responseMimeType".into(), json!("application/json"));
        }
        Some("json_schema") => {
            config_map.insert("responseMimeType".into(), json!("application/json"));
            if let Some(s) = client["response_format"]["json_schema"].get("schema") {
                config_map.insert("responseJsonSchema".into(), s.clone());
            }
        }
        Some(other) => return Err(bad(format!("response_format {other:?} is not supported"))),
    }
    if let Some(effort) = client["reasoning_effort"].as_str().filter(|e| *e != "none") {
        let budget = effort_budget(effort)
            .ok_or_else(|| bad(format!("reasoning_effort {effort:?} is not supported")))?;
        // Claude thinking needs signed thinking blocks replayed before earlier tool calls; Chat
        // clients cannot carry them, so thinking is only enabled on turns without prior calls.
        if !(claude && history_has_tool_calls) {
            let config = if claude || !model.starts_with("gemini-3") {
                json!({"includeThoughts": true, "thinkingBudget": budget})
            } else {
                json!({"includeThoughts": true, "thinkingLevel": if effort == "minimal" { "low" } else if effort == "xhigh" { "high" } else { effort }})
            };
            config_map.insert("thinkingConfig".into(), config);
        }
    }
    if !config_map.is_empty() {
        request["generationConfig"] = Value::Object(config_map);
    }
    Ok(Plan {
        body: wrap(request, model, project),
        placeholder_tools: placeholders,
    })
}

// ---------------------------------------------------------------------------------------------
// Anthropic Messages -> Gemini
// ---------------------------------------------------------------------------------------------

fn anthropic_source(block: &Value, what: &str) -> Result<Value, ApiError> {
    let src = &block["source"];
    match src["type"].as_str() {
        Some("base64") => {
            let mime = src["media_type"].as_str().unwrap_or("");
            let data = src["data"].as_str().unwrap_or("");
            if mime.is_empty() || data.is_empty() {
                return Err(bad(format!("{what} source needs media_type and data")));
            }
            Ok(inline(mime, data))
        }
        Some("text") if what == "document" => Ok(text_part(src["data"].as_str().unwrap_or(""))),
        other => Err(bad(format!(
            "{what} source type {:?} is not supported for Antigravity accounts (use base64).",
            other.unwrap_or("?")
        ))),
    }
}

/// Plans an Anthropic Messages request for an Antigravity model.
pub fn plan_messages(client: &Value, model: &str, project: &str) -> Result<Plan, ApiError> {
    let claude = is_claude_model(model);
    let messages = client["messages"]
        .as_array()
        .ok_or_else(|| bad("messages is required"))?;
    let mut system = Vec::new();
    match &client["system"] {
        Value::Null => {}
        Value::String(s) => system.push(text_part(s)),
        Value::Array(blocks) => {
            for b in blocks {
                if b["type"] != "text" {
                    return Err(bad("system blocks must be text"));
                }
                system.push(text_part(b["text"].as_str().unwrap_or("")));
            }
        }
        _ => return Err(bad("system must be a string or an array")),
    }

    let thinking_requested = matches!(
        client["thinking"]["type"].as_str(),
        Some("enabled" | "adaptive" | "auto")
    );
    let mut thinking_ok = thinking_requested;
    let mut contents = Vec::new();
    let mut tool_names: HashMap<String, String> = HashMap::new();
    for m in messages {
        let role = match m["role"].as_str() {
            Some("user") => "user",
            Some("assistant") => "model",
            other => return Err(bad(format!("Message role {other:?} is not supported."))),
        };
        let blocks: Vec<Value> = match &m["content"] {
            Value::String(s) => vec![json!({"type": "text", "text": s})],
            Value::Array(b) => b.clone(),
            _ => return Err(bad("message content must be a string or an array")),
        };
        let mut parts: Vec<Value> = Vec::new();
        // Gemini models: a thinking block's signature belongs on the next text or call part.
        let mut pending_signature: Option<String> = None;
        for (i, b) in blocks.iter().enumerate() {
            match b["type"].as_str().unwrap_or("") {
                "text" => {
                    let mut p = text_part(b["text"].as_str().unwrap_or(""));
                    if let Some(sig) = pending_signature.take() {
                        p["thoughtSignature"] = json!(sig);
                    }
                    parts.push(p);
                }
                "image" => parts.push(anthropic_source(b, "image")?),
                "document" => parts.push(anthropic_source(b, "document")?),
                "thinking" => {
                    let text = b["thinking"].as_str().unwrap_or("");
                    let sig = b["signature"].as_str().unwrap_or("");
                    if claude {
                        if sig.is_empty() {
                            // Antigravity validates Claude signatures; an unsigned block cannot be
                            // replayed, so thinking is turned off for this request.
                            thinking_ok = false;
                            continue;
                        }
                        parts.push(json!({"thought": true, "text": text, "thoughtSignature": sig}));
                    } else {
                        if !text.is_empty() {
                            parts.push(json!({"thought": true, "text": text}));
                        }
                        let next_carries = blocks.get(i + 1).is_some_and(|n| {
                            matches!(n["type"].as_str(), Some("text" | "tool_use"))
                        });
                        if !sig.is_empty() {
                            if next_carries {
                                pending_signature = Some(sig.to_string());
                            } else if let Some(last) = parts.last_mut() {
                                last["thoughtSignature"] = json!(sig);
                            }
                        }
                    }
                }
                "redacted_thinking" => {
                    return Err(bad(
                        "redacted_thinking blocks cannot be sent to Antigravity accounts.",
                    ));
                }
                "tool_use" => {
                    let id = b["id"].as_str().unwrap_or("").to_string();
                    let name = b["name"].as_str().unwrap_or("").to_string();
                    let input = if b["input"].is_null() {
                        json!({})
                    } else {
                        b["input"].clone()
                    };
                    let mut p = json!({"functionCall": {"id": id, "name": name, "args": input}});
                    if let Some(sig) = pending_signature.take() {
                        p["thoughtSignature"] = json!(sig);
                    } else if !claude {
                        p["thoughtSignature"] = json!(SKIP_SIGNATURE);
                    }
                    tool_names.insert(id, name);
                    parts.push(p);
                }
                "tool_result" => {
                    let id = b["tool_use_id"].as_str().unwrap_or("");
                    let name = tool_names.get(id).cloned().ok_or_else(|| {
                        bad(format!(
                            "tool_result {id:?} does not match an earlier tool_use."
                        ))
                    })?;
                    let mut images = Vec::new();
                    let text = match &b["content"] {
                        Value::String(s) => s.clone(),
                        Value::Array(items) => {
                            let mut texts = Vec::new();
                            for item in items {
                                match item["type"].as_str() {
                                    Some("text") => {
                                        texts.push(item["text"].as_str().unwrap_or("").to_string())
                                    }
                                    Some("image") => images.push(anthropic_source(item, "image")?),
                                    other => {
                                        return Err(bad(format!(
                                            "tool_result content {:?} is not supported.",
                                            other.unwrap_or("?")
                                        )));
                                    }
                                }
                            }
                            texts.join("\n")
                        }
                        Value::Null => String::new(),
                        _ => return Err(bad("tool_result content must be a string or an array")),
                    };
                    let response = if b["is_error"] == true {
                        json!({"error": text})
                    } else {
                        json!({"result": text})
                    };
                    let mut fr = json!({"id": id, "name": name, "response": response});
                    if !images.is_empty() {
                        fr["parts"] = Value::Array(images);
                    }
                    parts.push(json!({"functionResponse": fr}));
                }
                other => {
                    return Err(bad(format!(
                        "Content block {other:?} is not supported for Antigravity accounts."
                    )));
                }
            }
        }
        if let Some(sig) = pending_signature
            && let Some(last) = parts.last_mut()
        {
            last["thoughtSignature"] = json!(sig);
        }
        push_content(&mut contents, role, parts);
    }

    let mut request = json!({"contents": contents});
    if !system.is_empty() {
        request["systemInstruction"] = json!({"role": "user", "parts": system});
    }
    let mut placeholders = BTreeSet::new();
    let mut decls = Vec::new();
    for t in client["tools"].as_array().into_iter().flatten() {
        if t["type"].as_str().is_some_and(|ty| ty != "custom") {
            return Err(bad(format!(
                "Server tool {:?} is not supported for Antigravity accounts.",
                t["type"].as_str().unwrap_or("?")
            )));
        }
        decls.push(declaration(
            model,
            t["name"].as_str().unwrap_or(""),
            t["description"].as_str(),
            t.get("input_schema"),
            &mut placeholders,
        )?);
    }
    let mut tool_mode = None;
    match client["tool_choice"]["type"].as_str() {
        None => {}
        Some("auto") => tool_mode = Some(json!({"mode": "AUTO"})),
        Some("any") => tool_mode = Some(json!({"mode": "ANY"})),
        Some("none") => tool_mode = Some(json!({"mode": "NONE"})),
        Some("tool") => {
            tool_mode = Some(
                json!({"mode": "ANY", "allowedFunctionNames": [client["tool_choice"]["name"].as_str().unwrap_or("")]}),
            );
        }
        Some(other) => return Err(bad(format!("tool_choice {other:?} is not valid"))),
    }
    if !decls.is_empty() {
        request["tools"] = json!([{"functionDeclarations": decls}]);
    }
    if let Some(mode) = tool_mode {
        request["toolConfig"] = json!({"functionCallingConfig": mode});
    }

    let mut config_map = Map::new();
    for (from, to) in [
        ("temperature", "temperature"),
        ("top_p", "topP"),
        ("top_k", "topK"),
        ("max_tokens", "maxOutputTokens"),
    ] {
        if let Some(v) = number(&client[from], from)? {
            config_map.insert(to.into(), v);
        }
    }
    if let Some(stops) = client["stop_sequences"].as_array() {
        config_map.insert("stopSequences".into(), Value::Array(stops.clone()));
    }
    if thinking_requested && thinking_ok {
        let t = &client["thinking"];
        let config = match t["type"].as_str() {
            Some("enabled") => match t["budget_tokens"].as_u64() {
                Some(b) => json!({"includeThoughts": true, "thinkingBudget": b}),
                None => json!({"includeThoughts": true}),
            },
            _ => {
                json!({"includeThoughts": true, "thinkingLevel": client["output_config"]["effort"].as_str().unwrap_or("high")})
            }
        };
        config_map.insert("thinkingConfig".into(), config);
    }
    if !config_map.is_empty() {
        request["generationConfig"] = Value::Object(config_map);
    }
    Ok(Plan {
        body: wrap(request, model, project),
        placeholder_tools: placeholders,
    })
}

/// Plans a request for any supported protocol.
pub fn plan(
    protocol: Protocol,
    client: &Value,
    model: &str,
    project: &str,
) -> Result<Plan, ApiError> {
    match protocol {
        Protocol::Gemini => plan_gemini(client, model, project),
        Protocol::Chat => plan_chat(client, model, project),
        Protocol::Messages => plan_messages(client, model, project),
    }
}

// ---------------------------------------------------------------------------------------------
// Gemini -> client responses
// ---------------------------------------------------------------------------------------------

fn chat_finish(finish: Option<&str>, tool_calls: bool) -> &'static str {
    match finish {
        Some("MAX_TOKENS") => "length",
        Some(
            "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY",
        ) => "content_filter",
        _ if tool_calls => "tool_calls",
        _ => "stop",
    }
}
fn anthropic_stop(finish: Option<&str>, tool_calls: bool) -> &'static str {
    match finish {
        Some("MAX_TOKENS") => "max_tokens",
        Some(
            "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY",
        ) => "refusal",
        _ if tool_calls => "tool_use",
        _ => "end_turn",
    }
}
fn chat_usage(u: &Value) -> Value {
    let prompt = tokens(u, "promptTokenCount");
    let thoughts = tokens(u, "thoughtsTokenCount");
    let completion = tokens(u, "candidatesTokenCount") + thoughts;
    json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": u["totalTokenCount"].as_u64().unwrap_or(prompt + completion),
        "prompt_tokens_details": {"cached_tokens": tokens(u, "cachedContentTokenCount")},
        "completion_tokens_details": {"reasoning_tokens": thoughts},
    })
}
fn anthropic_usage(u: &Value) -> Value {
    let cached = tokens(u, "cachedContentTokenCount");
    json!({
        "input_tokens": tokens(u, "promptTokenCount").saturating_sub(cached),
        "cache_read_input_tokens": cached,
        "output_tokens": tokens(u, "candidatesTokenCount") + tokens(u, "thoughtsTokenCount"),
    })
}
fn call_args(fc: &Value, placeholders: &BTreeSet<String>) -> Value {
    let mut args = if fc["args"].is_object() {
        fc["args"].clone()
    } else {
        json!({})
    };
    if fc["name"]
        .as_str()
        .is_some_and(|n| placeholders.contains(n))
    {
        schema::strip_placeholder(&mut args);
    }
    args
}

/// Translates a complete (non-streaming) Gemini response into `protocol`'s response.
pub fn response(
    protocol: Protocol,
    gemini: Value,
    model: &str,
    placeholders: &BTreeSet<String>,
) -> Value {
    match protocol {
        Protocol::Gemini => gemini,
        Protocol::Chat => chat_response(&gemini, model, placeholders),
        Protocol::Messages => messages_response(&gemini, model, placeholders),
    }
}

fn chat_response(v: &Value, model: &str, placeholders: &BTreeSet<String>) -> Value {
    let (parts, finish, usage) = candidate(v);
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls = Vec::new();
    for p in parts {
        if let Some(fc) = p.get("functionCall") {
            let id = fc["id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_else(|| new_call_id("call_"));
            if let Some(sig) = p["thoughtSignature"].as_str() {
                remember_signature(&id, sig);
            }
            calls.push(json!({"id": id, "type": "function", "function": {"name": fc["name"], "arguments": call_args(fc, placeholders).to_string()}}));
        } else if p["thought"] == true {
            reasoning.push_str(p["text"].as_str().unwrap_or(""));
        } else if let Some(t) = p["text"].as_str() {
            text.push_str(t);
        }
    }
    let mut message = json!({"role": "assistant", "content": if text.is_empty() && !calls.is_empty() { Value::Null } else { json!(text) }});
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !calls.is_empty() {
        message["tool_calls"] = Value::Array(calls.clone());
    }
    json!({
        "id": v["responseId"].as_str().map_or_else(|| new_call_id("chatcmpl-"), |r| format!("chatcmpl-{r}")),
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": chat_finish(finish, !calls.is_empty())}],
        "usage": chat_usage(usage),
    })
}

fn messages_response(v: &Value, model: &str, placeholders: &BTreeSet<String>) -> Value {
    let (parts, finish, usage) = candidate(v);
    let mut blocks: Vec<Value> = Vec::new();
    let mut tool_calls = false;
    for p in parts {
        let sig = p["thoughtSignature"].as_str();
        if p["thought"] == true {
            let text = p["text"].as_str().unwrap_or("");
            match blocks.last_mut() {
                Some(last) if last["type"] == "thinking" && last["signature"] == "" => {
                    last["thinking"] = json!(format!(
                        "{}{}",
                        last["thinking"].as_str().unwrap_or(""),
                        text
                    ));
                    if let Some(s) = sig {
                        last["signature"] = json!(s);
                    }
                }
                _ => blocks.push(
                    json!({"type": "thinking", "thinking": text, "signature": sig.unwrap_or("")}),
                ),
            }
            continue;
        }
        // A signature on a non-thought part (Gemini models) belongs to the preceding thinking.
        if let Some(s) = sig {
            match blocks.last_mut() {
                Some(last) if last["type"] == "thinking" && last["signature"] == "" => {
                    last["signature"] = json!(s)
                }
                _ => blocks.push(json!({"type": "thinking", "thinking": "", "signature": s})),
            }
        }
        if let Some(fc) = p.get("functionCall") {
            tool_calls = true;
            let id = fc["id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_else(|| new_call_id("toolu_"));
            blocks.push(json!({"type": "tool_use", "id": id, "name": fc["name"], "input": call_args(fc, placeholders)}));
        } else if let Some(t) = p["text"].as_str() {
            match blocks.last_mut() {
                Some(last) if last["type"] == "text" => {
                    last["text"] = json!(format!("{}{}", last["text"].as_str().unwrap_or(""), t))
                }
                _ => blocks.push(json!({"type": "text", "text": t})),
            }
        }
    }
    // Unsigned thinking cannot be replayed; it is still shown, as the provider produced it.
    json!({
        "id": v["responseId"].as_str().map_or_else(|| new_call_id("msg_"), |r| format!("msg_{r}")),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": blocks,
        "stop_reason": anthropic_stop(finish, tool_calls),
        "stop_sequence": null,
        "usage": anthropic_usage(usage),
    })
}

// ---------------------------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------------------------

/// Turns upstream Gemini response chunks into client SSE bytes. Feed unwrapped chunks with
/// [`StreamTranslator::push`]; [`StreamTranslator::completed`] reports whether the turn ended
/// normally (only then is a terminal event such as `[DONE]` or `message_stop` emitted).
pub struct StreamTranslator {
    protocol: Protocol,
    model: String,
    placeholders: BTreeSet<String>,
    id: String,
    created: i64,
    started: bool,
    completed: bool,
    tool_index: usize,
    saw_tools: bool,
    usage: Value,
    /// Anthropic: index and kind of the open content block.
    open: Option<(usize, &'static str)>,
    next_block: usize,
    /// Anthropic: whether the open thinking block already has a signature.
    open_signed: bool,
}

fn sse_event(name: Option<&str>, data: &Value) -> Bytes {
    match name {
        Some(n) => Bytes::from(format!("event: {n}\ndata: {data}\n\n")),
        None => Bytes::from(format!("data: {data}\n\n")),
    }
}

impl StreamTranslator {
    pub fn new(protocol: Protocol, model: &str, placeholders: BTreeSet<String>) -> Self {
        Self {
            protocol,
            model: model.into(),
            placeholders,
            id: String::new(),
            created: chrono::Utc::now().timestamp(),
            started: false,
            completed: false,
            tool_index: 0,
            saw_tools: false,
            usage: Value::Null,
            open: None,
            next_block: 0,
            open_signed: false,
        }
    }
    /// True once the upstream turn finished normally and the terminal event was emitted.
    pub fn completed(&self) -> bool {
        self.completed
    }
    /// Latest usage metadata seen (Gemini `usageMetadata`), for accounting.
    pub fn usage(&self) -> &Value {
        &self.usage
    }

    /// Translates one unwrapped Gemini chunk.
    pub fn push(&mut self, chunk: &Value) -> Vec<Bytes> {
        if chunk["usageMetadata"].is_object() {
            self.usage = chunk["usageMetadata"].clone();
        }
        if self.id.is_empty() {
            self.id = chunk["responseId"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        }
        match self.protocol {
            Protocol::Gemini => {
                if super::is_terminal(chunk) {
                    self.completed = true;
                }
                vec![sse_event(None, chunk)]
            }
            Protocol::Chat => self.chat_chunk(chunk),
            Protocol::Messages => self.messages_chunk(chunk),
        }
    }

    fn chat_frame(&self, delta: Value, finish: Option<&str>) -> Bytes {
        sse_event(
            None,
            &json!({"id": format!("chatcmpl-{}", self.id), "object": "chat.completion.chunk", "created": self.created, "model": self.model,
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}),
        )
    }

    fn chat_chunk(&mut self, chunk: &Value) -> Vec<Bytes> {
        let mut out = Vec::new();
        if !self.started {
            self.started = true;
            out.push(self.chat_frame(json!({"role": "assistant", "content": ""}), None));
        }
        let (parts, finish, _) = candidate(chunk);
        for p in parts {
            if let Some(fc) = p.get("functionCall") {
                let id = fc["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| new_call_id("call_"));
                if let Some(sig) = p["thoughtSignature"].as_str() {
                    remember_signature(&id, sig);
                }
                let args = call_args(fc, &self.placeholders).to_string();
                out.push(self.chat_frame(
                    json!({"tool_calls": [{"index": self.tool_index, "id": id, "type": "function", "function": {"name": fc["name"], "arguments": args}}]}),
                    None,
                ));
                self.tool_index += 1;
                self.saw_tools = true;
            } else if p["thought"] == true {
                if let Some(t) = p["text"].as_str().filter(|t| !t.is_empty()) {
                    out.push(self.chat_frame(json!({"reasoning_content": t}), None));
                }
            } else if let Some(t) = p["text"].as_str().filter(|t| !t.is_empty()) {
                out.push(self.chat_frame(json!({"content": t}), None));
            }
        }
        if finish.is_some() || chunk["promptFeedback"]["blockReason"].is_string() {
            out.push(self.chat_frame(json!({}), Some(chat_finish(finish, self.saw_tools))));
            out.push(sse_event(
                None,
                &json!({"id": format!("chatcmpl-{}", self.id), "object": "chat.completion.chunk", "created": self.created, "model": self.model,
                    "choices": [], "usage": chat_usage(&self.usage)}),
            ));
            out.push(Bytes::from_static(b"data: [DONE]\n\n"));
            self.completed = true;
        }
        out
    }

    fn close_block(&mut self, out: &mut Vec<Bytes>) {
        if let Some((index, _)) = self.open.take() {
            out.push(sse_event(
                Some("content_block_stop"),
                &json!({"type": "content_block_stop", "index": index}),
            ));
        }
        self.open_signed = false;
    }
    fn open_block(&mut self, out: &mut Vec<Bytes>, kind: &'static str, block: Value) -> usize {
        self.close_block(out);
        let index = self.next_block;
        self.next_block += 1;
        self.open = Some((index, kind));
        out.push(sse_event(
            Some("content_block_start"),
            &json!({"type": "content_block_start", "index": index, "content_block": block}),
        ));
        index
    }
    fn signature(&mut self, out: &mut Vec<Bytes>, sig: &str) {
        let index = match self.open {
            Some((i, "thinking")) if !self.open_signed => i,
            _ => self.open_block(
                out,
                "thinking",
                json!({"type": "thinking", "thinking": "", "signature": ""}),
            ),
        };
        out.push(sse_event(Some("content_block_delta"), &json!({"type": "content_block_delta", "index": index, "delta": {"type": "signature_delta", "signature": sig}})));
        self.open_signed = true;
    }

    fn messages_chunk(&mut self, chunk: &Value) -> Vec<Bytes> {
        let mut out = Vec::new();
        if !self.started {
            self.started = true;
            let mut usage = anthropic_usage(&self.usage);
            usage["output_tokens"] = json!(0);
            out.push(sse_event(
                Some("message_start"),
                &json!({"type": "message_start", "message": {"id": format!("msg_{}", self.id), "type": "message", "role": "assistant", "model": self.model,
                    "content": [], "stop_reason": null, "stop_sequence": null, "usage": usage}}),
            ));
        }
        let (parts, finish, _) = candidate(chunk);
        for p in parts {
            let sig = p["thoughtSignature"].as_str().filter(|s| !s.is_empty());
            if p["thought"] == true {
                let index = match self.open {
                    Some((i, "thinking")) if !self.open_signed => i,
                    _ => self.open_block(
                        &mut out,
                        "thinking",
                        json!({"type": "thinking", "thinking": "", "signature": ""}),
                    ),
                };
                if let Some(t) = p["text"].as_str().filter(|t| !t.is_empty()) {
                    out.push(sse_event(Some("content_block_delta"), &json!({"type": "content_block_delta", "index": index, "delta": {"type": "thinking_delta", "thinking": t}})));
                }
                if let Some(s) = sig {
                    self.signature(&mut out, s);
                }
                continue;
            }
            if let Some(s) = sig {
                self.signature(&mut out, s);
            }
            if let Some(fc) = p.get("functionCall") {
                self.saw_tools = true;
                let id = fc["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| new_call_id("toolu_"));
                let index = self.open_block(
                    &mut out,
                    "tool_use",
                    json!({"type": "tool_use", "id": id, "name": fc["name"], "input": {}}),
                );
                out.push(sse_event(
                    Some("content_block_delta"),
                    &json!({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": call_args(fc, &self.placeholders).to_string()}}),
                ));
                self.close_block(&mut out);
            } else if let Some(t) = p["text"].as_str().filter(|t| !t.is_empty()) {
                let index = match self.open {
                    Some((i, "text")) => i,
                    _ => self.open_block(&mut out, "text", json!({"type": "text", "text": ""})),
                };
                out.push(sse_event(Some("content_block_delta"), &json!({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": t}})));
            }
        }
        if finish.is_some() || chunk["promptFeedback"]["blockReason"].is_string() {
            self.close_block(&mut out);
            out.push(sse_event(
                Some("message_delta"),
                &json!({"type": "message_delta", "delta": {"stop_reason": anthropic_stop(finish, self.saw_tools), "stop_sequence": null}, "usage": anthropic_usage(&self.usage)}),
            ));
            out.push(sse_event(
                Some("message_stop"),
                &json!({"type": "message_stop"}),
            ));
            self.completed = true;
        }
        out
    }
}

/// Bounded size guard shared with callers that buffer upstream JSON.
pub const RESPONSE_MAX: usize = EVENT_MAX;
