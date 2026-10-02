import { ApiError } from './api';
import type { Connection, OAuthFlow, OAuthProvider } from './types';

/** Fixed loopback callback ports the gateway listens on during a sign-in. */
export const CALLBACK_PORTS: Record<OAuthProvider, number> = { codex: 1455, claude: 54545, antigravity: 51121 };

export const PROVIDER_COPY: Record<OAuthProvider, { name: string; account: string; cli: string; example: string }> = {
  codex: {
    name: 'ChatGPT',
    account: 'ChatGPT account (Codex)',
    cli: 'Codex CLI',
    example: 'http://localhost:1455/auth/callback?code=…&state=…',
  },
  claude: {
    name: 'Claude',
    account: 'Claude account (Claude Code)',
    cli: 'Claude Code',
    example: 'http://localhost:54545/callback?code=…&state=…  or  code#state',
  },
  antigravity: {
    name: 'Google',
    account: 'Antigravity account (Google)',
    cli: 'Antigravity',
    example: 'http://localhost:51121/oauth-callback?code=…&state=…',
  },
};

export type CallbackCheck = { ok: true } | { ok: false; message: string };

/**
 * Light client-side check of a pasted callback, so obvious mistakes get a helpful
 * hint before a round-trip. The gateway does the real validation.
 */
export function checkCallbackInput(raw: string): CallbackCheck {
  const input = raw.trim();
  if (!input) return { ok: false, message: 'Paste the full address from your browser’s address bar.' };
  if (input.length > 8192) return { ok: false, message: 'That’s too long to be a sign-in callback.' };
  let url: URL | null;
  try {
    url = new URL(input);
    // `localhost:1455/…` parses with "localhost:" as the scheme; only http(s) counts.
    if (url.protocol !== 'http:' && url.protocol !== 'https:') url = null;
  } catch {
    url = null;
  }
  if (url) {
    const p = url.searchParams;
    if (p.get('error')) return { ok: true }; // let the gateway report the provider's error
    if (!p.get('code')) return { ok: false, message: 'That address has no sign-in code. Copy the address of the page you landed on after approving.' };
    if (!p.get('state')) return { ok: false, message: 'That address is missing its state parameter. Copy the whole address, not part of it.' };
    return { ok: true };
  }
  if (/^[^\s#]+#[^\s#]+$/.test(input)) return { ok: true }; // Claude's manual code#state
  return { ok: false, message: 'Paste the full callback address (starting with http://localhost) or Claude’s code#state.' };
}

export type FlowPhase = 'idle' | 'starting' | 'pending' | 'submitting' | 'complete' | 'error' | 'expired' | 'cancelled';

export interface FlowState {
  phase: FlowPhase;
  id: string | null;
  authorizationUrl: string | null;
  /** Seconds left before the gateway gives up waiting. */
  remaining: number | null;
  connection: Connection | null;
  message: string | null;
  /** Error kind for start failures, e.g. a busy callback port (409). */
  errorKind: 'busy' | 'other' | null;
  /** Inline error for the pasted callback field. */
  callbackError: string | null;
  /** Polling hit network trouble; we keep trying. */
  reconnecting: boolean;
}

export const initialFlowState: FlowState = {
  phase: 'idle',
  id: null,
  authorizationUrl: null,
  remaining: null,
  connection: null,
  message: null,
  errorKind: null,
  callbackError: null,
  reconnecting: false,
};

export interface OAuthApi {
  oauthStart(provider: OAuthProvider): Promise<OAuthFlow>;
  oauthStatus(id: string): Promise<OAuthFlow>;
  oauthCancel(id: string): Promise<OAuthFlow>;
  oauthCallback(id: string, input: string): Promise<OAuthFlow>;
}

export interface Timers {
  setTimeout(fn: () => void, ms: number): unknown;
  clearTimeout(id: unknown): void;
  now(): number;
}

const realTimers: Timers = {
  setTimeout: (fn, ms) => setTimeout(fn, ms),
  clearTimeout: (id) => clearTimeout(id as number),
  now: () => Date.now(),
};

/**
 * Drives one browser sign-in: start, poll until complete/error/expired, accept a
 * pasted callback (gateway on another machine), and always cancel on close so the
 * callback port is released.
 */
export class OAuthFlowController {
  private state: FlowState = initialFlowState;
  private listeners = new Set<() => void>();
  private pollTimer: unknown = null;
  private tickTimer: unknown = null;
  private deadline: number | null = null;
  // Bumped on every start/cancel; async work from an older generation is ignored
  // (and a flow started by a stale generation is cancelled to free its port).
  private generation = 0;

  constructor(
    private readonly api: OAuthApi,
    private readonly provider: OAuthProvider,
    private readonly opts: { pollMs?: number; timers?: Timers; onComplete?: (c: Connection | null) => void } = {},
  ) {}

  get snapshot() {
    return this.state;
  }

  subscribe = (fn: () => void) => {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  };

  private get t() {
    return this.opts.timers ?? realTimers;
  }

  private set(patch: Partial<FlowState>) {
    this.state = { ...this.state, ...patch };
    this.listeners.forEach((l) => l());
  }

  async start() {
    const gen = ++this.generation;
    this.stopTimers();
    this.set({ ...initialFlowState, phase: 'starting' });
    try {
      const flow = await this.api.oauthStart(this.provider);
      if (gen !== this.generation) {
        // Closed while starting: release the port the gateway just bound.
        void this.api.oauthCancel(flow.id).catch(() => {});
        return;
      }
      this.set({ phase: 'pending', id: flow.id, authorizationUrl: flow.authorization_url ?? null });
      this.setDeadline(flow.expires_in_seconds);
      this.schedulePoll();
      this.tick();
    } catch (e) {
      if (gen !== this.generation) return;
      const busy = e instanceof ApiError && e.status === 409;
      this.set({
        phase: 'error',
        errorKind: busy ? 'busy' : 'other',
        message: e instanceof Error ? e.message : 'Couldn’t start the sign-in.',
      });
    }
  }

  async submitCallback(input: string) {
    const id = this.state.id;
    if (!id || (this.state.phase !== 'pending' && this.state.phase !== 'submitting')) return;
    const check = checkCallbackInput(input);
    if (!check.ok) {
      this.set({ callbackError: check.message });
      return;
    }
    const gen = this.generation;
    this.set({ phase: 'submitting', callbackError: null });
    try {
      const flow = await this.api.oauthCallback(id, input.trim());
      if (gen !== this.generation) return;
      this.apply(flow);
      if (this.state.phase === 'submitting') this.set({ phase: 'pending' });
    } catch (e) {
      if (gen !== this.generation) return;
      if (e instanceof ApiError && e.status === 404) {
        this.finish({ phase: 'expired', message: e.message });
        return;
      }
      this.set({ phase: 'pending', callbackError: e instanceof Error ? e.message : 'That callback didn’t work.' });
    }
  }

  /** Cancel a pending sign-in (closing the dialog, leaving the page). Safe to call repeatedly. */
  cancel() {
    const { id, phase } = this.state;
    this.generation++;
    this.stopTimers();
    if (id && (phase === 'pending' || phase === 'submitting' || phase === 'starting')) {
      void this.api.oauthCancel(id).catch(() => {});
    }
    if (phase === 'pending' || phase === 'submitting' || phase === 'starting') this.set({ phase: 'cancelled' });
  }

  /** Unmount: cancel any pending flow. The controller can be started again (React StrictMode remounts). */
  dispose() {
    this.cancel();
  }

  private apply(flow: OAuthFlow) {
    if (typeof flow.expires_in_seconds === 'number') this.setDeadline(flow.expires_in_seconds);
    switch (flow.status) {
      case 'complete':
        this.finish({ phase: 'complete', connection: flow.connection ?? null, message: flow.message ?? null });
        this.opts.onComplete?.(flow.connection ?? null);
        break;
      case 'expired':
        this.finish({ phase: 'expired', message: flow.message ?? 'Sign-in expired. Start again.' });
        break;
      case 'error':
        this.finish({ phase: 'error', errorKind: 'other', message: flow.message ?? 'Sign-in failed.' });
        break;
      default:
        break;
    }
  }

  private finish(patch: Partial<FlowState>) {
    this.stopTimers();
    this.set({ ...patch, remaining: null, reconnecting: false, callbackError: null });
  }

  private setDeadline(seconds: number | undefined) {
    if (typeof seconds !== 'number') return;
    this.deadline = this.t.now() + seconds * 1000;
    this.set({ remaining: Math.max(0, Math.ceil(seconds)) });
  }

  private tick = () => {
    if (this.deadline === null) return;
    const left = Math.max(0, Math.ceil((this.deadline - this.t.now()) / 1000));
    if (left !== this.state.remaining) this.set({ remaining: left });
    if (this.state.phase === 'pending' || this.state.phase === 'submitting') this.tickTimer = this.t.setTimeout(this.tick, 1000);
  };

  private schedulePoll() {
    const gen = this.generation;
    this.pollTimer = this.t.setTimeout(async () => {
      if (gen !== this.generation || !this.state.id) return;
      try {
        const flow = await this.api.oauthStatus(this.state.id);
        if (gen !== this.generation) return;
        if (this.state.reconnecting) this.set({ reconnecting: false });
        this.apply(flow);
      } catch (e) {
        if (gen !== this.generation) return;
        if (e instanceof ApiError && e.status === 404) {
          this.finish({ phase: 'expired', message: e.message });
          return;
        }
        // Network blips or a gateway restart: keep waiting, but say so.
        this.set({ reconnecting: true });
      }
      if (gen === this.generation && (this.state.phase === 'pending' || this.state.phase === 'submitting')) this.schedulePoll();
    }, this.opts.pollMs ?? 1500);
  }

  private stopTimers() {
    if (this.pollTimer !== null) this.t.clearTimeout(this.pollTimer);
    if (this.tickTimer !== null) this.t.clearTimeout(this.tickTimer);
    this.pollTimer = this.tickTimer = null;
  }
}

/** Human description of where a credential comes from and who keeps it fresh. Never shows paths. */
export function credentialSourceInfo(source: string | undefined | null): { label: string; detail: string; owner: 'gateway' | 'source' | 'key' } | null {
  switch (source) {
    case 'oauth':
      return { label: 'Browser sign-in', detail: 'Switchyard owns this sign-in and refreshes it automatically, independent of any CLI.', owner: 'gateway' };
    case 'native_codex':
      return { label: 'Codex CLI login', detail: 'Follows your Codex CLI login. Switchyard never refreshes it; if it expires, run codex login, then re-import.', owner: 'source' };
    case 'native_claude':
      return { label: 'Claude Code login', detail: 'Follows your Claude Code login. Switchyard never refreshes it; if it expires, sign in to Claude Code again, then re-import.', owner: 'source' };
    case 'native_antigravity':
      return { label: 'Antigravity login', detail: 'Follows the Antigravity CLI login it was imported from. Switchyard never refreshes it; sign in to Antigravity again, then re-import.', owner: 'source' };
    case 'native_opencode':
      return { label: 'OpenCode key', detail: 'An OpenCode Zen or Go key imported read-only from OpenCode. Re-import if you change it in OpenCode.', owner: 'source' };
    case 'cliproxy':
      return { label: 'CLIProxyAPI account', detail: 'Follows the CLIProxyAPI auth file it was imported from. Re-import after that login changes.', owner: 'source' };
    case 'api_key':
      return { label: 'API key', detail: 'A provider API key stored by Switchyard. Replace it from Edit.', owner: 'key' };
    default:
      return null;
  }
}

/** Which browser sign-in provider can (re)authenticate this connection kind. */
export function oauthProviderFor(kind: string): OAuthProvider | null {
  return kind === 'codex' ? 'codex' : kind === 'anthropic' ? 'claude' : kind === 'antigravity' ? 'antigravity' : null;
}

export function formatCountdown(seconds: number | null): string {
  if (seconds === null) return '';
  const m = Math.floor(seconds / 60);
  const s = seconds % 60;
  return `${m}:${String(s).padStart(2, '0')}`;
}
