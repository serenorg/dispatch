# Provider Plugin Protocol

**Status:** draft for Dispatch v0.4.0. Subject to change until the first reference implementation lands.

Dispatch provider plugins are external executables that supply LLM inference to Dispatch couriers. They sit at the same layer as the in-process model backends that ship with `dispatch-core` (OpenAI, Anthropic, etc.) and let couriers reach models served by third-party or self-hosted endpoints without linking those backends into the Dispatch binary.

Dispatch uses JSON-RPC 2.0 messages framed as line-delimited JSON over stdio, matching the courier and channel plugin protocols. Inference itself is stream-first.

## Scope

Provider plugins answer "generate a completion for these messages on this model." They are stateless from Dispatch's perspective by default; any prompt caching, continuation, or per-session state is the provider's responsibility.

A provider plugin implements:

- `capabilities` - declare supported models, modalities, features
- `configure` - validate credentials and endpoint
- `health` - verify connectivity and auth
- `complete` - one-shot non-streaming completion
- `stream` - streaming completion with event notifications
- `cancel` - best-effort cancellation of an in-flight `stream`
- `shutdown` - allow a persistent process to exit cleanly

Providers are not couriers. They do not receive parcel directories, session state, or tool-execution responsibilities. A courier that delegates inference to a provider is responsible for translating its own run operation into `complete` or `stream` calls.

## Transport

JSON-RPC 2.0 over stdio, framed as newline-delimited JSON.

- Dispatch writes one JSON request line at a time to plugin stdin.
- The plugin writes one JSON-RPC message per line to stdout.
- stderr is reserved for human-readable diagnostics and logs.

Dispatch does not currently use JSON-RPC batch requests. The host keeps at most one request in flight per plugin stream and expects each terminal response to echo the request `id`.

Dispatch may keep a provider plugin process alive across multiple requests. Providers should not assume a fresh process per call.

## Plugin Manifest

Provider plugins declare themselves in `provider-plugin.json`:

```json
{
  "kind": "provider",
  "name": "seren-models",
  "version": "0.1.0",
  "protocol_version": 1,
  "transport": "jsonl",
  "description": "Seren Models provider for Dispatch inference.",
  "exec": {
    "command": "./target/release/seren-models",
    "args": []
  }
}
```

Dispatch supports protocol version `1`. The `kind` field is required and must be `"provider"` when present.

## Requests

Every host call is sent as a JSON-RPC request. The `method` identifies the provider
operation and `params` contains the typed Dispatch request payload.

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "provider.capabilities",
  "params": {
    "protocol_version": 1,
    "kind": "capabilities"
  }
}
```

Provider request methods:

- `provider.capabilities`
- `provider.configure`
- `provider.health`
- `provider.complete`
- `provider.stream`
- `provider.cancel`
- `provider.shutdown`

## Capabilities

`provider.capabilities` returns the provider's feature declaration. Dispatch uses this to decide which couriers can route which models through this provider.

```json
{
  "kind": "capabilities",
  "capabilities": {
    "provider_id": "seren-models",
    "protocol_version": 1,
    "models": [
      {
        "id": "seren-default",
        "display_name": "Seren Default",
        "context_window": 200000,
        "max_output_tokens": 8192,
        "modalities": { "input": ["text"], "output": ["text"] }
      }
    ],
    "supports_streaming": true,
    "supports_tool_use": true,
    "supports_system_prompt": true,
    "supports_vision": false,
    "supports_prompt_caching": false
  }
}
```

Providers may list models statically in capabilities, or return a wildcard model entry (`"id": "*"`) and resolve specific model IDs at `complete`/`stream` time. Couriers that match a model against a provider use whichever model IDs capabilities declared.

## Configuration

`provider.configure` validates credentials, endpoint URLs, and optional defaults. Config values are passed through from Dispatch's runtime config or from the invoking courier.

```json
{
  "kind": "configure",
  "config": {
    "base_url": "https://api.serendb.com/publishers/seren-models",
    "api_key": "seren_...",
    "defaults": { "max_output_tokens": 4096 }
  }
}
```

A successful response carries typed provider metadata:

```json
{
  "kind": "configured",
  "configuration": {
    "provider_id": "seren-models",
    "account_id": "acct_123",
    "default_model": "seren-default"
  }
}
```

`provider.health` performs the minimal round-trip required to confirm credentials and network reachability. It is separate from `configure` so that a courier may run health checks without re-validating config on every turn. The `health` request reuses the same `config` object shape as `configure`.

## Completion and Streaming

Dispatch models inference as a single canonical message list plus an optional tool catalog, tool-choice policy, and generation parameters:

```json
{
  "kind": "complete",
  "model": "seren-default",
  "messages": [
    { "role": "system", "content": [{ "kind": "text", "text": "You are helpful." }] },
    { "role": "user", "content": [{ "kind": "text", "text": "Hi." }] }
  ],
  "tools": [],
  "tool_choice": "auto",
  "parameters": {
    "max_output_tokens": 512,
    "temperature": 0.7
  },
  "metadata": {}
}
```

`provider.complete` returns one terminal result:

```json
{
  "kind": "completion",
  "response": {
    "model": "seren-default",
    "stop_reason": "end_turn",
    "content": [{ "kind": "text", "text": "Hello." }],
    "tool_calls": [],
    "usage": { "input_tokens": 17, "output_tokens": 2 }
  }
}
```

`provider.stream` is stream-first. The plugin emits zero or more JSON-RPC notifications with method `provider.event`, followed by exactly one terminal success response whose `result.kind` is `completion`.

Event kinds:

| Kind | Fields | Purpose |
|---|---|---|
| `content_delta` | `index`, `delta` | Incremental text or other content-block delta |
| `tool_call_delta` | `tool_call_id`, `delta` | Incremental tool-call argument delta |
| `message_start` | `response` | Begin-of-message marker with partial metadata |
| `message_stop` | `stop_reason` | End-of-message marker before the terminal response |
| `ping` | (none) | Keepalive |

Example stream for `provider.stream` request id `7`:

```json
{"jsonrpc":"2.0","method":"provider.event","params":{"kind":"content_delta","index":0,"delta":{"kind":"text","text":"Hel"}}}
{"jsonrpc":"2.0","method":"provider.event","params":{"kind":"content_delta","index":0,"delta":{"kind":"text","text":"lo."}}}
{"jsonrpc":"2.0","id":7,"result":{"kind":"completion","response":{"model":"seren-default","stop_reason":"end_turn","content":[{"kind":"text","text":"Hello."}],"tool_calls":[],"usage":{"input_tokens":17,"output_tokens":2}}}}
```

`provider.cancel` accepts an `id` field referencing the in-flight `provider.stream` request id. The provider should terminate generation as soon as possible and emit the terminal `completion` response with `stop_reason = "cancelled"`. Cancellation is best-effort; a provider that does not support cancellation should return an appropriate error.

## Content Blocks

Messages carry content blocks, not plain strings. The canonical kinds are:

- `text` - `{ "kind": "text", "text": "..." }`
- `image_url` - `{ "kind": "image_url", "url": "...", "media_type": "image/png" }`
- `image_base64` - `{ "kind": "image_base64", "data": "...", "media_type": "image/png" }`
- `tool_use` - `{ "kind": "tool_use", "id": "call_1", "name": "search", "input": { ... } }`
- `tool_result` - `{ "kind": "tool_result", "tool_use_id": "call_1", "content": [...] }`

Providers that cannot represent a block kind should reject the request with a structured error rather than silently dropping content.

## Errors

Structured Dispatch errors are returned as JSON-RPC error responses. Dispatch-specific
error details live in `error.data.dispatch_error`:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "error": {
    "code": -32000,
    "message": "provider rejected model",
    "data": {
      "dispatch_error": {
        "code": "unsupported_model",
        "message": "provider rejected model",
        "details": { "model": "unknown-model" }
      }
    }
  }
}
```

Reserved error codes:

- `unsupported_model` - the requested model is not offered by this provider
- `unsupported_modality` - the request contains a content-block kind the provider cannot render
- `context_length_exceeded` - input exceeds the model's context window
- `rate_limited` - upstream rate limit hit; retryable
- `authentication_failed` - credentials invalid
- `upstream_error` - transient upstream failure

## Implementation Guidance

- Keep warm upstream connections (HTTP keep-alive, websocket pools) between requests.
- Stream events as they arrive upstream - do not buffer to completion before emitting `provider.event` notifications.
- Emit `ping` events on long-running streams so Dispatch's host-side timeouts do not fire on otherwise-healthy streams.
- Never retry `complete` or `stream` internally past a single attempt. Retry policy belongs to the caller.
- Surface upstream usage metrics (`input_tokens`, `output_tokens`, cached tokens) in the terminal `completion` result so couriers can forward them to accounting and telemetry.

## Trust Model

Installing a provider plugin is an explicit trust action, equivalent to installing any other Dispatch plugin.

Providers typically receive:

- API credentials for the upstream inference service
- Raw prompt content, including system prompts, user messages, and tool results
- Declared environment and secret values routed through configuration

For that reason Dispatch does not auto-discover arbitrary executables as provider plugins. The capability-based trust work tracked in [`plugin-ecosystem.md`](./plugin-ecosystem.md) applies to providers once it lands.
