import { describe, expect, it } from 'vitest';
import { score } from '../app/CommandPalette';
import { tokenize } from '../lib/highlight';
import { addTags, splitTags } from './TagInput';
import { bucketize, niceMax } from './TrafficChart';

describe('bucketize', () => {
  const now = Date.parse('2026-10-02T12:30:30Z');
  it('lays sparse minutes onto a continuous hour and drops out-of-window points', () => {
    const b = bucketize(
      [
        { timestamp: '2026-10-02T12:30:00Z', requests: 3, errors: 1 },
        { timestamp: Date.parse('2026-10-02T12:00:00Z') / 1000, requests: 2, errors: 0 },
        { timestamp: '2026-10-02T10:00:00Z', requests: 99, errors: 0 },
      ],
      now,
    );
    expect(b).toHaveLength(60);
    expect(b.at(-1)).toMatchObject({ requests: 3, errors: 1 });
    expect(b.reduce((s, x) => s + x.requests, 0)).toBe(5);
  });
  it('clamps impossible error counts', () => {
    expect(bucketize([{ timestamp: now, requests: 1, errors: 5 }], now).at(-1)?.errors).toBe(1);
  });
  it('picks clean axis maxima', () => {
    expect(niceMax(3)).toBe(4);
    expect(niceMax(7)).toBe(10);
    expect(niceMax(130)).toBe(200);
    expect(niceMax(240)).toBe(250);
  });
});

describe('TagInput helpers', () => {
  it('splits pasted lists and dedupes', () => {
    expect(splitTags(' a, b\nc  d,,')).toEqual(['a', 'b', 'c', 'd']);
    expect(addTags(['a'], ['a', 'b', 'b'])).toEqual(['a', 'b']);
  });
});

describe('palette score', () => {
  it('ranks substring matches above fuzzy ones and rejects misses', () => {
    expect(score('API keys', 'keys')).toBeGreaterThan(score('Activity', 'acy'));
    expect(score('Connections', 'cnct')).toBeGreaterThan(0);
    expect(score('Routes', 'xyz')).toBe(0);
  });
});

describe('highlight', () => {
  it('tokenizes without losing characters', () => {
    const code = 'model = "x" # c\n[a.b]\nn = 1';
    const t = tokenize(code, 'toml');
    expect(t.map((x) => x.text).join('')).toBe(code);
    expect(t.find((x) => x.kind === 'com')?.text).toBe('# c');
    expect(t.find((x) => x.kind === 'key')?.text).toBe('model');
  });
  it('does not treat URL fragments as comments', () => {
    const t = tokenize('curl http://a/#x', 'bash');
    expect(t.some((x) => x.kind === 'com')).toBe(false);
  });
  it('marks JSON keys', () => {
    const t = tokenize('{"a": "b"}', 'json');
    expect(t.filter((x) => x.kind === 'key').map((x) => x.text)).toEqual(['"a"']);
  });
});
