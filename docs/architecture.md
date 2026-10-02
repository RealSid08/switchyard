# Architecture

Switchyard is one Rust binary. It serves the management API, the inference gateway and the React control room (embedded with rust-embed from `ui/dist`). State lives in a private data directory: an `admin-token` file, a SQLite database in WAL mode, and a `switchyard.lock` file. The lock is held for the life of the process, so a second Switchyard process (including `switchyard import` and `switchyard token-path`) cannot use the same directory at the same time.

## Modules

| File | Responsibility |
| --- | --- |
| `src/main.rs` | CLI (runs the server by default; `import` and `token-path` subcommands), limits validation, graceful shutdown on Ctrl-C or SIGTERM with a 10 second drain. |
| `src/app.rs` | Router, admin and client authentication, admin sessions, origin checks, connection/route/key CRUD, OAuth HTTP routes, events socket, embedded assets, body limits. |
| `src/proxy.rs` | Target selection, upstream authentication and protocol headers, HTTP and SSE transport, Codex payload shaping, Chat Completions translation, provider error mapping, Responses WebSocket bridge. |
| `src/resilience.rs` | Account cooldowns (in memory) and response affinity (memory plus SQLite). |
| `src/credentials.rs` | Read-only imports, account identity, source-owned token adoption, gateway-owned refresh. |
| `src/oauth.rs` | Browser sign-in: PKCE authorization code flow, temporary loopback callback listener, token exchange and refresh. |
| `src/store.rs` | SQLite key/value state, hashed client keys, bounded request history, lifetime counters. |
| `ui/` | The control room, built separately and embedded at compile time. |

## Request lifecycle

1. **Authenticate.** `/v1` and `/v1beta` routes need a client key (`Authorization: Bearer`, `x-api-key` or `x-goog-api-key`). `/api` routes need the admin token or an admin session cookie.
2. **Select targets.** A route (if one exists for the model) or every enabled connection that lists the model. Round-robin routes and implicit pools rotate a process-wide cursor; failover routes keep their order. Targets that cannot speak the requested endpoint, and targets cooling down, are removed. If every matching target is cooling, the client gets 429 with `Retry-After` and no provider is contacted.
3. **Apply affinity.** A request with `previous_response_id` is pinned to the account that produced that response. If that account is disabled, cooling or no longer routed, the client gets 409. An unknown id is forwarded only when exactly one account can serve the model; otherwise 409.
4. **Acquire a permit.** One semaphore bounds HTTP requests and WebSocket sessions together (`--max-in-flight`). A paused gateway returns 503; a full one returns 429 with `Retry-After: 1`.
5. **Call upstream.** The token is renewed first if needed (see [account sign-in](oauth.md)). Only protocol headers are forwarded: `anthropic-version`, `anthropic-beta` (merged with the OAuth beta for Claude sign-ins), `openai-beta` and `idempotency-key`. Client keys, cookies and other headers never reach the provider. Redirects are not followed.
6. **Fail over or answer.** Before any response bytes are delivered, a definite connection failure, a 401/403/429/502/503/504, or a failed token renewal moves to the next target and cools the failed one. A timeout after the request was sent is not retried, because the provider may have acted on it.
7. **Deliver.** Streams are forwarded chunk by chunk as the client reads them. Non-streaming responses are collected with a 16 MiB cap. Codex is always called with SSE and collected when the client did not ask to stream.
8. **Record.** A guard owns the permit for the full life of the response body or socket. When it drops (completion, error or client disconnect) it releases the permit, writes one metadata record and broadcasts it to dashboards. Prompts and outputs are never stored.

Client disconnects drop the upstream request or stream. A request abandoned before completion is recorded as 499.

## Stream integrity

The SSE parser follows the event-stream rules: CRLF or LF line endings, comment lines, and multi-line `data:` fields joined with newlines. Events up to 16 MiB are accepted; larger ones fail the stream. Passthrough streams are forwarded byte for byte; the parser only observes them for usage, completion and response ids.

A stream counts as complete only when its protocol says so: `response.completed`, `response.incomplete` or `response.failed` for Responses, `[DONE]` for Chat Completions, `message_stop` for Anthropic, or a candidate `finishReason` (or prompt block) for Gemini. A stream that ends early, breaks off, or goes quiet for longer than `--timeout` ends with an `event: error` frame of type `upstream_interrupted` and `partial_output: true`, and is recorded as 502. Provider `error` and `response.failed` events are recorded as 502.

## Responses WebSocket bridge

`GET /v1/responses` upgrades to a WebSocket. The first frame must be `response.create` within 30 seconds. Switchyard then opens one upstream socket (15 second handshake limit), trying the next WebSocket-capable account if a handshake is rejected or unreachable. No inference frame has been sent at that point, so trying another account is safe. After that the session is pinned: frames flow both ways unchanged except for model rewriting and Codex payload shaping, a model change is refused with an error frame, and the account never changes mid-session. A first frame that continues a previous response follows the same affinity rules as HTTP. Messages are limited to 64 MiB and the session closes after `--timeout` without traffic.

## Resilience state

- **Cooldowns** are kept in memory per connection and model, from 1 second to 1 hour. The duration comes from `Retry-After` (seconds or HTTP date), the provider's `resets_in_seconds` or `resets_at`, or `anthropic-ratelimit-unified-reset`; otherwise 60 seconds for 429 and 10 seconds for other failures. A 401 or 403 cools the whole account; other failures cool only that model. Saving a connection, a reimport that updates its token, and a token renewal clear its cooldowns. At most 4,096 entries are kept.
- **Response affinity** maps a response id to its connection. It is written to SQLite (`response_affinity`) and loaded at startup, keeps at most 4,096 entries for one hour each, and contains no response content.
- **Pending browser sign-ins** are in memory only; a restart abandons them.

## Persistence and concurrency

SQLite calls take a short synchronous lock and never hold it across network waits. Each account has an async lock used by token renewal, reimport and connection edits, so concurrent requests renew a token once and edits are never overwritten by a refresh. Request history keeps the newest 1,000 records, pruned in the same transaction as each insert; lifetime counters (total, success, failure, per transport) are stored separately and survive restarts and pruning. The overview's median latency and per-minute series describe the retained history.

The dashboard events channel buffers 256 messages. A dashboard that falls behind receives a fresh overview and can also recover by polling.

The design targets one local user. It is not a distributed or multi-tenant proxy.

See [SECURITY.md](../SECURITY.md) for trust boundaries and [compatibility](compatibility.md) for protocol details.
