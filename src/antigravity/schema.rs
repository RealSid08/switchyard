//! Tool schema cleaning for Claude models on Antigravity.
//!
//! Antigravity's `VALIDATED` tool calling for Claude accepts only a subset of JSON Schema. Rules
//! (behaviour modelled on CLIProxyAPI `internal/util/gemini_schema.go`, MIT):
//! - Local `$ref`s into `$defs`/`definitions` are inlined (bounded depth); unresolved ones become a
//!   description hint.
//! - `const` becomes a one-value `enum`; `allOf` is merged; `anyOf`/`oneOf` keep their first
//!   non-null branch; type arrays keep their first non-null type; a null branch sets `nullable`.
//! - Constraints the validator does not enforce (formats, bounds, patterns, enums, extra keys) are
//!   moved into the description as hints rather than silently dropped.
//! - Object schemas with no properties get a required `reason` string placeholder, which the
//!   response translator strips from the model's arguments again.
//!
//! Gemini-family models instead receive the client's schema unchanged in `parametersJsonSchema`.
use serde_json::{Map, Value, json};

/// Description of the placeholder property added to empty object schemas.
pub const PLACEHOLDER_DESCRIPTION: &str = "Brief explanation of why you are calling this tool";
const PLACEHOLDER: &str = "reason";
const MAX_DEPTH: usize = 32;

/// Constraints that are kept only as description hints.
const HINTED: &[&str] = &[
    "format",
    "pattern",
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minItems",
    "maxItems",
    "uniqueItems",
    "minProperties",
    "maxProperties",
    "default",
    "examples",
    "example",
];
/// Keywords removed outright (metadata or structure the validator rejects).
const DROPPED: &[&str] = &[
    "$schema",
    "$id",
    "$comment",
    "$anchor",
    "$defs",
    "definitions",
    "title",
    "deprecated",
    "readOnly",
    "writeOnly",
    "contentMediaType",
    "contentEncoding",
    "contentSchema",
    "if",
    "then",
    "else",
    "not",
    "dependentSchemas",
    "dependentRequired",
    "dependencies",
    "propertyNames",
    "patternProperties",
    "unevaluatedProperties",
    "unevaluatedItems",
    "additionalItems",
    "prefixItems",
    "contains",
    "discriminator",
    "xml",
    "externalDocs",
];

/// Result of cleaning one tool's parameter schema.
pub struct Cleaned {
    pub schema: Value,
    /// True when the placeholder property was added at the top level.
    pub placeholder: bool,
}

/// Cleans a tool parameter schema for Claude `VALIDATED` mode.
pub fn clean_for_claude(schema: &Value) -> Cleaned {
    let defs = collect_defs(schema);
    let mut out = clean(schema, &defs, 0);
    if !out.is_object() {
        out = json!({"type": "object"});
    }
    if out["type"].is_null() {
        out["type"] = json!("object");
    }
    let mut placeholder = false;
    if out["type"] == "object" && out["properties"].as_object().is_none_or(Map::is_empty) {
        out["properties"] =
            json!({PLACEHOLDER: {"type": "string", "description": PLACEHOLDER_DESCRIPTION}});
        out["required"] = json!([PLACEHOLDER]);
        placeholder = true;
    }
    Cleaned {
        schema: out,
        placeholder,
    }
}

/// Removes the placeholder argument from a tool call whose schema received one.
pub fn strip_placeholder(args: &mut Value) {
    if let Some(o) = args.as_object_mut() {
        o.remove(PLACEHOLDER);
    }
}

fn collect_defs(schema: &Value) -> Map<String, Value> {
    let mut defs = Map::new();
    for key in ["$defs", "definitions"] {
        if let Some(d) = schema[key].as_object() {
            for (k, v) in d {
                defs.insert(format!("#/{key}/{k}"), v.clone());
            }
        }
    }
    defs
}

fn hint(description: &mut Vec<String>, key: &str, value: &Value) {
    let rendered = match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if rendered.len() <= 200 {
        description.push(format!("{key}: {rendered}"));
    }
}

fn clean(schema: &Value, defs: &Map<String, Value>, depth: usize) -> Value {
    let Some(src) = schema.as_object() else {
        // `true`/`false` schemas and malformed values become an unconstrained string.
        return json!({"type": "string"});
    };
    if depth > MAX_DEPTH {
        return json!({"type": "object"});
    }
    // Inline a local reference; keep sibling description.
    if let Some(r) = src.get("$ref").and_then(Value::as_str) {
        let mut target = match defs.get(r) {
            Some(t) => clean(t, defs, depth + 1),
            None => json!({"type": "object", "description": format!("See {r}")}),
        };
        if let Some(d) = src.get("description").and_then(Value::as_str) {
            target["description"] = json!(d);
        }
        return target;
    }
    let mut node = src.clone();
    let mut notes: Vec<String> = Vec::new();
    let mut nullable = false;

    // allOf: merge properties and required into this node.
    if let Some(all) = node.remove("allOf").and_then(|v| v.as_array().cloned()) {
        for part in all {
            let part = clean(&part, defs, depth + 1);
            if let Some(props) = part["properties"].as_object() {
                let entry = node.entry("properties").or_insert_with(|| json!({}));
                if let Some(target) = entry.as_object_mut() {
                    for (k, v) in props {
                        target.insert(k.clone(), v.clone());
                    }
                }
            }
            if let Some(req) = part["required"].as_array() {
                let entry = node.entry("required").or_insert_with(|| json!([]));
                if let Some(target) = entry.as_array_mut() {
                    target.extend(req.iter().cloned());
                }
            }
            if node.get("type").is_none()
                && let Some(t) = part.get("type")
            {
                node.insert("type".into(), t.clone());
            }
        }
    }
    // anyOf/oneOf: first non-null branch wins; others noted.
    for key in ["anyOf", "oneOf"] {
        if let Some(branches) = node.remove(key).and_then(|v| v.as_array().cloned()) {
            nullable |= branches.iter().any(|b| b["type"] == "null");
            let chosen = branches.iter().find(|b| b["type"] != "null");
            if let Some(chosen) = chosen {
                let chosen = clean(chosen, defs, depth + 1);
                if branches.iter().filter(|b| b["type"] != "null").count() > 1 {
                    notes.push(format!("{key}: also accepts other shapes"));
                }
                if let Some(o) = chosen.as_object() {
                    for (k, v) in o {
                        node.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
            }
        }
    }
    // Type arrays: first non-null type.
    if let Some(types) = node.get("type").and_then(Value::as_array).cloned() {
        nullable |= types.iter().any(|t| t == "null");
        let first = types
            .iter()
            .find(|t| *t != "null")
            .cloned()
            .unwrap_or(json!("string"));
        node.insert("type".into(), first);
    }
    if node.get("type").and_then(Value::as_str) == Some("null") {
        node.insert("type".into(), json!("string"));
        nullable = true;
    }
    // const -> enum, then enums become hints (the validator does not enforce them).
    if let Some(c) = node.remove("const") {
        node.insert("enum".into(), json!([c]));
    }
    if let Some(e) = node.remove("enum") {
        hint(&mut notes, "allowed values", &e);
    }
    for key in HINTED {
        if let Some(v) = node.remove(*key) {
            hint(&mut notes, key, &v);
        }
    }
    // additionalProperties: only `false` is meaningful, as a hint.
    if let Some(ap) = node.remove("additionalProperties")
        && ap == false
    {
        notes.push("no additional properties".into());
    }
    for key in DROPPED {
        node.remove(*key);
    }
    node.retain(|k, _| !k.starts_with("x-") && k != "$ref");
    // Recurse into properties and items.
    if let Some(props) = node.get("properties").and_then(Value::as_object).cloned() {
        let cleaned: Map<String, Value> = props
            .iter()
            .map(|(k, v)| (k.clone(), clean(v, defs, depth + 1)))
            .collect();
        node.insert("properties".into(), Value::Object(cleaned));
        if node.get("type").is_none() {
            node.insert("type".into(), json!("object"));
        }
    }
    if let Some(items) = node.get("items").cloned() {
        let items = if items.is_array() {
            items
                .as_array()
                .and_then(|a| a.first())
                .cloned()
                .unwrap_or(json!({"type": "string"}))
        } else {
            items
        };
        node.insert("items".into(), clean(&items, defs, depth + 1));
        node.insert("type".into(), json!("array"));
    } else if node.get("type").and_then(Value::as_str) == Some("array") {
        node.insert("items".into(), json!({"type": "string"}));
    }
    // `required` may only name declared properties.
    if let Some(req) = node.get("required").and_then(Value::as_array).cloned() {
        let declared = node.get("properties").and_then(Value::as_object);
        let mut seen = std::collections::BTreeSet::new();
        let kept: Vec<Value> = req
            .into_iter()
            .filter(|r| {
                r.as_str().is_some_and(|name| {
                    declared.is_some_and(|d| d.contains_key(name)) && seen.insert(name.to_string())
                })
            })
            .collect();
        if kept.is_empty() {
            node.remove("required");
        } else {
            node.insert("required".into(), Value::Array(kept));
        }
    }
    if nullable {
        node.insert("nullable".into(), json!(true));
    }
    if !notes.is_empty() {
        let base = node
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let joined = notes.join("; ");
        let text = if base.is_empty() {
            format!("({joined})")
        } else {
            format!("{base} ({joined})")
        };
        node.insert("description".into(), json!(text));
    }
    Value::Object(node)
}
