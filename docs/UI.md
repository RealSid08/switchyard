# Switchyard UI

The control room for Switchyard: a React single-page app in `ui/`, built to `ui/dist` and embedded into the Rust binary. This document covers the product and design rationale, how to develop and test it, how the server should serve it, every assumption the UI makes about the backend contract, and what we'd like the backend to add.

## Product and UX rationale

Switchyard's users are coding-agent power users. They already pay for Codex and Claude subscriptions or hold API keys, and they want one fast local endpoint that keeps streaming, tool calls and WebSockets intact, without editing YAML. The UI is built around three jobs:

1. **Get to a working gateway fast.** With zero connections, the overview *is* the setup flow: import a Codex or Claude login in one click (or add an API provider), create a client key, copy a client snippet. Every step is driven by real state: steps tick off when a connection, a key and the first request actually exist.
2. **Keep many accounts reliable without babysitting.** Several accounts for the same provider are first-class: duplicate names are disambiguated (`Codex · 6b17f4`), re-importing the same login refreshes instead of duplicating (and the UI says so), the Routes page shows which models are pooled across accounts, and the UI warns before you disable or delete a connection that would strand a route.
3. **See what's happening.** Live activity over a WebSocket, a clear connection-status pill, request details with plain-language explanations of failure codes, and a playground that shows rendered output alongside every raw frame and its timing.

Principles:

- **No fake data, ever.** Empty states are real. Zero means zero; a missing value renders as `–`, not a placeholder number. The traffic chart only draws minutes that exist, on a continuous one-hour axis.
- **Trust is a feature.** The UI says, where it matters, that imports never modify original credential files, that the request log stores metadata only, that a client key is shown exactly once, and that a pasted admin token lives only in this tab.
- **Recover, don't strand.** Expired sessions re-authenticate silently. A dropped live socket backs off, falls back to polling, and recovers on its own. An unreachable gateway shows a start command and reconnects automatically. Deleting a connection that routes depend on detaches it from those routes first (with confirmation) rather than failing with a 409.
- **Dense but calm.** Developer-tool density with generous rhythm, hairline rules and one accent.

## Brand direction

- **Name and mark.** A railway switchyard sorts cars onto the right tracks; the gateway does the same for requests. The mark is one track in, a switch, three tracks out, lit by a signal lamp. It's an inline SVG (`ui/src/components/BrandMark.tsx`, `ui/public/favicon.svg`).
- **Palette.** Graphite surfaces with a single warm **signal amber** (`#f2a93b`) for brand, focus and primary actions. Status colours (green, red, amber, blue) are reserved for state and always ship with text or an icon, never colour alone. Light and dark themes are separately tuned (not inverted); the theme follows the system and can be overridden in Settings or the sidebar.
- **Type.** The system UI font for speed and native feel; the system monospace for model IDs, URLs, keys and code. Tabular numerals only in aligned columns.
- **Microcopy.** Plain, direct, sentence case. A few rail references are kept light ("Off the rails" for 404). Copy avoids em dashes.
- **Provider marks** are neutral monograms (`OA`, `A\`, `G`, `>_`), not third-party logos.

## Screen map

| Route | Screen | Notes |
| --- | --- | --- |
| `/` | Overview | First-run setup flow when there are no connections. Otherwise: setup strip (until a key exists and traffic has flowed), KPIs, traffic chart (last hour, success vs failed), transport mix, recent requests, connection health. |
| `/connections` | Connections | List with test, enable toggle, edit, delete, "try in playground", "view activity". `?new=1&preset=openai|anthropic|gemini|compatible` opens the add sheet; `?import=1` opens import. |
| `/routes` | Routes | Route cards with strategy, ordered targets and health (disabled/missing targets). "Direct models" lists everything callable without a route, grouped by model, showing round-robin pools. `?new=1&model=…` pre-fills a route with every account serving that model. |
| `/activity`, `/activity/:id` | Activity | Live table (cards on mobile), URL-synced filters (`status`, `model`, `transport`, `connection`, `q`), pause/resume stream, stats for the visible set (count, failure rate, p50, p95), detail sheet with explanations and "retry in playground". |
| `/playground` | Playground | Model picker (routes and models, WS-capable marked), HTTP / SSE / WebSocket, rendered output with tool calls and reasoning, metrics (status, TTFB, first token, total, tokens, detected format), frame inspector and raw view. `?model=…` deep link. |
| `/clients` | Connect clients | Endpoint URLs with copy, and generated snippets for Codex CLI, Claude Code, OpenCode, Cursor and others, curl, OpenAI SDK, Anthropic SDK and WebSocket. |
| `/keys` | API keys | List, create with one-time reveal (confirm before closing without copying), revoke. `?new=1` opens create. |
| `/settings` | Settings | Pause/resume, read-only gateway config, theme, admin session (forget token), about. |
| any other | Not found | |

Global: sidebar with gateway status card (pause/resume) and live pill; paused banner on every page; ⌘K / Ctrl+K command palette (pages, actions, and "try model X in the playground"); `/` focuses activity search; mobile drawer navigation.

## Stack

- **Vite 8 + React 19 + TypeScript 6.0.** TypeScript is pinned to 6.0 because typescript-eslint does not support TypeScript 7 yet.
- **TanStack Query** for caching, invalidation, polling and optimistic updates.
- **lucide-react** icons (tree-shaken).
- **Hand-written CSS** with design tokens (`ui/src/styles/`): no CSS framework.
- A ~60-line history router (`ui/src/app/router.tsx`); a handful of flat routes don't need a router dependency.
- Native `<dialog>` + `showModal()` for modals and sheets: focus trapping, inert background, Esc, and focus restoration come from the platform.
- A small tokenising highlighter for snippets; output is React spans, never injected HTML.
- **Vitest** (unit), **Playwright** + **@axe-core/playwright** (end to end and accessibility).

Production bundle: about 133 kB gzipped JS (React DOM 19 is roughly half; app code is about 30 kB) and 11 kB CSS, one request each, no web fonts.

## Development

Requirements: **Node 24** and **pnpm 12**. The project pins Node with `.node-version`/`.nvmrc` (`24`), `engines.node` (`>=24 <25`) and pnpm's `devEngines.runtime` (`24.x`, `onFail: download`). With pnpm 12, scripts run on Node 24 even if the system Node is newer: pnpm downloads it automatically. (Verified on a machine with system Node 26: `pnpm exec node -v` prints `v24.x`.)

```sh
pnpm --dir ui install --frozen-lockfile

# Against a running gateway (default 127.0.0.1:7410)
pnpm --dir ui dev                 # http://127.0.0.1:5180
SWITCHYARD_PORT=7411 pnpm --dir ui dev
SWITCHYARD_BACKEND=http://127.0.0.1:7410 SWITCHYARD_UI_PORT=5190 pnpm --dir ui dev

# Without the gateway: in-memory mock backend (starts EMPTY)
pnpm --dir ui dev:mock            # UI on 5180, mock on 5181
pnpm --dir ui dev:mock:seed       # same, with sample connections, routes, keys and traffic
MOCK_AUTH=token pnpm --dir ui dev:mock   # behave like a remote browser that needs the admin token

pnpm --dir ui typecheck
pnpm --dir ui lint
pnpm --dir ui test                # unit tests (Vitest)
pnpm --dir ui test:e2e            # builds, then Playwright against the production bundle + mock
pnpm --dir ui screenshots         # regenerates ui/screenshots/
pnpm --dir ui build               # -> ui/dist
pnpm --dir ui preview
```

### Dev proxy and ports

`vite.config.ts` proxies `/api`, `/v1` and `/v1beta`, including WebSocket upgrades, to `SWITCHYARD_BACKEND`, or `http://127.0.0.1:${SWITCHYARD_PORT:-7410}`. The proxy rewrites `Host` and `Origin` to the gateway's address so the gateway's same-origin checks and loopback cookie bootstrap behave exactly as in production. The dev server binds to `127.0.0.1:${SWITCHYARD_UI_PORT:-5180}` (`strictPort`). The base path is `/`, and all API calls use relative URLs.

Because the proxy presents the gateway's own host, `/api/config` returns the gateway's real address, so snippets in dev point at the gateway port (7410), not the Vite port. That's what clients need.

### Mock backend (dev only)

`ui/mock/server.ts` implements the whole admin contract in memory: cookie and token auth (including `POST /api/session` with Bearer), connections with the gateway's validation rules, imports (repeat imports refresh instead of duplicating), tests, routes (with target validation and the 409 on deleting a referenced connection), keys, requests with filters, the `/api/events` socket, `/api/playground` streaming OpenAI Responses, Anthropic Messages or Gemini (by connection kind, as SSE or JSON), and `/api/playground/ws`. Prompts containing "fail" return a 502 to exercise error paths.

It **starts empty**. Seed with `--seed`, the dev-only **Mock** panel (bottom left), or `curl -X POST localhost:5181/api/__mock/seed`. The panel and control endpoints also simulate traffic, bursts, dropped or refused live sockets, and an expired admin session.

It can never ship: it lives outside `src/`, the panel is loaded only when `VITE_SWITCHYARD_MOCK` is set (dead-code-eliminated otherwise), `vite build` refuses to run with that variable set, and an e2e test asserts the built bundle contains no mock code.

### Tests

- **Unit (Vitest, 100+ tests):** API client and error normalisation, auth bootstrap (cookie, token exchange, fallbacks, storage failures), events reconnect/backoff/polling state machine, SSE parsing (CRLF/CR across chunk boundaries, multi-line data, UTF-8 split across bytes), stream accumulation for OpenAI Responses, Anthropic Messages, Gemini and Chat Completions (text, tool calls, reasoning, usage, errors, non-streaming JSON), snippet generation and URL resolution (wildcard hosts, path prefixes, escaping), request filtering/merging/stats, timestamp and status normalisation, connection/route validation and multi-account helpers, chart bucketing, highlighter.
- **End to end (Playwright, production bundle + mock):** first run with no fake data; import and re-import; one-time key reveal; connection validation, auto-test, safe delete out of routes; route creation from a pooled model; playground over SSE, HTTP (Anthropic JSON), WebSocket with socket reuse, Gemini SSE and an error; activity filters and detail; live socket failure → polling → recovery; silent re-auth after session expiry; remote token sign-in and sign-out; keyboard (skip link, palette, focus); pause/resume; mobile drawer; axe accessibility scans on key screens; no console errors; no mock code in the bundle.

## Serving `ui/dist` from Rust

`pnpm --dir ui build` writes `ui/dist/index.html`, `ui/dist/theme-init.js`, `ui/dist/favicon.svg` and hashed assets under `ui/dist/assets/`.

- **SPA fallback:** serve `index.html` for any GET that isn't a file and isn't under `/api/`, `/v1/` or `/v1beta/` (those must keep returning JSON 404s). Client routes include `/connections`, `/routes`, `/activity/<id>`, `/playground?model=…` and so on. The current `static_asset` handler does this.
- **Caching:** `assets/*` are content-hashed: `Cache-Control: public, max-age=31536000, immutable`. Everything else (`index.html`, `theme-init.js`, `favicon.svg`): `no-cache`. The current handler does this.
- **CSP:** the UI works under the gateway's CSP (`script-src 'self'`, `style-src 'self' 'unsafe-inline'`, `connect-src 'self'`). There are no inline scripts: the pre-paint theme script is the external `theme-init.js`. Inline `style` attributes are used for a few dynamic values, which `'unsafe-inline'` for styles permits. `connect-src 'self'` covers same-origin `ws:`/`wss:` in current browsers.

## Contract assumptions

The UI follows the briefed contract. Specifics it relies on:

- **Auth bootstrap.** On load: `GET /api/session` (credentials included). On 401: if a token is in `sessionStorage`, `POST /api/session` with `Authorization: Bearer <token>` to mint the HttpOnly cookie, so the events and playground sockets work for remote users too. If `/api/session` is missing (404/405/501), the UI probes `GET /api/overview` with the Bearer token instead. A 403 is shown as "access blocked" (cross-origin administration). Any later 401 triggers one silent re-bootstrap (the session secret changes on gateway restart), and the sign-in screen appears only if that fails.
- **Token storage.** The pasted admin token lives in `sessionStorage['switchyard.admin-token']` only, and is sent as a Bearer header on admin API calls. It is never logged, never put in URLs and never written to `localStorage`. Pasted input is trimmed and an accidental `Bearer ` prefix or quotes are removed. Client keys (`sy_…` without `sy_admin_`) get a specific hint.
- **Errors.** `{error:{message,type}}` is preferred; string `error`, top-level `message`, short plain text and HTML bodies are handled. The generic `gateway_error` type isn't shown to users.
- **Connections.** `PUT` omits `api_key` to keep the stored credential (the form says so). An empty key is never sent. Client-side validation mirrors the gateway (1–100 models, http only for loopback, no credentials/query/fragment in URLs, WebSocket only for `openai`/`codex`, no line breaks in keys); the server's message is still shown if it disagrees. A loopback base URL without a key is not flagged as a problem. After saving an enabled connection, the UI runs `POST /api/connections/:id/test` and toasts the result.
- **Deleting a referenced connection** (the gateway's 409) is handled up front: the UI rewrites each affected route without that target (or deletes a route left empty), then deletes the connection.
- **Imports.** "Added" vs "refreshed" is computed by comparing returned connection IDs with the cached list, matching the gateway's identity-based dedupe. Paths refer to the gateway machine.
- **Routes.** Model names are `encodeURIComponent`-ed in paths (they can contain `/` and `:`). Renaming a route is `PUT` new name, then `DELETE` old name. Targets must exist on their connection's model list (validated client-side too).
- **Requests.** `status` may be a number or string; `"ok"`/`"success"` and `"error"`/`"failed"` are understood, and a non-empty `error` always means failure. Timestamps may be ISO strings or epoch seconds or milliseconds. The UI asks for `limit=500`; `status=error` and `model` filters are sent to the server, and transport, connection and text filters apply client-side.
- **Events.** `ws(s)://<host>/api/events`; messages `{type:'request'|'overview', data}`. Unknown or malformed frames are ignored. Request events are merged into every cached request list they match and into the overview's recent list; if overview events stop arriving, counters are refreshed via a throttled refetch. Reconnect uses exponential backoff with jitter (1 s → 30 s cap); after 3 consecutive failures the UI polls `/api/overview` and `/api/requests` every 5 s while continuing to retry the socket. Coming back online, the tab becoming visible, or clicking the status pill retries immediately; after an outage all queries are refreshed.
- **Playground.** `POST /api/playground {model,input,transport}`. The response format is detected per event or payload (`type` field and shape), not by provider kind: OpenAI Responses SSE/JSON, Anthropic Messages SSE/JSON, Gemini SSE/JSON (including a JSON array of chunks), and Chat Completions as a fallback. Non-2xx bodies are shown in the inspector. TTFB is measured at response headers (HTTP) or first frame (WebSocket); "first token" is the first visible text, reasoning or tool-call delta.
- **Playground WebSocket.** `ws(s)://<host>/api/playground/ws?model=…`, then `{type:'response.create', response:{model,input,stream:true}}`. The socket is kept open and reused for the same model (the gateway binds one upstream session per socket) and reopened when the model changes. Stop closes the socket, since an in-flight response can't be cancelled otherwise. A handshake refusal (close 1006, no reason) is explained in plain language. The WebSocket option is enabled only when a model or route has a `supports_websocket` connection.
- **Client URLs** come from `/api/config` (`api_base`, `websocket_url`). A trailing `/v1` is stripped to get the origin; Anthropic base = origin, Gemini = origin + `/v1beta`. Wildcard hosts (`0.0.0.0`, `::`) are replaced with the hostname the browser used. If `/api/config` fails, the page's own origin is used. Snippets reference `$SWITCHYARD_API_KEY` rather than embedding a key.
- **Pause.** `POST /api/settings {paused}` is applied optimistically; pausing asks for confirmation. Copy says paused clients get 503.
- **Models list** uses `/api/models` (enabled connections only), never `/v1/models`.

## Recommended backend additions (prioritised)

1. **Client key auth for Gemini SDKs.** Google's SDKs send `x-goog-api-key` (or `?key=`). `client_auth` accepts only `Authorization: Bearer` and `x-api-key`, so the Gemini curl snippet uses `x-api-key` and Gemini SDK snippets are omitted. Accepting `x-goog-api-key` would make Gemini clients work unchanged.
2. **Per-connection health in `/api/connections`.** Add `last_error`, `last_status`, `last_used_at`, and cooldown/`retry_after_until` (the architecture already tracks Retry-After cooldowns per connection/model). The UI would show "cooling down until 14:05" and recent failures per account, which is the most useful multi-account signal still missing. Today the UI only shows on-demand test results.
3. **Upstream model discovery.** `GET /api/connections/:id/models` (proxying the provider's model list) so the add/edit form can offer real model IDs instead of typed ones. The UI ships with small suggestion lists matching the gateway's import defaults.
4. **Request record extras.** `route` (the alias requested, distinct from the upstream model), `upstream_model`, `attempts` / failover trail, `time_to_first_byte_ms`, and the key name or prefix used. These would make the activity detail answer "which account actually served this, and why".
5. **`GET /api/requests/:id`** so deep links to `/activity/:id` work after a record leaves the client's 500-record window (the gateway keeps 1,000). Today the UI shows "not in the recent log".
6. **Request cursor/time filters** (`before`, `since`, `transport`, `connection_id`) on `/api/requests` to page through history server-side.
7. **Import preview / dry run.** `POST /api/import {dry_run:true}` returning what would be added vs refreshed, so CLIProxyAPI directory imports (up to 100 accounts) can be reviewed before committing.
8. **Explicit credential clearing.** `PUT` with `api_key: null` (distinct from omitted) to remove a stored key, e.g. when switching a connection to a local server.
9. **Playground parameters.** Accept optional `instructions`, `max_output_tokens`, `reasoning` and `previous_response_id`, so the playground can test multi-turn and tool-heavy flows the way agents use them.
10. **Session expiry signal.** Include `expires_at` in the `GET /api/session` response so the UI can refresh proactively instead of reacting to a 401.
11. **Overview series resolution.** Accept `?window=1h|24h` and return per-transport and per-connection series; the chart currently shows the last hour from the gateway's retained history.

Mismatches observed in the current backend (all handled by the UI):

- `DELETE /api/connections/:id` returns 409 while routes reference it; the UI detaches first.
- `/api/config` builds `api_base` from the request `Host` and always uses `http://`/`ws://`. Behind a TLS proxy without `SWITCHYARD_PUBLIC_ORIGIN`, snippets would show `http://`. The UI uses what the server reports; setting `SWITCHYARD_PUBLIC_ORIGIN` fixes it.
- `GET /api/requests` defaults to `limit=100`; the UI always passes `limit`.
