import { describe, expect, it } from 'vitest';
import { displayNames, normalizeInput, presetFor, routesUsing, strandedRoutes, validateConnection } from './connections';
import { directModels, moveItem, routeHealth, targetIssue, validateRoute } from './routes';
import type { Connection, ConnectionInput, Route } from './types';

const input = (p: Partial<ConnectionInput> = {}): ConnectionInput => ({
  name: 'OpenAI',
  kind: 'openai',
  base_url: 'https://api.openai.com/v1',
  enabled: true,
  models: ['gpt-6.1-sol'],
  supports_websocket: false,
  ...p,
});

const conn = (p: Partial<Connection>): Connection => ({
  id: 'c',
  name: 'Codex',
  kind: 'codex',
  base_url: 'https://chatgpt.com/backend-api/codex',
  enabled: true,
  models: ['gpt-6.1-sol'],
  supports_websocket: true,
  credential_present: true,
  created_at: '2026-10-01T00:00:00Z',
  ...p,
});

describe('validateConnection (mirrors the gateway)', () => {
  it('accepts a normal provider', () => {
    expect(validateConnection(input())).toEqual({});
  });
  it('rejects remote plain HTTP but allows loopback', () => {
    expect(validateConnection(input({ base_url: 'http://example.com/v1' })).base_url).toMatch(/only allowed for local/);
    expect(validateConnection(input({ base_url: 'http://127.0.0.1:11434/v1' }))).toEqual({});
    expect(validateConnection(input({ base_url: 'http://localhost:1234/v1' }))).toEqual({});
    expect(validateConnection(input({ base_url: 'http://[::1]:8000/v1' }))).toEqual({});
  });
  it('rejects credentials, query strings and junk in URLs', () => {
    expect(validateConnection(input({ base_url: 'https://user:pw@x.com' })).base_url).toMatch(/credentials/);
    expect(validateConnection(input({ base_url: 'https://x.com/v1?key=1' })).base_url).toMatch(/query/);
    expect(validateConnection(input({ base_url: 'ftp://x.com' })).base_url).toMatch(/http/);
    expect(validateConnection(input({ base_url: 'not a url' })).base_url).toMatch(/valid URL/);
  });
  it('requires a name and 1–100 models', () => {
    expect(validateConnection(input({ name: '  ' })).name).toBeTruthy();
    expect(validateConnection(input({ models: [] })).models).toBeTruthy();
    expect(validateConnection(input({ models: Array.from({ length: 101 }, (_, i) => `m${i}`) })).models).toBeTruthy();
  });
  it('only allows WebSockets on OpenAI/Codex and rejects multi-line keys', () => {
    expect(validateConnection(input({ kind: 'anthropic', supports_websocket: true })).supports_websocket).toBeTruthy();
    expect(validateConnection(input({ api_key: 'a\nb' })).api_key).toBeTruthy();
  });
});

describe('normalizeInput', () => {
  it('trims, dedupes models, strips trailing slashes and omits a blank key', () => {
    const out = normalizeInput(input({ name: ' x ', base_url: 'https://x.com/v1///', models: ['a', ' a ', '', 'b'], api_key: '   ' }));
    expect(out).toEqual({ ...input(), name: 'x', base_url: 'https://x.com/v1', models: ['a', 'b'] });
    expect('api_key' in out).toBe(false);
    expect(normalizeInput(input({ api_key: ' sk-1 ' })).api_key).toBe('sk-1');
  });
});

describe('multi-account helpers', () => {
  const a = conn({ id: 'aaaaaa1', name: 'Codex' });
  const b = conn({ id: 'bbbbbb2', name: 'Codex' });
  const c = conn({ id: 'cccccc3', name: 'Claude Code', kind: 'anthropic', models: ['claude-opus-5-5'], supports_websocket: false });
  const routes: Route[] = [
    { model: 'coding', strategy: 'failover', targets: [{ connection_id: a.id, model: 'gpt-6.1-sol' }, { connection_id: c.id, model: 'claude-opus-5-5' }] },
    { model: 'pool', strategy: 'round_robin', targets: [{ connection_id: a.id, model: 'gpt-6.1-sol' }, { connection_id: b.id, model: 'gpt-6.1-sol' }] },
  ];

  it('disambiguates duplicate account names', () => {
    const n = displayNames([a, b, c]);
    expect(n.get(a.id)).toBe('Codex · aaaaaa');
    expect(n.get(b.id)).toBe('Codex · bbbbbb');
    expect(n.get(c.id)).toBe('Claude Code');
  });

  it('finds routes that reference or would be stranded by a connection', () => {
    expect(routesUsing(routes, a.id).map((r) => r.model)).toEqual(['coding', 'pool']);
    expect(strandedRoutes(routes, [a, b, c], [a.id])).toEqual([]);
    expect(strandedRoutes(routes, [a, b, c], [a.id, b.id]).map((r) => r.model)).toEqual(['pool']);
  });

  it('reports target issues and route health', () => {
    expect(targetIssue({ connection_id: 'gone', model: 'x' }, [a])).toBe('missing-connection');
    expect(targetIssue({ connection_id: a.id, model: 'nope' }, [a])).toBe('model-missing');
    expect(targetIssue({ connection_id: a.id, model: 'gpt-6.1-sol' }, [{ ...a, enabled: false }])).toBe('disabled');
    expect(routeHealth(routes[1], [a, b])).toBe('ok');
    expect(routeHealth(routes[1], [a, { ...b, enabled: false }])).toBe('degraded');
    expect(routeHealth(routes[1], [])).toBe('down');
  });

  it('groups direct models served by several accounts', () => {
    const models = [a, b, c].flatMap((x) => x.models.map((m) => ({ id: m, connection_id: x.id, connection_name: x.name, kind: x.kind, supports_websocket: x.supports_websocket })));
    const d = directModels(models, [{ model: 'claude-opus-5-5', strategy: 'failover', targets: [] }]);
    expect(d).toHaveLength(1);
    expect(d[0]).toMatchObject({ id: 'gpt-6.1-sol' });
    expect(d[0].providers).toHaveLength(2);
  });

  it('validates route drafts', () => {
    const ok = { model: 'new', strategy: 'failover' as const, targets: [{ key: 'k1', connection_id: a.id, model: 'gpt-6.1-sol' }] };
    expect(validateRoute(ok, { existing: ['coding'], connections: [a] })).toEqual({});
    expect(validateRoute({ ...ok, model: 'coding' }, { existing: ['coding'], connections: [a] }).model).toMatch(/already exists/);
    expect(validateRoute({ ...ok, model: 'coding' }, { existing: ['coding'], previous: 'coding', connections: [a] })).toEqual({});
    expect(validateRoute({ ...ok, targets: [] }, { existing: [], connections: [a] }).targets).toBeTruthy();
    const dup = validateRoute({ ...ok, targets: [ok.targets[0], { ...ok.targets[0], key: 'k2' }] }, { existing: [], connections: [a] });
    expect(dup.target?.k2).toMatch(/twice/);
    expect(validateRoute({ ...ok, targets: [{ key: 'k', connection_id: a.id, model: 'x' }] }, { existing: [], connections: [a] }).target?.k).toMatch(/doesn’t list/);
  });

  it('moves items safely', () => {
    expect(moveItem([1, 2, 3], 2, 0)).toEqual([3, 1, 2]);
    expect(moveItem([1, 2, 3], 0, -1)).toEqual([1, 2, 3]);
  });

  it('maps connections to form presets', () => {
    expect(presetFor({ kind: 'openai', base_url: 'https://api.openai.com/v1' })).toBe('openai');
    expect(presetFor({ kind: 'openai', base_url: 'https://openrouter.ai/api/v1' })).toBe('compatible');
    expect(presetFor({ kind: 'gemini', base_url: '' })).toBe('gemini');
  });
});
