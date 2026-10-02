import { describe, expect, it, vi } from 'vitest';
import { EventsClient, backoffDelay, eventsUrl, parseEventMessage, type LiveStatus } from './events';

class FakeSocket {
  onopen: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  closed = false;
  close() {
    this.closed = true;
  }
  open() {
    this.onopen?.({});
  }
  drop() {
    this.onerror?.({});
    this.onclose?.({});
  }
  send(data: string) {
    this.onmessage?.({ data });
  }
}

function harness(opts: { failuresBeforePolling?: number } = {}) {
  const sockets: FakeSocket[] = [];
  const timers: { fn: () => void; ms: number; id: number }[] = [];
  let nextId = 1;
  const statuses: { s: LiveStatus; next: number | null }[] = [];
  const messages: unknown[] = [];
  const client = new EventsClient({
    url: 'ws://x/api/events',
    createSocket: () => {
      const s = new FakeSocket();
      sockets.push(s);
      return s;
    },
    timers: {
      setTimeout: (fn, ms) => {
        const id = nextId++;
        timers.push({ fn, ms, id });
        return id;
      },
      clearTimeout: (id) => {
        const i = timers.findIndex((t) => t.id === id);
        if (i !== -1) timers.splice(i, 1);
      },
    },
    random: () => 0.5,
    onMessage: (m) => messages.push(m),
    onStatus: (s, info) => statuses.push({ s, next: info.nextRetryMs }),
    ...opts,
  });
  const fire = () => {
    const t = timers.shift();
    t?.fn();
    return t;
  };
  return { client, sockets, timers, statuses, messages, fire, last: () => sockets[sockets.length - 1] };
}

describe('backoffDelay', () => {
  it('grows exponentially, is capped, and jitters within [half, full]', () => {
    expect(backoffDelay(1, 1000, 30000, () => 0)).toBe(500);
    expect(backoffDelay(1, 1000, 30000, () => 1)).toBe(1000);
    expect(backoffDelay(3, 1000, 30000, () => 1)).toBe(4000);
    expect(backoffDelay(20, 1000, 30000, () => 1)).toBe(30000);
    expect(backoffDelay(20, 1000, 30000, () => 0)).toBe(15000);
  });
});

describe('parseEventMessage', () => {
  it('accepts known frames and drops junk', () => {
    expect(parseEventMessage('{"type":"request","data":{"id":"1"}}')).toEqual({ type: 'request', data: { id: '1' } });
    expect(parseEventMessage('{"type":"overview","data":{}}')).not.toBeNull();
    expect(parseEventMessage('{"type":"other","data":{}}')).toBeNull();
    expect(parseEventMessage('{"type":"request"}')).toBeNull();
    expect(parseEventMessage('not json')).toBeNull();
    expect(parseEventMessage(new ArrayBuffer(2))).toBeNull();
  });
});

describe('eventsUrl', () => {
  it('follows the page scheme', () => {
    expect(eventsUrl({ protocol: 'http:', host: '127.0.0.1:7410' })).toBe('ws://127.0.0.1:7410/api/events');
    expect(eventsUrl({ protocol: 'https:', host: 'yard.example' })).toBe('wss://yard.example/api/events');
  });
});

describe('EventsClient', () => {
  it('connects, goes live and delivers messages', () => {
    const h = harness();
    h.client.start();
    expect(h.statuses.at(-1)?.s).toBe('connecting');
    h.last().open();
    expect(h.client.currentStatus).toBe('live');
    h.last().send('{"type":"request","data":{"id":"a"}}');
    h.last().send('garbage');
    expect(h.messages).toEqual([{ type: 'request', data: { id: 'a' } }]);
  });

  it('reconnects with backoff, then falls back to polling, then recovers', () => {
    const h = harness({ failuresBeforePolling: 3 });
    h.client.start();
    h.last().open();
    h.last().drop();
    expect(h.client.currentStatus).toBe('reconnecting');
    expect(h.timers[0].ms).toBe(750); // 1000 * 0.75 with random 0.5
    h.fire();
    h.last().drop(); // fails before opening
    expect(h.timers[0].ms).toBe(1500);
    h.fire();
    h.last().drop();
    expect(h.client.currentStatus).toBe('polling');
    // Still retrying in the background while polling.
    h.fire();
    expect(h.client.currentStatus).toBe('polling');
    h.last().open();
    expect(h.client.currentStatus).toBe('live');
    // Failure count resets after a successful open.
    h.last().drop();
    expect(h.timers[0].ms).toBe(750);
  });

  it('handles error+close as a single disconnect', () => {
    const h = harness();
    h.client.start();
    h.last().drop();
    expect(h.timers).toHaveLength(1);
  });

  it('ignores events from stale sockets', () => {
    const h = harness();
    h.client.start();
    const first = h.last();
    first.drop();
    h.fire();
    const second = h.last();
    first.onclose?.({});
    first.onmessage?.({ data: '{"type":"request","data":{}}' });
    expect(h.timers).toHaveLength(0);
    expect(h.messages).toHaveLength(0);
    second.open();
    expect(h.client.currentStatus).toBe('live');
  });

  it('retryNow skips the wait; offline pauses retries', () => {
    const h = harness();
    h.client.start();
    h.last().drop();
    expect(h.timers).toHaveLength(1);
    h.client.retryNow();
    expect(h.timers).toHaveLength(0);
    expect(h.sockets).toHaveLength(2);
    h.client.markOffline();
    expect(h.client.currentStatus).toBe('offline');
    expect(h.sockets[1].closed).toBe(true);
    expect(h.timers).toHaveLength(0);
    h.client.retryNow();
    expect(h.sockets).toHaveLength(3);
  });

  it('stop tears everything down and is idempotent', () => {
    const h = harness();
    h.client.start();
    h.client.start();
    expect(h.sockets).toHaveLength(1);
    h.last().drop();
    h.client.stop();
    expect(h.timers).toHaveLength(0);
    expect(h.client.currentStatus).toBe('stopped');
  });

  it('treats a constructor throw as a failed attempt', () => {
    const onStatus = vi.fn();
    const timers: (() => void)[] = [];
    const c = new EventsClient({
      url: 'ws://x',
      createSocket: () => {
        throw new Error('blocked');
      },
      timers: { setTimeout: (fn) => timers.push(fn), clearTimeout: () => {} },
      onMessage: () => {},
      onStatus,
    });
    c.start();
    expect(c.currentStatus).toBe('reconnecting');
    expect(timers).toHaveLength(1);
  });
});
