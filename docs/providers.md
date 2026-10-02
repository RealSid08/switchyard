# Providers: OpenCode Zen and Go, Antigravity, Cursor

This page covers the providers added on top of the core OpenAI, Codex, Anthropic and Gemini connection kinds: what each one can do through Switchyard, how to set it up, and how its credentials are handled. Facts about third-party services were checked against their documentation and installed clients on 2026-10-03 and can change without notice.

| Provider | Inference through Switchyard | Credentials | Quota and billing |
| --- | --- | --- | --- |
| OpenCode Zen | Yes: Responses, Chat Completions and Messages, using the existing `openai` and `anthropic` kinds | Read-only import of the Zen key from OpenCode, or a key entered by hand | No quota through the API key. Billing only through an opt-in console monitor |
| OpenCode Go | Same as Zen, with the Go base URL | Read-only import of the Go key, or a key entered by hand | Rolling 5-hour, weekly and monthly allowance, read automatically with the key |
| Antigravity (Google) | Yes: Gemini `generateContent`, Chat Completions and Messages. No Responses, no Responses WebSocket | Google browser sign-in, read-only import of the `agy` token file or CLIProxyAPI `antigravity` files | Remaining fraction and reset time per model |
| Cursor | No. Cursor's own models, including Composer, have no external inference API | None for inference | Usage monitor only, see [usage sources](usage-sources.md) |

## OpenCode Zen and Go

Zen and Go are HTTP APIs that speak several protocols on one key, so Switchyard uses its existing connection kinds rather than a new one. Each key becomes two connections:

| Connection | Kind | Base URL | Client endpoints | Upstream auth |
| --- | --- | --- | --- | --- |
| `OpenCode Zen` | `openai` | `https://opencode.ai/zen/v1` | `/v1/responses`, `/v1/chat/completions` | `Authorization: Bearer` |
| `OpenCode Zen (Messages)` | `anthropic` | `https://opencode.ai/zen/v1` | `/v1/messages`, `/v1/messages/count_tokens` | `x-api-key` |
| `OpenCode Go` | `openai` | `https://opencode.ai/zen/go/v1` | `/v1/responses`, `/v1/chat/completions` | `Authorization: Bearer` |
| `OpenCode Go (Messages)` | `anthropic` | `https://opencode.ai/zen/go/v1` | `/v1/messages`, `/v1/messages/count_tokens` | `x-api-key` |

Which models answer on which endpoint is decided by OpenCode, not Switchyard. Each connection starts with a suggested model list taken from OpenCode's Zen and Go endpoint tables; list the live catalog (`GET /api/connections/{id}/models`) and keep what the account actually offers. Zen's Gemini-family models are documented only for Google's SDK path (`/zen/v1/models/{model}`), which Switchyard does not route, so they are not offered. Responses WebSocket stays off unless you enable it on the connection and OpenCode supports it.

### Setup

1. In OpenCode on the machine running Switchyard, run `opencode auth login` and choose OpenCode Zen or OpenCode Go.
2. In the control room, open **Connections**, then **Import OpenCode keys**. Or from a terminal: `switchyard import opencode` (both Zen and Go) or `switchyard import opencode_go` (Go only). `--path` selects another `auth.json`.
3. Check the model list, then send a test from the playground.

The importer reads `$XDG_DATA_HOME/opencode/auth.json`, else `~/.local/share/opencode/auth.json`. Only `{"type":"api","key":...}` records under `opencode` (Zen) and `opencode-go` (Go) are imported; other providers in the file, OAuth records and empty keys are ignored. If none is found, the import fails with a message that says to run `opencode auth login`.

You can also create the same connections by hand with any Zen or Go key: kind `openai` or `anthropic` and the base URL from the table.

### Credentials

- Imported keys have `credential_source: native_opencode`. The file is read, never written, and keys are never refreshed. A key rejected by OpenCode returns 401 asking you to run `opencode auth login` again and reimport.
- The account identity is a hash of the provider, the kind and a hash of the key. Reimporting the same key updates the existing connections and keeps your name, enabled state, base URL and models. A rotated key is a different credential: it imports as new connections and leaves the old ones for you to remove.
- Keys are never returned by the API, written to logs or sent to anything other than the connection's base URL. Client keys are never forwarded to OpenCode.

### Go quota

Each enabled OpenCode Go connection gets its allowance read automatically with its own key from `GET https://opencode.ai/zen/go/v1/usage`, shown on the Sources page as three separate windows: rolling 5 hours, weekly and monthly. The two connections made from one Go key are one account and are polled once. A reply without the rolling window is reported as unreadable, never as 0% used. Windows belong to one account and are never added up.

A Zen key cannot read Zen balance or billing. Those need an opt-in OpenCode console monitor with a browser session cookie; see [usage sources](usage-sources.md). Gateway tokens and estimated cost for both appear in [Usage](usage.md) as for any other connection.

Go requires a stable conversation identifier and an identifiable client user agent, as documented in [Go's client contract](https://opencode.ai/docs/go/#where-can-i-use-it). Switchyard maps native OpenCode, Codex and Claude session headers to a hashed `x-opencode-session` and identifies itself as `switchyard/<version>`. Full-history requests without a session header use a hash of the first user message. Clients sending only a partial continuation should supply a native session header to retain stable routing; unrelated prompt text is never forwarded as a header or stored.

## Antigravity

An Antigravity connection (kind `antigravity`) sends requests to Google's Cloud Code `v1internal` API with the signed-in Google account's Antigravity entitlement. These endpoints are not a published Google API: Switchyard follows the shapes used by the Antigravity clients as documented in two MIT-licensed projects, CLIProxyAPI and CodexBar (see [THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md)). Google can change or restrict them at any time, and using them may be subject to Google's terms for Antigravity.

### Client endpoints

| Client endpoint | Supported | What happens |
| --- | --- | --- |
| `/v1beta/models/{model}:generateContent`, `:streamGenerateContent` | Yes | The Gemini body is forwarded inside the `v1internal` envelope |
| `/v1/chat/completions` | Yes | Translated to and from Gemini |
| `/v1/messages` | Yes | Translated to and from Gemini |
| `/v1/responses`, Responses WebSocket | No | Not routed to Antigravity accounts; a model only on Antigravity returns 400 naming the endpoints that serve it |
| `/v1/messages/count_tokens` | No | Not routed to Antigravity accounts |

Upstream, every request goes to `{base}/v1internal:generateContent` or `{base}/v1internal:streamGenerateContent?alt=sse` with base `https://daily-cloudcode-pa.googleapis.com`. The body is `{"model","project","request","requestType","userAgent","requestId"}`, where `request` is a Gemini `GenerateContentRequest` with a `sessionId` derived from a hash of the first user message (so retries share it; the text is not stored). Switchyard authenticates with the account's Google access token and the Antigravity Hub user agent, and never forwards client headers. Responses and each SSE event are unwrapped from `{"response": ...}` with the same 16 MiB event bound as other streams. A Google `RetryInfo` delay becomes `Retry-After`.

### Translation

Chat Completions and Messages requests are converted to Gemini and the replies converted back, streaming and non-streaming, including text, images, tool calls and results, thinking, finish reasons and usage.

- **Parameters.** Chat: `temperature`, `top_p`, `presence_penalty`, `frequency_penalty`, `seed`, `max_completion_tokens` or `max_tokens`, `stop`, `tool_choice`, `response_format` (`json_object`, `json_schema`) and `reasoning_effort`. Messages: `temperature`, `top_p`, `top_k`, `max_tokens`, `stop_sequences`, `tool_choice` and `thinking`.
- **Rejected with 400 naming the field**, because Antigravity cannot honour them: `n` above 1, `logprobs` and `top_logprobs`, audio output and non-text `modalities`, `prediction`, `logit_bias`, `web_search_options`, remote image URLs and file ids (send `data:` URLs or base64), server tools on Messages, `redacted_thinking` blocks, and Gemini `cachedContent`. Tool names must be letters, digits, `_ - . :` and at most 128 characters.
- **Accepted and ignored**, because they do not change the result upstream: `user`, `metadata`, `store`, `stream_options`, `parallel_tool_calls` and cache-control hints.
- **Claude models on Antigravity** need extra care, applied automatically: tool calling in `VALIDATED` mode, tool schemas reduced to what the upstream accepts, and real thinking signatures replayed on the thinking part (dummy signatures are refused). Gemini models carry the signature on the part after the thought and accept Google's documented `skip_thought_signature_validator` when the original is unavailable.
- **Thinking signatures for Chat clients.** Chat has no field for them, so opaque signatures (never text) are kept in a bounded in-memory cache keyed by tool call id. After a restart they are gone; for Claude models, thinking is then turned off for turns that replay earlier tool calls.

### Setup

**Browser sign-in.** In the control room choose **Sign in** for Antigravity, or call `POST /api/oauth/start` with `{"provider":"antigravity"}`. This is a Google sign-in on the fixed callback `http://localhost:51121/oauth-callback`; see [account sign-in](oauth.md#antigravity-google-sign-in) for the flow, the OAuth client and remote use. After sign-in Switchyard reads the account email, discovers its Cloud Code project and seeds the model list from the account's live catalog (or a suggested list if the catalog is unavailable).

**Import.** `switchyard import antigravity` reads the Antigravity CLI's file token at `~/.gemini/antigravity-cli/antigravity-oauth-token` (or `--path`). `agy` writes this file only when the OS keyring is unavailable, and Switchyard never reads the keyring, so on most machines this import finds nothing and browser sign-in is the way in. A CLIProxyAPI directory import (`switchyard import cliproxy`) also picks up its `type: "antigravity"` files, including their `project_id`.

**Project.** Each account needs its Cloud Code project, stored on the connection. Switchyard asks `loadCodeAssist` and, if the account has none yet, onboards it with `onboardUser` (polled up to 5 times within 25 seconds). If the project cannot be determined at sign-in or import, the first request retries discovery and otherwise fails with 424 asking you to open Antigravity once with that account.

### Models and quota

`GET /api/connections/{id}/models` reads `v1internal:fetchAvailableModels` (sorted, at most 1,000, internal tab-completion models omitted). It never changes the configured models by itself.

The Sources page shows, per model, the remaining fraction (0 to 100%) and reset time from the same catalog, falling back to `v1internal:retrieveUserQuota`. A missing fraction stays unknown. Values belong to one model on one account and are never summed. Gateway usage for Antigravity is recorded as subscription usage; see [Usage](usage.md).

### Credentials and security

- Browser sign-ins (`oauth`) are refreshed by Switchyard with Google's token endpoint and the same OAuth client used to sign in. The `agy` file (`native_agy`) and CLIProxyAPI files (`cliproxy`) are read-only copies: never refreshed by Switchyard, never written, and reread when the token expires or is rejected. See [account sign-in](oauth.md#imported-accounts-are-never-refreshed-by-switchyard).
- The account identity is the lowercased Google account email (hashed), so sign-in and every import of the same account update one connection. Without an email, the identity is the source itself and is never shared.
- Switchyard does not ship the Antigravity OAuth client secret. It is taken from the environment or read from the installed app; see [account sign-in](oauth.md#antigravity-google-sign-in).
- Control-plane calls (project, catalog, quota) are bounded to 1 MiB and 20 seconds. Errors shown to you are fixed messages; provider error text is not passed through from these calls.

### Verification status

Covered by automated tests against local mocks of Google's OAuth, userinfo and Cloud Code endpoints: sign-in with PKCE and offline access, project discovery and onboarding, refresh, `agy` and CLIProxyAPI imports (read-only, rotation adoption, no refresh), quota and catalog parsing, the request envelope, Chat and Messages translation (including signatures and rejected fields), and streaming through the gateway (`tests/antigravity_accounts.rs`, `tests/antigravity_protocol.rs`, `tests/antigravity_gateway.rs`).

Not verified live: neither the Antigravity app nor `agy` is installed on the development machine, and no Antigravity account was available, so no request has been sent to Google's real endpoints from Switchyard.

## Cursor

Cursor's own models, including Composer, are served only inside Cursor. There is no documented inference API for them, so Switchyard cannot offer them as a connection or route requests to them. Cursor's Cloud Agents API launches repository agents and is not a model endpoint; it is not implemented. What Switchyard can do with Cursor:

- **Read usage.** An opt-in monitor reads Cursor usage and spend for a personal account or a team. See [usage sources](usage-sources.md).
- **Act as Cursor's model provider.** Cursor's bring-your-own-key settings can point chat models at Switchyard. Requirements and limits are in [connecting clients](clients.md#cursor).

## Not implemented

- Zen Gemini-family models (Google AI SDK path).
- Antigravity through the Responses API or Responses WebSocket, and Anthropic token counting for Antigravity accounts.
- Reading the Antigravity login from the OS keyring.
- Any inference for Cursor's own models.
