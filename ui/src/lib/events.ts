import type { EventMessage } from './types';

/**
 * - `connecting`: first attempt in flight
 * - `live`: socket open, pushing updates
 * - `reconnecting`: socket dropped, retrying with backoff (data may be briefly stale)
 * - `polling`: socket keeps failing; the app polls REST endpoints while we keep retrying
 * - `offline`: the browser reports no network
 * - `stopped`: not started / torn down
 */
export type LiveStatus = 'connecting' | 'live' | 'reconnecting' | 'polling' | 'offline' | 'stopped';

interface SocketLike {
  onopen: ((ev: unknown) => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
  onclose: ((ev: unknown) => void) | null;
  onerror: ((ev: unknown) => void) | null;
  close(code?: number, reason?: string): void;
}

export interface Timers {
  setTimeout(fn: () => void, ms: number): unknown;
  clearTimeout(id: unknown): void;
}

export interface EventsClientOptions {
  url: string;
  onMessage: (msg: EventMessage) => void;
  onStatus?: (status: LiveStatus, info: { attempt: number; nextRetryMs: number | null }) => void;
  createSocket?: (url: string) => SocketLike;
  timers?: Timers;
  random?: () => number;
  /** Consecutive failures before we tell the app to start polling. */
  failuresBeforePolling?: number;
  baseDelayMs?: number;
  maxDelayMs?: number;
}

/** Exponential backoff with "equal jitter": half fixed, half random. */
export function backoffDelay(attempt: number, base: number, max: number, random: () => number): number {
  const exp = Math.min(max, base * 2 ** Math.max(0, attempt - 1));
  return Math.round(exp / 2 + (random() * exp) / 2);
}

/** Validate an incoming frame. Unknown or malformed frames are dropped, never thrown. */
export function parseEventMessage(data: unknown): EventMessage | null {
  if (typeof data !== 'string') return null;
  let msg: unknown;
  try {
    msg = JSON.parse(data);
  } catch {
    return null;
  }
  if (!msg || typeof msg !== 'object') return null;
  const { type, data: payload } = msg as { type?: unknown; data?: unknown };
  if ((type === 'request' || type === 'overview') && payload && typeof payload === 'object') {
    return msg as EventMessage;
  }
  return null;
}

export function eventsUrl(loc: Pick<Location, 'protocol' | 'host'>, path = '/api/events'): string {
  return `${loc.protocol === 'https:' ? 'wss:' : 'ws:'}//${loc.host}${path}`;
}

export class EventsClient {
  private socket: SocketLike | null = null;
  private retryTimer: unknown = null;
  private failures = 0;
  private status: LiveStatus = 'stopped';
  private running = false;
  private readonly o: Required<Omit<EventsClientOptions, 'onStatus'>> & Pick<EventsClientOptions, 'onStatus'>;

  constructor(options: EventsClientOptions) {
    this.o = {
      createSocket: (url) => new WebSocket(url) as unknown as SocketLike,
      timers: { setTimeout: (fn, ms) => setTimeout(fn, ms), clearTimeout: (id) => clearTimeout(id as number) },
      random: Math.random,
      failuresBeforePolling: 3,
      baseDelayMs: 1000,
      maxDelayMs: 30_000,
      ...options,
    };
  }

  get currentStatus() {
    return this.status;
  }

  start() {
    if (this.running) return;
    this.running = true;
    this.failures = 0;
    this.connect();
  }

  stop() {
    this.running = false;
    this.clearRetry();
    this.teardownSocket();
    this.setStatus('stopped', null);
  }

  /** Retry right away (e.g. the tab became visible or the network came back). */
  retryNow() {
    if (!this.running || this.status === 'live' || this.status === 'connecting') return;
    this.clearRetry();
    this.connect();
  }

  /** The browser went offline: stop hammering, wait for `retryNow`. */
  markOffline() {
    if (!this.running) return;
    this.clearRetry();
    this.teardownSocket();
    this.setStatus('offline', null);
  }

  private connect() {
    this.teardownSocket();
    this.setStatus(this.failures === 0 ? 'connecting' : this.failures >= this.o.failuresBeforePolling ? 'polling' : 'reconnecting', null);
    let socket: SocketLike;
    try {
      socket = this.o.createSocket(this.o.url);
    } catch {
      this.handleDisconnect(null);
      return;
    }
    this.socket = socket;
    socket.onopen = () => {
      if (this.socket !== socket) return;
      this.failures = 0;
      this.setStatus('live', null);
    };
    socket.onmessage = (ev) => {
      if (this.socket !== socket) return;
      const msg = parseEventMessage(ev.data);
      if (msg) this.o.onMessage(msg);
    };
    // `error` is always followed by `close`; handle the disconnect once, on close.
    socket.onerror = () => {};
    socket.onclose = () => this.handleDisconnect(socket);
  }

  private handleDisconnect(socket: SocketLike | null) {
    if (socket && this.socket !== socket) return;
    this.socket = null;
    if (!this.running || this.status === 'offline') return;
    this.failures += 1;
    const delay = backoffDelay(this.failures, this.o.baseDelayMs, this.o.maxDelayMs, this.o.random);
    const status = this.failures >= this.o.failuresBeforePolling ? 'polling' : 'reconnecting';
    this.setStatus(status, delay);
    this.clearRetry();
    this.retryTimer = this.o.timers.setTimeout(() => {
      this.retryTimer = null;
      if (this.running) this.connect();
    }, delay);
  }

  private teardownSocket() {
    const s = this.socket;
    this.socket = null;
    if (!s) return;
    s.onopen = s.onmessage = s.onclose = s.onerror = null;
    try {
      s.close(1000, 'client closing');
    } catch {
      // ignore
    }
  }

  private clearRetry() {
    if (this.retryTimer !== null) this.o.timers.clearTimeout(this.retryTimer);
    this.retryTimer = null;
  }

  private setStatus(status: LiveStatus, nextRetryMs: number | null) {
    const changed = status !== this.status;
    this.status = status;
    if (changed || nextRetryMs !== null) this.o.onStatus?.(status, { attempt: this.failures, nextRetryMs });
  }
}
