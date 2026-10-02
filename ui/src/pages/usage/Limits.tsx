import { useQueryClient } from '@tanstack/react-query';
import { Check, Clock, Eye, Pause, Pencil, Play, Plus, RefreshCw, Trash2, X } from 'lucide-react';
import { useEffect, useMemo, useState } from 'react';
import { qk, useConnections, useMonitors, useRefreshUsage, useUsageSources } from '../../app/queries';
import { Link, navigate, useLocation } from '../../app/router';
import { useConfirm, useToast } from '../../components/feedback';
import { Menu } from '../../components/Menu';
import { Badge, Button, Callout, EmptyState, KindMark, Skeleton } from '../../components/ui';
import { ApiError, errorMessage } from '../../lib/api';
import { api } from '../../lib/client';
import { displayNames } from '../../lib/connections';
import { fmtMoney, freshness, monitorProvider, providerKind, providerLabel, planLabel, quotaMeter, quotaWindows, resetLabel, STATUS_INFO, usageQueryToSearch } from '../../lib/usage';
import type { QuotaWindow, UsageMonitor, UsageSource } from '../../lib/usageTypes';
import { WatchDialog } from './WatchDialog';

export function LimitsTab() {
  const { search } = useLocation();
  const params = new URLSearchParams(search);
  const sources = useUsageSources();
  const monitors = useMonitors();
  const connections = useConnections();
  const refresh = useRefreshUsage();
  const toast = useToast();
  const [watchOpen, setWatchOpen] = useState(false);
  const [editing, setEditing] = useState<UsageMonitor | null>(null);
  const watchParam = params.get('watch') === '1';
  const open = watchOpen || watchParam || !!editing;
  const close = () => {
    setWatchOpen(false);
    setEditing(null);
    if (watchParam) navigate('/usage/limits', { replace: true });
  };

  // Re-render every 30 s so "updated 3 min ago" and reset countdowns stay honest.
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = window.setInterval(() => setNow(Date.now()), 30_000);
    return () => window.clearInterval(t);
  }, []);

  const list = useMemo(() => sources.data?.sources ?? [], [sources.data]);
  // API-key connections have no plan to read: list them briefly instead of as failing cards.
  const noPlan = (s: UsageSource) => s.source === 'gateway_only' && !s.windows.length && !s.balances.length && !s.reported_costs.length;
  const routedAll = list.filter((s) => s.connection_id && !s.id.startsWith('monitor:'));
  const routed = routedAll.filter((s) => !noPlan(s));
  const apiOnly = routedAll.filter(noPlan);
  const watched = list.filter((s) => !routedAll.includes(s));
  const monitorById = new Map((monitors.data ?? []).map((m) => [`monitor:${m.id}`, m]));
  const names = displayNames(connections.data ?? []);
  const nameOf = (s: UsageSource) => (s.connection_id && names.get(s.connection_id)) || s.name;

  const refreshAll = () =>
    refresh.mutate(undefined, {
      onSuccess: (r) => toast({ tone: 'info', title: 'Refreshing all accounts', message: r?.message ?? 'Results appear here as they arrive.' }),
      onError: (e) => toast({ tone: 'err', title: 'Couldn’t start a refresh', message: errorMessage(e) }),
    });

  if (sources.error instanceof ApiError && sources.error.isUnsupported) {
    return (
      <Callout tone="info" title="This gateway doesn’t read plan limits yet">
        Update Switchyard to see quotas, balances and billing for your accounts. Token and cost estimates on the Spend tab don’t depend on this.
      </Callout>
    );
  }

  return (
    <div className="stack">
      <div className="limits-head">
        <p className="muted small">
          Each limit stands on its own: windows from different accounts or periods are never added together. Numbers come from each provider and refresh every few minutes.
        </p>
        <div className="row">
          <Button icon={RefreshCw} onClick={refreshAll} loading={refresh.isPending && !refresh.variables} disabled={!list.length}>
            Refresh all
          </Button>
          <Button variant="primary" icon={Plus} onClick={() => setWatchOpen(true)}>
            Watch an account
          </Button>
        </div>
      </div>

      {sources.isPending ? (
        <div className="source-grid" role="status" aria-busy aria-label="Loading plan limits">
          {[0, 1, 2].map((i) => (
            <div key={i} className="card card-pad stack-sm">
              <Skeleton w="45%" h={16} />
              <Skeleton w="70%" h={12} />
              <Skeleton h={8} />
              <Skeleton h={8} />
            </div>
          ))}
        </div>
      ) : sources.isError && !sources.data ? (
        <Callout tone="err" title="Couldn’t load plan limits" role="alert" action={<Button size="sm" icon={RefreshCw} onClick={() => sources.refetch()}>Retry</Button>}>
          {errorMessage(sources.error)}
        </Callout>
      ) : !list.length ? (
        <div className="card">
          <EmptyState
            icon={Eye}
            title="No accounts to show limits for"
            actions={
              <>
                <Button variant="primary" icon={Plus} onClick={() => setWatchOpen(true)}>
                  Watch an account
                </Button>
                {!connections.data?.length ? <Button onClick={() => navigate('/connections?import=1')}>Connect an account</Button> : null}
              </>
            }
          >
            Codex, Claude and Antigravity accounts you connect show their plan limits here automatically. You can also watch Cursor, OpenCode and other accounts without routing traffic through them.
          </EmptyState>
        </div>
      ) : (
        <>
          {routed.length ? (
            <section className="stack-sm" aria-labelledby="routed-title">
              <h2 id="routed-title" className="section-title">
                Accounts Switchyard routes to
              </h2>
              <div className="source-grid">
                {routed.map((s) => (
                  <SourceCard key={s.id} s={{ ...s, name: nameOf(s) }} now={now} />
                ))}
              </div>
            </section>
          ) : null}
          {apiOnly.length ? (
            <section className="card card-pad api-only" aria-labelledby="api-only-title">
              <h2 id="api-only-title" className="section-title">
                API key accounts
              </h2>
              <p className="muted small">API keys don’t expose plan limits or billing. Their tokens and estimated cost are on the Spend tab.</p>
              <ul className="api-only-list">
                {apiOnly.map((s) => (
                  <li key={s.id}>
                    <KindMark kind={providerKind(s.provider)} size="sm" />
                    <span className="truncate">{nameOf(s)}</span>
                    <span className="muted xs">{providerLabel(s.provider)}</span>
                    <Link to={`/usage${usageQueryToSearch({ window: '30d', source: 'gateway', connection_id: s.connection_id! })}`} className="link small">
                      See usage
                    </Link>
                  </li>
                ))}
              </ul>
            </section>
          ) : null}
          {watched.length ? (
            <section className="stack-sm" aria-labelledby="watched-title">
              <h2 id="watched-title" className="section-title">
                Watched accounts
              </h2>
              <div className="source-grid">
                {watched.map((s) => (
                  <SourceCard key={s.id} s={s} now={now} monitor={monitorById.get(s.id)} onEdit={(m) => setEditing(m)} />
                ))}
              </div>
            </section>
          ) : null}
        </>
      )}
      <WatchDialog open={open} onClose={close} editing={editing} connections={connections.data ?? []} />
    </div>
  );
}

const CAPS: { key: keyof UsageSource['capabilities']; label: string; help: string }[] = [
  { key: 'quota', label: 'Limits', help: 'Plan or rate limits and when they reset' },
  { key: 'cost', label: 'Billing', help: 'Spend or credits the provider reports' },
  { key: 'tokens', label: 'Tokens', help: 'Token counts from the provider' },
  { key: 'history', label: 'History', help: 'Past usage, not just the current period' },
];

const KIND_LABEL: Record<string, string> = { billed: 'Billed', included: 'Included', subscription: 'Subscription', on_demand: 'On-demand' };

export function SourceCard({ s, now, monitor, onEdit }: { s: UsageSource; now: number; monitor?: UsageMonitor; onEdit?: (m: UsageMonitor) => void }) {
  const refresh = useRefreshUsage();
  const toast = useToast();
  const confirm = useConfirm();
  const qc = useQueryClient();
  const [showAll, setShowAll] = useState(false);
  const info = STATUS_INFO[s.status] ?? STATUS_INFO.unavailable;
  const refreshing = s.status === 'refreshing' || !!s.refreshing;
  const fresh = freshness(s, now);
  // Static provider notes are for watched accounts; connected ones are self-explanatory.
  const notes = s.capability_notes?.length ? s.capability_notes : s.connection_id ? [] : (monitorProvider(s.provider)?.notes ?? []).filter((n) => !n.includes('already show'));
  const allWindows = quotaWindows(s);
  const windows = showAll ? allWindows : allWindows.slice(0, 5);

  const setEnabled = async (enabled: boolean) => {
    if (!monitor) return;
    try {
      await api.updateMonitor(monitor.id, {
        name: monitor.name,
        provider: monitor.provider as never,
        credential_source: monitor.credential_source,
        source_path: monitor.source_path ?? null,
        connection_id: monitor.connection_id ?? null,
        enabled,
      });
      void qc.invalidateQueries({ queryKey: qk.monitors });
      void qc.invalidateQueries({ queryKey: qk.sources });
    } catch (e) {
      toast({ tone: 'err', title: `Couldn’t ${enabled ? 'resume' : 'pause'} ${s.name}`, message: errorMessage(e) });
    }
  };
  const remove = async () => {
    if (!monitor) return;
    const ok = await confirm({
      title: `Stop watching ${s.name}?`,
      message: 'Its stored credential is erased from Switchyard. The account itself, and any app it came from, aren’t changed.',
      confirmLabel: 'Stop watching',
      danger: true,
    });
    if (!ok) return;
    try {
      await api.deleteMonitor(monitor.id);
      toast({ tone: 'ok', title: `Stopped watching ${s.name}` });
      void qc.invalidateQueries({ queryKey: qk.monitors });
      void qc.invalidateQueries({ queryKey: qk.sources });
    } catch (e) {
      toast({ tone: 'err', title: 'Couldn’t remove it', message: errorMessage(e) });
    }
  };

  return (
    <article className={`card source-card status-${s.status}`} aria-labelledby={`src-${s.id}`}>
      <header className="source-head">
        <KindMark kind={providerKind(s.provider)} />
        <div className="source-title">
          <h3 id={`src-${s.id}`} className="source-name" title={s.name}>
            {s.name}
          </h3>
          <span className="muted xs">
            {providerLabel(s.provider)}
            {s.plan ? ` · ${planLabel(s.plan)}` : ''}
            {s.identity_label ? ` · ${s.identity_label}` : ''}
            {s.connection_id ? (
              <>
                {' · '}
                <Link to="/connections" className="link">
                  routes traffic
                </Link>
              </>
            ) : null}
          </span>
        </div>
        <span className={`health-chip tone-${info.tone === 'info' ? 'muted' : info.tone}`} title={info.help}>
          <span className="dot" aria-hidden />
          {refreshing ? 'Refreshing' : info.label}
        </span>
        <Button
          size="sm"
          variant="ghost"
          iconOnly
          icon={RefreshCw}
          aria-label={`Refresh ${s.name}`}
          title="Refresh now"
          loading={refreshing || (refresh.isPending && refresh.variables === s.id)}
          disabled={s.status === 'disabled'}
          onClick={() => refresh.mutate(s.id, { onError: (e) => toast({ tone: 'err', title: 'Couldn’t refresh', message: errorMessage(e) }) })}
        />
        {monitor ? (
          <Menu
            label={`More actions for ${s.name}`}
            items={[
              { label: 'Edit', icon: Pencil, onSelect: () => onEdit?.(monitor) },
              monitor.enabled ? { label: 'Pause watching', icon: Pause, onSelect: () => void setEnabled(false) } : { label: 'Resume watching', icon: Play, onSelect: () => void setEnabled(true) },
              'separator',
              { label: 'Stop watching', icon: Trash2, onSelect: remove, danger: true },
            ]}
          />
        ) : null}
      </header>

      <p className={`source-fresh ${fresh.stale ? 'stale' : ''}`}>
        <Clock aria-hidden />
        {fresh.text}
      </p>

      {s.message && s.status !== 'ok' ? (
        <Callout tone={s.status === 'needs_auth' || s.status === 'unavailable' ? 'err' : 'warn'}>
          {s.message}
          {s.status === 'needs_auth' ? (
            s.connection_id ? (
              <>
                {' '}
                <Link to="/connections" className="link">
                  Fix on Connections
                </Link>
              </>
            ) : monitor ? (
              <>
                {' '}
                <button type="button" className="link link-button" onClick={() => onEdit?.(monitor)}>
                  Update credential
                </button>
              </>
            ) : null
          ) : null}
        </Callout>
      ) : null}

      {allWindows.length ? (
        <ul className="quota-list" aria-label="Limits">
          {windows.map((w) => (
            <QuotaRow key={`${w.id}-${w.model ?? ''}`} w={w} now={now} />
          ))}
        </ul>
      ) : s.capabilities.quota ? (
        <p className="muted small">{s.status === 'ok' ? 'No limits reported right now.' : 'Limits unknown until the next successful refresh.'}</p>
      ) : null}
      {allWindows.length > 5 ? (
        <button type="button" className="link link-button small" onClick={() => setShowAll((v) => !v)} aria-expanded={showAll}>
          {showAll ? 'Show fewer' : `Show ${allWindows.length - 5} more`}
        </button>
      ) : null}

      {s.balances.length ? (
        <dl className="source-kv">
          {s.balances.map((b) => (
            <div key={b.label}>
              <dt>{b.label}</dt>
              <dd className="num">{b.value == null ? <span className="unknown">Unknown</span> : b.unit === 'usd' ? fmtMoney(b.value, b.currency ?? 'USD') : `${b.value.toLocaleString()} ${b.unit}`}</dd>
            </div>
          ))}
        </dl>
      ) : null}

      {s.reported_costs.length ? (
        <div className="reported">
          <h4>
            Reported by {providerLabel(s.provider)}
            {sharedPeriod(s) ? <span className="muted"> · billing cycle {sharedPeriod(s)}</span> : null}
          </h4>
          <ul>
            {s.reported_costs.map((c, i) => (
              <li key={`${c.label}-${i}`}>
                <Badge tone={c.kind === 'billed' || c.kind === 'on_demand' ? 'warn' : 'outline'}>{KIND_LABEL[c.kind] ?? c.kind}</Badge>
                <span className="truncate">{c.label}</span>
                <span className="spacer" />
                <span className="num">{c.amount == null ? <span className="unknown">Unknown</span> : fmtMoney(c.amount, c.currency)}</span>
                {(c.period_start || c.period_end) && !sharedPeriod(s) ? <span className="muted xs period">{periodLabel(c.period_start, c.period_end)}</span> : null}
              </li>
            ))}
          </ul>
        </div>
      ) : null}

      <ul className="caps" aria-label="What Switchyard can read for this account">
        {CAPS.map((c) => (
          <li key={c.key} className={s.capabilities[c.key] ? 'on' : 'off'} title={c.help}>
            {s.capabilities[c.key] ? <Check aria-hidden /> : <X aria-hidden />}
            {c.label}
            <span className="sr-only">{s.capabilities[c.key] ? ' available' : ' not available'}</span>
          </li>
        ))}
      </ul>
      {notes.length ? (
        <ul className="source-notes">
          {notes.map((n) => (
            <li key={n}>{n}</li>
          ))}
        </ul>
      ) : null}
    </article>
  );
}

const shortDate = new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric' });

/** When every reported amount covers the same period, show it once. */
function sharedPeriod(s: UsageSource): string | null {
  const labels = new Set(s.reported_costs.map((c) => periodLabel(c.period_start, c.period_end)));
  const [only] = [...labels];
  return labels.size === 1 && only ? only : null;
}

function periodLabel(start?: string | null, end?: string | null) {
  const s = start ? new Date(start) : null;
  const e = end ? new Date(end) : null;
  if (s && e && !Number.isNaN(s.getTime()) && !Number.isNaN(e.getTime())) return `${shortDate.format(s)} to ${shortDate.format(e)}`;
  if (e && !Number.isNaN(e.getTime())) return `until ${shortDate.format(e)}`;
  if (s && !Number.isNaN(s.getTime())) return `since ${shortDate.format(s)}`;
  return '';
}

function QuotaRow({ w, now }: { w: QuotaWindow; now: number }) {
  const m = quotaMeter(w);
  const reset = resetLabel(w.reset_at, now);
  return (
    <li className="quota-row">
      <div className="quota-top">
        <span className="quota-label">
          {w.label}
          {w.model ? <span className="mono muted xs"> {w.model}</span> : null}
        </span>
        <span className="num quota-value">{m.primary}</span>
      </div>
      <div
        className={`quota-meter tone-${m.tone}`}
        role={m.pct === null ? undefined : 'meter'}
        aria-label={`${w.label}${w.model ? ` ${w.model}` : ''}`}
        aria-valuemin={m.pct === null ? undefined : 0}
        aria-valuemax={m.pct === null ? undefined : 100}
        aria-valuenow={m.pct === null ? undefined : Math.round(m.pct)}
        aria-valuetext={m.pct === null ? undefined : m.primary}
      >
        {m.pct === null ? <span className="quota-unknown" /> : <span style={{ width: `${Math.max(m.pct > 0 ? 1.5 : 0, m.pct)}%` }} />}
      </div>
      {reset || m.secondary ? <div className="muted xs">{[reset, m.secondary].filter(Boolean).join(' · ')}</div> : null}
    </li>
  );
}
