# Courier Implementer Guide

Dispatch is defined by three layers:

- `Agentfile` as the authored build language
- the built parcel manifest and packaged context as the portable artifact
- the courier contract as the execution boundary

If you are implementing a Dispatch-compatible courier, the courier contract matters more than the CLI. Compatibility means your courier can load a Dispatch parcel, validate whether it can execute it, and honor the expected session and operation semantics.

## Courier Contract

The reference contract lives in [`CourierBackend`](../crates/dispatch-core/src/courier.rs).

Required responsibilities:

- `capabilities()` reports what the courier can do
- `validate_parcel()` checks whether this courier can execute the parcel's declared courier reference
- `inspect()` returns non-mutating courier metadata for the parcel
- `open_session()` creates a fresh courier session bound to the parcel digest
- `run()` executes one operation against one session and returns ordered courier events

## Compatibility Rules

`validate_parcel()` is the compatibility gate.

Minimum expectations:

- it must reject parcels whose `courier.reference` is incompatible with the courier implementation
- it must not mutate the parcel or session state
- it should fail before execution, not halfway through a turn

The Dispatch CLI calls `validate_parcel()` before opening a session.

## Session Rules

`open_session()` creates a courier-owned session record.

Minimum expectations:

- `session.parcel_digest` must match the loaded parcel digest
- `session.turn_count` starts at `0`
- `session.history` starts empty unless the courier explicitly restores persisted state outside this API
- `session.entrypoint` should reflect the parcel entrypoint when one is declared

`run()` must reject requests where the provided session is not bound to the loaded parcel.

## Operation Rules

Current operations:

- `ResolvePrompt`
- `ListLocalTools`
- `InvokeTool`
- `Chat`
- `Job`
- `Heartbeat`

Minimum expectations:

- `ResolvePrompt` and `ListLocalTools` are parcel-inspection helpers, not turn-execution operations
- `ResolvePrompt` returns the resolved prompt stack from packaged instruction files
- `ListLocalTools` returns the declared local tool list from the parcel manifest
- `InvokeTool` executes one declared local tool or rejects the request if unsupported
- `Chat`, `Job`, and `Heartbeat` must reject mismatched parcel entrypoints

Couriers are allowed to reject operations they do not support, but they should do so explicitly with an unsupported-operation error.

## Event Rules

Each `run()` call returns ordered courier events.

Minimum expectations:

- successful turns should end with `CourierEvent::Done`
- tool execution should emit `ToolCallStarted` before `ToolCallFinished`
- prompt resolution should emit `PromptResolved`
- local tool enumeration should emit `LocalToolsListed`
- couriers that fall back from one execution path to another should surface that as an explicit event when possible

Dispatch currently returns a bounded batch of ordered events per turn. A future courier protocol may stream them incrementally, but the event ordering guarantees should remain stable.

For external JSONL courier plugins, Dispatch keeps one long-lived subprocess per open session and sends subsequent `run` requests over the same stdio stream.

## Inspection Rules

`inspect()` should be safe and non-mutating.

Minimum expectations:

- report courier id and kind
- report parcel entrypoint
- report required secrets
- report declared mounts
- report declared local tools

Inspection should not require opening a courier session.

## Tool Execution Rules

For couriers that execute packaged local tools:

- only declared parcel tools may be exposed to the model or to operator-facing inspection commands
- only declared local tools may be invoked
- required secrets must be enforced before execution
- couriers should avoid forwarding their full ambient environment to tools
- tool execution results should preserve stdout, stderr, exit code, command, and args

The reference native courier clears the child environment and only forwards a minimal system environment plus declared `ENV` and declared `SECRET` values.

## Conformance Tests

The public conformance skeleton lives in:

- [`courier_conformance.rs`](../crates/dispatch-core/tests/courier_conformance.rs)

The current suite checks:

- courier/parcel compatibility validation
- session binding to parcel digests
- built-in mount resolution on `open_session()`
- prompt resolution behavior
- local tool listing behavior
- conditional chat execution
- conditional job execution
- conditional heartbeat execution
- conditional direct local tool invocation
- conditional A2A tool invocation through card discovery and expected agent identity
- explicit rejection of unsupported execution in stub couriers

If you are building a new courier, these tests are the minimum target. Add courier-specific tests in your own crate, but keep the shared public contract passing.

The Dispatch CLI also exposes a generated harness:

- `dispatch courier conformance <name>`

That command builds temporary fixture parcels, runs the shared contract checks against the selected built-in or installed courier, and reports pass/fail per check. Use it as the quickest operator-facing validation pass before you wire courier-specific tests into your own CI.

## Practical Guidance

- keep the parcel format portable and courier-agnostic
- treat `courier.reference` as a compatibility declaration, not just a label
- avoid depending on CLI-only behavior for correctness
- prefer explicit unsupported-operation errors over silent no-ops
- keep inspection and validation cheap and deterministic

Dispatch does not require Docker, WASM, or any other execution engine. Those are implementation choices behind the courier boundary.

For the built-in Docker courier specifically, the current boundary is:

- Dispatch keeps session history, mounts, and hosted-model orchestration on the host
- declared local tools run inside Docker as the isolated execution surface
- the parcel is not currently executed as a full in-container agent runtime

## WASM Host Model Calls

For `dispatch/wasm`, hosted-model access remains a host responsibility even when the guest initiates the request through the WIT ABI.

Minimum expectations:

- `model-complete` should use the parcel's declared `MODEL` and `FALLBACK` policy unless the guest explicitly requests a model id
- guest-supplied model ids should be treated as model selection within the host's configured provider policy, not as authority to switch to arbitrary providers
- if the primary hosted-model request fails before producing a reply, the courier should try declared fallback models in order
- prompt resolution, declared tool exposure, and memory access remain host-owned even when the guest orchestrates the turn
