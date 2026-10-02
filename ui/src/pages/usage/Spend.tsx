import { CircleHelp, Coins, Info, Layers, RefreshCw, SquareTerminal, TriangleAlert, X } from 'lucide-react';
import { Fragment, useMemo, useState } from 'react';
import { useConnections, useKeys, useUsage } from '../../app/queries';
import { Link, navigate, useLocation } from '../../app/router';
import { StackedBars, type StackBucket, type StackSeries } from '../../components/StackedBars';
import { Badge, Button, Callout, EmptyState, KindMark, Segmented, Skeleton } from '../../components/ui';
import { ApiError, errorMessage } from '../../lib/api';
import { displayNames } from '../../lib/connections';
import { formatMs, formatNumber, toDate } from '../../lib/format';
import {
  activeFilterCount,
  clientLabel,
  costParts,
  costView,
  fmtMicros,
  fmtRatio,
  fmtTokens,
  knowledge,
  outcomeView,
  worstKnowledge,
  providerKind,
  providerLabel,
  relativeTime,
  tokenKnowledge,
  tokenMix,
  usageQueryFromSearch,
  usageQueryToSearch,
  type CostView,
  type Knowledge,
} from '../../lib/usage';
import type { UsageMetrics, UsageQuery, UsageReport, UsageScope } from '../../lib/usageTypes';
import { AppHistory } from './AppHistory';

const SCOPE_HELP: Record<UsageScope, string> = {
  gateway: 'Traffic Switchyard routed, counted once per request and once per WebSocket turn.',
  external: 'Usage your apps recorded themselves (OpenCode, Codex CLI, Claude Code, Cursor), read-only.',
  all: 'Switchyard traffic and app reports together. Anything that could be the same request counted twice is shown side by side, not added up.',
};

const UNKNOWN_BILLING_HELP =
  'App logs (like Codex CLI sessions) don’t record which login or key paid. Shown at API list prices so you can compare, but it isn’t money you were charged.';

const OUTCOME_HELP = 'App logs record token usage, not whether each request succeeded, so they count as neither succeeded nor failed.';

const COST_SERIES: StackSeries[] = [
  { key: 'api', label: 'API keys', color: 'var(--bill-api)' },
  { key: 'subscription', label: 'Subscription value', color: 'var(--bill-sub)' },
  { key: 'unknown', label: 'Billing unknown', color: 'var(--bill-unknown)', hatch: true },
];

const OUTCOME_SERIES: StackSeries[] = [
  { key: 'succeeded', label: 'Succeeded', color: 'var(--chart-primary)' },
  { key: 'failed', label: 'Failed', color: 'var(--chart-error)' },
  { key: 'cancelled', label: 'Cancelled', color: 'var(--border-strong)' },
  { key: 'unknown', label: 'Outcome not reported', color: 'var(--bill-unknown)', hatch: true },
];

const MIX_SERIES: StackSeries[] = [
  { key: 'input', label: 'Input', color: 'var(--mix-input)' },
  { key: 'cache_read', label: 'Cache read', color: 'var(--mix-cache-read)' },
  { key: 'cache_write', label: 'Cache write', color: 'var(--mix-cache-write)' },
  { key: 'output', label: 'Output', color: 'var(--mix-output)' },
];

export function SpendTab() {
  const { search } = useLocation();
  const q = useMemo(() => usageQueryFromSearch(search), [search]);
  const report = useUsage(q);
  const connections = useConnections();
  const keys = useKeys();
  // "Both" where the server refuses to add sides (combined=false). Older gateways without
  // by_source get the split from two extra requests instead.
  const split = q.source === 'all' && !!report.data && (report.data.combined === false || !report.data.totals);
  const legacySplit = split && !report.data?.by_source;
  const gatewaySide = useUsage({ ...q, source: 'gateway' }, { enabled: legacySplit });
  const externalSide = useUsage({ ...q, source: 'external' }, { enabled: legacySplit });
  const sides: Side[] | null = !split
    ? null
    : report.data?.by_source
      ? report.data.by_source.map((b) => ({ source: b.source, m: b.metrics }))
      : gatewaySide.data?.totals && externalSide.data?.totals
        ? [
            { source: 'gateway', m: gatewaySide.data.totals },
            { source: 'external', m: externalSide.data.totals },
          ]
        : null;

  const set = (patch: Partial<UsageQuery>) => {
    const next = { ...q, ...patch };
    for (const k of ['connection_id', 'provider', 'model', 'client_key_id'] as const) if (!next[k]) delete next[k];
    navigate(`/usage${usageQueryToSearch(next)}`, { replace: true });
  };

  const err = report.error;
  if (err instanceof ApiError && err.isUnsupported) {
    return (
      <Callout tone="info" title="This gateway doesn’t track usage yet">
        Update Switchyard to start recording tokens and cost. Nothing is estimated for traffic from before the update.
      </Callout>
    );
  }

  const r = report.data;
  return (
    <div className="stack usage">
      <Toolbar q={q} set={set} report={r} fetching={report.isFetching} onRefresh={() => void report.refetch()} />
      <Filters q={q} set={set} report={r} connections={connections.data ?? []} keys={keys.data ?? []} />
      {report.isPending ? (
        <UsageSkeleton />
      ) : !r ? (
        <Callout tone="err" title="Couldn’t load usage" role="alert" action={<Button size="sm" icon={RefreshCw} onClick={() => report.refetch()}>Retry</Button>}>
          {errorMessage(err)}
        </Callout>
      ) : (
        <>
          {err ? (
            <Callout tone="warn" title="Showing the last loaded numbers">
              Refreshing failed: {errorMessage(err)}
            </Callout>
          ) : null}
          <CoverageNotes report={r} />
          {(r.totals ? r.totals.units.total : (sides ?? []).reduce((n, x) => n + x.m.units.total, 0)) === 0 ? (
            split && !sides ? (
              <UsageSkeleton />
            ) : (
              <EmptyUsage q={q} report={r} clearFilters={() => set({ connection_id: undefined, provider: undefined, model: undefined, client_key_id: undefined })} />
            )
          ) : r.totals && !split ? (
            <>
              <Hero m={r.totals} />
              <Kpis m={r.totals} />
              <SeriesCard report={r} />
              <Breakdowns report={r} q={q} set={set} split={false} />
            </>
          ) : sides ? (
            <>
              <div className="scope-split">
                {sides.map((x) => (
                  <ScopeColumn key={x.source} source={x.source} m={x.m} />
                ))}
              </div>
              <p className="muted small">
                These aren’t added together: an app may report the same requests Switchyard routed. Pick one scope above to see a single total.
              </p>
              <SeriesCard report={r} sides={sides.map((x) => x.source)} />
              <Breakdowns report={r} q={q} set={set} split />
            </>
          ) : (
            <UsageSkeleton />
          )}
        </>
      )}
      {q.source !== 'gateway' ? <AppHistory /> : null}
    </div>
  );
}

/* ---------------- Toolbar & filters ---------------- */

function Toolbar({ q, set, report, fetching, onRefresh }: { q: UsageQuery; set: (p: Partial<UsageQuery>) => void; report?: UsageReport; fetching: boolean; onRefresh: () => void }) {
  return (
    <div className="usage-toolbar">
      <Segmented
        label="Time window"
        value={q.window}
        onChange={(window) => set({ window })}
        options={[
          { value: '24h', label: '24 hours' },
          { value: '7d', label: '7 days' },
          { value: '30d', label: '30 days' },
          { value: 'all', label: 'All time' },
        ]}
      />
      <Segmented
        label="Whose numbers"
        value={q.source}
        onChange={(source) => set({ source })}
        options={[
          { value: 'gateway', label: 'Through Switchyard', icon: Layers },
          { value: 'external', label: 'Reported by apps', icon: SquareTerminal },
          { value: 'all', label: 'Both' },
        ]}
      />
      <span className="spacer" />
      <span className="muted xs usage-asof" aria-live="polite">
        {report ? <WindowLabel report={report} /> : null}
      </span>
      <Button size="sm" variant="ghost" icon={RefreshCw} onClick={onRefresh} loading={fetching} aria-label="Refresh usage">
        Refresh
      </Button>
    </div>
  );
}

const dayFmt = new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric', timeZone: 'UTC' });
const dayTimeFmt = new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', timeZone: 'UTC' });

function WindowLabel({ report }: { report: UsageReport }) {
  const from = toDate(report.window.from);
  const to = toDate(report.window.to);
  const f = report.window.granularity === 'hour' ? dayTimeFmt : dayFmt;
  return (
    <span title={`Times are UTC. Generated ${report.generated_at}`}>
      {from && to ? `${f.format(from)} to ${f.format(to)} UTC` : 'No data yet'} · updated {relativeTime(report.generated_at) ?? 'just now'}
    </span>
  );
}

function Filters({
  q,
  set,
  report,
  connections,
  keys,
}: {
  q: UsageQuery;
  set: (p: Partial<UsageQuery>) => void;
  report?: UsageReport;
  connections: { id: string; name: string; kind: string }[];
  keys: { id: string; name: string }[];
}) {
  // Options from facets (window-wide), falling back to known accounts/keys and what the report contains.
  const accounts = new Map<string, string>();
  for (const a of report?.facets?.accounts ?? []) accounts.set(a.connection_id, a.name);
  const display = displayNames(connections);
  for (const c of connections) accounts.set(c.id, display.get(c.id) ?? c.name);
  for (const a of report?.by_account ?? []) if (a.connection_id && !accounts.has(a.connection_id)) accounts.set(a.connection_id, a.connection_name ?? a.connection_id);
  const providers = new Set<string>([...(report?.facets?.providers ?? []), ...(report?.by_provider ?? []).map((p) => p.provider), ...connections.map((c) => c.kind)]);
  const models = new Set<string>([...(report?.facets?.models ?? []), ...(report?.by_model ?? []).map((m) => m.model)]);
  const clients = new Map<string, string>([['playground', 'Playground']]);
  for (const k of keys) clients.set(k.id, k.name);
  for (const c of report?.facets?.clients ?? []) clients.set(c.client_key_id, c.name);
  for (const c of report?.by_client ?? []) if (c.client_key_id && !clients.has(c.client_key_id)) clients.set(c.client_key_id, clientLabel(c.client_key_id, c.client_key_name));
  if (q.model) models.add(q.model);
  if (q.provider) providers.add(q.provider);
  const n = activeFilterCount(q);
  return (
    <div className="filters usage-filters" role="group" aria-label="Filter usage">
      <select className="select filter-select" aria-label="Account" value={q.connection_id ?? ''} onChange={(e) => set({ connection_id: e.target.value || undefined })}>
        <option value="">All accounts</option>
        {[...accounts].map(([id, name]) => (
          <option key={id} value={id}>
            {name}
          </option>
        ))}
      </select>
      <select className="select filter-select" aria-label="Provider" value={q.provider ?? ''} onChange={(e) => set({ provider: e.target.value || undefined })}>
        <option value="">All providers</option>
        {[...providers].sort().map((p, _, all) => (
          <option key={p} value={p}>
            {providerLabel(p)}
            {all.some((o) => o !== p && providerLabel(o) === providerLabel(p)) ? ` (${p})` : ''}
          </option>
        ))}
      </select>
      <select className="select filter-select" aria-label="Model" value={q.model ?? ''} onChange={(e) => set({ model: e.target.value || undefined })}>
        <option value="">All models</option>
        {[...models].sort().map((m) => (
          <option key={m} value={m}>
            {m}
          </option>
        ))}
      </select>
      <select className="select filter-select" aria-label="Client" value={q.client_key_id ?? ''} onChange={(e) => set({ client_key_id: e.target.value || undefined })}>
        <option value="">All clients</option>
        {[...clients].map(([id, name]) => (
          <option key={id} value={id}>
            {name}
          </option>
        ))}
      </select>
      {n ? (
        <Button variant="ghost" icon={X} onClick={() => set({ connection_id: undefined, provider: undefined, model: undefined, client_key_id: undefined })}>
          Clear {n === 1 ? 'filter' : `${n} filters`}
        </Button>
      ) : null}
      <p className="muted xs scope-help">{SCOPE_HELP[q.source]}</p>
    </div>
  );
}

/* ---------------- Coverage ---------------- */

function CoverageNotes({ report }: { report: UsageReport }) {
  const c = report.coverage;
  const started = toDate(c.ledger_started_at);
  return (
    <>
      {!c.complete_for_window ? (
        <Callout tone="info" icon={Info} title="Partial history for this window">
          {c.message ??
            (started
              ? `Switchyard started counting usage on ${dayFmt.format(started)}. Earlier traffic isn’t included or estimated.`
              : 'Usage counting has only just started. Earlier traffic isn’t included or estimated.')}
        </Callout>
      ) : null}
      {report.warnings.length ? (
        <Callout tone="warn" icon={TriangleAlert} title={report.warnings.length === 1 ? 'Heads up' : `${report.warnings.length} things to know`}>
          <ul className="plain-list">
            {report.warnings.map((w) => (
              <li key={w}>{w}</li>
            ))}
          </ul>
        </Callout>
      ) : null}
    </>
  );
}

/* ---------------- Hero, KPIs ---------------- */

/**
 * Unknown reads "Unknown" (never 0). Partial gets a quiet "≥": some requests didn't
 * report this, so the real figure is at least this much. The hero explains once.
 */
function UnknownValue({ k, children }: { k: Knowledge; children: React.ReactNode }) {
  if (k === 'unknown')
    return (
      <span className="unknown" title="Not reported, so it isn’t counted as zero.">
        Unknown
      </span>
    );
  return (
    <>
      {k === 'partial' ? (
        <span className="at-least" title="Some requests didn’t report this, so the real figure is at least this much.">
          ≥<span className="sr-only">at least </span>
        </span>
      ) : null}
      {children}
    </>
  );
}

function Hero({ m }: { m: UsageMetrics }) {
  const cost = costView(m);
  const mix = tokenMix(m);
  const tk = tokenKnowledge(m);
  const totalKnown = mix.reduce((s, p) => s + (p.knowledge === 'unknown' ? 0 : p.value), 0);
  const rk = knowledge(m, 'reasoning');
  return (
    <section className="card usage-hero" aria-label="Cost and tokens">
      <div className="hero-cost">
        <span className="kpi-label">
          Estimated cost
          <span className="help-dot" title="List-price estimate from token counts. Only the API key part is likely money spent; subscription and unknown-billing parts are what the same usage would cost on the API.">
            <CircleHelp aria-hidden />
          </span>
        </span>
        <span className="hero-figure">{cost.coverage === 'none' ? <span className="unknown">Not priced</span> : fmtMicros(cost.estimate)}</span>
        <dl className="cost-split" aria-label="Estimated cost by billing">
          <div>
            <dt title="API-key traffic at list prices.">API keys</dt>
            <dd>{cost.api ? fmtMicros(cost.api) : '–'}</dd>
          </div>
          <div>
            <dt title="What subscription traffic would have cost at API prices. You weren’t charged this.">Subscription value</dt>
            <dd>{cost.subscription ? fmtMicros(cost.subscription) : '–'}</dd>
          </div>
          {cost.unknownBillingUnits ? (
            <div className="cost-unknown">
              <dt title={UNKNOWN_BILLING_HELP}>Billing unknown</dt>
              <dd>{fmtMicros(cost.unknownBilling) ?? '–'}</dd>
            </div>
          ) : null}
        </dl>
        {cost.unknownBillingUnits ? (
          <p className="muted xs cost-note">
            {formatNumber(cost.unknownBillingUnits)} {cost.unknownBillingUnits === 1 ? 'request doesn’t' : 'requests don’t'} record whether an API key or a plan paid, so Switchyard doesn’t guess. That part is the API price equivalent, not money charged.
          </p>
        ) : null}
        <dl className="cost-billed">
          <dt title="Only amounts a provider actually reported. Never mixed into estimates.">Billed by providers</dt>
          <dd>{fmtMicros(cost.reported) ?? <span className="unknown">Not reported</span>}</dd>
        </dl>
        {cost.unpricedUnits ? (
          <Link to="/usage/pricing" className="unpriced-chip">
            <TriangleAlert aria-hidden />
            {formatNumber(cost.unpricedUnits)} {cost.unpricedUnits === 1 ? 'request has' : 'requests have'} no price yet
          </Link>
        ) : null}
      </div>
      <div className="hero-tokens">
        <span className="kpi-label">Tokens</span>
        <span className="hero-figure">
          <UnknownValue k={tk}>{fmtTokens(m.tokens.total)}</UnknownValue>
        </span>
        {tk !== 'unknown' ? (
          <>
            <div className="mix-bar" role="img" aria-label={`Token mix: ${mix.map((p) => `${p.label} ${fmtTokens(p.value)}`).join(', ')}`}>
              {mix.map((p) => (p.value > 0 && totalKnown ? <span key={p.key} className={`mix-${p.key}`} style={{ flexGrow: p.value }} /> : null))}
            </div>
            <dl className="mix-legend">
              {mix.map((p) => (
                <div key={p.key}>
                  <dt>
                    <span className={`swatch mix-${p.key}`} aria-hidden /> {p.label}
                  </dt>
                  <dd className="num">
                    <UnknownValue k={p.knowledge}>{fmtTokens(p.value)}</UnknownValue>
                  </dd>
                </div>
              ))}
            </dl>
            {rk !== 'unknown' && m.tokens.reasoning > 0 ? (
              <p className="muted xs">{fmtTokens(m.tokens.reasoning)} of the output was reasoning.</p>
            ) : null}
            {m.tokens.usage_missing_units ? (
              <p className="muted xs">
                {formatNumber(m.tokens.usage_missing_units)} of {formatNumber(m.units.total)} requests didn’t report token usage, so figures marked ≥ are a minimum.
              </p>
            ) : null}
          </>
        ) : (
          <p className="muted small">None of these requests reported token counts.</p>
        )}
      </div>
    </section>
  );
}

/** "98% succeeded · 40 outcome not reported": the rate covers only requests with an outcome. */
function OutcomeLine({ m, short }: { m: UsageMetrics; short?: boolean }) {
  const o = outcomeView(m);
  const u = m.units;
  const parts: React.ReactNode[] = [];
  if (o.state === 'none')
    parts.push(
      <span key="none" className={short ? 'unknown' : undefined} title={OUTCOME_HELP}>
        {short ? 'Not reported' : 'Outcome not reported'}
      </span>,
    );
  else if (o.decided) parts.push(<span key="rate" title={o.unknown ? `Of the ${formatNumber(o.decided)} requests with a reported outcome.` : undefined}>{fmtRatio(o.rate)} succeeded</span>);
  else if (!o.unknown) parts.push('none finished');
  if (u.cancelled) parts.push(`${formatNumber(u.cancelled)} cancelled`);
  if (o.state === 'partial')
    parts.push(
      <span key="unk" title={OUTCOME_HELP}>
        {formatNumber(o.unknown)} outcome not reported
      </span>,
    );
  return (
    <>
      {parts.map((p, i) => (
        <Fragment key={i}>
          {i ? ' · ' : ''}
          {p}
        </Fragment>
      ))}
    </>
  );
}

function Kpis({ m }: { m: UsageMetrics }) {
  // Every routed request has at least one attempt; none at all means attempts weren't tracked (app logs).
  const attempts = m.attempts && m.attempts.total > 0 ? m.attempts : null;
  return (
    <div className="kpis usage-kpis">
      <div className="card kpi">
        <span className="kpi-label">Requests</span>
        <span className="kpi-value">{formatNumber(m.units.total)}</span>
        <span className="kpi-sub">
          <OutcomeLine m={m} />
        </span>
      </div>
      <div className="card kpi">
        <span className="kpi-label">Cache hit rate</span>
        <span className="kpi-value">{m.cache?.read_ratio != null ? fmtRatio(m.cache.read_ratio) : <span className="unknown">Unknown</span>}</span>
        <span className="kpi-sub">{m.cache?.eligible_units ? `share of input served from cache` : 'no cache data reported'}</span>
      </div>
      <div className="card kpi">
        <span className="kpi-label">First token</span>
        <span className="kpi-value">{m.first_token_ms?.avg != null ? formatMs(m.first_token_ms.avg) : '–'}</span>
        <span className="kpi-sub">average, streamed requests</span>
      </div>
      <div className="card kpi">
        <span className="kpi-label">Output speed</span>
        <span className="kpi-value">{m.throughput?.output_tokens_per_second != null ? `${Math.round(m.throughput.output_tokens_per_second)}` : '–'}</span>
        <span className="kpi-sub">tokens per second while generating</span>
      </div>
      <div className="card kpi">
        <span className="kpi-label">Failovers</span>
        <span className="kpi-value">{attempts ? formatNumber(attempts.failovers) : '–'}</span>
        <span className="kpi-sub">{attempts ? `${formatNumber(attempts.failed)} failed attempts` : 'not tracked'}</span>
      </div>
    </div>
  );
}

interface Side {
  source: string;
  m: UsageMetrics;
}

const SIDE_INFO: Record<string, { title: string; sub: string; icon: typeof Layers; badge?: string }> = {
  gateway: { title: 'Through Switchyard', sub: 'Counted by Switchyard as it routed each request.', icon: Layers },
  external: { title: 'Reported by apps', sub: 'May include requests Switchyard also routed.', icon: SquareTerminal, badge: 'app report' },
  external_disjoint: { title: 'Reported by apps only', sub: 'Confirmed separate from Switchyard traffic.', icon: SquareTerminal, badge: 'app only' },
};

function sideInfo(source: string | null | undefined) {
  return SIDE_INFO[source ?? ''] ?? { title: source ?? 'Other', sub: '', icon: SquareTerminal, badge: source ?? undefined };
}

function ScopeColumn({ source, m }: { source: string; m: UsageMetrics }) {
  const cost = costView(m);
  const info = sideInfo(source);
  const Icon = info.icon;
  return (
    <section className="card card-pad scope-col" aria-label={info.title}>
      <h3 className="row">
        <Icon aria-hidden width={15} height={15} /> {info.title}
      </h3>
      {info.sub ? <p className="muted xs scope-sub">{info.sub}</p> : null}
      <dl className="kv">
        <dt>Requests</dt>
        <dd className="num">{formatNumber(m.units.total)}</dd>
        <dt>Outcome</dt>
        <dd className="num">
          <OutcomeLine m={m} short />
        </dd>
        <dt>Tokens</dt>
        <dd className="num">
          <UnknownValue k={tokenKnowledge(m)}>{fmtTokens(m.tokens.total)}</UnknownValue>
        </dd>
        <dt>Estimated cost</dt>
        <dd className="num">{cost.coverage === 'none' ? <span className="unknown">Not priced</span> : fmtMicros(cost.estimate)}</dd>
        {cost.unknownBillingUnits ? (
          <>
            <dt className="kv-sub" title={UNKNOWN_BILLING_HELP}>
              Billing unknown
            </dt>
            <dd className="num kv-sub">{fmtMicros(cost.unknownBilling) ?? '–'}</dd>
          </>
        ) : null}
        <dt>Billed</dt>
        <dd className="num">{fmtMicros(cost.reported) ?? 'Not reported'}</dd>
      </dl>
    </section>
  );
}

/* ---------------- Series ---------------- */

type SeriesMetric = 'tokens' | 'cost' | 'requests';

function bucketLabel(iso: string, g: UsageReport['window']['granularity']) {
  const d = toDate(iso);
  if (!d) return { label: iso, title: iso };
  if (g === 'hour') {
    const label = `${String(d.getUTCHours()).padStart(2, '0')}:00`;
    return { label, title: `${dayFmt.format(d)}, ${label} UTC` };
  }
  if (g === 'month') {
    const label = new Intl.DateTimeFormat(undefined, { month: 'short', year: 'numeric', timeZone: 'UTC' }).format(d);
    return { label, title: label };
  }
  return { label: dayFmt.format(d), title: `${dayFmt.format(d)} (UTC)` };
}

function SeriesCard({ report, sides }: { report: UsageReport; sides?: string[] }) {
  const [metric, setMetric] = useState<SeriesMetric>('tokens');
  const [picked, setPicked] = useState<string | null>(null);
  // Split reports chart one side at a time: stacking sides would add them up visually.
  const side = sides ? (picked && sides.includes(picked) ? picked : sides[0]) : null;
  const pts = report.series;
  const buckets: StackBucket[] = pts.map((p): StackBucket => {
    const { label, title } = bucketLabel(p.bucket_start, report.window.granularity);
    const m = (side ? p.by_source?.[side] : p.metrics) ?? null;
    if (!m) return { label, title, values: {} };
    if (metric === 'tokens') {
      return {
        label,
        title,
        values: { input: m.tokens.input, cache_read: m.tokens.cache_read, cache_write: m.tokens.cache_write, output: m.tokens.output },
        unknown: m.units.total > 0 && tokenKnowledge(m) === 'unknown',
      };
    }
    if (metric === 'cost') {
      const c = costView(m);
      const p = costParts(m);
      return {
        label,
        title,
        values: c.estimate === null ? {} : { api: p.api / 1_000_000, subscription: p.subscription / 1_000_000, unknown: p.unknown / 1_000_000 },
        unknown: m.units.total > 0 && c.coverage === 'none',
      };
    }
    return { label, title, values: { succeeded: m.units.succeeded, failed: m.units.failed, cancelled: m.units.cancelled, unknown: outcomeView(m).unknown } };
  });
  // Only the kinds present in this window get a legend entry; each keeps its color regardless.
  const present = (all: StackSeries[]) => {
    const used = all.filter((s) => buckets.some((b) => (b.values[s.key] ?? 0) > 0));
    return used.length ? used : all.slice(0, 1);
  };
  const series: StackSeries[] =
    metric === 'tokens'
      ? MIX_SERIES
      : metric === 'cost'
        ? present(COST_SERIES)
        : present(OUTCOME_SERIES);
  const format = metric === 'tokens' ? fmtTokens : metric === 'cost' ? (v: number) => (v === 0 ? '$0' : v < 0.01 ? '<$0.01' : `$${v < 10 ? v.toFixed(2) : Math.round(v)}`) : (v: number) => formatNumber(v);
  return (
    <section className="card" aria-labelledby="usage-series-title">
      <div className="card-head series-head">
        <h2 id="usage-series-title">
          Over time <span className="sub">per {report.window.granularity}, UTC</span>
        </h2>
        <div className="row row-wrap">
          {sides && sides.length > 1 ? (
            <select className="select filter-select" aria-label="Chart scope" value={side ?? ''} onChange={(e) => setPicked(e.target.value)}>
              {sides.map((x) => (
                <option key={x} value={x}>
                  {sideInfo(x).title}
                </option>
              ))}
            </select>
          ) : null}
          <Segmented
            label="Chart metric"
            value={metric}
            onChange={setMetric}
            options={[
              { value: 'tokens', label: 'Tokens' },
              { value: 'cost', label: 'Cost' },
              { value: 'requests', label: 'Requests' },
            ]}
          />
        </div>
      </div>
      <div className="card-body">
        {pts.length ? (
          <StackedBars buckets={buckets} series={series} format={format} ariaLabel={`${metric === 'tokens' ? 'Tokens' : metric === 'cost' ? 'Estimated cost' : 'Requests'} per ${report.window.granularity}`} />
        ) : (
          <div className="chart-empty">
            <p>Nothing to chart in this window.</p>
          </div>
        )}
      </div>
    </section>
  );
}

/* ---------------- Breakdowns ---------------- */

type Dim = 'account' | 'model' | 'provider' | 'client';

interface Row {
  key: string;
  name: string;
  sub?: string;
  kind?: string;
  origin?: 'gateway' | 'external';
  /** Set when the report is split by source (combined=false). */
  source?: string | null;
  m: UsageMetrics;
  filter?: Partial<UsageQuery>;
  unpriced?: boolean;
}

function rowsFor(report: UsageReport, dim: Dim, names: Map<string, string>): Row[] {
  return baseRows(report, dim, names).map((r) => (r.source ? { ...r, key: `${r.source}:${r.key}` } : r));
}

function baseRows(report: UsageReport, dim: Dim, names: Map<string, string>): Row[] {
  switch (dim) {
    case 'account':
      return report.by_account.map((a, i) => ({
        key: a.connection_id ?? a.source_id ?? `ext-${i}`,
        name: (a.connection_id && names.get(a.connection_id)) || a.connection_name || a.account_label || (a.origin === 'external' ? 'Reported by an app' : 'Unknown account'),
        sub: [providerLabel(a.provider), a.billing === 'subscription' ? 'subscription' : a.billing === 'api_key' ? 'API key' : a.billing === 'unknown' ? 'billing unknown' : null].filter(Boolean).join(' · '),
        kind: providerKind(a.provider),
        origin: a.origin,
        source: a.source,
        m: a.metrics,
        filter: a.connection_id ? { connection_id: a.connection_id } : undefined,
      }));
    case 'model':
      return report.by_model.map((m) => ({ key: `${m.provider}/${m.model}`, name: m.model, sub: providerLabel(m.provider), kind: providerKind(m.provider), source: m.source, m: m.metrics, filter: { model: m.model }, unpriced: m.priced === false }));
    case 'provider':
      return report.by_provider.map((p) => ({ key: p.provider, name: providerLabel(p.provider), kind: providerKind(p.provider), source: p.source, m: p.metrics, filter: { provider: p.provider } }));
    case 'client':
      return report.by_client.map((c, i) => ({
        key: c.client_key_id ?? `none-${i}`,
        name: clientLabel(c.client_key_id, c.client_key_name),
        origin: c.origin,
        source: c.source,
        m: c.metrics,
        filter: c.client_key_id ? { client_key_id: c.client_key_id } : undefined,
      }));
  }
}

/** Hover detail for an estimate that mixes billing kinds. */
function costTitle(c: CostView): string | undefined {
  if (!c.unknownBillingUnits) return undefined;
  return [
    c.api ? `API keys ${fmtMicros(c.api)}` : null,
    c.subscription ? `Subscription value ${fmtMicros(c.subscription)}` : null,
    `Billing unknown ${fmtMicros(c.unknownBilling) ?? '–'} (API price equivalent, not charged)`,
  ]
    .filter(Boolean)
    .join(' · ');
}

function SuccessCell({ m }: { m: UsageMetrics }) {
  const o = outcomeView(m);
  if (o.state === 'none')
    return (
      <span className="unknown" title={OUTCOME_HELP}>
        Not reported
      </span>
    );
  if (o.rate === null) return <>–</>;
  return <span title={o.unknown ? `Of ${formatNumber(o.decided)} requests with a reported outcome. ${formatNumber(o.unknown)} didn’t report one.` : undefined}>{fmtRatio(o.rate)}</span>;
}

function Breakdowns({ report, q, set, split }: { report: UsageReport; q: UsageQuery; set: (p: Partial<UsageQuery>) => void; split: boolean }) {
  const [dim, setDim] = useState<Dim>('account');
  const connections = useConnections();
  const names = useMemo(() => displayNames(connections.data ?? []), [connections.data]);
  const order = (report.by_source ?? []).map((b) => b.source);
  const rank = (r: Row) => (split ? (order.indexOf(r.source ?? '') + 1 || order.length + 1) : 0);
  // Split reports list each source as its own group (stable within a group, server order).
  const rows = rowsFor(report, dim, names)
    .map((r, i) => ({ r, i }))
    .sort((a, b) => rank(a.r) - rank(b.r) || a.i - b.i)
    .map((x) => x.r);
  // Shares are within a row's own source when split, so no bar implies the sides add up.
  const group = (r: Row) => (split ? (r.source ?? '') : '');
  const totalCost = new Map<string, number>();
  const totalTokens = new Map<string, number>();
  for (const r of rows) {
    totalCost.set(group(r), (totalCost.get(group(r)) ?? 0) + (costView(r.m).estimate ?? 0));
    totalTokens.set(group(r), (totalTokens.get(group(r)) ?? 0) + r.m.tokens.total);
  }
  const byCost = [...totalCost.values()].every((v) => v > 0);
  const tabs: [Dim, string][] = [
    ['account', 'Accounts'],
    ['model', 'Models'],
    ['provider', 'Providers'],
    ['client', 'Clients'],
  ];
  return (
    <section className="card" aria-labelledby="usage-breakdown-title">
      <div className="card-head">
        <h2 id="usage-breakdown-title">Breakdown</h2>
        <span className="muted xs">
          Share of {byCost ? 'estimated cost' : 'tokens'}
          {split ? ' within each source' : ''} · select a row to filter
        </span>
      </div>
      <div className="tabs" role="tablist" aria-label="Break down by" style={{ padding: '0 12px' }}>
        {tabs.map(([id, label]) => (
          <button key={id} role="tab" aria-selected={dim === id} tabIndex={dim === id ? 0 : -1} onClick={() => setDim(id)}>
            {label}
            <span className="count">{rowsFor(report, id, names).length}</span>
          </button>
        ))}
      </div>
      {rows.length ? (
        <div className="table-wrap">
          <table className="table usage-table">
            <caption className="sr-only">Usage by {dim}</caption>
            <thead>
              <tr>
                <th scope="col">{tabs.find((t) => t[0] === dim)?.[1].replace(/s$/, '')}</th>
                <th scope="col" className="r">
                  Requests
                </th>
                <th scope="col" className="r">
                  Tokens in / out
                </th>
                <th scope="col" className="r">
                  Cache hit
                </th>
                <th scope="col" className="r">
                  Est. cost
                </th>
                <th scope="col">Share</th>
                <th scope="col" className="r">
                  Success
                </th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row, idx) => {
                const c = costView(row.m);
                const tc = totalCost.get(group(row)) ?? 0;
                const tt = totalTokens.get(group(row)) ?? 0;
                const share = byCost ? (c.estimate ?? 0) / tc : tt ? row.m.tokens.total / tt : 0;
                const badge = split && row.source ? sideInfo(row.source).badge : row.origin === 'external' ? 'app report' : undefined;
                const active =
                  (row.filter?.connection_id && row.filter.connection_id === q.connection_id) ||
                  (row.filter?.model && row.filter.model === q.model) ||
                  (row.filter?.provider && row.filter.provider === q.provider) ||
                  (row.filter?.client_key_id && row.filter.client_key_id === q.client_key_id);
                // "In" sums input + cache read + cache write: it's only as known as its least-known part.
                const inK = worstKnowledge([knowledge(row.m, 'input'), knowledge(row.m, 'cache_read'), knowledge(row.m, 'cache_write')]);
                const outK = knowledge(row.m, 'output');
                const groupHead = split && (idx === 0 || rows[idx - 1].source !== row.source);
                return (
                  <Fragment key={row.key}>
                    {groupHead ? (
                      <tr className="group-row">
                        <th scope="colgroup" colSpan={7}>
                          {sideInfo(row.source).title}
                        </th>
                      </tr>
                    ) : null}
                    <tr className={active ? 'selected' : ''}>
                      <td>
                        <div className="usage-name">
                          {row.kind ? <KindMark kind={row.kind} size="sm" /> : null}
                          {row.filter ? (
                            <button type="button" className="link-button usage-row-link" onClick={() => set(active ? Object.fromEntries(Object.keys(row.filter!).map((k) => [k, undefined])) : row.filter!)} aria-pressed={!!active}>
                              {row.name}
                            </button>
                          ) : (
                            <span>{row.name}</span>
                          )}
                          {badge ? <Badge tone="outline">{badge}</Badge> : null}
                          {row.unpriced ? <Badge tone="warn">no price</Badge> : null}
                        </div>
                        {row.sub ? <div className="muted xs">{row.sub}</div> : null}
                      </td>
                      <td className="r num">{formatNumber(row.m.units.total)}</td>
                      <td className="r num">
                        {inK === 'unknown' && outK === 'unknown' ? (
                          <span className="unknown">Unknown</span>
                        ) : (
                          <>
                            <UnknownValue k={inK}>{fmtTokens(row.m.tokens.input + row.m.tokens.cache_read + row.m.tokens.cache_write)}</UnknownValue>
                            <span className="muted"> / </span>
                            <UnknownValue k={outK}>{fmtTokens(row.m.tokens.output)}</UnknownValue>
                          </>
                        )}
                      </td>
                      <td className="r num">{row.m.cache?.read_ratio != null ? fmtRatio(row.m.cache.read_ratio) : '–'}</td>
                      <td className="r num" title={costTitle(c)}>
                        {c.coverage === 'none' ? (
                          <span className="unknown">Not priced</span>
                        ) : (
                          <>
                            {c.coverage === 'partial' && !fmtMicros(c.estimate)?.startsWith('<') ? (
                              <span className="at-least" title={`${c.unpricedUnits} requests have no price, so the real cost is higher.`}>
                                ≥<span className="sr-only">at least </span>
                              </span>
                            ) : null}
                            {fmtMicros(c.estimate)}
                          </>
                        )}
                      </td>
                      <td>
                        <div className="share" role="img" aria-label={`${fmtRatio(share)} of ${split ? `${sideInfo(row.source).title.toLowerCase()} total` : 'total'}`}>
                          <span style={{ width: `${Math.max(share > 0 ? 2 : 0, share * 100)}%` }} />
                        </div>
                      </td>
                      <td className="r num">
                        <SuccessCell m={row.m} />
                      </td>
                    </tr>
                  </Fragment>
                );
              })}
            </tbody>
          </table>
        </div>
      ) : (
        <div className="card-body muted small">Nothing to break down for this selection.</div>
      )}
    </section>
  );
}

/* ---------------- Empty & loading ---------------- */

function EmptyUsage({ q, report, clearFilters }: { q: UsageQuery; report: UsageReport; clearFilters: () => void }) {
  if (activeFilterCount(q)) {
    return (
      <div className="card">
        <EmptyState icon={Coins} title="No usage matches these filters" actions={<Button onClick={clearFilters}>Clear filters</Button>}>
          Try a longer window or a different account, model or client.
        </EmptyState>
      </div>
    );
  }
  if (q.source === 'external') {
    return (
      <div className="card">
        <EmptyState icon={SquareTerminal} title="No app reports yet">
          Switchyard can read the usage OpenCode, Codex CLI, Claude Code and Cursor record on this machine, read-only. Import an app below to see it here.
        </EmptyState>
      </div>
    );
  }
  const started = toDate(report.coverage.ledger_started_at);
  return (
    <div className="card">
      <EmptyState
        icon={Coins}
        title={started ? 'No usage in this window' : 'Nothing counted yet'}
        actions={
          <>
            <Button variant="primary" icon={SquareTerminal} onClick={() => navigate('/clients')}>
              Connect a client
            </Button>
            <Button onClick={() => navigate('/playground')}>Send a test request</Button>
          </>
        }
      >
        {started
          ? 'Requests through Switchyard show up here with tokens, cache use and estimated cost.'
          : 'Switchyard counts tokens and cost from the first request it routes. Earlier traffic isn’t backfilled or guessed.'}
      </EmptyState>
    </div>
  );
}

function UsageSkeleton() {
  return (
    <div className="stack" role="status" aria-busy aria-label="Loading usage">
      <div className="card usage-hero">
        <div className="stack-sm">
          <Skeleton w="40%" h={12} />
          <Skeleton w="60%" h={34} />
          <Skeleton h={14} />
        </div>
        <div className="stack-sm">
          <Skeleton w="30%" h={12} />
          <Skeleton w="50%" h={34} />
          <Skeleton h={10} />
        </div>
      </div>
      <div className="card card-pad">
        <Skeleton h={200} />
      </div>
    </div>
  );
}
