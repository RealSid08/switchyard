import type {
  ApiKey,
  Connection,
  ConnectionInput,
  ConnectionTestResult,
  CreatedApiKey,
  GatewayConfig,
  ImportResult,
  ImportSource,
  ModelInfo,
  Overview,
  RequestRecord,
  Route,
  RouteStrategy,
  RouteTarget,
} from './types';

export type ApiErrorKind = 'network' | 'http' | 'aborted' | 'parse';

/** A normalised failure from the admin API. `status` is 0 for network failures. */
export class ApiError extends Error {
  readonly status: number;
  readonly type: string;
  readonly kind: ApiErrorKind;

  constructor(message: string, opts: { status: number; type?: string; kind: ApiErrorKind }) {
    super(message);
    this.name = 'ApiError';
    this.status = opts.status;
    this.type = opts.type ?? 'error';
    this.kind = opts.kind;
  }

  get isUnauthorized() {
    return this.status === 401;
  }

  /** The backend doesn't implement this route (yet). Used for feature detection. */
  get isUnsupported() {
    return this.status === 404 || this.status === 405 || this.status === 501;
  }
}

export function isApiError(e: unknown): e is ApiError {
  return e instanceof ApiError;
}

/** A human message for any thrown value, never including request bodies. */
export function errorMessage(e: unknown): string {
  if (e instanceof ApiError) return e.message;
  if (e instanceof Error && e.message) return e.message;
  return 'Something went wrong.';
}

const STATUS_MESSAGES: Record<number, string> = {
  400: 'The gateway rejected that request.',
  401: 'Your admin session has expired. Sign in again.',
  403: 'This admin session is not allowed to do that.',
  404: 'Not found.',
  409: 'That conflicts with something that already exists.',
  413: 'That request is too large.',
  429: 'Too many requests. Try again in a moment.',
  500: 'The gateway hit an internal error.',
  502: 'The upstream provider returned a bad response.',
  503: 'The gateway is unavailable right now.',
  504: 'The upstream provider timed out.',
};

/** Pull `{error:{message,type}}` (or a few common variants) out of an error body. */
export function parseErrorBody(text: string, status: number): { message: string; type: string } {
  const fallback = STATUS_MESSAGES[status] ?? `Request failed with status ${status}.`;
  if (!text) return { message: fallback, type: 'error' };
  try {
    const body = JSON.parse(text) as unknown;
    if (body && typeof body === 'object') {
      const err = (body as { error?: unknown }).error;
      if (err && typeof err === 'object') {
        const { message, type } = err as { message?: unknown; type?: unknown };
        return {
          message: typeof message === 'string' && message ? message : fallback,
          type: typeof type === 'string' ? type : 'error',
        };
      }
      if (typeof err === 'string' && err) return { message: err, type: 'error' };
      const msg = (body as { message?: unknown }).message;
      if (typeof msg === 'string' && msg) return { message: msg, type: 'error' };
    }
  } catch {
    // Not JSON: use a short plain-text body if it looks like a message, never HTML.
    const trimmed = text.trim();
    if (trimmed && trimmed.length <= 200 && !trimmed.startsWith('<')) return { message: trimmed, type: 'error' };
  }
  return { message: fallback, type: 'error' };
}

export interface RequestOptions {
  method?: 'GET' | 'POST' | 'PUT' | 'DELETE';
  body?: unknown;
  signal?: AbortSignal;
  /** Override the bearer token for this call (used to verify a pasted token). */
  token?: string | null;
  /** Don't fire the global unauthorized handler (used during auth bootstrap). */
  silent401?: boolean;
  /** Return non-2xx responses instead of throwing (the playground shows raw error bodies). */
  allowHttpError?: boolean;
}

export interface ApiClientOptions {
  fetch?: typeof fetch;
  getToken?: () => string | null;
  onUnauthorized?: () => void;
}

export function createApiClient(opts: ApiClientOptions = {}) {
  const doFetch = opts.fetch ?? ((...args: Parameters<typeof fetch>) => fetch(...args));

  function headers(o: RequestOptions, json: boolean): Headers {
    const h = new Headers({ Accept: 'application/json' });
    if (json) h.set('Content-Type', 'application/json');
    const token = o.token !== undefined ? o.token : (opts.getToken?.() ?? null);
    if (token) h.set('Authorization', `Bearer ${token}`);
    return h;
  }

  /** Low-level: returns the raw Response after auth and error handling (for streams). */
  async function raw(path: string, o: RequestOptions = {}): Promise<Response> {
    const hasBody = o.body !== undefined;
    let res: Response;
    try {
      res = await doFetch(path, {
        method: o.method ?? (hasBody ? 'POST' : 'GET'),
        headers: headers(o, hasBody),
        cache: 'no-store',
        body: hasBody ? JSON.stringify(o.body) : undefined,
        credentials: 'include',
        signal: o.signal,
      });
    } catch (e) {
      if (o.signal?.aborted || (e instanceof DOMException && e.name === 'AbortError')) {
        throw new ApiError('Request cancelled.', { status: 0, type: 'aborted', kind: 'aborted' });
      }
      throw new ApiError("Can't reach the Switchyard gateway. Is it running?", {
        status: 0,
        type: 'network_error',
        kind: 'network',
      });
    }
    if (!res.ok && o.allowHttpError) {
      if (res.status === 401 && !o.silent401) opts.onUnauthorized?.();
      return res;
    }
    if (!res.ok) {
      const text = await res.text().catch(() => '');
      const { message, type } = parseErrorBody(text, res.status);
      if (res.status === 401 && !o.silent401) opts.onUnauthorized?.();
      throw new ApiError(message, { status: res.status, type, kind: 'http' });
    }
    return res;
  }

  async function request<T>(path: string, o: RequestOptions = {}): Promise<T> {
    const res = await raw(path, o);
    if (res.status === 204) return undefined as T;
    const text = await res.text();
    if (!text) return undefined as T;
    try {
      return JSON.parse(text) as T;
    } catch {
      throw new ApiError('The gateway returned an unreadable response.', {
        status: res.status,
        type: 'parse_error',
        kind: 'parse',
      });
    }
  }

  const enc = encodeURIComponent;

  return {
    raw,
    request,
    session: (o?: RequestOptions) => request<unknown>('/api/session', o),
    /** Exchange an admin token for the HttpOnly session cookie (also enables /api/events). */
    mintSession: (token: string) => request<unknown>('/api/session', { method: 'POST', body: {}, token, silent401: true }),
    overview: (o?: RequestOptions) => request<Overview>('/api/overview', o),
    setPaused: (paused: boolean) => request<unknown>('/api/settings', { method: 'POST', body: { paused } }),
    config: () => request<GatewayConfig>('/api/config'),

    connections: () => request<Connection[]>('/api/connections'),
    createConnection: (body: ConnectionInput) => request<Connection>('/api/connections', { method: 'POST', body }),
    updateConnection: (id: string, body: ConnectionInput) =>
      request<Connection>(`/api/connections/${enc(id)}`, { method: 'PUT', body }),
    deleteConnection: (id: string) => request<void>(`/api/connections/${enc(id)}`, { method: 'DELETE' }),
    testConnection: (id: string) =>
      request<ConnectionTestResult>(`/api/connections/${enc(id)}/test`, { method: 'POST', body: {} }),
    importCredentials: (source: ImportSource, path?: string) =>
      request<ImportResult>('/api/import', { method: 'POST', body: path ? { source, path } : { source } }),

    models: () => request<ModelInfo[]>('/api/models'),
    routes: () => request<Route[]>('/api/routes'),
    saveRoute: (model: string, body: { targets: RouteTarget[]; strategy: RouteStrategy }) =>
      request<Route>(`/api/routes/${enc(model)}`, { method: 'PUT', body }),
    deleteRoute: (model: string) => request<void>(`/api/routes/${enc(model)}`, { method: 'DELETE' }),

    requests: (q: { limit?: number; status?: 'error' | 'success'; model?: string } = {}) => {
      const p = new URLSearchParams();
      p.set('limit', String(q.limit ?? 200));
      if (q.status) p.set('status', q.status);
      if (q.model) p.set('model', q.model);
      return request<RequestRecord[]>(`/api/requests?${p}`);
    },

    keys: () => request<ApiKey[]>('/api/keys'),
    createKey: (name: string) => request<CreatedApiKey>('/api/keys', { method: 'POST', body: { name } }),
    revokeKey: (id: string) => request<void>(`/api/keys/${enc(id)}`, { method: 'DELETE' }),
  };
}

export type ApiClient = ReturnType<typeof createApiClient>;
