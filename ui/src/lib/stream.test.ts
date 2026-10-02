import { describe, expect, it } from 'vitest';
import { applyEvent, detectFormat, emptyResult, hasContentDelta, parseFinalJson, type StreamResult } from './stream';

const fold = (events: unknown[]) => events.reduce<StreamResult>((s, e) => applyEvent(s, e), emptyResult());

describe('OpenAI Responses stream', () => {
  it('accumulates text, tool calls, usage and completion', () => {
    const s = fold([
      { type: 'response.created', response: { id: 'r1', model: 'gpt-6.1-sol' } },
      { type: 'response.output_text.delta', delta: 'Hel' },
      { type: 'response.output_text.delta', delta: 'lo' },
      { type: 'response.output_item.added', output_index: 1, item: { type: 'function_call', id: 'fc1', call_id: 'call_1', name: 'get_weather', arguments: '' } },
      { type: 'response.function_call_arguments.delta', item_id: 'fc1', delta: '{"city":' },
      { type: 'response.function_call_arguments.delta', item_id: 'fc1', delta: '"Oslo"}' },
      { type: 'response.reasoning_summary_text.delta', delta: 'thinking' },
      { type: 'response.completed', response: { id: 'r1', status: 'completed', usage: { input_tokens: 10, output_tokens: 5 } } },
    ]);
    expect(s.format).toBe('openai-responses');
    expect(s.text).toBe('Hello');
    expect(s.reasoning).toBe('thinking');
    expect(s.toolCalls).toEqual([{ key: 'fc1', id: 'call_1', name: 'get_weather', arguments: '{"city":"Oslo"}' }]);
    expect(s.usage).toEqual({ input: 10, output: 5 });
    expect(s.done).toBe(true);
    expect(s.model).toBe('gpt-6.1-sol');
  });

  it('prefers the final arguments from .done events', () => {
    const s = fold([
      { type: 'response.function_call_arguments.delta', item_id: 'x', delta: '{"a"' },
      { type: 'response.function_call_arguments.done', item_id: 'x', arguments: '{"a":1}', name: 'f' },
    ]);
    expect(s.toolCalls[0]).toMatchObject({ arguments: '{"a":1}', name: 'f' });
  });

  it('surfaces errors from error events, failed and incomplete responses', () => {
    expect(fold([{ type: 'error', code: 'rate_limit', message: 'Slow down' }]).error).toBe('Slow down (rate_limit)');
    expect(fold([{ type: 'error', error: { message: 'Changing models requires a new WebSocket session' } }]).error).toMatch(/new WebSocket/);
    const failed = fold([{ type: 'response.failed', response: { error: { message: 'boom' } } }]);
    expect(failed).toMatchObject({ error: 'boom', done: true });
    expect(fold([{ type: 'response.incomplete', response: { incomplete_details: { reason: 'max_output_tokens' } } }]).stopReason).toBe('incomplete: max_output_tokens');
  });

  it('uses output_text from the completed response when no deltas arrived', () => {
    const s = fold([{ type: 'response.completed', response: { output: [{ type: 'message', content: [{ type: 'output_text', text: 'all at once' }] }] } }]);
    expect(s.text).toBe('all at once');
  });
});

describe('Anthropic Messages stream', () => {
  it('accumulates text, thinking, tool input JSON and usage', () => {
    const s = fold([
      { type: 'message_start', message: { id: 'm1', model: 'claude-opus-5-5', usage: { input_tokens: 12, output_tokens: 1 } } },
      { type: 'content_block_start', index: 0, content_block: { type: 'thinking', thinking: '' } },
      { type: 'content_block_delta', index: 0, delta: { type: 'thinking_delta', thinking: 'hmm' } },
      { type: 'content_block_start', index: 1, content_block: { type: 'text', text: '' } },
      { type: 'content_block_delta', index: 1, delta: { type: 'text_delta', text: 'Hi' } },
      { type: 'content_block_start', index: 2, content_block: { type: 'tool_use', id: 'tu1', name: 'search', input: {} } },
      { type: 'content_block_delta', index: 2, delta: { type: 'input_json_delta', partial_json: '{"q":' } },
      { type: 'content_block_delta', index: 2, delta: { type: 'input_json_delta', partial_json: '"x"}' } },
      { type: 'ping' },
      { type: 'message_delta', delta: { stop_reason: 'tool_use' }, usage: { output_tokens: 40 } },
      { type: 'message_stop' },
    ]);
    expect(s.format).toBe('anthropic-messages');
    expect(s.text).toBe('Hi');
    expect(s.reasoning).toBe('hmm');
    expect(s.toolCalls).toEqual([{ key: 'block-2', id: 'tu1', name: 'search', arguments: '{"q":"x"}' }]);
    expect(s.usage).toEqual({ input: 12, output: 40 });
    expect(s.stopReason).toBe('tool_use');
    expect(s.done).toBe(true);
  });

  it('reports stream errors', () => {
    expect(fold([{ type: 'error', error: { type: 'overloaded_error', message: 'Overloaded' } }]).error).toBe('Overloaded (overloaded_error)');
  });
});

describe('Gemini', () => {
  it('streams text chunks, function calls and usage', () => {
    const s = fold([
      { candidates: [{ content: { parts: [{ text: 'Hel' }] } }], modelVersion: 'gemini-3-pro' },
      { candidates: [{ content: { parts: [{ text: 'thought', thought: true }, { text: 'lo' }] } }] },
      { candidates: [{ content: { parts: [{ functionCall: { name: 'lookup', args: { id: 3 } } }] }, finishReason: 'STOP' }], usageMetadata: { promptTokenCount: 4, candidatesTokenCount: 9 } },
    ]);
    expect(s.format).toBe('gemini');
    expect(s.text).toBe('Hello');
    expect(s.reasoning).toBe('thought');
    expect(s.toolCalls[0]).toMatchObject({ name: 'lookup', arguments: '{"id":3}' });
    expect(s.usage).toEqual({ input: 4, output: 9 });
    expect(s.stopReason).toBe('STOP');
  });

  it('parses non-streaming JSON and chunk arrays, and blocked prompts', () => {
    const one = { candidates: [{ content: { parts: [{ text: 'ok' }] }, finishReason: 'STOP' }], usageMetadata: { promptTokenCount: 1, candidatesTokenCount: 1 } };
    expect(parseFinalJson(one)).toMatchObject({ text: 'ok', done: true, format: 'gemini' });
    expect(parseFinalJson([one, one]).text).toBe('okok');
    expect(fold([{ promptFeedback: { blockReason: 'SAFETY' } }]).error).toBe('Prompt blocked: SAFETY');
  });
});

describe('Chat Completions (defensive)', () => {
  it('handles deltas, tool calls and [DONE]', () => {
    let s = fold([
      { object: 'chat.completion.chunk', choices: [{ delta: { content: 'a' } }] },
      { object: 'chat.completion.chunk', choices: [{ delta: { tool_calls: [{ index: 0, id: 'c1', function: { name: 'f', arguments: '{' } }] } }] },
      { object: 'chat.completion.chunk', choices: [{ delta: { tool_calls: [{ index: 0, function: { arguments: '}' } }] }, finish_reason: 'tool_calls' }], usage: { prompt_tokens: 2, completion_tokens: 3 } },
    ]);
    s = applyEvent(s, '[DONE]');
    expect(s).toMatchObject({ text: 'a', stopReason: 'tool_calls', done: true, usage: { input: 2, output: 3 } });
    expect(s.toolCalls[0]).toMatchObject({ id: 'c1', name: 'f', arguments: '{}' });
  });
});

describe('parseFinalJson', () => {
  it('parses Responses JSON with tool calls and reasoning', () => {
    const r = parseFinalJson({
      object: 'response',
      id: 'r',
      model: 'm',
      status: 'completed',
      output: [
        { type: 'reasoning', summary: [{ type: 'summary_text', text: 'why' }] },
        { type: 'message', content: [{ type: 'output_text', text: 'hi' }] },
        { type: 'function_call', call_id: 'c', name: 'f', arguments: '{}' },
      ],
      usage: { input_tokens: 1, output_tokens: 2 },
    });
    expect(r).toMatchObject({ format: 'openai-responses', text: 'hi', reasoning: 'why', usage: { input: 1, output: 2 } });
    expect(r.toolCalls).toHaveLength(1);
  });

  it('parses Anthropic JSON', () => {
    const r = parseFinalJson({ type: 'message', id: 'm', model: 'claude', content: [{ type: 'text', text: 'yo' }, { type: 'tool_use', id: 't', name: 'n', input: { a: 1 } }], stop_reason: 'end_turn', usage: { input_tokens: 3, output_tokens: 4 } });
    expect(r).toMatchObject({ format: 'anthropic-messages', text: 'yo', stopReason: 'end_turn' });
    expect(r.toolCalls[0].arguments).toBe('{"a":1}');
  });

  it('turns error bodies and unknown shapes into errors', () => {
    expect(parseFinalJson({ error: { message: 'nope', type: 'gateway_error' } }).error).toBe('nope');
    expect(parseFinalJson({ hello: 1 }).error).toMatch(/Unrecognised/);
    expect(parseFinalJson(null).error).toMatch(/non-JSON/);
  });
});

describe('helpers', () => {
  it('detectFormat uses payload type or event name', () => {
    expect(detectFormat({}, 'message_start')).toBe('anthropic-messages');
    expect(detectFormat({ type: 'response.created' })).toBe('openai-responses');
    expect(detectFormat({ nothing: true })).toBe('unknown');
  });
  it('hasContentDelta detects visible output', () => {
    const a = emptyResult();
    expect(hasContentDelta(a, { ...a, text: 'x' })).toBe(true);
    expect(hasContentDelta(a, { ...a, model: 'm' })).toBe(false);
  });
  it('ignores non-object payloads', () => {
    expect(applyEvent(emptyResult(), 42)).toEqual(emptyResult());
  });
});
