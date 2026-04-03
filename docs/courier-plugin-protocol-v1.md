# Courier Plugin Protocol v1

Dispatch courier plugins are external executables that implement the Dispatch courier contract over stdio.

Protocol goals:

- language-agnostic
- stream-friendly
- easy to debug locally
- explicit about parcel access

## Transport

Protocol v1 uses newline-delimited JSON over stdio.

- Dispatch writes one JSON request line to plugin stdin
- the plugin writes one JSON object per line to stdout
- stderr is reserved for human-readable diagnostics and logs

Dispatch chose JSONL rather than JSON-RPC because courier turns are fundamentally sequential event streams, not general-purpose RPC sessions.

Dispatch also publishes these protocol types from `dispatch-core::plugin_protocol`, and the in-repo reference implementation lives in `crates/dispatch-courier-echo`.

## Plugin Manifest

Plugins are installed from a manifest JSON file.

Example:

```json
{
  "name": "remote-worker",
  "version": "0.1.0",
  "protocol_version": 1,
  "transport": "jsonl",
  "description": "Execute Dispatch parcels on a remote worker pool.",
  "exec": {
    "command": "/usr/local/bin/dispatch-courier-remote-worker",
    "args": ["--stdio"]
  }
}
```

Dispatch stores installed plugin manifests in the local courier registry and exposes them through:

- `dispatch courier ls`
- `dispatch courier inspect <name>`
- `dispatch courier install <manifest>`
- `dispatch run --courier <name> --registry <path>`
- `dispatch inspect <parcel> --courier <name> --registry <path>`

## Request Envelope

Every plugin request is one JSON object. All requests should include:

- `protocol_version`
- `request`

Example:

```json
{
  "protocol_version": 1,
  "request": {
    "kind": "capabilities"
  }
}
```

For parcel-aware requests, Dispatch passes the absolute built parcel directory:

```json
{
  "protocol_version": 1,
  "request": {
    "kind": "inspect",
    "parcel_dir": "/absolute/path/to/.dispatch/parcels/<digest>"
  }
}
```

## Request Kinds

Current v1 request kinds:

- `capabilities`
- `validate_parcel`
- `inspect`
- `open_session`
- `run`

## Responses

Non-streaming requests return one line with `kind: "result"` or `kind: "error"`.

Example:

```json
{
  "kind": "result",
  "capabilities": {
    "courier_id": "remote-worker",
    "supports_chat": true
  }
}
```

Other successful non-streaming response shapes are:

- `{"kind":"result"}` for `validate_parcel`
- `{"kind":"result","inspection":{...}}` for `inspect`
- `{"kind":"result","session":{...}}` for `open_session`

## Streaming `run`

`run` is stream-first in v1.

Dispatch sends one `run` request line. The plugin then emits zero or more event lines followed by a terminal `done` line.

Example request:

```json
{
  "protocol_version": 1,
  "request": {
    "kind": "run",
    "parcel_dir": "/absolute/path/to/.dispatch/parcels/<digest>",
    "session": {
      "id": "remote-worker-<digest>-1",
      "parcel_digest": "<digest>",
      "entrypoint": "chat",
      "turn_count": 1,
      "history": []
    },
    "operation": {
      "kind": "chat",
      "input": "hello"
    }
  }
}
```

Example event stream:

```json
{"kind":"event","event":{"kind":"message","role":"assistant","content":"hello"}}
{"kind":"done","session":{"id":"remote-worker-<digest>-1","parcel_digest":"<digest>","entrypoint":"chat","turn_count":2,"history":[{"role":"user","content":"hello"},{"role":"assistant","content":"hello"}]}}
```

## Error Handling

Plugins should emit structured errors on stdout:

```json
{
  "kind": "error",
  "error": {
    "code": "unsupported_operation",
    "message": "heartbeat is not supported by courier remote-worker"
  }
}
```

Dispatch treats malformed stdout as a protocol error.

## Trust Model

Installing a courier plugin is an explicit trust action.

Plugins may receive:

- absolute parcel directory paths
- courier session state
- operation input
- declared environment and secret values, when Dispatch routes execution through the plugin

For that reason, Dispatch does not auto-discover arbitrary executables as courier plugins.
