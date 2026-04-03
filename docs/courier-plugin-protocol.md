# Courier Plugin Protocol

Dispatch courier plugins are external executables that implement the Dispatch courier contract over stdio.

Dispatch uses newline-delimited JSON envelopes and responses over stdio, with one important execution rule:

- `open_session` may start a persistent plugin process for that session
- subsequent `run` requests for the same session are sent to the same process over the same stdio stream

This removes the per-turn process spawn cost for multi-turn chat, job, and heartbeat flows.

## Transport

The protocol uses newline-delimited JSON over stdio.

- Dispatch writes one JSON request line at a time to plugin stdin
- the plugin writes one JSON object per line to stdout
- stderr is reserved for human-readable diagnostics and logs

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

Dispatch currently supports protocol version `1`.

## Request Envelope

Every request uses the same envelope shape:

```json
{
  "protocol_version": 1,
  "request": {
    "kind": "capabilities"
  }
}
```

Current request kinds:

- `capabilities`
- `validate_parcel`
- `inspect`
- `open_session`
- `resume_session`
- `shutdown`
- `run`

For parcel-aware requests, Dispatch passes the absolute built parcel directory in `parcel_dir`.

## Session Lifecycle

The key rule is session affinity.

- `capabilities`, `validate_parcel`, and `inspect` may still be handled as one-shot requests
- `open_session` creates a session and may leave the plugin process running
- `resume_session` lets Dispatch recreate a persistent plugin process from a previously saved `CourierSession`
- `run` requests for that session are sent to the same process
- `shutdown` gives a persistent plugin process one explicit chance to flush state and exit cleanly

Dispatch keeps one persistent process per open plugin session.

Plugins should therefore:

- treat stdin as a request stream, not a single request body
- keep session-local state in memory after `open_session`
- accept `resume_session` when Dispatch needs to reattach to a saved session after a new host process starts
- continue reading requests until `shutdown`, stdin close, or process termination

## Responses

Response shapes are:

Non-streaming requests return one line with `kind: "result"` or `kind: "error"`.

`run` remains stream-first:

- zero or more `{"kind":"event",...}` lines
- one terminal `{"kind":"done",...}` line

Example `run` stream:

```json
{"kind":"event","event":{"kind":"message","role":"assistant","content":"hello"}}
{"kind":"done","session":{"id":"remote-worker-<digest>-1","parcel_digest":"<digest>","entrypoint":"chat","turn_count":2,"history":[{"role":"user","content":"hello"},{"role":"assistant","content":"hello"}]}}
```

## Implementation Guidance

The intended implementation model is:

- keep warm model/tool/runtime state in memory per session
- continue reading requests until stdin closes or the process is terminated
- avoid per-turn process startup during multi-turn chat, job, and heartbeat flows

## Trust Model

Installing a courier plugin is an explicit trust action.

Plugins may receive:

- absolute parcel directory paths
- courier session state
- operation input
- declared environment and secret values, when Dispatch routes execution through the plugin

For that reason, Dispatch does not auto-discover arbitrary executables as courier plugins.
