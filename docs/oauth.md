# Account sign-in and credential lifecycle

A Codex, Claude or Antigravity subscription account can be added in two ways. The difference is who owns the refresh token.

| `credential_source` | How it was added | Who refreshes it |
| --- | --- | --- |
| `oauth` | Browser sign-in started from Switchyard | Switchyard |
| `native_codex` | Import of `$CODEX_HOME/auth.json` or `~/.codex/auth.json` | The Codex CLI |
| `native_claude` | Import of `.credentials.json` in `$CLAUDE_CONFIG_DIR` or `~/.claude`, or the macOS Keychain record | Claude Code |
| `native_agy` | Import of the Antigravity CLI file token | Antigravity CLI |
| `native_opencode` | Read-only import of Zen or Go API keys | Not refreshed; key changes are adopted from the source |
| `cliproxy` | Import of CLIProxyAPI auth files | CLIProxyAPI |
| `api_key` | API key entered or imported | Never refreshed |

Every connection in `GET /api/connections` reports its `credential_source`. Tokens themselves are never returned.

## Imported accounts are never refreshed by Switchyard

OpenAI and Anthropic refresh tokens are single use. If Switchyard refreshed a token copied from the Codex CLI, Claude Code or CLIProxyAPI, the original program's copy would stop working (and the reverse). So imports are read-only copies:

- Switchyard never sends a refresh request for an imported account and never writes to the source file or Keychain record.
- When the stored access token reaches its expiry (within 60 seconds), or after a provider 401, Switchyard rereads the original source and adopts a newer token that the owning program wrote there.
- If the source has no newer usable token, the request fails with 401 and a message telling you to run `codex` or `claude` once on that machine, or to use browser sign-in instead.
- If the source now belongs to a different account, Switchyard refuses to adopt it and asks you to reimport. It also never replaces a token with a strictly older one.
- When the token's expiry is unknown, the token is used until the provider rejects it. On a 401, Switchyard rereads the source once and retries only if a different usable token is available. Otherwise it cools the account and tries the next target.

For an account that keeps working without the native CLI, use browser sign-in. It creates an independent token family that Switchyard refreshes itself, so it never conflicts with a CLI signed in to the same account.

## Account identity and reimport

Each connection has an account identity (a SHA-256 hash, never a token) used to recognise the same account on reimport or sign-in:

- **Codex:** the ChatGPT user (`sub`, `chatgpt_user_id` or `user_id` from the ID or access token, else the email) plus the workspace (`chatgpt_account_id`). Two people in one Team workspace are two accounts.
- **Claude:** the account UUID when known: from the sign-in response or profile, CLIProxyAPI's `account_uuid`, or Claude Code's `~/.claude.json` profile (default location only). Otherwise the CLIProxyAPI email, or the source file itself, because Claude access tokens are opaque and rotate.
- **API keys:** a hash of the key.

Reimporting an account updates its tokens in place. Its name, enabled state, base URL, WebSocket setting and model list are kept. Several files for the same account in one CLIProxyAPI import keep the freshest token. An import never replaces an account you signed in to with the browser while Switchyard can still renew it. Signing in with the browser to an account that was imported converts it to `oauth`.

Expiry is read from `expiresAt`, `expires_at`, `expired`, `expiry` or `expire` (seconds, milliseconds or RFC3339), falling back to the access token's JWT `exp`. Credential files over 1 MiB are refused. A CLIProxyAPI directory import reads at most 100 `.json` files.

## Browser sign-in

All routes require administrator access (session cookie or admin token) and the same-origin checks used by the rest of the admin API.

| Route | Purpose |
| --- | --- |
| `POST /api/oauth/start` with `{"provider":"codex"}` or `{"provider":"claude"}` | Start a sign-in |
| `GET /api/oauth/{id}` | Poll its status |
| `DELETE /api/oauth/{id}` | Cancel it |
| `POST /api/oauth/{id}/callback` with `{"input":"..."}` | Finish it with a pasted callback URL |

`start` returns:

```json
{"id":"…","provider":"codex","authorization_url":"https://auth.openai.com/oauth/authorize?…","expires_in_seconds":300,"status":"pending"}
```

Open `authorization_url` in a browser on the machine running Switchyard. After you approve, the provider redirects to a temporary loopback listener:

- Codex: `http://localhost:1455/auth/callback`
- Claude: `http://localhost:54545/callback`

These ports are fixed because the providers only accept these registered redirect URIs. If a port is busy (for example `codex login` or a CLIProxyAPI login is running), `start` returns 409 and names the port.

Status responses are `{id, provider, status, expires_in_seconds?, connection?, message?}`, where `status` is `pending`, `complete`, `error` or `expired`. Token exchange remains `pending`; cancellation is an `error` with a cancellation message. On success, `connection` is the public view of the new or updated account. Authorization codes, PKCE verifiers and tokens are never returned or logged.

How it behaves:

- PKCE with S256 and a fresh verifier for every sign-in. `state` is compared in constant time.
- The loopback listener accepts only loopback `Host` headers. A forged or mismatched callback is rejected without ending the sign-in.
- One sign-in per provider at a time: starting another replaces the older one, unless the older one is already exchanging its code (409). At most 8 are pending. Each expires after 5 minutes, and its listener closes as soon as it completes, fails, is cancelled or expires.
- Token exchange and account-lock waiting can be cancelled. Cancellation and account persistence share one commit point: a completed account stays complete, and a cancelled or expired flow never writes an account later. Token exchange has a separate 60-second deadline.
- A sign-in belongs to the Switchyard instance that started it; finished sign-ins can be polled for 10 minutes.
- A provider `error` callback ends the sign-in with a clear message. Token exchange and refresh calls time out after 20 seconds, and responses are capped at 256 KiB. Provider error bodies are never shown.
- The browser page after the callback contains only static text.
- Pending sign-ins are kept in memory. Restarting Switchyard abandons them; completed accounts are stored.

### Switchyard on another machine

The provider redirects the browser to `localhost` on the machine running the browser. When Switchyard runs elsewhere, the browser shows a connection error after you approve. Copy the full URL from the address bar and send it as `input` to `POST /api/oauth/{id}/callback`. Claude's manual `code#state` form is also accepted.

## Antigravity Google sign-in

Start with `POST /api/oauth/start {"provider":"antigravity"}`. Google uses PKCE, offline access and the fixed callback `http://localhost:51121/oauth-callback`. Remote clients can paste the callback URL using the same flow above.

Switchyard needs the installed Antigravity OAuth client configuration. It reads that configuration from the app where supported, or accepts `SWITCHYARD_ANTIGRAVITY_CLIENT_ID` and `SWITCHYARD_ANTIGRAVITY_CLIENT_SECRET` in the server environment. These are never shown in the control room. Without them, importing an existing login still works; browser sign-in explains what is missing.

`switchyard import antigravity` reads the CLI file fallback at `~/.gemini/antigravity-cli/antigravity-oauth-token`, or an explicit `--path`. It does not read the OS keyring. Google identity, project discovery, inference limitations and verification status are described in [providers](providers.md#antigravity).

## Refresh of browser sign-ins

- Before each request, a token expiring within 60 seconds is renewed under a per-account lock (25 second wait). Concurrent requests wait for one renewal and reuse its result.
- A rotated refresh token replaces the old one. Only token fields are written, so edits made meanwhile are kept; if the account was deleted or changed during renewal, the renewal fails instead of writing.
- A revoked or expired sign-in (provider 400, 401 or 403) returns 401 asking you to sign in again. A provider outage or rate limit returns 503 with `Retry-After`. In a multi-account route, either failure moves the request to another account.

## Limitations

- Endpoint URLs, public client IDs, scopes and callback ports mirror the official CLIs as documented by CLIProxyAPI (`internal/auth/codex`, `internal/auth/claude`, MIT). Providers can change them without notice. Automated tests use local mock token endpoints, not the live providers.
- CLIProxyAPI uses a browser-like TLS fingerprint for Anthropic's sign-in endpoints. Switchyard uses standard rustls, which a provider's bot protection could challenge.
- Rereading the Keychain source runs `security find-generic-password`, which can prompt or time out (10 seconds) when no GUI session is available.
- When a token has no known expiry, a newer token in its source is only picked up by reimporting.
