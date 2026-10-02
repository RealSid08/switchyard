/**
 * DEV-ONLY usage, limits and pricing mock for `pnpm dev:mock` and e2e tests.
 * Never bundled. Starts empty; `seed` adds deliberately imperfect history
 * (partial coverage, missing usage, an unpriced model, stale and needs-auth
 * sources) so the UI's honest states are exercised. The price table is a
 * labelled fixture, not real pricing.
 */
import { randomUUID } from 'node:crypto';

type Origin = 'gateway' | 'external';
type Outcome = 'succeeded' | 'failed' | 'cancelled' | 'unknown';

export interface MockConn {
  id: string;
  name: string;
  kind: string;
  enabled: boolean;
  source: string;
  models: string[];
}

interface UsageEvent {
  id: string;
  at: number;
  origin: Origin;
  connection_id: string | null;
  connection_name: string | null;
  source_id: string | null;
  account_label: string | null;
  provider: string;
  model: string;
  billing: 'api_key' | 'subscription' | 'unknown';
  client_key_id: string | null;
  client_key_name: string | null;
  outcome: Outcome;
  input: number | null;
  cache_read: number | null;
  cache_write: number | null;
  output: number | null;
  reasoning: number | null;
  latency_ms: number;
  ttfb_ms: number | null;
  first_token_ms: number | null;
  streamed: boolean;
  attempts: number;
  failed_attempts: number;
  failovers: number;
  /** Frozen at record time, like the real ledger. null = unpriced. */
  cost_micros: number | null;
}

interface Rates {
  input: string | null;
  output: string | null;
  cache_read: string | null;
  cache_write_5m: string | null;
  cache_write_1h: string | null;
}

const FIXTURE_RATES: { model: string; provider: string; r: Rates; long?: { above: number; input: string; output: string } }[] = [
  { model: 'gpt-6.1-sol', provider: 'openai', r: { input: '2.50', output: '15.00', cache_read: '0.25', cache_write_5m: null, cache_write_1h: null } },
  { model: 'gpt-6-astra', provider: 'openai', r: { input: '1.25', output: '10.00', cache_read: '0.125', cache_write_5m: null, cache_write_1h: null } },
  { model: 'gpt-6-luna', provider: 'openai', r: { input: '0.40', output: '1.60', cache_read: '0.04', cache_write_5m: null, cache_write_1h: null } },
  {
    model: 'claude-opus-5-5',
    provider: 'anthropic',
    r: { input: '5.00', output: '25.00', cache_read: '0.50', cache_write_5m: '6.25', cache_write_1h: '10.00' },
    long: { above: 200000, input: '10.00', output: '37.50' },
  },
  { model: 'claude-sonnet-5-5', provider: 'anthropic', r: { input: '3.00', output: '15.00', cache_read: '0.30', cache_write_5m: '3.75', cache_write_1h: '6.00' } },
  { model: 'gemini-3-pro', provider: 'gemini', r: { input: '2.00', output: '12.00', cache_read: '0.20', cache_write_5m: null, cache_write_1h: null } },
];

const NO_RATES: Rates = { input: null, output: null, cache_read: null, cache_write_5m: null, cache_write_1h: null };
const HOUR = 3600_000;
const DAY = 24 * HOUR;

export function createUsageMock(deps: { connections: () => MockConn[]; keys: () => { id: string; name: string }[]; broadcast?: () => void }) {
  let events: UsageEvent[] = [];
  let ledgerStartedAt: number | null = null;
  let overrides: { model: string; r: Rates }[] = [];
  const monitors: Map<string, Record<string, unknown>> = new Map();
  const sourceState = new Map<string, { status?: string; refreshingUntil?: number; updatedAt: number; message?: string | null }>();
  let externalImported = new Set<string>();
  const nativeJobs = new Map<string, { startedAt: number; done: boolean; updatedAt: number }>();
  const NATIVE_LABELS: Record<string, string> = { opencode: 'OpenCode', codex: 'Codex CLI', claude: 'Claude Code', cursor: 'Cursor' };

  /** Same shape as the real GET /api/usage/native: one row per app, plus one per watched Cursor account. */
  function nativeIds() {
    const cursors = [...monitors.values()].filter((m) => m.provider === 'cursor' && m.credential_source !== 'api_key').map((m) => `cursor:${m.id}`);
    return ['opencode', 'codex', 'claude', ...cursors];
  }
  function nativeState() {
    const now = Date.now();
    let current: { source: string; startedAt: number } | null = null;
    const sources = nativeIds().map((id) => {
      const provider = id.split(':')[0];
      const job = nativeJobs.get(id);
      if (job && !job.done && now - job.startedAt > 2000) {
        importExternal(provider);
        job.done = true;
        job.updatedAt = now;
      }
      const running = !!job && !job.done;
      if (running) current = { source: id, startedAt: job.startedAt };
      const evs = events.filter((e) => e.source_id === `native:${provider}`);
      return {
        id,
        provider,
        label: NATIVE_LABELS[provider],
        available: true,
        enabled: !!job,
        status: !job ? 'not_imported' : running ? 'pending' : 'complete',
        message: null,
        imported_events: evs.length,
        // Requests the app recorded that Switchyard had already counted (matched by response id).
        excluded_gateway_events: job && !running && provider === 'codex' ? 12 : 0,
        first_event_at: evs.length ? new Date(Math.min(...evs.map((e) => e.at))).toISOString() : null,
        last_event_at: evs.length ? new Date(Math.max(...evs.map((e) => e.at))).toISOString() : null,
        last_run_at: job?.done ? new Date(job.updatedAt).toISOString() : null,
        custom_path: false,
        overlap: provider === 'cursor' ? 'disjoint_except_own_api_keys' : 'unknown',
        account_attribution: provider === 'codex' ? 'workspace_from_record' : provider === 'cursor' ? 'monitor_account' : 'device',
      };
    });
    const cur = current as { source: string; startedAt: number } | null;
    return {
      sources,
      job: { running: !!cur, source: cur?.source ?? null, started_at: cur ? new Date(cur.startedAt).toISOString() : null },
      notes: ['Native history is read-only and labelled by device or by the account the record names, never by the current sign-in.'],
      generated_at: new Date(now).toISOString(),
    };
  }


  const rateFor = (model: string): Rates | null => overrides.find((o) => o.model === model)?.r ?? FIXTURE_RATES.find((x) => x.model === model)?.r ?? null;

  function priceEvent(e: Omit<UsageEvent, 'cost_micros'>): number | null {
    const r = rateFor(e.model);
    if (!r || r.input == null || r.output == null) return null;
    if (e.input == null || e.output == null) return null;
    if ((e.cache_read ?? 0) > 0 && r.cache_read == null) return null;
    if ((e.cache_write ?? 0) > 0 && r.cache_write_5m == null) return null;
    const usd =
      (e.input * Number(r.input) + e.output * Number(r.output) + (e.cache_read ?? 0) * Number(r.cache_read ?? 0) + (e.cache_write ?? 0) * Number(r.cache_write_5m ?? 0)) / 1e6;
    return Math.round(usd * 1e6);
  }

  function push(e: Omit<UsageEvent, 'cost_micros' | 'id'>) {
    const full = { ...e, id: randomUUID() } as UsageEvent;
    full.cost_micros = priceEvent(full);
    events.push(full);
    if (e.origin === 'gateway' && (ledgerStartedAt === null || e.at < ledgerStartedAt)) ledgerStartedAt = e.at;
  }

  const billingOf = (c: MockConn | undefined) => (!c ? 'unknown' : c.source === 'api_key' ? 'api_key' : 'subscription');
  const providerOf = (c: MockConn | undefined) => (c ? c.kind : 'unknown');

  function tokensFor(kind: string, rnd = Math.random) {
    const input = Math.round(800 + rnd() * 30000);
    const output = Math.round(60 + rnd() * 2600);
    const cacheRead = kind === 'gemini' ? 0 : Math.round(input * (0.2 + rnd() * 0.6));
    const cacheWrite = kind === 'anthropic' ? Math.round(rnd() * 4000) : 0;
    return { input, output, cache_read: cacheRead, cache_write: cacheWrite, reasoning: Math.round(output * rnd() * 0.4) };
  }

  /** Called for every request the mock gateway records. */
  function recordGateway(rec: {
    timestamp: string;
    model: string;
    connection_id: string;
    connection_name: string;
    status: number;
    latency_ms: number;
    transport: string;
    route?: string | null;
    attempts?: { model: string; error: string | null; connection_id: string }[];
    failovers?: number;
    ttfb_ms?: number | null;
    first_token_ms?: number | null;
    client_key_id?: string | null;
  }) {
    const c = deps.connections().find((x) => x.id === rec.connection_id);
    const kind = providerOf(c);
    const ok = rec.status < 400;
    const missing = Math.random() < 0.05; // some upstreams don't report usage
    const t = ok && !missing ? tokensFor(kind) : null;
    const upstreamModel = rec.attempts?.at(-1)?.model ?? rec.model;
    const keys = deps.keys();
    const client = rec.client_key_id ?? (Math.random() < 0.15 ? 'playground' : (keys[Math.floor(Math.random() * keys.length)]?.id ?? null));
    push({
      at: Date.parse(rec.timestamp),
      origin: 'gateway',
      connection_id: rec.connection_id,
      connection_name: rec.connection_name,
      source_id: null,
      account_label: null,
      provider: kind,
      model: upstreamModel,
      billing: billingOf(c),
      client_key_id: client,
      client_key_name: client === 'playground' ? 'Playground' : (keys.find((k) => k.id === client)?.name ?? null),
      outcome: rec.status === 499 ? 'cancelled' : ok ? 'succeeded' : 'failed',
      input: t?.input ?? null,
      cache_read: t?.cache_read ?? null,
      cache_write: t ? t.cache_write : null,
      output: t?.output ?? null,
      reasoning: t?.reasoning ?? null,
      latency_ms: rec.latency_ms,
      ttfb_ms: rec.ttfb_ms ?? null,
      first_token_ms: rec.first_token_ms ?? null,
      streamed: rec.transport !== 'http',
      attempts: rec.attempts?.length ?? 1,
      failed_attempts: (rec.attempts ?? []).filter((a) => a.error).length,
      failovers: rec.failovers ?? 0,
    });
  }

  /** 21 days of ledger history, so the 30-day window shows partial coverage. */
  function seedHistory() {
    const conns = deps.connections();
    const keys = deps.keys();
    let s = 42;
    const rnd = () => ((s = (s * 1103515245 + 12345) % 2 ** 31) / 2 ** 31);
    const start = Date.now() - 21 * DAY;
    for (let i = 0; i < 1400; i++) {
      const at = start + rnd() * (21 * DAY - HOUR);
      const c = conns[Math.floor(rnd() * conns.length)];
      if (!c) break;
      const model = c.models[Math.floor(rnd() * c.models.length)];
      const ok = rnd() > 0.06;
      const missing = rnd() < 0.04;
      const t = ok && !missing ? tokensFor(c.kind, rnd) : null;
      const k = rnd() < 0.2 ? null : keys[Math.floor(rnd() * keys.length)];
      const lat = Math.round(400 + rnd() * 9000);
      const ttfb = ok ? Math.round(150 + rnd() * 800) : null;
      push({
        at,
        origin: 'gateway',
        connection_id: c.id,
        connection_name: c.name,
        source_id: null,
        account_label: null,
        provider: c.kind,
        model,
        billing: billingOf(c),
        client_key_id: k?.id ?? 'playground',
        client_key_name: k?.name ?? 'Playground',
        outcome: ok ? 'succeeded' : 'failed',
        input: t?.input ?? null,
        cache_read: t?.cache_read ?? null,
        cache_write: t ? t.cache_write : null,
        output: t?.output ?? null,
        reasoning: t?.reasoning ?? null,
        latency_ms: lat,
        ttfb_ms: ttfb,
        first_token_ms: ttfb !== null ? ttfb + Math.round(50 + rnd() * 500) : null,
        streamed: rnd() > 0.2,
        attempts: 1,
        failed_attempts: ok ? 0 : 1,
        failovers: 0,
      });
    }
    // A local model that was used before it was disabled: real tokens, no price.
    const local = conns.find((c) => c.kind === 'openai' && c.source === 'api_key');
    if (local) {
      for (let i = 0; i < 24; i++) {
        const t = tokensFor('openai', rnd);
        push({
          at: Date.now() - (1 + rnd() * 6) * DAY,
          origin: 'gateway',
          connection_id: local.id,
          connection_name: local.name,
          source_id: null,
          account_label: null,
          provider: 'openai',
          model: local.models[0],
          billing: 'api_key',
          client_key_id: keys[0]?.id ?? null,
          client_key_name: keys[0]?.name ?? null,
          outcome: 'succeeded',
          ...t,
          cache_read: 0,
          cache_write: 0,
          latency_ms: Math.round(900 + rnd() * 4000),
          ttfb_ms: 300,
          first_token_ms: 420,
          streamed: true,
          attempts: 1,
          failed_attempts: 0,
          failovers: 0,
        });
      }
    }
  }

  /** App-reported history appears only after the user imports it. */
  function importExternal(provider: string) {
    if (externalImported.has(provider)) return 0;
    externalImported.add(provider);
    const meta: Record<string, { label: string; client: string; clientName: string; model: string; provider: string }> = {
      opencode: { label: 'OpenCode on this machine', client: 'external:opencode', clientName: 'OpenCode', model: 'gpt-6-astra', provider: 'opencode' },
      codex: { label: 'Codex CLI on this machine', client: 'external:codex_cli', clientName: 'Codex CLI', model: 'gpt-6.1-sol', provider: 'codex' },
      claude: { label: 'Claude Code on this machine', client: 'external:claude_code', clientName: 'Claude Code', model: 'claude-sonnet-5-5', provider: 'anthropic' },
      // Cursor's own model has no public list price, so it stays unpriced.
      cursor: { label: 'Cursor account', client: 'external:cursor', clientName: 'Cursor', model: 'composer-2', provider: 'cursor' },
    };
    const mt = meta[provider];
    if (!mt) return 0;
    const { label, client, clientName, model } = mt;
    let s = provider.length * 97;
    const rnd = () => ((s = (s * 1103515245 + 12345) % 2 ** 31) / 2 ** 31);
    for (let i = 0; i < 160; i++) {
      const t = tokensFor(provider === 'claude' ? 'anthropic' : 'openai', rnd);
      push({
        at: Date.now() - rnd() * 12 * DAY,
        origin: 'external',
        connection_id: null,
        connection_name: null,
        source_id: `native:${provider}`,
        account_label: label,
        provider: mt.provider,
        model,
        // App logs don't prove which login paid or whether the request succeeded.
        billing: 'unknown',
        client_key_id: client,
        client_key_name: clientName,
        outcome: 'unknown',
        input: t.input,
        cache_read: t.cache_read,
        // Codex CLI logs don't report cache writes.
        cache_write: provider === 'codex' ? null : t.cache_write,
        output: t.output,
        reasoning: t.reasoning,
        latency_ms: 0,
        ttfb_ms: null,
        first_token_ms: null,
        streamed: false,
        // Like the gateway: app logs carry no attempt data.
        attempts: 0,
        failed_attempts: 0,
        failovers: 0,
      });
    }
    return 160;
  }

  /* ---------- aggregation ---------- */

  function metrics(list: UsageEvent[]) {
    const sum = (f: (e: UsageEvent) => number | null) => list.reduce((s, e) => s + (f(e) ?? 0), 0);
    const known = (f: (e: UsageEvent) => number | null) => list.filter((e) => f(e) !== null).length;
    const succeeded = list.filter((e) => e.outcome === 'succeeded').length;
    const failed = list.filter((e) => e.outcome === 'failed').length;
    const cancelled = list.filter((e) => e.outcome === 'cancelled').length;
    const unknownOutcome = list.length - succeeded - failed - cancelled;
    const priced = list.filter((e) => e.cost_micros !== null);
    const est = priced.reduce((s, e) => s + (e.cost_micros ?? 0), 0);
    const pricedBy = (b: UsageEvent['billing']) => priced.filter((e) => e.billing === b);
    const sumCost = (es: UsageEvent[]) => es.reduce((s, e) => s + (e.cost_micros ?? 0), 0);
    const api = pricedBy('api_key');
    const sub = pricedBy('subscription');
    const unknownBilling = pricedBy('unknown');
    const cacheEligible = list.filter((e) => e.input !== null && e.cache_read !== null && e.cache_write !== null);
    const denom = cacheEligible.reduce((s, e) => s + (e.input ?? 0) + (e.cache_read ?? 0) + (e.cache_write ?? 0), 0);
    const lat = list.filter((e) => e.latency_ms > 0);
    const ft = list.filter((e) => e.first_token_ms !== null);
    const tp = list.filter((e) => e.streamed && e.output !== null && e.first_token_ms !== null && e.latency_ms > (e.first_token_ms ?? 0));
    const genSec = tp.reduce((s, e) => s + (e.latency_ms - (e.first_token_ms ?? 0)) / 1000, 0);
    const input = sum((e) => e.input);
    const cr = sum((e) => e.cache_read);
    const cw = sum((e) => e.cache_write);
    const out = sum((e) => e.output);
    const reportedUnits = list.filter((e) => e.input !== null || e.output !== null).length;
    return {
      units: { total: list.length, succeeded, failed, cancelled, ...(unknownOutcome ? { unknown: unknownOutcome } : {}) },
      success_rate: succeeded + failed ? succeeded / (succeeded + failed) : null,
      attempts: { total: sum((e) => e.attempts), failed: sum((e) => e.failed_attempts), failovers: sum((e) => e.failovers) },
      tokens: {
        input,
        cache_read: cr,
        cache_write: cw,
        cache_write_5m: cw,
        cache_write_1h: 0,
        output: out,
        reasoning: sum((e) => e.reasoning),
        total: input + cr + cw + out,
        known_units: {
          input: known((e) => e.input),
          cache_read: known((e) => e.cache_read),
          cache_write: known((e) => e.cache_write),
          output: known((e) => e.output),
          reasoning: known((e) => e.reasoning),
        },
        usage_reported_units: reportedUnits,
        usage_missing_units: list.length - reportedUnits,
      },
      cache: { read_ratio: denom ? cacheEligible.reduce((s, e) => s + (e.cache_read ?? 0), 0) / denom : null, eligible_units: cacheEligible.length },
      cost: {
        estimated_micros: est,
        estimated_usd: (est / 1e6).toFixed(6),
        api_estimated_micros: api.length ? sumCost(api) : null,
        subscription_equivalent_micros: sub.length ? sumCost(sub) : null,
        unknown_billing_micros: unknownBilling.length ? sumCost(unknownBilling) : null,
        unknown_billing_units: unknownBilling.length,
        reported_micros: null,
        reported_usd: null,
        priced_units: priced.length,
        // Requests with token usage but no price. Requests that reported no usage are neither.
        unpriced_units: list.filter((e) => e.cost_micros === null && e.input !== null && e.output !== null).length,
      },
      latency_ms: { avg: lat.length ? Math.round(lat.reduce((s, e) => s + e.latency_ms, 0) / lat.length) : null, max: lat.length ? Math.max(...lat.map((e) => e.latency_ms)) : null, samples: lat.length },
      ttfb_ms: { avg: null, samples: 0 },
      first_token_ms: { avg: ft.length ? Math.round(ft.reduce((s, e) => s + (e.first_token_ms ?? 0), 0) / ft.length) : null, samples: ft.length },
      throughput: { output_tokens_per_second: genSec > 0 ? tp.reduce((s, e) => s + (e.output ?? 0), 0) / genSec : null, samples: tp.length },
    };
  }

  function groupBy<K extends string>(list: UsageEvent[], key: (e: UsageEvent) => K) {
    const m = new Map<K, UsageEvent[]>();
    for (const e of list) m.set(key(e), [...(m.get(key(e)) ?? []), e]);
    return [...m.entries()];
  }

  const byCost = (a: { metrics: ReturnType<typeof metrics> }, b: { metrics: ReturnType<typeof metrics> }) =>
    (b.metrics.cost.estimated_micros ?? 0) - (a.metrics.cost.estimated_micros ?? 0) || b.metrics.units.total - a.metrics.units.total;

  function usage(q: URLSearchParams) {
    const win = q.get('window') ?? '24h';
    const source = q.get('source') ?? 'gateway';
    if (!['24h', '7d', '30d', 'all'].includes(win) || !['gateway', 'external', 'all'].includes(source)) return { status: 400, body: { error: { message: 'Unsupported window or source', type: 'gateway_error' } } };
    const to = Date.now();
    const span = win === '24h' ? DAY : win === '7d' ? 7 * DAY : win === '30d' ? 30 * DAY : null;
    const earliest = events.length ? Math.min(...events.filter((e) => source === 'all' || e.origin === source).map((e) => e.at), to) : null;
    const from = span ? to - span : earliest;
    const gran = win === '24h' ? 'hour' : win === 'all' ? 'month' : 'day';
    const inWindow = events.filter((e) => (from === null || e.at >= from) && e.at <= to && (source === 'all' || e.origin === source));
    const f = {
      connection_id: q.get('connection_id'),
      provider: q.get('provider'),
      model: q.get('model'),
      client_key_id: q.get('client_key_id'),
    };
    const list = inWindow.filter(
      (e) => (!f.connection_id || e.connection_id === f.connection_id) && (!f.provider || e.provider === f.provider) && (!f.model || e.model === f.model) && (!f.client_key_id || e.client_key_id === f.client_key_id),
    );
    // Buckets.
    const step = gran === 'hour' ? HOUR : gran === 'day' ? DAY : 30 * DAY;
    // Like the real ledger: gateway and app reports of unknown overlap are never added up.
    const split = source === 'all' && list.some((e) => e.origin === 'gateway') && list.some((e) => e.origin === 'external');
    const src = (e: UsageEvent) => (split ? e.origin : null);
    const sideMetrics = (es: UsageEvent[]) => ({ gateway: metrics(es.filter((e) => e.origin === 'gateway')), external: metrics(es.filter((e) => e.origin === 'external')) });
    const series: { bucket_start: string; metrics: ReturnType<typeof metrics> | null; by_source: ReturnType<typeof sideMetrics> | null }[] = [];
    if (from !== null) {
      const first = Math.floor(from / step) * step;
      const n = Math.min(gran === 'month' ? 120 : gran === 'hour' ? 24 : 30, Math.ceil((to - first) / step));
      for (let i = Math.max(0, Math.ceil((to - first) / step) - n); i < Math.ceil((to - first) / step); i++) {
        const b0 = first + i * step;
        const inBucket = list.filter((e) => e.at >= b0 && e.at < b0 + step);
        series.push({ bucket_start: new Date(b0).toISOString(), metrics: split ? null : metrics(inBucket), by_source: split ? sideMetrics(inBucket) : null });
      }
    }
    const complete = from === null || ledgerStartedAt === null || source === 'external' ? true : from >= ledgerStartedAt;
    const accountsFacet = groupBy(inWindow.filter((e) => e.connection_id), (e) => e.connection_id as string).map(([id, es]) => ({ connection_id: id, name: es[0].connection_name ?? id, provider: es[0].provider }));
    return {
      status: 200,
      body: {
        generated_at: new Date(to).toISOString(),
        window: { key: win, from: from === null ? null : new Date(from).toISOString(), to: new Date(to).toISOString(), granularity: gran, timezone: 'UTC' },
        filters: { ...f, source },
        coverage: {
          ledger_started_at: ledgerStartedAt === null ? null : new Date(ledgerStartedAt).toISOString(),
          complete_for_window: complete,
          message: complete ? null : `Switchyard started counting usage ${Math.round((to - (ledgerStartedAt ?? to)) / DAY)} days ago, so the start of this window has no data. Nothing before that is estimated.`,
          excluded_legacy_requests: 0,
          overlap: split ? 'possible' : 'none',
        },
        totals: split ? null : metrics(list),
        combined: !split,
        by_source: split
          ? [
              { source: 'gateway', metrics: sideMetrics(list).gateway },
              { source: 'external', metrics: sideMetrics(list).external },
            ]
          : undefined,
        series,
        by_account: groupBy(list, (e) => `${src(e)}\u0000${e.connection_id ?? e.source_id ?? 'unknown'}`)
          .map(([, es]) => ({
            connection_id: es[0].connection_id,
            connection_name: es[0].connection_name,
            provider: es[0].provider,
            billing: es[0].billing,
            origin: es[0].origin,
            source: src(es[0]),
            source_id: es[0].source_id,
            account_label: es[0].account_label,
            metrics: metrics(es),
          }))
          .sort(byCost),
        by_provider: groupBy(list, (e) => `${src(e)}\u0000${e.provider}`)
          .map(([, es]) => ({ provider: es[0].provider, source: src(es[0]), metrics: metrics(es) }))
          .sort(byCost),
        by_model: groupBy(list, (e) => `${src(e)}\u0000${e.provider}\u0000${e.model}`)
          .map(([, es]) => ({ model: es[0].model, provider: es[0].provider, priced: rateFor(es[0].model) !== null, source: src(es[0]), metrics: metrics(es) }))
          .sort(byCost),
        by_client: groupBy(list, (e) => `${src(e)}\u0000${e.client_key_id ?? 'none'}`)
          .map(([, es]) => ({ client_key_id: es[0].client_key_id, client_key_name: es[0].client_key_name, origin: es[0].origin, source: src(es[0]), metrics: metrics(es) }))
          .sort(byCost),
        warnings: split ? ['Apps may report requests that also went through Switchyard. Totals are shown side by side rather than added up.'] : [],
        pricing: { version: 'mock-fixture', as_of: new Date().toISOString().slice(0, 10), overrides: overrides.length },
        facets: {
          accounts: accountsFacet,
          providers: [...new Set(inWindow.map((e) => e.provider))],
          models: [...new Set(inWindow.map((e) => e.model))],
          clients: groupBy(inWindow.filter((e) => e.client_key_id), (e) => e.client_key_id as string).map(([id, es]) => ({ client_key_id: id, name: es[0].client_key_name ?? id })),
        },
      },
    };
  }

  function pricing() {
    const unpricedModels = groupBy(events.filter((e) => e.cost_micros === null && e.input !== null && rateFor(e.model) === null), (e) => e.model).map(([m, es]) => ({
      model: m,
      provider: es[0].provider,
      units: es.length,
      last_seen_day: new Date(Math.max(...es.map((e) => e.at))).toISOString().slice(0, 10),
    }));
    const nextYear = `${new Date().getUTCFullYear() + 1}-01-01`;
    return {
      version: 'mock-fixture',
      as_of: new Date().toISOString().slice(0, 10),
      sources: [
        { provider: 'openai', url: 'https://example.invalid/mock-openai-prices', retrieved: new Date().toISOString().slice(0, 10) },
        { provider: 'anthropic', url: 'https://example.invalid/mock-anthropic-prices', retrieved: new Date().toISOString().slice(0, 10) },
      ],
      rates: [
        ...FIXTURE_RATES.map((x) => ({
          model: x.model,
          provider: x.provider,
          origin: 'official',
          usd_per_mtok: { ...x.r, cache_write: x.r.cache_write_5m },
          long_context: x.long ? { above_input_tokens: x.long.above, usd_per_mtok: { input: x.long.input, output: x.long.output } } : null,
          note: 'Mock fixture price for development',
        })),
        ...overrides.map((o) => ({ model: o.model, provider: 'custom', origin: 'override', usd_per_mtok: o.r, long_context: null, note: null })),
      ],
      unpriced_models: unpricedModels,
      scheduled: [
        {
          model: 'gemini-3-pro',
          provider: 'gemini',
          origin: 'official',
          usd_per_mtok: { input: '2.50', output: '14.00', cache_read: '0.25', cache_write: null, cache_write_5m: null, cache_write_1h: null },
          long_context: null,
          effective_from: nextYear,
          note: 'Mock fixture price for development',
        },
      ],
      scope: 'Mock fixture. Estimates use list prices per token and leave out batch discounts, priority tiers, tool and search fees, and audio or image inputs.',
    };
  }

  /* ---------- sources ---------- */

  /** API-key connections: no plan to read, like the real gateway_only sources. */
  function apiKeySources() {
    return deps
      .connections()
      .filter((c) => c.kind !== 'codex' && c.kind !== 'anthropic' && c.kind !== 'antigravity')
      .map((c) => ({
        id: `connection:${c.id}`,
        connection_id: c.id,
        connection_name: c.name,
        name: c.name,
        provider: c.kind,
        status: 'unavailable',
        updated_at: null,
        next_refresh_at: null,
        last_success_at: null,
        last_error_at: null,
        message: 'API keys cannot read subscription quota or account billing. Gateway token and cost metrics for this account appear in Usage.',
        windows: [],
        balances: [],
        reported_costs: [],
        capabilities: { quota: false, cost: false, tokens: true, history: false },
        capability_notes: [],
        source: 'gateway_only',
        refreshing: false,
        plan: null,
      }));
  }

  function connSources() {
    const now = Date.now();
    return deps
      .connections()
      .filter((c) => c.kind === 'codex' || c.kind === 'anthropic' || c.kind === 'antigravity')
      .map((c, i) => {
        const id = `connection:${c.id}`;
        const st = sourceState.get(id) ?? { updatedAt: now - (2 + i) * 60_000 };
        const refreshing = !!st.refreshingUntil && st.refreshingUntil > now;
        const stale = c.source === 'native_codex';
        const base = {
          id,
          connection_id: c.id,
          connection_name: c.name,
          name: c.name,
          provider: c.kind === 'anthropic' ? 'claude' : c.kind,
          status: !c.enabled ? 'disabled' : refreshing ? 'refreshing' : stale ? 'stale' : 'ok',
          updated_at: new Date(stale ? now - 47 * 60_000 : st.updatedAt).toISOString(),
          next_refresh_at: new Date(now + 3 * 60_000).toISOString(),
          last_error_at: stale ? new Date(now - 4 * 60_000).toISOString() : null,
          message: stale ? 'Couldn’t reach the provider on the last refresh. Showing the last good numbers.' : null,
          balances: [] as unknown[],
          reported_costs: [] as unknown[],
          source: 'provider_api',
          refreshing,
        };
        if (c.kind === 'codex') {
          return {
            ...base,
            windows: [
              { id: 'primary', label: '5-hour limit', unit: 'percent', used: stale ? 64 : 23 + i * 9, reset_at: new Date(now + 2.4 * HOUR).toISOString(), scope: 'account' },
              { id: 'secondary', label: 'Weekly limit', unit: 'percent', used: stale ? 81 : 41 + i * 12, reset_at: new Date(now + 3.6 * DAY).toISOString(), scope: 'account' },
            ],
            balances: [{ label: 'Credits', unit: 'credits', currency: null, value: 120 }],
            capabilities: { quota: true, cost: true, tokens: false, history: false },
          };
        }
        if (c.kind === 'antigravity') {
          return {
            ...base,
            windows: [
              { id: 'm1', label: 'Gemini 3 Pro', unit: 'percent', remaining: 72, reset_at: new Date(now + 4 * HOUR).toISOString(), model: null, scope: 'model' },
              { id: 'm2', label: 'Claude Sonnet 5.5', unit: 'percent', remaining: 9, reset_at: new Date(now + 1.5 * HOUR).toISOString(), scope: 'model' },
            ],
            capabilities: { quota: true, cost: false, tokens: false, history: false },
          };
        }
        return {
          ...base,
          windows: [
            { id: 'session', label: 'Current session', unit: 'percent', used: 18, reset_at: new Date(now + 3.2 * HOUR).toISOString(), scope: 'account' },
            { id: 'weekly', label: 'Weekly, all models', unit: 'percent', used: 57, reset_at: new Date(now + 4.5 * DAY).toISOString(), scope: 'account' },
            { id: 'weekly-opus', label: 'Weekly', unit: 'percent', used: 92, reset_at: new Date(now + 4.5 * DAY).toISOString(), model: 'Opus', scope: 'model' },
          ],
          balances: [],
          reported_costs: [{ label: 'Extra usage this month', currency: 'USD', amount: 3.4, period_start: new Date(now - 12 * DAY).toISOString(), period_end: new Date(now + 18 * DAY).toISOString(), kind: 'on_demand' }],
          capabilities: { quota: true, cost: true, tokens: false, history: false },
        };
      });
  }

  function monitorSource(m: Record<string, unknown>) {
    const now = Date.now();
    const id = `monitor:${m.id}`;
    const st = sourceState.get(id) ?? { updatedAt: now - 90_000 };
    const refreshing = !!st.refreshingUntil && st.refreshingUntil > now;
    const provider = String(m.provider);
    const needsAuth = !m.credential && m.credential_source !== 'native';
    const base = {
      id,
      connection_id: (m.connection_id as string) ?? null,
      connection_name: null,
      name: String(m.name),
      provider,
      status: !m.enabled ? 'disabled' : refreshing ? 'refreshing' : needsAuth ? 'needs_auth' : (st.status ?? 'ok'),
      updated_at: needsAuth ? null : new Date(st.updatedAt).toISOString(),
      next_refresh_at: m.enabled ? new Date(now + 4 * 60_000).toISOString() : null,
      message: needsAuth ? 'The session cookie was rejected. Paste a fresh one.' : (st.message ?? null),
      windows: [] as unknown[],
      balances: [] as unknown[],
      reported_costs: [] as unknown[],
      capabilities: { quota: false, cost: false, tokens: false, history: false },
      source: m.credential_source === 'native' ? 'native_file' : 'provider_api',
      refreshing,
    };
    if (needsAuth) return { ...base, capabilities: { quota: true, cost: true, tokens: false, history: true } };
    switch (provider) {
      case 'cursor':
        return {
          ...base,
          windows: [
            { id: 'included', label: 'Included usage', unit: 'percent', used: 62.4, reset_at: new Date(now + 11 * DAY).toISOString(), scope: 'account' },
            { id: 'on_demand', label: 'On-demand spend', unit: 'usd', used: 12.4, limit: 50, reset_at: new Date(now + 11 * DAY).toISOString(), scope: 'account' },
          ],
          reported_costs: [
            { label: 'Pro plan', currency: 'USD', amount: 20, period_start: new Date(now - 19 * DAY).toISOString(), period_end: new Date(now + 11 * DAY).toISOString(), kind: 'subscription' },
            { label: 'Included usage consumed', currency: 'USD', amount: 12.48, period_start: new Date(now - 19 * DAY).toISOString(), period_end: new Date(now + 11 * DAY).toISOString(), kind: 'included' },
            { label: 'On-demand charges', currency: 'USD', amount: 12.4, period_start: new Date(now - 19 * DAY).toISOString(), period_end: new Date(now + 11 * DAY).toISOString(), kind: 'on_demand' },
          ],
          capabilities: { quota: true, cost: true, tokens: false, history: true },
          capability_notes: ['Cursor’s own models (like Composer) aren’t offered as an API, so Switchyard can watch this account’s usage but can’t route traffic to it.'],
        };
      case 'opencode_go':
        return {
          ...base,
          windows: [
            { id: 'rolling', label: '5-hour', unit: 'percent', used: 18, reset_at: new Date(now + 3.1 * HOUR).toISOString() },
            { id: 'weekly', label: 'Weekly', unit: 'percent', used: 43, reset_at: new Date(now + 2.2 * DAY).toISOString() },
            { id: 'monthly', label: 'Monthly', unit: 'percent', used: 61, reset_at: new Date(now + 17 * DAY).toISOString() },
          ],
          capabilities: { quota: true, cost: false, tokens: false, history: false },
        };
      case 'opencode':
        return { ...base, balances: [{ label: 'Zen balance', unit: 'usd', currency: 'USD', value: 18.25 }], capabilities: { quota: false, cost: true, tokens: false, history: false } };
      case 'codex':
      case 'claude':
        return {
          ...base,
          windows: [{ id: 'weekly', label: 'Weekly limit', unit: 'percent', used: 34, reset_at: new Date(now + 5 * DAY).toISOString() }],
          capabilities: { quota: true, cost: false, tokens: true, history: true },
        };
      default:
        return { ...base, status: base.status === 'ok' ? 'unavailable' : base.status, message: 'This provider didn’t return usage for this account.', updated_at: null };
    }
  }

  function sources() {
    const list = [...connSources(), ...apiKeySources(), ...[...monitors.values()].map(monitorSource)];
    return { sources: list, generated_at: new Date().toISOString(), refreshing: list.some((s) => s.refreshing) };
  }

  function publicMonitor(m: Record<string, unknown>) {
    const { credential: _c, ...rest } = m;
    return { ...rest, credential_present: m.credential_source === 'native' ? true : !!_c };
  }

  const PROVIDERS = ['cursor', 'opencode', 'opencode_go', 'codex', 'claude', 'antigravity', 'openai', 'anthropic'];
  const CRED = ['native', 'api_key', 'cookie', 'file'];

  function validateMonitor(b: Record<string, unknown> | null): string | null {
    if (!b) return 'Invalid JSON body';
    if (typeof b.name !== 'string' || !b.name.trim() || b.name.length > 100) return 'name must be 1-100 characters';
    if (!PROVIDERS.includes(String(b.provider))) return 'provider is not supported';
    if (!CRED.includes(String(b.credential_source))) return 'credential_source must be native, api_key, cookie or file';
    return null;
  }

  async function handle(path: string, method: string, url: URL, body: Record<string, unknown> | null): Promise<{ status: number; body?: unknown } | null> {
    if (path === '/api/usage' && method === 'GET') return usage(url.searchParams);
    if (path === '/api/usage/pricing' && method === 'GET') return { status: 200, body: pricing() };
    if (path === '/api/usage/pricing/overrides' && method === 'PUT') {
      const list = Array.isArray(body?.overrides) ? (body!.overrides as { model: string; usd_per_mtok: Rates }[]) : null;
      if (!list || list.length > 200) return { status: 400, body: { error: { message: 'overrides must be a list of at most 200 entries', type: 'gateway_error' } } };
      for (const o of list) {
        if (!o.model || o.model.length > 200) return { status: 400, body: { error: { message: 'Each override needs a model name of 1-200 characters', type: 'gateway_error' } } };
        for (const [k, v] of Object.entries(o.usd_per_mtok ?? {})) {
          if (v !== null && !/^\d+(\.\d{1,6})?$/.test(String(v))) return { status: 400, body: { error: { message: `${o.model}: ${k} must be a non-negative decimal`, type: 'gateway_error' } } };
        }
      }
      overrides = list.map((o) => ({ model: o.model, r: { ...NO_RATES, ...o.usd_per_mtok } }));
      return { status: 200, body: pricing() };
    }
    if (path === '/api/usage/sources' && method === 'GET') return { status: 200, body: sources() };
    if (path === '/api/usage/refresh' && method === 'POST') {
      const ids = body?.id ? [String(body.id)] : sources().sources.map((s) => s.id);
      for (const id of ids) sourceState.set(id, { ...(sourceState.get(id) ?? { updatedAt: Date.now() }), refreshingUntil: Date.now() + 1500, updatedAt: Date.now() + 1500 });
      setTimeout(() => deps.broadcast?.(), 1600);
      return { status: 202, body: { accepted: true, message: 'Refresh queued; poll sources for results.' } };
    }
    if (path === '/api/usage/monitors' && method === 'GET') return { status: 200, body: [...monitors.values()].map(publicMonitor) };
    if (path === '/api/usage/monitors' && method === 'POST') {
      const err = validateMonitor(body);
      if (err) return { status: 400, body: { error: { message: err, type: 'gateway_error' } } };
      const m = { id: randomUUID(), name: body!.name, provider: body!.provider, credential_source: body!.credential_source, source_path: body!.source_path ?? null, connection_id: body!.connection_id ?? null, enabled: body!.enabled ?? true, credential: body!.credential ?? null, created_at: new Date().toISOString() };
      monitors.set(m.id, m);
      return { status: 200, body: publicMonitor(m) };
    }
    const mm = path.match(/^\/api\/usage\/monitors\/([^/]+)$/);
    if (mm) {
      const m = monitors.get(decodeURIComponent(mm[1]));
      if (!m) return { status: 404, body: { error: { message: 'Monitor not found', type: 'gateway_error' } } };
      if (method === 'DELETE') {
        monitors.delete(m.id as string);
        return { status: 204 };
      }
      if (method === 'PUT') {
        const err = validateMonitor(body);
        if (err) return { status: 400, body: { error: { message: err, type: 'gateway_error' } } };
        Object.assign(m, { name: body!.name, provider: body!.provider, credential_source: body!.credential_source, connection_id: body!.connection_id ?? null, enabled: body!.enabled ?? true });
        if (typeof body!.credential === 'string') m.credential = body!.credential || null;
        return { status: 200, body: publicMonitor(m) };
      }
    }
    if (path === '/api/usage/native' && method === 'GET') return { status: 200, body: nativeState() };
    if (path === '/api/usage/native/import' && method === 'POST') {
      const source = String(body?.source ?? '');
      if (!nativeIds().includes(source)) return { status: 400, body: { error: { message: 'source must be opencode, codex, claude or cursor:<monitor id>', type: 'gateway_error' } } };
      nativeJobs.set(source, { startedAt: Date.now(), done: false, updatedAt: Date.now() });
      // Re-import is idempotent: clear the guard so an update can re-add (dedup by source keeps it single).
      const provider = source.split(':')[0];
      if (externalImported.has(provider)) {
        events = events.filter((e) => e.source_id !== `native:${provider}`);
        externalImported.delete(provider);
      }
      return { status: 202, body: { accepted: true, message: 'Import started. Native files are read-only; poll /api/usage/native for progress.' } };
    }
    const nd = decodeURIComponent(path).match(/^\/api\/usage\/native\/([a-z_]+(?::[\w-]+)?)$/);
    if (nd && method === 'DELETE') {
      if (!nativeJobs.has(nd[1])) return { status: 404, body: { error: { message: 'Native source not imported', type: 'gateway_error' } } };
      const provider = nd[1].split(':')[0];
      events = events.filter((e) => e.source_id !== `native:${provider}`);
      externalImported.delete(provider);
      nativeJobs.delete(nd[1]);
      return { status: 204 };
    }
    if (path === '/api/usage/import' && method === 'POST') {
      const provider = String(body?.provider ?? '');
      if (!['cursor', 'opencode', 'opencode_go', 'codex', 'claude', 'antigravity'].includes(provider)) return { status: 400, body: { error: { message: 'provider is not importable', type: 'gateway_error' } } };
      if ([...monitors.values()].some((m) => m.provider === provider && m.credential_source === 'native')) {
        return { status: 200, body: { imported: 0, monitors: [], message: 'Import read-only; native accounts stay owned by their app.', skipped: [{ reason: 'already_monitored', message: 'Already watching this account.' }] } };
      }
      const label = { cursor: 'Cursor', opencode: 'OpenCode Zen', opencode_go: 'OpenCode Go', codex: 'Codex CLI', claude: 'Claude Code', antigravity: 'Antigravity' }[provider];
      const m = { id: randomUUID(), name: `${label} on this machine`, provider, credential_source: 'native', source_path: null, connection_id: null, enabled: true, credential: null, created_at: new Date().toISOString() };
      monitors.set(m.id, m);
      return { status: 200, body: { imported: 1, monitors: [publicMonitor(m)], message: 'Import read-only; native accounts stay owned by their app.' } };
    }
    return null;
  }

  return {
    handle,
    recordGateway,
    seedHistory,
    /** Make one watched account's credential fail (dev panel + tests). */
    breakMonitor() {
      const m = [...monitors.values()].find((x) => x.credential_source !== 'native');
      if (m) m.credential = null;
    },
    reset() {
      events = [];
      ledgerStartedAt = null;
      overrides = [];
      monitors.clear();
      sourceState.clear();
      externalImported = new Set();
      nativeJobs.clear();
    },
  };
}
