import { describe, expect, it, vi } from 'vitest';
import { createApiClient } from './api';
import { bootstrapSession, cleanToken, createTokenStore, signInWithToken } from './auth';

function memoryStorage() {
  const m = new Map<string, string>();
  return { getItem: (k: string) => m.get(k) ?? null, setItem: (k: string, v: string) => void m.set(k, v), removeItem: (k: string) => void m.delete(k), m };
}

type Handler = (url: string, init: RequestInit) => Response | Promise<Response>;
const client = (h: Handler) => createApiClient({ fetch: async (u, i) => h(String(u), i ?? {}) });
const auth = (i: RequestInit) => new Headers(i.headers).get('authorization');
const ok = () => new Response('{"authenticated":true}', { status: 200 });
const status = (s: number) => new Response('{"error":{"message":"no"}}', { status: s });

describe('bootstrapSession', () => {
  it('uses the loopback cookie when GET /api/session succeeds', async () => {
    const store = createTokenStore(memoryStorage());
    const api = client((url, i) => (url === '/api/session' && (i.method ?? 'GET') === 'GET' ? ok() : status(500)));
    expect(await bootstrapSession(api, store)).toEqual({ status: 'ready', mode: 'cookie' });
  });

  it('asks for a token when remote and none is stored', async () => {
    const api = client(() => status(401));
    expect(await bootstrapSession(api, createTokenStore(memoryStorage()))).toEqual({ status: 'needs-token', reason: 'remote' });
  });

  it('exchanges a stored token for a cookie via POST /api/session', async () => {
    const s = memoryStorage();
    const store = createTokenStore(s);
    store.set('sy_admin_x');
    const handler = vi.fn<Handler>((_url, i) => (i.method === 'POST' && auth(i) === 'Bearer sy_admin_x' ? ok() : status(401)));
    expect(await bootstrapSession(client(handler), store)).toEqual({ status: 'ready', mode: 'token' });
    expect(handler.mock.calls.some(([, i]) => i.method === 'POST')).toBe(true);
  });

  it('clears a stored token the gateway rejects', async () => {
    const s = memoryStorage();
    const store = createTokenStore(s);
    store.set('old');
    expect(await bootstrapSession(client(() => status(401)), store)).toEqual({ status: 'needs-token', reason: 'expired' });
    expect(store.get()).toBeNull();
  });

  it('falls back to probing /api/overview on backends without POST /api/session', async () => {
    const store = createTokenStore(memoryStorage());
    store.set('tok');
    const api = client((url, i) => {
      if (url === '/api/session') return i.method === 'POST' ? status(405) : status(401);
      return auth(i) === 'Bearer tok' ? new Response('{}', { status: 200 }) : status(401);
    });
    expect(await bootstrapSession(api, store)).toEqual({ status: 'ready', mode: 'token' });
  });

  it('reports an unreachable gateway and cross-origin blocks distinctly', async () => {
    const down = createApiClient({ fetch: async () => Promise.reject(new TypeError('fail')) });
    expect((await bootstrapSession(down, createTokenStore(null))).status).toBe('error');
    expect(await bootstrapSession(down, createTokenStore(null))).toMatchObject({ kind: 'unreachable' });
    expect(await bootstrapSession(client(() => status(403)), createTokenStore(null))).toMatchObject({ kind: 'forbidden' });
  });
});

describe('signInWithToken', () => {
  it('cleans pasted input and stores only an accepted token', async () => {
    const s = memoryStorage();
    const store = createTokenStore(s);
    const api = client((_u, i) => (auth(i) === 'Bearer sy_admin_ok' ? ok() : status(401)));
    expect(await signInWithToken(api, store, '  "sy_admin_bad"  ')).toEqual({ status: 'needs-token', reason: 'rejected' });
    expect(store.get()).toBeNull();
    expect(await signInWithToken(api, store, 'Bearer sy_admin_ok\n')).toEqual({ status: 'ready', mode: 'token' });
    expect(s.m.get('switchyard.admin-token')).toBe('sy_admin_ok');
  });

  it('cleanToken strips Bearer prefixes and quotes', () => {
    expect(cleanToken(" bearer 'abc' ")).toBe('abc');
    expect(cleanToken('"abc"')).toBe('abc');
  });

  it('keeps working when storage throws', () => {
    const broken = { getItem: () => { throw new Error('denied'); }, setItem: () => { throw new Error('denied'); }, removeItem: () => { throw new Error('denied'); } };
    const store = createTokenStore(broken);
    store.set('t');
    expect(store.get()).toBe('t');
    store.clear();
    expect(store.get()).toBeNull();
  });
});
