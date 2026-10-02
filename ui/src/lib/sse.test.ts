import { describe, expect, it } from 'vitest';
import { SseParser, readSse } from './sse';

describe('SseParser', () => {
  it('parses events with names, ids and multi-line data', () => {
    const p = new SseParser();
    const out = p.feed('event: ping\nid: 7\ndata: a\ndata: b\n\n: comment\ndata: c\n\n');
    expect(out).toEqual([
      { event: 'ping', data: 'a\nb', id: '7' },
      { event: 'message', data: 'c', id: '7' },
    ]);
  });

  it('handles CRLF and CR split across chunk boundaries', () => {
    const p = new SseParser();
    const chunks = ['data: one\r', '\n\r', '\ndata: two\r\r'];
    const out = chunks.flatMap((c) => p.feed(c));
    expect(out.map((e) => e.data)).toEqual(['one', 'two']);
  });

  it('handles events split mid-field and mid-JSON', () => {
    const text = 'event: response.output_text.delta\ndata: {"type":"response.output_text.delta","delta":"Hé"}\n\n';
    for (let size = 1; size < text.length; size += 7) {
      const p = new SseParser();
      const out = [];
      for (let i = 0; i < text.length; i += size) out.push(...p.feed(text.slice(i, i + size)));
      expect(out).toHaveLength(1);
      expect(JSON.parse(out[0].data).delta).toBe('Hé');
    }
  });

  it('strips exactly one leading space and ignores events without data', () => {
    const p = new SseParser();
    expect(p.feed('data:  two spaces\n\nevent: lonely\n\n')).toEqual([{ event: 'message', data: ' two spaces' }]);
  });

  it('dispatches empty data lines and parses retry', () => {
    const p = new SseParser();
    expect(p.feed('retry: 1500\ndata:\n\n')).toEqual([{ event: 'message', data: '', retry: 1500 }]);
  });

  it('flushes a trailing event without a blank line', () => {
    const p = new SseParser();
    expect(p.feed('data: [DONE]')).toEqual([]);
    expect(p.flush()).toEqual([{ event: 'message', data: '[DONE]' }]);
  });
});

describe('readSse', () => {
  it('decodes UTF-8 split across byte chunks', async () => {
    const bytes = new TextEncoder().encode('data: 日本語\n\ndata: end');
    const body = new ReadableStream<Uint8Array>({
      start(c) {
        for (let i = 0; i < bytes.length; i += 2) c.enqueue(bytes.slice(i, i + 2));
        c.close();
      },
    });
    const out = [];
    for await (const ev of readSse(body)) out.push(ev.data);
    expect(out).toEqual(['日本語', 'end']);
  });
});
