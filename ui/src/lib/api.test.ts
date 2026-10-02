import { describe, expect, it, vi } from 'vitest';
import { ApiError, createApiClient, errorMessage, parseErrorBody } from './api';

const json = (status: number, body: unknown, headers: Record<string, string> = {}) =>
  new Response(body === undefined ? null : JSON.stringify(body), { status, headers: { 'content-type': 'application/json', ...headers } });

describe('parseErrorBody', () => {
  it('reads the contract error shape', () => {
    expect(parseErrorBody('{"error":{"message":"Nope","type":"gateway_error"}}', 400)).toEqual({ message: 'Nope', type: 'gateway_error' });
  });
  it('accepts string errors and top-level messages', () => {
    expect(parseErrorBody('{"error":"bad"}', 400).message).toBe('bad');
    expect(parseErrorBody('{"message":"meh"}', 400).message).toBe('meh');
  });
  it('falls back to a status message for HTML or empty bodies', () => {
    expect(parseErrorBody('<html>502</html>', 502).message).toMatch(/upstream/);
    expect(parseErrorBody('', 503).message).toMatch(/unavailable/);
    expect(parseErrorBody('', 418).message).toBe('Request failed with status 418.');
  });
  it('uses short plain-text bodies', () => {
    expect(parseErrorBody('upstream exploded', 500).message).toBe('upstream exploded');
  });
});

describe('createApiClient', () => {
  it('sends credentials, JSON and the bearer token', async () => {
    const fetch = vi.fn(async (_url: RequestInfo | URL, _init?: RequestInit) => json(200, { ok: true }));
    const api = createApiClient({ fetch, getToken: () => 'tok' });
    await api.createKey('ci');
    const [url, init] = fetch.mock.calls[0];
    expect(url).toBe('/api/keys');
    expect(init?.method).toBe('POST');
    expect(init?.credentials).toBe('include');
    expect(init?.body).toBe('{"name":"ci"}');
    const h = new Headers(init?.headers);
    expect(h.get('authorization')).toBe('Bearer tok');
    expect(h.get('content-type')).toBe('application/json');
  });

  it('URL-encodes route models containing slashes and colons', async () => {
    const fetch = vi.fn(async (_u: RequestInfo | URL, _i?: RequestInit) => new Response(null, { status: 204 }));
    const api = createApiClient({ fetch });
    await api.deleteRoute('openrouter/qwen:free');
    expect(fetch.mock.calls[0][0]).toBe('/api/routes/openrouter%2Fqwen%3Afree');
  });

  it('handles 204 and empty bodies', async () => {
    const api = createApiClient({ fetch: async () => new Response(null, { status: 204 }) });
    await expect(api.revokeKey('x')).resolves.toBeUndefined();
  });

  it('normalises HTTP errors and notifies on 401', async () => {
    const onUnauthorized = vi.fn();
    const api = createApiClient({ fetch: async () => json(401, { error: { message: 'Admin session required', type: 'gateway_error' } }), onUnauthorized });
    const err = await api.overview().catch((e) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect(err.status).toBe(401);
    expect(err.isUnauthorized).toBe(true);
    expect(err.message).toBe('Admin session required');
    expect(onUnauthorized).toHaveBeenCalledOnce();
  });

  it('can suppress the 401 handler', async () => {
    const onUnauthorized = vi.fn();
    const api = createApiClient({ fetch: async () => json(401, {}), onUnauthorized });
    await api.overview({ silent401: true }).catch(() => {});
    expect(onUnauthorized).not.toHaveBeenCalled();
  });

  it('flags unsupported routes for feature detection', async () => {
    for (const status of [404, 405, 501]) {
      const api = createApiClient({ fetch: async () => json(status, {}) });
      const err = (await api.session().catch((e) => e)) as ApiError;
      expect(err.isUnsupported).toBe(true);
    }
  });

  it('maps network failures and aborts', async () => {
    const api = createApiClient({ fetch: async () => Promise.reject(new TypeError('Failed to fetch')) });
    const err = (await api.overview().catch((e) => e)) as ApiError;
    expect(err.kind).toBe('network');
    expect(err.status).toBe(0);

    const ctrl = new AbortController();
    ctrl.abort();
    const api2 = createApiClient({ fetch: async () => Promise.reject(new DOMException('aborted', 'AbortError')) });
    const err2 = (await api2.raw('/api/overview', { signal: ctrl.signal }).catch((e) => e)) as ApiError;
    expect(err2.kind).toBe('aborted');
  });

  it('rejects unreadable JSON success bodies', async () => {
    const api = createApiClient({ fetch: async () => new Response('not json', { status: 200 }) });
    const err = (await api.overview().catch((e) => e)) as ApiError;
    expect(err.kind).toBe('parse');
  });

  it('returns error responses untouched when asked (playground)', async () => {
    const api = createApiClient({ fetch: async () => json(502, { error: { message: 'x' } }) });
    const res = await api.raw('/api/playground', { method: 'POST', body: {}, allowHttpError: true });
    expect(res.status).toBe(502);
  });

  it('only sends the filters it was given', async () => {
    const fetch = vi.fn(async (_u: RequestInfo | URL, _i?: RequestInit) => json(200, []));
    const api = createApiClient({ fetch });
    await api.requests({ status: 'error', model: 'a/b' });
    expect(fetch.mock.calls[0][0]).toBe('/api/requests?limit=200&status=error&model=a%2Fb');
  });

  it('omits api_key on update when not provided', async () => {
    const fetch = vi.fn(async (_u: RequestInfo | URL, _i?: RequestInit) => json(200, {}));
    const api = createApiClient({ fetch });
    await api.updateConnection('c1', { name: 'n', kind: 'openai', base_url: 'https://x', enabled: true, models: ['m'], supports_websocket: false });
    expect(JSON.parse(String(fetch.mock.calls[0][1]?.body))).not.toHaveProperty('api_key');
  });
});

describe('errorMessage', () => {
  it('never throws and gives something human', () => {
    expect(errorMessage(new ApiError('x', { status: 400, kind: 'http' }))).toBe('x');
    expect(errorMessage(new Error('y'))).toBe('y');
    expect(errorMessage(42)).toBe('Something went wrong.');
  });
});
