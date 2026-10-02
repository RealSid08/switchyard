import { describe, expect, it } from 'vitest';
import { CLIENT_ORDER, buildGuide, kindMismatch, resolveGatewayUrls, shellQuote } from './snippets';

const loc = { protocol: 'http:', hostname: '127.0.0.1', host: '127.0.0.1:5180', origin: 'http://127.0.0.1:5180' };
const cfg = { host: '127.0.0.1', port: 7410, api_base: 'http://127.0.0.1:7410/v1', websocket_url: 'ws://127.0.0.1:7410/v1/responses' };

describe('resolveGatewayUrls', () => {
  it('derives every base from api_base', () => {
    expect(resolveGatewayUrls(cfg, loc)).toEqual({
      origin: 'http://127.0.0.1:7410',
      openai: 'http://127.0.0.1:7410/v1',
      anthropic: 'http://127.0.0.1:7410',
      gemini: 'http://127.0.0.1:7410/v1beta',
      websocket: 'ws://127.0.0.1:7410/v1/responses',
    });
  });

  it('accepts api_base without /v1 and with a trailing slash', () => {
    expect(resolveGatewayUrls({ ...cfg, api_base: 'http://127.0.0.1:7410/' }, loc).openai).toBe('http://127.0.0.1:7410/v1');
  });

  it('keeps a path prefix (reverse proxy) intact', () => {
    const u = resolveGatewayUrls({ ...cfg, api_base: 'https://box.example/yard/v1', websocket_url: '' }, loc);
    expect(u.anthropic).toBe('https://box.example/yard');
    expect(u.websocket).toBe('wss://box.example/yard/v1/responses');
  });

  it('replaces wildcard bind addresses with the host the browser used', () => {
    const remote = { protocol: 'http:', hostname: 'yard.tailnet', host: 'yard.tailnet:7410', origin: 'http://yard.tailnet:7410' };
    const u = resolveGatewayUrls({ host: '0.0.0.0', port: 7410, api_base: 'http://0.0.0.0:7410/v1', websocket_url: 'ws://0.0.0.0:7410/v1/responses' }, remote);
    expect(u.openai).toBe('http://yard.tailnet:7410/v1');
    expect(u.websocket).toBe('ws://yard.tailnet:7410/v1/responses');
  });

  it('falls back to host/port, then to the page origin', () => {
    expect(resolveGatewayUrls({ host: '::', port: 9000, api_base: '', websocket_url: '' }, loc).origin).toBe('http://127.0.0.1:9000');
    expect(resolveGatewayUrls(null, loc).openai).toBe('http://127.0.0.1:5180/v1');
  });
});

describe('shellQuote', () => {
  it('leaves safe strings alone and quotes the rest', () => {
    expect(shellQuote('gpt-6.1-sol')).toBe('gpt-6.1-sol');
    expect(shellQuote('qwen3:8b')).toBe('qwen3:8b');
    expect(shellQuote("it's")).toBe(`'it'"'"'s'`);
    expect(shellQuote('a b')).toBe("'a b'");
  });
});

describe('buildGuide', () => {
  const urls = resolveGatewayUrls(cfg, loc);
  it('builds every client without leaking a real key', () => {
    for (const id of CLIENT_ORDER) {
      const g = buildGuide(id, { urls, model: 'coding', websocket: true });
      expect(g.blocks.length).toBeGreaterThan(0);
      for (const b of g.blocks) expect(b.code).not.toMatch(/sy_[0-9a-f]{20}/);
    }
  });

  it('Codex config uses the Responses wire API and only enables WebSockets when supported', () => {
    const ws = buildGuide('codex', { urls, model: 'gpt-6.1-sol', websocket: true }).blocks[1].code;
    expect(ws).toContain('base_url = "http://127.0.0.1:7410/v1"');
    expect(ws).toContain('wire_api = "responses"');
    expect(ws).toContain('env_key = "SWITCHYARD_API_KEY"');
    expect(ws).toContain('supports_websockets = true');
    expect(buildGuide('codex', { urls, model: 'm', websocket: false }).blocks[1].code).not.toContain('supports_websockets');
  });

  it('Claude Code points at the Anthropic base, not /v1', () => {
    const code = buildGuide('claude-code', { urls, model: 'claude-opus-5-5' }).blocks[1].code;
    expect(code).toContain('ANTHROPIC_BASE_URL=http://127.0.0.1:7410\n');
    expect(code).toContain('ANTHROPIC_AUTH_TOKEN="$SWITCHYARD_API_KEY"');
  });

  it('escapes awkward model names in every language', () => {
    const model = `we"ird/mo'del:1`;
    const opencode = JSON.parse(buildGuide('opencode', { urls, model }).blocks[1].code);
    expect(opencode.provider.switchyard.models[model]).toEqual({ name: model });
    const curl = buildGuide('curl', { urls, model }).blocks[0].code;
    expect(curl).toContain(`'"'"'`);
    const gem = buildGuide('curl', { urls, model }).blocks[3].code;
    expect(gem).toContain('models/we%22ird%2Fmo');
    const py = buildGuide('openai-sdk', { urls, model }).blocks[0].code;
    expect(py).toContain(JSON.stringify(model));
  });
});

describe('gemini guide', () => {
  const urls = resolveGatewayUrls(cfg, loc);
  it('uses x-goog-api-key and the gateway origin as SDK base URL', () => {
    const g = buildGuide('gemini', { urls, model: 'gemini-3-pro' });
    const all = g.blocks.map((b) => b.code).join('\n');
    expect(all).toContain('base_url="http://127.0.0.1:7410"');
    expect(all).toContain('baseUrl: "http://127.0.0.1:7410"');
    expect(all).toContain('x-goog-api-key: $SWITCHYARD_API_KEY');
    expect(all).toContain('/v1beta/models/gemini-3-pro:streamGenerateContent?alt=sse');
    expect(g.notes.join(' ')).toMatch(/hasn’t been verified against a live Gemini account/);
    expect(buildGuide('curl', { urls, model: 'g' }).blocks[3].code).toContain('x-goog-api-key');
  });
});

describe('kindMismatch', () => {
  it('warns when a client cannot natively speak to the provider', () => {
    expect(kindMismatch('anthropic', 'codex')).toBe(true);
    expect(kindMismatch('anthropic', 'anthropic')).toBe(false);
    expect(kindMismatch('openai', 'anthropic')).toBe(true);
    expect(kindMismatch('openai', 'codex')).toBe(false);
    expect(kindMismatch('any', 'gemini')).toBe(false);
    expect(kindMismatch('gemini', 'gemini')).toBe(false);
    expect(kindMismatch('gemini', 'codex')).toBe(true);
    expect(kindMismatch('openai', null)).toBe(false);
  });
});
