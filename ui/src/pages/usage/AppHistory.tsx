import { useQueryClient } from '@tanstack/react-query';
import { Download, Eye, RefreshCw, ShieldCheck, Trash2 } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { qk, useMonitors, useNativeHistory } from '../../app/queries';
import { navigate } from '../../app/router';
import { useConfirm, useToast } from '../../components/feedback';
import { Button, KindMark } from '../../components/ui';
import { ApiError, errorMessage } from '../../lib/api';
import { api } from '../../lib/client';
import { formatNumber, toDate } from '../../lib/format';
import { relativeTime } from '../../lib/usage';
import type { NativeHistorySource } from '../../lib/usageTypes';

const APPS: { id: string; label: string; kind: string; reads: string }[] = [
  { id: 'opencode', label: 'OpenCode', kind: 'opencode', reads: 'Token counts and cost estimates from OpenCode’s local database.' },
  { id: 'codex', label: 'Codex CLI', kind: 'codex', reads: 'Token counts from Codex session logs. They don’t record which login paid, so billing shows as unknown.' },
  { id: 'claude', label: 'Claude Code', kind: 'anthropic', reads: 'Token counts from Claude Code project logs.' },
];
const CURSOR_READS = 'Usage events from this Cursor account.';

const dayFmt = new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric' });

function describe(s: NativeHistorySource | undefined): { text: string; tone: 'ok' | 'warn' | 'err' | 'muted'; running: boolean } {
  if (!s) return { text: 'Not available from this gateway', tone: 'muted', running: false };
  if (s.running) return { text: s.units ? `Importing, ${formatNumber(s.units)} so far` : 'Importing…', tone: 'muted', running: true };
  if (s.queued) return { text: 'Waiting for another import to finish', tone: 'muted', running: true };
  if (!s.available) return { text: s.message ?? 'Not found on this machine', tone: 'muted', running: false };
  if (s.status === 'not_found') return { text: s.message ?? 'No history found on this machine', tone: 'muted', running: false };
  if (s.status === 'error') return { text: s.message ?? 'The last import failed', tone: 'err', running: false };
  if (s.status === 'not_imported') return { text: 'Not imported', tone: 'muted', running: false };
  const from = toDate(s.coverage?.from ?? null);
  const to = toDate(s.coverage?.to ?? null);
  const span = from && to ? `, ${dayFmt.format(from)} to ${dayFmt.format(to)}` : '';
  const when = relativeTime(s.updated_at) ? ` · updated ${relativeTime(s.updated_at)}` : '';
  const more = s.status === 'partial' ? ' · more to read' : '';
  if (!s.units) return { text: `Imported, no usage found yet${when}`, tone: 'muted', running: false };
  return { text: `${formatNumber(s.units)} ${s.units === 1 ? 'request' : 'requests'}${span}${more}${when}`, tone: 'ok', running: false };
}

interface AppRow {
  id: string;
  label: string;
  kind: string;
  reads: string;
  s?: NativeHistorySource;
}

/** Read-only imports of the usage your apps record locally, kept as their own scope. */
export function AppHistory() {
  const native = useNativeHistory();
  const monitors = useMonitors();
  const qc = useQueryClient();
  const toast = useToast();
  const confirm = useConfirm();
  const [busy, setBusy] = useState<string | null>(null);
  // When a background import finishes, the usage on the page is out of date: reload it.
  const wasRunning = useRef(false);
  const running = native.data?.running ?? false;
  useEffect(() => {
    if (wasRunning.current && !running) void qc.invalidateQueries({ queryKey: ['usage'] });
    wasRunning.current = running;
  }, [running, qc]);

  if (native.error instanceof ApiError && native.error.isUnsupported) return null;
  const sources = native.data?.sources ?? [];
  const byId = new Map(sources.map((s) => [s.source, s]));
  const monitorName = new Map((monitors.data ?? []).map((m) => [m.id, m.name]));
  const cursors = sources.filter((s) => s.provider === 'cursor');
  const rows: AppRow[] = [
    ...APPS.map((a) => ({ ...a, s: byId.get(a.id) })),
    ...cursors.map((s) => {
      const name = monitorName.get(s.source.slice('cursor:'.length));
      return { id: s.source, label: name ? `Cursor · ${name}` : 'Cursor', kind: 'cursor', reads: CURSOR_READS, s };
    }),
  ];

  const run = async (id: string, label: string) => {
    setBusy(id);
    try {
      await api.importNativeHistory(id);
      toast({ tone: 'info', title: `Importing ${label} history`, message: 'This runs in the background. Numbers appear as they arrive.' });
      void qc.invalidateQueries({ queryKey: qk.native });
      void qc.invalidateQueries({ queryKey: ['usage'] });
    } catch (e) {
      toast({ tone: 'err', title: `Couldn’t import ${label}`, message: errorMessage(e) });
    } finally {
      setBusy(null);
    }
  };
  const clear = async (id: string, label: string) => {
    const ok = await confirm({
      title: `Remove ${label} history?`,
      message: `This removes the ${label} usage Switchyard imported. ${label} itself and its files aren’t touched, and you can import again any time.`,
      confirmLabel: 'Remove history',
      danger: true,
    });
    if (!ok) return;
    try {
      await api.clearNativeHistory(id);
      toast({ tone: 'ok', title: `Removed ${label} history` });
      void qc.invalidateQueries({ queryKey: qk.native });
      void qc.invalidateQueries({ queryKey: ['usage'] });
    } catch (e) {
      toast({ tone: 'err', title: 'Couldn’t remove it', message: errorMessage(e) });
    }
  };

  return (
    <section className="card" aria-labelledby="app-history-title">
      <div className="card-head">
        <h2 id="app-history-title">
          App history <span className="sub">usage your apps record on this machine</span>
        </h2>
      </div>
      <ul className="app-history">
        {rows.map((a) => {
          const s = a.s;
          const d = native.isPending ? { text: '…', tone: 'muted' as const, running: false } : describe(s);
          const imported = !!s && s.status !== 'not_imported';
          return (
            <li key={a.id}>
              <KindMark kind={a.kind} />
              <div className="app-history-main">
                <strong>{a.label}</strong>
                <span className="muted xs">{a.reads}</span>
                <span className={`app-history-status tone-${d.tone}`} aria-live="polite">
                  {d.running ? <RefreshCw className="spin" aria-hidden /> : null}
                  {d.text}
                </span>
                {s?.excluded ? (
                  <span className="muted xs">
                    {formatNumber(s.excluded)} already counted through Switchyard, so left out here.
                  </span>
                ) : null}
              </div>
              <div className="row">
                <Button size="sm" icon={imported ? RefreshCw : Download} onClick={() => void run(a.id, a.label)} loading={busy === a.id} disabled={!s || d.running || !s.available}>
                  {imported ? 'Update' : 'Import'}
                </Button>
                {imported ? <Button size="sm" variant="ghost" iconOnly icon={Trash2} aria-label={`Remove ${a.label} history`} onClick={() => void clear(a.id, a.label)} /> : null}
              </div>
            </li>
          );
        })}
        {!cursors.length && !native.isPending ? (
          <li>
            <KindMark kind="cursor" />
            <div className="app-history-main">
              <strong>Cursor</strong>
              <span className="muted xs">Usage events from a Cursor account you watch.</span>
              <span className="app-history-status tone-muted">Watch a Cursor account first, then import its history.</span>
            </div>
            <div className="row">
              <Button size="sm" icon={Eye} onClick={() => navigate('/usage/limits?watch=1')}>
                Watch Cursor
              </Button>
            </div>
          </li>
        ) : null}
      </ul>
      <div className="card-foot">
        <span className="row">
          <ShieldCheck aria-hidden width={14} height={14} />
          Read-only. Switchyard scans these files for usage records (tokens, model, time) and never stores your prompts or outputs. Requests an app sent through Switchyard are matched and counted once where IDs allow; anything uncertain stays in this separate scope.
        </span>
      </div>
    </section>
  );
}
