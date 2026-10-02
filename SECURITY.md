# Security policy

Supported versions: the latest release and `trunk`.

Report a vulnerability privately through GitHub's **Report a vulnerability** flow. Do not put tokens, account files, prompts or unredacted request bodies in a public issue.

## Trust boundary

Switchyard is a local, single-user service. An administrator can point connections at any HTTPS host, read local credential files, import and sign in to accounts, and manage client keys. Administrator access therefore needs the same trust as the server user's account. Switchyard is not a multi-tenant hosted proxy.

Client keys grant model access through the gateway, nothing more. They cannot administer it, and an admin session cannot be used as a client key.

## Administration

- **Admin token.** Generated on first start (`sy_admin_` plus 64 hex characters) and written atomically with mode 0600. Startup fails if the file is empty or malformed, rather than running without a usable token. It lives at `admin-token` in the data directory.
- **Local bootstrap.** When Switchyard listens on a loopback address, the dashboard can create a session without the token, but only for a request whose `Host` is `127.0.0.1`, `localhost` or `[::1]` on the configured port and which carries same-origin Fetch Metadata. This blocks DNS rebinding and cross-site bootstrap. On a non-loopback bind (including the Docker image), every session starts from the admin token.
- **Sessions.** Each sign-in gets its own random session token, stored only as a SHA-256 hash in memory. Sessions expire after 12 hours; at most 256 are kept and the oldest is evicted first. `DELETE /api/session` revokes only the presenting browser's session. Restarting Switchyard ends all sessions. The cookie is `HttpOnly`, `SameSite=Strict` and scoped to `/api`, plus `Secure` when `SWITCHYARD_PUBLIC_ORIGIN` is an `https://` origin. A cookie is accepted only with a loopback `Host`, the configured host and port, or the host of `SWITCHYARD_PUBLIC_ORIGIN`.
- **Origin checks.** Admin requests, sign-in and logout are refused (403) when `Origin` does not match `Host` or Fetch Metadata says `same-site` or `cross-site`. There is no CORS policy that would let another site call the API.
- **Comparisons.** The admin token, client keys and OAuth `state` are compared in constant time; sessions are looked up by the SHA-256 hash of the cookie value.

## Credentials and data

- The data directory is created with mode 0700, and the database and admin token are 0600. Switchyard refuses to start in an existing data directory that other users can access (it does not change its permissions), and refuses `/`, your home directory, and symlinked state files. A lock file prevents two processes from using one data directory. Windows users must restrict the directory's ACL themselves.
- Provider tokens are stored in SQLite **in plaintext**, because they are sent upstream. Use disk encryption and keep backups private.
- Client keys are stored only as SHA-256 hashes and are shown once.
- Provider tokens never appear in management responses, sign-in status or request history. Credentials echoed in provider error messages are replaced with `[redacted]`; provider error bodies are otherwise not logged.
- Imports read credential files without modifying them, and imported tokens are never refreshed (see [account sign-in](docs/oauth.md)). Browser sign-in uses PKCE and a loopback-only callback listener.
- Request history holds metadata only (model, account, transport, status, latency, token counts, error summary) and keeps the newest 1,000 entries. Response affinity stores response ids and account ids only, for one hour.

## Upstreams and limits

- Provider redirects are not followed, so credentials are never sent to a redirected host. Plain HTTP upstreams are accepted only on loopback. Base URLs cannot contain credentials, a query or a fragment.
- Only protocol headers are forwarded upstream; client keys, cookies and arbitrary headers are not.
- Inference request bodies and WebSocket messages are limited to 64 MiB, admin request bodies to 8 MiB, non-streaming responses and single SSE events to 16 MiB, and provider error bodies read for mapping to 64 KiB.
- `--max-in-flight` bounds concurrent requests and WebSocket sessions together. Connect timeout is 10 seconds; `--timeout` bounds non-streaming requests and stream or socket inactivity.

## Deployment

Keep the default loopback bind unless you need remote access. For remote use, put Switchyard behind TLS or a private network, set `SWITCHYARD_PUBLIC_ORIGIN=https://your-host.example`, and administer it with the admin token. Use a separate client key for each tool and revoke keys you no longer use. Revocation applies to new requests and WebSocket handshakes; an open WebSocket session continues until it closes or goes idle.
