/**
 * Wire types for usage, limits and pricing. Mirrors
 * /tmp/switchyard-usage-20261002/usage-contract.json (v1) and sources-contract.json.
 * Unknown is never 0: nullable numbers stay null, and token sums come with
 * `known_units` so the UI can say "Unknown" or "Partial" honestly.
 */

export type UsageWindowKey = '24h' | '7d' | '30d' | 'all' | 'custom';
export type UsageScope = 'gateway' | 'external' | 'all';

export interface UsageMetrics {
  /**
   * `unknown`: units whose outcome was never reported (app logs record usage, not whether
   * the request succeeded). `success_rate` is over succeeded + failed only, null when none.
   */
  units: { total: number; succeeded: number; failed: number; cancelled: number; unknown?: number };
  success_rate: number | null;
  attempts?: { total: number; failed: number; failovers: number };
  tokens: {
    input: number;
    cache_read: number;
    cache_write: number;
    cache_write_5m?: number;
    cache_write_1h?: number;
    output: number;
    /** Subset of output, already included in it. */
    reasoning: number;
    total: number;
    known_units: { input: number; cache_read: number; cache_write: number; output: number; reasoning: number };
    usage_reported_units?: number;
    usage_missing_units?: number;
  };
  cache?: { read_ratio: number | null; eligible_units: number };
  cost: {
    estimated_micros: number | null;
    estimated_usd?: string | null;
    api_estimated_micros?: number | null;
    subscription_equivalent_micros?: number | null;
    /**
     * Priced units whose billing (API key or subscription) can't be proved, e.g. app logs
     * that don't record which login was used. An API-price equivalent, excluded from the
     * two figures above but included in `estimated_micros`.
     */
    unknown_billing_micros?: number | null;
    unknown_billing_units?: number;
    reported_micros?: number | null;
    reported_usd?: string | null;
    priced_units: number;
    unpriced_units: number;
  };
  latency_ms?: { avg: number | null; max: number | null; samples: number };
  ttfb_ms?: { avg: number | null; samples: number };
  first_token_ms?: { avg: number | null; samples: number };
  throughput?: { output_tokens_per_second: number | null; samples: number };
}

export interface UsageAccountRow {
  connection_id: string | null;
  connection_name: string | null;
  provider: string;
  billing?: 'api_key' | 'subscription' | 'unknown';
  origin?: 'gateway' | 'external';
  source?: string | null;
  source_id?: string | null;
  account_label?: string | null;
  metrics: UsageMetrics;
}

export interface UsageReport {
  generated_at: string;
  window: { key: UsageWindowKey; from: string | null; to: string; granularity: 'hour' | 'day' | 'month'; timezone?: string };
  filters: { connection_id: string | null; provider: string | null; model: string | null; client_key_id: string | null; source: UsageScope };
  coverage: {
    ledger_started_at: string | null;
    external_started_at?: string | null;
    complete_for_window: boolean;
    message: string | null;
    excluded_legacy_requests?: number;
    overlap?: 'none' | 'deduplicated' | 'possible' | 'unknown';
  };
  /** null only when combined=false (source=all mixing gateway and external usage of unknown overlap). */
  totals: UsageMetrics | null;
  /** false: gateway and external usage may overlap, so the server refuses to add them. */
  combined?: boolean;
  by_source?: { source: 'gateway' | 'external' | 'external_disjoint' | string; metrics: UsageMetrics }[];
  series: { bucket_start: string; metrics: UsageMetrics | null; by_source?: Record<string, UsageMetrics> | null }[];
  by_account: UsageAccountRow[];
  by_provider: { provider: string; source?: string | null; metrics: UsageMetrics }[];
  by_model: { model: string; provider: string; priced?: boolean; source?: string | null; metrics: UsageMetrics }[];
  by_client: { client_key_id: string | null; client_key_name: string | null; origin?: 'gateway' | 'external'; source?: string | null; metrics: UsageMetrics }[];
  warnings: string[];
  pricing?: { version: string; as_of: string; overrides: number };
  facets?: {
    accounts?: { connection_id: string; name: string; provider: string }[];
    providers?: string[];
    models?: string[];
    clients?: { client_key_id: string; name: string }[];
  };
}

export interface UsageQuery {
  window: Exclude<UsageWindowKey, 'custom'>;
  source: UsageScope;
  connection_id?: string;
  provider?: string;
  model?: string;
  client_key_id?: string;
}

/* ---------- Pricing ---------- */

export interface RateSet {
  input: string | null;
  output: string | null;
  cache_read?: string | null;
  /** Generic cache-write rate (providers without TTL tiers). */
  cache_write?: string | null;
  cache_write_5m?: string | null;
  cache_write_1h?: string | null;
}

export interface PriceRate {
  model: string;
  provider: string;
  origin: 'official' | 'override';
  usd_per_mtok: RateSet;
  long_context?: { above_input_tokens: number; usd_per_mtok: Partial<RateSet> } | null;
  note?: string | null;
  version?: string | null;
  effective_from?: string | null;
  effective_until?: string | null;
  updated_at?: string | null;
}

export interface PricingTable {
  version: string;
  as_of: string;
  sources: { provider: string; url: string; retrieved: string }[];
  rates: PriceRate[];
  unpriced_models: { model: string; provider?: string; units: number; last_seen?: string | null; last_seen_day?: string | null }[];
  /** Prices that take effect later. */
  scheduled?: PriceRate[] | unknown;
  /** What the estimate does not model (plain sentence). */
  scope?: string | null;
}

export interface PriceOverride {
  model: string;
  usd_per_mtok: RateSet;
}

/* ---------- Sources (quotas, balances, reported billing) ---------- */

export type SourceStatus = 'ok' | 'stale' | 'unavailable' | 'needs_auth' | 'disabled' | 'refreshing';
export type QuotaUnit = 'percent' | 'tokens' | 'usd' | 'requests' | 'credits';

export interface QuotaWindow {
  id: string;
  label: string;
  unit: QuotaUnit;
  used?: number | null;
  limit?: number | null;
  remaining?: number | null;
  reset_at?: string | null;
  model?: string | null;
  scope?: 'account' | 'model' | string | null;
  period?: string | null;
}

export interface UsageSource {
  id: string;
  connection_id?: string | null;
  connection_name?: string | null;
  name: string;
  provider: string;
  status: SourceStatus;
  updated_at?: string | null;
  next_refresh_at?: string | null;
  last_success_at?: string | null;
  last_error_at?: string | null;
  message?: string | null;
  windows: QuotaWindow[];
  balances: { label: string; unit: 'usd' | 'credits' | string; currency?: string | null; value: number | null }[];
  reported_costs: {
    label: string;
    currency: string;
    amount: number | null;
    period_start?: string | null;
    period_end?: string | null;
    kind: 'billed' | 'included' | 'subscription' | 'on_demand';
  }[];
  capabilities: { quota: boolean; cost: boolean; tokens: boolean; history: boolean };
  capability_notes?: string[];
  source: 'provider_api' | 'native_file' | 'gateway_only' | string;
  refreshing?: boolean;
  /** Plan name if the provider reports one (e.g. "Pro"). */
  plan?: string | null;
  /** Non-secret account label (hashed or redacted), never an email or token. */
  identity_label?: string | null;
  stale_since?: string | null;
}

/* ---------- Native app history ---------- */

/** opencode | codex | claude, or cursor:<monitor id> (one per watched Cursor account). */
export type NativeSourceId = string;

/** Normalized for the UI from GET /api/usage/native (see normalizeNative). */
export interface NativeHistorySource {
  source: NativeSourceId;
  provider: string;
  label?: string | null;
  /** not_imported | pending | partial | complete | not_found | error | running (normalized) */
  status: string;
  available: boolean;
  /** Events imported so far. */
  units: number;
  /** App events dropped because Switchyard had already counted the same request. */
  excluded: number;
  coverage: { from: string | null; to: string | null } | null;
  running: boolean;
  /** Waiting behind another import. */
  queued: boolean;
  updated_at: string | null;
  message: string | null;
}

export interface NativeHistory {
  sources: NativeHistorySource[];
  running: boolean;
}

export interface UsageSources {
  sources: UsageSource[];
  generated_at: string;
  refreshing: boolean;
}

/* ---------- Monitors ---------- */

export type MonitorProvider = 'cursor' | 'opencode' | 'opencode_go' | 'codex' | 'claude' | 'antigravity' | 'openai' | 'anthropic' | 'gemini';
export type MonitorCredential = 'native' | 'api_key' | 'cookie' | 'file';

export interface UsageMonitor {
  id: string;
  name: string;
  provider: MonitorProvider | string;
  credential_source: MonitorCredential;
  source_path?: string | null;
  connection_id?: string | null;
  enabled: boolean;
  credential_present: boolean;
  created_at: string;
}

export interface MonitorInput {
  name: string;
  provider: MonitorProvider;
  credential_source: MonitorCredential;
  source_path?: string | null;
  /** Omit to keep, '' to clear (on update). Never echoed back. */
  credential?: string;
  connection_id?: string | null;
  enabled?: boolean;
}

export type UsageImportProvider = 'cursor' | 'opencode' | 'opencode_go' | 'codex' | 'claude' | 'antigravity';

export interface UsageImportResult {
  imported: number;
  monitors: UsageMonitor[];
  message: string;
  skipped?: { reason: string; message: string }[];
}
