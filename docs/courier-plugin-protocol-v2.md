# Courier Plugin Protocol v2

Dispatch courier plugins are external executables that implement the Dispatch courier contract over stdio.

Protocol v2 keeps the same newline-delimited JSON envelope and response shapes as v1, but adds one important execution rule:

- `open_session` may start a persistent plugin process for that session
- subsequent `run` requests for the same session are sent to the same process over the same stdio stream

This removes the per-turn process spawn cost for multi-turn chat, job, and heartbeat flows.

## Transport

Protocol v2 uses newline-delimited JSON over stdio.

- Dispatch writes one JSON request line at a time to plugin stdin
- the plugin writes one JSON object per line to stdout
- stderr is reserved for human-readable diagnostics and logs

Unlike v1, Dispatch may keep stdin/stdout open across multiple requests for one session.

## Plugin Manifest

Plugins opt into v2 by declaring:

```json
{
  "name": "remote-worker",
  "version": "0.2.0",
  "protocol_version": 2,
  "transport": "jsonl",
  "description": "Execute Dispatch parcels on a remote worker pool.",
  "exec": {
    "command": "/usr/local/bin/dispatch-courier-remote-worker",
    "args": ["--stdio"]
  }
}
```

Dispatch currently supports protocol versions `1` and `2`.

## Request Envelope

Every request uses the same envelope shape as v1:

```json
{
  "protocol_version": 2,
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
- `run`

For parcel-aware requests, Dispatch passes the absolute built parcel directory in `parcel_dir`.

## Session Lifecycle

The key v2 rule is session affinity.

- `capabilities`, `validate_parcel`, and `inspect` may still be handled as one-shot requests
- `open_session` creates a session and may leave the plugin process running
- `run` requests for that session are sent to the same process

Dispatch keeps one persistent process per open plugin session.

Plugins should therefore:

- treat stdin as a request stream, not a single request body
- keep session-local state in memory after `open_session`
- continue reading requests until stdin closes or the process is terminated

## Responses

Response shapes are unchanged from v1.

Non-streaming requests return one line with `kind: "result"` or `kind: "error"`.

`run` remains stream-first:

- zero or more `{"kind":"event",...}` lines
- one terminal `{"kind":"done",...}` line

Example `run` stream:

```json
{"kind":"event","event":{"kind":"message","role":"assistant","content":"hello"}}
{"kind":"done","session":{"id":"remote-worker-<digest>-1","parcel_digest":"<digest>","entrypoint":"chat","turn_count":2,"history":[{"role":"user","content":"hello"},{"role":"assistant","content":"hello"}]}}
```

## Compatibility Guidance

Use protocol v1 if:

- your courier is one-shot
- process startup cost is negligible
- you want the simplest possible implementation

Use protocol v2 if:

- your courier needs multi-turn performance
- you keep warm model/tool/runtime state in memory
- you want one long-lived process per session

## Trust Model

Installing a courier plugin is an explicit trust action.

Plugins may receive:

- absolute parcel directory paths
- courier session state
- operation input
- declared environment and secret values, when Dispatch routes execution through the plugin

For that reason, Dispatch does not auto-discover arbitrary executables as courier plugins.
