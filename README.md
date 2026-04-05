# Dispatch

Dispatch packages agents into verifiable parcels and runs them through pluggable couriers.

The core idea: an agent should be a self-describing, verifiable artifact - separate from the infrastructure that runs it. Build it once. Run it anywhere a courier exists.

```
Agentfile  ->  dispatch parcel build  ->  parcel (artifact)  ->  dispatch run  ->  courier
```

Available couriers today include native, Docker, WASM, and external JSONL plugins. The WASM path runs a guest component compiled against the [Dispatch WIT ABI](crates/dispatch-wasm-abi/wit/dispatch-courier.wit) in any host that implements the interface - local machine, cloud worker, edge node, or multi-tenant platform - with no container daemon required and with WebAssembly isolation by default.

## Why Dispatch

Most agent "frameworks" solve the programming problem. Dispatch solves the packaging problem.

Without a standard artifact format:

- an agent's prompt, tools, model policy, and security constraints live in ad-hoc code
- the author and the executor must share runtime assumptions
- deploying to a new environment means rewriting configuration
- verifying that what runs matches what was authored is manual or impossible

With Dispatch:

- `Agentfile` is the canonical authored spec - human-editable, diff-friendly, reviewable
- `dispatch parcel build` produces a content-addressed parcel with a verifiable manifest
- `dispatch parcel verify` re-hashes every file and checks detached signatures
- `dispatch run` selects a courier backend and executes - the parcel carries its contract with it
- couriers can be native, Docker, WASM, or custom; the parcel format is independent of which one runs it

The practical applications: deploying untrusted third-party agents in a sandboxed WASM host, running agents at the edge without a container runtime, distributing agents through a depot network with integrity guarantees, and letting authors declare model policy and tool permissions explicitly rather than via ambient prompt text.

## Vocabulary

- an `Agentfile` builds a **parcel**
- a parcel is described by `manifest.json`
- `parcel.lock` records parcel integrity metadata
- a **courier** executes a parcel
- a **depot** stores parcels
- a running parcel execution is a **dispatch**

## Agentfile

`Agentfile` is the authored build format inside Dispatch, similar to how `Dockerfile` fits inside Docker - line-oriented, diff-friendly, composable.

An agent project has:

- an `Agentfile`
- optional instruction files loaded into the agent's prompt stack:

| File | Purpose |
|---|---|
| `IDENTITY.md` | Name, role, and display metadata |
| `SOUL.md` | Persona, tone, writing style, and behavioral invariants |
| `SKILL.md` | What the agent does and how to approach tasks |
| `AGENTS.md` | Operating procedures: tool discipline, memory discipline, scope rules |
| `USER.md` | Operator context: timezone, preferences, access boundaries |
| `TOOLS.md` | When and how to use each declared tool |
| `MEMORY.md` | Memory policy: what to store, when, and in what format |
| `HEARTBEAT.md` | Procedures to execute on each scheduled run |

- optional [Agent Skills](https://agentskills.io/specification) bundles referenced with `SKILL path/to/skill-dir`
- an explicit `COMPONENT` for `dispatch/wasm` parcels
- local tools, reference assets, evals, and code

Example (`examples/parcels/basic/Agentfile`):

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

MODEL gpt-5.4-mini PROVIDER openai
FALLBACK claude-sonnet-4-6 PROVIDER anthropic

TOOL BUILTIN system_time
TOOL BUILTIN web_search
TOOL BUILTIN topic_lookup
TOOL BUILTIN human_approval
TOOL BUILTIN memory_put
TOOL BUILTIN memory_get
SECRET PLANNER_TOKEN
TOOL A2A planner URL https://planner.example.com DISCOVERY card AUTH bearer PLANNER_TOKEN EXPECT_AGENT_NAME planner-agent EXPECT_CARD_SHA256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef DESCRIPTION "Delegate planning to a remote agent."
TOOL A2A search URL https://search.example.com AUTH header X-Api-Key SEARCH_TOKEN DESCRIPTION "Call a remote search agent with header auth."
TOOL A2A backoffice URL https://backoffice.example.com AUTH basic BACKOFFICE_USER BACKOFFICE_PASSWORD DESCRIPTION "Call a remote backoffice agent with basic auth."

MOUNT SESSION sqlite
MOUNT MEMORY sqlite
MOUNT ARTIFACTS local

ENV TZ=UTC
VISIBILITY open

LIMIT ITERATIONS 20
LIMIT TOOL_CALLS 12
LIMIT TOOL_ROUNDS 8
LIMIT TOOL_OUTPUT 10000
LIMIT CONTEXT_TOKENS 16000
COMPACTION 200 OVERLAP 32
TIMEOUT RUN 300s
TIMEOUT TOOL 60s
TIMEOUT LLM 120s
EVAL evals/smoke.eval

ENTRYPOINT chat
```

`TOOL A2A` endpoints are declared in the parcel, and discovered agent cards are allowed to refine the RPC path but not pivot execution onto a different origin than the declared URL. Dispatch requires `https://` for non-loopback A2A endpoints and rejects URLs with embedded credentials; plain `http://` is only accepted for loopback development targets such as `localhost` or `127.0.0.1`. Operators can still constrain outbound calls at runtime with `DISPATCH_A2A_ALLOWED_ORIGINS`, using a comma-separated list of allowed origins or hostnames, or with `DISPATCH_A2A_TRUST_POLICY`, a TOML policy file that can match by origin/hostname and require discovered agent-card identity fields such as `expected_agent_name` and `expected_card_sha256`. Command-scoped CLI A2A policy flags override inherited environment values for that one invocation without mutating the process environment. The current `TOOL A2A` contract is synchronous: Dispatch will poll `tasks/get` for unfinished remote tasks until completion or the configured tool timeout. For the full declaration and operator model, see [docs/a2a.md](./docs/a2a.md).

`TIMEOUT RUN` is enforced as a persisted pre-turn session budget using accumulated elapsed runtime across successful runs and resumes. It does not currently preempt a turn that has already started.
`TIMEOUT TOOL` is currently enforced for host-executed local tools and host-executed A2A tool calls.

CLI-scoped A2A operator policy overrides are available on:

- `dispatch run`
- `dispatch parcel eval`
- `dispatch courier conformance`

Use `--a2a-allowed-origins ...` and `--a2a-trust-policy ...` when you want command-scoped A2A policy without exporting environment variables.
Hosted model backends also receive `TIMEOUT LLM` as an HTTP request timeout when the parcel declares it.
Timeout durations must be positive integers ending in `ms`, `s`, `m`, or `h`.

## Agent Skills Compatibility

Dispatch supports the [Agent Skills specification](https://agentskills.io/specification) as a first-class skill packaging layout.

`SKILL` accepts either:

- a markdown file such as `SKILL SKILL.md`
- a skill directory such as `SKILL skills/file-analyst`

When `SKILL` points at a directory, Dispatch expects:

- `SKILL.md` for the skill instructions
- an optional `dispatch.toml` sidecar for Dispatch-executable tool metadata
- the rest of the Agent Skills bundle layout such as `scripts/`, `references/`, and `assets/`

`SKILL.md` stays Agent Skills compliant. Dispatch-specific execution metadata lives in `dispatch.toml`, or in a sidecar path referenced by `metadata.dispatch-manifest` in the skill frontmatter.
`dispatch.toml` is a reserved filename inside skill directories: if it exists, Dispatch will try to load it as the sidecar unless frontmatter points at a different file.
If you only want to work with a skill locally, `dispatch skill validate <path>` checks that Dispatch can synthesize a parcel from a `SKILL.md` file or skill bundle directory, and `dispatch skill run <path>` executes that synthesized parcel without requiring an authored `Agentfile`.

Example skill bundle:

```text
skills/file-analyst/
|-- SKILL.md
|-- dispatch.toml
|-- scripts/
|   |-- read_file.sh
|   \-- find_files.sh
|-- schemas/
|   \-- find_files.json
\-- references/
    \-- REFERENCE.md
```

Example `Agentfile`:

```dockerfile
FROM dispatch/native:latest
NAME file-analyst-agent
SOUL SOUL.md
SKILL skills/file-analyst
MODEL claude-sonnet-4-6 PROVIDER anthropic
ENTRYPOINT chat
```

Example `dispatch.toml` sidecar:

```toml
entrypoint = "chat"

[[tools]]
name = "read_file"
script = "scripts/read_file.sh"
risk = "low"
description = "Read the full contents of a file."

[[tools]]
name = "find_files"
script = "scripts/find_files.sh"
schema = "schemas/find_files.json"
risk = "low"
description = "Find files matching a pattern."
```

Dispatch packages the whole skill directory, strips `SKILL.md` frontmatter out of the prompt text seen by the model for directory-based skill bundles, and synthesizes the sidecar tool declarations into the parcel manifest as normal local tools. File-based `SKILL path/to/file.md` instructions are left unchanged even if they happen to contain YAML frontmatter. The built parcel preserves skill annotations such as `allowed-tools` as structured lists, and skill-generated tools retain `skill_source` provenance using the skill's canonical `name`. `dispatch.toml` may also provide a default `entrypoint`, but an explicit `ENTRYPOINT` in the `Agentfile` still wins. Explicit `TOOL ...` declarations may override skill-generated tool aliases, but duplicate explicit aliases and conflicting aliases across skills fail the build.

`allowed-tools` is currently preserved as informational metadata for interoperability and downstream policy engines. The reference courier does not enforce it yet, but `dispatch parcel lint` and `dispatch parcel build` warn when a skill's `allowed-tools` entries do not line up with synthesized or declared tool aliases.


## WASM Courier

Dispatch includes a WASM courier for parcels that package a guest component targeting the Dispatch
WIT ABI.

A `dispatch/wasm` parcel contains a WASM component compiled against the Dispatch WIT ABI:

```wit
// dispatch:courier@0.0.1 - full definition in crates/dispatch-wasm-abi/wit/
interface host {
    model-complete: func(request: model-request) -> result<model-response, string>;
    invoke-tool:    func(invocation: tool-invocation) -> result<tool-result, string>;
    memory-get:     func(namespace: string, key: string) -> result<option<memory-entry>, string>;
    memory-put:     func(namespace: string, key: string, value: string) -> result<bool, string>;
    memory-delete:  func(namespace: string, key: string) -> result<bool, string>;
    memory-list:    func(namespace: string, prefix: option<string>) -> result<list<memory-entry>, string>;
}
```

The guest component implements `open-session` and `handle-operation`. The host owns:

- **model routing** - the guest can request a model ID, but provider and API key selection come from the parcel manifest and host environment
- **tool execution** - the host invokes declared local tools; the guest cannot access tools outside the parcel manifest
- **memory** - the host provides durable parcel-scoped sqlite storage; the guest sees it as a namespace/key/value API
- **sandboxing** - WASM memory isolation applies by default; the guest cannot access host resources unless the host imports them

This separation enables:

- running untrusted third-party agent components with bounded resource access
- edge and serverless deployment with no container daemon
- multi-tenant agent execution on a shared host
- auditing what a guest can actually do based on the parcel manifest and WIT imports, not inferred from prompt text

The `dispatch-wasm-guest-reference` crate shows how to build a guest component with multi-round tool calling, `previous_response_id` chain management, and session state. Any language that compiles to WASM with WIT component support can implement a guest.

The reference WASM courier keeps a bounded in-process component cache keyed by component SHA256. Override the cache size with `DISPATCH_WASM_COMPONENT_CACHE_SIZE` if you need a smaller or larger warm set.

## Getting Started

Build and run the reference examples:

```bash
# Lint an Agentfile
cargo run -p dispatch -- parcel lint examples/parcels/basic
cargo run -p dispatch -- parcel lint examples/parcels/wasm-reference
cargo run -p dispatch -- parcel lint examples/skills/file-analyst

# Build a parcel
cargo run -p dispatch -- parcel build examples/parcels/basic
cargo run -p dispatch -- parcel build examples/parcels/wasm-reference
cargo run -p dispatch -- parcel build examples/skills/file-analyst

# Run packaged evals
cargo run -p dispatch -- parcel eval examples/parcels/basic
cargo run -p dispatch -- parcel eval examples/parcels/basic --courier native
cargo run -p dispatch -- parcel eval examples/skills/file-analyst --courier native

# Inspect a built parcel
cargo run -p dispatch -- parcel inspect examples/parcels/basic/.dispatch/parcels/<digest>
cargo run -p dispatch -- parcel inspect examples/parcels/wasm-reference/.dispatch/parcels/<digest> --courier wasm

# Verify parcel integrity
cargo run -p dispatch -- parcel verify examples/parcels/basic/.dispatch/parcels/<digest>

# Sign a parcel
cargo run -p dispatch -- parcel keygen --key-id release --output-dir .dispatch/keys
cargo run -p dispatch -- parcel sign examples/parcels/basic/.dispatch/parcels/<digest> --secret-key .dispatch/keys/release.dispatch-secret.json
cargo run -p dispatch -- parcel verify examples/parcels/basic/.dispatch/parcels/<digest> --public-key .dispatch/keys/release.dispatch-public.json

# Run a parcel (native courier, requires LLM_API_KEY or provider env vars)
cargo run -p dispatch -- run examples/parcels/basic/.dispatch/parcels/<digest> --chat "hello"
cargo run -p dispatch -- run examples/parcels/basic/.dispatch/parcels/<digest> --interactive

# Run a skill bundle directly without authoring an Agentfile
cargo run -p dispatch -- skill validate examples/skills/file-analyst/skills/file-analyst
cargo run -p dispatch -- skill run examples/skills/file-analyst/skills/file-analyst --list-tools
cargo run -p dispatch -- skill run examples/skills/file-analyst/skills/file-analyst --model gpt-5-mini --provider openai --chat "Summarize this repository."

# Run a WASM parcel
cargo run -p dispatch -- run examples/parcels/wasm-reference/.dispatch/parcels/<digest> --courier wasm --chat "hello"

# Run a heartbeat
cargo run -p dispatch -- run examples/parcels/heartbeat-monitor/.dispatch/parcels/<digest> --heartbeat

# List and invoke tools
cargo run -p dispatch -- run examples/parcels/heartbeat-monitor/.dispatch/parcels/<digest> --list-tools
cargo run -p dispatch -- run examples/parcels/heartbeat-monitor/.dispatch/parcels/<digest> --tool poll_mentions

# Push/pull to a depot
dispatch depot push examples/parcels/basic/.dispatch/parcels/<digest> file:///tmp/dispatch-depot::acme/basic:0.1.0
dispatch depot pull file:///tmp/dispatch-depot::acme/basic:0.1.0
dispatch depot push examples/parcels/basic/.dispatch/parcels/<digest> file:///tmp/dispatch-depot::acme/basic:0.1.0 --json
dispatch depot pull file:///tmp/dispatch-depot::acme/basic:0.1.0 --json
dispatch depot push examples/parcels/basic/.dispatch/parcels/<digest> https://depot.example.com::acme/basic:0.1.0
dispatch depot pull https://depot.example.com::acme/basic:0.1.0
dispatch depot pull https://depot.example.com::acme/basic:0.1.0 --public-key .dispatch/keys/release.dispatch-public.json
dispatch depot pull https://depot.example.com::acme/basic:0.1.0 --trust-policy trust-policy.toml
```

Print the parsed AST:

```bash
cargo run -p dispatch -- parcel lint examples/parcels/basic --json
```

## Parcel Format

A built parcel contains:

- `manifest.json` - typed parcel manifest with `$schema` pointer
- `parcel.lock` - file and digest integrity metadata
- `context/` - packaged build content referenced by the `Agentfile`
- `signatures/<key_id>.json` - detached Ed25519 signatures (optional)

The manifest is described by [`schemas/parcel.v1.json`](schemas/parcel.v1.json).
Schema publication and compatibility policy live in [`docs/schema-compatibility.md`](docs/schema-compatibility.md).

Packaged eval files live under `context/` with the other authored inputs. A minimal eval file looks like:

```toml
name = "smoke"
input = "What time is it?"
expects_tool = "system_time"
expects_text_contains = "plugin reply"
```

Eval files can also group multiple cases:

```toml
[[cases]]
name = "smoke"
input = "What time is it?"
expects_tool = "system_time"

[[cases]]
name = "exact"
input = "What time is it?"
expects_tool_count = 1
expects_tool_stdout_contains = { tool = "system_time", contains = "2026-04-03" }
expects_text_exact = "plugin reply"
```

`dispatch parcel eval` runs packaged `EVAL` cases and `TEST` tool smoke checks against a live courier and reports pass/fail per case.
Tool result assertions can be either a plain value or a tool-scoped object, so multi-tool evals can target one tool explicitly.
`expects_no_tool = true` can be used for cases that should complete without invoking any tool.
`expects_tool_stdout_matches_schema` validates JSON stdout from a tool against a packaged JSON schema file, and `expects_a2a_endpoint` asserts that an A2A tool alias resolved to the expected declared endpoint.

Parcel format compatibility:

- `load_parcel` validates `manifest.json` against the bundled Dispatch JSON Schema before parsing
- the reference implementation supports exactly `format_version: 1`
- couriers must reject parcels whose `$schema` or `format_version` they do not support
- published schema URLs are immutable; new manifest-shape changes require a new schema URL and `format_version`

`verify` behavior:

- recomputes the parcel manifest digest from normalized manifest content
- validates `parcel.lock` digest, layout metadata, and file list
- re-hashes every packaged file under `context/`
- optionally verifies detached Ed25519 signatures with `--public-key <path>`
- fails if packaged files are missing or modified

## Native Courier

The native courier runs the parcel directly on the local machine as a host process with a model-backed chat loop.

Model backend selection:

- if the parcel declares `MODEL <id> PROVIDER <backend>`, that provider is used
- if no parcel-level provider, `LLM_BACKEND` selects the backend: `openai`, `anthropic`, `gemini`, `openai_compatible`, `codex`
- `FALLBACK <id> [PROVIDER <backend>]` entries are tried in order when the primary backend fails before producing a reply

Supported backends:

| Backend | API | Environment |
|---------|-----|-------------|
| `openai` | OpenAI Responses API | `OPENAI_API_KEY` |
| `anthropic` | Anthropic Messages API | `ANTHROPIC_API_KEY` |
| `gemini` | Gemini generateContent | `GEMINI_API_KEY` or `GOOGLE_API_KEY` |
| `openai_compatible` | Chat Completions | `LLM_API_KEY` + `LLM_BASE_URL` |
| `codex` | Local `codex app-server` JSON-RPC transport | optional `CODEX_BINARY`, `CODEX_HOME`, `CODEX_REASONING_EFFORT` |

`LLM_API_KEY` and `LLM_BASE_URL` take precedence over provider-specific vars. Provider-specific vars (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, etc.) are checked as fallbacks when the `LLM_*` vars are not set.

`codex` uses the local `codex app-server` process instead of a hosted HTTP API. Dispatch starts a local app-server session in the parcel `context/` directory, persists the Codex thread id in `CourierSession.backend_state`, and resumes that thread on later turns. To preserve Dispatch's capability boundary, app-server permission requests are denied by default in this backend, so ambient Codex command/file/MCP actions are not available unless Dispatch grows an explicit tool bridge for them. This backend intentionally inherits the parent process environment so local Codex auth and config keep working. On Unix the reference implementation uses a PTY-backed transport for Codex; other targets currently fall back to plain process pipes.

`run` flags:

- `--interactive` - multi-turn chat session in the terminal
- `--session-file <path>` - persist and resume session state across invocations
- `--chat <text>` - single chat turn
- `--job <payload>` - execute the parcel `job` entrypoint
- `--heartbeat [payload]` - execute the parcel `heartbeat` entrypoint
- `--print-prompt` - resolve and print the courier prompt stack
- `--list-tools` - list declared tools
- `--json` - when combined with `--list-tools`, print full tool metadata as JSON
- `--tool <name>` - execute one declared local tool
- `--tool-approval <ask|always|never>` - control how `APPROVAL confirm` tools are handled at the CLI
- `/prompt`, `/tools`, `/help` - handled locally during interactive sessions

`dispatch skill validate` and `dispatch skill run` are convenience wrappers over the same build path. They copy the referenced `SKILL.md` file or skill bundle into a temporary workspace, synthesize a minimal `Agentfile`, and run the same synthesis and parcel build that an authored `Agentfile` would use. `dispatch skill validate` stops after that build-time validation, while `dispatch skill run` then delegates to `dispatch run`. This means `validate` surfaces sidecar, frontmatter, packaging, and build errors directly and is suitable for CI, but it is intentionally heavier than a schema-only lint. The current shortcuts support built-in `native` and `docker` couriers and accept `--model`, `--provider`, and `--entrypoint` overrides for the synthesized parcel.

## Courier Architecture

Dispatch defines the parcel format and courier contract. Multiple couriers can implement that contract:

- **native** - executes the parcel as a host process with model-backed chat; reference implementation in `crates/dispatch-core`
- **docker** - keeps session state, mounts, and model orchestration on the host, while running declared local tools inside Docker as an execution sandbox
- **wasm** - typed component-model courier using the Dispatch WIT ABI; see [WASM Courier](#wasm-courier)
- **plugins** - external JSONL courier plugins launched as subprocesses; protocol in `docs/courier-plugin-protocol.md`

The courier/plugin boundary lives in [`crates/dispatch-core/src/courier.rs`](crates/dispatch-core/src/courier.rs).

Core traits and types:

- `CourierBackend` - trait every courier backend implements
- `CourierSession` - dispatch-owned session identity and turn state
- `CourierRequest` / `CourierResponse` - courier operation envelope
- `CourierEvent` - ordered event stream emitted per turn
- `CourierCapabilities` / `CourierInspection` - backend introspection
- `MountProvider`, `MountRequest`, `ResolvedMount` - mount abstraction

For courier implementers:

- [`docs/schema-compatibility.md`](docs/schema-compatibility.md)
- [`docs/courier-implementers.md`](docs/courier-implementers.md)
- [`docs/courier-plugin-protocol.md`](docs/courier-plugin-protocol.md)
- [`crates/dispatch-core/tests/courier_conformance.rs`](crates/dispatch-core/tests/courier_conformance.rs)

Courier registry:

- `dispatch parcel lint|build|inspect|verify|keygen|sign` - manage parcel sources, signatures, and built artifacts
- `dispatch depot push|pull` - move parcels to and from depots
- `dispatch courier ls` - list built-in backends and installed plugins
- `dispatch courier inspect <name>` - show courier metadata
- `dispatch courier install <manifest>` - install a plugin manifest
- `dispatch courier conformance <name>` - run the public courier contract checks against one backend
- `dispatch courier conformance <name> --json` - emit the same conformance report as machine-readable JSON
- `dispatch run --courier <name>` - select a backend by name
- `dispatch run --registry <path>` - use a non-default courier registry
- plugin installation records the executable SHA256; Dispatch checks that digest before each launch

## Mounts

State is not baked into the parcel. Sessions, memory, and artifacts are mounts declared in the `Agentfile`.

- `MOUNT SESSION sqlite` - session-scoped sqlite; persists `CourierSession` state per turn
- `MOUNT MEMORY sqlite` - parcel-scoped sqlite; exposes `memory_get`, `memory_put`, `memory_delete`, `memory_list` to model-backed turns
- `MOUNT ARTIFACTS local` - parcel-scoped artifact storage

State layout:

- parcels opened from a normal build tree: `.dispatch/state/<digest>/`
- parcels at custom locations: `<parcel-parent>/.dispatch-state/<digest>/`
- `DISPATCH_STATE_ROOT` overrides the state root completely

State management:

- `dispatch state ls` - list digest-scoped state directories
- `dispatch state gc` - remove orphaned state for parcels no longer present
- `dispatch state migrate <old> <new>` - copy state when a rebuilt parcel gets a new digest

## Depot

- `dispatch depot push <parcel> <reference>` - publish a parcel into a depot
- `dispatch depot pull <reference>` - resolve a tagged reference into `.dispatch/parcels/`
- `dispatch depot push ... --json` / `dispatch depot pull ... --json` - emit machine-readable depot results
- `dispatch depot pull <reference> --public-key <path>` - require detached signature verification during fetch
- `dispatch depot pull <reference> --trust-policy <path>` - apply pull-time trust rules during fetch
- trust, provenance, and depot operator guidance live in [`docs/trust-and-depots.md`](docs/trust-and-depots.md)
- v1 depot references include:
- `file:///absolute/path/to/depot::org/parcel:v1`
- `https://depot.example.com::org/parcel:v1`
- file depots store parcels by digest under `blobs/parcels/<digest>/`
- file depots store tags under `refs/<org>/<parcel>/tags/<tag>.json`
- HTTP depots expose parcel blobs at `/v1/parcels/<digest>.tar` and tag lookup at `/v1/tags?repository=<repo>&tag=<tag>`
- set `DISPATCH_DEPOT_TOKEN` to send `Authorization: Bearer <token>` on HTTP depot requests
- set `DISPATCH_TRUST_POLICY` to apply a default pull-time trust policy without passing `--trust-policy`
- trust policy files are TOML documents with `rules`, optional `reference_prefix`, optional `repository_prefix`, `public_keys`, and optional `require_signatures`
- each trust-policy rule must set at least one matcher: `reference_prefix`, `repository_prefix`, or both
- if a rule sets both prefixes, both must match for the rule to apply
- matching rules compose:
  - `require_signatures` is enabled if any matching rule requires it
  - `public_keys` from matching rules are merged and deduplicated
- `--public-key` composes with `--trust-policy`; explicit keys are added to any matching policy keys
- trust-policy verification happens before a pulled parcel is committed into the local parcel store
- `FRAMEWORK` metadata is informational provenance, not a trust root; use signatures and trust policy for publisher authorization

## Design Principles

- `Agentfile` is line-oriented and human-editable.
- Dispatch owns the courier/parcel contract; `Agentfile` stays the authored format.
- State is not baked into the parcel. Sessions, memory, and artifacts are mounts.
- Tools are declared capabilities, not implicit filesystem accidents.
- A courier must not advertise or execute undeclared tools based on ambient prompt text.
- A built parcel should have a digest and be runnable by reference.
- Couriers must reject parcels whose format version or schema they do not support.

## Non-Goals

- Replacing Docker or OCI
- Hiding execution or security policy behind prompt text
- Treating agent memory as part of the immutable build artifact
- Requiring any specific agent framework or language runtime

## License

Licensed under the MIT License. See [LICENSE](LICENSE).
