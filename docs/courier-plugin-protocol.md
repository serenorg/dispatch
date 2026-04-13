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

Dispatch supports protocol version `1`.

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

Request kinds:

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

Response shapes are:

Non-streaming requests return exactly one line with one of:

- `{"kind":"capabilities","capabilities":...}`
- `{"kind":"inspection","inspection":...}`
- `{"kind":"session","session":...}`
- `{"kind":"ok"}`
- `{"kind":"error","error":...}`

`run` remains stream-first:

- zero or more `{"kind":"event",...}` lines
- one terminal `{"kind":"done",...}` line

Example `run` stream:

```json
{"kind":"event","event":{"kind":"message","role":"assistant","content":"hello"}}
{"kind":"done","session":{"id":"remote-worker-<digest>-1","parcel_digest":"<digest>","entrypoint":"chat","turn_count":2,"history":[{"role":"user","content":"hello"},{"role":"assistant","content":"hello"}]}}
```

Plugins may also emit a first-class structured channel reply event when the
caller will bridge the courier response back through a channel plugin:

```json
{"kind":"event","event":{"kind":"channel_reply","message":{"content":"Dispatch attached the report.","content_type":"text/plain","attachments":[{"name":"report.txt","mime_type":"text/plain","data_base64":"aGVsbG8="}],"metadata":{"custom":"value"}}}}
```

## Implementation Guidance

The intended implementation model is:

- keep warm model/tool/runtime state in memory per session
- mirror any state needed after a host restart into `CourierSession.backend_state`
- continue reading requests until stdin closes or the process is terminated
- avoid per-turn process startup during multi-turn chat, job, and heartbeat flows

If a plugin cannot reconstruct a saved session during `resume_session`, it should return an
`error` response and let Dispatch surface the failed resume. Dispatch does not retry with a
different protocol version or silently downgrade `resume_session` into `open_session`.

## Trust Model

Installing a courier plugin is an explicit trust action.

Plugins may receive:

- absolute parcel directory paths
- courier session state
- operation input
- declared environment and secret values, when Dispatch routes execution through the plugin

For that reason, Dispatch does not auto-discover arbitrary executables as courier plugins.
