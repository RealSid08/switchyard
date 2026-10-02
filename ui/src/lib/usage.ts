import { toDate } from './format';
import type {
  MonitorCredential,
  MonitorProvider,
  QuotaWindow,
  SourceStatus,
  UsageMetrics,
  UsageQuery,
  UsageScope,
  UsageSource,
  NativeHistory,
  NativeHistorySource,
} from './usageTypes';

/* ---------- Known / partial / unknown ---------- */

export type Knowledge = 'known' | 'partial' | 'unknown';
export type TokenDim = 'input' | 'cache_read' | 'cache_write' | 'output' | 'reasoning';

/** How much of a token dimension was actually reported, out of all accounted units. */
export function knowledge(m: UsageMetrics, dim: TokenDim): Knowledge {
  const known = m.tokens.known_units?.[dim] ?? 0;
  const total = m.units.total;
  if (!known || !total) return 'unknown';
  return known >= total ? 'known' : 'partial';
}

/** Combined knowledge of several summed dimensions: all unknown = unknown, all known = known, else partial. */
export function worstKnowledge(states: Knowledge[]): Knowledge {
  if (states.every((s) => s === 'unknown')) return 'unknown';
  if (states.every((s) => s === 'known')) return 'known';
  return 'partial';
}

/** Overall token knowledge: unknown only if nothing reported any token dimension. */
export function tokenKnowledge(m: UsageMetrics): Knowledge {
  return worstKnowledge((['input', 'cache_read', 'cache_write', 'output'] as const).map((d) => knowledge(m, d)));
}

/* ---------- Formatting ---------- */

const intFmt = new Intl.NumberFormat('en-US');

export function fmtTokens(n: number | null | undefined): string {
  if (n === null || n === undefined || !Number.isFinite(n)) return '–';
  const a = Math.abs(n);
  if (a < 10_000) return intFmt.format(Math.round(n));
  if (a < 1_000_000) return `${(n / 1000).toFixed(a < 100_000 ? 1 : 0)}K`;
  if (a < 1_000_000_000) return `${(n / 1_000_000).toFixed(a < 10_000_000 ? 2 : 1)}M`;
  return `${(n / 1_000_000_000).toFixed(2)}B`;
}

/** Money from integer micro-USD. null stays null (unknown is not $0). */
export function fmtMicros(micros: number | null | undefined, opts: { precise?: boolean } = {}): string | null {
  if (micros === null || micros === undefined || !Number.isFinite(micros)) return null;
  const usd = micros / 1_000_000;
  if (micros === 0) return '$0.00';
  if (Math.abs(usd) < 0.01) return opts.precise ? `$${usd.toFixed(4)}` : '<$0.01';
  if (Math.abs(usd) >= 1000) return `$${intFmt.format(Math.round(usd))}`;
  return `$${usd.toFixed(2)}`;
}

export function fmtMoney(amount: number | null | undefined, currency = 'USD'): string | null {
  if (amount === null || amount === undefined || !Number.isFinite(amount)) return null;
  try {
    return new Intl.NumberFormat('en-US', { style: 'currency', currency, maximumFractionDigits: amount !== 0 && Math.abs(amount) < 1 ? 4 : 2 }).format(amount);
  } catch {
    return `${amount.toFixed(2)} ${currency}`;
  }
}

export function fmtRatio(r: number | null | undefined): string {
  if (r === null || r === undefined || !Number.isFinite(r)) return '–';
  const p = r * 100;
  if (p > 0 && p < 0.1) return '<0.1%';
  return `${p >= 99.95 && p < 100 ? '99.9' : p.toFixed(p < 10 ? 1 : 0)}%`;
}

/* ---------- Cost ---------- */

export interface CostView {
  /** Best estimate across priced units (API list price; for subscriptions, equivalent value). */
  estimate: number | null;
  api: number | null;
  subscription: number | null;
  /**
   * Priced usage whose billing can't be proved (API key or plan), at API list prices.
   * Part of `estimate`, never of `api` or `subscription`, and never money charged.
   */
  unknownBilling: number | null;
  unknownBillingUnits: number;
  /** Actually billed, as reported by the provider. Never mixed into estimates. */
  reported: number | null;
  pricedUnits: number;
  unpricedUnits: number;
  coverage: 'full' | 'partial' | 'none';
}

export function costView(m: UsageMetrics): CostView {
  const c = m.cost;
  const priced = c.priced_units ?? 0;
  const unpriced = c.unpriced_units ?? 0;
  const coverage = priced === 0 ? 'none' : unpriced > 0 ? 'partial' : 'full';
  const unknownUnits = c.unknown_billing_units ?? 0;
  return {
    estimate: priced === 0 ? null : (c.estimated_micros ?? null),
    api: c.api_estimated_micros ?? null,
    subscription: c.subscription_equivalent_micros ?? null,
    unknownBilling: unknownUnits > 0 ? (c.unknown_billing_micros ?? null) : null,
    unknownBillingUnits: unknownUnits,
    reported: c.reported_micros ?? null,
    pricedUnits: priced,
    unpricedUnits: unpriced,
    coverage,
  };
}

/**
 * The estimate in its three disjoint parts, for stacking. Older gateways without
 * `unknown_billing_micros` put everything in api/subscription, so the remainder is 0.
 */
export function costParts(m: UsageMetrics): { api: number; subscription: number; unknown: number } {
  const c = m.cost;
  const api = c.api_estimated_micros ?? 0;
  const subscription = c.subscription_equivalent_micros ?? 0;
  const unknown = c.unknown_billing_micros ?? Math.max(0, (c.estimated_micros ?? 0) - api - subscription);
  return { api, subscription, unknown };
}

/* ---------- Outcomes ---------- */

export interface OutcomeView {
  /** Units whose outcome nobody reported. Never counted as succeeded or failed. */
  unknown: number;
  /** Succeeded + failed: the denominator of `rate`. */
  decided: number;
  rate: number | null;
  /** reported: every unit has an outcome; partial: some do; none: none were reported. */
  state: 'reported' | 'partial' | 'none';
}

export function outcomeView(m: UsageMetrics): OutcomeView {
  const u = m.units;
  const unknown = u.unknown ?? Math.max(0, u.total - u.succeeded - u.failed - u.cancelled);
  const decided = u.succeeded + u.failed;
  const state = unknown === 0 ? 'reported' : unknown >= u.total ? 'none' : 'partial';
  return { unknown, decided, rate: decided ? m.success_rate : null, state };
}

/* ---------- Token mix ---------- */

export interface MixPart {
  key: 'input' | 'cache_read' | 'cache_write' | 'output';
  label: string;
  value: number;
  knowledge: Knowledge;
}

/** Non-overlapping parts: total = input + cache_read + cache_write + output. */
export function tokenMix(m: UsageMetrics): MixPart[] {
  const parts: MixPart[] = [
    { key: 'input', label: 'Input', value: m.tokens.input, knowledge: knowledge(m, 'input') },
    { key: 'cache_read', label: 'Cache read', value: m.tokens.cache_read, knowledge: knowledge(m, 'cache_read') },
    { key: 'cache_write', label: 'Cache write', value: m.tokens.cache_write, knowledge: knowledge(m, 'cache_write') },
    { key: 'output', label: 'Output', value: m.tokens.output, knowledge: knowledge(m, 'output') },
  ];
  return parts;
}

/* ---------- Quotas ---------- */

export interface Meter {
  /** 0..100 of the window used, when it can be computed. */
  pct: number | null;
  primary: string;
  secondary: string | null;
  tone: 'ok' | 'warn' | 'err' | 'muted';
}

function unitAmount(v: number, unit: QuotaWindow['unit']): string {
  switch (unit) {
    case 'usd':
      return fmtMoney(v) ?? '–';
    case 'tokens':
      return `${fmtTokens(v)} tokens`;
    case 'requests':
      return `${intFmt.format(v)} requests`;
    case 'credits':
      return `${intFmt.format(Math.round(v * 100) / 100)} credits`;
    default:
      return `${v}%`;
  }
}

/**
 * One quota window as a meter. Percent windows are already 0 to 100 (Cursor's
 * 0.36 means 0.36 %). Each window stands alone: percentages are never summed
 * across windows or accounts.
 */
export function quotaMeter(w: QuotaWindow): Meter {
  let pct: number | null = null;
  const has = (x: number | null | undefined): x is number => typeof x === 'number' && Number.isFinite(x);
  if (w.unit === 'percent') {
    if (has(w.used)) pct = w.used;
    else if (has(w.remaining)) pct = 100 - w.remaining;
  } else if (has(w.limit) && w.limit > 0) {
    if (has(w.used)) pct = (w.used / w.limit) * 100;
    else if (has(w.remaining)) pct = ((w.limit - w.remaining) / w.limit) * 100;
  }
  if (pct !== null) pct = Math.max(0, Math.min(100, pct));
  const tone = pct === null ? 'muted' : pct >= 90 ? 'err' : pct >= 75 ? 'warn' : 'ok';
  let primary: string;
  let secondary: string | null = null;
  if (w.unit === 'percent') {
    primary = pct === null ? 'Unknown' : `${pct < 1 && pct > 0 ? pct.toFixed(2) : Math.round(pct)}% used`;
  } else if (has(w.used) && has(w.limit)) {
    primary = `${unitAmount(w.used, w.unit)} of ${unitAmount(w.limit, w.unit)}`;
  } else if (has(w.remaining) && has(w.limit)) {
    primary = `${unitAmount(w.remaining, w.unit)} left of ${unitAmount(w.limit, w.unit)}`;
  } else if (has(w.used)) {
    primary = `${unitAmount(w.used, w.unit)} used`;
    secondary = 'No limit reported';
  } else if (has(w.remaining)) {
    primary = `${unitAmount(w.remaining, w.unit)} left`;
  } else {
    primary = 'Unknown';
  }
  return { pct, primary, secondary, tone };
}

export function resetLabel(resetAt: string | null | undefined, now = Date.now()): string | null {
  const d = toDate(resetAt ?? null);
  if (!d) return null;
  const s = Math.round((d.getTime() - now) / 1000);
  if (s <= 0) return 'Resetting now';
  if (s < 3600) return `Resets in ${Math.max(1, Math.round(s / 60))} min`;
  if (s < 86_400) return `Resets in ${Math.floor(s / 3600)} h ${Math.round((s % 3600) / 60)} min`;
  const days = Math.round(s / 86_400);
  return `Resets in ${days} ${days === 1 ? 'day' : 'days'}`;
}

/**
 * A source's windows as shown: the same limit reported twice (same unit, scope, model,
 * usage and reset within a minute) appears once, keeping the better-described copy.
 * Labels get a leading capital ("weekly" -> "Weekly").
 */
export function quotaWindows(s: Pick<UsageSource, 'windows'>): QuotaWindow[] {
  const out: QuotaWindow[] = [];
  for (const w of s.windows ?? []) {
    const reset = toDate(w.reset_at ?? null)?.getTime() ?? null;
    const dupe = out.findIndex(
      (o) =>
        o.unit === w.unit &&
        (o.scope ?? null) === (w.scope ?? null) &&
        (o.model ?? null) === (w.model ?? null) &&
        (o.used ?? null) === (w.used ?? null) &&
        (o.remaining ?? null) === (w.remaining ?? null) &&
        (o.limit ?? null) === (w.limit ?? null) &&
        reset !== null &&
        Math.abs((toDate(o.reset_at ?? null)?.getTime() ?? Number.NaN) - reset) <= 60_000,
    );
    const label = w.label ? w.label.charAt(0).toUpperCase() + w.label.slice(1) : w.label;
    const next = { ...w, label };
    if (dupe < 0) out.push(next);
    else if (!out[dupe].period && w.period) out[dupe] = next;
  }
  return out;
}

const PLANS: Record<string, string> = { prolite: 'Pro Lite', pro: 'Pro', plus: 'Plus', max: 'Max', max5x: 'Max 5x', max20x: 'Max 20x', team: 'Team', free: 'Free', business: 'Business', enterprise: 'Enterprise', edu: 'Edu', ultra: 'Ultra' };

/** Provider plan ids ("prolite", "max_20x") as readable names; unknown ids are only capitalized. */
export function planLabel(plan: string): string {
  const key = plan.toLowerCase().replace(/[\s_-]/g, '');
  if (PLANS[key]) return PLANS[key];
  return plan.replace(/[_-]+/g, ' ').replace(/\b\w/g, (c) => c.toUpperCase());
}

/** The most-used meter on a source, for compact summaries (never a sum). */
export function tightestWindow(s: UsageSource): { window: QuotaWindow; meter: Meter } | null {
  let best: { window: QuotaWindow; meter: Meter } | null = null;
  for (const w of quotaWindows(s)) {
    const m = quotaMeter(w);
    if (m.pct === null) continue;
    if (!best || (best.meter.pct ?? 0) < m.pct) best = { window: w, meter: m };
  }
  return best;
}

/* ---------- Source status & freshness ---------- */

export const STATUS_INFO: Record<SourceStatus, { label: string; tone: 'ok' | 'warn' | 'err' | 'muted' | 'info'; help: string }> = {
  ok: { label: 'Up to date', tone: 'ok', help: 'The last refresh succeeded.' },
  stale: { label: 'Stale', tone: 'warn', help: 'The latest refresh failed or is overdue. Showing the last good data.' },
  unavailable: { label: 'Unavailable', tone: 'err', help: 'The provider didn’t return usage for this account.' },
  needs_auth: { label: 'Needs sign-in', tone: 'err', help: 'The credential used to read usage was rejected or is missing.' },
  disabled: { label: 'Paused', tone: 'muted', help: 'Monitoring is turned off for this account.' },
  refreshing: { label: 'Refreshing', tone: 'info', help: 'Fetching fresh numbers now.' },
};

export function relativeTime(iso: string | null | undefined, now = Date.now()): string | null {
  const d = toDate(iso ?? null);
  if (!d) return null;
  const s = Math.round((now - d.getTime()) / 1000);
  const future = s < 0;
  const a = Math.abs(s);
  const span = a < 45 ? 'moments' : a < 3600 ? `${Math.max(1, Math.round(a / 60))} min` : a < 86_400 ? `${Math.round(a / 3600)} h` : `${Math.round(a / 86_400)} d`;
  if (span === 'moments') return future ? 'in a moment' : 'just now';
  return future ? `in ${span}` : `${span} ago`;
}

/** Always-visible freshness line. Never says "fresh" when we don't know. */
export function freshness(s: UsageSource, now = Date.now()): { text: string; stale: boolean } {
  const updated = relativeTime(s.updated_at ?? s.last_success_at, now);
  const next = relativeTime(s.next_refresh_at, now);
  if (!updated) return { text: s.status === 'refreshing' ? 'First refresh in progress' : 'Never refreshed', stale: true };
  const stale = s.status === 'stale' || s.status === 'needs_auth' || s.status === 'unavailable';
  let text = `Updated ${updated}`;
  if (s.last_error_at && stale) text += `, last attempt failed ${relativeTime(s.last_error_at, now)}`;
  if (next && s.status !== 'disabled') text += ` · next ${next}`;
  return { text, stale };
}

/* ---------- URL <-> query ---------- */

const WINDOWS = ['24h', '7d', '30d', 'all'] as const;
const SCOPES: UsageScope[] = ['gateway', 'external', 'all'];

export function usageQueryFromSearch(search: string): UsageQuery {
  const p = new URLSearchParams(search);
  const w = p.get('window');
  const s = p.get('scope');
  const q: UsageQuery = {
    window: (WINDOWS as readonly string[]).includes(w ?? '') ? (w as UsageQuery['window']) : '7d',
    source: SCOPES.includes(s as UsageScope) ? (s as UsageScope) : 'gateway',
  };
  for (const [param, key] of [
    ['account', 'connection_id'],
    ['provider', 'provider'],
    ['model', 'model'],
    ['client', 'client_key_id'],
  ] as const) {
    const v = p.get(param);
    if (v) q[key] = v;
  }
  return q;
}

export function usageQueryToSearch(q: UsageQuery): string {
  const p = new URLSearchParams();
  if (q.window !== '7d') p.set('window', q.window);
  if (q.source !== 'gateway') p.set('scope', q.source);
  if (q.connection_id) p.set('account', q.connection_id);
  if (q.provider) p.set('provider', q.provider);
  if (q.model) p.set('model', q.model);
  if (q.client_key_id) p.set('client', q.client_key_id);
  const s = p.toString();
  return s ? `?${s}` : '';
}

export function activeFilterCount(q: UsageQuery): number {
  return [q.connection_id, q.provider, q.model, q.client_key_id].filter(Boolean).length;
}

/* ---------- Pricing overrides ---------- */

/** Non-negative decimal, at most 6 decimal places, at most 10000 USD per million tokens. Empty = not set. */
export function rateError(raw: string): string | null {
  const v = raw.trim();
  if (!v) return null;
  if (!/^\d+(\.\d{1,6})?$/.test(v)) return 'Use a number like 1.25 (up to 6 decimals).';
  if (Number(v) > 10_000) return 'That’s above 10,000 USD per million tokens.';
  return null;
}

/* ---------- Providers you can monitor ---------- */

export interface MonitorProviderInfo {
  id: MonitorProvider;
  label: string;
  kind: string;
  blurb: string;
  methods: { id: MonitorCredential; label: string; help: string; field?: { label: string; placeholder: string; secret: boolean } }[];
  importable: boolean;
  notes: string[];
  supported: boolean;
}

export const MONITOR_PROVIDERS: MonitorProviderInfo[] = [
  {
    id: 'cursor',
    label: 'Cursor',
    kind: 'cursor',
    blurb: 'Plan usage, included and on-demand spend for the billing cycle.',
    importable: true,
    supported: true,
    methods: [
      { id: 'native', label: 'Cursor on this machine', help: 'Reads the signed-in Cursor app’s session, read-only.' },
      {
        id: 'cookie',
        label: 'Session cookie',
        help: 'The WorkosCursorSessionToken cookie from cursor.com, for a Cursor account not signed in here.',
        field: { label: 'Session cookie', placeholder: 'WorkosCursorSessionToken value', secret: true },
      },
      {
        id: 'api_key',
        label: 'Admin API key (Teams)',
        help: 'A Cursor Teams admin key, for team-wide usage.',
        field: { label: 'Admin API key', placeholder: 'key_…', secret: true },
      },
    ],
    notes: [
      'Cursor’s own models (like Composer) aren’t offered as an API, so Switchyard can’t route traffic to them. You can still point Cursor at Switchyard with a custom OpenAI base URL where Cursor allows it.',
    ],
  },
  {
    id: 'opencode',
    label: 'OpenCode Zen',
    kind: 'opencode',
    blurb: 'Zen balance and billing status from the OpenCode console.',
    importable: true,
    supported: true,
    methods: [
      { id: 'native', label: 'OpenCode on this machine', help: 'Reads OpenCode’s saved login, read-only.' },
      { id: 'cookie', label: 'Console session cookie', help: 'From opencode.ai/console, for an account not signed in here.', field: { label: 'Session cookie', placeholder: 'auth cookie value', secret: true } },
    ],
    notes: ['Zen API keys can also be added as connections, so Switchyard can route traffic through them. Watching usage and routing traffic are separate.'],
  },
  {
    id: 'opencode_go',
    label: 'OpenCode Go',
    kind: 'opencode',
    blurb: 'Rolling 5-hour, weekly and monthly Go usage.',
    importable: true,
    supported: true,
    methods: [
      { id: 'native', label: 'OpenCode on this machine', help: 'Reads OpenCode’s saved Go key, read-only.' },
      { id: 'api_key', label: 'Go API key', help: 'An OpenCode Go API key.', field: { label: 'API key', placeholder: 'sk-…', secret: true } },
    ],
    notes: ['Go keys you’ve connected for routing already show their limits automatically.'],
  },
  {
    id: 'codex',
    label: 'Codex',
    kind: 'codex',
    blurb: 'ChatGPT plan limits and credits for a Codex login.',
    importable: true,
    supported: true,
    methods: [{ id: 'native', label: 'Codex CLI on this machine', help: 'Reads the Codex CLI login, read-only. It is never refreshed or changed.' }],
    notes: ['Codex accounts you’ve connected for routing already show their limits automatically.'],
  },
  {
    id: 'claude',
    label: 'Claude',
    kind: 'anthropic',
    blurb: 'Session, weekly and per-model limits, plus extra usage.',
    importable: true,
    supported: true,
    methods: [{ id: 'native', label: 'Claude Code on this machine', help: 'Reads the Claude Code login, read-only.' }],
    notes: ['Claude accounts you’ve connected for routing already show their limits automatically.'],
  },
  {
    id: 'antigravity',
    label: 'Antigravity',
    kind: 'antigravity',
    blurb: 'Per-model remaining quota and reset times.',
    importable: true,
    supported: true,
    methods: [{ id: 'native', label: 'Antigravity on this machine', help: 'Reads the Antigravity login, read-only.' }],
    notes: ['Antigravity accounts you’ve connected for routing already show their limits automatically.'],
  },
  {
    id: 'openai',
    label: 'OpenAI organization',
    kind: 'openai',
    blurb: 'Organization costs from the OpenAI Costs API.',
    importable: false,
    supported: true,
    methods: [{ id: 'api_key', label: 'Admin key', help: 'An organization admin key. Ordinary API keys can’t read costs.', field: { label: 'Admin key', placeholder: 'sk-admin-…', secret: true } }],
    notes: [],
  },
  {
    id: 'anthropic',
    label: 'Anthropic organization',
    kind: 'anthropic',
    blurb: 'Organization costs from the Anthropic Admin API.',
    importable: false,
    supported: true,
    methods: [{ id: 'api_key', label: 'Admin API key', help: 'An Admin API key. Ordinary API keys can’t read costs.', field: { label: 'Admin API key', placeholder: 'sk-ant-admin…', secret: true } }],
    notes: [],
  },
  {
    id: 'gemini',
    label: 'Gemini',
    kind: 'gemini',
    blurb: 'Billing lives in Google Cloud and needs a service account.',
    importable: false,
    supported: false,
    methods: [],
    notes: ['Gemini API keys can’t read billing, so Switchyard can’t monitor Gemini spend yet. Traffic through Switchyard is still counted on the Usage page.'],
  },
];

export function monitorProvider(id: string): MonitorProviderInfo | undefined {
  return MONITOR_PROVIDERS.find((p) => p.id === id);
}

const PROVIDER_LABELS: Record<string, string> = {
  openai: 'OpenAI',
  codex: 'Codex',
  anthropic: 'Anthropic',
  claude: 'Claude',
  gemini: 'Gemini',
  antigravity: 'Antigravity',
  cursor: 'Cursor',
  opencode: 'OpenCode Zen',
  opencode_go: 'OpenCode Go',
};

export function providerLabel(p: string | null | undefined): string {
  if (!p) return 'Unknown';
  // Apps spell ids their own way ("opencode-go" in Codex config): match on a canonical form.
  const canon = p.replace(/-/g, '_');
  return PROVIDER_LABELS[p] ?? PROVIDER_LABELS[canon] ?? p.charAt(0).toUpperCase() + p.slice(1);
}

/** Map provider ids to the existing monogram kinds. */
export function providerKind(p: string): string {
  if (p === 'claude') return 'anthropic';
  if (p === 'opencode_go' || p === 'opencode-go') return 'opencode';
  return p;
}

const EXTERNAL_APPS: Record<string, string> = {
  opencode: 'OpenCode',
  codex_cli: 'Codex CLI',
  claude_code: 'Claude Code',
  cursor: 'Cursor',
};

export function clientLabel(id: string | null, name: string | null): string {
  if (name) return name;
  if (id === 'playground') return 'Playground';
  if (!id) return 'Unattributed';
  if (id.startsWith('external:')) {
    const app = id.slice('external:'.length);
    return EXTERNAL_APPS[app] ?? providerLabel(app);
  }
  return `Key ${id.slice(0, 8)}`;
}

/* ---------------- Native app history ---------------- */

type RawNative = Record<string, unknown>;
const str = (v: unknown) => (typeof v === 'string' && v ? v : null);
const num = (v: unknown) => (typeof v === 'number' && Number.isFinite(v) ? v : null);

/**
 * GET /api/usage/native -> one row per source. Accepts the shipped shape (id, imported_events,
 * first/last_event_at, top-level job) and tolerates missing fields: unknown stays unknown.
 */
export function normalizeNative(raw: unknown): NativeHistory {
  const r = (raw && typeof raw === 'object' ? raw : {}) as RawNative;
  const job = (r.job && typeof r.job === 'object' ? r.job : {}) as RawNative;
  const jobSource = job.running === true ? str(job.source) : null;
  const list = Array.isArray(r.sources) ? (r.sources as RawNative[]) : [];
  const sources = list
    .map((s): NativeHistorySource | null => {
      const id = str(s.id) ?? str(s.source);
      if (!id) return null;
      const status = str(s.status) ?? 'not_imported';
      const running = jobSource === id || status === 'running';
      const queued = !running && status === 'pending';
      const from = str(s.first_event_at);
      const to = str(s.last_event_at);
      return {
        source: id,
        provider: str(s.provider) ?? id.split(':')[0],
        label: str(s.label),
        status: running ? 'running' : status,
        available: s.available !== false,
        units: num(s.imported_events) ?? num(s.units) ?? 0,
        excluded: num(s.excluded_gateway_events) ?? 0,
        coverage: from || to ? { from, to } : null,
        running,
        queued,
        updated_at: str(s.last_run_at) ?? str(s.updated_at),
        message: str(s.message),
      };
    })
    .filter((s): s is NativeHistorySource => !!s);
  return { sources, running: job.running === true || sources.some((s) => s.running || s.queued) };
}
