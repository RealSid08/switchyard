# Changelog

All notable changes are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.2.0] - 2026-10-03

### Added

- Durable, idempotent usage accounting for HTTP, SSE and each Responses WebSocket turn, with individual and aggregate views by provider, account, client and model.
- Token dimensions, public price cards and custom overrides; separate API estimates, subscription equivalents, unknown billing and provider-reported charges.
- Live quota monitoring for connected Codex, Claude, OpenCode Go and Antigravity accounts, plus configurable Cursor monitoring and supported organization cost reports.
- Bounded, incremental read-only Codex, Claude Code and OpenCode history imports with persistent checkpoints and explicit source coverage.
- Native OpenCode Zen and Go credential imports and Antigravity Google sign-in, credential imports, project discovery and Chat, Messages and Gemini translation.
- Usage control room, plan limits, pricing, import jobs and client setup guides designed with Claude Opus 5.5.
- Authentication, retention, interruption, native import and quota regression coverage; third-party notices included in release archives.

### Reliability

- Permanent compact deduplication keys prevent old history imports from double-counting after raw-event retention.
- Generation guards and queue handoff prevent removed or restarted imports from receiving stale writes.
- Unknown native billing and request outcomes are retained as unknown. Historical account ownership is never inferred from the current login.
- Unproven native/gateway overlap is presented as separate totals; estimated and reported charges are never added together.

## [0.1.0] - 2026-10-02

Initial release.

### Gateway

- Native endpoints for OpenAI Responses and Chat Completions, Anthropic Messages and Gemini `generateContent`/`streamGenerateContent`, with client keys accepted as Bearer, `x-api-key` or `x-goog-api-key`.
- Codex subscription support with Responses payload shaping and a Chat Completions adapter (text, images, tools, tool choice, structured output, reasoning effort, Chat-style usage, no reasoning leakage).
- Persistent, bidirectional Responses WebSockets with one upstream socket per client session.
- Incremental SSE forwarding with backpressure and cancellation. Streams that end without a completion marker, break off or stall end with an `upstream_interrupted` error event and are logged as failures.
- Native provider error codes preserved, with echoed credentials redacted.

### Accounts and reliability

- Browser sign-in (PKCE) for Codex and Claude with gateway-owned refresh, plus read-only imports of Codex CLI, Claude Code and CLIProxyAPI logins that follow tokens rotated by the owning program.
- Stable account identities, so reimport and sign-in update an account in place and keep its settings.
- Round-robin and failover routes, failover before output on connection failure, 401, 403, 429, 502, 503 and 504, and cooldowns that follow `Retry-After` and provider reset hints.
- Response affinity stored in SQLite (4,096 ids, one hour) so follow-ups stay on their account across restarts.

### Control room and security

- Embedded React control room: overview, connections, routes, activity, playground, keys, client setup and settings, with live updates.
- Loopback default, hashed client keys, per-browser admin sessions with logout, strict origin checks, private data directory guarded by a single-process lock, and a validated admin token.
- Body limits of 64 MiB for inference and 8 MiB for administration; 16 MiB response and SSE event limits; shared concurrency budget.
- Metadata-only request history (newest 1,000) with lifetime counters, bounded attempt traces and first-byte/first-output timing.
- Per-account cooldown health, credential expiry and bounded provider model discovery.

### Project

- Offline integration test suites against loopback mock providers.
- CI on Linux, macOS and Windows; tagged releases with platform archives and SHA-256 checksums; Dockerfile and Compose file.
