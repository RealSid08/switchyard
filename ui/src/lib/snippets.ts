import type { ConnectionKind, GatewayConfig } from './types';

export const KEY_ENV = 'SWITCHYARD_API_KEY';

export interface GatewayUrls {
  /** Scheme + host + port, no trailing slash, e.g. http://127.0.0.1:8317 */
  origin: string;
  /** OpenAI-compatible base, e.g. http://127.0.0.1:8317/v1 */
  openai: string;
  /** Anthropic base (clients append /v1/messages), e.g. http://127.0.0.1:8317 */
  anthropic: string;
  /** Gemini base, e.g. http://127.0.0.1:8317/v1beta */
  gemini: string;
  /** Responses WebSocket endpoint, e.g. ws://127.0.0.1:8317/v1/responses */
  websocket: string;
}

const WILDCARD_HOSTS = new Set(['0.0.0.0', '::', '[::]', '']);

/**
 * Derive client-facing URLs from /api/config. A wildcard bind address (0.0.0.0)
 * isn't something a client can dial, so it's replaced with the host the browser
 * used to reach this UI. Missing config falls back to the UI's own origin.
 */
export function resolveGatewayUrls(
  config: Pick<GatewayConfig, 'api_base' | 'websocket_url' | 'host' | 'port'> | null | undefined,
  loc: Pick<Location, 'protocol' | 'hostname' | 'host' | 'origin'>,
): GatewayUrls {
  let origin = loc.origin;
  if (config?.api_base) {
    try {
      const u = new URL(config.api_base, loc.origin);
      if (WILDCARD_HOSTS.has(u.hostname)) u.hostname = loc.hostname;
      origin = `${u.protocol}//${u.host}`;
      const path = u.pathname.replace(/\/+$/, '').replace(/\/v1$/, '');
      if (path) origin += path;
    } catch {
      // keep loc.origin
    }
  } else if (config?.host && config.port) {
    const host = WILDCARD_HOSTS.has(config.host) ? loc.hostname : config.host;
    origin = `${loc.protocol}//${host.includes(':') && !host.startsWith('[') ? `[${host}]` : host}:${config.port}`;
  }

  let websocket = `${origin.replace(/^http/, 'ws')}/v1/responses`;
  if (config?.websocket_url) {
    try {
      const u = new URL(config.websocket_url, origin.replace(/^http/, 'ws'));
      if (WILDCARD_HOSTS.has(u.hostname)) u.hostname = loc.hostname;
      websocket = u.toString();
    } catch {
      // keep derived
    }
  }
  return { origin, openai: `${origin}/v1`, anthropic: origin, gemini: `${origin}/v1beta`, websocket };
}

/** POSIX shell single-quote. */
export function shellQuote(s: string): string {
  return /^[A-Za-z0-9_./:@%+=,-]+$/.test(s) ? s : `'${s.replace(/'/g, `'"'"'`)}'`;
}

/** JSON/JS/Python/TOML double-quoted string literal. */
const q = (s: string) => JSON.stringify(s);

export type ClientId = 'codex' | 'claude-code' | 'opencode' | 'cursor' | 'curl' | 'openai-sdk' | 'anthropic-sdk' | 'websocket';

export interface SnippetBlock {
  title: string;
  language: 'bash' | 'toml' | 'json' | 'python' | 'typescript' | 'text';
  code: string;
  /** Where this goes, e.g. ~/.codex/config.toml */
  path?: string;
}

export interface ClientGuide {
  id: ClientId;
  label: string;
  blurb: string;
  /** Which upstream API family the client speaks, to warn on mismatched models. */
  speaks: 'openai' | 'anthropic' | 'any';
  blocks: SnippetBlock[];
  notes: string[];
}

export interface SnippetInput {
  urls: GatewayUrls;
  model: string;
  /** Kind of the connection serving `model`, if known. */
  modelKind?: ConnectionKind | null;
  /** Does the selected model support the Responses WebSocket transport? */
  websocket?: boolean;
}

const exportKey = `export ${KEY_ENV}=sy_...   # the client key you created in Switchyard`;

export const CLIENT_ORDER: ClientId[] = ['codex', 'claude-code', 'opencode', 'cursor', 'curl', 'openai-sdk', 'anthropic-sdk', 'websocket'];

export function buildGuide(id: ClientId, input: SnippetInput): ClientGuide {
  const { urls, model } = input;
  switch (id) {
    case 'codex': {
      const toml = [
        `model = ${q(model)}`,
        `model_provider = "switchyard"`,
        '',
        '[model_providers.switchyard]',
        'name = "Switchyard"',
        `base_url = ${q(urls.openai)}`,
        `env_key = ${q(KEY_ENV)}`,
        'wire_api = "responses"',
        ...(input.websocket ? ['# Stream over the Responses WebSocket instead of SSE', 'supports_websockets = true'] : []),
      ].join('\n');
      return {
        id,
        label: 'Codex CLI',
        blurb: 'Point Codex at Switchyard as a custom model provider using the Responses API.',
        speaks: 'openai',
        blocks: [
          { title: 'Export your client key', language: 'bash', code: exportKey },
          { title: 'Add a provider', language: 'toml', code: toml, path: '~/.codex/config.toml' },
          { title: 'Run it', language: 'bash', code: `codex --model ${shellQuote(model)}` },
        ],
        notes: input.websocket
          ? ['This model supports the Responses WebSocket transport, so Codex can keep one socket open per session.']
          : ['This model streams over SSE. WebSocket mode is available only for connections that support it.'],
      };
    }
    case 'claude-code': {
      const env = [
        `export ANTHROPIC_BASE_URL=${shellQuote(urls.anthropic)}`,
        `export ANTHROPIC_AUTH_TOKEN="$${KEY_ENV}"`,
        `export ANTHROPIC_MODEL=${shellQuote(model)}`,
        'claude',
      ].join('\n');
      const settings = JSON.stringify(
        { env: { ANTHROPIC_BASE_URL: urls.anthropic, ANTHROPIC_AUTH_TOKEN: 'sy_...', ANTHROPIC_MODEL: model } },
        null,
        2,
      );
      return {
        id,
        label: 'Claude Code',
        blurb: 'Claude Code speaks the Anthropic Messages API; Switchyard serves it at /v1/messages.',
        speaks: 'anthropic',
        blocks: [
          { title: 'Export your client key', language: 'bash', code: exportKey },
          { title: 'Launch with environment variables', language: 'bash', code: env },
          { title: 'Or persist it in settings', language: 'json', code: settings, path: '~/.claude/settings.json' },
        ],
        notes: ['ANTHROPIC_AUTH_TOKEN is sent as a Bearer token; Switchyard also accepts x-api-key.'],
      };
    }
    case 'opencode': {
      const config = JSON.stringify(
        {
          $schema: 'https://opencode.ai/config.json',
          provider: {
            switchyard: {
              npm: '@ai-sdk/openai-compatible',
              name: 'Switchyard',
              options: { baseURL: urls.openai, apiKey: `{env:${KEY_ENV}}` },
              models: { [model]: { name: model } },
            },
          },
          model: `switchyard/${model}`,
        },
        null,
        2,
      );
      return {
        id,
        label: 'OpenCode',
        blurb: 'Register Switchyard as an OpenAI-compatible provider.',
        speaks: 'openai',
        blocks: [
          { title: 'Export your client key', language: 'bash', code: exportKey },
          { title: 'Add the provider', language: 'json', code: config, path: 'opencode.json (project) or ~/.config/opencode/opencode.json' },
        ],
        notes: ['Add more entries under "models" for every route you want to pick from inside OpenCode.'],
      };
    }
    case 'cursor': {
      const env = [`export OPENAI_BASE_URL=${shellQuote(urls.openai)}`, `export OPENAI_API_KEY="$${KEY_ENV}"`].join('\n');
      const fields = [`Base URL   ${urls.openai}`, `API key    sy_...  (your Switchyard client key)`, `Model      ${model}`].join('\n');
      return {
        id,
        label: 'Cursor & others',
        blurb: 'Anything with an "OpenAI base URL" setting works: Cursor, Continue, Cline, Aider, Zed and more.',
        speaks: 'openai',
        blocks: [
          { title: 'Settings fields', language: 'text', code: fields },
          { title: 'Tools that read OpenAI environment variables', language: 'bash', code: env },
        ],
        notes: [
          'Cursor sends custom-model traffic through its own servers, so it cannot reach a gateway on 127.0.0.1. Expose Switchyard on a reachable address (for example over Tailscale) first.',
          'Editors that call the API from your machine (Continue, Cline, Aider, Zed) work with the local address as-is.',
        ],
      };
    }
    case 'curl': {
      const chat = [
        `curl ${shellQuote(`${urls.openai}/chat/completions`)} \\`,
        `  -H "Authorization: Bearer $${KEY_ENV}" \\`,
        '  -H "Content-Type: application/json" \\',
        `  -d ${shellQuote(JSON.stringify({ model, messages: [{ role: 'user', content: 'Say hello from Switchyard' }] }))}`,
      ].join('\n');
      const responses = [
        `curl -N ${shellQuote(`${urls.openai}/responses`)} \\`,
        `  -H "Authorization: Bearer $${KEY_ENV}" \\`,
        '  -H "Content-Type: application/json" \\',
        `  -d ${shellQuote(JSON.stringify({ model, input: 'Write a haiku about rail yards', stream: true }))}`,
      ].join('\n');
      const messages = [
        `curl ${shellQuote(`${urls.anthropic}/v1/messages`)} \\`,
        `  -H "x-api-key: $${KEY_ENV}" \\`,
        '  -H "anthropic-version: 2023-06-01" \\',
        '  -H "Content-Type: application/json" \\',
        `  -d ${shellQuote(JSON.stringify({ model, max_tokens: 256, messages: [{ role: 'user', content: 'Hello' }] }))}`,
      ].join('\n');
      const gemini = [
        `curl ${shellQuote(`${urls.gemini}/models/${encodeURIComponent(model)}:generateContent`)} \\`,
        `  -H "x-api-key: $${KEY_ENV}" \\`,
        '  -H "Content-Type: application/json" \\',
        `  -d ${shellQuote(JSON.stringify({ contents: [{ role: 'user', parts: [{ text: 'Hello' }] }] }))}`,
      ].join('\n');
      return {
        id,
        label: 'curl',
        blurb: 'Smoke-test every API surface from a terminal.',
        speaks: 'any',
        blocks: [
          { title: 'Chat Completions', language: 'bash', code: chat },
          { title: 'Responses, streamed over SSE', language: 'bash', code: responses },
          { title: 'Anthropic Messages', language: 'bash', code: messages },
          { title: 'Gemini generateContent', language: 'bash', code: gemini },
        ],
        notes: ['Client keys work as `Authorization: Bearer` or `x-api-key`.'],
      };
    }
    case 'openai-sdk': {
      const py = [
        'import os',
        'from openai import OpenAI',
        '',
        `client = OpenAI(base_url=${q(urls.openai)}, api_key=os.environ[${q(KEY_ENV)}])`,
        '',
        'stream = client.responses.create(',
        `    model=${q(model)},`,
        '    input="Explain backpressure in one paragraph.",',
        '    stream=True,',
        ')',
        'for event in stream:',
        '    if event.type == "response.output_text.delta":',
        '        print(event.delta, end="", flush=True)',
      ].join('\n');
      const ts = [
        "import OpenAI from 'openai';",
        '',
        `const client = new OpenAI({ baseURL: ${q(urls.openai)}, apiKey: process.env.${KEY_ENV} });`,
        '',
        'const stream = await client.responses.create({',
        `  model: ${q(model)},`,
        "  input: 'Explain backpressure in one paragraph.',",
        '  stream: true,',
        '});',
        'for await (const event of stream) {',
        "  if (event.type === 'response.output_text.delta') process.stdout.write(event.delta);",
        '}',
      ].join('\n');
      return {
        id,
        label: 'OpenAI SDK',
        blurb: 'Use the official SDKs unchanged; only the base URL and key differ.',
        speaks: 'openai',
        blocks: [
          { title: 'Python', language: 'python', code: py },
          { title: 'TypeScript', language: 'typescript', code: ts },
        ],
        notes: ['client.chat.completions works too, at the same base URL.'],
      };
    }
    case 'anthropic-sdk': {
      const py = [
        'import os',
        'from anthropic import Anthropic',
        '',
        `client = Anthropic(base_url=${q(urls.anthropic)}, api_key=os.environ[${q(KEY_ENV)}])`,
        '',
        'with client.messages.stream(',
        `    model=${q(model)},`,
        '    max_tokens=1024,',
        '    messages=[{"role": "user", "content": "Explain backpressure in one paragraph."}],',
        ') as stream:',
        '    for text in stream.text_stream:',
        '        print(text, end="", flush=True)',
      ].join('\n');
      const ts = [
        "import Anthropic from '@anthropic-ai/sdk';",
        '',
        `const client = new Anthropic({ baseURL: ${q(urls.anthropic)}, apiKey: process.env.${KEY_ENV} });`,
        '',
        'const stream = client.messages.stream({',
        `  model: ${q(model)},`,
        '  max_tokens: 1024,',
        "  messages: [{ role: 'user', content: 'Explain backpressure in one paragraph.' }],",
        '});',
        "stream.on('text', (text) => process.stdout.write(text));",
        'await stream.finalMessage();',
      ].join('\n');
      return {
        id,
        label: 'Anthropic SDK',
        blurb: 'Point the Anthropic SDK at Switchyard; the SDK appends /v1/messages.',
        speaks: 'anthropic',
        blocks: [
          { title: 'Python', language: 'python', code: py },
          { title: 'TypeScript', language: 'typescript', code: ts },
        ],
        notes: [],
      };
    }
    case 'websocket': {
      const node = [
        "import WebSocket from 'ws';",
        '',
        `const ws = new WebSocket(${q(urls.websocket)}, {`,
        `  headers: { Authorization: \`Bearer \${process.env.${KEY_ENV}}\` },`,
        '});',
        '',
        "ws.on('open', () => {",
        '  ws.send(JSON.stringify({',
        "    type: 'response.create',",
        `    response: { model: ${q(model)}, input: 'Hello over a WebSocket', stream: true },`,
        '  }));',
        '});',
        "ws.on('message', (raw) => {",
        '  const event = JSON.parse(raw.toString());',
        "  if (event.type === 'response.output_text.delta') process.stdout.write(event.delta);",
        "  if (event.type === 'response.completed') ws.close();",
        '});',
      ].join('\n');
      const websocat = [
        `websocat -H "Authorization: Bearer $${KEY_ENV}" ${shellQuote(urls.websocket)}`,
        `# then paste:`,
        JSON.stringify({ type: 'response.create', response: { model, input: 'Hello', stream: true } }),
      ].join('\n');
      return {
        id,
        label: 'WebSocket',
        blurb: 'OpenAI Responses WebSocket mode: one long-lived socket, many responses, no per-request handshake.',
        speaks: 'openai',
        blocks: [
          { title: 'Node.js (ws)', language: 'typescript', code: node },
          { title: 'websocat', language: 'bash', code: websocat },
        ],
        notes: input.websocket
          ? ['The selected model is served by a connection that supports WebSockets.']
          : ['The selected model is not served by a WebSocket-capable connection. Pick a model marked WS, or the gateway will reject the socket.'],
      };
    }
  }
}

/** Should we warn that the chosen model's provider doesn't natively match the client? */
export function kindMismatch(speaks: ClientGuide['speaks'], kind: ConnectionKind | null | undefined): boolean {
  if (!kind || speaks === 'any') return false;
  if (speaks === 'anthropic') return kind !== 'anthropic';
  return kind === 'anthropic' || kind === 'gemini';
}
