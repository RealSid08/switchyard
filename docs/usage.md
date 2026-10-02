# Usage, tokens and cost

Switchyard records the usage of every request it serves in a durable ledger, so totals survive restarts and outlive the 1000-row activity log. The dashboard reads it through `GET /api/usage`. No prompts, outputs, tool arguments or credentials are stored: only counts, timings, labels and public identifiers.

## What is counted

An accounted unit is one HTTP or SSE request, or one turn of a Responses WebSocket session. A WebSocket session with three turns is three units. Each unit records:

- The account (connection) that served it, its provider and whether it is billed per token (`api_key`) or through a plan (`subscription`, for signed-in Codex, Claude and Antigravity accounts).
- The model actually used upstream. Requests to a route alias are counted under the target model.
- The client key's public id and name (never the key). Dashboard playground requests are attributed to `playground`.
- Outcome (`succeeded`, `failed`, `cancelled`), upstream attempts, failed attempts and failovers between accounts. Usage comes only from the attempt that served the request; failed attempts are counted separately.
- Latency, time to first byte and time to first output (text, tool arguments or thinking).
- Tokens and an estimated cost.

Native history reports unknown outcomes unless the source can prove them; it does not contribute invented success rates or latency.

Failed or cancelled requests keep whatever usage the provider reported before the failure. A WebSocket terminal event repeated by the provider is counted once.

## Token dimensions

Providers report tokens differently. Switchyard normalizes them into dimensions that never overlap:

`total = input + cache_read + cache_write + output`

- `input` is uncached input.
- `cache_write` is split into 5 minute and 1 hour writes when the provider reports the split.
- `reasoning` is part of `output`, shown separately for information.

| Provider | How it reports | How Switchyard stores it |
| --- | --- | --- |
| OpenAI (Responses, Chat) | `input_tokens` includes cached and cache-write tokens | `input = input_tokens - cached - cache_write` |
| Gemini, Antigravity | `promptTokenCount` includes cached tokens; thinking is reported apart from candidates | `input = prompt - cached`; `output = candidates + thoughts` |
| Anthropic | input, cache reads and cache writes are reported separately | stored as reported |

A value the provider does not report is unknown, never 0. Every token total comes with `known_units`, the number of units that reported that dimension, so the dashboard can say "unknown" or "partial" instead of showing a misleading zero.

## Cost

Costs are estimates at public list prices, in integer micro-USD. Each unit is priced when it is recorded, using the bundled price card or custom override, with published date and UTC time tiers where available, and keeps that price. Later price changes or overrides apply only to new usage. Historical imports use the available card, not a reconstruction of past invoices.

- **Estimated cost** for `api_key` accounts approximates what the provider charges.
- **Subscription equivalent** for signed-in plan accounts is the API list price of the same usage. It is a value, not money charged; the plan's actual limits are shown on the Sources page.
- **Billing unknown** holds list-price or SDK estimates for history whose billing type cannot be proven. It is excluded from API and subscription subtotals; current credentials do not establish historical billing.
- **Reported cost** appears only when a provider reports a charged amount in its response. It is never added to estimates.

Prices come from the providers' published pages, read on 2026-10-03:

- Anthropic: https://platform.claude.com/docs/en/about-claude/pricing
- OpenAI: https://developers.openai.com/api/docs/pricing
- Google Gemini API: https://ai.google.dev/gemini-api/docs/pricing
- OpenCode Go: https://opencode.ai/docs/go/

Go has provider-specific rate cards, including long-context tiers and DeepSeek weekday peak hours. These estimate consumed plan value; they are not subscription invoices. Limited-time free models without stable effective dates remain unpriced. OpenCode native SDK estimates are retained as reported estimates.

Long-context tiers (OpenAI above 272K input tokens, Gemini above 200K) price the whole request at the higher rate. Gemini prices with a published change date use the price in force on the day of the request.

A unit is left unpriced, with its cost shown as unknown, when the model is not on the card, a token dimension it used is unknown, or the provider publishes no price for it. Examples: an unpublished cached-input rate, or audio input on Gemini models with a separate audio price. Add a custom rate for such models on the pricing screen (`PUT /api/usage/pricing/overrides`).

Not modelled: batch, priority and flex tiers, Anthropic fast mode and US data residency, server-side tool fees such as web search, and Gemini context-cache storage.

## Views and filters

`GET /api/usage?window=24h|7d|30d|all`, or a custom `from=YYYY-MM-DD&to=YYYY-MM-DD` of at most 366 days. All times are UTC. Filters: `connection_id`, `provider`, `model`, `client_key_id`, `source=gateway|external|all`.

The response has totals, a time series (hourly for 24h, daily for 7d, 30d and custom, monthly for all time) and breakdowns by account, provider, model and client. Every query reads pre-aggregated hourly or daily rows, so it stays fast however much history there is.

Averages and rates are computed only over units that report the value. Examples: latency over units with latency, cache read ratio over units with all input dimensions known, and success rate over decided units (client cancellations excluded). Throughput is output tokens per second of generation time (from first output to completion) for streamed units.

## Usage reported by apps (external sources)

Usage collected from apps' own histories (for example Codex, Claude Code, OpenCode or Cursor logs) is stored as a separate source and never mixed into gateway totals by default. With `source=all`, if any of it may include requests that also went through Switchyard, the two are shown side by side and not added together. They are combined only when the collector proved they do not overlap. Collectors skip native records whose provider response id the gateway already recorded.

## Coverage and retention

- Usage tracking starts when this version first records a request. Requests from before that remain in the activity log but are not turned into usage; the response says how many and marks older windows as partial.
- Daily totals are kept indefinitely. Hourly totals (for the 24h, 7d and 30d windows) are kept for 40 days, and individual units for 400 days. Units are deduplicated by a stable id, so replays and re-imports never count twice, even after retention. Compact event identifiers are retained indefinitely for this purpose; they contain no prompt or response content.
