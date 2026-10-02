/** A dispatched Server-Sent Event, per the WHATWG event-stream spec. */
export interface SseEvent {
  event: string;
  data: string;
  id?: string;
  retry?: number;
}

/**
 * Incremental event-stream parser. Feed it decoded text chunks of any size; it
 * handles CRLF/CR/LF line endings split across chunks, multi-line `data`,
 * comments, and a missing trailing blank line (via `flush`).
 */
export class SseParser {
  private buffer = '';
  private pendingCR = false;
  private event = '';
  private data: string[] = [];
  private id: string | undefined;
  private retry: number | undefined;

  feed(chunk: string): SseEvent[] {
    const out: SseEvent[] = [];
    let text = chunk;
    // A CR at the end of the previous chunk already ended a line; drop a following LF.
    if (this.pendingCR && text.startsWith('\n')) text = text.slice(1);
    this.pendingCR = false;
    this.buffer += text;

    let start = 0;
    for (let i = 0; i < this.buffer.length; i++) {
      const ch = this.buffer[i];
      if (ch !== '\n' && ch !== '\r') continue;
      const line = this.buffer.slice(start, i);
      if (ch === '\r') {
        if (i + 1 < this.buffer.length) {
          if (this.buffer[i + 1] === '\n') i++;
        } else {
          this.pendingCR = true;
        }
      }
      start = i + 1;
      const ev = this.line(line);
      if (ev) out.push(ev);
    }
    this.buffer = this.buffer.slice(start);
    return out;
  }

  /** End of stream: dispatch a final event that wasn't followed by a blank line. */
  flush(): SseEvent[] {
    const out: SseEvent[] = [];
    if (this.buffer) {
      const ev = this.line(this.buffer);
      if (ev) out.push(ev);
      this.buffer = '';
    }
    const ev = this.dispatch();
    if (ev) out.push(ev);
    return out;
  }

  private line(line: string): SseEvent | null {
    if (line === '') return this.dispatch();
    if (line.startsWith(':')) return null;
    const colon = line.indexOf(':');
    const field = colon === -1 ? line : line.slice(0, colon);
    let value = colon === -1 ? '' : line.slice(colon + 1);
    if (value.startsWith(' ')) value = value.slice(1);
    switch (field) {
      case 'event':
        this.event = value;
        break;
      case 'data':
        this.data.push(value);
        break;
      case 'id':
        if (!value.includes('\0')) this.id = value;
        break;
      case 'retry':
        if (/^\d+$/.test(value)) this.retry = Number(value);
        break;
    }
    return null;
  }

  private dispatch(): SseEvent | null {
    const had = this.data.length > 0;
    const ev: SseEvent = { event: this.event || 'message', data: this.data.join('\n') };
    if (this.id !== undefined) ev.id = this.id;
    if (this.retry !== undefined) ev.retry = this.retry;
    this.event = '';
    this.data = [];
    this.retry = undefined;
    return had ? ev : null;
  }
}

/** Read a fetch body as SSE events. */
export async function* readSse(body: ReadableStream<Uint8Array>, signal?: AbortSignal): AsyncGenerator<SseEvent> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  const parser = new SseParser();
  try {
    while (true) {
      if (signal?.aborted) return;
      const { value, done } = await reader.read();
      if (done) break;
      for (const ev of parser.feed(decoder.decode(value, { stream: true }))) yield ev;
    }
    const tail = decoder.decode();
    if (tail) for (const ev of parser.feed(tail)) yield ev;
    for (const ev of parser.flush()) yield ev;
  } finally {
    reader.releaseLock();
  }
}
