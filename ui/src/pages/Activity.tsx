import { Activity as ActivityIcon, Copy, FlaskConical, Lock, Pause, Play, RefreshCw, Search, Shuffle, X } from 'lucide-react';
import { useQuery } from '@tanstack/react-query';
import { api } from '../lib/client';
import { wasRetried } from '../lib/health';
import { RequestDetail, servedModel } from './activity/RequestDetail';
import { useEffect, useMemo, useRef, useState } from 'react';
import { useConnections, useRequests } from '../app/queries';
import { Link, navigate, useLocation } from '../app/router';
import { Dialog } from '../components/Dialog';
import { Badge, Button, Callout, EmptyState, PageHead, Segmented, Skeleton, StatusCode } from '../components/ui';
import { errorMessage } from '../lib/api';
import { displayNames } from '../lib/connections';
import { formatDateTime, formatMs, formatNumber, formatPercent, formatRelative, formatTime } from '../lib/format';
import { filterRequests, filtersFromSearch, filtersToSearch, hasActiveFilters, summarize, type RequestFilters } from '../lib/requests';
import type { RequestRecord, Transport } from '../lib/types';

const TRANSPORT_LABEL: Record<Transport, string> = { http: 'HTTP', sse: 'SSE', websocket: 'WebSocket' };

export function ActivityPage({ selectedId }: { selectedId?: string }) {
  const { search } = useLocation();
  const filters = useMemo(() => filtersFromSearch(search), [search]);
  const [live, setLive] = useState(true);
  // Server-side filters widen history (the gateway keeps the last 1000 records).
  const serverQuery = { status: filters.status === 'error' ? ('error' as const) : undefined, model: filters.model || undefined };
  const requests = useRequests(serverQuery, { live });
  const connections = useConnections();
  const names = useMemo(() => displayNames(connections.data ?? []), [connections.data]);
  const [frozen, setFrozen] = useState<RequestRecord[] | null>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const [query, setQuery] = useState(filters.query);

  const setFilters = (patch: Partial<RequestFilters>) => {
    const next = { ...filters, ...patch };
    navigate(`/activity${filtersToSearch(next)}`, { replace: true });
  };

  // Debounce free-text search into the URL.
  useEffect(() => {
    if (query === filters.query) return;
    const t = window.setTimeout(() => setFilters({ query }), 200);
    return () => window.clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [query]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const el = e.target as HTMLElement;
      if (e.key === '/' && !['INPUT', 'TEXTAREA', 'SELECT'].includes(el.tagName) && !el.isContentEditable) {
        e.preventDefault();
        searchRef.current?.focus();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  const toggleLive = () => {
    if (live) setFrozen(requests.data ?? []);
    else setFrozen(null);
    setLive(!live);
  };

  const source = useMemo(() => frozen ?? requests.data ?? [], [frozen, requests.data]);
  const rows = useMemo(() => filterRequests(source, filters), [source, filters]);
  const newSinceFrozen = frozen ? (requests.data ?? []).filter((r) => !frozen.some((f) => f.id === r.id)).length : 0;
  const stats = useMemo(() => summarize(rows), [rows]);
  const models = useMemo(() => [...new Set([...(requests.data ?? []).map((r) => r.model), filters.model].filter(Boolean))].sort(), [requests.data, filters.model]);
  const allRecords = requests.data ?? [];
  const cached = selectedId ? (allRecords.find((r) => r.id === selectedId) ?? frozen?.find((r) => r.id === selectedId)) : undefined;
  // Deep links to records outside the loaded window come from the gateway (it keeps the last 1,000).
  const detail = useQuery({
    queryKey: ['request', selectedId],
    queryFn: () => api.requestDetail(selectedId!),
    enabled: !!selectedId && !cached && !requests.isPending,
    retry: false,
    staleTime: 60_000,
  });
  const selected = cached ?? detail.data;
  const filtered = hasActiveFilters(filters);

  return (
    <>
      <PageHead
        title="Activity"
        description="Every request through the gateway, as it happens."
        actions={
          <>
            <Button icon={live ? Pause : Play} onClick={toggleLive} aria-pressed={!live}>
              {live ? 'Pause stream' : newSinceFrozen ? `Resume (${formatNumber(newSinceFrozen)} new)` : 'Resume stream'}
            </Button>
          </>
        }
      />
      <p className="privacy-note">
        <Lock aria-hidden />
        Metadata only. Switchyard never stores prompts, responses or credentials in its request log.
      </p>

      <div className="filters" role="search" aria-label="Filter requests">
        <div className="input-group filter-search">
          <Search className="input-icon" aria-hidden />
          <input
            ref={searchRef}
            className="input has-icon"
            type="search"
            placeholder="Search model, connection, status, error…"
            aria-label="Search requests"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            aria-keyshortcuts="/"
          />
        </div>
        <Segmented
          label="Outcome"
          value={filters.status}
          onChange={(status) => setFilters({ status })}
          options={[
            { value: 'all', label: 'All' },
            { value: 'success', label: 'Succeeded' },
            { value: 'error', label: 'Failed' },
          ]}
        />
        <select className="select filter-select" aria-label="Model" value={filters.model} onChange={(e) => setFilters({ model: e.target.value })}>
          <option value="">All models</option>
          {models.map((m) => (
            <option key={m} value={m}>
              {m}
            </option>
          ))}
        </select>
        <select className="select filter-select" aria-label="Connection" value={filters.connection} onChange={(e) => setFilters({ connection: e.target.value })}>
          <option value="">All connections</option>
          {(connections.data ?? []).map((c) => (
            <option key={c.id} value={c.id}>
              {names.get(c.id)}
            </option>
          ))}
          {filters.connection && !(connections.data ?? []).some((c) => c.id === filters.connection) ? <option value={filters.connection}>Deleted connection</option> : null}
        </select>
        <select className="select filter-select" aria-label="Transport" value={filters.transport} onChange={(e) => setFilters({ transport: e.target.value as RequestFilters['transport'] })}>
          <option value="all">All transports</option>
          <option value="http">HTTP</option>
          <option value="sse">SSE</option>
          <option value="websocket">WebSocket</option>
        </select>
        <button type="button" className={`filter-toggle ${filters.retried ? 'on' : ''}`} aria-pressed={filters.retried} onClick={() => setFilters({ retried: !filters.retried })}>
          <Shuffle aria-hidden />
          Retried
        </button>
        {filtered ? (
          <Button
            variant="ghost"
            icon={X}
            onClick={() => {
              setQuery('');
              navigate('/activity', { replace: true });
            }}
          >
            Clear
          </Button>
        ) : null}
      </div>

      {requests.isPending ? (
        <div className="card" role="status" aria-busy aria-label="Loading requests">
          {[0, 1, 2, 3, 4, 5].map((i) => (
            <div key={i} className="row" style={{ padding: '12px 16px', gap: 16 }}>
              <Skeleton w={70} />
              <Skeleton w={40} />
              <Skeleton w="30%" />
              <Skeleton w="20%" />
            </div>
          ))}
        </div>
      ) : requests.isError && !requests.data ? (
        <Callout tone="err" title="Couldn’t load activity" role="alert" action={<Button size="sm" icon={RefreshCw} onClick={() => requests.refetch()}>Retry</Button>}>
          {errorMessage(requests.error)}
        </Callout>
      ) : (
        <section className="card" aria-label="Requests">
          <div className="activity-stats" aria-live="polite">
            <span>
              <strong className="num">{formatNumber(stats.total)}</strong> {filtered ? 'matching' : 'recent'}
            </span>
            <span>
              <strong className="num">{formatPercent(stats.errors, stats.total)}</strong> failed
            </span>
            <span>
              p50 <strong className="num">{formatMs(stats.p50)}</strong>
            </span>
            <span>
              p95 <strong className="num">{formatMs(stats.p95)}</strong>
            </span>
            {stats.retried ? (
              <span title="Requests that failed over to another account or needed more than one attempt">
                <strong className="num">{formatNumber(stats.retried)}</strong> retried
              </span>
            ) : null}
            <span className="spacer" />
            {!live ? <Badge tone="warn">Paused</Badge> : <Badge tone="ok">Live</Badge>}
          </div>
          {rows.length ? (
            <RequestTable rows={rows} selectedId={selectedId} names={names} search={search} />
          ) : allRecords.length && filtered ? (
            <EmptyState
              icon={Search}
              title="No requests match"
              actions={
                <Button
                  onClick={() => {
                    setQuery('');
                    navigate('/activity', { replace: true });
                  }}
                >
                  Clear filters
                </Button>
              }
            >
              Try a broader search or clear the filters.
            </EmptyState>
          ) : (
            <EmptyState
              icon={ActivityIcon}
              title={filtered ? 'No requests match' : 'No requests yet'}
              actions={
                <>
                  <Button icon={FlaskConical} onClick={() => navigate('/playground')}>
                    Send a test request
                  </Button>
                  <Button variant="ghost" onClick={() => navigate('/clients')}>
                    Connect a client
                  </Button>
                </>
              }
            >
              Requests appear here live as clients use the gateway.
            </EmptyState>
          )}
        </section>
      )}

      <Dialog open={!!selectedId} onClose={() => navigate(`/activity${search}`)} sheet title="Request details" description={selected ? `${selected.model} · ${formatRelative(selected.timestamp)}` : undefined}>
        {selected ? <RequestDetail r={selected} names={names} explain={explainStatus} /> : <MissingRecord loading={requests.isPending || detail.isFetching} />}
      </Dialog>
    </>
  );
}

function RequestTable({ rows, selectedId, names, search }: { rows: RequestRecord[]; selectedId?: string; names: Map<string, string>; search: string }) {
  const [limit, setLimit] = useState(150);
  // Rows present at first render don't flash; anything that arrives later animates in once.
  const [initialIds] = useState(() => new Set(rows.map((r) => r.id)));
  const isFresh = (id: string) => !initialIds.has(id);
  const visible = rows.slice(0, limit);
  return (
    <>
      <div className="table-wrap activity-table">
        <table className="table">
          <caption className="sr-only">Requests, newest first</caption>
          <thead>
            <tr>
              <th scope="col">Time</th>
              <th scope="col">Status</th>
              <th scope="col">Model</th>
              <th scope="col">Connection</th>
              <th scope="col">Transport</th>
              <th scope="col" className="r">
                Latency
              </th>
              <th scope="col" className="r">
                Tokens in / out
              </th>
            </tr>
          </thead>
          <tbody>
            {visible.map((r) => {
              const href = `/activity/${encodeURIComponent(r.id)}${search}`;
              return (
                <tr
                  key={r.id}
                  className={`clickable ${r.id === selectedId ? 'selected' : ''} ${isFresh(r.id) ? 'fresh' : ''}`}
                  onClick={(e) => {
                    if ((e.target as HTMLElement).closest('a,button')) return;
                    navigate(href);
                  }}
                >
                  <td>
                    <Link to={href} className="row-time" title={formatDateTime(r.timestamp)}>
                      <span className="num">{formatTime(r.timestamp)}</span>
                      <span className="sr-only">, open details</span>
                    </Link>
                  </td>
                  <td>
                    <span className="row" style={{ gap: 6 }}>
                      <StatusCode record={r} />
                      <RetriedMark r={r} />
                    </span>
                  </td>
                  <td className="mono cell-model" title={r.route ? `${r.model} (route) → ${servedModel(r)}` : r.model}>
                    {r.model}
                    {r.route && servedModel(r) !== r.model ? <span className="muted served-as"> → {servedModel(r)}</span> : null}
                  </td>
                  <td className="cell-conn" title={r.connection_name ?? undefined}>
                    {(r.connection_id && names.get(r.connection_id)) || r.connection_name || <span className="muted">—</span>}
                  </td>
                  <td>
                    <Badge>{TRANSPORT_LABEL[r.transport] ?? r.transport}</Badge>
                  </td>
                  <td className="r num">{formatMs(r.latency_ms)}</td>
                  <td className="r num muted">
                    {r.input_tokens ?? r.output_tokens ? `${formatNumber(r.input_tokens ?? null)} / ${formatNumber(r.output_tokens ?? null)}` : '–'}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      <ul className="activity-cards" aria-label="Requests">
        {visible.map((r) => (
          <li key={r.id}>
            <Link to={`/activity/${encodeURIComponent(r.id)}${search}`} className={`activity-card ${isFresh(r.id) ? 'fresh' : ''}`}>
              <div className="row">
                <StatusCode record={r} />
                <RetriedMark r={r} />
                <span className="mono truncate">{r.model}</span>
                <span className="spacer" />
                <span className="num small">{formatMs(r.latency_ms)}</span>
              </div>
              <div className="row muted xs">
                <span className="truncate">{(r.connection_id && names.get(r.connection_id)) || r.connection_name || '—'}</span>
                <span>·</span>
                <span>{TRANSPORT_LABEL[r.transport] ?? r.transport}</span>
                <span className="spacer" />
                <span>{formatRelative(r.timestamp)}</span>
              </div>
              {r.error ? <div className="err xs truncate">{r.error}</div> : null}
            </Link>
          </li>
        ))}
      </ul>
      {rows.length > limit ? (
        <div className="card-foot" style={{ justifyContent: 'center' }}>
          <Button size="sm" onClick={() => setLimit((l) => l + 150)}>
            Show more ({formatNumber(rows.length - limit)} hidden)
          </Button>
        </div>
      ) : null}
    </>
  );
}

function RetriedMark({ r }: { r: RequestRecord }) {
  if (!wasRetried(r)) return null;
  const label = r.failovers ? `${r.failovers} ${r.failovers === 1 ? 'failover' : 'failovers'}` : `${r.attempts?.length ?? 0} attempts`;
  return (
    <span className="retried-mark" title={label}>
      <Shuffle aria-hidden />
      <span className="sr-only">{label}</span>
      {r.failovers || r.attempts?.length}
    </span>
  );
}

export function explainStatus(code: number | null): string {
  switch (code) {
    case 401:
      return 'Unauthorized: the client sent a missing or revoked Switchyard key, or the upstream rejected the stored credential.';
    case 403:
      return 'Forbidden by the upstream provider. Check the account’s plan and model access.';
    case 404:
      return 'No enabled connection or route serves this model. Check the model name, or add it to a connection.';
    case 424:
      return 'The provider rejected this account’s credentials. Sign in again, re-import, or replace the key on Connections. Your Switchyard session is unaffected.';
    case 409:
      return 'A follow-up referenced a response created on another account that Switchyard no longer tracks (for example after a restart). Start a new conversation in the client.';
    case 429:
      return 'Rate limited, or the gateway’s concurrency limit was reached. Round-robin across more accounts can help.';
    case 499:
      return 'The client disconnected before the response finished.';
    case 502:
      return 'The upstream provider returned an invalid response or the stream was interrupted.';
    case 503:
      return 'The gateway was paused, or every target for this route was unavailable.';
    case 504:
      return 'Timed out waiting for the upstream provider.';
    default:
      return code && code >= 500 ? 'An upstream or gateway error. Test the connection from Connections.' : 'The request did not succeed.';
  }
}

function MissingRecord({ loading }: { loading: boolean }) {
  if (loading) return <Skeleton h={120} />;
  return (
    <EmptyState icon={Copy} title="Not in the recent log" headingLevel={3}>
      Switchyard keeps the most recent 1,000 requests. This one has rolled off, or the link is from another gateway.
    </EmptyState>
  );
}
