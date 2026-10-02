/**
 * Switchyard mock backend for UI development. DEV ONLY: never bundled or shipped.
 *
 * Implements the admin API contract in memory (starts EMPTY), including SSE and
 * WebSocket endpoints, so the UI can be exercised without the Rust gateway.
 *
 *   node mock/server.ts [--seed] [--port 5181]
 *   MOCK_AUTH=token  -> behave like a non-loopback browser (needs the admin token)
 *
 * Control endpoints (POST): /api/__mock/{seed,reset,traffic-on,traffic-off,burst,
 * drop-events,events-off,events-on,expire-session}
 */
import { randomUUID } from 'node:crypto';
import { createServer, type IncomingMessage, type ServerResponse } from 'node:http';
import { pathToFileURL } from 'node:url';
import { WebSocketServer, type WebSocket } from 'ws';
import { createUsageMock } from './usage.ts';

type Kind = 'openai' | 'anthropic' | 'gemini' | 'codex' | 'antigravity';
interface Conn {
  id: string;
  name: string;
  kind: Kind;
  base_url: string;
  enabled: boolean;
  models: string[];
  supports_websocket: boolean;
  created_at: string;
  secret: string;
  source: 'api_key' | 'native_codex' | 'native_claude' | 'native_antigravity' | 'native_opencode' | 'cliproxy' | 'oauth';
  /** Unix seconds the credential expires, if known. */
  expires?: number;
}
interface Route {
  model: string;
  targets: { connection_id: string; model: string }[];
  strategy: 'round_robin' | 'failover';
}
interface Rec {
  id: string;
  timestamp: string;
  model: string;
  connection_id: string;
  connection_name: string;
  transport: 'http' | 'sse' | 'websocket';
  status: number;
  latency_ms: number;
  input_tokens: number | null;
  output_tokens: number | null;
  error: string | null;
  route?: string | null;
  failovers?: number;
  attempts?: { connection_id: string; connection_name: string; model: string; status: number; duration_ms: number; error: string | null }[];
  ttfb_ms?: number | null;
  first_token_ms?: number | null;
}
interface Key {
  id: string;
  name: string;
  prefix: string;
  created_at: string;
  key: string;
}

export const ADMIN_TOKEN = 'sy_admin_mockmockmockmockmockmock';

export function createMockServer(opts: { auth?: 'cookie' | 'token'; seed?: boolean } = {}) {
  const authMode = opts.auth ?? 'cookie';
  const started = Date.now();
  // One entry per browser session, like the gateway (DELETE /api/session ends just one).
  const sessions = new Set<string>();
  let connections: Conn[] = [];
  let routes: Route[] = [];
  let keys: Key[] = [];
  let requests: Rec[] = [];
  let paused = false;
  let counters = { total: 0, success: 0, failed: 0, http: 0, sse: 0, websocket: 0 };
  let active = 0;
  let cursor = 0;
  let trafficTimer: NodeJS.Timeout | null = null;
  let generation = 0;
  let eventsEnabled = true;
  const eventSockets = new Set<WebSocket>();

  const now = () => new Date().toISOString();
  /** Cooldowns per connection: model ("*" = whole account) until a wall-clock time. */
  const cooldowns = new Map<string, { model: string; until: number }[]>();
  let catalogReject = false;
  const health = (c: Conn) => {
    const now = Date.now();
    const active = (cooldowns.get(c.id) ?? []).filter((x) => x.until > now);
    const last = requests.find((r) => r.connection_id === c.id);
    return {
      status: !c.enabled ? 'disabled' : active.some((x) => x.model === '*') ? 'cooling' : active.length ? 'limited' : 'ready',
      cooldowns: active.map((x) => ({ model: x.model, retry_after_seconds: Math.ceil((x.until - now) / 1000) })),
      last_used_at: last?.timestamp ?? null,
      last_status: last?.status ?? null,
      last_error: last?.error ? (last.attempts?.at(-1)?.error ?? last.error) : null,
    };
  };
  const publicConn = ({ secret, source, expires, ...c }: Conn) => ({
    ...c,
    credential_present: secret.length > 0,
    credential_source: source,
    credential_expires_at: expires ?? null,
    health: health({ ...c, secret, source }),
  });

  /* ---------- browser sign-in (OAuth) ---------- */
  interface Flow {
    id: string;
    provider: 'codex' | 'claude' | 'antigravity';
    state: string;
    status: 'pending' | 'complete' | 'error' | 'expired';
    expires: number;
    connection?: ReturnType<typeof publicConn>;
    message?: string;
  }
  const flows = new Map<string, Flow>();
  let oauthTtl = 300;
  let oauthBusy = false;
  const OAUTH_PORTS = { codex: 1455, claude: 54545, antigravity: 51121 } as const;
  const flowView = (f: Flow) => {
    if (f.status === 'pending' && Date.now() > f.expires) {
      f.status = 'expired';
      f.message = 'Sign-in expired. Start again from the control room.';
    }
    return {
      id: f.id,
      provider: f.provider,
      status: f.status,
      ...(f.status === 'pending' ? { expires_in_seconds: Math.max(0, Math.ceil((f.expires - Date.now()) / 1000)) } : {}),
      ...(f.connection ? { connection: f.connection } : {}),
      ...(f.message ? { message: f.message } : {}),
    };
  };
  /** Completing a sign-in creates (or refreshes, for the same account) a gateway-owned connection. */
  function completeFlow(f: Flow, account: string) {
    const kind: Kind = f.provider === 'codex' ? 'codex' : f.provider === 'antigravity' ? 'antigravity' : 'anthropic';
    const secret = `oauth-${f.provider}-${account}`;
    let c = connections.find((x) => x.kind === kind && x.secret === secret);
    if (!c) {
      c = {
        id: randomUUID(),
        name: f.provider === 'codex' ? `ChatGPT · ${account}` : f.provider === 'antigravity' ? `Antigravity · ${account}` : `Claude · ${account}`,
        kind,
        base_url: f.provider === 'codex' ? 'https://chatgpt.com/backend-api/codex' : f.provider === 'antigravity' ? 'https://daily-cloudcode-pa.googleapis.com' : 'https://api.anthropic.com/v1',
        enabled: true,
        models: f.provider === 'codex' ? ['gpt-6.1-sol', 'gpt-6-astra', 'gpt-6-luna'] : f.provider === 'antigravity' ? ['gemini-3-pro', 'claude-sonnet-5-5'] : ['claude-opus-5-5', 'claude-sonnet-5-5'],
        supports_websocket: f.provider === 'codex',
        created_at: now(),
        secret,
        source: 'oauth',
      };
      connections.push(c);
    }
    f.status = 'complete';
    f.connection = publicConn(c);
    broadcast({ type: 'overview', data: overview() });
  }

  function overview() {
    const lat = requests.map((r) => r.latency_ms).sort((a, b) => a - b);
    const buckets = new Map<number, { requests: number; errors: number }>();
    for (const r of requests) {
      const t = Math.floor(Date.parse(r.timestamp) / 60000) * 60;
      const b = buckets.get(t) ?? { requests: 0, errors: 0 };
      b.requests++;
      if (r.status >= 400) b.errors++;
      buckets.set(t, b);
    }
    return {
      version: '0.1.0-mock',
      uptime_seconds: Math.floor((Date.now() - started) / 1000),
      paused,
      requests_total: counters.total,
      requests_success: counters.success,
      requests_failed: counters.failed,
      active_requests: active,
      connections_total: connections.length,
      connections_enabled: connections.filter((c) => c.enabled).length,
      latency_ms_p50: lat[Math.floor(lat.length / 2)] ?? 0,
      transport_counts: { http: counters.http, sse: counters.sse, websocket: counters.websocket },
      recent_requests: requests.slice(0, 20),
      series: [...buckets.entries()].sort((a, b) => a[0] - b[0]).map(([t, b]) => ({ timestamp: new Date(t * 1000).toISOString(), ...b })),
    };
  }

  function broadcast(msg: unknown) {
    const text = JSON.stringify(msg);
    for (const ws of eventSockets) if (ws.readyState === ws.OPEN) ws.send(text);
  }

  // Usage ledger, limits and pricing (dev-only fixtures, see mock/usage.ts).
  const usageMock = createUsageMock({
    connections: () => connections.map((c) => ({ id: c.id, name: c.name, kind: c.kind, enabled: c.enabled, source: c.source, models: c.models })),
    keys: () => keys.map((k) => ({ id: k.id, name: k.name })),
  });

  function record(r: Omit<Rec, 'id' | 'timestamp'> & { timestamp?: string }, push = true) {
    const rec: Rec = { id: randomUUID(), timestamp: r.timestamp ?? now(), ...r } as Rec;
    usageMock.recordGateway(rec);
    requests.unshift(rec);
    requests = requests.slice(0, 1000);
    counters.total++;
    counters[rec.status < 400 ? 'success' : 'failed']++;
    counters[rec.transport]++;
    if (push) {
      broadcast({ type: 'request', data: rec });
      broadcast({ type: 'overview', data: overview() });
    }
    return rec;
  }

  function candidates(model: string, ws = false): { conn: Conn; model: string }[] {
    const route = routes.find((r) => r.model === model);
    let list = route
      ? route.targets.flatMap((t) => {
          const c = connections.find((x) => x.id === t.connection_id && x.enabled && (!ws || x.supports_websocket));
          return c ? [{ conn: c, model: t.model }] : [];
        })
      : connections.filter((c) => c.enabled && c.models.includes(model) && (!ws || c.supports_websocket)).map((c) => ({ conn: c, model }));
    if ((!route || route.strategy === 'round_robin') && list.length) {
      const n = cursor++ % list.length;
      list = [...list.slice(n), ...list.slice(0, n)];
    }
    return list;
  }

  /* ---------- seed data ---------- */
  function seed() {
    const t = (minsAgo: number) => new Date(Date.now() - minsAgo * 60000).toISOString();
    connections = [
      { id: randomUUID(), name: 'Codex', kind: 'codex', base_url: 'https://chatgpt.com/backend-api/codex', enabled: true, models: ['gpt-6.1-sol', 'gpt-6-astra', 'gpt-6-luna'], supports_websocket: true, created_at: t(4000), secret: 'x', source: 'oauth' },
      { id: randomUUID(), name: 'Codex', kind: 'codex', base_url: 'https://chatgpt.com/backend-api/codex', enabled: true, models: ['gpt-6.1-sol', 'gpt-6-astra', 'gpt-6-luna'], supports_websocket: true, created_at: t(3000), secret: 'x', source: 'native_codex' },
      { id: randomUUID(), name: 'Claude Code', kind: 'anthropic', base_url: 'https://api.anthropic.com/v1', enabled: true, models: ['claude-opus-5-5', 'claude-sonnet-5-5'], supports_websocket: false, created_at: t(2000), secret: 'x', source: 'native_claude' },
      { id: randomUUID(), name: 'Gemini', kind: 'gemini', base_url: 'https://generativelanguage.googleapis.com/v1beta', enabled: true, models: ['gemini-3-pro'], supports_websocket: false, created_at: t(1500), secret: 'x', source: 'api_key' },
      { id: randomUUID(), name: 'Ollama', kind: 'openai', base_url: 'http://127.0.0.1:11434/v1', enabled: false, models: ['qwen3-coder:30b', 'llama4:scout'], supports_websocket: false, created_at: t(1000), secret: '', source: 'api_key' },
    ];
    const [codexA, codexB, claude] = connections;
    const nowS = Math.floor(Date.now() / 1000);
    codexB.expires = nowS + 5 * 3600; // CLI login that will need a re-login soon
    claude.expires = nowS + 9 * 86400;
    cooldowns.clear();
    cooldowns.set(codexA.id, [{ model: 'gpt-6.1-sol', until: Date.now() + 95_000 }]);
    routes = [
      { model: 'coding', strategy: 'failover', targets: [{ connection_id: codexA.id, model: 'gpt-6.1-sol' }, { connection_id: claude.id, model: 'claude-opus-5-5' }] },
      { model: 'gpt-6.1-sol', strategy: 'round_robin', targets: [{ connection_id: codexA.id, model: 'gpt-6.1-sol' }, { connection_id: codexB.id, model: 'gpt-6.1-sol' }] },
    ];
    keys = [
      { id: randomUUID(), name: 'Codex on laptop', prefix: 'sy_3f9a1c2b', created_at: t(3900), key: 'sy_mock1' },
      { id: randomUUID(), name: 'Claude Code', prefix: 'sy_81bd04e7', created_at: t(1900), key: 'sy_mock2' },
    ];
    requests = [];
    counters = { total: 0, success: 0, failed: 0, http: 0, sse: 0, websocket: 0 };
    usageMock.reset();
    usageMock.seedHistory();
    for (let i = 240; i > 0; i--) record({ ...randomRequest(), timestamp: new Date(Date.now() - i * 14_000 - Math.random() * 9000).toISOString() }, false);
    requests.sort((a, b) => Date.parse(b.timestamp) - Date.parse(a.timestamp));
    broadcast({ type: 'overview', data: overview() });
  }

  function randomRequest(): Omit<Rec, 'id' | 'timestamp'> {
    const base = baseRequest();
    if (base.status === 401) return { ...base, attempts: [], failovers: 0 };
    const c = connections.find((x) => x.id === base.connection_id)!;
    const route = base.route ? routes.find((r) => r.model === base.route) : undefined;
    const upstream = route?.targets.find((t) => t.connection_id === c.id)?.model ?? base.model;
    const total = base.latency_ms;
    const streamed = base.transport !== 'http';
    const attempts: NonNullable<Rec['attempts']> = [];
    let failovers = 0;
    // Some routed requests fail over: another target was rate limited or down first.
    const other = route?.targets.find((t) => t.connection_id !== c.id);
    const otherConn = other && connections.find((x) => x.id === other.connection_id);
    if (otherConn && base.status === 200 && Math.random() < 0.3) {
      const failed = Math.random() < 0.7 ? { status: 429, error: 'rate_limited' } : { status: 503, error: 'provider_unavailable' };
      attempts.push({ connection_id: otherConn.id, connection_name: otherConn.name, model: other!.model, ...failed, duration_ms: Math.round(80 + Math.random() * 400) });
      failovers = 1;
    }
    const errLabel: Record<number, string> = { 429: 'rate_limited', 502: 'provider_unavailable', 504: 'transport_failed' };
    attempts.push({
      connection_id: c.id,
      connection_name: c.name,
      model: upstream,
      status: base.status === 504 ? 0 : base.transport === 'websocket' && base.status === 200 ? 101 : base.status,
      duration_ms: Math.round(Math.min(total, 120 + Math.random() * 500)),
      error: base.status === 200 ? null : (errLabel[base.status] ?? 'request_rejected'),
    });
    const before = attempts.slice(0, -1).reduce((n, a) => n + a.duration_ms, 0);
    const ttfb = base.status === 200 ? before + Math.round(150 + Math.random() * 700) : null;
    return {
      ...base,
      latency_ms: total + before,
      failovers,
      attempts,
      ttfb_ms: ttfb,
      first_token_ms: ttfb !== null && streamed ? ttfb + Math.round(40 + Math.random() * 600) : null,
    };
  }

  function baseRequest(): Omit<Rec, 'id' | 'timestamp'> {
    const enabled = connections.filter((c) => c.enabled);
    let c = enabled[Math.floor(Math.random() * enabled.length)] ?? connections[0];
    let model = c.models[Math.floor(Math.random() * c.models.length)];
    const route = Math.random() < 0.35 ? routes[Math.floor(Math.random() * routes.length)] : undefined;
    const target = route?.targets[Math.floor(Math.random() * route.targets.length)];
    const viaRoute = target && enabled.find((x) => x.id === target.connection_id);
    let routeName: string | null = null;
    if (route && viaRoute) {
      c = viaRoute;
      model = route.model;
      routeName = route.model;
    }
    const transport: Rec['transport'] = c.supports_websocket && Math.random() < 0.55 ? 'websocket' : Math.random() < 0.75 ? 'sse' : 'http';
    const roll = Math.random();
    const status = roll < 0.9 ? 200 : roll < 0.94 ? 429 : roll < 0.97 ? 502 : roll < 0.99 ? 401 : 504;
    const errors: Record<number, string> = {
      429: 'Upstream rate limit reached',
      502: 'Provider stream failed',
      401: 'A valid Switchyard client API key is required',
      504: 'Provider timed out',
    };
    return {
      model,
      connection_id: c.id,
      connection_name: c.name,
      transport,
      status,
      latency_ms: Math.round(status === 504 ? 300000 : 300 + Math.random() * (transport === 'http' ? 2400 : 9000)),
      input_tokens: status === 200 ? Math.round(400 + Math.random() * 24000) : null,
      output_tokens: status === 200 ? Math.round(20 + Math.random() * 2400) : null,
      error: status === 200 ? null : errors[status],
      route: routeName,
    };
  }

  function reset() {
    generation++;
    usageMock.reset();
    flows.clear();
    cooldowns.clear();
    catalogReject = false;
    oauthTtl = 300;
    oauthBusy = false;
    connections = [];
    routes = [];
    keys = [];
    requests = [];
    counters = { total: 0, success: 0, failed: 0, http: 0, sse: 0, websocket: 0 };
    paused = false;
    if (trafficTimer) clearInterval(trafficTimer);
    trafficTimer = null;
    broadcast({ type: 'overview', data: overview() });
  }

  if (opts.seed) seed();

  /* ---------- helpers ---------- */
  const json = (res: ServerResponse, status: number, body?: unknown) => {
    res.writeHead(status, { 'content-type': 'application/json', 'cache-control': 'no-store' });
    res.end(body === undefined ? '' : JSON.stringify(body));
  };
  const fail = (res: ServerResponse, status: number, message: string) => json(res, status, { error: { message, type: 'gateway_error' } });
  const readBody = (req: IncomingMessage) =>
    new Promise<any>((resolve) => {
      let data = '';
      req.on('data', (c) => (data += c));
      req.on('end', () => {
        try {
          resolve(data ? JSON.parse(data) : {});
        } catch {
          resolve(null);
        }
      });
    });
  const cookieToken = (req: IncomingMessage) =>
    (req.headers.cookie ?? '')
      .split(';')
      .map((c) => c.trim())
      .find((c) => c.startsWith('sy_session='))
      ?.slice('sy_session='.length) ?? null;
  const hasCookie = (req: IncomingMessage) => {
    const t = cookieToken(req);
    return !!t && sessions.has(t);
  };
  const bearer = (req: IncomingMessage) => {
    const h = req.headers.authorization;
    return h?.startsWith('Bearer ')
      ? h.slice(7)
      : ((req.headers['x-api-key'] as string | undefined) ?? (req.headers['x-goog-api-key'] as string | undefined) ?? null);
  };
  const isAdmin = (req: IncomingMessage) => hasCookie(req) || bearer(req) === ADMIN_TOKEN;
  const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

  function validate(v: any): string | null {
    if (!v || typeof v !== 'object') return 'Invalid JSON body';
    if (typeof v.name !== 'string' || !v.name.trim() || v.name.length > 100) return 'Connection name must be 1–100 characters';
    if (!['openai', 'anthropic', 'gemini', 'codex'].includes(v.kind)) return 'Unknown provider kind';
    let u: URL;
    try {
      u = new URL(v.base_url);
    } catch {
      return 'Invalid provider base URL';
    }
    if (!['http:', 'https:'].includes(u.protocol) || u.username || u.search || u.hash) return 'Use an HTTP(S) base URL without credentials, query or fragment';
    if (u.protocol === 'http:' && !['localhost', '127.0.0.1', '[::1]'].includes(u.hostname)) return 'HTTP providers must be on loopback. Use HTTPS for remote providers.';
    if (!Array.isArray(v.models) || !v.models.length || v.models.length > 100) return 'Provide 1–100 model identifiers';
    if (v.supports_websocket && !['openai', 'codex'].includes(v.kind)) return 'Responses WebSocket requires an OpenAI or Codex connection';
    return null;
  }

  /* ---------- streaming playground ---------- */
  const WORDS = 'A switchyard sorts railcars onto the right tracks so each train leaves with exactly the cars it needs. It turns a jumble of arrivals into orderly departures, one switch at a time.'.split(' ');

  async function streamPlayground(res: ServerResponse, kind: Kind, model: string, stream: boolean, signal: { closed: boolean }) {
    const text = WORDS.map((w, i) => (i ? ' ' : '') + w);
    const usage = { input_tokens: 23, output_tokens: text.length };
    if (!stream) {
      await sleep(600);
      const full = text.join('');
      if (kind === 'anthropic') return json(res, 200, { id: 'msg_mock', type: 'message', role: 'assistant', model, content: [{ type: 'text', text: full }], stop_reason: 'end_turn', usage });
      if (kind === 'gemini')
        return json(res, 200, { candidates: [{ content: { role: 'model', parts: [{ text: full }] }, finishReason: 'STOP' }], usageMetadata: { promptTokenCount: 23, candidatesTokenCount: text.length }, modelVersion: model });
      return json(res, 200, { id: 'resp_mock', object: 'response', model, status: 'completed', output: [{ type: 'message', role: 'assistant', content: [{ type: 'output_text', text: full }] }], usage });
    }
    res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
    const send = (event: string | null, data: unknown) => res.write(`${event ? `event: ${event}\n` : ''}data: ${JSON.stringify(data)}\n\n`);
    await sleep(250);
    if (kind === 'anthropic') {
      send('message_start', { type: 'message_start', message: { id: 'msg_mock', model, usage: { input_tokens: 23, output_tokens: 1 } } });
      send('content_block_start', { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } });
      for (const t of text) {
        if (signal.closed) return;
        await sleep(35);
        send('content_block_delta', { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: t } });
      }
      send('content_block_stop', { type: 'content_block_stop', index: 0 });
      send('message_delta', { type: 'message_delta', delta: { stop_reason: 'end_turn' }, usage: { output_tokens: text.length } });
      send('message_stop', { type: 'message_stop' });
    } else if (kind === 'gemini') {
      for (const t of text) {
        if (signal.closed) return;
        await sleep(35);
        send(null, { candidates: [{ content: { role: 'model', parts: [{ text: t }] } }], modelVersion: model });
      }
      send(null, { candidates: [{ content: { role: 'model', parts: [{ text: '' }] }, finishReason: 'STOP' }], usageMetadata: { promptTokenCount: 23, candidatesTokenCount: text.length } });
    } else {
      send('response.created', { type: 'response.created', response: { id: 'resp_mock', model, status: 'in_progress' } });
      for (const t of text) {
        if (signal.closed) return;
        await sleep(35);
        send('response.output_text.delta', { type: 'response.output_text.delta', item_id: 'msg_1', output_index: 0, delta: t });
      }
      send('response.completed', { type: 'response.completed', response: { id: 'resp_mock', model, status: 'completed', usage } });
    }
    res.end();
  }

  /* ---------- HTTP ---------- */
  const server = createServer(async (req, res) => {
    const url = new URL(req.url ?? '/', 'http://mock');
    const path = url.pathname;
    const method = req.method ?? 'GET';

    if (path === '/healthz') return json(res, 200, { status: 'ok' });

    // Stand-in for the provider's consent page. `remote=1` mimics a browser on another
    // machine: the loopback redirect can't reach the gateway, so nothing completes.
    if (path === '/__mock/oauth/authorize') {
      const f = flows.get(url.searchParams.get('id') ?? '');
      const remote = url.searchParams.get('remote') === '1';
      if (f && f.status === 'pending' && !remote && url.searchParams.get('state') === f.state) completeFlow(f, url.searchParams.get('account') ?? 'you@example.com');
      res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
      res.end(
        f && !remote
          ? '<!doctype html><title>Signed in</title><p>Signed in. You can close this tab and return to Switchyard.</p>'
          : `<!doctype html><title>localhost</title><p>This site can’t be reached (mock). Copy this address: http://localhost:${f ? OAUTH_PORTS[f.provider] : 1455}/auth/callback?code=mockcode&amp;state=${f?.state ?? ''}</p>`,
      );
      return;
    }

    if (path === '/api/session' && method === 'DELETE') {
      const t = cookieToken(req);
      if (t) sessions.delete(t);
      res.setHeader('set-cookie', 'sy_session=; HttpOnly; SameSite=Strict; Path=/api; Max-Age=0');
      return json(res, 200, { authenticated: false });
    }
    if (path === '/api/session') {
      const tokenOk = bearer(req) === ADMIN_TOKEN;
      const session = randomUUID();
      sessions.add(session);
      if (!tokenOk && authMode === 'token') return fail(res, 401, 'Open the local dashboard or provide the admin token');
      res.setHeader('set-cookie', `sy_session=${session}; HttpOnly; SameSite=Strict; Path=/api; Max-Age=43200`);
      return json(res, 200, { authenticated: true });
    }

    if (path.startsWith('/api/__mock/') && method === 'POST') {
      const action = path.slice('/api/__mock/'.length);
      switch (action) {
        case 'seed':
          seed();
          break;
        case 'monitor-auth':
          usageMock.breakMonitor();
          break;
        case 'catalog-reject':
          catalogReject = true;
          break;
        case 'catalog-accept':
          catalogReject = false;
          break;
        case 'cooldown': {
          // Bench the first enabled Codex account for two minutes.
          const c = connections.find((x) => x.enabled && x.kind === 'codex');
          if (c) cooldowns.set(c.id, [{ model: '*', until: Date.now() + 120_000 }]);
          break;
        }
        case 'oauth-busy':
          oauthBusy = true;
          break;
        case 'oauth-free':
          oauthBusy = false;
          break;
        case 'oauth-ttl':
          oauthTtl = Number(url.searchParams.get('seconds') ?? 300);
          break;
        case 'reset':
          reset();
          break;
        case 'traffic-on':
          if (!trafficTimer && connections.length)
            trafficTimer = setInterval(() => connections.some((c) => c.enabled) && !paused && record(randomRequest()), 1800);
          break;
        case 'traffic-off':
          if (trafficTimer) clearInterval(trafficTimer);
          trafficTimer = null;
          break;
        case 'burst': {
          const current = generation;
          if (connections.some((c) => c.enabled)) for (let i = 0; i < 25; i++) setTimeout(() => {
            // A later test may reset the fixture while this burst is queued.
            if (generation === current && !paused && connections.some((c) => c.enabled)) record(randomRequest());
          }, i * 80);
          break;
        }
        case 'drop-events':
          for (const ws of eventSockets) ws.terminate();
          break;
        case 'events-off':
          eventsEnabled = false;
          for (const ws of eventSockets) ws.terminate();
          break;
        case 'events-on':
          eventsEnabled = true;
          break;
        case 'expire-session':
          sessions.clear();
          for (const ws of eventSockets) ws.terminate();
          break;
        default:
          return fail(res, 404, 'Unknown mock action');
      }
      return json(res, 200, { ok: true });
    }

    if (path.startsWith('/api/')) {
      if (!isAdmin(req)) return fail(res, 401, 'Admin session required');
      await sleep(40 + Math.random() * 80); // feel like a network
      const body = method === 'POST' || method === 'PUT' ? await readBody(req) : null;
      if (path.startsWith('/api/usage')) {
        const u = await usageMock.handle(path, method, url, body);
        if (u) {
          if (u.status === 204) {
            res.writeHead(204).end();
            return;
          }
          return json(res, u.status, u.body);
        }
      }
      const seg = path.split('/').filter(Boolean); // ['api', ...]

      if (path === '/api/overview' && method === 'GET') return json(res, 200, overview());
      if (path === '/api/config' && method === 'GET') {
        const host = req.headers.host ?? '127.0.0.1:7410';
        return json(res, 200, { host: '127.0.0.1', port: 7410, api_base: `http://${host}/v1`, websocket_url: `ws://${host}/v1/responses`, requires_api_key: true, max_in_flight: 64, request_timeout_seconds: 300 });
      }
      if (path === '/api/settings' && method === 'POST') {
        if (typeof body?.paused !== 'boolean') return fail(res, 400, 'paused must be a boolean');
        paused = body.paused;
        broadcast({ type: 'overview', data: overview() });
        return json(res, 200, overview());
      }
      if (path === '/api/connections' && method === 'GET') return json(res, 200, connections.map(publicConn));
      if (path === '/api/connections' && method === 'POST') {
        const err = validate(body);
        if (err) return fail(res, 400, err);
        const c: Conn = { id: randomUUID(), name: body.name, kind: body.kind, base_url: body.base_url.replace(/\/+$/, ''), enabled: body.enabled ?? true, models: body.models, supports_websocket: !!body.supports_websocket, created_at: now(), secret: body.api_key ?? '', source: 'api_key' };
        connections.push(c);
        broadcast({ type: 'overview', data: overview() });
        return json(res, 200, publicConn(c));
      }
      if (seg[1] === 'connections' && seg.length === 3) {
        const c = connections.find((x) => x.id === decodeURIComponent(seg[2]));
        if (!c) return fail(res, 404, 'Connection not found');
        if (method === 'PUT') {
          const err = validate(body);
          if (err) return fail(res, 400, err);
          Object.assign(c, { name: body.name, kind: body.kind, base_url: body.base_url.replace(/\/+$/, ''), enabled: body.enabled ?? true, models: body.models, supports_websocket: !!body.supports_websocket });
          if (body.api_key) {
            c.secret = body.api_key;
            c.source = 'api_key';
          }
          broadcast({ type: 'overview', data: overview() });
          return json(res, 200, publicConn(c));
        }
        if (method === 'DELETE') {
          if (routes.some((r) => r.targets.some((t) => t.connection_id === c.id))) return fail(res, 409, 'Remove this connection from its model routes first');
          connections = connections.filter((x) => x !== c);
          broadcast({ type: 'overview', data: overview() });
          res.writeHead(204).end();
          return;
        }
      }
      if (seg[1] === 'connections' && seg[3] === 'models' && method === 'GET') {
        const c = connections.find((x) => x.id === decodeURIComponent(seg[2]));
        if (!c) return fail(res, 404, 'Connection not found');
        await sleep(350 + Math.random() * 400);
        const local = c.base_url.includes('127.0.0.1');
        if (catalogReject || (!c.secret && !local)) return fail(res, 424, 'Provider model catalog unavailable. Check the account or enter model identifiers manually.');
        const catalogs: Record<string, [string, string][]> = {
          codex: [
            ['gpt-6.1-sol', 'GPT-6.1 Sol'],
            ['gpt-6-astra', 'GPT-6 Astra'],
            ['gpt-6-luna', 'GPT-6 Luna'],
            ['gpt-6.1-sol-mini', 'GPT-6.1 Sol mini'],
            ['gpt-6-astra-codex', 'GPT-6 Astra (Codex)'],
            ['gpt-6-nova', 'GPT-6 Nova'],
            ['gpt-5.5', 'GPT-5.5'],
            ['gpt-5.5-codex', 'GPT-5.5 Codex'],
            ['gpt-5.5-mini', 'GPT-5.5 mini'],
            ['codex-mini-latest', 'Codex mini'],
          ],
          anthropic: [
            ['claude-opus-5-5', 'Claude Opus 5.5'],
            ['claude-sonnet-5-5', 'Claude Sonnet 5.5'],
            ['claude-fable-5-1', 'Claude Fable 5.1'],
            ['claude-haiku-4-5', 'Claude Haiku 4.5'],
            ['claude-opus-5-1', 'Claude Opus 5.1'],
            ['claude-sonnet-5', 'Claude Sonnet 5'],
            ['claude-opus-4-5', 'Claude Opus 4.5'],
            ['claude-sonnet-4-5', 'Claude Sonnet 4.5'],
          ],
          gemini: [
            ['gemini-3-pro', 'Gemini 3 Pro'],
            ['gemini-3-flash', 'Gemini 3 Flash'],
            ['gemini-3-flash-lite', 'Gemini 3 Flash-Lite'],
          ],
          openai: [
            ['qwen3-coder:30b', 'qwen3-coder:30b'],
            ['llama4:scout', 'llama4:scout'],
            ['gpt-oss:20b', 'gpt-oss:20b'],
          ],
        };
        const list = catalogs[c.kind] ?? [];
        return json(res, 200, {
          connection_id: c.id,
          models: list.map(([id, name]) => ({ id, name })),
          ...(c.kind === 'gemini' ? { truncated: true, message: "The provider's catalog could only be read in part. Add any missing models manually." } : {}),
        });
      }
      if (seg[1] === 'connections' && seg[3] === 'test' && method === 'POST') {
        const c = connections.find((x) => x.id === decodeURIComponent(seg[2]));
        if (!c) return fail(res, 404, 'Connection not found');
        await sleep(200 + Math.random() * 500);
        const local = c.base_url.includes('127.0.0.1');
        if (local) return json(res, 200, { ok: false, status: 502, latency_ms: 3, message: 'Could not reach provider' });
        if (!c.secret) return json(res, 200, { ok: false, status: 401, latency_ms: 180, message: 'Provider returned an error; check credentials and base URL' });
        return json(res, 200, { ok: true, status: 200, latency_ms: Math.round(120 + Math.random() * 300), message: 'Provider reachable' });
      }
      if (path === '/api/import' && method === 'POST') {
        await sleep(400);
        const source = body?.source;
        const make = (name: string, kind: Kind, base: string, models: string[], ws: boolean, account: string) => {
          const existing = connections.find((c) => c.kind === kind && c.secret === account);
          if (existing) return existing;
          const src: Conn['source'] =
            source === 'cliproxy' ? 'cliproxy' : source === 'opencode' || source === 'opencode_go' ? 'native_opencode' : kind === 'codex' ? 'native_codex' : kind === 'antigravity' ? 'native_antigravity' : 'native_claude';
          const c: Conn = { id: randomUUID(), name, kind, base_url: base, enabled: true, models, supports_websocket: ws, created_at: now(), secret: account, source: src };
          connections.push(c);
          return c;
        };
        let out: Conn[];
        if (source === 'codex') out = [make('Codex', 'codex', 'https://chatgpt.com/backend-api/codex', ['gpt-6.1-sol', 'gpt-6-astra', 'gpt-6-luna'], true, 'codex-account-1')];
        else if (source === 'claude') out = [make('Claude Code', 'anthropic', 'https://api.anthropic.com/v1', ['claude-opus-5-5', 'claude-sonnet-5-5'], false, 'claude-account-1')];
        else if (source === 'cliproxy') {
          if (!body.path) return fail(res, 400, 'Choose a CLIProxyAPI auth JSON file or directory');
          if (String(body.path).includes('missing')) return fail(res, 400, 'Cannot read auth directory');
          out = [
            make('dev@example.com', 'codex', 'https://chatgpt.com/backend-api/codex', ['gpt-6.1-sol', 'gpt-6-astra', 'gpt-6-luna'], true, 'cliproxy-codex'),
            make('dev@example.com', 'anthropic', 'https://api.anthropic.com/v1', ['claude-opus-5-5', 'claude-sonnet-5-5'], false, 'cliproxy-claude'),
          ];
        } else if (source === 'antigravity') {
          out = [make('Antigravity', 'antigravity', 'https://daily-cloudcode-pa.googleapis.com', ['gemini-3-pro', 'claude-sonnet-5-5'], false, 'agy-account-1')];
        } else if (source === 'opencode' || source === 'opencode_go') {
          // Mirrors the providers contract: one connection per API family on the same Go key.
          out = [
            make('OpenCode Go', 'openai', 'https://opencode.ai/zen/go/v1', ['kimi-k2.5', 'glm-5'], false, 'opencode-go-key'),
            make('OpenCode Go (Messages)', 'anthropic', 'https://opencode.ai/zen/go/v1', ['minimax-m2.5'], false, 'opencode-go-key-msgs'),
          ];
        } else return fail(res, 400, 'Supported imports: codex, claude, cliproxy, antigravity, opencode');
        broadcast({ type: 'overview', data: overview() });
        return json(res, 200, { imported: out.length, connections: out.map(publicConn), message: 'Credentials imported locally. Original files were not changed.' });
      }
      if (path === '/api/models' && method === 'GET')
        return json(res, 200, connections.filter((c) => c.enabled).flatMap((c) => c.models.map((m) => ({ id: m, connection_id: c.id, connection_name: c.name, kind: c.kind, supports_websocket: c.supports_websocket }))));
      if (path === '/api/routes' && method === 'GET') return json(res, 200, routes);
      if (seg[1] === 'routes' && seg.length >= 3) {
        const model = decodeURIComponent(path.slice('/api/routes/'.length));
        if (method === 'PUT') {
          if (!body || !Array.isArray(body.targets) || !body.targets.length || body.targets.length > 20 || !['round_robin', 'failover'].includes(body.strategy))
            return fail(res, 400, 'Route requires a model, 1–20 targets and a valid strategy');
          for (const t of body.targets) {
            const c = connections.find((x) => x.id === t.connection_id);
            if (!c) return fail(res, 400, 'Route target connection not found');
            if (!c.models.includes(t.model)) return fail(res, 400, 'Route target model is not configured on its connection');
          }
          const r: Route = { model, targets: body.targets, strategy: body.strategy };
          const i = routes.findIndex((x) => x.model === model);
          if (i === -1) routes.push(r);
          else routes[i] = r;
          return json(res, 200, r);
        }
        if (method === 'DELETE') {
          routes = routes.filter((r) => r.model !== model);
          res.writeHead(204).end();
          return;
        }
      }
      if (seg[1] === 'requests' && seg.length === 3 && method === 'GET') {
        const r = requests.find((x) => x.id === decodeURIComponent(seg[2]));
        return r ? json(res, 200, r) : fail(res, 404, 'Request no longer retained in activity history');
      }
      if (path === '/api/requests' && method === 'GET') {
        const status = url.searchParams.get('status');
        const model = url.searchParams.get('model');
        const limit = Math.min(1000, Number(url.searchParams.get('limit') ?? 100));
        return json(
          res,
          200,
          requests.filter((r) => (!status || (status === 'error' ? r.status >= 400 : status === 'success' ? r.status < 400 : String(r.status) === status)) && (!model || r.model === model)).slice(0, limit),
        );
      }
      if (path === '/api/keys' && method === 'GET') return json(res, 200, keys.map(({ key: _k, ...k }) => k));
      if (path === '/api/keys' && method === 'POST') {
        if (typeof body?.name !== 'string' || !body.name.trim() || body.name.length > 100) return fail(res, 400, 'Key name must be 1–100 characters');
        const key = `sy_${randomUUID().replace(/-/g, '')}${randomUUID().replace(/-/g, '')}`;
        const k: Key = { id: randomUUID(), name: body.name, prefix: key.slice(0, 11), created_at: now(), key };
        keys.push(k);
        return json(res, 200, k);
      }
      if (seg[1] === 'keys' && seg.length === 3 && method === 'DELETE') {
        keys = keys.filter((k) => k.id !== decodeURIComponent(seg[2]));
        res.writeHead(204).end();
        return;
      }
      if (path === '/api/oauth/start' && method === 'POST') {
        const provider = body?.provider;
        if (provider !== 'codex' && provider !== 'claude' && provider !== 'antigravity') return fail(res, 400, 'Choose codex, claude or antigravity');
        if (oauthBusy)
          return fail(res, 409, `Sign-in callback port ${OAUTH_PORTS[provider as 'codex' | 'claude' | 'antigravity']} is already in use. Close any running codex login, claude login or CLIProxyAPI login, then retry.`);
        for (const f of flows.values()) {
          if (f.provider === provider && f.status === 'pending') {
            f.status = 'error';
            f.message = 'Replaced by a newer sign-in.';
          }
        }
        const f: Flow = { id: randomUUID().replace(/-/g, ''), provider, state: randomUUID().replace(/-/g, ''), status: 'pending', expires: Date.now() + oauthTtl * 1000 };
        flows.set(f.id, f);
        const host = req.headers.host ?? '127.0.0.1:5181';
        return json(res, 200, {
          id: f.id,
          provider,
          authorization_url: `http://${host}/__mock/oauth/authorize?id=${f.id}&state=${f.state}&provider=${provider}`,
          expires_in_seconds: oauthTtl,
          status: 'pending',
        });
      }
      if (seg[1] === 'oauth' && seg.length >= 3) {
        const f = flows.get(decodeURIComponent(seg[2]));
        if (!f) return fail(res, 404, 'Sign-in not found. It may have expired; start again.');
        if (seg.length === 3 && method === 'GET') return json(res, 200, flowView(f));
        if (seg.length === 3 && method === 'DELETE') {
          if (flowView(f).status === 'pending') {
            f.status = 'error';
            f.message = 'Sign-in cancelled.';
          }
          return json(res, 200, flowView(f));
        }
        if (seg[3] === 'callback' && method === 'POST') {
          const input = String(body?.input ?? '').trim();
          if (flowView(f).status !== 'pending') return json(res, 200, flowView(f));
          let code: string | null = null;
          let state: string | null = null;
          try {
            const u = new URL(input);
            if (u.searchParams.get('error')) {
              f.status = 'error';
              f.message = `The provider reported: ${u.searchParams.get('error')}`;
              return json(res, 200, flowView(f));
            }
            code = u.searchParams.get('code');
            state = u.searchParams.get('state');
          } catch {
            const parts = input.split('#');
            if (parts.length === 2) [code, state] = parts;
          }
          if (!code || !state) return fail(res, 400, 'Paste the full callback URL from the browser address bar');
          if (state !== f.state) return fail(res, 400, 'That callback belongs to a different sign-in. Copy the address from this sign-in’s page.');
          completeFlow(f, code === 'mockcode' ? 'you@example.com' : code);
          return json(res, 200, flowView(f));
        }
      }
      if (path === '/api/playground' && method === 'POST') {
        if (!body?.model) return fail(res, 400, 'Choose a model');
        if (typeof body.input !== 'string') return fail(res, 400, 'Enter a prompt');
        if (paused) return fail(res, 503, 'Gateway is paused');
        const cs = candidates(body.model);
        if (!cs.length) return fail(res, 404, 'No enabled connection for this model. Add a provider or route in the dashboard.');
        const { conn } = cs[0];
        const t0 = Date.now();
        const transport = body.transport === 'sse' ? 'sse' : 'http';
        if (/fail/i.test(body.input)) {
          record({ model: body.model, connection_id: conn.id, connection_name: conn.name, transport, status: 502, latency_ms: 420, input_tokens: null, output_tokens: null, error: 'Provider rejected the request' });
          return fail(res, 502, 'Provider rejected the request. Check credentials, account quota and model availability.');
        }
        const signal = { closed: false };
        res.on('close', () => (signal.closed = true));
        active++;
        await streamPlayground(res, conn.kind, body.model, transport === 'sse', signal);
        active--;
        record({ model: body.model, connection_id: conn.id, connection_name: conn.name, transport, status: signal.closed && !res.writableFinished ? 499 : 200, latency_ms: Date.now() - t0, input_tokens: 23, output_tokens: WORDS.length, error: null });
        return;
      }
      return fail(res, 404, 'Endpoint not found');
    }

    if (path.startsWith('/v1')) {
      const token = bearer(req);
      if (!token || !keys.some((k) => k.key === token)) return fail(res, 401, 'A valid Switchyard client API key is required');
      if (path === '/v1/models') return json(res, 200, { object: 'list', data: [...new Set([...connections.flatMap((c) => c.models), ...routes.map((r) => r.model)])].map((id) => ({ id, object: 'model', owned_by: 'switchyard' })) });
      return fail(res, 501, 'The mock backend only implements /v1/models');
    }

    res.writeHead(404, { 'content-type': 'text/plain' }).end('Mock backend: UI is served by Vite.');
  });

  /* ---------- WebSockets ---------- */
  const wss = new WebSocketServer({ noServer: true });
  server.on('upgrade', (req, socket, head) => {
    const url = new URL(req.url ?? '/', 'http://mock');
    const refuse = (code: number) => {
      socket.write(`HTTP/1.1 ${code} Refused\r\nConnection: close\r\n\r\n`);
      socket.destroy();
    };
    if (url.pathname === '/api/events') {
      if (!eventsEnabled) return refuse(503);
      if (!isAdmin(req)) return refuse(401);
      wss.handleUpgrade(req, socket, head, (ws) => {
        eventSockets.add(ws);
        ws.send(JSON.stringify({ type: 'overview', data: overview() }));
        ws.on('close', () => eventSockets.delete(ws));
      });
      return;
    }
    if (url.pathname === '/api/playground/ws') {
      if (!isAdmin(req)) return refuse(401);
      const model = url.searchParams.get('model') ?? '';
      if (paused) return refuse(503);
      const cs = candidates(model, true);
      if (!cs.length) return refuse(404);
      wss.handleUpgrade(req, socket, head, (ws) => {
        const conn = cs[0].conn;
        let turn = 0;
        ws.on('message', async (raw) => {
          let ev: any;
          try {
            ev = JSON.parse(String(raw));
          } catch {
            ws.send(JSON.stringify({ type: 'error', error: { message: 'Invalid JSON frame' } }));
            return ws.close();
          }
          if (ev.type !== 'response.create') return;
          const requested = ev.response?.model ?? ev.model ?? model;
          if (requested !== model) return ws.send(JSON.stringify({ type: 'error', error: { message: 'Changing models requires a new WebSocket session' } }));
          turn++;
          const t0 = Date.now();
          const id = `resp_ws_${turn}`;
          ws.send(JSON.stringify({ type: 'response.created', response: { id, model, status: 'in_progress' } }));
          for (const w of WORDS) {
            if (ws.readyState !== ws.OPEN) return;
            await sleep(30);
            ws.send(JSON.stringify({ type: 'response.output_text.delta', item_id: 'msg_1', output_index: 0, delta: w + ' ' }));
          }
          ws.send(JSON.stringify({ type: 'response.completed', response: { id, model, status: 'completed', usage: { input_tokens: 23, output_tokens: WORDS.length } } }));
          record({ model, connection_id: conn.id, connection_name: conn.name, transport: 'websocket', status: 200, latency_ms: Date.now() - t0, input_tokens: 23, output_tokens: WORDS.length, error: null });
        });
      });
      return;
    }
    refuse(404);
  });

  return {
    server,
    close: () => {
      if (trafficTimer) clearInterval(trafficTimer);
      for (const ws of eventSockets) ws.terminate();
      wss.close();
      server.close();
      server.closeAllConnections();
    },
  };
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? '').href) {
  const args = process.argv.slice(2);
  const portArg = args.includes('--port') ? args[args.indexOf('--port') + 1] : undefined;
  const port = Number(portArg ?? process.env.SWITCHYARD_MOCK_PORT ?? 5181);
  const auth = process.env.MOCK_AUTH === 'token' ? 'token' : 'cookie';
  const mockServer = createMockServer({ seed: args.includes('--seed'), auth });
  const { server } = mockServer;
  // Exit promptly on SIGTERM/SIGINT even with open sockets or keep-alive connections.
  const shutdown = () => {
    mockServer.close();
    setTimeout(() => process.exit(0), 500).unref();
  };
  process.once('SIGTERM', shutdown);
  process.once('SIGINT', shutdown);
  server.listen(port, '127.0.0.1', () => {
    console.log(`Switchyard mock backend on http://127.0.0.1:${port} (auth: ${auth}${auth === 'token' ? `, token ${ADMIN_TOKEN}` : ''})`);
  });
}
