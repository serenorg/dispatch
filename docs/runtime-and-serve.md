# Long-Lived Runtime and `dispatch serve`

This document defines the full runtime program for Dispatch long-lived execution.

The goal is not to treat detached runs and `dispatch serve` as separate products.
They should share one runtime model, one run registry, and one lifecycle contract.

## Goals

- Add a durable run model for parcel execution beyond single foreground turns.
- Support both detached one-off execution and always-on service execution.
- Keep the runtime model local-first and repo-friendly.
- Avoid requiring a central daemon for the first usable implementation.
- Make room for a later service supervisor without changing user-facing run identity.
- Preserve the existing parcel, courier, and session vocabulary.

## Non-Goals

- Do not add a fake Docker-compatible surface for concepts Dispatch does not yet have.
- Do not add detached interactive chat in the first runtime slice.
- Do not add `ENTRYPOINT http` before the runtime trigger model exists.
- Do not build a networked daemon API before the local process/runtime model is stable.

## Runtime Model

Dispatch should treat a long-lived execution as a first-class `run`.

A run is distinct from:

- a parcel: the built artifact
- a courier session: the backend conversation state
- parcel state: built-in memory/checkpoint/session storage

A run is the process-level lifecycle wrapper around one parcel execution mode.

## Run Types

The runtime should support these run kinds:

- `job`
- `heartbeat`
- `service`

`chat` remains foreground-only until there is a compelling background use case.

`service` is the umbrella runtime for long-lived wakeable parcels and future ingress.

## Command Surface

The full runtime command set should converge on:

- `dispatch run <path> --detach --job <payload>`
- `dispatch run <path> --detach --heartbeat [payload]`
- `dispatch serve <path>`
- `dispatch serve <path> --schedule "<cron>"`
- `dispatch serve <path> --listen <addr>`
- `dispatch ps`
- `dispatch logs <run>`
- `dispatch stop <run>`
- `dispatch rm <run>`
- `dispatch inspect-run <run>`

Docker-style aliases also make sense on top of the base commands:

- `dispatch container ls`
- `dispatch container logs <run>`
- `dispatch container inspect <run>`
- `dispatch container stop <run>`
- `dispatch container rm <run>`

## Shared Run Registry

All detached and service execution should use the same on-disk run registry.

Default location:

- repo-local: `.dispatch/runs/`

Files:

- `.dispatch/runs/<run-id>.json`
- `.dispatch/runs/<run-id>.log`
- optional `.dispatch/runs/<run-id>.session.json`

This matches Dispatch's existing local-first storage posture and keeps runtime state
discoverable next to `.dispatch/parcels` and `.dispatch/state`.

## Run Record Schema

The current run record shape should contain:

- `run_id`
- `parcel_digest`
- `parcel_name`
- `parcel_version`
- `parcel_path`
- `courier`
- `operation`
- `status`
- `pid`
- `process_group_id`
- `started_at`
- `stopped_at`
- `exit_code`
- `session_file`
- `log_path`
- `trigger`
- `metadata`

Recommended enums:

- `status`: `starting`, `running`, `stopped`, `exited`, `failed`
- `trigger`: `manual`, `schedule`, `webhook`, `event`, `resume`

`operation.kind` carries the runtime kind (`job`, `heartbeat`, `service`) today,
so a separate top-level `kind` field is optional rather than required.

## Process Architecture

Dispatch should use a helper process model.

Foreground CLI:

1. resolve or build the parcel
2. create the run record
3. spawn a detached helper
4. return immediately with the run id

Detached helper:

1. load the run record
2. open or resume the courier session
3. execute the operation
4. write stdout/stderr and run events to the log
5. update run status and exit metadata

Implementation detail:

- use a hidden internal subcommand such as `dispatch internal run-record`

This keeps lifecycle behavior in one binary and avoids shell-script wrappers.

## Daemonless First, Service-Compatible Later

The first working implementation should not require a resident daemon.

Instead:

- each detached run is its own process
- `ps` reads the run registry
- `stop` signals the recorded pid/process group
- `logs` reads the run log
- status reconciliation happens by checking pid liveness

This follows the Podman model rather than Docker's daemon model. The distinction
matters:

| | Daemon (Docker-style) | Daemonless (Podman-style) |
|---|---|---|
| Process owner | Central daemon owns all containers | Each run is its own process tree |
| Lifecycle coupling | Daemon restart affects all runs | Runs survive parent CLI exit |
| State source of truth | Daemon in-memory state | On-disk records + pid liveness |
| Coordination | Daemon mediates all access | File-level locking or advisory |
| Complexity | Higher (daemon health, socket auth) | Lower (just processes and files) |
| Future supervisor path | Already is the supervisor | Supervisor wraps existing records |

The daemonless model is correct for Dispatch because:

- the local-first, repo-scoped storage posture already assumes no central service
- `dispatch ps` can reconstruct state from run records + process table cheaply
- a future `dispatch serve` supervisor can manage the same run ids and files
  without migration pain - it just becomes the long-lived process that was
  previously the detached helper itself
- users who embed Dispatch in larger systems should not be forced to manage a
  daemon lifecycle

The daemon path should only be considered if coordination requirements emerge
that file-level state cannot serve (e.g., cross-machine run routing, live
WebSocket multiplexing). Even then, it should manage existing run records rather
than introduce a parallel runtime identity.

## `dispatch serve`

`dispatch serve` should be built on the same runtime layer, not as a parallel system.

`dispatch serve <path>` should:

- create a `service` run record
- spawn a long-lived helper
- keep the run alive while idle
- react to wake reasons

Wake reasons:

- manual turn
- heartbeat
- schedule
- webhook/event ingress
- resume/recovery

The service helper should own:

- a parcel/session lifecycle
- trigger dispatch
- log emission
- graceful shutdown

## Ingress Model

Do not add `ENTRYPOINT http`.

If ingress is added, it should land as:

1. a courier/runtime operation contract that represents inbound requests/events
2. a server layer that binds ports and turns inbound traffic into runtime wake events

This keeps the network server separate from the parcel execution contract.

## Scheduling

Scheduling should be attached to `service` runs, not one-off detached jobs.

Possible future command shape:

- `dispatch serve <path> --schedule "*/5 * * * *"`

But the runtime primitives should exist before exposing the scheduling UX:

- run registry
- service helper
- wake reasons
- status/logging

### Schedule persistence

Schedules should be stored in the run record or an adjacent SQLite table, not
in-memory only. This allows the service helper to reconstruct its schedule after
a restart without the user re-specifying it. The current implementation stores
the schedule state directly in the run record.

Minimal schema:

- `run_id`, `callback` (wake reason kind), `schedule_expr` (cron string),
  `next_fire_at`, `last_fired_at`, `payload`

The helper evaluates `next_fire_at` on each tick and fires the corresponding
wake reason. After each fire, `next_fire_at` is recalculated from the cron
expression.

### Schedule sources

Schedules can originate from:

- CLI flag: `dispatch serve <path> --schedule "*/5 * * * *"`
- Agentfile declaration: `SCHEDULE "<cron>"`
- Runtime API: if Dispatch ever exposes a control socket

The current implementation supports CLI flags and parcel-authored `SCHEDULE`
directives. Runtime APIs remain a later follow-on.

## Ingress Sources

Listener bindings can originate from:

- CLI flag: `dispatch serve <path> --listen 127.0.0.1:0`
- Agentfile declaration: `LISTEN "127.0.0.1:0"`

The current implementation supports both and merges them without duplication
when a service run record is created.

## Current Ingress Behavior

The current implementation supports local HTTP ingress on service runs via:

- `dispatch serve <path> --listen 127.0.0.1:0`

Listener state is persisted directly in the run record:

- `listen_addr`
- `bound_addr`
- `requests_handled`
- `last_request_at`

Inbound requests are translated into heartbeat payload envelopes rather than a
new parcel entrypoint. The current envelope shape includes:

- listener address
- remote address
- HTTP method
- request target/path/query
- lowercased request headers
- text body

Responses are intentionally simple:

- `202 Accepted` when the heartbeat dispatch succeeds
- `400 Bad Request` for malformed HTTP
- `500 Internal Server Error` when the heartbeat execution fails

This keeps the current runtime honest: ingress is a wake source for heartbeat
services, not a separate HTTP application contract.

## Session Semantics

Runs should be allowed to reference a courier session file, but the run is the
top-level lifecycle object.

That means:

- a run may own a session
- a run may reuse a session
- a stopped run does not imply the parcel state is deleted

For background execution, run identity should be the thing users inspect, stop,
and tail. Session identity stays backend-facing.

## Log Semantics

Each run gets one append-only log file.

The minimal contract:

- combine stdout and stderr
- include lifecycle messages from the helper
- keep the file after exit until `rm`
- support `logs --follow`

Later improvements:

- structured event log
- split `stdout` and `stderr`
- lifecycle events emitted in JSON

## Liveness and Adoption

The runtime should validate on-disk records against the actual process table.

At minimum:

- if pid is dead, reconcile status to `exited` or `failed`
- if the command no longer matches, mark the record stale
- if a helper survives a parent CLI exit, `ps` should still show it

This follows the same broad pattern as local process supervisors that:

- store pid + process-group ids
- validate liveness on read
- clean up stale records opportunistically

## State Separation

Keep these roots separate:

- `.dispatch/parcels`
- `.dispatch/state`
- `.dispatch/runs`

`state` is parcel-scoped runtime data for built-in tools and courier state.

`runs` is process-scoped lifecycle data for long-lived execution.

Do not merge them.

## Implementation Order

1. Add `runs.rs` with:
  - run record type
  - root resolution
  - load/save/list helpers
  - pid liveness reconciliation
2. Add hidden `dispatch internal run-detached`
3. Add `dispatch run --detach` for:
  - `--job`
  - `--heartbeat`
4. Add:
  - `dispatch ps`
  - `dispatch logs`
  - `dispatch stop`
  - `dispatch rm`
5. Add `dispatch inspect-run`
6. Add `dispatch serve <path>` on top of the same helper/runtime layer
7. Add wake reason plumbing for schedules and ingress

## Testing Strategy

Required tests:

- detached run creates a run record and returns immediately
- run helper writes final status on success and failure
- `ps` reconciles dead pids
- `logs` reads the correct file
- `stop` terminates a running helper or helper process group
- `rm` refuses to remove running runs without force, and force removes after stop
- `serve` creates a long-lived service run record
- service helper survives idle waits and updates state on shutdown

## Recommendation

Implement the runtime as one program with one run registry and two modes:

- detached finite runs
- long-lived service runs

Do not build a separate daemon-only architecture first.

If a supervisor daemon becomes necessary later, it should manage the same run
records and command semantics rather than introducing a second runtime identity.

## Reference Implementations

These external projects informed the design decisions above. None is a direct
template for Dispatch, but each validates or constrains a specific design axis.

### Podman (process-per-container, no daemon)

The primary model for Dispatch's daemonless runtime. Each container is a
standalone process. `podman ps` reads state from disk + process table.
`podman run --detach` returns immediately. No socket, no daemon health to manage.
Dispatch's run registry, pid liveness checks, and `dispatch ps` follow this
pattern directly.

### Cloudflare Agents SDK (SQLite-backed scheduling, alarm-based wake)

Cloudflare Agents store schedules in a SQLite table (`cf_agents_schedules`) with
cron, delayed, and interval variants. An alarm fires when the next schedule is
due, executes all due tasks, then recalculates the next alarm. This remains a
useful reference for a future Dispatch scheduler, but the current implementation
persists schedule state directly in the run record rather than SQLite.

Also relevant: their state sync model (server-side `setState` with broadcast to
connected clients) is a future pattern if Dispatch ever needs live run status
pushed to a UI rather than polled via `dispatch ps`.

### Modal (declarative scheduling, ephemeral execution)

Modal attaches schedules to function definitions declaratively. The equivalent
for Dispatch would be Agentfile-level `SCHEDULE` directives that embed cron
expressions into the parcel, so `dispatch serve` can read them without CLI flags.
Modal's execution is fully ephemeral (no persistent container state between
calls), which validates Dispatch's separation of run lifecycle from parcel state.

### IronClaw (routine engine, hybrid daemon)

IronClaw's `RoutineEngine` implements cron-polled and event-triggered routines
with a background ticker. Its scheduler dispatches parallel jobs with per-job
credential scoping. The ticker + event-matching pattern maps to Dispatch's
planned wake reason model: the service helper runs a tick loop that checks
schedules and listens for external triggers (webhooks, manual turns).

IronClaw's hybrid model (long-lived host process + optional container isolation)
also validates keeping `dispatch serve` as a simple long-lived process that can
optionally route execution to Docker/WASM couriers.

### CrewAI (checkpoint/resume, human-in-the-loop)

CrewAI's flow persistence (`save_state` / `load_state` / `save_pending_feedback`)
validates Dispatch's existing `checkpoint_store` pattern. Their human-in-the-loop
model (pause execution, persist state, resume with feedback) maps to a future
where `dispatch serve` could pause a session on a tool approval and resume when
the operator responds.

### Daytona (snapshot/fork, stateless runner)

Daytona's snapshot model (OCI images of sandbox state) is relevant if Dispatch
ever needs to checkpoint and fork running parcel state for branching execution.
Their stateless poller pattern (runner polls control plane for jobs) is the
natural model for a future remote `dispatch serve` deployment where a fleet of
runners pulls work from a central queue.
