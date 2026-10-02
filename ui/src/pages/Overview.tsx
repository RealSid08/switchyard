import { ArrowRight, Check, FlaskConical, KeyRound, Plug, Plus, RefreshCw, SquareTerminal, Waypoints, Zap } from 'lucide-react';
import { useMemo, useState, useEffect } from 'react';
import { useConfig, useConnections, useKeys, useOverview } from '../app/queries';
import { Link, navigate } from '../app/router';
import { CopyField } from '../components/Code';
import { TrafficChart, bucketize } from '../components/TrafficChart';
import { Badge, Button, Callout, KindMark, PageHead, Skeleton, StatusCode, kindLabel } from '../components/ui';
import { errorMessage } from '../lib/api';
import { displayNames, missingKeyWarning } from '../lib/connections';
import { formatCompact, formatDuration, formatMs, formatNumber, formatPercent, formatRelative, toMillis } from '../lib/format';
import { resolveGatewayUrls } from '../lib/snippets';
import type { Overview, Transport } from '../lib/types';
import { ImportPanel } from './connections/ImportPanel';

export function OverviewPage() {
  const overview = useOverview();
  const connections = useConnections();
  const keys = useKeys();

  if (overview.isPending || connections.isPending) return <OverviewSkeleton />;
  if (overview.isError) {
    return (
      <>
        <PageHead title="Overview" />
        <Callout tone="err" title="Couldn’t load the overview" role="alert" action={<Button size="sm" icon={RefreshCw} onClick={() => overview.refetch()}>Retry</Button>}>
          {errorMessage(overview.error)}
        </Callout>
      </>
    );
  }
  const o = overview.data;
  const noConnections = (connections.data?.length ?? o.connections_total) === 0;
  if (noConnections) return <FirstRun keysCount={keys.data?.length ?? 0} />;

  const steps = {
    connection: true,
    key: (keys.data?.length ?? 0) > 0,
    traffic: o.requests_total > 0,
  };
  const setupDone = steps.key && steps.traffic;

  return (
    <>
      <PageHead
        title="Overview"
        description={`Switchyard ${o.version ? `v${o.version}` : ''} · up ${formatDuration(o.uptime_seconds)}`}
        actions={
          <Button icon={FlaskConical} onClick={() => navigate('/playground')}>
            Open playground
          </Button>
        }
      />
      {!setupDone ? <SetupStrip steps={steps} /> : null}
      <Kpis o={o} />
      <div className="overview-grid">
        <TrafficCard o={o} />
        <TransportCard o={o} />
        <RecentCard o={o} />
        <HealthCard />
      </div>
    </>
  );
}

function OverviewSkeleton() {
  return (
    <div role="status" aria-busy aria-label="Loading overview" className="stack">
      <Skeleton w={180} h={24} />
      <div className="kpis">
        {[0, 1, 2, 3, 4].map((i) => (
          <div className="card kpi" key={i}>
            <Skeleton w="50%" h={12} />
            <Skeleton w="70%" h={26} />
          </div>
        ))}
      </div>
      <div className="card card-pad">
        <Skeleton h={180} />
      </div>
    </div>
  );
}

/* ---------------- First run ---------------- */

function FirstRun({ keysCount }: { keysCount: number }) {
  const config = useConfig();
  const urls = useMemo(() => resolveGatewayUrls(config.data, window.location), [config.data]);
  return (
    <>
      <section className="hero" aria-labelledby="hero-title">
        <div className="hero-copy">
          <span className="eyebrow">
            <span className="dot dot-ok dot-pulse" aria-hidden /> Gateway running
          </span>
          <h1 id="hero-title" tabIndex={-1} data-page-title>
            Three steps to your first routed request
          </h1>
          <p>
            Switchyard is listening, but it has nowhere to send traffic yet. Connect an account, create a key for your tools, and point your agent at one
            local address.
          </p>
        </div>
        <div className="hero-address">
          <span className="muted xs">OpenAI-compatible base URL</span>
          <CopyField value={urls.openai} label="base URL" />
        </div>
      </section>

      <ol className="steps" aria-label="Setup steps">
        <li className="step is-current">
          <div className="step-marker" aria-hidden>
            1
          </div>
          <div className="step-body">
            <div className="step-head">
              <h2>Connect an account</h2>
              <span className="muted small">Use the subscriptions you already have, or an API key.</span>
            </div>
            <div className="card card-pad stack">
              <ImportPanel />
              <hr className="divider" />
              <div className="stack-sm">
                <h3>Add an API provider</h3>
                <div className="provider-buttons">
                  {(['openai', 'anthropic', 'gemini'] as const).map((k) => (
                    <button key={k} type="button" className="provider-button" onClick={() => navigate(`/connections?new=1&preset=${k}`)}>
                      <KindMark kind={k} size="sm" />
                      {kindLabel(k)}
                      <Plus aria-hidden className="muted" />
                    </button>
                  ))}
                  <button type="button" className="provider-button" onClick={() => navigate('/connections?new=1&preset=compatible')}>
                    <KindMark kind="openai" size="sm" />
                    OpenAI-compatible
                    <Plus aria-hidden className="muted" />
                  </button>
                </div>
              </div>
            </div>
          </div>
        </li>
        <li className={`step ${keysCount ? 'is-done' : ''}`}>
          <div className="step-marker" aria-hidden>
            {keysCount ? <Check width={14} height={14} /> : 2}
          </div>
          <div className="step-body">
            <div className="step-head">
              <h2>Create a client key</h2>
              <span className="muted small">{keysCount ? 'Done. You have a key ready for your tools.' : 'Each tool gets its own revocable key.'}</span>
            </div>
            {keysCount ? null : (
              <div>
                <Button icon={KeyRound} onClick={() => navigate('/keys?new=1')}>
                  Create key
                </Button>
              </div>
            )}
          </div>
        </li>
        <li className="step">
          <div className="step-marker" aria-hidden>
            3
          </div>
          <div className="step-body">
            <div className="step-head">
              <h2>Point your agent at Switchyard</h2>
              <span className="muted small">Copy-paste setup for Codex, Claude Code, OpenCode, Cursor, SDKs and curl.</span>
            </div>
            <div>
              <Button icon={SquareTerminal} onClick={() => navigate('/clients')}>
                See client setup
              </Button>
            </div>
          </div>
        </li>
      </ol>
    </>
  );
}

function SetupStrip({ steps }: { steps: { connection: boolean; key: boolean; traffic: boolean } }) {
  const items = [
    { done: steps.connection, label: 'Connect an account', to: '/connections', icon: Plug },
    { done: steps.key, label: 'Create a client key', to: '/keys?new=1', icon: KeyRound },
    { done: steps.traffic, label: 'Send your first request', to: steps.key ? '/clients' : '/playground', icon: Zap },
  ];
  const remaining = items.filter((i) => !i.done).length;
  return (
    <section className="card setup-strip" aria-label="Setup progress">
      <div className="setup-strip-head">
        <h2>Finish setup</h2>
        <span className="muted small">{remaining === 1 ? 'One step left' : `${remaining} steps left`}</span>
      </div>
      <ol className="setup-items">
        {items.map((i) => (
          <li key={i.label} className={i.done ? 'done' : ''}>
            {i.done ? (
              <span className="setup-item">
                <span className="check" aria-hidden>
                  <Check width={12} height={12} />
                </span>
                <span>{i.label}</span>
                <span className="sr-only">(done)</span>
              </span>
            ) : (
              <Link to={i.to} className="setup-item">
                <i.icon aria-hidden width={14} height={14} />
                <span>{i.label}</span>
                <ArrowRight aria-hidden width={14} height={14} className="muted" />
              </Link>
            )}
          </li>
        ))}
      </ol>
    </section>
  );
}

/* ---------------- Populated ---------------- */

function Kpis({ o }: { o: Overview }) {
  const finished = o.requests_success + o.requests_failed;
  return (
    <div className="kpis">
      <div className="card kpi">
        <span className="kpi-label">Requests</span>
        <span className="kpi-value">{formatCompact(o.requests_total)}</span>
        <span className="kpi-sub">{o.active_requests ? <><span className="dot dot-info" aria-hidden /> {formatNumber(o.active_requests)} in flight</> : 'none in flight'}</span>
      </div>
      <div className="card kpi">
        <span className="kpi-label">Success rate</span>
        <span className="kpi-value">{formatPercent(o.requests_success, finished)}</span>
        <span className="kpi-sub">{finished ? `${formatNumber(o.requests_failed)} failed` : 'no requests yet'}</span>
      </div>
      <div className="card kpi">
        <span className="kpi-label">Median latency</span>
        <span className="kpi-value">{o.requests_total && o.latency_ms_p50 ? formatMs(o.latency_ms_p50) : '–'}</span>
        <span className="kpi-sub">p50, recent requests</span>
      </div>
      <div className="card kpi">
        <span className="kpi-label">Connections</span>
        <span className="kpi-value">
          {formatNumber(o.connections_enabled)}
          <span className="kpi-of">/{formatNumber(o.connections_total)}</span>
        </span>
        <span className="kpi-sub">enabled</span>
      </div>
      <div className="card kpi">
        <span className="kpi-label">Uptime</span>
        <span className="kpi-value">{formatDuration(o.uptime_seconds)}</span>
        <span className="kpi-sub">{o.paused ? 'paused' : 'accepting traffic'}</span>
      </div>
    </div>
  );
}

function useNow(intervalMs: number) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = window.setInterval(() => setNow(Date.now()), intervalMs);
    return () => window.clearInterval(t);
  }, [intervalMs]);
  return now;
}

function TrafficCard({ o }: { o: Overview }) {
  const now = useNow(30_000);
  const buckets = useMemo(() => bucketize(o.series ?? [], now), [o.series, now]);
  const inWindow = buckets.reduce((s, b) => s + b.requests, 0);
  const errors = buckets.reduce((s, b) => s + b.errors, 0);
  const last = o.recent_requests?.[0];
  return (
    <section className="card traffic-card" aria-labelledby="traffic-title">
      <div className="card-head">
        <h2 id="traffic-title">
          Traffic <span className="sub">last hour</span>
        </h2>
        <div className="legend" aria-hidden={!inWindow}>
          <span>
            <span className="swatch" style={{ background: 'var(--chart-primary)' }} /> Succeeded
          </span>
          <span>
            <span className="swatch" style={{ background: 'var(--chart-error)' }} /> Failed
          </span>
        </div>
      </div>
      <div className="card-body">
        {inWindow ? (
          <>
            <p className="traffic-summary">
              <strong className="num">{formatNumber(inWindow)}</strong> requests · <span className="num">{formatNumber(errors)}</span> failed
            </p>
            <TrafficChart buckets={buckets} />
          </>
        ) : (
          <div className="chart-empty">
            <p>{o.requests_total ? `No requests in the last hour. Last one ${last ? formatRelative(last.timestamp) : 'a while ago'}.` : 'No traffic yet. Send a test request to see it here in real time.'}</p>
            <div className="row row-wrap" style={{ justifyContent: 'center' }}>
              <Button size="sm" icon={FlaskConical} onClick={() => navigate('/playground')}>
                Try the playground
              </Button>
              <Button size="sm" variant="ghost" icon={SquareTerminal} onClick={() => navigate('/clients')}>
                Connect a client
              </Button>
            </div>
          </div>
        )}
      </div>
    </section>
  );
}

const TRANSPORTS: { key: Transport; label: string; desc: string }[] = [
  { key: 'http', label: 'HTTP', desc: 'single JSON response' },
  { key: 'sse', label: 'SSE', desc: 'streamed events' },
  { key: 'websocket', label: 'WebSocket', desc: 'Responses WS mode' },
];

function TransportCard({ o }: { o: Overview }) {
  const counts = o.transport_counts ?? { http: 0, sse: 0, websocket: 0 };
  const total = TRANSPORTS.reduce((s, t) => s + (counts[t.key] ?? 0), 0);
  return (
    <section className="card" aria-labelledby="transport-title">
      <div className="card-head">
        <h2 id="transport-title">Transports</h2>
        <span className="sub">all time</span>
      </div>
      <div className="card-body stack">
        {total ? (
          TRANSPORTS.map((t) => {
            const n = counts[t.key] ?? 0;
            return (
              <div key={t.key} className="transport-row">
                <div className="row">
                  <span className="transport-name">{t.label}</span>
                  <span className="muted xs">{t.desc}</span>
                  <span className="spacer" />
                  <span className="num small">{formatNumber(n)}</span>
                  <span className="num muted xs transport-pct">{formatPercent(n, total)}</span>
                </div>
                <div className="meter" role="img" aria-label={`${t.label}: ${formatPercent(n, total)} of requests`}>
                  <span style={{ width: `${(n / total) * 100}%` }} />
                </div>
              </div>
            );
          })
        ) : (
          <p className="muted small">Request transports show up here once traffic flows. WebSocket sessions from Codex appear as their own line.</p>
        )}
      </div>
    </section>
  );
}

function RecentCard({ o }: { o: Overview }) {
  const recent = [...(o.recent_requests ?? [])].sort((a, b) => toMillis(b.timestamp) - toMillis(a.timestamp)).slice(0, 8);
  return (
    <section className="card recent-card" aria-labelledby="recent-title">
      <div className="card-head">
        <h2 id="recent-title">Recent requests</h2>
        <Link to="/activity" className="link small">
          All activity <ArrowRight aria-hidden />
        </Link>
      </div>
      {recent.length ? (
        <ul className="recent-list" aria-live="polite" aria-relevant="additions">
          {recent.map((r) => (
            <li key={r.id}>
              <Link to={`/activity/${encodeURIComponent(r.id)}`} className="recent-item">
                <StatusCode record={r} />
                <span className="mono truncate recent-model">{r.model}</span>
                <span className="muted small truncate recent-conn">{r.connection_name ?? '—'}</span>
                <Badge>{r.transport === 'websocket' ? 'WS' : r.transport.toUpperCase()}</Badge>
                <span className="num small recent-lat">{formatMs(r.latency_ms)}</span>
                <span className="muted xs recent-time">{formatRelative(r.timestamp)}</span>
              </Link>
            </li>
          ))}
        </ul>
      ) : (
        <div className="card-body muted small">Requests appear here the moment they finish.</div>
      )}
    </section>
  );
}

function HealthCard() {
  const connections = useConnections();
  const list = connections.data ?? [];
  const names = displayNames(list);
  return (
    <section className="card" aria-labelledby="health-title">
      <div className="card-head">
        <h2 id="health-title">Connections</h2>
        <Link to="/connections" className="link small">
          Manage <ArrowRight aria-hidden />
        </Link>
      </div>
      <ul className="health-list">
        {list.slice(0, 8).map((c) => (
          <li key={c.id}>
            <KindMark kind={c.kind} size="sm" />
            <span className="truncate">{names.get(c.id)}</span>
            <span className="spacer" />
            {missingKeyWarning(c) ? <Badge tone="warn">No key</Badge> : null}
            {c.supports_websocket ? <Badge tone="info">WS</Badge> : null}
            <span className={`health-state ${c.enabled ? 'on' : ''}`}>
              <span className={`dot ${c.enabled ? 'dot-ok' : ''}`} aria-hidden />
              {c.enabled ? 'Enabled' : 'Disabled'}
            </span>
          </li>
        ))}
        {list.length > 8 ? (
          <li className="muted small">
            <Link to="/connections" className="link">
              +{list.length - 8} more
            </Link>
          </li>
        ) : null}
      </ul>
      <div className="card-foot">
        <span>Routes decide which account serves a model.</span>
        <Link to="/routes" className="link small">
          <Waypoints aria-hidden /> Routes
        </Link>
      </div>
    </section>
  );
}
