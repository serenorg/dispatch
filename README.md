# Dispatch

Dispatch is a Docker-style packaging and courier system for agents.

`Agentfile` is the authored build format inside Dispatch, similar to how `Dockerfile` fits inside Docker.

The goal is to make agents:

- buildable
- portable
- versioned
- reproducible
- deployable across couriers

Instead of treating an agent as "some prompt plus some tools plus some hidden execution config", `Agentfile` makes the build contract explicit.

## Vocabulary

Dispatch uses a consistent operational vocabulary:

- an `Agentfile` builds a **parcel**
- a parcel is described by `manifest.json`
- `parcel.lock` records parcel integrity metadata
- a **courier** executes a parcel
- a **depot** stores parcels
- a running parcel execution is a **dispatch**
- future multi-agent orchestration can be modeled as a **route** or **convoy**

## Core Idea

An agent project has:

- an `Agentfile`
- optional instruction files like `IDENTITY.md`, `SOUL.md`, `SKILL.md`, `AGENTS.md`, `USER.md`, `TOOLS.md`, `HEARTBEAT.md`, `MEMORY.md`
- an explicit `COMPONENT` for `dispatch/wasm` parcels
- local tools, reference assets, evals, and code

The `Agentfile` is the canonical authored manifest, similar to a `Dockerfile`.

Example (`examples/basic/Agentfile`):

```dockerfile
FROM dispatch/native:latest

NAME basic-assistant
VERSION 0.1.0

IDENTITY IDENTITY.md
SOUL SOUL.md
SKILL SKILL.md
AGENTS AGENTS.md
USER USER.md
TOOLS TOOLS.md
MEMORY POLICY MEMORY.md

MODEL gpt-5-mini
FALLBACK gpt-5-nano

TOOL BUILTIN system_time
TOOL BUILTIN web_search
TOOL BUILTIN topic_lookup
TOOL BUILTIN human_approval

MOUNT SESSION sqlite
MOUNT MEMORY none
MOUNT ARTIFACTS local

ENV TZ=UTC
VISIBILITY open

LIMIT ITERATIONS 20
TIMEOUT RUN 300s
TIMEOUT TOOL 60s
TIMEOUT LLM 120s

EVAL evals/smoke.eval

ENTRYPOINT chat
```


## Implementation

The repo includes:

- a real `Agentfile` parser
- heredoc support for inline prompt bodies
- a semantic validator for core instructions
- a working `dispatch lint` command
- a working `dispatch build` command
- a working `dispatch inspect` command
- a working `dispatch verify` command
- a working `dispatch run` command for local prompt resolution and declared local tool execution
- working native `chat`, `job`, and `heartbeat` operations with session history and event-based courier responses
- digest-addressed parcel artifact output under `.dispatch/parcels/<digest>/`
- an explicit courier trait layer for pluggable backends
- a typed `manifest.json` manifest with a published JSON Schema
- typed framework provenance for authoring metadata such as `FRAMEWORK adk-rust TARGET wasm`
- an explicit `COMPONENT` contract for `dispatch/wasm` parcels
- a typed WIT package for the Dispatch WASM guest ABI
- a reference external JSONL courier plugin in `crates/dispatch-courier-echo`

Run it:

```bash
cargo run -p dispatch -- lint examples/basic
cargo run -p dispatch -- lint examples/heartbeat-monitor
cargo run -p dispatch -- lint examples/wasm-reference
cargo run -p dispatch -- build examples/basic
cargo run -p dispatch -- build examples/heartbeat-monitor
cargo run -p dispatch -- build examples/wasm-reference
cargo run -p dispatch -- inspect examples/basic/.dispatch/parcels/<digest>
cargo run -p dispatch -- inspect examples/wasm-reference/.dispatch/parcels/<digest> --courier wasm
cargo run -p dispatch -- inspect examples/basic/.dispatch/parcels/<digest> --courier native
cargo run -p dispatch -- inspect examples/basic/.dispatch/parcels/<digest> --courier remote-worker --registry .dispatch/couriers.json
cargo run -p dispatch -- verify examples/basic/.dispatch/parcels/<digest>
cargo run -p dispatch -- courier ls
cargo run -p dispatch -- courier inspect docker
cargo run -p dispatch-courier-echo -- --stdio
cargo run -p dispatch -- run examples/basic/.dispatch/parcels/<digest> --print-prompt
cargo run -p dispatch -- run examples/basic/.dispatch/parcels/<digest> --courier native --print-prompt
cargo run -p dispatch -- run examples/basic/.dispatch/parcels/<digest> --courier remote-worker --registry .dispatch/couriers.json --chat "hello"
cargo run -p dispatch -- run examples/basic/.dispatch/parcels/<digest> --session-file .dispatch/session.json --chat "hello"
cargo run -p dispatch -- run examples/basic/.dispatch/parcels/<digest> --session-file .dispatch/session.json --interactive
cargo run -p dispatch -- run examples/heartbeat-monitor/.dispatch/parcels/<digest> --heartbeat "tick"
cargo run -p dispatch -- run examples/heartbeat-monitor/.dispatch/parcels/<digest> --heartbeat
cargo run -p dispatch -- run examples/heartbeat-monitor/.dispatch/parcels/<digest> --list-tools
cargo run -p dispatch -- run examples/heartbeat-monitor/.dispatch/parcels/<digest> --tool poll_mentions
cargo run -p dispatch -- run examples/wasm-reference/.dispatch/parcels/<digest> --courier wasm --chat "hello"
```

Print the parsed AST:

```bash
cargo run -p dispatch -- lint examples/basic --json
```

Build output:

- `manifest.json` - typed parcel manifest
- `parcel.lock` - file and digest metadata
- `context/` - packaged build content referenced by the `Agentfile`

The built manifest now includes a `$schema` pointer and is described by [`schemas/parcel.v1.json`](schemas/parcel.v1.json).

`verify` behavior:

- recomputes the parcel manifest digest from the normalized manifest content
- validates `parcel.lock` digest, layout metadata, and file list
- re-hashes every packaged file under `context/`
- fails if packaged files are missing or modified

`run` behavior:

- `--courier <name>` resolves either a built-in backend or an installed courier plugin by name
- `--registry <path>` lets `dispatch run` resolve plugins from a non-default courier registry file
- `--interactive` starts a multi-turn local chat session in the terminal
- `--session-file <path>` persists and resumes `CourierSession` state across separate CLI invocations
- `--chat <text>` sends one chat turn through the native reference courier
- `--job <payload>` executes the parcel `job` entrypoint through the native courier
- `--heartbeat [payload]` executes the parcel `heartbeat` entrypoint through the native courier
- `--print-prompt` resolves the courier prompt stack from packaged instruction files
- `--list-tools` lists declared local tools from the built parcel
- `--tool <name>` executes one declared local tool from the packaged parcel context
- required `SECRET` declarations are enforced before local tool execution
- `dispatch run` validates that the parcel courier target matches the selected backend before opening a courier session

Tool declaration behavior:

- `TOOL ... DESCRIPTION "..."` preserves authored tool guidance in the built manifest
- `TOOL LOCAL ... SCHEMA <file>` packages a JSON input schema with the tool and records its digest in the manifest
- native model-backed chat uses that description when exposing local tools to the model
- schema-backed local tools are exposed as structured function tools to the OpenAI Responses backend
- if no description is declared, Dispatch falls back to a generic packaged-path description

Framework provenance behavior:

- `FRAMEWORK <name> [VERSION <version>] [TARGET <target>]` records typed authoring metadata in the built parcel manifest
- framework provenance describes how an agent was built, not which courier executes it
- parcels can declare metadata such as `FRAMEWORK adk-rust VERSION 0.5.0 TARGET wasm`

Native courier behavior:

- if the parcel declares `MODEL <id>` and `OPENAI_API_KEY` is present, Dispatch calls the OpenAI Responses API
- declared local tools are exposed to that model-backed path as OpenAI custom tools
- custom tool calls are executed locally and their outputs are sent back to the model before the assistant reply is finalized
- tool execution is surfaced as ordered courier events during the chat turn
- backend request failures are surfaced as courier events and then fall back to the local reference reply
- otherwise it falls back to the local reference reply path
- `/prompt`, `/tools`, and `/help` are handled locally without a model call

Non-native courier behavior:

- `docker` is a courier backend for declared local tool execution via the Docker CLI
- the Docker courier can validate parcel/courier compatibility, inspect parcels, resolve prompts, list local tools, and execute `--tool`
- `wasm` is a typed component-model courier family with an explicit parcel-side `COMPONENT <path>` contract
- the WASM courier validates and loads declared Dispatch ABI components, inspects parcels, resolves prompts, and lists declared local tools
- WASM guests now execute `chat`, `job`, and `heartbeat` turns through the Dispatch WIT ABI while keeping prompt resolution and declared local tool discovery host-owned
- the repo includes a reference guest component targeting the same ABI

Courier registry behavior:

- `dispatch courier ls` lists built-in backends and installed courier plugins
- `dispatch courier inspect <name>` shows either built-in courier metadata or an installed plugin manifest
- `dispatch courier install <manifest>` installs a courier plugin manifest into the local registry
- installed JSONL courier plugins can now be executed through `dispatch run --courier <name>` and `dispatch inspect --courier <name>`
- `dispatch run` and `dispatch inspect` both support `--registry <path>` when you want to target a non-default courier registry
- external plugins execute through the stream-first JSONL plugin protocol defined in `docs/courier-plugin-protocol-v1.md`
- `crates/dispatch-courier-echo` is the in-repo reference implementation of that protocol

Depot behavior:

- `dispatch push <parcel> <reference>` publishes a built parcel into a file-backed depot
- `dispatch pull <reference>` resolves a tagged parcel reference back into local `.dispatch/parcels/`
- v1 depot references use the form `file:///absolute/path/to/depot::org/parcel:v1`
- pushed parcels are stored by digest under `blobs/parcels/<digest>/`
- pushed tags are stored under `refs/<org>/<parcel>/tags/<tag>.json`
- the first depot implementation is intentionally local/file-backed; remote depot protocols come later

Optional environment variables for the native OpenAI-backed chat path:

- `OPENAI_API_KEY` - enables live model calls
- `OPENAI_BASE_URL` - overrides the default `https://api.openai.com`

Optional environment variables for the Docker courier:

- `DISPATCH_DOCKER_BIN` - overrides the Docker CLI binary path
- `DISPATCH_DOCKER_IMAGE` - overrides the helper container image used for local tool execution

## Courier Direction

Dispatch should define a standard for packaging and executing agents, but it should not require Docker itself.

The right model is:

- `Agentfile` defines the authored build language
- Dispatch defines the parcel format and courier contract
- multiple couriers can implement that contract
- Docker/container execution is one courier
- a native courier is another courier that executes the parcel directly on the local machine as a host-process backend, without Docker or WASM isolation
- WASM/worker-style couriers are additional couriers

That keeps the standard portable.

Frameworks like ADK are useful authoring and implementation targets, but they are not courier families. An ADK-authored agent can be packaged as a Dispatch parcel and then executed through a Dispatch courier such as `native`, `docker`, or `wasm`.

For `dispatch/wasm` parcels, the compiled component is an explicit part of the parcel contract. The parcel manifest records the packaged component path, its digest, and the Dispatch WIT world/ABI it targets.

## Courier Interface

The courier/plugin boundary now lives in [`crates/dispatch-core/src/courier.rs`](crates/dispatch-core/src/courier.rs).

Core pieces:

- `CourierBackend` - trait every courier backend implements
- `CourierSession` - dispatch-owned session identity and turn state
- `CourierRequest` / `CourierResponse` - courier operation envelope
- `CourierEvent` - ordered event stream emitted for each courier turn
- `CourierCapabilities` / `CourierInspection` - backend introspection
- `ToolInvocation` / `ToolRunResult` - stable local tool execution envelope
- `MountProvider`, `MountRequest`, `ResolvedMount` - mount abstraction for session, memory, and artifacts
- `NativeCourier` - reference implementation

This is the intended extension model for:

- native local couriers
- Docker/container couriers
- WASM couriers
- remote worker couriers

For courier implementers, see:

- [`docs/courier-implementers.md`](docs/courier-implementers.md)
- [`docs/courier-plugin-protocol-v1.md`](docs/courier-plugin-protocol-v1.md)
- [`crates/dispatch-core/tests/courier_conformance.rs`](crates/dispatch-core/tests/courier_conformance.rs)

## Design Principles

- `Agentfile` is line-oriented and human-editable.
- Dispatch owns the courier/parcel contract; `Agentfile` stays the authored format.
- Markdown files remain first-class inputs, but not the primary manifest.
- State is not baked into the parcel. Sessions, memory, and artifacts are mounts.
- Tools are declared capabilities, not implicit filesystem accidents.
- A built parcel should have a digest and be runnable by parcel reference.

## Non-Goals

- Replacing Docker or OCI
- Hiding execution/security policy behind prompt text
- Treating agent memory as part of the immutable build artifact

## Initial CLI Shape

```bash
dispatch lint .
dispatch build .
dispatch inspect examples/basic/.dispatch/parcels/<digest>
dispatch run examples/basic/.dispatch/parcels/<digest> --chat "hello"
dispatch push examples/basic/.dispatch/parcels/<digest> file:///tmp/dispatch-depot::acme/market-monitor:0.1.0
dispatch pull file:///tmp/dispatch-depot::acme/market-monitor:0.1.0
```

## Why Not YAML

YAML is fine for structured config, but it is weak as a build language.

`Dockerfile` succeeded because it is:

- line-oriented
- diff-friendly
- composable
- easy to parse
- easy to teach

`Agentfile` should follow that model.

## License

Licensed under the MIT License. See [LICENSE](LICENSE).
