import type { RequestRecord } from './types';

/** Accepts ISO strings, epoch seconds, epoch milliseconds, or numeric strings. */
export function toDate(value: string | number | null | undefined): Date | null {
  if (value === null || value === undefined || value === '') return null;
  let n: number | null = null;
  if (typeof value === 'number') n = value;
  else if (/^\d+(\.\d+)?$/.test(value.trim())) n = Number(value);
  if (n !== null) {
    if (!Number.isFinite(n)) return null;
    // Anything below ~year 2286 in seconds is treated as seconds.
    const ms = n < 1e11 ? n * 1000 : n;
    const d = new Date(ms);
    return Number.isNaN(d.getTime()) ? null : d;
  }
  const d = new Date(value);
  return Number.isNaN(d.getTime()) ? null : d;
}

export function toMillis(value: string | number | null | undefined): number {
  return toDate(value)?.getTime() ?? 0;
}

/** HTTP status as a number, or null if absent/unparseable ("ok"/"error" strings are mapped). */
export function statusCode(status: unknown): number | null {
  if (typeof status === 'number' && Number.isFinite(status)) return status;
  if (typeof status === 'string') {
    const s = status.trim().toLowerCase();
    if (/^\d{3}$/.test(s)) return Number(s);
    if (s === 'ok' || s === 'success') return 200;
    if (s === 'error' || s === 'failed' || s === 'failure') return 500;
  }
  return null;
}

export type Outcome = 'success' | 'error' | 'pending';

/** Success/error derived defensively: an explicit error string always means error. */
export function outcome(r: Pick<RequestRecord, 'status' | 'error'>): Outcome {
  if (r.error) return 'error';
  const code = statusCode(r.status);
  if (code === null) return 'pending';
  if (code === 0) return 'error';
  return code < 400 ? 'success' : 'error';
}

const nf = new Intl.NumberFormat('en-US');
const compact = new Intl.NumberFormat('en-US', { notation: 'compact', maximumFractionDigits: 1 });

export function formatNumber(n: number | null | undefined): string {
  if (n === null || n === undefined || !Number.isFinite(n)) return '–';
  return nf.format(n);
}

export function formatCompact(n: number | null | undefined): string {
  if (n === null || n === undefined || !Number.isFinite(n)) return '–';
  return Math.abs(n) < 10_000 ? nf.format(n) : compact.format(n);
}

export function formatMs(ms: number | null | undefined): string {
  if (ms === null || ms === undefined || !Number.isFinite(ms)) return '–';
  if (ms < 1) return '<1 ms';
  if (ms < 1000) return `${Math.round(ms)} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(ms < 10_000 ? 2 : 1)} s`;
  return `${Math.floor(ms / 60_000)}m ${Math.round((ms % 60_000) / 1000)}s`;
}

export function formatDuration(seconds: number | null | undefined): string {
  if (seconds === null || seconds === undefined || !Number.isFinite(seconds) || seconds < 0) return '–';
  const s = Math.floor(seconds);
  const d = Math.floor(s / 86_400);
  const h = Math.floor((s % 86_400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m`;
  return `${s}s`;
}

export function formatPercent(part: number, total: number): string {
  if (!total) return '–';
  const p = (part / total) * 100;
  if (p === 100 || p === 0) return `${p}%`;
  return `${p >= 99.95 ? 99.9 : p.toFixed(1)}%`;
}

const rtf = new Intl.RelativeTimeFormat('en', { numeric: 'auto', style: 'narrow' });

export function formatRelative(value: string | number | null | undefined, now = Date.now()): string {
  const d = toDate(value);
  if (!d) return '–';
  const diff = (d.getTime() - now) / 1000;
  const abs = Math.abs(diff);
  if (abs < 5) return 'just now';
  if (abs < 60) return rtf.format(Math.round(diff), 'second');
  if (abs < 3600) return rtf.format(Math.round(diff / 60), 'minute');
  if (abs < 86_400) return rtf.format(Math.round(diff / 3600), 'hour');
  return rtf.format(Math.round(diff / 86_400), 'day');
}

const timeFmt = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit', second: '2-digit' });
const dateTimeFmt = new Intl.DateTimeFormat(undefined, {
  year: 'numeric',
  month: 'short',
  day: 'numeric',
  hour: '2-digit',
  minute: '2-digit',
  second: '2-digit',
});

export function formatTime(value: string | number | null | undefined): string {
  const d = toDate(value);
  return d ? timeFmt.format(d) : '–';
}

export function formatDateTime(value: string | number | null | undefined): string {
  const d = toDate(value);
  return d ? dateTimeFmt.format(d) : '–';
}

export function pluralize(n: number, one: string, many = `${one}s`): string {
  return `${formatNumber(n)} ${n === 1 ? one : many}`;
}
