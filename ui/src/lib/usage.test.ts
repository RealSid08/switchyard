import { describe, expect, it } from 'vitest';
import {
  normalizeNative,
  planLabel,
  quotaWindows,
  MONITOR_PROVIDERS,
  activeFilterCount,
  clientLabel,
  costParts,
  costView,
  outcomeView,
  fmtMicros,
  fmtRatio,
  fmtTokens,
  freshness,
  knowledge,
  providerKind,
  providerLabel,
  quotaMeter,
  rateError,
  resetLabel,
  tightestWindow,
  tokenKnowledge,
  usageQueryFromSearch,
  usageQueryToSearch,
  worstKnowledge,
} from './usage';
import type { QuotaWindow, UsageMetrics, UsageSource } from './usageTypes';

const metrics = (p: { total?: number; known?: Partial<Record<'input' | 'cache_read' | 'cache_write' | 'output' | 'reasoning', number>>; cost?: Partial<UsageMetrics['cost']> } = {}): UsageMetrics => {
  const total = p.total ?? 10;
  const k = { input: total, cache_read: total, cache_write: total, output: total, reasoning: total, ...p.known };
  return {
    units: { total, succeeded: total, failed: 0, cancelled: 0 },
    success_rate: 1,
    tokens: { input: 100, cache_read: 50, cache_write: 10, output: 40, reasoning: 5, total: 200, known_units: k },
    cost: { estimated_micros: 1_500_000, priced_units: total, unpriced_units: 0, ...p.cost },
  };
};

describe('knowledge (unknown is never zero)', () => {
  it('distinguishes known, partial and unknown per dimension', () => {
    const m = metrics({ known: { input: 10, cache_write: 0, output: 4 } });
    expect(knowledge(m, 'input')).toBe('known');
    expect(knowledge(m, 'cache_write')).toBe('unknown');
    expect(knowledge(m, 'output')).toBe('partial');
    expect(knowledge(metrics({ total: 0 }), 'input')).toBe('unknown');
  });
  it('combines dimensions honestly', () => {
    expect(tokenKnowledge(metrics())).toBe('known');
    expect(tokenKnowledge(metrics({ known: { input: 0, cache_read: 0, cache_write: 0, output: 0 } }))).toBe('unknown');
    expect(tokenKnowledge(metrics({ known: { cache_write: 0 } }))).toBe('partial');
    expect(worstKnowledge(['known', 'unknown'])).toBe('partial');
  });
});

describe('formatting', () => {
  it('formats tokens compactly', () => {
    expect(fmtTokens(9999)).toBe('9,999');
    expect(fmtTokens(12_345)).toBe('12.3K');
    expect(fmtTokens(9_740_000)).toBe('9.74M');
    expect(fmtTokens(14_600_000)).toBe('14.6M');
    expect(fmtTokens(null)).toBe('–');
  });
  it('formats micro-USD without inventing $0', () => {
    expect(fmtMicros(null)).toBeNull();
    expect(fmtMicros(0)).toBe('$0.00');
    expect(fmtMicros(4_000)).toBe('<$0.01');
    expect(fmtMicros(4_000, { precise: true })).toBe('$0.0040');
    expect(fmtMicros(30_308_817)).toBe('$30.31');
    expect(fmtMicros(1_234_567_890)).toBe('$1,235');
  });
  it('formats ratios', () => {
    expect(fmtRatio(0.2811)).toBe('28%');
    expect(fmtRatio(0.051)).toBe('5.1%');
    expect(fmtRatio(0.9997)).toBe('99.9%');
    expect(fmtRatio(0.0004)).toBe('<0.1%');
    expect(fmtRatio(null)).toBe('–');
  });
});

describe('costView', () => {
  it('reports coverage and keeps billed separate from estimates', () => {
    expect(costView(metrics()).coverage).toBe('full');
    const partial = costView(metrics({ cost: { unpriced_units: 3, priced_units: 7, reported_micros: 2_000_000 } }));
    expect(partial).toMatchObject({ coverage: 'partial', unpricedUnits: 3, reported: 2_000_000, estimate: 1_500_000 });
    const none = costView(metrics({ cost: { priced_units: 0, unpriced_units: 10, estimated_micros: 0 } }));
    expect(none).toMatchObject({ coverage: 'none', estimate: null });
  });
  it('keeps unknown billing as its own part of the estimate, never API spend', () => {
    const m = metrics({ cost: { estimated_micros: 1_500_000, api_estimated_micros: 200_000, subscription_equivalent_micros: 300_000, unknown_billing_micros: 1_000_000, unknown_billing_units: 6 } });
    expect(costView(m)).toMatchObject({ estimate: 1_500_000, api: 200_000, subscription: 300_000, unknownBilling: 1_000_000, unknownBillingUnits: 6 });
    expect(costParts(m)).toEqual({ api: 200_000, subscription: 300_000, unknown: 1_000_000 });
    // No unknown-billing units: nothing shown, even if a stray amount is present.
    expect(costView(metrics({ cost: { unknown_billing_micros: null, unknown_billing_units: 0 } }))).toMatchObject({ unknownBilling: null, unknownBillingUnits: 0 });
    // Older gateways put everything in api/subscription: no invented remainder.
    expect(costParts(metrics({ cost: { estimated_micros: 900, api_estimated_micros: 600, subscription_equivalent_micros: 300 } }))).toEqual({ api: 600, subscription: 300, unknown: 0 });
  });
});

describe('outcomeView (unreported is neither success nor failure)', () => {
  const m = (u: Partial<UsageMetrics['units']>, rate: number | null): UsageMetrics => ({ ...metrics(), units: { total: 0, succeeded: 0, failed: 0, cancelled: 0, ...u }, success_rate: rate });
  it('reads app-reported units as outcome not reported, not as unfinished or successful', () => {
    expect(outcomeView(m({ total: 40, unknown: 40 }, null))).toEqual({ unknown: 40, decided: 0, rate: null, state: 'none' });
  });
  it('rates only units with an outcome when some are unknown', () => {
    expect(outcomeView(m({ total: 50, succeeded: 9, failed: 1, unknown: 40 }, 0.9))).toEqual({ unknown: 40, decided: 10, rate: 0.9, state: 'partial' });
  });
  it('keeps gateway metrics unchanged and derives unknown for older shapes', () => {
    expect(outcomeView(m({ total: 10, succeeded: 8, failed: 1, cancelled: 1 }, 8 / 9))).toMatchObject({ unknown: 0, state: 'reported', rate: 8 / 9 });
    expect(outcomeView(m({ total: 12, succeeded: 8, failed: 1, cancelled: 1 }, 8 / 9))).toMatchObject({ unknown: 2, state: 'partial' });
  });
});

describe('quotaMeter (each window stands alone)', () => {
  const w = (p: Partial<QuotaWindow>): QuotaWindow => ({ id: 'w', label: 'Weekly', unit: 'percent', ...p });
  it('reads percent windows as 0 to 100, including sub-1% values', () => {
    expect(quotaMeter(w({ used: 41 }))).toMatchObject({ pct: 41, primary: '41% used', tone: 'ok' });
    expect(quotaMeter(w({ used: 0.36 }))).toMatchObject({ pct: 0.36, primary: '0.36% used' });
    expect(quotaMeter(w({ remaining: 9 }))).toMatchObject({ pct: 91, tone: 'err' });
    expect(quotaMeter(w({ used: 80 })).tone).toBe('warn');
    expect(quotaMeter(w({ used: 140 })).pct).toBe(100);
  });
  it('handles absolute units and unknowns', () => {
    expect(quotaMeter(w({ unit: 'usd', used: 12.4, limit: 50 }))).toMatchObject({ primary: '$12.40 of $50.00', pct: 24.8 });
    expect(quotaMeter(w({ unit: 'tokens', remaining: 250_000, limit: 1_000_000 })).pct).toBe(75);
    expect(quotaMeter(w({ unit: 'requests', used: 12 }))).toMatchObject({ pct: null, primary: '12 requests used', secondary: 'No limit reported' });
    expect(quotaMeter(w({}))).toMatchObject({ pct: null, primary: 'Unknown', tone: 'muted' });
  });
  it('labels resets', () => {
    const now = Date.parse('2026-10-03T00:00:00Z');
    expect(resetLabel('2026-10-03T00:30:00Z', now)).toBe('Resets in 30 min');
    expect(resetLabel('2026-10-03T02:24:00Z', now)).toBe('Resets in 2 h 24 min');
    expect(resetLabel('2026-10-07T00:00:00Z', now)).toBe('Resets in 4 days');
    expect(resetLabel('2026-10-02T00:00:00Z', now)).toBe('Resetting now');
    expect(resetLabel(null, now)).toBeNull();
  });
  it('picks the tightest window per source without summing', () => {
    const s = { windows: [w({ id: 'a', used: 20 }), w({ id: 'b', used: 92 }), w({ id: 'c' })] } as UsageSource;
    expect(tightestWindow(s)?.window.id).toBe('b');
    expect(tightestWindow({ windows: [w({})] } as UsageSource)).toBeNull();
  });
});

describe('freshness (always visible, never claims fresh when unknown)', () => {
  const now = Date.parse('2026-10-03T12:00:00Z');
  const src = (p: Partial<UsageSource>) => ({ status: 'ok', windows: [], ...p }) as UsageSource;
  it('describes last update and next refresh', () => {
    expect(freshness(src({ updated_at: '2026-10-03T11:58:00Z', next_refresh_at: '2026-10-03T12:03:00Z' }), now)).toEqual({ text: 'Updated 2 min ago · next in 3 min', stale: false });
  });
  it('marks stale and failed attempts', () => {
    const f = freshness(src({ status: 'stale', updated_at: '2026-10-03T11:13:00Z', last_error_at: '2026-10-03T11:56:00Z' }), now);
    expect(f.stale).toBe(true);
    expect(f.text).toMatch(/Updated 47 min ago, last attempt failed 4 min ago/);
  });
  it('never invents a time', () => {
    expect(freshness(src({}), now)).toEqual({ text: 'Never refreshed', stale: true });
    expect(freshness(src({ status: 'refreshing' }), now).text).toBe('First refresh in progress');
  });
});

describe('URL state', () => {
  it('round-trips and rejects junk', () => {
    const q = { window: '30d' as const, source: 'all' as const, connection_id: 'c1', provider: 'anthropic', model: 'a/b:c', client_key_id: 'playground' };
    expect(usageQueryFromSearch(usageQueryToSearch(q))).toEqual(q);
    expect(usageQueryFromSearch('?window=1y&scope=everything')).toEqual({ window: '7d', source: 'gateway' });
    expect(usageQueryToSearch({ window: '7d', source: 'gateway' })).toBe('');
    expect(activeFilterCount(q)).toBe(4);
  });
});

describe('pricing overrides', () => {
  it('validates rates like the gateway', () => {
    expect(rateError('')).toBeNull();
    expect(rateError('1.25')).toBeNull();
    expect(rateError('0.000001')).toBeNull();
    expect(rateError('1.1234567')).toMatch(/6 decimals/);
    expect(rateError('-1')).toMatch(/number/);
    expect(rateError('$3')).toMatch(/number/);
    expect(rateError('10000.5')).toMatch(/10,000/);
  });
});

describe('provider copy and labels', () => {
  it('never claims Cursor models can be routed, and marks unsupported providers', () => {
    const cursor = MONITOR_PROVIDERS.find((p) => p.id === 'cursor')!;
    expect(cursor.notes.join(' ')).toMatch(/can’t route traffic/);
    expect(MONITOR_PROVIDERS.find((p) => p.id === 'gemini')).toMatchObject({ supported: false, methods: [] });
    for (const p of MONITOR_PROVIDERS) for (const n of [p.blurb, ...p.notes, ...p.methods.map((m) => m.help)]) expect(n).not.toContain('—');
  });
  it('maps providers and clients to readable labels', () => {
    expect(providerKind('claude')).toBe('anthropic');
    expect(providerKind('opencode_go')).toBe('opencode');
    expect(clientLabel('playground', null)).toBe('Playground');
    expect(clientLabel('external:opencode', null)).toBe('OpenCode');
    expect(clientLabel('external:codex_cli', null)).toBe('Codex CLI');
    expect(clientLabel(null, null)).toBe('Unattributed');
    expect(clientLabel('k1', 'Cursor laptop')).toBe('Cursor laptop');
  });
});

describe('normalizeNative', () => {
  it('reads the shipped shape and keeps unknowns honest', () => {
    const n = normalizeNative({
      job: { running: true, source: 'codex', started_at: '2026-10-02T15:00:00Z' },
      sources: [
        { id: 'opencode', provider: 'opencode', available: false, status: 'not_imported', imported_events: 0 },
        { id: 'codex', provider: 'codex', available: true, status: 'partial', imported_events: 40, excluded_gateway_events: 3, first_event_at: '2026-09-01T00:00:00Z', last_event_at: null, last_run_at: '2026-10-02T14:00:00Z' },
        { id: 'claude', available: true, status: 'pending' },
        { id: 'cursor:m1', provider: 'cursor', available: true, status: 'complete', imported_events: 7 },
        { status: 'complete' },
      ],
    });
    expect(n.running).toBe(true);
    expect(n.sources.map((s) => s.source)).toEqual(['opencode', 'codex', 'claude', 'cursor:m1']);
    const [oc, cx, cl, cu] = n.sources;
    expect(oc.available).toBe(false);
    expect(cx).toMatchObject({ running: true, status: 'running', units: 40, excluded: 3, coverage: { from: '2026-09-01T00:00:00Z', to: null }, updated_at: '2026-10-02T14:00:00Z' });
    expect(cl).toMatchObject({ queued: true, running: false, provider: 'claude', units: 0 });
    expect(cu).toMatchObject({ provider: 'cursor', units: 7, running: false, coverage: null });
  });

  it('survives an empty or odd payload', () => {
    expect(normalizeNative(null)).toEqual({ sources: [], running: false });
    expect(normalizeNative({ sources: 'nope' })).toEqual({ sources: [], running: false });
  });
});

describe('quotaWindows', () => {
  const w = (x: Partial<QuotaWindow>): QuotaWindow => ({ id: 'x', label: 'x', unit: 'percent', scope: 'account', model: null, ...x });
  it('shows a limit reported twice once, keeping the described copy', () => {
    const out = quotaWindows({
      windows: [
        w({ id: 'limit:session:account', label: 'session', used: 87, limit: 100, remaining: 13, reset_at: '2026-10-02T19:29:59Z' }),
        w({ id: 'session', label: 'Session (5-hour)', period: '5h', used: 87, limit: 100, remaining: 13, reset_at: '2026-10-02T19:29:59Z' }),
        w({ id: 'limit:weekly:fable', label: 'weekly', model: 'Fable', scope: 'model', used: 0, limit: 100, remaining: 100, reset_at: '2026-10-06T02:00:00Z' }),
        w({ id: 'limit:weekly:account', label: 'weekly', used: 27, limit: 100, remaining: 73, reset_at: '2026-10-06T01:59:59Z' }),
      ],
    });
    expect(out.map((x) => x.label)).toEqual(['Session (5-hour)', 'Weekly', 'Weekly']);
    expect(out[1].model).toBe('Fable');
  });
  it('keeps distinct limits that only share a reset time', () => {
    const r = '2026-10-06T02:00:00Z';
    expect(quotaWindows({ windows: [w({ id: 'a', used: 10, reset_at: r }), w({ id: 'b', used: 20, reset_at: r }), w({ id: 'c', used: 10 }), w({ id: 'd', used: 10 })] })).toHaveLength(4);
  });
});

describe('planLabel', () => {
  it('names known plan ids and tidies unknown ones', () => {
    expect(planLabel('prolite')).toBe('Pro Lite');
    expect(planLabel('max_20x')).toBe('Max 20x');
    expect(planLabel('Pro')).toBe('Pro');
    expect(planLabel('team_plus')).toBe('Team Plus');
  });
});

describe('providerLabel spellings', () => {
  it('treats opencode-go like opencode_go', () => {
    expect(providerLabel('opencode-go')).toBe(providerLabel('opencode_go'));
    expect(providerKind('opencode-go')).toBe('opencode');
  });
});
