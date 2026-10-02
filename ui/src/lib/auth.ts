import { ApiError, type ApiClient } from './api';

/**
 * The pasted admin token lives in sessionStorage only: it disappears when the tab
 * closes and is never written to localStorage, cookies, logs or the URL.
 */
const TOKEN_KEY = 'switchyard.admin-token';

export interface TokenStore {
  get(): string | null;
  set(token: string): void;
  clear(): void;
}

export function createTokenStore(storage: Pick<Storage, 'getItem' | 'setItem' | 'removeItem'> | null): TokenStore {
  let memory: string | null = null;
  return {
    get() {
      try {
        return storage?.getItem(TOKEN_KEY) ?? memory;
      } catch {
        return memory;
      }
    },
    set(token) {
      memory = token;
      try {
        storage?.setItem(TOKEN_KEY, token);
      } catch {
        // Storage disabled: keep it in memory for this page only.
      }
    },
    clear() {
      memory = null;
      try {
        storage?.removeItem(TOKEN_KEY);
      } catch {
        // ignore
      }
    },
  };
}

function safeSessionStorage(): Storage | null {
  try {
    return typeof window !== 'undefined' ? window.sessionStorage : null;
  } catch {
    return null;
  }
}

export const tokenStore = createTokenStore(safeSessionStorage());

export type AuthState =
  | { status: 'checking' }
  | { status: 'ready'; mode: 'cookie' | 'token' }
  | { status: 'needs-token'; reason: 'remote' | 'expired' | 'rejected' }
  | { status: 'error'; kind: 'unreachable' | 'forbidden' | 'server'; message: string };

const UNREACHABLE: AuthState = {
  status: 'error',
  kind: 'unreachable',
  message: "Can't reach the Switchyard gateway. Check that it's running, then retry.",
};

function failure(e: unknown): AuthState {
  if (e instanceof ApiError) {
    if (e.kind === 'network') return UNREACHABLE;
    if (e.status === 403) return { status: 'error', kind: 'forbidden', message: e.message };
    return { status: 'error', kind: 'server', message: e.message };
  }
  return { status: 'error', kind: 'server', message: 'Unexpected error while signing in.' };
}

/**
 * Establish an admin session:
 * 1. GET /api/session mints the HttpOnly cookie for same-origin loopback browsers.
 * 2. Otherwise exchange a token saved in sessionStorage via POST /api/session (Bearer).
 * 3. Otherwise ask for a token.
 * Backends without /api/session (404/405/501) are probed via /api/overview instead.
 */
export async function bootstrapSession(api: ApiClient, tokens: TokenStore): Promise<AuthState> {
  try {
    await api.session({ silent401: true, token: null });
    return { status: 'ready', mode: 'cookie' };
  } catch (e) {
    if (!(e instanceof ApiError)) return failure(e);
    if (e.kind === 'network') return UNREACHABLE;
    if (e.status === 403) return failure(e);
    if (!e.isUnsupported && e.status !== 401) return failure(e);
  }
  const stored = tokens.get();
  if (!stored) return { status: 'needs-token', reason: 'remote' };
  const verdict = await exchangeToken(api, stored);
  if (verdict === 'ok') return { status: 'ready', mode: 'token' };
  if (verdict === 'rejected') {
    tokens.clear();
    return { status: 'needs-token', reason: 'expired' };
  }
  return verdict;
}

/** Try the token: mint a cookie with it, or (older backends) just check it against /api/overview. */
export async function exchangeToken(api: ApiClient, token: string): Promise<'ok' | 'rejected' | AuthState> {
  try {
    await api.mintSession(token);
    return 'ok';
  } catch (e) {
    if (e instanceof ApiError && e.status === 401) return 'rejected';
    if (!(e instanceof ApiError) || !e.isUnsupported) return failure(e);
  }
  try {
    await api.overview({ token, silent401: true });
    return 'ok';
  } catch (e) {
    if (e instanceof ApiError && e.status === 401) return 'rejected';
    return failure(e);
  }
}

/** Normalise a pasted token: trim, drop an accidental "Bearer " prefix and surrounding quotes. */
export function cleanToken(input: string): string {
  return input
    .trim()
    .replace(/^bearer\s+/i, '')
    .replace(/^["'](.*)["']$/, '$1')
    .trim();
}

/** Verify and persist a pasted token. */
export async function signInWithToken(api: ApiClient, tokens: TokenStore, input: string): Promise<AuthState> {
  const token = cleanToken(input);
  if (!token) return { status: 'needs-token', reason: 'rejected' };
  const verdict = await exchangeToken(api, token);
  if (verdict === 'ok') {
    tokens.set(token);
    return { status: 'ready', mode: 'token' };
  }
  if (verdict === 'rejected') return { status: 'needs-token', reason: 'rejected' };
  return verdict;
}
