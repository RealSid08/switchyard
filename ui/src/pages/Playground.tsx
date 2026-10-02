import { CircleStop, FlaskConical, Plug, Send, Unplug, Wrench, Zap } from 'lucide-react';
import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { useConnections, useModels, useOverview, useRoutes } from '../app/queries';
import { navigate, useLocation } from '../app/router';
import { CopyButton } from '../components/Code';
import { Badge, Button, Callout, EmptyState, Field, PageHead, Segmented, Skeleton } from '../components/ui';
import { api } from '../lib/client';
import { displayNames } from '../lib/connections';
import { formatMs, formatNumber } from '../lib/format';
import { FORMAT_LABELS } from '../lib/stream';
import type { Connection, ModelInfo, Route } from '../lib/types';
import { PlaygroundSocket, RunRecorder, initialRun, runHttp, type PlaygroundTransport, type RunState } from './playground/runner';

interface ModelOption {
  id: string;
  label: string;
  group: 'Routes' | 'Models';
  websocket: boolean;
  detail: string;
}

export function buildModelOptions(routes: Route[], models: ModelInfo[], connections: Connection[]): ModelOption[] {
  const names = displayNames(connections);
  const byId = new Map(connections.map((c) => [c.id, c]));
  const out: ModelOption[] = [];
  const seen = new Set<string>();
  for (const r of routes) {
    const targets = r.targets.map((t) => byId.get(t.connection_id)).filter((c): c is Connection => !!c && c.enabled);
    seen.add(r.model);
    out.push({
      id: r.model,
      label: r.model,
      group: 'Routes',
      websocket: targets.some((c) => c.supports_websocket),
      detail: `${r.strategy === 'failover' ? 'Failover' : 'Round robin'} across ${targets.length} enabled target${targets.length === 1 ? '' : 's'}`,
    });
  }
  const grouped = new Map<string, ModelInfo[]>();
  for (const m of models) grouped.set(m.id, [...(grouped.get(m.id) ?? []), m]);
  for (const [id, list] of [...grouped.entries()].sort((a, b) => a[0].localeCompare(b[0]))) {
    if (seen.has(id)) continue;
    out.push({
      id,
      label: id,
      group: 'Models',
      websocket: list.some((m) => m.supports_websocket),
      detail: list.length > 1 ? `${list.length} accounts, round robin` : (names.get(list[0].connection_id) ?? list[0].connection_name),
    });
  }
  return out;
}

const SAMPLE_PROMPT = 'In two sentences, explain what a railway switchyard does.';

export function PlaygroundPage() {
  const { search } = useLocation();
  const routes = useRoutes();
  const models = useModels();
  const connections = useConnections();
  const overview = useOverview();
  const options = useMemo(() => buildModelOptions(routes.data ?? [], models.data ?? [], connections.data ?? []), [routes.data, models.data, connections.data]);
  const urlModel = new URLSearchParams(search).get('model') ?? '';
  // A pick made in the select wins until the URL asks for a different model.
  const [pick, setPick] = useState<{ url: string; model: string } | null>(null);
  const [transportPref, setTransport] = useState<PlaygroundTransport>('sse');
  const [input, setInput] = useState(SAMPLE_PROMPT);
  const [run, setRun] = useState<RunState>(initialRun);
  const [tab, setTab] = useState<'output' | 'events' | 'raw'>('output');
  const [, forceSocket] = useState(0);
  const abortRef = useRef<AbortController | null>(null);
  const [socket] = useState(() => new PlaygroundSocket(() => forceSocket((n) => n + 1)));

  const model = pick && pick.url === urlModel ? pick.model : urlModel || options[0]?.id || '';
  const setModel = (m: string) => setPick({ url: urlModel, model: m });
  useEffect(
    () => () => {
      abortRef.current?.abort();
      socket.close();
    },
    [socket],
  );

  const selected = options.find((o) => o.id === model);
  const wsAvailable = !!selected?.websocket;
  const transport: PlaygroundTransport = transportPref === 'websocket' && selected && !wsAvailable ? 'sse' : transportPref;

  const send = async () => {
    if (run.running || !model || !input.trim()) return;
    const controller = new AbortController();
    abortRef.current = controller;
    setTab('output');
    const rec = new RunRecorder(setRun);
    if (transport === 'websocket') await socket.run(rec, model, input, controller.signal);
    else await runHttp(api, rec, { model, input, transport }, controller.signal);
    abortRef.current = null;
  };
  const stop = () => abortRef.current?.abort();

  const onKey = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      void send();
    }
  };

  const loading = routes.isPending || models.isPending;
  if (!loading && !options.length) {
    return (
      <>
        <PageHead title="Playground" description="Send a real request through the gateway and inspect every frame." />
        <div className="card">
          <EmptyState
            icon={FlaskConical}
            title="No models to try yet"
            actions={
              <Button variant="primary" icon={Plug} onClick={() => navigate('/connections')}>
                Add a connection
              </Button>
            }
          >
            {(connections.data?.length ?? 0) > 0 ? 'All connections are disabled. Enable one to try its models here.' : 'Connect an account or API provider, then come back to test it end to end.'}
          </EmptyState>
        </div>
      </>
    );
  }

  const r = run.result;
  const unknownModel = !!model && !loading && !selected;

  return (
    <>
      <PageHead
        title="Playground"
        description="Send a real request through the gateway over HTTP, SSE or WebSocket and inspect every frame. Prompts are not saved."
      />
      {overview.data?.paused ? (
        <Callout tone="warn" title="The gateway is paused">
          Playground requests go through the gateway too, so they’ll get 503 until you resume.
        </Callout>
      ) : null}
      <div className="playground">
        <section className="card card-pad stack composer" aria-label="Request">
          <Field label="Model" htmlFor="pg-model" hint={selected?.detail} error={unknownModel ? 'No enabled connection or route serves this model.' : undefined}>
            {loading ? (
              <Skeleton h={34} />
            ) : (
              <select id="pg-model" className="select mono" value={model} onChange={(e) => setModel(e.target.value)}>
                {unknownModel ? <option value={model}>{model} (unavailable)</option> : null}
                {(['Routes', 'Models'] as const).map((g) => {
                  const list = options.filter((o) => o.group === g);
                  return list.length ? (
                    <optgroup key={g} label={g}>
                      {list.map((o) => (
                        <option key={o.id} value={o.id}>
                          {o.label}
                          {o.websocket ? '  · WS' : ''}
                        </option>
                      ))}
                    </optgroup>
                  ) : null;
                })}
              </select>
            )}
          </Field>
          <div className="field">
            <span className="field-label" id="pg-transport-label">
              Transport
            </span>
            <Segmented
              label="Transport"
              value={transport}
              onChange={setTransport}
              options={[
                { value: 'http', label: 'HTTP' },
                { value: 'sse', label: 'SSE stream' },
                { value: 'websocket', label: 'WebSocket', icon: Zap, disabled: !wsAvailable, title: wsAvailable ? undefined : 'Not available for this model' },
              ]}
            />
            <span className="field-hint">
              {transport === 'websocket'
                ? 'Responses WebSocket mode. The socket stays open between turns for this model.'
                : wsAvailable
                  ? transport === 'http'
                    ? 'One JSON response when the model finishes.'
                    : 'Server-Sent Events, token by token.'
                  : 'WebSocket needs a model served by an OpenAI or Codex connection with WebSocket support.'}
            </span>
          </div>
          <Field label="Prompt" htmlFor="pg-input" hint={<>Send with <kbd>⌘</kbd> <kbd>Enter</kbd></>}>
            <textarea id="pg-input" className="textarea" rows={6} value={input} onChange={(e) => setInput(e.target.value)} onKeyDown={onKey} spellCheck />
          </Field>
          <div className="row">
            {run.running ? (
              <Button variant="danger" icon={CircleStop} onClick={stop}>
                Stop
              </Button>
            ) : (
              <Button variant="primary" icon={Send} onClick={() => void send()} disabled={!model || !input.trim() || unknownModel}>
                Send
              </Button>
            )}
            <span className="spacer" />
            {socket.isOpen ? (
              <span className="socket-state">
                <span className="dot dot-ok dot-pulse" aria-hidden />
                Socket open · {socket.turns} {socket.turns === 1 ? 'turn' : 'turns'}
                <Button size="sm" variant="ghost" icon={Unplug} onClick={() => socket.close()} disabled={run.running}>
                  Close
                </Button>
              </span>
            ) : null}
          </div>
        </section>

        <section className="card output-card" aria-label="Response">
          <Metrics run={run} />
          <div className="tabs" role="tablist" aria-label="Response views">
            {(
              [
                ['output', 'Output'],
                ['events', `Frames`],
                ['raw', 'Raw'],
              ] as const
            ).map(([id, label]) => (
              <button key={id} role="tab" id={`tab-${id}`} aria-controls={`panel-${id}`} aria-selected={tab === id} tabIndex={tab === id ? 0 : -1} onClick={() => setTab(id)}
                onKeyDown={(e) => {
                  const order = ['output', 'events', 'raw'] as const;
                  const i = order.indexOf(tab);
                  if (e.key === 'ArrowRight' || e.key === 'ArrowLeft') {
                    const next = order[(i + (e.key === 'ArrowRight' ? 1 : 2)) % 3];
                    setTab(next);
                    document.getElementById(`tab-${next}`)?.focus();
                  }
                }}>
                {label}
                {id === 'events' && run.frames.length ? <span className="count">{run.frames.length}</span> : null}
              </button>
            ))}
          </div>
          <div className="output-body" role="tabpanel" id={`panel-${tab}`} aria-labelledby={`tab-${tab}`}>
            {tab === 'output' ? <OutputView run={run} /> : tab === 'events' ? <FramesView run={run} /> : <RawView run={run} />}
          </div>
          <div className="sr-only" role="status" aria-live="polite">
            {!run.running && run.metrics.total !== null ? (r.error ? `Request failed: ${r.error}` : `Response complete in ${formatMs(run.metrics.total)}.`) : ''}
          </div>
        </section>
      </div>
    </>
  );
}

function Metrics({ run }: { run: RunState }) {
  const m = run.metrics;
  const r = run.result;
  const items: [string, string][] = [
    ['Status', m.status === null ? '–' : m.status === 101 ? 'WS 101' : String(m.status)],
    ['TTFB', formatMs(m.ttfb)],
    ['First token', formatMs(m.ttft)],
    ['Total', run.running ? '…' : formatMs(m.total)],
    ['Tokens in', formatNumber(r.usage.input)],
    ['Tokens out', formatNumber(r.usage.output)],
  ];
  return (
    <div className="metrics">
      {items.map(([k, v]) => (
        <div key={k} className="metric">
          <span className="metric-k">{k}</span>
          <span className={`metric-v num ${k === 'Status' && m.status && m.status >= 400 ? 'err' : ''}`}>{v}</span>
        </div>
      ))}
      <div className="metric metric-format">
        <span className="metric-k">Format</span>
        <span className="metric-v small">{r.format === 'unknown' ? (run.running ? 'detecting…' : '–') : FORMAT_LABELS[r.format]}</span>
      </div>
    </div>
  );
}

function OutputView({ run }: { run: RunState }) {
  const r = run.result;
  const empty = !run.running && !r.text && !r.toolCalls.length && !r.reasoning && !r.error && !run.frames.length;
  if (empty) {
    return <p className="muted small output-placeholder">The response renders here as it streams. Frames and timings appear alongside.</p>;
  }
  return (
    <div className="stack">
      {r.reasoning ? (
        <details className="reasoning">
          <summary>Reasoning</summary>
          <p>{r.reasoning}</p>
        </details>
      ) : null}
      <div className="output-text" aria-live="polite" aria-atomic="false" aria-busy={run.running && !r.text}>
        {r.text}
        {run.running ? <span className="caret" aria-hidden /> : null}
      </div>
      {r.toolCalls.map((t) => (
        <div key={t.key} className="tool-call">
          <div className="row">
            <Wrench aria-hidden width={14} height={14} className="muted" />
            <span className="mono small">{t.name}</span>
            <span className="muted xs truncate">{t.id}</span>
          </div>
          <pre className="mono">{prettyArgs(t.arguments)}</pre>
        </div>
      ))}
      {r.error ? (
        <Callout tone="err" title="Request failed" role="alert">
          {r.error}
        </Callout>
      ) : null}
      {!run.running && (r.stopReason || run.note || r.model) ? (
        <div className="row row-wrap muted xs">
          {r.model ? <Badge>{r.model}</Badge> : null}
          {r.stopReason ? <Badge>stop: {r.stopReason}</Badge> : null}
          {run.note ? <span>{run.note}</span> : null}
        </div>
      ) : null}
    </div>
  );
}

function prettyArgs(args: string): string {
  try {
    return JSON.stringify(JSON.parse(args), null, 2);
  } catch {
    return args || '{}';
  }
}

function FramesView({ run }: { run: RunState }) {
  if (!run.frames.length) return <p className="muted small output-placeholder">No frames yet.</p>;
  return (
    <ol className="frames">
      {run.frames.map((f) => (
        <li key={f.seq} className={`frame frame-${f.dir}`}>
          <details>
            <summary>
              <span className="num frame-t">+{f.t}ms</span>
              <span className="frame-dir" aria-label={f.dir === 'out' ? 'sent' : f.dir === 'in' ? 'received' : 'note'}>
                {f.dir === 'out' ? '↑' : f.dir === 'in' ? '↓' : '•'}
              </span>
              <span className="mono frame-label truncate">{f.label}</span>
              <span className="muted xs num">{f.bytes} B</span>
            </summary>
            <pre className="mono">{prettyArgs(f.data)}</pre>
          </details>
        </li>
      ))}
    </ol>
  );
}

function RawView({ run }: { run: RunState }) {
  const raw = run.frames
    .filter((f) => f.dir === 'in')
    .map((f) => f.data)
    .join('\n');
  if (!raw) return <p className="muted small output-placeholder">Nothing received yet.</p>;
  return (
    <div className="raw-view">
      <div className="raw-copy">
        <CopyButton text={raw} label="Copy raw response" />
      </div>
      <pre className="mono">{raw}</pre>
    </div>
  );
}
