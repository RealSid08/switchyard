import { credentialSourceInfo } from './oauth';
import type { CatalogModel, Connection, ConnectionHealth, RequestRecord } from './types';

export type Tone = 'ok' | 'warn' | 'err' | 'muted';

/**
 * What a connection's health means. "Ready" is deliberately modest: it says the
 * gateway isn't holding the account back, not that the provider was probed.
 */
export function healthInfo(h: ConnectionHealth | undefined, enabled: boolean): { tone: Tone; label: string; help: string } | null {
  if (!enabled || h?.status === 'disabled') {
    return { tone: 'muted', label: 'Disabled', help: 'Receives no traffic until you enable it.' };
  }
  if (!h) return null;
  switch (h.status) {
    case 'ready':
      return { tone: 'ok', label: 'Ready', help: 'Not held back by Switchyard. This isn’t a live check of the provider; use Test for that.' };
    case 'limited':
      return {
        tone: 'warn',
        label: 'Limited',
        help: 'Some models are benched on this account after a rate limit or failure, until their cooldown ends. Other models, and other accounts, keep serving.',
      };
    case 'cooling':
      return {
        tone: 'err',
        label: 'Cooling down',
        help: 'The whole account is benched after a limit or failure. Switchyard sends its traffic to other accounts until the cooldown ends.',
      };
    default:
      return null;
  }
}

export interface CooldownEntry {
  model: string;
  /** "All models" for an account-wide cooldown. */
  label: string;
  remaining: number;
}

/** Cooldowns counted down from when the data was fetched; finished ones drop out. */
export function activeCooldowns(h: ConnectionHealth | undefined, fetchedAt: number, now: number): CooldownEntry[] {
  if (!h) return [];
  const elapsed = Math.max(0, Math.floor((now - fetchedAt) / 1000));
  return h.cooldowns
    .map((c) => ({ model: c.model, label: c.model === '*' ? 'All models' : c.model, remaining: Math.max(0, Math.ceil(c.retry_after_seconds) - elapsed) }))
    .filter((c) => c.remaining > 0)
    .sort((a, b) => (a.model === '*' ? -1 : b.model === '*' ? 1 : a.remaining - b.remaining));
}

export function formatSeconds(s: number): string {
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, '0')}s`;
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}

function humanSpan(seconds: number): string {
  const s = Math.abs(seconds);
  if (s < 3600) return `${Math.max(1, Math.round(s / 60))} min`;
  if (s < 86_400) return `${Math.round(s / 3600)} h`;
  return `${Math.round(s / 86_400)} days`;
}

/**
 * Credential expiry that the user may need to act on. Gateway-owned sign-ins renew
 * themselves, so their expiry is informational; source-owned logins only renew when
 * the CLI (or CLIProxyAPI) signs in again.
 */
export function expiryInfo(c: Pick<Connection, 'credential_expires_at' | 'credential_source' | 'kind'>, nowMs: number): { tone: Tone; text: string } | null {
  const at = c.credential_expires_at;
  if (typeof at !== 'number' || !Number.isFinite(at) || at <= 0) return null;
  const left = at - Math.floor(nowMs / 1000);
  const owner = credentialSourceInfo(c.credential_source)?.owner;
  const cli = c.credential_source === 'native_codex' ? 'Codex' : c.credential_source === 'native_claude' ? 'Claude Code' : c.credential_source === 'cliproxy' ? 'CLIProxyAPI' : null;
  if (owner === 'gateway') {
    return left > 0 ? null : { tone: 'muted', text: 'Access token renews automatically on next use.' };
  }
  if (left <= 0) {
    return { tone: 'err', text: `Login expired ${humanSpan(left)} ago.${cli ? ` Sign in to ${cli} again, then re-import.` : ''}` };
  }
  if (left < 24 * 3600) {
    return { tone: 'warn', text: `Login expires in ${humanSpan(left)}.${cli ? ` Switchyard can’t renew it; sign in to ${cli} again before then.` : ''}` };
  }
  return null;
}

export const ATTEMPT_LABELS: Record<string, string> = {
  credential_unavailable: 'Credential unavailable',
  connect_failed: 'Couldn’t connect',
  transport_failed: 'Connection dropped',
  auth_rejected: 'Credential rejected',
  rate_limited: 'Rate limited',
  provider_unavailable: 'Provider unavailable',
  request_rejected: 'Request rejected',
};

export const ATTEMPT_HELP: Record<string, string> = {
  credential_unavailable: 'The account’s login couldn’t be made usable, so nothing was sent.',
  connect_failed: 'No connection to the provider; safe to try another account.',
  transport_failed: 'Sent, but the connection failed or timed out before a response.',
  auth_rejected: 'The provider refused this account’s credential (401/403).',
  rate_limited: 'The provider rate-limited this account or model (429).',
  provider_unavailable: 'The provider returned a server error (5xx).',
  request_rejected: 'The provider rejected the request itself.',
};

export function attemptLabel(error: string | null | undefined, status: number): string {
  if (!error) return status === 101 ? 'Connected' : 'Served';
  return ATTEMPT_LABELS[error] ?? error.replace(/_/g, ' ');
}

/** Did this request need more than one try (failover to another account or a retry)? */
export function wasRetried(r: Pick<RequestRecord, 'failovers' | 'attempts'>): boolean {
  return (r.failovers ?? 0) > 0 || (r.attempts?.length ?? 0) > 1;
}

export interface CatalogRow extends CatalogModel {
  inCatalog: boolean;
  configured: boolean;
}

/** Catalog first (in provider order), then configured models the catalog doesn't list. */
export function mergeCatalog(configured: string[], catalog: CatalogModel[]): CatalogRow[] {
  const have = new Set(configured);
  const seen = new Set<string>();
  const rows: CatalogRow[] = [];
  for (const m of catalog) {
    if (seen.has(m.id)) continue;
    seen.add(m.id);
    rows.push({ id: m.id, name: m.name && m.name !== m.id ? m.name : '', inCatalog: true, configured: have.has(m.id) });
  }
  for (const id of configured) if (!seen.has(id)) rows.push({ id, name: '', inCatalog: false, configured: true });
  return rows;
}

export function filterCatalog(rows: CatalogRow[], query: string): CatalogRow[] {
  const q = query.trim().toLowerCase();
  if (!q) return rows;
  return rows.filter((r) => r.id.toLowerCase().includes(q) || r.name.toLowerCase().includes(q));
}
