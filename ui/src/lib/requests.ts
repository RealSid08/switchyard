import { outcome, statusCode, toMillis } from './format';
import { wasRetried } from './health';
import type { RequestRecord, Transport } from './types';

export interface RequestFilters {
  status: 'all' | 'success' | 'error';
  model: string;
  transport: 'all' | Transport;
  connection: string;
  query: string;
  /** Only requests that failed over to another account or needed several attempts. */
  retried: boolean;
}

export const EMPTY_FILTERS: RequestFilters = { status: 'all', model: '', transport: 'all', connection: '', query: '', retried: false };

export function hasActiveFilters(f: RequestFilters): boolean {
  return f.status !== 'all' || !!f.model || f.transport !== 'all' || !!f.connection || !!f.query.trim() || f.retried;
}

export function matchesFilters(r: RequestRecord, f: RequestFilters): boolean {
  if (f.status !== 'all') {
    const o = outcome(r);
    if (f.status === 'error' ? o !== 'error' : o !== 'success') return false;
  }
  if (f.model && r.model !== f.model) return false;
  if (f.transport !== 'all' && r.transport !== f.transport) return false;
  if (f.connection && r.connection_id !== f.connection && !(r.attempts ?? []).some((a) => a.connection_id === f.connection)) return false;
  if (f.retried && !wasRetried(r)) return false;
  const q = f.query.trim().toLowerCase();
  if (q) {
    const code = statusCode(r.status);
    const haystack = [
      r.id,
      r.model,
      r.route ?? '',
      r.connection_name ?? '',
      ...(r.attempts ?? []).flatMap((a) => [a.connection_name, a.model]),
      r.transport,
      code === null ? '' : String(code),
      r.error ?? '',
    ]
      .join(' ')
      .toLowerCase();
    // Every whitespace-separated term must match somewhere.
    if (!q.split(/\s+/).every((term) => haystack.includes(term))) return false;
  }
  return true;
}

export function filterRequests(records: RequestRecord[], f: RequestFilters): RequestRecord[] {
  if (!hasActiveFilters(f)) return records;
  return records.filter((r) => matchesFilters(r, f));
}

/** Merge new records into a newest-first list, de-duplicating by id and capping length. */
export function mergeRequests(existing: RequestRecord[] | undefined, incoming: RequestRecord[], cap = 500): RequestRecord[] {
  const byId = new Map<string, RequestRecord>();
  for (const r of existing ?? []) byId.set(r.id, r);
  for (const r of incoming) byId.set(r.id, r);
  return [...byId.values()].sort((a, b) => toMillis(b.timestamp) - toMillis(a.timestamp)).slice(0, cap);
}

/** Read filters from / write filters to a URL query string so views are linkable. */
export function filtersFromSearch(search: string): RequestFilters {
  const p = new URLSearchParams(search);
  const status = p.get('status');
  const transport = p.get('transport');
  return {
    status: status === 'error' || status === 'success' ? status : 'all',
    model: p.get('model') ?? '',
    transport: transport === 'http' || transport === 'sse' || transport === 'websocket' ? transport : 'all',
    connection: p.get('connection') ?? '',
    query: p.get('q') ?? '',
    retried: p.get('retried') === '1',
  };
}

export function filtersToSearch(f: RequestFilters): string {
  const p = new URLSearchParams();
  if (f.status !== 'all') p.set('status', f.status);
  if (f.model) p.set('model', f.model);
  if (f.transport !== 'all') p.set('transport', f.transport);
  if (f.connection) p.set('connection', f.connection);
  if (f.query.trim()) p.set('q', f.query.trim());
  if (f.retried) p.set('retried', '1');
  const s = p.toString();
  return s ? `?${s}` : '';
}

export interface RequestStats {
  total: number;
  errors: number;
  retried: number;
  p50: number | null;
  p95: number | null;
}

export function summarize(records: RequestRecord[]): RequestStats {
  const lat = records.map((r) => r.latency_ms).filter((n) => Number.isFinite(n)).sort((a, b) => a - b);
  const q = (p: number) => (lat.length ? lat[Math.min(lat.length - 1, Math.floor(p * lat.length))] : null);
  return {
    total: records.length,
    errors: records.filter((r) => outcome(r) === 'error').length,
    retried: records.filter(wasRetried).length,
    p50: q(0.5),
    p95: q(0.95),
  };
}
