import { describe, expect, it } from 'vitest';
import { EMPTY_FILTERS, filterRequests, filtersFromSearch, filtersToSearch, hasActiveFilters, matchesFilters, mergeRequests, summarize } from './requests';
import type { RequestRecord } from './types';

const rec = (p: Partial<RequestRecord>): RequestRecord => ({
  id: 'id',
  timestamp: '2026-10-02T10:00:00Z',
  model: 'gpt-6.1-sol',
  connection_id: 'c1',
  connection_name: 'Codex',
  transport: 'sse',
  status: 200,
  latency_ms: 100,
  ...p,
});

const rows = [
  rec({ id: 'a', status: 200, model: 'coding', transport: 'websocket' }),
  rec({ id: 'b', status: '502', error: 'Provider stream failed' }),
  rec({ id: 'c', status: 'error' }),
  rec({ id: 'd', status: 200, error: 'partial failure', connection_id: 'c2', connection_name: 'Claude Code' }),
  rec({ id: 'e', status: 429, model: 'claude-opus-5-5', connection_id: 'c2', connection_name: 'Claude Code' }),
];

describe('filters', () => {
  it('splits success and error defensively (string statuses, explicit error text)', () => {
    expect(filterRequests(rows, { ...EMPTY_FILTERS, status: 'error' }).map((r) => r.id)).toEqual(['b', 'c', 'd', 'e']);
    expect(filterRequests(rows, { ...EMPTY_FILTERS, status: 'success' }).map((r) => r.id)).toEqual(['a']);
  });

  it('filters by model, transport, connection and multi-term search', () => {
    expect(filterRequests(rows, { ...EMPTY_FILTERS, model: 'coding' }).map((r) => r.id)).toEqual(['a']);
    expect(filterRequests(rows, { ...EMPTY_FILTERS, transport: 'websocket' }).map((r) => r.id)).toEqual(['a']);
    expect(filterRequests(rows, { ...EMPTY_FILTERS, connection: 'c2' }).map((r) => r.id)).toEqual(['d', 'e']);
    expect(filterRequests(rows, { ...EMPTY_FILTERS, query: 'claude 429' }).map((r) => r.id)).toEqual(['e']);
    expect(filterRequests(rows, { ...EMPTY_FILTERS, query: 'stream FAILED' }).map((r) => r.id)).toEqual(['b']);
    expect(matchesFilters(rows[0], EMPTY_FILTERS)).toBe(true);
  });

  it('round-trips through the URL and ignores junk', () => {
    const f = { status: 'error' as const, model: 'a/b:c', transport: 'sse' as const, connection: 'c1', query: 'x y' };
    expect(filtersFromSearch(filtersToSearch(f))).toEqual(f);
    expect(filtersToSearch(EMPTY_FILTERS)).toBe('');
    expect(filtersFromSearch('?status=bogus&transport=carrier-pigeon')).toEqual(EMPTY_FILTERS);
    expect(hasActiveFilters({ ...EMPTY_FILTERS, query: '   ' })).toBe(false);
  });
});

describe('mergeRequests', () => {
  it('dedupes by id, sorts newest first across timestamp formats, and caps', () => {
    const merged = mergeRequests(
      [rec({ id: 'old', timestamp: '2026-09-01T00:00:00Z' }), rec({ id: 'same', timestamp: 1790000000, latency_ms: 1 })],
      [rec({ id: 'same', timestamp: 1790000000, latency_ms: 2 }), rec({ id: 'new', timestamp: 1790000500000 })],
      2,
    );
    expect(merged.map((r) => r.id)).toEqual(['new', 'same']);
    expect(merged[1].latency_ms).toBe(2);
  });
});

describe('summarize', () => {
  it('computes counts and latency percentiles', () => {
    const s = summarize(Array.from({ length: 100 }, (_, i) => rec({ id: String(i), latency_ms: i + 1, status: i < 10 ? 500 : 200 })));
    expect(s).toEqual({ total: 100, errors: 10, p50: 51, p95: 96 });
    expect(summarize([])).toEqual({ total: 0, errors: 0, p50: null, p95: null });
  });
});
