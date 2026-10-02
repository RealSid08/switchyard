# v0.2 verification

Verified on 2026-10-03 on Apple Silicon macOS:

- Rust formatting, Clippy with warnings denied, and 248 Rust tests.
- UI typecheck, lint, 150 unit tests and 38 browser tests against the production bundle.
- OpenAI Python SDK native Codex stream and two Responses WebSocket turns with continuation.
- OpenCode 2.0.21 in an isolated home and standalone server, through Switchyard to Go, including a read tool and the correct final file contents.
- Live Codex, Claude and Go quota readings; real read-only Codex, Claude and OpenCode history imports drained the job queue.
- Native fixture imports across queue admission, reimport, restart and in-flight removal; source files and SQLite remained unchanged.
- Actual gateway and app-history views on desktop and mobile, including separate possibly overlapping totals, unknown billing and native outcomes.
- Authentication and origin boundaries for usage reads and mutations; bounded/stale quota failures and durable deduplication past retention.

The current Claude account exhausted its allowance during verification; the v0.2 SDK probe returned 429. Native Claude inference and tools were live-verified in v0.1 and remain covered by the gateway regression suite.

Cursor personal/team monitoring is covered with provider fixtures. Cursor BYOK needs account access and a reachable HTTPS gateway; its own Composer/Tab are not inference providers. No live Cursor account was available. Antigravity OAuth, project discovery, quota, request translation and stream interruption are covered with mocks; neither its app nor an account was available for live access. Gemini live access remains unverified.

Public CI separately checks Linux, macOS, Windows and the Docker Compose setup. Release checks use the exact archives, checksums and build attestations.
