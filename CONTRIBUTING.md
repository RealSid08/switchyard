# Contributing

Small, focused contributions are welcome. Open an issue to discuss a new provider or a large protocol change before implementing it.

## Setup

1. Fork the repository and branch from `trunk`.
2. Install the pinned toolchains: Rust from `rust-toolchain.toml`, Node 24 (`.node-version`) and pnpm 12.
3. Install UI dependencies with `pnpm --dir ui install --frozen-lockfile`.
4. Build the UI (`pnpm --dir ui build`) before building or testing Rust. The binary embeds `ui/dist`, so Rust does not compile without it.

## Checks

Run everything CI runs before opening a pull request:

```sh
pnpm --dir ui typecheck
pnpm --dir ui lint
pnpm --dir ui test
pnpm --dir ui build
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

## Tests

- Protocol, routing, authentication and credential changes need deterministic integration tests under `tests/`. The shared harness in `tests/support` starts a real gateway on a loopback port against loopback mock providers, with state in a temporary directory.
- Tests must run offline and must never need real provider credentials.
- Never read or write the contributor's own CLI logins (`~/.codex`, `~/.claude`, the Keychain, CLIProxyAPI files). Pass explicit temporary paths to imports, and use the OAuth test hooks (`oauth::set_test_endpoints`, `oauth::set_test_ttl`) with mock token endpoints instead of real providers.
- Assert behavior a client would see: status codes, bytes on the wire, request log entries, and what the mock provider received.

## Design rules

- Pass native protocol fields through rather than guessing translations.
- Never replay a request after it may have reached the provider, and never move a WebSocket session or a `previous_response_id` conversation to a different account.
- Never refresh a token that another program owns. Imported credentials are read-only.
- Keep credentials, prompts and outputs out of logs, request history, responses and commits.

## Pull requests

Explain the user-visible behavior, how you verified it, and any compatibility limits. Update the relevant docs and add a line to `CHANGELOG.md` under **Unreleased**.

## UI

The control room must stay useful with zero accounts and zero traffic. Error, empty and loading states are part of a feature, and every primary action must work from the keyboard. Product copy avoids em dashes.
