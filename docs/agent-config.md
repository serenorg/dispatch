# Dispatch Agent Configuration

## Overview

Dispatch is a packaging and courier standard for agent parcels.

An agent is defined under `[agent]` in `dispatch.toml`. `dispatch parcel build` compiles this agent definition into an immutable parcel with a digest. Sibling deployment configuration is not a build input.

An agent definition declares:

- the courier target
- the instruction stack
- the tool surface
- state mounts
- model and routing defaults
- guardrails
- eval gates
- entrypoint behavior

It does not declare deployment topology. Channel bindings, channel listener addresses, plugin registries, and operator credentials live in sibling tables of `dispatch.toml` and never reach the parcel. Portable schedules, service listeners, and ingress defaults remain agent fields because they are part of the parcel's runtime contract. This separation is the same one Docker draws between a Dockerfile and `docker-compose.yml`, except both halves live in one file here.

## One file, two modes

`dispatch.toml` either defines an agent or references a built one. The two are mutually exclusive, and declaring both is an error.

Inline mode, for a single agent and a single deployment:

```toml
courier = "native"

[agent]
courier_reference = "dispatch/native:latest"
name = "basic-assistant"
entrypoint = "chat"

[[channels]]
name = "telegram"
plugin = "channel-telegram"
mode = "listen"
```

Reference mode, for a shared or third-party parcel:

```toml
parcel = "sha256:PARCEL_DIGEST"
courier = "native"

[[channels]]
name = "telegram"
plugin = "channel-telegram"
mode = "listen"
```

The top-level `courier` names the installed courier plugin a deployment runs with. `agent.courier_reference` preserves the parcel's courier target reference, including a tag or digest when supplied. Bare built-in aliases such as `native` remain accepted. In inline mode the file is its own parcel source, so `dispatch up` runs the agent defined beside the bindings.

## Migrating from Agentfile

There is no compatibility loader. Move the source declarations into `[agent]`, remove the old `Agentfile`, and rebuild every parcel. Parcels built with manifest format 1 are not executable by the current runtime.

| Former instruction | Typed field |
|---|---|
| `FROM` | `agent.courier_reference` |
| `NAME` | `agent.name` |
| `VERSION` | `agent.version` |
| `FRAMEWORK` | `[agent.framework]` |
| `COMPONENT` | `agent.component` |
| `SCHEDULE` | `agent.schedules` array |
| `LISTEN` | `agent.listeners` array |
| `LISTEN_PATH` | `agent.ingress.path` |
| `LISTEN_METHOD` | `agent.ingress.methods` array |
| `LISTEN_SECRET` | `agent.ingress.secret_env` |
| `LISTEN_MAX_BODY_BYTES` | `agent.ingress.max_body_bytes` |
| `LISTEN_MAX_HEADER_BYTES` | `agent.ingress.max_header_bytes` |
| `LABEL` | `[agent.labels]` entries |
| `IDENTITY` | `agent.instructions.identity` |
| `SOUL` | `agent.instructions.soul` |
| `SKILL` | A standalone file becomes `agent.instructions.skill`; a bundle directory becomes an entry in `agent.skills`. |
| `AGENTS` | `agent.instructions.agents` |
| `USER` | `agent.instructions.user` |
| `TOOLS` | `agent.instructions.tools` |
| `HEARTBEAT` | `agent.instructions.heartbeat`; declare `agent.entrypoint = "heartbeat"` separately. |
| `MEMORY` | The prompt document path becomes `agent.instructions.memory`. |
| `PROMPT` | `agent.prompts` array |
| `MODEL` | `[agent.model]` fields `id`, `provider`, and `options` |
| `FALLBACK` | `[[agent.model.fallbacks]]` |
| `ROUTING` | `agent.model.routing` |
| `TOOL` | `[[agent.tools]]`, selected by `kind` |
| `COPY` | Entry in `agent.files` |
| `ADD` | Entry in `agent.files` |
| `ENV` | `[agent.env]` entries |
| `SECRET` | `[[agent.secrets]]` |
| `NETWORK` | `[[agent.network]]` |
| `VISIBILITY` | `agent.visibility` |
| `TIMEOUT` | `[agent.timeouts]` |
| `LIMIT` | `[agent.limits]` |
| `COMPACTION` | `[agent.compaction]` |
| `MOUNT` | `[[agent.mounts]]` |
| `EVAL` | Entry in the `agent.evals` array |
| `TEST` | `[[agent.tests]]` |
| `ENTRYPOINT` | `agent.entrypoint` |

The old tool approval value `required` has no alias in the typed schema; use `approval = "confirm"`. Limit and timeout qualifier tokens have no replacement because the runtime never consumed them. Network qualifiers remain available as `agent.network.qualifiers`.

## Build model

### Inputs

Build context:

- `dispatch.toml`
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

The build reads `[agent]` and ignores every other table, so a deployment-only edit cannot change the parcel digest. The source config file is never packaged into the parcel context, including through `agent.files` or a referenced directory; only other referenced files are eligible for packaging.

Implementations should provide a verification path that can recompute the manifest digest and validate packaged file hashes against the built parcel metadata.

Format compatibility:

- every parcel declares a `$schema` URL and an integer `format_version`
- couriers must validate parcels against the schema they claim to support before execution
- couriers must reject parcels with unsupported schema URLs or format versions
- the Dispatch reference implementation currently supports `format_version: 2`
- schema publication and versioning policy are documented in [`schema-compatibility.md`](schema-compatibility.md)

### Parcel vs mounts

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

## Field reference

### Agent identity and target

| Field | Type | Notes |
|---|---|---|
| `courier_reference` | string | Required. Courier target reference, for example `dispatch/native:latest`, `native`, `dispatch/docker:latest`, `dispatch/wasm:latest`. |
| `name` | string | Agent name. |
| `version` | string | Agent version. |
| `entrypoint` | string | One of `chat`, `job`, `heartbeat`. |
| `visibility` | string | `open` or `opaque`. |
| `component` | string | WebAssembly component path. Required when `courier_reference` targets wasm, rejected otherwise. |
| `schedules` | array of string | Cron schedules. |
| `listeners` | array of string | Listener addresses. |
| `skills` | array of string | Agent Skills bundle directories, each containing a `SKILL.md`. Standalone files belong in `instructions.skill`. |
| `evals` | array of string | Eval documents to package. |
| `prompts` | array of string | Inline prompt text. |
| `files` | array of string | Extra files or directories to package. The source `dispatch.toml` is always excluded. |

### `[agent.framework]`

| Field | Type | Notes |
|---|---|---|
| `name` | string | Required. Framework name. |
| `version` | string | Framework version. |
| `target` | string | Framework target. |

### `[agent.instructions]`

Each value is a file path relative to `dispatch.toml`. Keys: `identity`, `soul`, `skill`, `agents`, `user`, `tools`, `memory`, `heartbeat`.

`heartbeat` requires `entrypoint = "heartbeat"`.

### `[agent.model]`

| Field | Type | Notes |
|---|---|---|
| `id` | string | Primary model identifier. Optional when the table declares only `routing` or `fallbacks`. |
| `provider` | string | Backend name. Requires `id`. |
| `routing` | string | Routing policy. |
| `options` | table of string | Backend options. Supported: `persist-thread`, `reasoning-effort`. Requires `id`. |
| `fallbacks` | array of table | Each entry takes `id`, `provider`, `options`. Order is the fallback order. |

### `[agent.ingress]`

| Field | Type | Notes |
|---|---|---|
| `path` | string | Must start with `/`. |
| `methods` | array of string | HTTP method tokens, normalized to uppercase. |
| `secret_env` | string | Name of a declared secret. |
| `max_body_bytes` | integer | Must be greater than zero. |
| `max_header_bytes` | integer | Must be greater than zero. |

### `[agent.env]` and `[agent.labels]`

Tables of string to string.

### `[[agent.secrets]]`

| Field | Type | Notes |
|---|---|---|
| `name` | string | Required. Secret name only, never a value. |
| `required` | bool | Defaults to `true`. |

### `[[agent.mounts]]`

| Field | Type | Notes |
|---|---|---|
| `kind` | string | Required. `session`, `memory`, or `artifacts`. |
| `driver` | string | Required. |
| `options` | array of string | Driver options. |

### `[[agent.tools]]`

Every tool entry carries a `kind` and may set `approval` (`never`, `always`, `confirm`, `audit`), `risk` (`low`, `medium`, `high`), and `description`. The former approval alias `required` is not accepted; use `confirm`.

`kind = "builtin"` takes `name`.

`kind = "mcp"` takes `server`.

`kind = "local"` takes `path`, optional `alias` (defaults to the file stem), optional `schema`, and an optional `runner` inline table of `command` and `args`. Without `runner`, the command is inferred from the file extension.

`kind = "a2a"` takes `alias`, `url`, and optional `discovery` (`auto`, `card`, `direct`), `expect_agent_name`, `expect_card_sha256`, and `schema`. `discovery = "direct"` cannot be combined with the identity expectations. Credentials bind through `[agent.tools.auth]` with a `scheme` of `bearer` (`secret_name`), `header` (`header_name`, `secret_name`), or `basic` (`username_secret_name`, `password_secret_name`). Every referenced secret must also be declared in `[[agent.secrets]]`.

### `[agent.limits]` and `[agent.timeouts]`

Limits accept `iterations`, `tool_calls`, `tool_output`, `context_tokens`, and `tool_rounds` as integers.

Timeouts accept `run`, `tool`, and `llm` as durations: a positive integer followed by `ms`, `s`, `m`, or `h`.

### `[agent.compaction]`

| Field | Type | Notes |
|---|---|---|
| `interval` | string | Required. |
| `overlap` | integer | Optional. |

### `[[agent.network]]`

| Field | Type | Notes |
|---|---|---|
| `action` | string | Required, for example `allow`. |
| `target` | string | Required. |
| `qualifiers` | array of string | Optional. |

### `[[agent.tests]]`

| Field | Type | Notes |
|---|---|---|
| `tool` | string | Required. Alias of a declared local or A2A tool. |

## Resolution order

The resolved prompt stack is deterministic:

1. prompt-bearing instruction files are appended in a fixed order, independent of how the file is written
2. inline `prompts` bodies are appended after the packaged instruction files
3. courier-specific system supplements, if any, are injected after the parcel-owned prompt stack

The prompt-bearing instruction order is `identity`, `soul`, the standalone `instructions.skill` file, the `skills` bundles in array order, `agents`, `user`, `tools`, `memory`, `heartbeat`.

Eval documents are packaged but omitted from the runtime prompt stack.

Skills lower before `[[agent.tools]]`, so an explicit tool declaration always overrides a skill-provided tool of the same alias and the build emits a warning. Two skills declaring the same alias is an error.

## Build-time validation

Serde rejects unknown fields at every nested level, missing required fields, and values outside a declared enum. Build validation rejects invalid semantic values and cross-references. A declaration that cannot be lowered fails the build rather than being dropped.

`dispatch parcel build` fails if:

- referenced files are missing
- a declared local tool does not exist
- `tests.tool` references an unknown local or A2A tool alias
- an A2A auth secret is not declared in `[[agent.secrets]]`
- `ingress.secret_env` is not declared in `[[agent.secrets]]`
- `instructions.heartbeat` is set without `entrypoint = "heartbeat"`
- a wasm courier target has no `component`
- `component` is set for a non-wasm courier target
- a packaged file is recorded twice with conflicting content hashes
- the source `dispatch.toml` is referenced directly as parcel content

## Credential hygiene

Keep secret values out of `dispatch.toml`. The agent schema accepts secret names in `[[agent.secrets]]`, A2A auth bindings, and ingress policy, but never agent secret values. Put credential-bearing channel configuration in a separate `config_file`; inline channel `config` is for non-secret settings, and the project loader rejects common credential-like inline keys such as tokens, passwords, API keys, passphrases, and secrets, matched case-insensitively across `snake_case`, `camelCase`, and `kebab-case`. Keep credential files outside every directory referenced by `agent.files`, `agent.skills`, tool paths, component paths, or instruction paths. The builder excludes only the source `dispatch.toml`; an explicitly referenced credential file is still parcel content.

## Normalized parcel manifest

Every built parcel exposes a normalized courier config that any backend can consume:

- parcel digest
- instruction stack
- resolved tools
- policy
- entrypoint
- mount requirements
- env and secret declarations

This normalized config is the bridge into the local runner, container courier, worker courier, sandbox courier, and control plane deployment systems.

## Lockfile

Optional but recommended: `parcel.lock`.

It records the parcel format version and digest, the manifest and context layout, and the packaged file records used for integrity verification.

## Key decision

The agent table is not a generic programming language.

It is a constrained, strictly typed declaration for packaging and running agents reproducibly. Everything it accepts is a field with a type; anything it does not recognize is an error at build time, not a silently discarded line.
