# Dispatch Agentfile Specification

## Overview

Dispatch is a packaging and courier standard for agent parcels.

`Agentfile` is the declarative, Dockerfile-style build language used to author Dispatch parcels.

An `Agentfile` defines:

- the base courier target
- the instruction stack
- the tool surface
- state mounts
- model and routing defaults
- guardrails
- eval gates
- entrypoint behavior

The output of `dispatch build` is an immutable **agent parcel** with a digest.

## Build Model

### Inputs

Build context:

- `Agentfile`
- referenced markdown files
- local tools
- code
- reference assets
- eval definitions

### Outputs

The build produces:

- normalized manifest
- resolved instruction stack
- packaged tool bundle
- asset bundle
- typed parcel manifest
- parcel digest
- optional lockfile

Implementations should provide a verification path that can recompute the manifest digest and validate packaged file hashes against the built parcel metadata.

Format compatibility:

- every parcel declares a `$schema` URL and an integer `format_version`
- couriers must validate parcels against the schema they claim to support before execution
- couriers must reject parcels with unsupported schema URLs or format versions
- the Dispatch reference implementation currently supports `format_version: 1`

### Parcel vs Mounts

Immutable parcel content:

- prompts and instruction files
- tool declarations
- local tool files
- static assets
- defaults and policy

State mounts:

- session state
- long-term memory
- artifact storage
- secrets

This is the same split Docker makes between image layers and volumes.

## Courier Contract

The standard should not stop at the parcel artifact. It also needs a courier contract so third parties can implement compatible executors.

At minimum, a courier must be able to:

- load an agent parcel
- inspect capabilities and requirements
- open a courier session for a specific parcel
- resolve instruction files into a courier prompt stack
- resolve and enforce mounts
- execute declared tools
- handle entrypoint modes like `chat`, `job`, and `heartbeat`
- emit ordered events for each turn so interactive couriers can stream output incrementally

In the Rust reference implementation, this boundary is represented by:

- `CourierBackend`
- `CourierSession`
- `CourierRequest`
- `CourierResponse`
- `CourierEvent`
- `CourierCapabilities`
- `CourierInspection`
- `MountProvider`
- `ToolInvocation`

This is the core execution contract for a Dispatch courier.

Resolved prompt/tool invariant:

- instruction files contribute prompt content only after they are packaged into the parcel
- tool exposure is driven by declared tool entries in the parcel manifest
- prompt text must not be treated as authority to expose undeclared tools

The current native courier implements prompt resolution, local tool execution, and reference `chat`, `job`, and `heartbeat` entrypoints that preserve session history and emit ordered courier events. When a primary model is declared and provider credentials are available, the native courier may delegate turns to a hosted model backend, expose declared local tools plus the supported built-in memory tools to that backend, execute returned tool calls locally, and resume the model turn with tool outputs. `MODEL <id> PROVIDER <backend>` selects a parcel-level backend explicitly; otherwise the courier falls back to `LLM_BACKEND`. If no parcel model is declared, Dispatch falls back to `LLM_MODEL`. `FALLBACK` models are tried in declaration order when the hosted-model request fails before producing a reply. The same primary-plus-fallback model policy is also used by the WASM host when a guest calls `model-complete`. Without a usable hosted-model configuration it falls back to a local reference reply path.

### Pluggable Courier Model

An agent parcel should be portable across multiple couriers:

- `native`
- `docker`
- `wasm`
- `custom`

That means the parcel format must remain courier-agnostic.

The courier is an implementation detail. The parcel contract is the standard.

CLI implementations may expose explicit courier selection so the same built parcel can be validated or executed against different backends without changing the parcel itself.

## Syntax

`Agentfile` is line-oriented:

- one instruction per line
- `#` starts a comment
- instruction names are uppercase
- arguments are space-separated
- quoted strings are allowed
- multi-line bodies use heredoc blocks

Example:

```dockerfile
PROMPT <<EOF
You are a careful market monitoring agent.
Use tools before answering with live data.
EOF
```

## Core Instructions

### Base

#### `FROM`

Selects the target courier family for the built parcel.

```dockerfile
FROM dispatch/native:latest
FROM dispatch/wasm:0.1
```

Semantics:

- required unless the builder injects a default
- establishes courier compatibility
- is normalized into `courier.reference` in the built parcel manifest
- may define default toolchains and instruction loaders
- couriers should reject execution when `courier.reference` is incompatible with the selected backend

#### `NAME`

```dockerfile
NAME market-monitor
```

#### `VERSION`

```dockerfile
VERSION 0.1.0
```

#### `FRAMEWORK`

Records optional authoring or toolchain provenance in the built parcel manifest.

```dockerfile
FRAMEWORK adk-rust
FRAMEWORK adk-rust VERSION 0.5.0 TARGET wasm
```

Semantics:

- optional
- normalized into top-level `framework` metadata in `manifest.json`
- describes how the parcel was authored or compiled
- does not affect courier compatibility or backend selection

#### `COMPONENT`

Declares the packaged WebAssembly component for `dispatch/wasm` parcels.

```dockerfile
COMPONENT components/assistant.wasm
```

Semantics:

- only valid for `dispatch/wasm` courier targets
- packages the component into the parcel `context/`
- is normalized into `courier.component` in the built manifest
- records the packaged component digest plus the Dispatch courier ABI the component targets
- makes the guest binary an explicit part of the parcel contract instead of relying on implicit file conventions
- the `dispatch/wasm` courier executes `chat`, `job`, and `heartbeat` by calling into that packaged guest component through the Dispatch WIT ABI

#### `LABEL`

```dockerfile
LABEL org.opencontainers.image.source="github.com/acme/market-monitor"
LABEL ai.agent.category="finance"
```

### Instruction Stack

These instructions attach structured markdown or inline prompt content.

#### `SOUL`

Persona, behavioral invariants, style, and constitutional rules.

```dockerfile
SOUL SOUL.md
```

#### `SKILL`

Primary task instructions and operating playbook.

```dockerfile
SKILL SKILL.md
SKILL skills/file-analyst
```

Semantics:

- accepts either a markdown file or an [Agent Skills](https://agentskills.io/specification) directory
- when the argument is a file, Dispatch packages it as a normal skill instruction
- when the argument is a directory, Dispatch requires `SKILL.md` in that directory
- `SKILL.md` frontmatter is parsed for Agent Skills metadata and stripped from the prompt text seen by the model
- the rest of the directory is packaged with the parcel so `scripts/`, `references/`, and `assets/` travel with the skill bundle
- if the skill directory contains `dispatch.toml`, or `SKILL.md` frontmatter sets `metadata.dispatch-manifest = "..."`, Dispatch loads Dispatch-specific tool metadata from that TOML sidecar and synthesizes those entries into the parcel as normal local tools
- the sidecar may also declare a default `entrypoint = "chat" | "job" | "heartbeat"` when the `Agentfile` does not set `ENTRYPOINT`
- built parcels preserve skill annotations such as `allowed-tools` as structured lists, and skill-generated tools keep `skill_source` provenance using the skill's canonical `name`
- if the `Agentfile` later declares `TOOL ...` with the same alias as a skill-generated tool, the explicit `TOOL` declaration wins
- if multiple skills declare the same tool alias, Dispatch fails the build instead of picking one silently

Recommended Agent Skills-compatible layout:

```text
skills/file-analyst/
|-- SKILL.md
|-- dispatch.toml
|-- scripts/
|-- references/
|-- assets/
\-- schemas/
```

Example `SKILL.md`:

```markdown
---
name: file-analyst
description: Analyze files and directories.
metadata:
  dispatch-manifest: dispatch.toml
---

Use the file tools before answering.
```

Example `dispatch.toml`:

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

#### `IDENTITY`

Short public identity and display metadata for the agent.

```dockerfile
IDENTITY IDENTITY.md
```

This is optional when `NAME` and `LABEL` are enough, but supported so Dispatch can package common workspace layouts without losing meaning.

#### `AGENTS`

Operating procedures, workflows, routing rules, and rules of engagement.

```dockerfile
AGENTS AGENTS.md
```

#### `USER`

Operator or owner context such as preferences, timezone, access boundaries, or other private working assumptions.

```dockerfile
USER USER.md
```

This file often contains personal or sensitive context. Treat it as private workspace input, not default-public project metadata.

#### `TOOLS`

Human-authored guidance for how the agent should use its available tools.

```dockerfile
TOOLS TOOLS.md
```

#### `HEARTBEAT`

Declares scheduled or recurring execution semantics.

```dockerfile
HEARTBEAT EVERY 30s FILE HEARTBEAT.md
HEARTBEAT CRON "*/5 * * * *" FILE HEARTBEAT.md
```

#### `MEMORY`

Declares memory policy, not the memory contents themselves.

```dockerfile
MEMORY POLICY MEMORY.md
```

#### `PROMPT`

Adds inline prompt text.

```dockerfile
PROMPT "Prefer concise responses."
```

or:

```dockerfile
PROMPT <<EOF
Prefer concise responses.
Do not claim live data without a tool call.
EOF
```

### Models

#### `MODEL`

```dockerfile
MODEL gpt-5.4-mini
MODEL claude-sonnet-4-6 PROVIDER anthropic
```

#### `FALLBACK`

```dockerfile
FALLBACK gpt-5.4-nano
FALLBACK claude-sonnet-4-6 PROVIDER anthropic
```

Fallback models are attempted in declaration order when the primary hosted-model request fails before producing a reply. A fallback may use a different provider than the primary model.

#### `ROUTING`

```dockerfile
ROUTING balanced
ROUTING deep
ROUTING fast
```

`ROUTING` is stored as parcel metadata. The reference implementation does not yet enforce routing-specific model selection behavior.

### Tools

Tools must be explicitly declared.

#### `TOOL LOCAL`

```dockerfile
TOOL LOCAL tools/fetch_price.py AS fetch_price
TOOL LOCAL tools/browser.ts AS browse APPROVAL confirm RISK medium
TOOL LOCAL tools/report.py AS report USING python3 -u
TOOL LOCAL tools/report.py AS report USING python3 -u DESCRIPTION "Generate a report from JSON input."
TOOL LOCAL tools/report.py AS report SCHEMA schemas/report.json DESCRIPTION "Generate a report from structured JSON input."
```

Supported clauses:

- `AS <alias>`
- `USING <command> [args...]`
- `APPROVAL <policy>` where policy is `never`, `always`, `confirm`, or `audit`
- `RISK <level>` where level is `low`, `medium`, or `high`
- `DESCRIPTION "..."` for model/tooling guidance
- `SCHEMA <file>` to package a JSON input schema for structured tool invocation

#### `TOOL BUILTIN`

```dockerfile
TOOL BUILTIN web_search
TOOL BUILTIN human_approval APPROVAL audit RISK medium DESCRIPTION "Request a human approval with an audit trail."
TOOL BUILTIN memory_put DESCRIPTION "Store a durable profile fact."
TOOL BUILTIN memory_get DESCRIPTION "Load a durable profile fact."
```

Native courier note:

- the native reference courier currently host-implements `memory_get`, `memory_put`, `memory_delete`, and `memory_list` for model-backed turns when a parcel declares `MOUNT MEMORY sqlite`
- other builtin capabilities remain declarative until a courier provides a concrete implementation

#### `TOOL MCP`

```dockerfile
TOOL MCP slack
TOOL MCP github APPROVAL confirm RISK high DESCRIPTION "Use the GitHub MCP server for repository operations."
```

#### `TOOL A2A`

Declares a remote agent-to-agent tool backed by a fixed A2A endpoint.

```dockerfile
TOOL A2A planner URL https://planner.example.com DESCRIPTION "Delegate planning to a remote agent."
TOOL A2A broker URL https://broker.example.com DISCOVERY card AUTH bearer BROKER_TOKEN EXPECT_AGENT_NAME broker-agent EXPECT_CARD_SHA256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef SCHEMA schemas/broker-input.json APPROVAL confirm RISK high
```

Reference implementation notes:

- the endpoint is declared statically in the parcel via `URL`, not supplied by the model at call time
- `DISCOVERY auto|card|direct` controls endpoint resolution:
  - `auto` tries `/.well-known/agent.json` and falls back to `<url>/a2a`
  - `card` requires successful agent-card discovery
  - `direct` skips discovery and normalizes the endpoint as a direct JSON-RPC target
- non-loopback A2A endpoints must use `https://`; plain `http://` is accepted only for loopback development targets like `localhost` or `127.0.0.1`
- A2A URLs must not embed credentials; use `AUTH ...` with a declared `SECRET` instead
- `AUTH bearer <secret_name>` sends `Authorization: Bearer ...` using a declared `SECRET`
- `AUTH header <header_name> <secret_name>` sends the secret value in the named HTTP header for both discovery and RPC calls
- `AUTH basic <username_secret_name> <password_secret_name>` sends `Authorization: Basic ...` using two declared `SECRET`s
- `EXPECT_AGENT_NAME <name>` fails the call if discovered agent-card identity does not match, or if card discovery succeeds without a `name`
- `EXPECT_CARD_SHA256 <digest>` pins the discovered agent card body to a specific lowercase SHA256 digest
- discovered agent cards may refine the RPC path, but they cannot pivot execution onto a different origin than the declared `URL`
- operators can constrain resolved A2A URLs at runtime with `DISPATCH_A2A_ALLOWED_ORIGINS`, using a comma-separated allowlist of origins or hostnames
- operators can also enforce structured outbound A2A policy with `DISPATCH_A2A_TRUST_POLICY`, a TOML file whose rules match by `origin_prefix` and/or `hostname` and can require discovered agent-card `expected_agent_name` / `expected_card_sha256`
- `dispatch run`, `dispatch eval`, and `dispatch courier conformance` also accept `--a2a-allowed-origins` and `--a2a-trust-policy` for command-scoped operator overrides
- `TOOL A2A` currently exposes a synchronous request/response tool surface; when `message/send` returns an unfinished task, Dispatch polls `tasks/get` until completion or the configured tool timeout
- task polling and cancellation are not part of the current Dispatch tool contract

See [a2a.md](./a2a.md) for the full operator and trust model.

### Files and Assets

#### `COPY`

Copies files from build context into the parcel.

```dockerfile
COPY refs/ /app/refs/
COPY tools/ /app/tools/
```

#### `ADD`

Reserved for remote or archive-aware expansion. Optional in v1.

### Courier Policy

#### `ENV`

```dockerfile
ENV TZ=UTC
ENV LOG_LEVEL=info
```

#### `SECRET`

Declares a required execution secret.

```dockerfile
SECRET OPENAI_API_KEY
SECRET DATABASE_URL
```

#### `NETWORK`

```dockerfile
NETWORK none
NETWORK publishers-only
NETWORK allow api.example.com
```

#### `VISIBILITY`

```dockerfile
VISIBILITY open
VISIBILITY opaque
```

#### `TIMEOUT`

```dockerfile
TIMEOUT RUN 300s
TIMEOUT TOOL 60s
TIMEOUT LLM 120s
```

The reference implementation currently enforces `TIMEOUT RUN` as a persisted pre-turn session budget using accumulated elapsed runtime across successful runs and resumes.
It does not currently preempt a turn that has already started.
`TIMEOUT TOOL` is enforced for host-executed local tools and host-executed `TOOL A2A` calls.
Hosted model backends also receive `TIMEOUT LLM` as an HTTP request timeout when the parcel declares it.
Timeout durations must be positive integers ending in `ms`, `s`, `m`, or `h`.

#### `LIMIT`

```dockerfile
LIMIT ITERATIONS 20
LIMIT TOOL_CALLS 12
LIMIT TOOL_OUTPUT 10000
LIMIT CONTEXT_TOKENS 16000
```

Hosted model backends may use `LIMIT CONTEXT_TOKENS` when the provider supports an explicit token budget field. The reference implementation currently applies it to Anthropic `max_tokens`.

#### `COMPACTION`

```dockerfile
COMPACTION 200
COMPACTION 200 OVERLAP 32
```

`COMPACTION` declares parcel-level event/session compaction policy as framework-neutral metadata.
Dispatch stores it in the parcel manifest but does not impose compaction behavior by itself.
Execution environments can use it to tune long-running session history compaction.

### Mounts

Mounts are state backends, similar to volumes.

#### `MOUNT SESSION`

```dockerfile
MOUNT SESSION memory
MOUNT SESSION sqlite
MOUNT SESSION postgres
```

Semantics:

- `memory` is process-local session state with no durable backing
- `sqlite` is a durable session mount; built-in couriers persist `CourierSession` state into the resolved sqlite database when a session opens and after each turn
- unsupported drivers must fail when the courier opens a session instead of being silently ignored

#### `MOUNT MEMORY`

```dockerfile
MOUNT MEMORY none
MOUNT MEMORY sqlite
MOUNT MEMORY pgvector
```

Semantics:

- `none` means no durable long-term memory backend
- `sqlite` resolves a parcel-scoped local durable memory database for built-in courier memory APIs
- `pgvector` is a declared remote memory backend target for couriers that support it
- guests and built-in memory helpers must fail explicitly when no usable memory mount is declared
- unsupported memory drivers must fail when the courier opens a session instead of being silently ignored

#### `MOUNT ARTIFACTS`

```dockerfile
MOUNT ARTIFACTS local
MOUNT ARTIFACTS s3
```

Semantics:

- `local` resolves to a parcel-scoped local artifacts directory for built-in couriers
- other artifact drivers are courier-specific and must fail fast when unsupported

Built-in courier state root:

- when a parcel is opened from the standard `.dispatch/parcels/<digest>/` layout, built-in courier state is stored under `.dispatch/state/<digest>/`
- when a parcel is opened from a custom location, built-in courier state is stored under `<parcel-parent>/.dispatch-state/<digest>/`
- `DISPATCH_STATE_ROOT` overrides the built-in courier state root completely

### Evaluation

#### `EVAL`

```dockerfile
EVAL evals/smoke.eval
EVAL evals/safety.eval REQUIRED
```

Minimal eval file:

```toml
name = "smoke"
input = "What time is it?"
expects_tool = "system_time"
expects_text_contains = "plugin reply"
```

Multi-case eval file:

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

[[cases]]
name = "invalid-entrypoint"
input = ""
entrypoint = "unsupported"
expects_error_contains = "unsupported eval entrypoint"
```

Supported fields in the reference runner:

- `name`
- `input`
- `entrypoint` (`chat`, `job`, or `heartbeat`; defaults to the parcel entrypoint or `chat`)
- `expects_tool`
- `expects_tools`
- `expects_tool_count`
- `expects_tool_stdout_contains`
- `expects_tool_stderr_contains`
- `expects_tool_exit_code`
- `expects_text_contains`
- `expects_text_exact`
- `expects_text_not_contains`
- `expects_error_contains`

Tool result assertions accept either:

- a plain value, for example `expects_tool_exit_code: 0`
- a tool-scoped object, for example:

```toml
expects_tool_stdout_contains = { tool = "system_time", contains = "2026-04-03" }
```

Run packaged evals with:

```bash
dispatch eval <parcel-or-source>
dispatch eval <parcel-or-source> --courier wasm
```

#### `TEST`

Reserved for local build verification commands.

```dockerfile
TEST tool:fetch_price
```

### Entrypoint

#### `ENTRYPOINT`

Defines how the agent is invoked.

```dockerfile
ENTRYPOINT chat
ENTRYPOINT heartbeat
ENTRYPOINT job
ENTRYPOINT http
```

## Resolution Order

The resolved prompt stack is deterministic:

1. prompt-bearing instruction files are appended in the order they appear in the authored `Agentfile`
2. inline `PROMPT` bodies are appended after the packaged instruction files
3. courier-specific system supplements, if any, are injected after the parcel-owned prompt stack

In the reference implementation, the prompt-bearing instruction kinds are:

- `IDENTITY`
- `SOUL`
- `SKILL`
- `AGENTS`
- `USER`
- `TOOLS`
- `MEMORY`
- `HEARTBEAT`

`EVAL` files are packaged but omitted from the runtime prompt stack.

## Build-Time Validation

`dispatch build` should fail if:

- referenced files are missing
- a declared local tool does not exist
- a built-in tool is unknown
- a required secret is malformed at deploy time
- `HEARTBEAT` exists without a schedulable entry mode
- incompatible mounts are declared
- unsupported instructions are used by the selected `FROM`

## Normalized Parcel Manifest

Every built parcel should expose a normalized courier config that any backend can consume:

- parcel digest
- instruction stack
- resolved tools
- policy
- entrypoint
- mount requirements
- env and secret declarations

This normalized config is the bridge into:

- local runner
- container courier
- worker courier
- sandbox courier
- control plane deployment systems

## Layering

Layering should exist, but not dominate v1.

Supported pattern:

```dockerfile
FROM acme/research-base:1
SKILL market/SKILL.md
MODEL gpt-5.4-mini PROVIDER openai
```

The derived parcel can override:

- prompt files
- model defaults
- tool declarations
- policy

It should not mutate parent parcel history.

## Lockfile

Optional but recommended:

- `parcel.lock`

It records:

- source file digests
- normalized instruction stack digest
- tool resolution
- base parcel digest
- output parcel digest

## v1 Scope

In scope:

- parser
- normalized build graph
- local build
- local run
- parcel digest
- explicit tool declarations
- prompt stack resolution
- session/memory/artifact mounts

Out of scope for v1:

- OCI transport compatibility
- distributed layer registry
- binary delta layers
- arbitrary shell build steps

## Key Decision

`Agentfile` is not a generic programming language.

It is a constrained build language for packaging and running agents reproducibly.
