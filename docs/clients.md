# Connect clients

Create a client key under **Keys** in the control room and keep it in an environment variable such as `SWITCHYARD_API_KEY`. The key is shown once. Use one key per tool so you can revoke them separately. Never give a client the admin token.

Replace model names below with identifiers configured on your connections or with your route aliases. `GET /v1/models` lists both.

| Client protocol | Base URL | Key header |
| --- | --- | --- |
| OpenAI (Responses, Chat Completions) | `http://127.0.0.1:7410/v1` | `Authorization: Bearer` |
| Anthropic Messages | `http://127.0.0.1:7410` | `x-api-key` or `Authorization: Bearer` |
| Gemini API | `http://127.0.0.1:7410` (API version `v1beta`) | `x-goog-api-key` or `Authorization: Bearer` |
| Responses WebSocket | `ws://127.0.0.1:7410/v1/responses` | `Authorization: Bearer` on the handshake |

Keys in URL query strings are not accepted.

## OpenAI SDK

```python
from openai import OpenAI
import os

client = OpenAI(base_url="http://127.0.0.1:7410/v1", api_key=os.environ["SWITCHYARD_API_KEY"])

stream = client.responses.create(model="gpt-6.1-sol", input="Hello", stream=True)
for event in stream:
    if event.type == "response.output_text.delta":
        print(event.delta, end="", flush=True)

# Chat Completions works for OpenAI-compatible and Codex connections.
reply = client.chat.completions.create(
    model="gpt-6.1-sol", messages=[{"role": "user", "content": "Hello"}]
)
print(reply.choices[0].message.content)
```

To continue a conversation, pass `previous_response_id`. Switchyard sends it to the account that produced that response.

## Anthropic SDK

```python
from anthropic import Anthropic
import os

client = Anthropic(base_url="http://127.0.0.1:7410", api_key=os.environ["SWITCHYARD_API_KEY"])
with client.messages.stream(
    model="claude-opus-5-5", max_tokens=1024, messages=[{"role": "user", "content": "Hello"}]
) as stream:
    for text in stream.text_stream:
        print(text, end="", flush=True)
```

`POST /v1/messages` and `POST /v1/messages/count_tokens` are proxied natively. Batch endpoints return 404.

## Google Gen AI SDK

```python
from google import genai
from google.genai import types
import os

client = genai.Client(
    api_key=os.environ["SWITCHYARD_API_KEY"],
    http_options=types.HttpOptions(base_url="http://127.0.0.1:7410", api_version="v1beta"),
)
for chunk in client.models.generate_content_stream(model="gemini-3-pro", contents="Hello"):
    print(chunk.text, end="", flush=True)
```

Or with curl:

```sh
curl "http://127.0.0.1:7410/v1beta/models/gemini-3-pro:streamGenerateContent?alt=sse" \
  -H "x-goog-api-key: $SWITCHYARD_API_KEY" -H 'Content-Type: application/json' \
  -d '{"contents":[{"role":"user","parts":[{"text":"Hello"}]}]}'
```

## Responses WebSocket

```js
import WebSocket from 'ws';

const socket = new WebSocket('ws://127.0.0.1:7410/v1/responses', {
  headers: { Authorization: `Bearer ${process.env.SWITCHYARD_API_KEY}` },
});
socket.on('open', () =>
  socket.send(JSON.stringify({ type: 'response.create', model: 'gpt-6.1-sol', input: 'Hello' })),
);
socket.on('message', (data) => {
  const event = JSON.parse(data.toString());
  if (event.type === 'response.output_text.delta') process.stdout.write(event.delta);
  if (event.type === 'response.completed') {
    // Next turn on the same socket: same account, same model.
    // socket.send(JSON.stringify({ type: 'response.create', model: 'gpt-6.1-sol',
    //   previous_response_id: event.response.id, input: 'And then?' }));
  }
});
```

Keep the socket open for later turns, including tool results. Send the first `response.create` within 30 seconds. Fields may be top-level or nested under `response`. Changing the model needs a new socket. Revoking a key stops new handshakes; a socket that is already open continues until it closes or goes idle.

## Coding agents

**Codex CLI.** Add a provider in `~/.codex/config.toml`:

```toml
model_provider = "switchyard"
model = "gpt-6.1-sol"

[model_providers.switchyard]
name = "Switchyard"
base_url = "http://127.0.0.1:7410/v1"
env_key = "SWITCHYARD_API_KEY"
wire_api = "responses"
```

**Claude Code.** Set `ANTHROPIC_BASE_URL=http://127.0.0.1:7410` and `ANTHROPIC_API_KEY=$SWITCHYARD_API_KEY`, and choose a model that a Claude connection or route provides.

## OpenCode

Add a custom provider to your OpenCode configuration:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "switchyard": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "Switchyard",
      "options": {
        "baseURL": "http://127.0.0.1:7410/v1",
        "apiKey": "{env:SWITCHYARD_API_KEY}"
      },
      "models": { "deepseek-v4-flash": { "name": "DeepSeek via Switchyard" } }
    }
  },
  "model": "switchyard/deepseek-v4-flash"
}
```

Choose a model served by your connection or route. For native Responses semantics use the OpenAI SDK provider; for Messages use the Anthropic SDK provider and its base URL. Importing Zen/Go credentials into Switchyard and configuring OpenCode as a gateway client are separate steps. See [providers](providers.md#opencode-zen-and-go).

## Cursor

Under Models, enable your OpenAI API key and use a Switchyard client key. Enable **Override OpenAI Base URL** and enter the gateway's reachable HTTPS `/v1` URL. Add a compatible model served by that connection or route.

Cursor's servers send BYOK requests. A loopback address on your machine is unreachable from those servers. Configure a secured externally reachable HTTPS endpoint before using the override. This integration applies to supported chat models; it does not replace Cursor Tab or expose Composer as a Switchyard provider. Native Cursor end-to-end verification remains pending account and endpoint access.

## T3 Code

Use a provider adapter that accepts a custom API base URL. Do not change T3 Code's existing account or runtime settings to point at Switchyard.

These client settings come from each tool's documentation and can change between versions; check the tool's current docs if a setting is rejected.

## Remote clients

If Switchyard runs on another machine, put it behind TLS (or a private network such as Tailscale), set `SWITCHYARD_PUBLIC_ORIGIN` so the Clients screen shows the right URLs, and use `https://` and `wss://` base URLs. See [SECURITY.md](../SECURITY.md).

## Verification

Codex CLI 0.160.0 was exercised against Switchyard with an isolated `CODEX_HOME`, a client key, and the custom Responses provider above. Live Codex subscription requests passed HTTP, SSE, Chat translation, function calls, and two WebSocket turns including a continuation. Claude Code subscription imports passed native Messages JSON, SSE, and automatic tool selection with Opus 5.5. Claude Code CLI was also exercised through the gateway, with isolated configuration, native streaming, and tool use. Native auth stores were not modified. The OpenAI Python SDK stream and a two-turn Responses WebSocket continuation were also verified on v0.2. OpenCode 2.0.21 completed an isolated read-tool request through Switchyard to Go. The v0.2 Claude SDK probe returned the account's current rate limit; its live recheck awaits reset. Gemini SDK live access and Cursor/Antigravity live access remain unverified.
