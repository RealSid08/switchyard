//! Antigravity protocol layer: request planning, response translation (JSON and SSE), thought
//! signatures, schema cleaning, SSE unwrapping and quota parsing. Pure functions, no network.

use serde_json::{Value, json};
use std::collections::BTreeSet;
use switchyard::antigravity::{
    self, SseUnwrapper,
    translate::{self, Protocol, SKIP_SIGNATURE, StreamTranslator},
};

const GEMINI: &str = "gemini-3-flash";
const CLAUDE: &str = "claude-sonnet-4-6";

fn plan_chat(body: Value, model: &str) -> Value {
    translate::plan_chat(&body, model, "proj-1")
        .unwrap_or_else(|e| panic!("{}", e.message))
        .body
}
fn bad_request(r: Result<translate::Plan, switchyard::app::ApiError>) -> String {
    match r {
        Ok(_) => panic!("expected a 400"),
        Err(e) => {
            assert_eq!(e.status.as_u16(), 400, "{}", e.message);
            e.message
        }
    }
}
fn sse_text(chunks: &[axum::body::Bytes]) -> String {
    chunks
        .iter()
        .map(|b| String::from_utf8(b.to_vec()).unwrap())
        .collect()
}
fn data_events(text: &str) -> Vec<String> {
    text.split("\n\n")
        .filter_map(|b| {
            b.lines()
                .find_map(|l| l.strip_prefix("data: "))
                .map(String::from)
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------------------------

#[test]
fn envelope_wraps_a_gemini_request_for_the_project() {
    let body = json!({"model":"client-alias","stream":true,"contents":[{"role":"user","parts":[{"text":"hello there"}]}],
        "generationConfig":{"temperature":0.2},"safetySettings":[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"BLOCK_NONE"}]});
    let plan = translate::plan(Protocol::Gemini, &body, GEMINI, "proj-1").unwrap();
    let env = plan.body;
    assert_eq!(env["model"], GEMINI);
    assert_eq!(env["project"], "proj-1");
    assert_eq!(env["userAgent"], "antigravity");
    assert_eq!(env["requestType"], "agent");
    assert!(env["requestId"].as_str().unwrap().starts_with("agent-"));
    let req = &env["request"];
    assert!(
        req.get("model").is_none() && req.get("stream").is_none(),
        "routing fields are not forwarded"
    );
    assert_eq!(req["generationConfig"]["temperature"], 0.2);
    assert_eq!(
        req["safetySettings"][0]["threshold"], "BLOCK_NONE",
        "client fields are forwarded, not dropped"
    );
    // The session id is stable for one conversation and never contains the text.
    let again = translate::plan(Protocol::Gemini, &body, GEMINI, "proj-1")
        .unwrap()
        .body;
    assert_eq!(req["sessionId"], again["request"]["sessionId"]);
    assert!(!req["sessionId"].as_str().unwrap().contains("hello"));
    let other = translate::plan(
        Protocol::Gemini,
        &json!({"contents":[{"role":"user","parts":[{"text":"different"}]}]}),
        GEMINI,
        "",
    )
    .unwrap()
    .body;
    assert_ne!(other["request"]["sessionId"], req["sessionId"]);
    assert!(
        other.get("project").is_none(),
        "no project field when unknown"
    );
    // Image models use the image request type.
    let img = translate::plan(Protocol::Gemini, &body, "gemini-3.1-flash-image", "p")
        .unwrap()
        .body;
    assert_eq!(img["requestType"], "image_gen");
    assert!(
        antigravity::inference_url("https://x.example/", true)
            .ends_with("/v1internal:streamGenerateContent?alt=sse")
    );
    assert!(
        antigravity::inference_url("https://x.example", false)
            .ends_with("/v1internal:generateContent")
    );
    // cachedContent cannot be honoured.
    assert!(
        bad_request(translate::plan(
            Protocol::Gemini,
            &json!({"contents":[],"cachedContent":"c/1"}),
            GEMINI,
            "p"
        ))
        .contains("cachedContent")
    );
}

#[test]
fn gemini_passthrough_cleans_claude_tool_schemas_and_validates_calls() {
    let body = json!({"contents":[{"role":"user","parts":[{"text":"x"}]}],
        "tools":[{"functionDeclarations":[{"name":"noop","parametersJsonSchema":{"type":"object","properties":{}}},
                                          {"name":"get","parametersJsonSchema":{"$schema":"x","type":"object","properties":{"q":{"type":"string","format":"uri"}},"required":["q"]}}]}]});
    let plan = translate::plan(Protocol::Gemini, &body, CLAUDE, "p").unwrap();
    let decls = &plan.body["request"]["tools"][0]["functionDeclarations"];
    assert!(decls[0].get("parametersJsonSchema").is_none());
    assert_eq!(
        decls[0]["parameters"]["required"],
        json!(["reason"]),
        "empty object gets the placeholder"
    );
    assert_eq!(
        decls[1]["parameters"]["properties"]["q"]["description"],
        "(format: uri)"
    );
    assert!(decls[1]["parameters"].get("$schema").is_none());
    assert_eq!(plan.placeholder_tools, BTreeSet::from(["noop".to_string()]));
    assert_eq!(
        plan.body["request"]["toolConfig"]["functionCallingConfig"]["mode"],
        "VALIDATED"
    );
    // Gemini models keep the client's schema untouched.
    let plan = translate::plan(Protocol::Gemini, &body, GEMINI, "p").unwrap();
    assert_eq!(plan.body["request"]["tools"], body["tools"]);
    assert!(plan.body["request"].get("toolConfig").is_none());
}

// ---------------------------------------------------------------------------------------------
// Chat Completions -> Antigravity
// ---------------------------------------------------------------------------------------------

#[test]
fn chat_request_maps_messages_images_tools_and_results() {
    let body = json!({
        "model":"alias","stream":false,"user":"u-1","parallel_tool_calls":true,"stream_options":{"include_usage":true},
        "messages":[
            {"role":"system","content":"Be brief."},
            {"role":"developer","content":[{"type":"text","text":"Use tools."}]},
            {"role":"user","content":[{"type":"text","text":"What is in this image?"},{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo="}}]},
            {"role":"assistant","content":null,"tool_calls":[
                {"id":"call_a","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"cats\"}"}},
                {"id":"call_b","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"dogs\"}"}}]},
            {"role":"tool","tool_call_id":"call_a","content":"cats: 3"},
            {"role":"tool","tool_call_id":"call_b","content":[{"type":"text","text":"dogs: 2"},{"type":"image_url","image_url":{"url":"data:image/jpeg;base64,/9j/"}}]},
            {"role":"assistant","content":"There are 3 cats and 2 dogs."},
            {"role":"user","content":"Thanks"}
        ],
        "tools":[{"type":"function","function":{"name":"lookup","description":"Search","parameters":{"type":"object","properties":{"q":{"type":"string"}},"required":["q"]}}}],
        "tool_choice":{"type":"function","function":{"name":"lookup"}},
        "temperature":0.3,"top_p":0.9,"max_completion_tokens":512,"stop":"END","seed":7,"presence_penalty":0.1,"frequency_penalty":0.2,
        "response_format":{"type":"json_schema","json_schema":{"name":"r","schema":{"type":"object"}}}
    });
    let env = plan_chat(body.clone(), GEMINI);
    let r = &env["request"];
    assert_eq!(
        r["systemInstruction"]["parts"],
        json!([{"text":"Be brief."},{"text":"Use tools."}])
    );
    let c = r["contents"].as_array().unwrap();
    assert_eq!(c.len(), 5, "{c:#?}");
    assert_eq!(c[0]["role"], "user");
    assert_eq!(
        c[0]["parts"][1],
        json!({"inlineData":{"mimeType":"image/png","data":"iVBORw0KGgo="}})
    );
    assert_eq!(c[1]["role"], "model");
    assert_eq!(
        c[1]["parts"][0]["functionCall"],
        json!({"id":"call_a","name":"lookup","args":{"q":"cats"}})
    );
    assert_eq!(
        c[1]["parts"][0]["thoughtSignature"], SKIP_SIGNATURE,
        "Gemini calls without a known signature use the documented placeholder"
    );
    // Both tool results share one user turn, with names resolved from the calls.
    assert_eq!(c[2]["role"], "user");
    let frs: Vec<&Value> = c[2]["parts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| &p["functionResponse"])
        .collect();
    assert_eq!(
        frs[0],
        &json!({"id":"call_a","name":"lookup","response":{"result":"cats: 3"}})
    );
    assert_eq!(frs[1]["response"]["result"], "dogs: 2");
    assert_eq!(frs[1]["parts"][0]["inlineData"]["mimeType"], "image/jpeg");
    assert_eq!(
        c[3],
        json!({"role":"model","parts":[{"text":"There are 3 cats and 2 dogs."}]})
    );
    assert_eq!(
        r["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["required"],
        json!(["q"])
    );
    assert_eq!(
        r["toolConfig"]["functionCallingConfig"],
        json!({"mode":"ANY","allowedFunctionNames":["lookup"]})
    );
    let g = &r["generationConfig"];
    assert_eq!(
        (
            g["temperature"].clone(),
            g["topP"].clone(),
            g["maxOutputTokens"].clone()
        ),
        (json!(0.3), json!(0.9), json!(512))
    );
    assert_eq!(g["stopSequences"], json!(["END"]));
    assert_eq!(
        (
            g["seed"].clone(),
            g["presencePenalty"].clone(),
            g["frequencyPenalty"].clone()
        ),
        (json!(7), json!(0.1), json!(0.2))
    );
    assert_eq!(g["responseMimeType"], "application/json");
    assert_eq!(g["responseJsonSchema"], json!({"type":"object"}));

    // Claude models: no dummy signatures, cleaned schemas, VALIDATED mode.
    let mut auto = body.clone();
    auto["tool_choice"] = json!("auto");
    let env = plan_chat(auto, CLAUDE);
    let calls = &env["request"]["contents"][1]["parts"][0];
    assert!(
        calls.get("thoughtSignature").is_none(),
        "Antigravity rejects dummy signatures for Claude"
    );
    assert!(
        env["request"]["tools"][0]["functionDeclarations"][0]
            .get("parameters")
            .is_some()
    );
    assert_eq!(
        env["request"]["toolConfig"]["functionCallingConfig"]["mode"],
        "VALIDATED"
    );
}

#[test]
fn chat_tool_choice_and_reasoning_mappings() {
    let base = json!({"messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"t"}}]});
    for (choice, mode) in [("none", "NONE"), ("auto", "AUTO"), ("required", "ANY")] {
        let mut b = base.clone();
        b["tool_choice"] = json!(choice);
        assert_eq!(
            plan_chat(b, GEMINI)["request"]["toolConfig"]["functionCallingConfig"]["mode"],
            mode
        );
    }
    let mut b = base.clone();
    b["reasoning_effort"] = json!("medium");
    assert_eq!(
        plan_chat(b.clone(), GEMINI)["request"]["generationConfig"]["thinkingConfig"],
        json!({"includeThoughts":true,"thinkingLevel":"medium"})
    );
    assert_eq!(
        plan_chat(b.clone(), CLAUDE)["request"]["generationConfig"]["thinkingConfig"],
        json!({"includeThoughts":true,"thinkingBudget":8192})
    );
    assert_eq!(
        plan_chat(b.clone(), "gpt-oss-120b-medium")["request"]["generationConfig"]["thinkingConfig"]
            ["thinkingBudget"],
        8192
    );
    b["reasoning_effort"] = json!("none");
    assert!(plan_chat(b.clone(), GEMINI)["request"]["generationConfig"].is_null());
    // Claude: earlier tool calls without replayable thinking mean thinking stays off this turn.
    let history = json!({"reasoning_effort":"high","messages":[{"role":"user","content":"a"},
        {"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"t","arguments":"{}"}}]},
        {"role":"tool","tool_call_id":"c1","content":"ok"}],"tools":[{"type":"function","function":{"name":"t"}}]});
    assert!(
        plan_chat(history.clone(), CLAUDE)["request"]["generationConfig"]
            .get("thinkingConfig")
            .is_none()
    );
    assert!(
        plan_chat(history, GEMINI)["request"]["generationConfig"]["thinkingConfig"].is_object()
    );
    b["reasoning_effort"] = json!("extreme");
    assert!(bad_request(translate::plan_chat(&b, GEMINI, "p")).contains("reasoning_effort"));
}

#[test]
fn chat_fields_antigravity_cannot_honour_are_rejected_not_dropped() {
    let msg = json!([{"role":"user","content":"hi"}]);
    let cases = [
        (json!({"messages":msg,"n":2}), "n > 1"),
        (json!({"messages":msg,"logprobs":true}), "logprobs"),
        (json!({"messages":msg,"audio":{"voice":"x"}}), "audio"),
        (
            json!({"messages":msg,"modalities":["text","audio"]}),
            "modalities",
        ),
        (
            json!({"messages":msg,"web_search_options":{}}),
            "web_search_options",
        ),
        (json!({"messages":msg,"logit_bias":{"1":2}}), "logit_bias"),
        (
            json!({"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.com/cat.png"}}]}]}),
            "data: URLs",
        ),
        (
            json!({"messages":[{"role":"user","content":[{"type":"input_audio","input_audio":{}}]}]}),
            "input_audio",
        ),
        (
            json!({"messages":[{"role":"tool","tool_call_id":"nope","content":"x"}]}),
            "does not match",
        ),
        (
            json!({"messages":[{"role":"assistant","tool_calls":[{"id":"c","type":"function","function":{"name":"t","arguments":"{not json"}}]}]}),
            "not valid JSON",
        ),
        (
            json!({"messages":msg,"tools":[{"type":"web_search"}]}),
            "web_search",
        ),
        (
            json!({"messages":msg,"tools":[{"type":"function","function":{"name":"bad name!"}}]}),
            "not valid",
        ),
    ];
    for (body, needle) in cases {
        let message = bad_request(translate::plan_chat(&body, GEMINI, "p"));
        assert!(message.contains(needle), "{needle}: {message}");
    }
}

// ---------------------------------------------------------------------------------------------
// Antigravity -> Chat Completions
// ---------------------------------------------------------------------------------------------

fn gemini_reply(parts: Value, finish: &str) -> Value {
    json!({"responseId":"r1","candidates":[{"content":{"role":"model","parts":parts},"finishReason":finish}],
        "usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":20,"thoughtsTokenCount":5,"cachedContentTokenCount":40,"totalTokenCount":125}})
}

#[test]
fn chat_response_carries_text_reasoning_tools_usage_and_replays_signatures() {
    let upstream = json!({"response": gemini_reply(json!([
        {"thought":true,"text":"Considering."},
        {"text":"Let me look."},
        {"functionCall":{"id":"fc-sig-1","name":"lookup","args":{"q":"cats"}},"thoughtSignature":"SIG-ONE"},
        {"functionCall":{"name":"lookup","args":{"q":"dogs"}}}
    ]), "STOP")});
    let gemini = antigravity::unwrap(upstream);
    let chat = translate::response(Protocol::Chat, gemini, "alias", &BTreeSet::new());
    let m = &chat["choices"][0]["message"];
    assert_eq!(m["content"], "Let me look.");
    assert_eq!(
        m["reasoning_content"], "Considering.",
        "thoughts are separated from the answer"
    );
    let calls = m["tool_calls"].as_array().unwrap();
    assert_eq!(calls[0]["id"], "fc-sig-1");
    assert_eq!(calls[0]["function"]["arguments"], "{\"q\":\"cats\"}");
    assert!(
        calls[1]["id"].as_str().unwrap().starts_with("call_"),
        "missing ids are generated"
    );
    assert_eq!(chat["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(chat["model"], "alias");
    assert_eq!(
        chat["usage"],
        json!({"prompt_tokens":100,"completion_tokens":25,"total_tokens":125,
        "prompt_tokens_details":{"cached_tokens":40},"completion_tokens_details":{"reasoning_tokens":5}})
    );
    // The next request replays the real signature for that call (Chat cannot carry it).
    let next = plan_chat(
        json!({"messages":[{"role":"user","content":"q"},
        {"role":"assistant","tool_calls":[{"id":"fc-sig-1","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"cats\"}"}}]},
        {"role":"tool","tool_call_id":"fc-sig-1","content":"3"}]}),
        GEMINI,
    );
    assert_eq!(
        next["request"]["contents"][1]["parts"][0]["thoughtSignature"],
        "SIG-ONE"
    );
}

#[test]
fn chat_finish_reasons_and_blocked_prompts() {
    let cases = [
        ("STOP", "stop"),
        ("MAX_TOKENS", "length"),
        ("SAFETY", "content_filter"),
        ("RECITATION", "content_filter"),
        ("OTHER", "stop"),
    ];
    for (finish, expected) in cases {
        let chat = translate::response(
            Protocol::Chat,
            gemini_reply(json!([{"text":"x"}]), finish),
            "m",
            &BTreeSet::new(),
        );
        assert_eq!(chat["choices"][0]["finish_reason"], expected, "{finish}");
    }
    let blocked =
        json!({"promptFeedback":{"blockReason":"SAFETY"},"usageMetadata":{"promptTokenCount":3}});
    let chat = translate::response(Protocol::Chat, blocked, "m", &BTreeSet::new());
    assert_eq!(chat["choices"][0]["finish_reason"], "content_filter");
}

#[test]
fn chat_stream_emits_role_deltas_tools_finish_usage_and_done() {
    let mut t = StreamTranslator::new(Protocol::Chat, "alias", BTreeSet::new());
    let mut out = Vec::new();
    out.extend(t.push(&json!({"responseId":"rs","candidates":[{"content":{"parts":[{"thought":true,"text":"Hmm. "}]}}]})));
    out.extend(t.push(&json!({"candidates":[{"content":{"parts":[{"text":"Héllo 🦀"}]}}]})));
    assert!(!t.completed());
    out.extend(t.push(&json!({"candidates":[{"content":{"parts":[{"functionCall":{"id":"s1","name":"a","args":{"x":1}},"thoughtSignature":"S"}]}}]})));
    out.extend(t.push(&json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"b","args":{}}}]},"finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":4,"totalTokenCount":14}})));
    assert!(t.completed());
    let text = sse_text(&out);
    let events = data_events(&text);
    assert_eq!(events.last().unwrap(), "[DONE]");
    let chunks: Vec<Value> = events[..events.len() - 1]
        .iter()
        .map(|e| serde_json::from_str(e).unwrap())
        .collect();
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert!(
        chunks
            .iter()
            .all(|c| c["id"] == "chatcmpl-rs" && c["model"] == "alias")
    );
    assert_eq!(
        chunks[1]["choices"][0]["delta"]["reasoning_content"],
        "Hmm. "
    );
    assert_eq!(chunks[2]["choices"][0]["delta"]["content"], "Héllo 🦀");
    assert_eq!(
        chunks[3]["choices"][0]["delta"]["tool_calls"][0]["index"],
        0
    );
    assert_eq!(
        chunks[4]["choices"][0]["delta"]["tool_calls"][0]["index"],
        1
    );
    assert_eq!(chunks[5]["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(chunks[6]["usage"]["prompt_tokens"], 10);
    assert_eq!(t.usage()["totalTokenCount"], 14);

    // A stream that never finishes never claims completion.
    let mut cut = StreamTranslator::new(Protocol::Chat, "m", BTreeSet::new());
    let text =
        sse_text(&cut.push(&json!({"candidates":[{"content":{"parts":[{"text":"partial"}]}}]})));
    assert!(!cut.completed() && !text.contains("[DONE]"));
}

// ---------------------------------------------------------------------------------------------
// Anthropic Messages <-> Antigravity
// ---------------------------------------------------------------------------------------------

#[test]
fn messages_request_places_thinking_signatures_per_model_family() {
    let body = json!({
    "system":[{"type":"text","text":"Sys","cache_control":{"type":"ephemeral"}}],
    "max_tokens":1024,"temperature":0.5,"top_k":40,"stop_sequences":["STOP"],
    "thinking":{"type":"enabled","budget_tokens":2048},
    "tools":[{"name":"read","description":"Read a file","input_schema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}],
    "tool_choice":{"type":"tool","name":"read"},
    "messages":[
        {"role":"user","content":[{"type":"text","text":"Read it"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}}]},
        {"role":"assistant","content":[
            {"type":"thinking","thinking":"I should read.","signature":"SIGNED"},
            {"type":"tool_use","id":"tu1","name":"read","input":{"path":"a.txt"}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"tu1","content":[{"type":"text","text":"file body"}]},{"type":"text","text":"Summarise"}]}
    ]});
    // Claude: the signature stays on the thought part.
    let env = translate::plan_messages(&body, CLAUDE, "p").unwrap().body;
    let r = &env["request"];
    assert_eq!(r["systemInstruction"]["parts"], json!([{"text":"Sys"}]));
    let model_parts = &r["contents"][1]["parts"];
    assert_eq!(
        model_parts[0],
        json!({"thought":true,"text":"I should read.","thoughtSignature":"SIGNED"})
    );
    assert_eq!(
        model_parts[1]["functionCall"],
        json!({"id":"tu1","name":"read","args":{"path":"a.txt"}})
    );
    assert!(model_parts[1].get("thoughtSignature").is_none());
    assert_eq!(
        r["contents"][2]["parts"][0]["functionResponse"],
        json!({"id":"tu1","name":"read","response":{"result":"file body"}})
    );
    assert_eq!(r["contents"][2]["parts"][1], json!({"text":"Summarise"}));
    assert_eq!(
        r["generationConfig"]["thinkingConfig"],
        json!({"includeThoughts":true,"thinkingBudget":2048})
    );
    assert_eq!(r["generationConfig"]["topK"], 40);
    assert_eq!(r["generationConfig"]["stopSequences"], json!(["STOP"]));
    assert_eq!(
        r["toolConfig"]["functionCallingConfig"],
        json!({"mode":"ANY","allowedFunctionNames":["read"]})
    );

    // Gemini: the thought text stays, its signature moves to the following function call.
    let env = translate::plan_messages(&body, GEMINI, "p").unwrap().body;
    let model_parts = &env["request"]["contents"][1]["parts"];
    assert_eq!(
        model_parts[0],
        json!({"thought":true,"text":"I should read."})
    );
    assert_eq!(model_parts[1]["thoughtSignature"], "SIGNED");
}

#[test]
fn messages_unsigned_claude_thinking_disables_thinking_and_unsupported_blocks_fail() {
    let body = json!({"max_tokens":10,"thinking":{"type":"enabled","budget_tokens":1024},"messages":[
        {"role":"user","content":"a"},
        {"role":"assistant","content":[{"type":"thinking","thinking":"unsigned","signature":""},{"type":"text","text":"b"}]},
        {"role":"user","content":"c"}]});
    let env = translate::plan_messages(&body, CLAUDE, "p").unwrap().body;
    assert_eq!(
        env["request"]["contents"][1]["parts"],
        json!([{"text":"b"}]),
        "unsigned thinking cannot be replayed"
    );
    assert!(
        env["request"]["generationConfig"]
            .get("thinkingConfig")
            .is_none()
    );

    let error_result = json!({"max_tokens":10,"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"t","name":"n","input":{}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"t","is_error":true,"content":"boom"}]}]});
    let env = translate::plan_messages(&error_result, GEMINI, "p")
        .unwrap()
        .body;
    assert_eq!(
        env["request"]["contents"][1]["parts"][0]["functionResponse"]["response"],
        json!({"error":"boom"})
    );

    let cases = [
        (
            json!({"messages":[{"role":"assistant","content":[{"type":"redacted_thinking","data":"x"}]}]}),
            "redacted_thinking",
        ),
        (
            json!({"messages":[{"role":"user","content":[{"type":"image","source":{"type":"url","url":"https://x/y.png"}}]}]}),
            "base64",
        ),
        (
            json!({"messages":[{"role":"user","content":"x"}],"tools":[{"type":"web_search_20250305","name":"web_search"}]}),
            "web_search_20250305",
        ),
        (
            json!({"messages":[{"role":"user","content":[{"type":"server_tool_use"}]}]}),
            "server_tool_use",
        ),
        (
            json!({"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"none","content":"x"}]}]}),
            "does not match",
        ),
    ];
    for (b, needle) in cases {
        let message = bad_request(translate::plan_messages(&b, CLAUDE, "p"));
        assert!(message.contains(needle), "{needle}: {message}");
    }
}

#[test]
fn messages_response_builds_thinking_text_and_tool_blocks() {
    let mut placeholders = BTreeSet::new();
    placeholders.insert("noop".to_string());
    // Claude-style: signature on the thought part.
    let claude = gemini_reply(
        json!([
            {"thought":true,"text":"Plan","thoughtSignature":"CLAUDE-SIG"},
            {"text":"Answer"},
            {"functionCall":{"id":"tu9","name":"noop","args":{"reason":"placeholder","keep":1}}}
        ]),
        "STOP",
    );
    let m = translate::response(Protocol::Messages, claude, "alias", &placeholders);
    assert_eq!(
        m["content"][0],
        json!({"type":"thinking","thinking":"Plan","signature":"CLAUDE-SIG"})
    );
    assert_eq!(m["content"][1], json!({"type":"text","text":"Answer"}));
    assert_eq!(
        m["content"][2],
        json!({"type":"tool_use","id":"tu9","name":"noop","input":{"keep":1}}),
        "placeholder argument is stripped"
    );
    assert_eq!(m["stop_reason"], "tool_use");
    assert_eq!(
        m["usage"],
        json!({"input_tokens":60,"cache_read_input_tokens":40,"output_tokens":25}),
        "Anthropic input excludes cache reads"
    );
    // Gemini-style: signature on the call, attached to the preceding thinking block.
    let gemini = gemini_reply(
        json!([
            {"thought":true,"text":"Think"},
            {"functionCall":{"name":"x","args":{}},"thoughtSignature":"GEM-SIG"}
        ]),
        "STOP",
    );
    let m = translate::response(Protocol::Messages, gemini, "alias", &BTreeSet::new());
    assert_eq!(
        m["content"][0],
        json!({"type":"thinking","thinking":"Think","signature":"GEM-SIG"})
    );
    assert!(
        m["content"][1]["id"]
            .as_str()
            .unwrap()
            .starts_with("toolu_")
    );
    // A signature without visible thought becomes an empty signed thinking block (replayable).
    let bare = gemini_reply(
        json!([{"text":"Hi","thoughtSignature":"ONLY-SIG"}]),
        "MAX_TOKENS",
    );
    let m = translate::response(Protocol::Messages, bare, "alias", &BTreeSet::new());
    assert_eq!(
        m["content"][0],
        json!({"type":"thinking","thinking":"","signature":"ONLY-SIG"})
    );
    assert_eq!(m["stop_reason"], "max_tokens");
}

#[test]
fn messages_stream_event_sequence() {
    let mut t = StreamTranslator::new(Protocol::Messages, "alias", BTreeSet::new());
    let mut out = Vec::new();
    out.extend(t.push(&json!({"responseId":"m1","candidates":[{"content":{"parts":[{"thought":true,"text":"a"}]}}],"usageMetadata":{"promptTokenCount":9}})));
    out.extend(t.push(&json!({"candidates":[{"content":{"parts":[{"thought":true,"text":"b","thoughtSignature":"SIG"}]}}]})));
    out.extend(t.push(&json!({"candidates":[{"content":{"parts":[{"text":"Hi"}]}}]})));
    out.extend(t.push(&json!({"candidates":[{"content":{"parts":[{"text":" there"},{"functionCall":{"id":"t1","name":"f","args":{"k":"v"}}}]},"finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":9,"candidatesTokenCount":3}})));
    assert!(t.completed());
    let text = sse_text(&out);
    let names: Vec<&str> = text
        .split("\n\n")
        .filter_map(|b| b.strip_prefix("event: "))
        .map(|b| b.lines().next().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );
    let events: Vec<Value> = data_events(&text)
        .iter()
        .map(|e| serde_json::from_str(e).unwrap())
        .collect();
    assert_eq!(events[0]["message"]["id"], "msg_m1");
    assert_eq!(
        events[4]["delta"],
        json!({"type":"signature_delta","signature":"SIG"})
    );
    assert_eq!(
        events[11]["delta"],
        json!({"type":"input_json_delta","partial_json":"{\"k\":\"v\"}"})
    );
    assert_eq!(events[13]["delta"]["stop_reason"], "tool_use");
    assert_eq!(events[13]["usage"]["output_tokens"], 3);
}

// ---------------------------------------------------------------------------------------------
// SSE unwrapping, schema cleaning, quota, client discovery
// ---------------------------------------------------------------------------------------------

#[test]
fn sse_unwrapper_handles_split_crlf_and_multiline_events() {
    let body = "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Grüße\"}]}}]},\"traceId\":\"t\"}\r\n\r\n: comment\r\n\r\ndata: {\"response\":\r\ndata: {\"candidates\":[{\"finishReason\":\"STOP\"}]}}\r\n\r\n";
    let bytes = body.as_bytes();
    let mut u = SseUnwrapper::new();
    let mut events = Vec::new();
    for chunk in bytes.chunks(3) {
        events.extend(u.push(chunk).unwrap());
    }
    events.extend(u.finish());
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0]["candidates"][0]["content"]["parts"][0]["text"],
        "Grüße"
    );
    assert!(
        events[0].get("traceId").is_none(),
        "the envelope is removed"
    );
    assert!(antigravity::is_terminal(&events[1]));
    assert!(!antigravity::is_terminal(&events[0]));
    // A final event without a trailing blank line is still delivered.
    let mut u = SseUnwrapper::new();
    assert!(
        u.push(b"data: {\"response\":{\"x\":1}}")
            .unwrap()
            .is_empty()
    );
    assert_eq!(u.finish(), vec![json!({"x":1})]);
    // Oversized events fail instead of truncating.
    let mut u = SseUnwrapper::new();
    let big = vec![b'a'; antigravity::EVENT_MAX + 1];
    let mut line = b"data: ".to_vec();
    line.extend_from_slice(&big);
    assert!(u.push(&line).is_err());
}

#[test]
fn claude_schema_cleaner_rules() {
    use switchyard::antigravity::schema::clean_for_claude;
    let schema = json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema","title":"T",
        "type":"object","additionalProperties":false,
        "$defs":{"Point":{"type":"object","properties":{"x":{"type":"number","minimum":0}},"required":["x"]}},
        "properties":{
            "p":{"$ref":"#/$defs/Point","description":"A point"},
            "mode":{"const":"fast"},
            "tags":{"type":["array","null"],"items":{"type":"string","maxLength":5}},
            "either":{"anyOf":[{"type":"null"},{"type":"integer","enum":[1,2]}]},
            "both":{"allOf":[{"properties":{"a":{"type":"string"}},"required":["a"]},{"properties":{"b":{"type":"string"}}}]}
        },
        "required":["p","missing","p"]
    });
    let cleaned = clean_for_claude(&schema);
    let s = cleaned.schema;
    assert!(!cleaned.placeholder);
    assert!(
        s.get("$schema").is_none()
            && s.get("title").is_none()
            && s.get("$defs").is_none()
            && s.get("additionalProperties").is_none()
    );
    assert_eq!(s["description"], "(no additional properties)");
    assert_eq!(
        s["required"],
        json!(["p"]),
        "required keeps declared, unique names only"
    );
    let p = &s["properties"]["p"];
    assert_eq!(p["description"], "A point");
    assert_eq!(
        p["properties"]["x"],
        json!({"type":"number","description":"(minimum: 0)"})
    );
    assert_eq!(
        s["properties"]["mode"]["description"],
        "(allowed values: [\"fast\"])"
    );
    assert_eq!(s["properties"]["tags"]["type"], "array");
    assert_eq!(s["properties"]["tags"]["nullable"], true);
    assert_eq!(
        s["properties"]["tags"]["items"]["description"],
        "(maxLength: 5)"
    );
    assert_eq!(s["properties"]["either"]["type"], "integer");
    assert_eq!(s["properties"]["either"]["nullable"], true);
    assert_eq!(s["properties"]["both"]["required"], json!(["a"]));
    assert!(s["properties"]["both"]["properties"]["b"].is_object());
    let empty = clean_for_claude(&json!({"type":"object"}));
    assert!(empty.placeholder);
    assert_eq!(empty.schema["properties"]["reason"]["type"], "string");
    // Pathological depth is bounded.
    let mut deep = json!({"type":"string"});
    for _ in 0..200 {
        deep = json!({"type":"object","properties":{"n":deep}});
    }
    let _ = clean_for_claude(&deep);
}

#[test]
fn quota_parsing_is_per_model_and_bounded() {
    let models = json!({"models":{
        "gemini-3-flash":{"displayName":"Gemini 3 Flash","quotaInfo":{"remainingFraction":0.25,"resetTime":"2026-10-03T05:00:00Z"}},
        "claude-sonnet-4-6":{"displayName":"Claude Sonnet 4.6","quotaInfo":{"remainingFraction":1.7}},
        "tab_flash_lite_preview":{"quotaInfo":{"remainingFraction":0.1}},
        "no-quota":{"displayName":"x"},
        "bad id":{"quotaInfo":{"remainingFraction":0.5}}
    }});
    let q = antigravity::parse_quota(&models);
    assert_eq!(q.len(), 2);
    assert_eq!(q[0].model, "claude-sonnet-4-6");
    assert_eq!(q[0].remaining_fraction, Some(1.0), "clamped to 0..1");
    assert_eq!(q[0].reset_at, None);
    assert_eq!(q[1].label, "Gemini 3 Flash");
    assert_eq!(q[1].remaining_fraction, Some(0.25));
    assert_eq!(
        q[1].reset_at,
        Some(
            chrono::DateTime::parse_from_rfc3339("2026-10-03T05:00:00Z")
                .unwrap()
                .timestamp()
        )
    );
    let buckets = json!({"buckets":[
        {"modelId":"gemini-3-flash","remainingFraction":0.8,"resetTime":"2026-10-03T05:00:00Z"},
        {"modelId":"gemini-3-flash","remainingFraction":0.3,"resetTime":"2026-10-04T05:00:00Z"},
        {"modelId":"","remainingFraction":0.1}]});
    let b = antigravity::parse_quota_buckets(&buckets);
    assert_eq!(b.len(), 1);
    assert_eq!(
        b[0].remaining_fraction,
        Some(0.3),
        "the most constrained bucket wins"
    );
    let listed = antigravity::parse_models(&models);
    assert_eq!(
        listed.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        ["claude-sonnet-4-6", "gemini-3-flash", "no-quota"]
    );
    assert_eq!(
        antigravity::retry_after(
            &json!({"error":{"details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"12.2s"}]}})
        ),
        Some(13)
    );
    assert_eq!(antigravity::retry_after(&json!({"error":{}})), None);
}

#[test]
fn oauth_client_is_extracted_from_an_installed_bundle_only() {
    let bundle = b"...vendor code... vs/platform/cloudCode/common/oauthClient.js ... const CLIENT_ID=\"123456789-abcDEF_ghi.apps.googleusercontent.com\",CLIENT_SECRET=\"GOCSPX-FakeSecretForTests_12\" ...";
    let client = antigravity::client_from_bundle(bundle).unwrap();
    assert_eq!(client.id, "123456789-abcDEF_ghi.apps.googleusercontent.com");
    assert_eq!(client.secret, "GOCSPX-FakeSecretForTests_12");
    assert!(
        !format!("{client:?}").contains("GOCSPX"),
        "Debug output redacts the secret"
    );
    assert!(antigravity::client_from_bundle(b"no client here").is_none());
    assert!(
        antigravity::client_from_bundle(b"x.apps.googleusercontent.com GOCSPX-short").is_none()
    );
}
