import type { Connection, ConnectionInput, ConnectionKind, Route } from './types';

export type ProviderPresetId = 'openai' | 'anthropic' | 'gemini' | 'compatible';

export interface ProviderPreset {
  id: ProviderPresetId;
  kind: ConnectionKind;
  label: string;
  description: string;
  name: string;
  baseUrl: string;
  websocket: boolean;
  suggestions: string[];
  keyHint: string;
}

export const PROVIDER_PRESETS: ProviderPreset[] = [
  {
    id: 'openai',
    kind: 'openai',
    label: 'OpenAI',
    description: 'Platform API key',
    name: 'OpenAI',
    baseUrl: 'https://api.openai.com/v1',
    websocket: true,
    suggestions: ['gpt-6.1-sol', 'gpt-6-astra', 'gpt-6-luna'],
    keyHint: 'sk-…',
  },
  {
    id: 'anthropic',
    kind: 'anthropic',
    label: 'Anthropic',
    description: 'Console API key',
    name: 'Anthropic',
    baseUrl: 'https://api.anthropic.com/v1',
    websocket: false,
    suggestions: ['claude-opus-5-5', 'claude-sonnet-5-5', 'claude-haiku-4-5'],
    keyHint: 'sk-ant-…',
  },
  {
    id: 'gemini',
    kind: 'gemini',
    label: 'Gemini',
    description: 'AI Studio API key',
    name: 'Gemini',
    baseUrl: 'https://generativelanguage.googleapis.com/v1beta',
    websocket: false,
    suggestions: [],
    keyHint: 'AIza…',
  },
  {
    id: 'compatible',
    kind: 'openai',
    label: 'OpenAI-compatible',
    description: 'OpenRouter, Ollama, vLLM…',
    name: 'OpenRouter',
    baseUrl: 'https://openrouter.ai/api/v1',
    websocket: false,
    suggestions: [],
    keyHint: 'Provider API key (leave blank for local servers)',
  },
];

export const COMPATIBLE_ENDPOINTS: { label: string; baseUrl: string; local?: boolean }[] = [
  { label: 'OpenRouter', baseUrl: 'https://openrouter.ai/api/v1' },
  { label: 'Groq', baseUrl: 'https://api.groq.com/openai/v1' },
  { label: 'DeepSeek', baseUrl: 'https://api.deepseek.com/v1' },
  { label: 'Ollama', baseUrl: 'http://127.0.0.1:11434/v1', local: true },
  { label: 'LM Studio', baseUrl: 'http://127.0.0.1:1234/v1', local: true },
  { label: 'vLLM', baseUrl: 'http://127.0.0.1:8000/v1', local: true },
];

export function presetFor(c: Pick<Connection, 'kind' | 'base_url'>): ProviderPresetId {
  if (c.kind === 'anthropic') return 'anthropic';
  if (c.kind === 'gemini') return 'gemini';
  if (c.kind === 'openai' && /^https:\/\/api\.openai\.com(\/|$)/.test(c.base_url)) return 'openai';
  return 'compatible';
}

export function isLoopbackHost(host: string): boolean {
  const h = host.replace(/^\[|\]$/g, '').toLowerCase();
  return h === 'localhost' || h === '::1' || /^127(\.\d{1,3}){3}$/.test(h);
}

/** Remote providers need a key; local servers (Ollama, LM Studio…) usually don't. */
export function missingKeyWarning(c: Pick<Connection, 'credential_present' | 'base_url'>): boolean {
  if (c.credential_present) return false;
  try {
    return !isLoopbackHost(new URL(c.base_url).hostname);
  } catch {
    return true;
  }
}

export type ConnectionErrors = Partial<Record<'name' | 'base_url' | 'models' | 'api_key' | 'supports_websocket', string>>;

/** Mirrors the gateway's own validation so problems show inline before saving. */
export function validateConnection(input: ConnectionInput): ConnectionErrors {
  const e: ConnectionErrors = {};
  const name = input.name.trim();
  if (!name) e.name = 'Give this connection a name.';
  else if (name.length > 100) e.name = 'Keep the name under 100 characters.';

  const raw = input.base_url.trim();
  if (!raw) e.base_url = 'Enter the provider base URL.';
  else {
    let u: URL | null = null;
    try {
      u = new URL(raw);
    } catch {
      e.base_url = 'That isn’t a valid URL. Include https://';
    }
    if (u) {
      if (u.protocol !== 'http:' && u.protocol !== 'https:') e.base_url = 'Use an http:// or https:// URL.';
      else if (u.username || u.password) e.base_url = 'Remove the credentials from the URL; use the API key field.';
      else if (u.search || u.hash) e.base_url = 'Remove the query string or fragment.';
      else if (u.protocol === 'http:' && !isLoopbackHost(u.hostname)) e.base_url = 'Plain HTTP is only allowed for local servers. Use https:// for remote providers.';
    }
  }

  const models = input.models.map((m) => m.trim()).filter(Boolean);
  if (!models.length) e.models = 'Add at least one model this connection serves.';
  else if (models.length > 100) e.models = 'A connection can list at most 100 models.';
  else if (models.some((m) => m.length > 200)) e.models = 'Model identifiers must be under 200 characters.';

  if (input.supports_websocket && input.kind !== 'openai' && input.kind !== 'codex') {
    e.supports_websocket = 'Responses WebSocket mode is only available for OpenAI and Codex connections.';
  }
  if (input.api_key && /[\r\n]/.test(input.api_key)) e.api_key = 'The key contains a line break. Paste it on a single line.';
  return e;
}

/** Normalise user input the way the gateway stores it. */
export function normalizeInput(input: ConnectionInput): ConnectionInput {
  const out: ConnectionInput = {
    ...input,
    name: input.name.trim(),
    base_url: input.base_url.trim().replace(/\/+$/, ''),
    models: [...new Set(input.models.map((m) => m.trim()).filter(Boolean))],
  };
  const key = input.api_key?.trim();
  if (key) out.api_key = key;
  else delete out.api_key; // omitted = keep the stored credential
  return out;
}

/** Routes that reference a connection, for safe delete/disable flows. */
export function routesUsing(routes: Route[] | undefined, connectionId: string): Route[] {
  return (routes ?? []).filter((r) => r.targets.some((t) => t.connection_id === connectionId));
}

/** Routes left with no enabled target if the given connections are disabled/removed. */
export function strandedRoutes(routes: Route[] | undefined, connections: Connection[] | undefined, offIds: string[]): Route[] {
  const enabled = new Set((connections ?? []).filter((c) => c.enabled && !offIds.includes(c.id)).map((c) => c.id));
  return (routes ?? []).filter((r) => r.targets.length > 0 && r.targets.every((t) => !enabled.has(t.connection_id)));
}

/** Disambiguate accounts that share a display name (common with several Codex logins). */
export function displayNames(connections: Connection[]): Map<string, string> {
  const counts = new Map<string, number>();
  for (const c of connections) counts.set(c.name, (counts.get(c.name) ?? 0) + 1);
  const out = new Map<string, string>();
  for (const c of connections) {
    out.set(c.id, (counts.get(c.name) ?? 0) > 1 ? `${c.name} · ${c.id.slice(0, 6)}` : c.name);
  }
  return out;
}

export function hostOf(url: string): string {
  try {
    const u = new URL(url);
    return u.host + (u.pathname !== '/' ? u.pathname : '');
  } catch {
    return url;
  }
}
