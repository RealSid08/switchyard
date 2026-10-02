/**
 * Accumulates model output from any of the wire formats the gateway can return:
 * OpenAI Responses (SSE, WebSocket frames and JSON), Anthropic Messages (SSE and
 * JSON), Gemini generateContent (SSE and JSON) and, defensively, OpenAI Chat Completions.
 */

export type WireFormat = 'openai-responses' | 'anthropic-messages' | 'gemini' | 'openai-chat' | 'unknown';

export interface ToolCall {
  key: string;
  id: string;
  name: string;
  arguments: string;
}

export interface StreamResult {
  format: WireFormat;
  text: string;
  reasoning: string;
  toolCalls: ToolCall[];
  usage: { input: number | null; output: number | null };
  model: string | null;
  responseId: string | null;
  stopReason: string | null;
  error: string | null;
  done: boolean;
}

export const FORMAT_LABELS: Record<WireFormat, string> = {
  'openai-responses': 'OpenAI Responses',
  'anthropic-messages': 'Anthropic Messages',
  gemini: 'Gemini generateContent',
  'openai-chat': 'OpenAI Chat Completions',
  unknown: 'Unknown format',
};

export function emptyResult(): StreamResult {
  return {
    format: 'unknown',
    text: '',
    reasoning: '',
    toolCalls: [],
    usage: { input: null, output: null },
    model: null,
    responseId: null,
    stopReason: null,
    error: null,
    done: false,
  };
}

const ANTHROPIC_EVENTS = new Set([
  'message_start',
  'content_block_start',
  'content_block_delta',
  'content_block_stop',
  'message_delta',
  'message_stop',
  'ping',
]);

type Json = Record<string, unknown>;
const isObj = (v: unknown): v is Json => !!v && typeof v === 'object' && !Array.isArray(v);
const str = (v: unknown): string | null => (typeof v === 'string' ? v : null);
const num = (v: unknown): number | null => (typeof v === 'number' && Number.isFinite(v) ? v : null);

/** Best-effort format detection from a content-type and/or a payload. */
export function detectFormat(payload: unknown, eventName?: string): WireFormat {
  const type = (isObj(payload) && str(payload.type)) || eventName || '';
  if (type.startsWith('response.') || (isObj(payload) && payload.object === 'response')) return 'openai-responses';
  if (ANTHROPIC_EVENTS.has(type) || (isObj(payload) && payload.type === 'message' && Array.isArray(payload.content)))
    return 'anthropic-messages';
  if (isObj(payload) && (Array.isArray(payload.candidates) || isObj(payload.usageMetadata) || isObj(payload.promptFeedback)))
    return 'gemini';
  if (isObj(payload) && (Array.isArray(payload.choices) || payload.object === 'chat.completion' || payload.object === 'chat.completion.chunk'))
    return 'openai-chat';
  return 'unknown';
}

function errorText(v: unknown): string | null {
  if (typeof v === 'string') return v;
  if (isObj(v)) {
    const m = str(v.message) ?? (isObj(v.error) ? str(v.error.message) : null);
    const code = str(v.code) ?? str(v.type) ?? (isObj(v.error) ? str(v.error.type) : null);
    if (m) return code && code !== 'error' && code !== 'gateway_error' && !m.includes(code) ? `${m} (${code})` : m;
    if (code) return code;
  }
  return null;
}

function upsertTool(s: StreamResult, key: string, patch: Partial<ToolCall>, appendArgs?: string): StreamResult {
  const idx = s.toolCalls.findIndex((t) => t.key === key);
  const calls = s.toolCalls.slice();
  if (idx === -1) {
    calls.push({ key, id: patch.id ?? key, name: patch.name ?? 'tool', arguments: (patch.arguments ?? '') + (appendArgs ?? '') });
  } else {
    const prev = calls[idx];
    calls[idx] = {
      ...prev,
      id: patch.id || prev.id,
      name: patch.name || prev.name,
      arguments: patch.arguments !== undefined ? patch.arguments : prev.arguments + (appendArgs ?? ''),
    };
  }
  return { ...s, toolCalls: calls };
}

function withFormat(s: StreamResult, f: WireFormat): StreamResult {
  return s.format === 'unknown' && f !== 'unknown' ? { ...s, format: f } : s;
}

/** Apply one streamed event (an SSE `data` payload or a WebSocket frame). */
export function applyEvent(state: StreamResult, payload: unknown, eventName?: string): StreamResult {
  if (payload === '[DONE]') return { ...state, done: true };
  if (!isObj(payload)) return state;
  const type = str(payload.type) ?? eventName ?? '';
  const format = detectFormat(payload, eventName);
  let s = withFormat(state, format);

  if (type === 'error') {
    return { ...s, error: errorText(payload.error ?? payload) ?? 'The provider returned an error.', done: true };
  }

  if (format === 'openai-responses') {
    const response = isObj(payload.response) ? payload.response : null;
    if (response) {
      s = { ...s, model: str(response.model) ?? s.model, responseId: str(response.id) ?? s.responseId };
      if (isObj(response.usage)) {
        s = { ...s, usage: { input: num(response.usage.input_tokens), output: num(response.usage.output_tokens) } };
      }
    }
    switch (type) {
      case 'response.output_text.delta':
      case 'response.refusal.delta':
        return { ...s, text: s.text + (str(payload.delta) ?? '') };
      case 'response.reasoning_summary_text.delta':
      case 'response.reasoning_text.delta':
        return { ...s, reasoning: s.reasoning + (str(payload.delta) ?? '') };
      case 'response.output_item.added':
      case 'response.output_item.done': {
        const item = isObj(payload.item) ? payload.item : null;
        if (item?.type === 'function_call' || item?.type === 'custom_tool_call') {
          const key = str(item.id) ?? String(payload.output_index ?? s.toolCalls.length);
          const args = str(item.arguments) ?? str(item.input) ?? undefined;
          return upsertTool(s, key, {
            id: str(item.call_id) ?? key,
            name: str(item.name) ?? undefined,
            arguments: type === 'response.output_item.done' || args ? args : undefined,
          });
        }
        return s;
      }
      case 'response.function_call_arguments.delta':
      case 'response.custom_tool_call_input.delta': {
        const key = str(payload.item_id) ?? String(payload.output_index ?? 0);
        return upsertTool(s, key, {}, str(payload.delta) ?? '');
      }
      case 'response.function_call_arguments.done': {
        const key = str(payload.item_id) ?? String(payload.output_index ?? 0);
        return upsertTool(s, key, { arguments: str(payload.arguments) ?? undefined, name: str(payload.name) ?? undefined });
      }
      case 'response.completed':
        return { ...s, done: true, stopReason: str(response?.status) ?? 'completed', text: s.text || outputText(response) };
      case 'response.incomplete': {
        const details = response && isObj(response.incomplete_details) ? str(response.incomplete_details.reason) : null;
        return { ...s, done: true, stopReason: details ? `incomplete: ${details}` : 'incomplete' };
      }
      case 'response.failed':
        return { ...s, done: true, stopReason: 'failed', error: errorText(response?.error) ?? 'The response failed.' };
      default:
        return s;
    }
  }

  if (format === 'anthropic-messages') {
    switch (type) {
      case 'message_start': {
        const m = isObj(payload.message) ? payload.message : {};
        const u = isObj(m.usage) ? m.usage : {};
        return {
          ...s,
          model: str(m.model) ?? s.model,
          responseId: str(m.id) ?? s.responseId,
          usage: { input: num(u.input_tokens) ?? s.usage.input, output: num(u.output_tokens) ?? s.usage.output },
        };
      }
      case 'content_block_start': {
        const block = isObj(payload.content_block) ? payload.content_block : {};
        const key = `block-${String(payload.index ?? 0)}`;
        if (block.type === 'tool_use' || block.type === 'server_tool_use') {
          const input = isObj(block.input) && Object.keys(block.input).length ? JSON.stringify(block.input) : '';
          return upsertTool(s, key, { id: str(block.id) ?? key, name: str(block.name) ?? 'tool', arguments: input });
        }
        if (block.type === 'text' && str(block.text)) return { ...s, text: s.text + str(block.text) };
        return s;
      }
      case 'content_block_delta': {
        const d = isObj(payload.delta) ? payload.delta : {};
        if (d.type === 'text_delta') return { ...s, text: s.text + (str(d.text) ?? '') };
        if (d.type === 'thinking_delta') return { ...s, reasoning: s.reasoning + (str(d.thinking) ?? '') };
        if (d.type === 'input_json_delta') {
          return upsertTool(s, `block-${String(payload.index ?? 0)}`, {}, str(d.partial_json) ?? '');
        }
        return s;
      }
      case 'message_delta': {
        const d = isObj(payload.delta) ? payload.delta : {};
        const u = isObj(payload.usage) ? payload.usage : {};
        return {
          ...s,
          stopReason: str(d.stop_reason) ?? s.stopReason,
          usage: { input: num(u.input_tokens) ?? s.usage.input, output: num(u.output_tokens) ?? s.usage.output },
        };
      }
      case 'message_stop':
        return { ...s, done: true };
      default:
        return s;
    }
  }

  if (format === 'gemini') return applyGemini(s, payload);

  if (format === 'openai-chat') {
    const choice = Array.isArray(payload.choices) && isObj(payload.choices[0]) ? payload.choices[0] : null;
    if (isObj(payload.usage)) {
      s = { ...s, usage: { input: num(payload.usage.prompt_tokens), output: num(payload.usage.completion_tokens) } };
    }
    s = { ...s, model: str(payload.model) ?? s.model, responseId: str(payload.id) ?? s.responseId };
    if (!choice) return s;
    const delta = isObj(choice.delta) ? choice.delta : isObj(choice.message) ? choice.message : {};
    if (str(delta.content)) s = { ...s, text: s.text + str(delta.content) };
    if (str(delta.reasoning_content)) s = { ...s, reasoning: s.reasoning + str(delta.reasoning_content) };
    if (Array.isArray(delta.tool_calls)) {
      for (const tc of delta.tool_calls) {
        if (!isObj(tc)) continue;
        const fn = isObj(tc.function) ? tc.function : {};
        s = upsertTool(s, `tc-${String(tc.index ?? 0)}`, { id: str(tc.id) ?? undefined, name: str(fn.name) ?? undefined }, str(fn.arguments) ?? '');
      }
    }
    if (str(choice.finish_reason)) s = { ...s, stopReason: str(choice.finish_reason) };
    return s;
  }

  return s;
}

function applyGemini(state: StreamResult, payload: Json): StreamResult {
  let s = state;
  const u = isObj(payload.usageMetadata) ? payload.usageMetadata : null;
  if (u) {
    s = { ...s, usage: { input: num(u.promptTokenCount) ?? s.usage.input, output: num(u.candidatesTokenCount) ?? s.usage.output } };
  }
  s = { ...s, model: str(payload.modelVersion) ?? s.model, responseId: str(payload.responseId) ?? s.responseId };
  const block = isObj(payload.promptFeedback) ? str(payload.promptFeedback.blockReason) : null;
  if (block) return { ...s, done: true, error: `Prompt blocked: ${block}` };
  const cand = Array.isArray(payload.candidates) && isObj(payload.candidates[0]) ? payload.candidates[0] : null;
  if (!cand) return s;
  const parts = isObj(cand.content) && Array.isArray(cand.content.parts) ? cand.content.parts : [];
  parts.forEach((part, i) => {
    if (!isObj(part)) return;
    if (typeof part.text === 'string') {
      s = part.thought === true ? { ...s, reasoning: s.reasoning + part.text } : { ...s, text: s.text + part.text };
    }
    if (isObj(part.functionCall)) {
      const name = str(part.functionCall.name) ?? 'tool';
      const key = `fn-${s.toolCalls.length}-${i}`;
      s = upsertTool(s, key, { id: str(part.functionCall.id) ?? key, name, arguments: JSON.stringify(part.functionCall.args ?? {}) });
    }
  });
  const finish = str(cand.finishReason);
  if (finish) s = { ...s, stopReason: finish };
  return s;
}

function outputText(response: Json | null): string {
  if (!response) return '';
  if (typeof response.output_text === 'string') return response.output_text;
  if (!Array.isArray(response.output)) return '';
  let text = '';
  for (const item of response.output) {
    if (!isObj(item) || item.type !== 'message' || !Array.isArray(item.content)) continue;
    for (const part of item.content) {
      if (isObj(part) && (part.type === 'output_text' || part.type === 'text') && typeof part.text === 'string') text += part.text;
    }
  }
  return text;
}

/** Parse a complete (non-streaming) JSON response body in any supported format. */
export function parseFinalJson(body: unknown): StreamResult {
  const s = emptyResult();
  // Gemini streamGenerateContent without ?alt=sse returns a JSON array of chunks.
  if (Array.isArray(body) && body.length && body.every(isObj)) {
    return { ...body.reduce<StreamResult>((acc, chunk) => applyEvent(acc, chunk), s), done: true };
  }
  if (!isObj(body)) return { ...s, done: true, error: 'The gateway returned an empty or non-JSON response.' };
  if (isObj(body.error) || typeof body.error === 'string') {
    return { ...s, done: true, error: errorText(body.error) ?? 'The provider returned an error.' };
  }
  const format = detectFormat(body);
  if (format === 'openai-responses') {
    const usage = isObj(body.usage) ? body.usage : {};
    const tools: ToolCall[] = [];
    if (Array.isArray(body.output)) {
      for (const item of body.output) {
        if (isObj(item) && (item.type === 'function_call' || item.type === 'custom_tool_call')) {
          const id = str(item.call_id) ?? str(item.id) ?? `call-${tools.length}`;
          tools.push({ key: id, id, name: str(item.name) ?? 'tool', arguments: str(item.arguments) ?? str(item.input) ?? '' });
        }
      }
    }
    let reasoning = '';
    if (Array.isArray(body.output)) {
      for (const item of body.output) {
        if (isObj(item) && item.type === 'reasoning' && Array.isArray(item.summary)) {
          for (const p of item.summary) if (isObj(p) && typeof p.text === 'string') reasoning += p.text;
        }
      }
    }
    return {
      ...s,
      format,
      done: true,
      text: outputText(body),
      reasoning,
      toolCalls: tools,
      model: str(body.model),
      responseId: str(body.id),
      stopReason: str(body.status),
      usage: { input: num(usage.input_tokens), output: num(usage.output_tokens) },
      error: body.status === 'failed' ? (errorText(body.error) ?? 'The response failed.') : null,
    };
  }
  if (format === 'anthropic-messages') {
    const usage = isObj(body.usage) ? body.usage : {};
    let text = '';
    let reasoning = '';
    const tools: ToolCall[] = [];
    for (const block of body.content as unknown[]) {
      if (!isObj(block)) continue;
      if (block.type === 'text' && typeof block.text === 'string') text += block.text;
      if (block.type === 'thinking' && typeof block.thinking === 'string') reasoning += block.thinking;
      if (block.type === 'tool_use') {
        const id = str(block.id) ?? `tool-${tools.length}`;
        tools.push({ key: id, id, name: str(block.name) ?? 'tool', arguments: JSON.stringify(block.input ?? {}) });
      }
    }
    return {
      ...s,
      format,
      done: true,
      text,
      reasoning,
      toolCalls: tools,
      model: str(body.model),
      responseId: str(body.id),
      stopReason: str(body.stop_reason),
      usage: { input: num(usage.input_tokens), output: num(usage.output_tokens) },
    };
  }
  if (format === 'openai-chat' || format === 'gemini') {
    return { ...applyEvent(s, body), done: true };
  }
  return { ...s, done: true, error: 'Unrecognised response format. See the raw inspector.' };
}

/** Does this event carry user-visible model output? Used for time-to-first-token. */
export function hasContentDelta(before: StreamResult, after: StreamResult): boolean {
  return (
    after.text.length > before.text.length ||
    after.reasoning.length > before.reasoning.length ||
    after.toolCalls.some((t, i) => !before.toolCalls[i] || t.arguments.length > before.toolCalls[i].arguments.length)
  );
}
