import { describe, expect, it } from 'vitest';
import { formatCompact, formatDuration, formatMs, formatPercent, formatRelative, outcome, statusCode, toDate } from './format';

describe('toDate', () => {
  it('accepts ISO, epoch seconds, epoch ms and numeric strings', () => {
    const iso = '2026-10-02T12:00:00.000Z';
    const ms = Date.parse(iso);
    expect(toDate(iso)?.getTime()).toBe(ms);
    expect(toDate(ms / 1000)?.getTime()).toBe(ms);
    expect(toDate(ms)?.getTime()).toBe(ms);
    expect(toDate(String(ms / 1000))?.getTime()).toBe(ms);
    expect(toDate('nonsense')).toBeNull();
    expect(toDate(null)).toBeNull();
    expect(toDate(Number.NaN)).toBeNull();
  });
});

describe('status helpers', () => {
  it('parses numbers and strings', () => {
    expect(statusCode(200)).toBe(200);
    expect(statusCode(' 502 ')).toBe(502);
    expect(statusCode('ok')).toBe(200);
    expect(statusCode('failed')).toBe(500);
    expect(statusCode('weird')).toBeNull();
    expect(statusCode(undefined)).toBeNull();
  });
  it('derives outcome defensively', () => {
    expect(outcome({ status: 204 })).toBe('success');
    expect(outcome({ status: 399 })).toBe('success');
    expect(outcome({ status: 400 })).toBe('error');
    expect(outcome({ status: 200, error: 'x' })).toBe('error');
    expect(outcome({ status: 0 })).toBe('error');
    expect(outcome({ status: 'pending?' })).toBe('pending');
  });
});

describe('formatting', () => {
  it('formats durations and latencies', () => {
    expect(formatMs(0.4)).toBe('<1 ms');
    expect(formatMs(842)).toBe('842 ms');
    expect(formatMs(3870)).toBe('3.87 s');
    expect(formatMs(300000)).toBe('5m 0s');
    expect(formatMs(null)).toBe('–');
    expect(formatDuration(41)).toBe('41s');
    expect(formatDuration(3700)).toBe('1h 1m');
    expect(formatDuration(90000)).toBe('1d 1h');
  });
  it('formats percentages without rounding to a fake 100%', () => {
    expect(formatPercent(0, 0)).toBe('–');
    expect(formatPercent(9999, 10000)).toBe('99.9%');
    expect(formatPercent(1, 1)).toBe('100%');
    expect(formatPercent(1, 3)).toBe('33.3%');
  });
  it('compacts only large numbers', () => {
    expect(formatCompact(9999)).toBe('9,999');
    expect(formatCompact(12900)).toBe('12.9K');
  });
  it('relative times', () => {
    const now = Date.parse('2026-10-02T12:00:00Z');
    expect(formatRelative(now - 2000, now)).toBe('just now');
    expect(formatRelative(now - 120_000, now)).toMatch(/2 ?min|2m/);
  });
});
