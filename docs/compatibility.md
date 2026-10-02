# Protocol compatibility

Switchyard forwards each provider's native protocol. The one translation it performs is Chat Completions for Codex accounts. A route chooses accounts; it never converts one wire protocol into another.

## Matrix

| Connection kind | Credentials | Client endpoints | Streaming | Responses WebSocket |
| --- | --- | --- | --- | --- |
| `openai` (OpenAI-compatible) | API key | `/v1/responses`, `/v1/chat/completions` | Native SSE passthrough | When enabled on the connection and supported upstream |
| `codex` (ChatGPT subscription) | Browser sign-in, Codex CLI import, CLIProxyAPI import | `/v1/responses`, `/v1/chat/completions` (translated) | Native Responses SSE; Chat translated | Yes |
| `anthropic` | API key, browser sign-in, Claude Code import, CLIProxyAPI import | `/v1/messages`, `/v1/messages/count_tokens` | Native SSE passthrough | No |
| `gemini` (Gemini API) | API key | `/v1beta/models/{model}:generateContent`, `:streamGenerateContent` | Native SSE passthrough | No |

`GET /v1/models` lists every model on enabled connections plus every route alias.

Clients authenticate with `Authorization: Bearer <key>`, `x-api-key: <key>` (Anthropic SDKs) or `x-goog-api-key: <key>` (Google SDKs). The client's key is never sent upstream; Switchyard authenticates with the connection's own credential.

`POST /v1/messages/count_tokens` forwards Anthropic's token counting to `{base}/messages/count_tokens` with the same account selection, failover and redaction as Messages. It is not recorded in request history and does not create cooldowns.

A request for a model that only exists on an incompatible connection kind returns 400 and names the right endpoint. Mixed-kind routes use only the targets compatible with the endpoint called.

## Requests

- **Model names.** The model in the request is replaced with the target model of the selected route target, so aliases work everywhere.
- **Headers.** Only `anthropic-version`, `anthropic-beta`, `openai-beta` and `idempotency-key` are forwarded, once each. Claude accounts that use OAuth tokens (browser sign-in, Claude Code and CLIProxyAPI imports) automatically add the OAuth beta, merged with any betas the client sent.
- **Gemini.** The model moves into the URL path (URL-encoded); the body is forwarded without the routing fields. Streaming adds `?alt=sse`.
- **Codex.** Payloads are shaped for the ChatGPT Codex backend: `store: false`, `stream: true`, default `instructions` when none or empty, a plain string `input` becomes one user message, and `max_output_tokens`, `max_tokens`, `temperature`, `top_p` and `stream_options` are removed because that backend rejects them.
- **Body size.** Inference requests up to 64 MiB; WebSocket messages up to 64 MiB.

## Chat Completions on Codex

Requests are converted to Responses:

| Chat Completions | Responses |
| --- | --- |
| `system` and `developer` messages (string or text parts) | `instructions`, joined with newlines |
| `user` text and `image_url` parts | `input_text` and `input_image` (with `detail`) |
| `assistant` content and `tool_calls` | `output_text` content and `function_call` items |
| `tool` messages | `function_call_output` items |
| `tools` with `function` | flat function tools, `strict` preserved |
| `tool_choice` (string or function) | `tool_choice` |
| `response_format` (`json_schema`, `json_object`) | `text.format` |
| `reasoning_effort` | `reasoning.effort` |
| `parallel_tool_calls` | unchanged |

Responses are converted back:

- Streaming: the first chunk carries `role: "assistant"`; every chunk uses the upstream response id; text, refusal and tool-call argument deltas keep stable tool indices; the final chunk has `finish_reason` `stop`, `tool_calls` or `length` (for `response.incomplete`), Chat-style `usage` (`prompt_tokens`, `completion_tokens`, `total_tokens`), then `data: [DONE]`.
- Non-streaming: one `chat.completion` with the same finish reasons and usage.
- Reasoning summaries, reasoning text and encrypted reasoning are not included in Chat output.
- A provider error event becomes a final chunk with an `error` object, followed by `[DONE]`.

Chat Completions on `openai` connections is passed through: streams byte for byte, non-streaming JSON with its content unchanged.

## Responses and errors

- Streams are forwarded as they arrive. See [architecture](architecture.md#stream-integrity) for how incomplete streams are detected and reported.
- Codex streams are parsed as SSE even when the backend omits `Content-Type`. Empty terminal `response.output` is populated from completed output items, preserving tool calls and SDK final-response helpers in SSE and WebSockets. Other events are unchanged.
- Non-streaming responses, SSE events and Codex completed-output accumulation are limited to 16 MiB. An oversized Codex terminal response fails explicitly with partial output instead of a misleading empty success.
- Provider errors keep their status and the fields `type`, `code`, `param`, `status`, `resets_at`, `resets_in_seconds` and `plan_type` (for example `context_length_exceeded`, `usage_limit_reached`, `rate_limit_error`). The message is prefixed with `Provider rejected the request:` and any credential it echoes is replaced with `[redacted]`. `/v1/messages`, `/v1/messages/count_tokens` errors use Anthropic's `{"type":"error","error":{...}}` envelope. `Retry-After` is passed through, or derived from the provider's reset hint for 429s.
- Gateway errors use `{"error":{"type":"gateway_error","message":...,"retry_after_seconds":...}}`.

## Multiple accounts

- Before any output, a refused connection, 401, 403, 429, 502, 503, 504 or failed token renewal moves the request to the next account and cools the failed one. For OAuth-based accounts a 401 first gets one token renewal (or adoption of a newer imported token) and one retry on the same account. An uncertain failure after the request was sent (such as a timeout) is returned, not replayed.
- Conversations stay on their account. Responses ids from completed responses (HTTP, SSE and WebSocket) are remembered for one hour (4,096 most recent), including across restarts. A follow-up whose account is unavailable, or an unknown id in a multi-account pool, returns 409 rather than guessing.
- WebSocket handshakes fail over between accounts before the first inference frame. After that, a session never changes account.

## WebSocket sessions

The first frame must be `response.create`, with fields either top-level or nested under `response`. Error frames have the shape `{"type":"error","error":{...}}`. Changing the model mid-session returns an error frame and keeps the session open; open a new socket for another model. An upstream close closes the client socket, and an upstream failure sends an `upstream_interrupted` error frame first. Request history and transport counters record one entry per session, not per turn.

## Accounts and models

Imported and signed-in accounts start with a suggested model list (for Codex, based on the plan in the token). These are editable starting points, not an availability guarantee. To see what an account actually offers, list the provider's catalog (`GET /api/connections/{id}/models`) and save the identifiers you want; listing never changes the configured models by itself.

| Connection kind | Catalog request | Identifier | Display name |
| --- | --- | --- | --- |
| `openai` | `GET {base}/models` | `data[].id` | `display_name` if present, else `id` |
| `anthropic` | `GET {base}/models?limit=1000`, then `&after_id=` | `data[].id` | `display_name` |
| `codex` | `GET {base}/models?client_version=...` | `models[].slug` | `display_name` |
| `gemini` | `GET {base}/models?pageSize=1000`, then `&pageToken=` | `models[].name` without `models/` | `displayName` |

Anthropic (`limit=1000` with `after_id`) and Gemini (`pageSize=1000` with `pageToken`) catalogs are paged through, up to 5 pages, 2 MiB and 1,000 models within 20 seconds. When a catalog is cut short by those bounds, a repeated cursor or a failed later page, the result says `truncated` with a message, and you can add missing identifiers by hand. See [architecture](architecture.md#management-api-for-the-dashboard) for the details. Connection tests check that the provider's model endpoint answers; the playground verifies real inference.

## Not implemented

- Gemini CLI OAuth, Vertex AI, Antigravity, Grok, Qwen and other CLIProxyAPI providers.
- Translation between providers (for example Anthropic Messages to OpenAI), except Chat Completions on Codex.
- Automatic model discovery: catalogs are listed on request and never applied automatically. Catalogs larger than the bounds above are returned in part.
- Full CLIProxyAPI parity in general.
