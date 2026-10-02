import { ArrowUpRight, FlaskConical, Lock, Shuffle } from 'lucide-react';
import { navigate, Link } from '../../app/router';
import { CopyButton } from '../../components/Code';
import { Badge, Button, Callout, StatusCode } from '../../components/ui';
import { formatDateTime, formatMs, formatNumber, outcome, statusCode } from '../../lib/format';
import { ATTEMPT_HELP, attemptLabel, wasRetried } from '../../lib/health';
import type { RequestRecord } from '../../lib/types';

const TRANSPORT_LABEL: Record<string, string> = { http: 'HTTP', sse: 'SSE', websocket: 'WebSocket' };

/** The model actually sent upstream: the last attempt's, else the requested name. */
export function servedModel(r: RequestRecord): string {
  const last = r.attempts?.[r.attempts.length - 1];
  return last?.model || r.model;
}

export function RequestDetail({ r, names, explain }: { r: RequestRecord; names: Map<string, string>; explain: (code: number | null) => string }) {
  const o = outcome(r);
  const code = statusCode(r.status);
  const attempts = r.attempts ?? [];
  const served = servedModel(r);
  const connName = (r.connection_id && names.get(r.connection_id)) || r.connection_name || '—';
  return (
    <div className="stack">
      <div className={`detail-hero ${o}`}>
        <StatusCode record={r} />
        <span className="detail-hero-text">{o === 'success' ? 'Succeeded' : o === 'error' ? 'Failed' : 'In progress'}</span>
        {wasRetried(r) ? (
          <Badge tone="warn" icon={Shuffle}>
            {r.failovers ? `${r.failovers} ${r.failovers === 1 ? 'failover' : 'failovers'}` : `${attempts.length} attempts`}
          </Badge>
        ) : null}
        <span className="spacer" />
        <span className="num">{formatMs(r.latency_ms)}</span>
      </div>

      {r.error ? (
        <Callout tone="err" title="Error">
          <span className="mono small" style={{ overflowWrap: 'anywhere' }}>
            {r.error}
          </span>
        </Callout>
      ) : null}
      {o === 'error' ? <Callout tone="info">{explain(code)}</Callout> : null}

      <div className="route-path" aria-label="How this request was routed">
        <div className="route-step">
          <span className="muted xs">Requested</span>
          <span className="mono">{r.model}</span>
          {r.route ? <Badge tone="brand">route</Badge> : <Badge tone="outline">direct</Badge>}
        </div>
        <span className="route-arrow" aria-hidden>
          →
        </span>
        <div className="route-step">
          <span className="muted xs">{o === 'success' ? 'Served by' : 'Last tried'}</span>
          <span>
            {r.connection_id && names.get(r.connection_id) ? (
              <Link className="link" to={`/activity?connection=${encodeURIComponent(r.connection_id)}`}>
                {connName}
              </Link>
            ) : (
              connName
            )}
          </span>
          {served !== r.model ? <span className="mono muted xs">as {served}</span> : null}
        </div>
      </div>

      <UpstreamTiming r={r} />

      {attempts.length ? <Attempts r={r} names={names} /> : null}

      <dl className="kv">
        <dt>Time</dt>
        <dd>{formatDateTime(r.timestamp)}</dd>
        <dt>Transport</dt>
        <dd>{TRANSPORT_LABEL[r.transport] ?? r.transport}</dd>
        <dt>HTTP status</dt>
        <dd className="num">{code ?? String(r.status)}</dd>
        <dt>Tokens</dt>
        <dd className="num">
          {formatNumber(r.input_tokens ?? null)} in · {formatNumber(r.output_tokens ?? null)} out
        </dd>
        <dt>Request ID</dt>
        <dd className="row">
          <span className="mono small truncate">{r.id}</span>
          <CopyButton text={r.id} label="Copy request ID" />
        </dd>
      </dl>
      <div className="row row-wrap">
        <Button icon={FlaskConical} onClick={() => navigate(`/playground?model=${encodeURIComponent(r.model)}`)}>
          Retry in playground
        </Button>
        <Button variant="ghost" icon={ArrowUpRight} onClick={() => navigate(`/activity?model=${encodeURIComponent(r.model)}`)}>
          All requests for this model
        </Button>
      </div>
      <p className="privacy-note">
        <Lock aria-hidden /> Prompt and response bodies are never recorded, so they can’t be shown here.
      </p>
    </div>
  );
}

/**
 * Gateway-side timings, measured from when Switchyard received the request
 * (including earlier failed attempts). Not the same as the playground's
 * browser-measured numbers.
 */
function UpstreamTiming({ r }: { r: RequestRecord }) {
  const total = Math.max(1, r.latency_ms || 0);
  const ttfb = r.ttfb_ms ?? null;
  const first = r.first_token_ms ?? null;
  if (ttfb === null && first === null) {
    return (
      <section className="timing" aria-label="Gateway timing">
        <h3>Gateway timing</h3>
        <p className="muted small">
          Total {formatMs(r.latency_ms)}. {r.transport === 'http' ? 'Delivered as one JSON response, so there’s no first-token time.' : 'No successful upstream body was received.'}
        </p>
      </section>
    );
  }
  const pct = (v: number | null) => (v === null ? 0 : Math.min(100, (v / total) * 100));
  return (
    <section className="timing" aria-label="Gateway timing">
      <h3>Gateway timing</h3>
      <div className="timing-bar" aria-hidden>
        <span className="seg wait" style={{ width: `${pct(ttfb)}%` }} />
        {first !== null ? <span className="seg think" style={{ width: `${Math.max(0, pct(first) - pct(ttfb))}%` }} /> : null}
        <span className="seg stream" style={{ flex: 1 }} />
      </div>
      <dl className="timing-kv">
        <div>
          <dt>
            <span className="swatch wait" aria-hidden /> First byte
          </dt>
          <dd className="num">{formatMs(ttfb)}</dd>
        </div>
        <div>
          <dt>
            <span className="swatch think" aria-hidden /> First token
          </dt>
          <dd className="num">{formatMs(first)}</dd>
        </div>
        <div>
          <dt>
            <span className="swatch stream" aria-hidden /> Total
          </dt>
          <dd className="num">{formatMs(r.latency_ms)}</dd>
        </div>
      </dl>
      <p className="muted xs">
        Measured by the gateway from when it received the request, including any earlier attempts.
        {r.transport === 'websocket' ? ' WebSocket sessions are one row; timings cover the first turn.' : ''}
      </p>
    </section>
  );
}

function Attempts({ r, names }: { r: RequestRecord; names: Map<string, string> }) {
  const attempts = r.attempts ?? [];
  const longest = Math.max(1, ...attempts.map((a) => a.duration_ms));
  return (
    <section className="attempts" aria-label="Upstream attempts">
      <h3>
        Attempts <span className="muted small">in order</span>
      </h3>
      <ol>
        {attempts.map((a, i) => {
          const last = i === attempts.length - 1;
          const served = last && !a.error && outcome(r) === 'success';
          const prev = attempts[i - 1];
          const switched = prev && prev.connection_id !== a.connection_id;
          return (
            <li key={i} className={served ? 'served' : a.error ? 'failed' : ''}>
              <span className="attempt-n" aria-hidden>
                {i + 1}
              </span>
              <div className="attempt-main">
                <div className="row row-wrap">
                  <strong className="truncate">{names.get(a.connection_id) ?? a.connection_name}</strong>
                  <span className="mono muted xs">{a.model}</span>
                  {switched ? <Badge tone="outline">failover</Badge> : prev ? <Badge tone="outline">retry</Badge> : null}
                </div>
                <div className="attempt-bar" aria-hidden>
                  <span style={{ width: `${Math.max(2, (a.duration_ms / longest) * 100)}%` }} />
                </div>
              </div>
              <div className="attempt-meta">
                <span className={`attempt-label ${served ? 'ok' : a.error ? 'err' : ''}`} title={a.error ? ATTEMPT_HELP[a.error] : undefined}>
                  {attemptLabel(a.error, a.status)}
                </span>
                <span className="muted xs num">
                  {a.status ? `HTTP ${a.status}` : 'no response'} · {formatMs(a.duration_ms)}
                </span>
              </div>
            </li>
          );
        })}
      </ol>
      {attempts.length > 1 ? (
        <p className="muted xs">
          A failover moves to a different account; a retry is the same account again (for example after refreshing its credential).
        </p>
      ) : null}
    </section>
  );
}
