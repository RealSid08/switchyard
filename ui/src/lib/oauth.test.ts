import { describe, expect, it, vi } from 'vitest';
import { ApiError } from './api';
import { OAuthFlowController, checkCallbackInput, credentialSourceInfo, formatCountdown, oauthProviderFor, type OAuthApi, type Timers } from './oauth';
import type { Connection, OAuthFlow } from './types';

function fakeTimers() {
  let now = 0;
  let seq = 0;
  const queue: { id: number; at: number; fn: () => void }[] = [];
  const timers: Timers = {
    setTimeout: (fn, ms) => {
      const id = ++seq;
      queue.push({ id, at: now + ms, fn });
      return id;
    },
    clearTimeout: (id) => {
      const i = queue.findIndex((t) => t.id === id);
      if (i !== -1) queue.splice(i, 1);
    },
    now: () => now,
  };
  /** Advance time, running due timers and letting their promises settle. */
  const advance = async (ms: number) => {
    const end = now + ms;
    for (;;) {
      queue.sort((a, b) => a.at - b.at);
      const next = queue[0];
      if (!next || next.at > end) break;
      queue.shift();
      now = next.at;
      next.fn();
      await flush();
    }
    now = end;
  };
  return { timers, advance };
}

const flush = () => new Promise((r) => setTimeout(r, 0));

const conn: Connection = { id: 'c1', name: 'ChatGPT · you@example.com', kind: 'codex', base_url: 'x', enabled: true, models: ['gpt-6.1-sol'], supports_websocket: true, credential_present: true, created_at: 0, credential_source: 'oauth' };

function fakeApi(over: Partial<OAuthApi> = {}) {
  const statuses: OAuthFlow[] = [];
  const api = {
    oauthStart: vi.fn<OAuthApi['oauthStart']>(async (provider) => ({ id: 'f1', provider, status: 'pending', authorization_url: 'https://auth.example/authorize?state=s1', expires_in_seconds: 300 })),
    oauthStatus: vi.fn<OAuthApi['oauthStatus']>(async (id) => statuses.shift() ?? { id, provider: 'codex', status: 'pending', expires_in_seconds: 290 }),
    oauthCancel: vi.fn<OAuthApi['oauthCancel']>(async (id) => ({ id, provider: 'codex', status: 'error', message: 'Sign-in cancelled.' })),
    oauthCallback: vi.fn<OAuthApi['oauthCallback']>(async (id) => ({ id, provider: 'codex', status: 'complete', connection: conn })),
    ...over,
  };
  return { api, statuses };
}

describe('OAuthFlowController', () => {
  it('starts, polls while pending, and completes', async () => {
    const { timers, advance } = fakeTimers();
    const { api, statuses } = fakeApi();
    const onComplete = vi.fn();
    const c = new OAuthFlowController(api, 'codex', { timers, pollMs: 1500, onComplete });
    await c.start();
    expect(c.snapshot).toMatchObject({ phase: 'pending', id: 'f1', authorizationUrl: 'https://auth.example/authorize?state=s1', remaining: 300 });
    await advance(1500);
    expect(api.oauthStatus).toHaveBeenCalledTimes(1);
    expect(c.snapshot.phase).toBe('pending');
    statuses.push({ id: 'f1', provider: 'codex', status: 'complete', connection: conn });
    await advance(1500);
    expect(c.snapshot).toMatchObject({ phase: 'complete', connection: conn, remaining: null });
    expect(onComplete).toHaveBeenCalledWith(conn);
    // Polling stops after completion.
    await advance(10_000);
    expect(api.oauthStatus).toHaveBeenCalledTimes(2);
    // Completed flows aren't cancelled on close.
    c.dispose();
    expect(api.oauthCancel).not.toHaveBeenCalled();
  });

  it('counts down locally and resyncs from the server', async () => {
    const { timers, advance } = fakeTimers();
    const { api, statuses } = fakeApi();
    const c = new OAuthFlowController(api, 'codex', { timers, pollMs: 5000 });
    await c.start();
    await advance(3000);
    expect(c.snapshot.remaining).toBe(297);
    statuses.push({ id: 'f1', provider: 'codex', status: 'pending', expires_in_seconds: 100 });
    await advance(2000);
    expect(c.snapshot.remaining).toBe(100);
    expect(formatCountdown(100)).toBe('1:40');
  });

  it('reports expiry from the server, or a vanished flow (404) as expired', async () => {
    const { timers, advance } = fakeTimers();
    const { api, statuses } = fakeApi();
    const c = new OAuthFlowController(api, 'codex', { timers });
    await c.start();
    statuses.push({ id: 'f1', provider: 'codex', status: 'expired', message: 'Sign-in expired. Start again from the control room.' });
    await advance(1500);
    expect(c.snapshot).toMatchObject({ phase: 'expired', message: 'Sign-in expired. Start again from the control room.' });

    const { api: api2 } = fakeApi({ oauthStatus: async () => Promise.reject(new ApiError('Sign-in not found. It may have expired; start again.', { status: 404, kind: 'http' })) });
    const c2 = new OAuthFlowController(api2, 'claude', { timers });
    await c2.start();
    await advance(1500);
    expect(c2.snapshot.phase).toBe('expired');
  });

  it('keeps waiting through network blips (e.g. a gateway restart)', async () => {
    const { timers, advance } = fakeTimers();
    let fail = true;
    const { api } = fakeApi({
      oauthStatus: async (id) => {
        if (fail) throw new ApiError("Can't reach the Switchyard gateway.", { status: 0, kind: 'network' });
        return { id, provider: 'codex', status: 'complete', connection: conn };
      },
    });
    const c = new OAuthFlowController(api, 'codex', { timers });
    await c.start();
    await advance(1500);
    expect(c.snapshot).toMatchObject({ phase: 'pending', reconnecting: true });
    fail = false;
    await advance(1500);
    expect(c.snapshot).toMatchObject({ phase: 'complete', reconnecting: false });
  });

  it('cancels a pending flow on close and stops polling', async () => {
    const { timers, advance } = fakeTimers();
    const { api } = fakeApi();
    const c = new OAuthFlowController(api, 'codex', { timers });
    await c.start();
    c.cancel();
    expect(api.oauthCancel).toHaveBeenCalledWith('f1');
    expect(c.snapshot.phase).toBe('cancelled');
    await advance(10_000);
    expect(api.oauthStatus).not.toHaveBeenCalled();
    c.cancel();
    expect(api.oauthCancel).toHaveBeenCalledTimes(1);
  });

  it('cancels a flow the gateway started after the dialog already closed', async () => {
    const { timers } = fakeTimers();
    let resolve!: (f: OAuthFlow) => void;
    const { api } = fakeApi({ oauthStart: () => new Promise((r) => (resolve = r)) });
    const c = new OAuthFlowController(api, 'claude', { timers });
    const started = c.start();
    c.dispose();
    resolve({ id: 'late', provider: 'claude', status: 'pending', expires_in_seconds: 300 });
    await started;
    await flush();
    expect(api.oauthCancel).toHaveBeenCalledWith('late');
    expect(c.snapshot.phase).not.toBe('pending');
  });

  it('can start again after an unmount (React StrictMode remount)', async () => {
    const { timers } = fakeTimers();
    const { api } = fakeApi();
    const c = new OAuthFlowController(api, 'codex', { timers });
    void c.start();
    c.dispose();
    await c.start();
    expect(c.snapshot.phase).toBe('pending');
  });

  it('completes from a pasted remote callback, validating input first', async () => {
    const { timers } = fakeTimers();
    const { api } = fakeApi();
    const onComplete = vi.fn();
    const c = new OAuthFlowController(api, 'codex', { timers, onComplete });
    await c.start();
    await c.submitCallback('localhost:1455');
    expect(c.snapshot.callbackError).toMatch(/full callback address/);
    expect(api.oauthCallback).not.toHaveBeenCalled();
    await c.submitCallback('http://localhost:1455/auth/callback?code=abc&state=s1');
    expect(api.oauthCallback).toHaveBeenCalledWith('f1', 'http://localhost:1455/auth/callback?code=abc&state=s1');
    expect(c.snapshot.phase).toBe('complete');
    expect(onComplete).toHaveBeenCalled();
  });

  it('shows server rejections of a pasted callback inline and stays pending', async () => {
    const { timers } = fakeTimers();
    const { api } = fakeApi({ oauthCallback: async () => Promise.reject(new ApiError('That callback belongs to a different sign-in.', { status: 400, kind: 'http' })) });
    const c = new OAuthFlowController(api, 'claude', { timers });
    await c.start();
    await c.submitCallback('code123#wrongstate');
    expect(c.snapshot).toMatchObject({ phase: 'pending', callbackError: 'That callback belongs to a different sign-in.' });
  });

  it('explains a busy callback port (409) and can retry', async () => {
    const { timers } = fakeTimers();
    let busy = true;
    const { api } = fakeApi({
      oauthStart: async (provider) => {
        if (busy) throw new ApiError('Sign-in callback port 1455 is already in use.', { status: 409, kind: 'http' });
        return { id: 'f2', provider, status: 'pending', expires_in_seconds: 300 };
      },
    });
    const c = new OAuthFlowController(api, 'codex', { timers });
    await c.start();
    expect(c.snapshot).toMatchObject({ phase: 'error', errorKind: 'busy' });
    busy = false;
    await c.start();
    expect(c.snapshot).toMatchObject({ phase: 'pending', id: 'f2', errorKind: null });
  });

  it('surfaces provider errors reported via status', async () => {
    const { timers, advance } = fakeTimers();
    const { api, statuses } = fakeApi();
    const c = new OAuthFlowController(api, 'codex', { timers });
    await c.start();
    statuses.push({ id: 'f1', provider: 'codex', status: 'error', message: 'Replaced by a newer sign-in.' });
    await advance(1500);
    expect(c.snapshot).toMatchObject({ phase: 'error', message: 'Replaced by a newer sign-in.' });
  });
});

describe('checkCallbackInput', () => {
  it('accepts full callback URLs, provider errors and Claude code#state', () => {
    expect(checkCallbackInput('http://localhost:1455/auth/callback?code=a&state=b').ok).toBe(true);
    expect(checkCallbackInput('http://localhost:1455/auth/callback?error=access_denied').ok).toBe(true);
    expect(checkCallbackInput('abc123#state456').ok).toBe(true);
  });
  it('rejects partial input with a specific hint', () => {
    expect(checkCallbackInput('')).toMatchObject({ ok: false });
    expect(checkCallbackInput('http://localhost:1455/auth/callback?state=b')).toMatchObject({ ok: false, message: expect.stringMatching(/no sign-in code/) });
    expect(checkCallbackInput('http://localhost:1455/auth/callback?code=a')).toMatchObject({ ok: false, message: expect.stringMatching(/state/) });
    expect(checkCallbackInput('just-a-code')).toMatchObject({ ok: false });
  });
});

describe('credential sources', () => {
  it('describes ownership without paths', () => {
    expect(credentialSourceInfo('oauth')?.owner).toBe('gateway');
    expect(credentialSourceInfo('native_codex')).toMatchObject({ label: 'Codex CLI login', owner: 'source' });
    expect(credentialSourceInfo('native_claude')?.owner).toBe('source');
    expect(credentialSourceInfo('cliproxy')?.owner).toBe('source');
    expect(credentialSourceInfo('api_key')?.owner).toBe('key');
    expect(credentialSourceInfo(undefined)).toBeNull();
    for (const s of ['oauth', 'native_codex', 'native_claude', 'cliproxy', 'api_key']) expect(credentialSourceInfo(s)?.detail).not.toMatch(/[/~]\./);
  });
  it('maps connection kinds to sign-in providers', () => {
    expect(oauthProviderFor('codex')).toBe('codex');
    expect(oauthProviderFor('anthropic')).toBe('claude');
    expect(oauthProviderFor('gemini')).toBeNull();
  });
});
