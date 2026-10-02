import { describe, expect, it } from 'vitest';
import { activeCooldowns, attemptLabel, expiryInfo, filterCatalog, formatSeconds, healthInfo, mergeCatalog, wasRetried } from './health';
import type { ConnectionHealth } from './types';

const h = (p: Partial<ConnectionHealth> = {}): ConnectionHealth => ({ status: 'ready', cooldowns: [], last_used_at: null, last_status: null, last_error: null, ...p });

describe('healthInfo', () => {
  it('labels each state and is honest that ready is not a probe', () => {
    expect(healthInfo(h(), true)).toMatchObject({ tone: 'ok', label: 'Ready' });
    expect(healthInfo(h(), true)?.help).toMatch(/isn’t a live check/);
    expect(healthInfo(h({ status: 'limited' }), true)).toMatchObject({ tone: 'warn', label: 'Limited' });
    expect(healthInfo(h({ status: 'cooling' }), true)).toMatchObject({ tone: 'err', label: 'Cooling down' });
    expect(healthInfo(h({ status: 'disabled' }), true)?.label).toBe('Disabled');
    expect(healthInfo(h(), false)?.label).toBe('Disabled');
    expect(healthInfo(undefined, true)).toBeNull();
  });
});

describe('activeCooldowns', () => {
  it('counts down from fetch time, drops finished ones, puts account-wide first', () => {
    const health = h({ cooldowns: [{ model: 'gpt-6.1-sol', retry_after_seconds: 30 }, { model: '*', retry_after_seconds: 90 }, { model: 'x', retry_after_seconds: 5 }] });
    const list = activeCooldowns(health, 0, 10_000);
    expect(list.map((c) => [c.label, c.remaining])).toEqual([
      ['All models', 80],
      ['gpt-6.1-sol', 20],
    ]);
    expect(activeCooldowns(health, 0, 100_000)).toEqual([]);
    expect(formatSeconds(80)).toBe('1m 20s');
    expect(formatSeconds(3720)).toBe('1h 2m');
  });
});

describe('expiryInfo', () => {
  const now = 1_800_000_000_000;
  const secs = now / 1000;
  it('warns about source-owned logins that Switchyard cannot renew', () => {
    expect(expiryInfo({ kind: 'codex', credential_source: 'native_codex', credential_expires_at: secs + 3 * 3600 }, now)).toMatchObject({ tone: 'warn', text: expect.stringMatching(/expires in 3 h.*Codex/) });
    expect(expiryInfo({ kind: 'anthropic', credential_source: 'native_claude', credential_expires_at: secs - 7200 }, now)).toMatchObject({ tone: 'err', text: expect.stringMatching(/expired 2 h ago.*Claude Code/) });
    expect(expiryInfo({ kind: 'codex', credential_source: 'native_codex', credential_expires_at: secs + 5 * 86400 }, now)).toBeNull();
  });
  it('treats gateway-owned sign-ins as self-renewing and ignores unknown expiry', () => {
    expect(expiryInfo({ kind: 'codex', credential_source: 'oauth', credential_expires_at: secs + 60 }, now)).toBeNull();
    expect(expiryInfo({ kind: 'codex', credential_source: 'oauth', credential_expires_at: secs - 60 }, now)?.tone).toBe('muted');
    expect(expiryInfo({ kind: 'codex', credential_source: 'native_codex', credential_expires_at: null }, now)).toBeNull();
    expect(expiryInfo({ kind: 'codex', credential_source: 'native_codex' }, now)).toBeNull();
  });
});

describe('attempts', () => {
  it('labels constant errors and detects retries', () => {
    expect(attemptLabel('rate_limited', 429)).toBe('Rate limited');
    expect(attemptLabel('auth_rejected', 401)).toBe('Credential rejected');
    expect(attemptLabel(null, 200)).toBe('Served');
    expect(attemptLabel(null, 101)).toBe('Connected');
    expect(attemptLabel('something_new', 418)).toBe('something new');
    expect(wasRetried({ failovers: 1 })).toBe(true);
    expect(wasRetried({ failovers: 0, attempts: [{ connection_id: 'a', connection_name: 'A', model: 'm', status: 401, duration_ms: 1, error: 'auth_rejected' }, { connection_id: 'a', connection_name: 'A', model: 'm', status: 200, duration_ms: 1, error: null }] })).toBe(true);
    expect(wasRetried({})).toBe(false);
  });
});

describe('model catalog', () => {
  it('merges catalog with configured models, flagging ones the provider no longer offers', () => {
    const rows = mergeCatalog(['b', 'retired'], [{ id: 'a', name: 'Model A' }, { id: 'b', name: 'b' }, { id: 'a', name: 'dup' }]);
    expect(rows).toEqual([
      { id: 'a', name: 'Model A', inCatalog: true, configured: false },
      { id: 'b', name: '', inCatalog: true, configured: true },
      { id: 'retired', name: '', inCatalog: false, configured: true },
    ]);
    expect(filterCatalog(rows, 'model a').map((r) => r.id)).toEqual(['a']);
    expect(filterCatalog(rows, '  ')).toHaveLength(3);
  });
});
