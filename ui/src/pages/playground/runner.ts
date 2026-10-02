import { parseErrorBody, type ApiClient } from '../../lib/api';
import { eventsUrl } from '../../lib/events';
import { readSse } from '../../lib/sse';
import { applyEvent, emptyResult, hasContentDelta, parseFinalJson, type StreamResult } from '../../lib/stream';

export type PlaygroundTransport = 'http' | 'sse' | 'websocket';

export interface FrameLog {
  seq: number;
  /** ms since the run started */
  t: number;
  dir: 'out' | 'in' | 'note';
  label: string;
  bytes: number;
  data: string;
}

export interface RunMetrics {
  status: number | null;
  contentType: string | null;
  ttfb: number | null;
  ttft: number | null;
  total: number | null;
}

export interface RunState {
  running: boolean;
  result: StreamResult;
  frames: FrameLog[];
  metrics: RunMetrics;
  note: string | null;
}

export const initialRun = (): RunState => ({
  running: false,
  result: emptyResult(),
  frames: [],
  metrics: { status: null, contentType: null, ttfb: null, ttft: null, total: null },
  note: null,
});

const MAX_FRAMES = 2000;
const MAX_FRAME_TEXT = 64 * 1024;

/** Accumulates a run and reports snapshots at most once per animation frame. */
export class RunRecorder {
  state: RunState;
  private t0 = performance.now();
  private seq = 0;
  private scheduled = false;
  private readonly emit: (s: RunState) => void;
  private dropped = 0;

  constructor(emit: (s: RunState) => void) {
    this.emit = emit;
    this.state = { ...initialRun(), running: true };
    this.flush();
  }

  elapsed() {
    return Math.round(performance.now() - this.t0);
  }

  frame(dir: FrameLog['dir'], label: string, data: string) {
    if (this.state.frames.length >= MAX_FRAMES) {
      this.dropped++;
      return;
    }
    const text = data.length > MAX_FRAME_TEXT ? `${data.slice(0, MAX_FRAME_TEXT)}\n… truncated (${data.length} chars)` : data;
    this.state = {
      ...this.state,
      frames: [...this.state.frames, { seq: this.seq++, t: this.elapsed(), dir, label, bytes: new TextEncoder().encode(data).length, data: text }],
    };
    this.schedule();
  }

  apply(payload: unknown, eventName?: string) {
    const before = this.state.result;
    const after = applyEvent(before, payload, eventName);
    const metrics = { ...this.state.metrics };
    if (metrics.ttft === null && hasContentDelta(before, after)) metrics.ttft = this.elapsed();
    this.state = { ...this.state, result: after, metrics };
    this.schedule();
  }

  setResult(result: StreamResult) {
    const metrics = { ...this.state.metrics };
    if (metrics.ttft === null && (result.text || result.toolCalls.length || result.reasoning)) metrics.ttft = this.elapsed();
    this.state = { ...this.state, result, metrics };
    this.schedule();
  }

  metrics(patch: Partial<RunMetrics>) {
    this.state = { ...this.state, metrics: { ...this.state.metrics, ...patch } };
    this.schedule();
  }

  fail(message: string) {
    this.state = { ...this.state, result: { ...this.state.result, error: this.state.result.error ?? message } };
    this.schedule();
  }

  finish(note?: string) {
    const total = this.elapsed();
    let n = note ?? null;
    if (!n && !this.state.result.done && !this.state.result.error) n = 'The stream ended without a completion event.';
    if (this.dropped) n = `${n ? `${n} ` : ''}${this.dropped} later frames were not kept in the inspector.`;
    this.state = { ...this.state, running: false, note: n, metrics: { ...this.state.metrics, total } };
    this.flush();
  }

  private schedule() {
    if (this.scheduled) return;
    this.scheduled = true;
    requestAnimationFrame(() => {
      this.scheduled = false;
      this.emit(this.state);
    });
  }

  private flush() {
    this.emit(this.state);
  }
}

function safeJson(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

/** POST /api/playground and stream/parse whatever comes back. */
export async function runHttp(api: ApiClient, rec: RunRecorder, body: { model: string; input: string; transport: 'http' | 'sse' }, signal: AbortSignal) {
  rec.frame('out', `POST /api/playground`, JSON.stringify(body, null, 2));
  let res: Response;
  try {
    res = await api.raw('/api/playground', { method: 'POST', body, signal, allowHttpError: true });
  } catch (e) {
    rec.fail(e instanceof Error ? e.message : 'Request failed.');
    rec.finish(signal.aborted ? 'Stopped.' : undefined);
    return;
  }
  const contentType = res.headers.get('content-type');
  rec.metrics({ status: res.status, contentType, ttfb: rec.elapsed() });

  if (!res.ok) {
    const text = await res.text().catch(() => '');
    rec.frame('in', `HTTP ${res.status}`, text || '(empty body)');
    rec.fail(parseErrorBody(text, res.status).message);
    rec.finish();
    return;
  }

  try {
    if (contentType?.includes('text/event-stream') && res.body) {
      for await (const ev of readSse(res.body, signal)) {
        rec.frame('in', ev.event !== 'message' ? ev.event : (/"type"\s*:\s*"([^"]+)"/.exec(ev.data)?.[1] ?? 'data'), ev.data);
        if (ev.data === '[DONE]') {
          rec.apply('[DONE]');
          continue;
        }
        const json = safeJson(ev.data);
        if (json !== undefined) rec.apply(json, ev.event !== 'message' ? ev.event : undefined);
      }
    } else {
      const text = await res.text();
      rec.frame('in', `HTTP ${res.status} body`, text);
      const json = safeJson(text);
      rec.setResult(json === undefined ? { ...parseFinalJson(null), error: 'The gateway returned a non-JSON body. See the inspector.' } : parseFinalJson(json));
    }
    rec.finish(signal.aborted ? 'Stopped.' : undefined);
  } catch (e) {
    if (signal.aborted) {
      rec.finish('Stopped.');
      return;
    }
    rec.fail(e instanceof Error ? `Stream interrupted: ${e.message}` : 'Stream interrupted.');
    rec.finish();
  }
}

/**
 * A reusable playground WebSocket. The gateway binds one upstream session per
 * socket and model, so we keep it open between turns and reconnect on model change.
 */
export class PlaygroundSocket {
  private ws: WebSocket | null = null;
  model: string | null = null;
  turns = 0;
  private onFrame: ((data: string) => void) | null = null;
  private onClose: ((code: number, reason: string) => void) | null = null;
  private readonly onStateChange: () => void;

  constructor(onStateChange: () => void) {
    this.onStateChange = onStateChange;
  }

  get isOpen() {
    return this.ws?.readyState === WebSocket.OPEN;
  }

  close() {
    const ws = this.ws;
    this.ws = null;
    this.model = null;
    this.turns = 0;
    if (ws && ws.readyState <= WebSocket.OPEN) ws.close(1000, 'closed by user');
    this.onStateChange();
  }

  private connect(model: string, timeoutMs = 15_000): Promise<WebSocket> {
    this.close();
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(eventsUrl(window.location, `/api/playground/ws?model=${encodeURIComponent(model)}`));
      const timer = window.setTimeout(() => {
        ws.close();
        reject(new Error('Timed out opening the WebSocket.'));
      }, timeoutMs);
      ws.onopen = () => {
        window.clearTimeout(timer);
        this.ws = ws;
        this.model = model;
        this.turns = 0;
        this.onStateChange();
        resolve(ws);
      };
      ws.onmessage = (ev) => this.onFrame?.(typeof ev.data === 'string' ? ev.data : '[binary frame]');
      ws.onclose = (ev) => {
        window.clearTimeout(timer);
        if (this.ws === ws) {
          this.ws = null;
          this.model = null;
          this.onStateChange();
        }
        this.onClose?.(ev.code, ev.reason);
        reject(new Error(closeMessage(ev.code, ev.reason, true)));
      };
    });
  }

  async run(rec: RunRecorder, model: string, input: string, signal: AbortSignal) {
    const reused = this.isOpen && this.model === model;
    let ws: WebSocket;
    try {
      if (reused) ws = this.ws!;
      else {
        rec.frame('note', 'connect', `GET /api/playground/ws?model=${model} (Upgrade: websocket)`);
        ws = await this.connect(model);
      }
    } catch (e) {
      rec.fail(e instanceof Error ? e.message : 'Could not open the WebSocket.');
      rec.finish();
      return;
    }
    rec.metrics({ status: 101, contentType: 'websocket' });
    if (reused) rec.frame('note', 'reuse', `Reusing the open socket (turn ${this.turns + 1}).`);
    await new Promise<void>((resolve) => {
      let settled = false;
      const done = (note?: string) => {
        if (settled) return;
        settled = true;
        this.onFrame = null;
        this.onClose = null;
        signal.removeEventListener('abort', abort);
        rec.finish(note);
        resolve();
      };
      const abort = () => {
        // The upstream session can't cancel a response mid-flight; closing is the only stop.
        this.close();
        done('Stopped. The socket was closed to cancel the response.');
      };
      signal.addEventListener('abort', abort);
      this.onFrame = (data) => {
        if (rec.state.metrics.ttfb === null) rec.metrics({ ttfb: rec.elapsed() });
        const json = safeJson(data);
        const type = json && typeof json === 'object' ? String((json as { type?: unknown }).type ?? 'frame') : 'frame';
        rec.frame('in', type, data);
        if (json !== undefined) rec.apply(json);
        if (type === 'response.completed' || type === 'response.failed' || type === 'response.incomplete' || type === 'error') done();
      };
      this.onClose = (code, reason) => {
        rec.frame('note', `close ${code}`, reason || '(no reason)');
        rec.fail(closeMessage(code, reason, false));
        done();
      };
      const msg = JSON.stringify({ type: 'response.create', response: { model, input, stream: true } });
      rec.frame('out', 'response.create', JSON.stringify(JSON.parse(msg), null, 2));
      try {
        ws.send(msg);
        this.turns += 1;
        this.onStateChange();
      } catch {
        rec.fail('Could not send on the WebSocket.');
        done();
      }
    });
  }
}

export function closeMessage(code: number, reason: string, duringOpen: boolean): string {
  if (reason) return `Socket closed: ${reason}`;
  if (duringOpen || code === 1006) {
    return 'The gateway refused the WebSocket. Check that this model is served by an enabled, WebSocket-capable connection and that the gateway isn’t paused or at its concurrency limit.';
  }
  if (code === 1000) return 'The server closed the socket.';
  return `Socket closed (code ${code}).`;
}
