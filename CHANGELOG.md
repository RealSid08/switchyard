# Changelog

All notable changes are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## Unreleased

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
- Metadata-only request history (newest 1,000) with lifetime counters.

### Project

- Offline integration test suites against loopback mock providers.
- CI on Linux, macOS and Windows; tagged releases with platform archives and SHA-256 checksums; Dockerfile and Compose file.
