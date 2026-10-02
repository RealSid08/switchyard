# Usage sources

The Usage screen distinguishes gateway requests from app-reported history. You can view each separately or side by side. A combined total is available only when non-overlap is proven. Token estimates, plan limits and charged money measure different things; they are never added together.

## Capabilities

| Source | Gateway inference and tokens | Plan limits | Native history or billing |
| --- | --- | --- | --- |
| Codex | Responses, Chat and Responses WebSocket turns | Connected subscription account, or read-only native monitor | Codex session JSONL token records |
| Claude Code | Messages and token counting | Connected subscription account, or read-only native monitor | Claude Code assistant usage records |
| OpenCode Zen | Responses, Chat and Messages by model family | Console cookie monitor for supported balance data | OpenCode SQLite usage and SDK cost estimates |
| OpenCode Go | Responses, Chat and Messages by model family | Connected Go keys, or native/key monitor | OpenCode SQLite usage; Go is subscription usage |
| Antigravity | Chat, Messages and Gemini translation | Connected account, per model | Gateway usage; no native history importer |
| Cursor | Compatible BYOK chat models through a reachable gateway | Personal native/cookie monitor or supported team API | Personal paged usage events; team reports |
| OpenAI API | Responses and Chat | No subscription quota from ordinary API keys | Organization cost reports with an authorized admin key |
| Anthropic API | Messages | No subscription quota from ordinary API keys | Organization cost reports with an authorized admin key |
| Gemini API | Gemini endpoints | No account quota from ordinary API keys | Gateway tokens and estimates; no Cloud Billing integration |
| Custom compatible endpoint | Its configured protocol | No generic account API | Gateway tokens and custom price overrides |

Cursor's own Composer and Tab inference cannot be proxied as a provider. Antigravity native credentials and model access were unavailable on the development machine; its protocol and quota paths are mock-tested. See [providers](providers.md).

## Set up monitoring

Connected Codex, Claude, Go and Antigravity accounts are monitored automatically. Add other watchers under Usage, Plan limits. A watcher can be disabled or removed without affecting inference connections. Matching Go protocol connections and native watchers share one quota read.

Native imports read credentials on the machine running Switchyard, never on the viewing browser. They copy or follow existing native sessions without changing the original files, refreshing another app's login, or signing out the app. A changed identity requires reimport rather than silently attaching another account's history.

Cursor personal monitoring uses its local app session or an explicitly supplied cookie. Its dashboard endpoints are undocumented and can change. Team monitoring needs an authorized team API key; it is distinct from a personal session. OpenAI and Anthropic organization cost reports need admin credentials with access to those reports. Ordinary model API keys cannot supply organization billing.

A missing field remains unknown. Plan windows belong to one account or model and are shown separately, with reset times. Percentages are never summed across accounts. Provider errors keep the last good reading marked stale, with a fixed explanatory message. Refresh is coalesced, throttled and subject to provider backoff; it cannot bypass a rate limit.

## Import app history

Use Usage, App history to opt in to each source. The API is:

- `GET /api/usage/native`: source progress and current job.
- `POST /api/usage/native/import {"source":"codex|claude|opencode","path":"/optional/absolute/root"}`: accept a background job.
- `DELETE /api/usage/native/{source}`: stop future reads, preserving imported totals.
- Cursor history uses `source: "cursor:<monitor id>"` after a personal monitor is configured.

Imports are incremental and bounded by rows, bytes, pages and deadlines, with private checkpoints. New work is queued while another source runs; checkpoints survive restarts. Removed or restarted jobs cannot commit stale work. Long imports may report partial progress and resume on the scheduled poll.

Codex scans session and archived-session token records. Claude scans project assistant usage records, deduplicating repeated message/request pairs. OpenCode opens its SQLite history read-only, extracts only usage metadata, and supports the v1 and v2 message layouts. Cursor personal history pages a bounded initial 30-day interval, then continues with overlap for late arrivals.

Historical ownership is taken from records when available, otherwise labelled by device. The current login never proves who owned older requests or whether they were billed as API or subscription. Native outcomes and timings are unknown when the source does not report them.

## Accuracy and privacy

A provider named Switchyard is excluded from identifiable native records. Claude response IDs already observed by the gateway are excluded too. Other history may overlap gateway traffic and stays separate. Stable event identifiers survive raw-event retention, so repeated imports do not change totals.

OpenCode's SDK cost is an estimate, not a charge. Unknown historical billing has its own subtotal. Cursor provider-reported billed amounts and organization reports retain their source and period; they are never added to gateway estimates.

Prompts, generated text and tool arguments are never stored in the usage ledger. JSONL files must be scanned to find metadata records; their contents are not retained. OpenCode uses SQL JSON extraction. Chosen import paths and credential source locations remain private checkpoint/configuration state. Public APIs never return credential values. The data directory and backups must remain private.

See [usage accounting](usage.md), [security](../SECURITY.md) and [third-party notices](../THIRD_PARTY_NOTICES.md).
