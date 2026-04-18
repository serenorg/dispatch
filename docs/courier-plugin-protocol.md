# Courier Plugin Protocol

Dispatch courier plugins are external executables that implement the Dispatch courier contract over stdio.

Dispatch uses JSON-RPC 2.0 messages framed as line-delimited JSON over stdio, with one important execution rule:

- `open_session` may start a persistent plugin process for that session
- subsequent `run` requests for the same session are sent to the same process over the same stdio stream

This removes the per-turn process spawn cost for multi-turn chat, job, and heartbeat flows.

## Transport

The protocol uses JSON-RPC 2.0 over stdio, framed as newline-delimited JSON.

- Dispatch writes one JSON request line at a time to plugin stdin
- the plugin writes one JSON-RPC message per line to stdout
- stderr is reserved for human-readable diagnostics and logs

Dispatch does not currently use JSON-RPC batch requests. The host keeps at
most one request in flight per plugin/session stream and expects each terminal
response to echo the request `id`.

Dispatch may keep stdin/stdout open across multiple requests for one session.

## Plugin Manifest

Plugins declare the protocol version in their manifest:

```json
{
  "name": "remote-worker",
  "version": "0.2.0",
  "protocol_version": 1,
  "transport": "jsonl",
  "description": "Execute Dispatch parcels on a remote worker pool.",
  "exec": {
    "command": "/usr/local/bin/dispatch-courier-remote-worker",
    "args": ["--stdio"]
  }
}
```

Dispatch supports protocol version `1`.

## Requests

Every host call is sent as a JSON-RPC request. The `method` identifies the courier
operation and `params` contains the typed Dispatch request payload.

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "courier.capabilities",
  "params": {
    "protocol_version": 1,
    "kind": "capabilities"
  }
}
```

Courier request methods:

- `courier.capabilities`
- `courier.validate_parcel`
- `courier.inspect`
- `courier.open_session`
- `courier.resume_session`
- `courier.shutdown`
- `courier.run`

For parcel-aware requests, Dispatch passes the absolute built parcel directory in
`parcel_dir`. The `params.kind` payload still uses the same typed Dispatch request
shape that the Rust protocol crate exposes; JSON-RPC only standardizes the envelope.

## Session Lifecycle

The key rule is session affinity.

- `capabilities`, `validate_parcel`, and `inspect` may be handled as one-shot requests
- `open_session` creates a session and may leave the plugin process running
- `resume_session` lets Dispatch recreate a persistent plugin process from a previously saved `CourierSession`
- `run` requests for that session are sent to the same process
- `shutdown` gives a persistent plugin process one explicit chance to flush state and exit cleanly

Dispatch keeps one persistent process per open plugin session.
Dispatch owns that session-to-process affinity; plugins should not assume they can detect or repair host-side violations of it.

Plugins should therefore:

- treat stdin as a request stream, not a single request body
- keep session-local state in memory after `open_session`
- persist any opaque plugin-owned resume data in `CourierSession.backend_state`
- accept `resume_session` when Dispatch needs to reattach to a saved session after a new host process starts
- continue reading requests until `shutdown`, stdin close, or process termination

## Responses

Non-streaming requests return exactly one JSON-RPC success response:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "kind": "capabilities",
    "capabilities": {
      "id": "dispatch-courier-echo",
      "kind": "plugin"
    }
  }
}
```

Structured Dispatch errors are returned as JSON-RPC error responses. Dispatch-specific
error details live in `error.data.dispatch_error`.

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "error": {
    "code": -32000,
    "message": "courier rejected parcel",
    "data": {
      "dispatch_error": {
        "code": "unsupported_parcel",
        "message": "courier rejected parcel",
        "details": {
          "reference": "dispatch/docker:latest"
        }
      }
    }
  }
}
```

`run` remains stream-first:

- zero or more JSON-RPC notifications with method `courier.event`
- one terminal JSON-RPC success response whose `result.kind` is `done`

Example `run` stream:

```json
{"jsonrpc":"2.0","method":"courier.event","params":{"kind":"message","role":"assistant","content":"hello"}}
{"jsonrpc":"2.0","id":7,"result":{"kind":"done","session":{"id":"remote-worker-<digest>-1","parcel_digest":"<digest>","entrypoint":"chat","turn_count":2,"history":[{"role":"user","content":"hello"},{"role":"assistant","content":"hello"}]}}}
```

Plugins may also emit a first-class structured channel reply event when the
caller will bridge the courier response back through a channel plugin:

```json
{"jsonrpc":"2.0","method":"courier.event","params":{"kind":"channel_reply","message":{"content":"Dispatch attached the report.","content_type":"text/plain","attachments":[{"name":"report.txt","mime_type":"text/plain","data_base64":"aGVsbG8="}],"metadata":{"custom":"value"}}}}
```

The manifest field `transport = "jsonl"` still refers to framing: one JSON
message per line on stdio. JSON-RPC defines the message shape inside that framing.

## Implementation Guidance

The intended implementation model is:

- keep warm model/tool/runtime state in memory per session
- mirror any state needed after a host restart into `CourierSession.backend_state`
- continue reading requests until stdin closes or the process is terminated
- avoid per-turn process startup during multi-turn chat, job, and heartbeat flows

If a plugin cannot reconstruct a saved session during `resume_session`, it should return an
JSON-RPC error response and let Dispatch surface the failed resume. Dispatch does not retry
with a different protocol version or silently downgrade `resume_session` into `open_session`.

## Trust Model

Installing a courier plugin is an explicit trust action.

Plugins may receive:

- absolute parcel directory paths
- courier session state
- operation input
- declared environment and secret values, when Dispatch routes execution through the plugin

For that reason, Dispatch does not auto-discover arbitrary executables as courier plugins.
