# Switchyard UI

The control room for Switchyard: a React single-page app in `ui/`, built to `ui/dist` and embedded into the Rust binary. This document covers the product and design rationale, how to develop and test it, how the server should serve it, every assumption the UI makes about the backend contract, and what we'd like the backend to add.

## Product and UX rationale

Switchyard's users are coding-agent power users. They already pay for Codex and Claude subscriptions or hold API keys, and they want one fast local endpoint that keeps streaming, tool calls and WebSockets intact, without editing YAML. The UI is built around three jobs:

1. **Get to a working gateway fast.** With zero connections, the overview *is* the setup flow: sign in with ChatGPT or Claude in the browser, or import an existing Codex / Claude Code login in one click, or add an API key; then create a client key and copy a client snippet. Every step is driven by real state: steps tick off when a connection and a key exist and a request has actually *succeeded* (failed requests don't count).
2. **Keep many accounts reliable without babysitting.** Several accounts for the same provider are first-class: duplicate names are disambiguated (`Codex · 6b17f4`), re-importing the same login refreshes instead of duplicating (and the UI says so), the Routes page shows which models are pooled across accounts, and the UI warns before you disable or delete a connection that would strand a route.
3. **See what's happening.** Live activity over a WebSocket, a clear connection-status pill, request details with plain-language explanations of failure codes, and a playground that shows rendered output alongside every raw frame and its timing.

Principles:

- **No fake data, ever.** Empty states are real. Zero means zero; a missing value renders as `–`, not a placeholder number. The traffic chart only draws minutes that exist, on a continuous one-hour axis.
- **Trust is a feature.** The UI says, where it matters, that imports never modify original credential files, that the request log stores metadata only, that a client key is shown exactly once, and that a pasted admin token lives only in this tab.
- **Recover, don't strand.** Expired sessions re-authenticate silently. During a gateway outage or restart, pages keep their last known data under a "Can't reach the gateway" banner instead of turning into error screens, and recover by themselves (verified against the real gateway: a 20 s restart recovers about 2 s after it's back, no reload). A dropped live socket backs off, falls back to polling, and recovers on its own. An unreachable gateway shows a start command and reconnects automatically. Deleting a connection that routes depend on detaches it from those routes first (with confirmation) rather than failing with a 409.
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
| `/` | Overview | First-run setup flow when there are no connections. Otherwise: setup strip (until a key exists and traffic has flowed), KPIs, a usage strip (estimated cost and tokens for 24 hours, and the account window closest to its limit), traffic chart (last hour, success vs failed), transport mix, recent requests, connection health. |
| `/connections` | Connections | List with credential source ("Browser sign-in · refreshed by Switchyard", "Codex CLI login · follows the CLI login", "API key · stored key"…), test, enable toggle, edit, delete, source-appropriate re-login (sign in again / re-import / replace key; offered inline when a test fails with 401/403). "Connect account" opens the sign-in / import / API key chooser. `?import=1` opens the chooser, `?signin=codex|claude` starts a browser sign-in, `?new=1&preset=openai|anthropic|gemini|compatible` opens the API key sheet. |
| `/routes` | Routes | Route cards with strategy, ordered targets and health (disabled/missing targets). "Direct models" lists everything callable without a route, grouped by model, showing round-robin pools. `?new=1&model=…` pre-fills a route with every account serving that model. |
| `/activity`, `/activity/:id` | Activity | Live table (cards on mobile), URL-synced filters (`status`, `model`, `transport`, `connection`, `q`, `retried`), pause/resume stream, stats for the visible set (count, failure rate, p50, p95, retried). Rows show route aliases (`coding → gpt-6.1-sol`) and a failover mark. The detail sheet shows requested vs served, gateway timing, and the attempt timeline; deep links outside the loaded window come from `GET /api/requests/{id}`. |
| `/usage` | Usage: Spend & tokens | Window (24 hours, 7 days, 30 days, all time) and scope ("Through Switchyard", "Reported by apps", "Both"), filters for account, provider, model and client, all in the URL (`window`, `scope`, `account`, `provider`, `model`, `client`). Cost hero split into API list price, subscription value and amounts providers billed; token mix (input, cache read, cache write, output, with reasoning as part of output); KPIs; stacked chart per hour/day/month; breakdown by account, model, provider and client (select a row to filter). "Both" shows the sides next to each other whenever the server can't prove they don't overlap. App history panel for imports of local app usage. |
| `/usage/limits` | Usage: Plan limits | One card per account with plan windows as meters (never summed across windows or accounts), reset times, balances, provider-reported costs, freshness and per-card refresh. Routed accounts first, then watched accounts (Cursor, OpenCode Zen/Go, Codex, Claude, Antigravity, OpenAI/Anthropic organizations) with edit, pause, resume and stop watching. API-key connections are listed compactly because they have no plan to read. `?watch=1` opens "Watch an account". |
| `/usage/pricing` | Usage: Pricing | Price list version and official sources, what estimates leave out, models without a price (with "Set price"), your price overrides (validated, saved together), upcoming scheduled prices, and the full list with search and provider filter. Prices are shown exactly (0.125 stays $0.125). |
| `/playground` | Playground | Model picker (routes and models, WS-capable marked), HTTP / SSE / WebSocket, rendered output with tool calls and reasoning, metrics (status, TTFB, first token, total, tokens, detected format), frame inspector and raw view. `?model=…` deep link. |
| `/clients` | Connect clients | Endpoint URLs with copy, and generated snippets for Codex CLI, Claude Code, OpenCode, Cursor and others, curl, OpenAI SDK, Anthropic SDK, Gemini (Google Gen AI SDKs + curl over `x-goog-api-key`) and WebSocket. `?client=gemini` etc. deep-links a tab. |
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

Production bundle: about 144 kB gzipped JS (React DOM 19 is roughly 60 kB of it) and 12 kB CSS, one request each, no web fonts.

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
pnpm --dir ui test:e2e            # builds, then Playwright against the production bundle + mock (bounded, see below)
pnpm --dir ui screenshots         # regenerates ui/screenshots/
pnpm --dir ui build               # -> ui/dist
pnpm --dir ui preview
```

## Connecting accounts: sign in, import, or API key

Subscriptions (ChatGPT/Codex and Claude) offer two paths side by side, with the difference spelled out in the chooser:

- **Sign in** (browser OAuth, `POST /api/oauth/start`): an independent login that the gateway owns and refreshes. The dialog shows two steps (open the sign-in page or copy its link; come back), a live "waiting" status with an expiry countdown, and a disclosure for finishing from another computer. On a remote browser (any non-loopback hostname) that disclosure is open by default and step 2 becomes "paste the address you land on", because the provider redirects to `localhost:1455` (Codex) or `localhost:54545` (Claude) on the *browser's* machine. The pasted value can be the full callback URL or Claude's `code#state`; obvious mistakes (missing `code`/`state`, a bare `localhost:1455/…`) get a specific hint before any request. On completion the connections, models, overview and routes queries are refreshed and a toast says whether a new account was added or an existing one refreshed.
- **Import**: reuses the CLI's saved login on the gateway machine, read-only, and follows that CLI. If it expires, sign in to the CLI again and re-import (offered from the connection's menu and inline when a test fails), or switch to a browser sign-in.

Flow lifecycle (`ui/src/lib/oauth.ts`, `OAuthFlowController`): poll `GET /api/oauth/{id}` every 1.5 s while pending; keep waiting with a "lost touch" note through network errors (a gateway restart); treat 404 as expired; `complete`/`error`/`expired` stop polling. Closing the dialog, pressing Escape, navigating away or the page unloading (`pagehide`, `fetch(..., {keepalive:true})`) sends `DELETE /api/oauth/{id}` so the callback port is released immediately. A flow that the gateway starts after the dialog already closed is cancelled as soon as its id arrives. A 409 from start (busy callback port) shows the gateway's message and a Try again button.

The connection's `credential_source` drives the card's ownership line and its recovery actions. Paths are never shown (the public API doesn't expose them and the UI never asks for them).

| `credential_source` | Shown as | Who keeps it fresh | Recovery offered |
| --- | --- | --- | --- |
| `oauth` | Browser sign-in · refreshed by Switchyard | the gateway | Sign in again |
| `native_codex` / `native_claude` | Codex CLI login / Claude Code login · follows the CLI login | the CLI | Re-import, or use a browser sign-in instead |
| `cliproxy` | CLIProxyAPI account · follows its CLIProxyAPI file | that file | Re-import from CLIProxyAPI |
| `api_key` | API key · stored key (or "No key needed · local server") | you | Replace key |

## Account health and model discovery

Each connection card shows a health chip from `health.status`, refreshed every 15 s while Connections or Overview is visible:

| Status | Chip | Meaning shown to the user |
| --- | --- | --- |
| `ready` | Ready (green) | Not held back by Switchyard. Explicitly *not* a live check of the provider; Test does that. |
| `limited` | Limited (amber) | Some models are benched on this account after a rate limit or failure; others, and other accounts, keep serving. |
| `cooling` | Cooling down (red) | An account-wide (`"*"`) cooldown after a limit or failure; traffic goes to other accounts. |
| `disabled` | Disabled (grey) | Receives no traffic. |

Cooldowns render as live countdown chips ("gpt-6.1-sol back in 1m 20s", "All models back in 2m"), counted from when the list was fetched; when the last one ends the UI refetches. Routes show "cooling" / "account cooling" on affected targets. A "Last used … · HTTP status · reason" line comes from `last_used_at`/`last_status`/`last_error`.

`credential_expires_at` only produces a warning where the user has to act: source-owned logins (`native_codex`, `native_claude`, `cliproxy`) warn under 24 h ("Switchyard can't renew it; sign in to Codex again before then") and turn red once expired. Gateway-owned browser sign-ins renew themselves, so an expired access token there is just a muted note.

**Model discovery** (`GET /api/connections/{id}/models`) opens a searchable picker: from a card's menu ("Choose models…", saves straight away via `PUT` with the credential omitted), from the edit sheet ("Browse provider models", fills the form; uses the *saved* credential and URL, which the hint says), and from the toast after a new connection passes its test. It shows the provider's display names, marks configured models the provider no longer offers ("Not offered"), keeps the existing order and appends new picks, enforces 1–100 models, warns when unticking a model that a route targets, and shows the `truncated` message for partial catalogs. A **424** means the provider rejected the account's credential: the picker explains the right fix for the credential source (sign in again / re-import / replace key) and never treats it as an admin sign-out. Timeouts and other failures leave manual entry available.

## Activity: routing, attempts and timing

- **Requested vs served.** `model` is what the client asked for; `route` is set when that was a route alias. The served model comes from the last attempt. Rows show `coding → gpt-6.1-sol`; the detail shows a Requested → Served by panel.
- **Attempts.** An ordered timeline: account, upstream model, outcome label (constant codes such as `rate_limited` mapped to "Rate limited", with an explanation on hover), HTTP status (or "no response"), duration bar, and "failover" (different account) vs "retry" (same account) badges. A failover mark and a "Retried" filter make multi-account behaviour easy to find.
- **Timing.** "Gateway timing" shows first byte, first token and total from `ttfb_ms`/`first_token_ms`/`latency_ms`, labelled as measured by the gateway from request start including earlier attempts; WebSocket rows note that a session is one row with first-turn timings. The playground's numbers are labelled "measured in this browser" and link to Activity, so the two are never conflated.

## Gemini clients

The Gemini tab generates `google-genai` (Python) and `@google/genai` (TypeScript) setups with the base URL set to the gateway origin (the SDKs add `/v1beta/models/…`), plus curl for `generateContent` and `streamGenerateContent?alt=sse`, all authenticating with `x-goog-api-key`. The UI says plainly that this is protocol-tested (the gateway's tests cover the header and both wire formats) but not yet proven against a live Gemini account.

## Usage, limits and pricing

The Usage section answers three questions: what did my traffic use and cost, how close is each account to its plan limits, and where do the prices come from. The rules behind every screen:

- **Unknown is never zero.** A token dimension nobody reported reads "Unknown". When only some requests reported it, the figure gets a quiet "≥" (at least this much) and the hero says once how many requests didn't report usage. A model without a price reads "Not priced", and the hero links to Pricing with the number of unpriced requests. Sub-cent amounts read "<$0.01".
- **Tokens don't overlap.** Total = input + cache read + cache write + output; reasoning is shown as part of output, never added again. Colors come from a validated categorical palette and every chart has a table equivalent for screen readers.
- **Three kinds of money, never mixed.** "API list price" (API-key traffic at list prices), "Subscription value" (what subscription traffic would have cost on the API, not money charged) and "Billed by providers" (only amounts a provider reported).
- **Scopes stay separate.** "Through Switchyard" is the gateway ledger, counted once per request or WebSocket turn. "Reported by apps" is read-only usage that OpenCode, Codex CLI, Claude Code and Cursor record themselves. "Both" uses the server's answer: when `combined` is false the page shows a column per source (`gateway`, `external`, and `external_disjoint` for app usage proven not to overlap), charts one source at a time, and groups the breakdown by source with shares computed within each source.
- **Limits are never added up.** Each window is its own meter (percent or "X of Y"), warning from 75% and critical from 90%, with its reset time. The same limit reported twice is shown once. The Overview strip shows only the single tightest window.
- **Freshness is always visible.** Every account card says when it last updated, whether the last attempt failed, and when the next refresh is due. Stale cards keep the last good numbers and say so; "Needs sign-in" links to the fix (Connections for routed accounts, "Update credential" for watched ones).
- **Watching is read-only.** "Watch an account" lists providers with what each can report; unsupported ones are disabled with the reason. Credentials go in a password field, are never echoed back, and can be kept, replaced or cleared on edit. Native imports ("Import from this machine") read the app's own sign-in and never take it over. Cursor is watch-only: it has no inference API Switchyard could route to, and the copy says so.
- **App history.** Import, update or remove the usage each app recorded locally. Each watched Cursor account gets its own history row; without one, the row offers "Watch Cursor". Requests an app sent through Switchyard are matched and left out ("N already counted through Switchyard").
- **Pricing.** Overrides apply to new usage only; past requests keep the price they were recorded with. "Cache write" is the 5 minute tier where a provider has tiers, otherwise its single rate; the 1 hour tier is its own column.

### Antigravity, OpenCode and Cursor

- **Antigravity** is a connection kind with Google browser sign-in (callback port 51121) and a read-only import of an existing Antigravity login. If the gateway has no OAuth client configured, its message is shown as is.
- **OpenCode the client and OpenCode Zen/Go the providers are separate things.** Connect clients has an OpenCode guide (checked against the v2 config schema: `provider.switchyard` with `@ai-sdk/openai` or `@ai-sdk/anthropic`, `options.baseURL`, `options.apiKey`, and `opencode run --model switchyard/<model>`). Connections offers OpenCode Zen and Go as API-key endpoints, plus "Import OpenCode keys", which creates one connection per API family.
- **Cursor** can use Switchyard only through its "Override OpenAI Base URL" setting, which Cursor's servers call, so the gateway must be reachable over public HTTPS (not a private network or Tailscale address). It covers chat models, not Tab completion, and Composer has no API. Cursor usage and plan limits are available by watching the account.

## Sessions and sign-out

- The gateway's browser session lasts 12 hours. Any 401 (expiry, or a restart that invalidated sessions) triggers one silent re-bootstrap: loopback browsers get a fresh cookie from `GET /api/session`, remote browsers re-exchange the stored token with `POST /api/session`. The sign-in screen appears only if that fails.
- **Sign out** (Settings) calls `DELETE /api/session` (ends only this browser's cookie), removes any stored admin token and clears the whole query cache. The signed-out screen is honest about the difference: on the gateway machine it says signing back in is one click and that reloading also signs in automatically; remote browsers are told the token was removed and get the token form. Signing back in returns to the page you were on.

### Dev proxy and ports

`vite.config.ts` proxies `/api`, `/v1` and `/v1beta`, including WebSocket upgrades, to `SWITCHYARD_BACKEND`, or `http://127.0.0.1:${SWITCHYARD_PORT:-7410}`. The proxy rewrites `Host` and `Origin` to the gateway's address so the gateway's same-origin checks and loopback cookie bootstrap behave exactly as in production. The dev server binds to `127.0.0.1:${SWITCHYARD_UI_PORT:-5180}` (`strictPort`). The base path is `/`, and all API calls use relative URLs.

Because the proxy presents the gateway's own host, `/api/config` returns the gateway's real address, so snippets in dev point at the gateway port (7410), not the Vite port. That's what clients need.

### Mock backend (dev only)

`ui/mock/server.ts` implements the whole admin contract in memory: cookie and token auth (including `POST /api/session` with Bearer), connections with the gateway's validation rules, imports (repeat imports refresh instead of duplicating), tests, routes (with target validation and the 409 on deleting a referenced connection), keys, requests with filters, the `/api/events` socket, `/api/playground` streaming OpenAI Responses, Anthropic Messages or Gemini (by connection kind, as SSE or JSON), and `/api/playground/ws`. Prompts containing "fail" return a 502 to exercise error paths.

Usage endpoints live in `ui/mock/usage.ts`: an event ledger seeded with 21 days of traffic (including requests without usage and an unpriced local model), app history imports, the `combined=false` split, quota sources for connected and watched accounts (one stale, API-key accounts as `gateway_only`), monitors, pricing with overrides and a scheduled price, and native history in the shipped response shape. Its prices are labelled `mock-fixture` and are not real list prices.

It **starts empty**. Seed with `--seed`, the dev-only **Mock** panel (bottom left), or `curl -X POST localhost:5181/api/__mock/seed`. The panel and control endpoints also simulate traffic, bursts, dropped or refused live sockets, and an expired admin session.

It can never ship: it lives outside `src/`, the panel is loaded only when `VITE_SWITCHYARD_MOCK` is set (dead-code-eliminated otherwise), `vite build` refuses to run with that variable set, and an e2e test asserts the built bundle contains no mock code.

### Tests

- **Unit (Vitest, 146 tests):** usage knowledge states (unknown, partial, known), token and money formatting, cost coverage and the three money kinds, quota meters (percent, "X of Y", tones, unknown), duplicate window removal, the tightest window (never a sum), freshness wording, plan names, provider spellings, URL query round trips, price validation, native history normalisation; health labels, cooldown countdowns, expiry warnings by credential source, attempt labels and retry detection, catalog merge/filter, Gemini guide; browser sign-in controller (pending → complete, countdown resync, expiry and 404, network blips, cancel on close, cancelling a flow that started after close, StrictMode remount, pasted callback validation and server rejection, busy port retry), credential source descriptions, API client and error normalisation, auth bootstrap (cookie, token exchange, fallbacks, storage failures), events reconnect/backoff/polling state machine, SSE parsing (CRLF/CR across chunk boundaries, multi-line data, UTF-8 split across bytes), stream accumulation for OpenAI Responses, Anthropic Messages, Gemini and Chat Completions (text, tool calls, reasoning, usage, errors, non-streaming JSON), snippet generation and URL resolution (wildcard hosts, path prefixes, escaping), request filtering/merging/stats, timestamp and status normalisation, connection/route validation and multi-account helpers, chart bucketing, highlighter.
- **End to end (Playwright, production bundle + mock, 38 tests):** usage empty states with no invented numbers; spend with partial data, unpriced models and filters (including one matching nothing); app history import with gateway-matched requests, "Both" shown side by side from the server split with no extra requests and a per-source chart; plan limits with stale data, meters, refresh and API-key accounts; watching Cursor (native import, no duplicate on re-import, per-account history, cookie credential never displayed, needs sign-in, pause, stop); pricing (set a price with a cache write rate, validation, exact prices, scope note, scheduled prices); the Overview usage strip; older gateways without usage endpoints; mobile layouts without horizontal overflow; Antigravity and OpenCode in the account chooser. Also: model discovery save (with route-breakage warning), 424 explained without sign-out then retry with a partial catalog, discovery from the edit sheet; health chips, cooldowns, CLI-login expiry and route cooling; failover attempts and gateway timing in Activity with the Retried filter; request-by-id fallback and expired-record message; Gemini setup; browser sign-in completing automatically via the provider popup and re-sign-in refreshing the same account; remote pasted callback (wrong state, then right); Claude `code#state`; expiry with retry; cancel via button and Escape releasing the flow; busy port with retry; credential sources and re-import on cards; honest local sign-out and one-click return; gateway outage keeping last data and recovering without reload; first run with no fake data; import and re-import; one-time key reveal; connection validation, auto-test, safe delete out of routes; route creation from a pooled model; playground over SSE, HTTP (Anthropic JSON), WebSocket with socket reuse, Gemini SSE and an error; activity filters and detail; live socket failure → polling → recovery; silent re-auth after session expiry; remote token sign-in and sign-out; keyboard (skip link, palette, focus); pause/resume; mobile drawer; axe accessibility scans on key screens; no console errors; no mock code in the bundle.

- **Usage against the real gateway** (QA instance on 127.0.0.1:7421, UI served by `vite preview` on 5190, headless Playwright at 1440 and 390 px): Spend, Both, Reported by apps, Plan limits (real Codex and Claude windows), Pricing (official list), Overview and Connections rendered with no console errors, no horizontal overflow, no axe WCAG A/AA violations and the admin token never in the page. A real Codex CLI history update ran through the collector and its numbers appeared in the app scope. No gateway traffic had been routed on that instance yet, so the gateway scope was verified only in its empty state there (and with data against the mock). Watching a real Cursor account was not exercised.
- **Against the real gateway** (local build, throwaway data dir, a loopback fake upstream; no real accounts used): catalog discovery and save; a rejected provider key returning 424 with the admin session intact; a `failover` route through a dead account traced as "Couldn't connect" then "Served", with gateway timing and `GET /api/requests/{id}`; health chips from real cooldown data; `x-goog-api-key` accepted (wrong key → 401). Earlier passes also verified cookie bootstrap, live events, playground HTTP/SSE, pause, browser sign-in start/cancel/busy-port, sign-out and restart recovery. Live Gemini traffic has not been tested here (no key); the Gemini tab says so.

### Bounded test runs

`pnpm test:e2e` and `pnpm screenshots` run Playwright through `ui/scripts/e2e.ts`, which:

- refuses to start if any test port (5188–5191) is already taken, so a stray server can't silently serve stale code;
- enforces a hard deadline (default 10 min, `E2E_DEADLINE_MS`), on top of Playwright's own `globalTimeout` (8 min) and per-test timeout (45 s);
- after Playwright exits, verifies every test server port is free and fails the run (killing the stragglers) if not.

Each web server is launched directly with `node` (not through `pnpm exec`, whose wrapper swallowed the shutdown signal and left `vite preview` orphaned, the cause of the earlier 5-minute hang), stopped with SIGTERM and a 3 s grace period, and the mock exits promptly on SIGTERM/SIGINT even with open sockets. A full run takes about 50 s and leaves no processes behind. Never pipe these runs into `tail -f`.

The mock supports the OAuth endpoints with controls for tests: `POST /api/__mock/oauth-busy`, `oauth-free`, `oauth-ttl?seconds=N`; `GET /__mock/oauth/authorize?id&state` stands in for the provider page and completes the flow like the local callback (`&remote=1` shows the unreachable-localhost case instead).

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
- **Browser sign-in.** `POST /api/oauth/start {provider}` → `{id, authorization_url, expires_in_seconds, status:"pending"}`; `GET /api/oauth/{id}` for status (`connection` on complete, `message` on error/expired); `DELETE /api/oauth/{id}` to cancel; `POST /api/oauth/{id}/callback {input}` for a pasted URL or `code#state`, whose 400 messages are shown inline under the field while the flow stays pending. The authorization link opens in a new tab with `noopener noreferrer`. The UI never sees codes or tokens beyond what the user pastes, and never stores them.
- **Credential source.** `credential_source` on connections is optional for older gateways (cards simply omit the ownership line).
- **Health and expiry.** `health` and `credential_expires_at` are optional; without them cards show no chip (or "Disabled"/"Enabled" on Overview) and no expiry line. `retry_after_seconds` is treated as relative to when the list response arrived.
- **Model discovery.** `GET /api/connections/{id}/models` → `{connection_id, models:[{id,name}], truncated?, message?}`. Read-only; the chosen IDs are saved with the existing `PUT` (credential omitted). 424 = provider rejected the stored credential (not an admin 401, so it never triggers re-authentication); 504 = catalog timed out. Requests are aborted when the picker closes.
- **Request detail.** `GET /api/requests/{id}` is used only when a deep-linked record isn't in the loaded list; 404 shows "Not in the recent log".
- **Record extras.** `route`, `failovers`, `attempts`, `ttfb_ms`, `first_token_ms` are all optional; older records simply show fewer sections. Attempt `error` values are the gateway's constant labels; unknown labels are shown humanised.
- **Client auth headers.** Snippets use `Authorization: Bearer` for OpenAI-style clients, `x-api-key` for Anthropic-style, and `x-goog-api-key` for Gemini.
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

- **Usage.** `GET /api/usage` per the usage contract: micro-USD integers, `null` for unknown, `known_units` per token dimension, `combined`/`by_source` and `series[].by_source` when sides may overlap, `source` on breakdown rows, `facets` for filter options. `GET /api/usage/pricing`, `PUT /api/usage/pricing/overrides {overrides}`, `GET /api/usage/sources`, `POST /api/usage/refresh {id?}`, `/api/usage/monitors` CRUD (bare array on list), `POST /api/usage/import {provider}` (with `skipped` reasons), and `/api/usage/native` (`id`, `status`, `imported_events`, `excluded_gateway_events`, `first_event_at`, `last_event_at`, `last_run_at`, top-level `job`). A 404 from any of them shows an "update Switchyard" note instead of an error. Older usage responses without `by_source` fall back to one request per scope.

## Recommended backend additions (prioritised)

Implemented since the first draft and now used by the UI: Gemini `x-goog-api-key` client auth, per-connection health, model discovery, request attempt traces and timings, and `GET /api/requests/{id}`. Still open:

1. **Key attribution on requests.** Record the client key's name or prefix on each request, so Activity can answer "which tool sent this" and filter by key.
2. **Sign-in identity and freshness.** A non-secret account label (email or workspace) and `last_refreshed_at` for `oauth` and imported connections, so cards can say "signed in as …, refreshed 3 min ago". (`credential_expires_at` now covers expiry warnings.)
3. **Request cursor/time filters** (`before`, `since`, `transport`, `connection_id`, `retried`) on `/api/requests` to page through history server-side. Today transport/connection/retried filters apply to the loaded window.
4. **Cooldown end timestamps.** Alongside `retry_after_seconds`, an absolute `until` would let the UI count down exactly across slow responses. The UI currently counts down from when the list was fetched and refetches when a cooldown ends.
5. **Import preview / dry run.** `POST /api/import {dry_run:true}` returning what would be added vs refreshed, so CLIProxyAPI directory imports (up to 100 accounts) can be reviewed first.
6. **Explicit credential clearing.** `PUT` with `api_key: null` (distinct from omitted) to remove a stored key.
7. **Playground parameters.** Optional `instructions`, `max_output_tokens`, `reasoning` and `previous_response_id`, and the created request id in a response header, so the playground can link straight to the gateway-side record of its own run.
8. **Callback mismatch semantics.** If a wrong pasted state ends the flow server-side, return the terminal status so the UI can switch to "start again" immediately rather than on the next poll.
9. **Session expiry signal.** `expires_at` in `GET /api/session` so the UI can refresh proactively instead of reacting to a 401.
10. **Overview series resolution.** `?window=1h|24h` and per-transport / per-connection series.

Usage requests (details in the shared `ui-needs.md`): emit each Claude plan window once (it currently comes back twice under two ids); canonicalise `opencode-go` to `opencode_go` at ingest; keep usage with unknown billing (Codex CLI sessions today) out of "API list price"; a display name for `plan`.

Mismatches observed in the current backend (all handled by the UI):

- `DELETE /api/connections/:id` returns 409 while routes reference it; the UI detaches first.
- `/api/config` builds `api_base` from the request `Host` and always uses `http://`/`ws://`. Behind a TLS proxy without `SWITCHYARD_PUBLIC_ORIGIN`, snippets would show `http://`. The UI uses what the server reports; setting `SWITCHYARD_PUBLIC_ORIGIN` fixes it.
- `GET /api/requests` defaults to `limit=100`; the UI always passes `limit`.
- `health.last_error` comes from the request record, so it can be free text rather than one of the constant attempt labels; the UI shows the friendly label when it recognises one and the text otherwise.
