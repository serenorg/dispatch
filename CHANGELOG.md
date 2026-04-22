# Changelog

All notable changes to Dispatch are documented in this file.

## [0.3.0] - 2026-04-22

Extensions and plugin infrastructure release.

This release makes runtime wiring first-class through `dispatch.toml` and `dispatch up`, ships the first host-side channel plugin runtime, and adds catalog-based discovery for third-party extensions. Channel support is usable but still provisional; capability-enforced trust remains follow-up work.

### Extensions

- Added project-level runtime wiring through `dispatch.toml`
- Added `dispatch up` to reconcile project-local extension manifests and start configured channel bindings
- Added first-class host support for channel plugins, including listen/poll runtime bindings, ingress forwarding, reply delivery, status handling, and attachment staging
- Added one-shot `dispatch channel poll --once` support for ingress-capable channel plugins
- Added persistent channel plugin sessions for long-lived ingress bindings instead of one process per hook
- Added channel reply media staging for URL-only channels that do not support inline attachments
- Added explicit host-side tracking of channel plugin attachment capabilities and inbound attachment source handling
- Added Tier 1 extension discovery with `dispatch extension catalog add|ls|rm|refresh`, `search`, and `show`
- Added direct `dispatch extension install <name>` for catalog entries that publish GitHub release binary metadata and a pinned manifest URL
- Added project-local extension registries under `.dispatch/registries/` so deployment wiring does not mutate global host inventory
- Added shared plugin protocol crates plus JSON-RPC envelopes for courier and channel plugin transports

### Changed

- Renamed skill sidecar manifests from `dispatch.toml` to `skill.toml` to reserve `dispatch.toml` for project deployment wiring
- `dispatch.toml` extension entries can infer `kind` from plugin manifests instead of repeating it manually
- Reply delivery through channel bindings now requires a parcel-backed runtime binding instead of silently proceeding without one
- Channel poll ingress state now persists across runs and is updated atomically
- Catalog refresh now fails correctly on fetch errors and uses more useful derived names for common GitHub catalog URLs

## [0.2.0] - 2026-04-10

Security and hardening release.

### Security

- Signing secret key files are now written with restricted permissions (0600 on Unix)
- HTTP depot tag reads are bounded to 1 MiB
- HTTP depot blob reads are bounded to 512 MiB
- HTTP depot error-body reads are bounded to 64 KiB
- `dispatch secret set` now supports `--value-stdin` to avoid exposing secrets in argv and shell history
- Local tools no longer inherit `HOME` from the host environment
- Parcels declaring `NETWORK` rules are rejected until courier enforcement is implemented
- Secret stdin input via `--value-stdin` is capped at 1 MiB

### Changed

- Replaced direct `libc` calls with `nix` wrappers for Unix process operations; only the `pre_exec` detach closure retains raw `libc` for async-signal-safety
- Secret store temp-file writes use unique per-process paths to prevent collisions
- Detached-run liveness checks now use process-group liveness when a distinct stored process group ID is tracked
- Signing key writes use atomic temp-file paths with per-process uniqueness

## [0.1.0] - 2026-04-09

First public release.

### Core

- `Agentfile` authoring format with line-oriented, diff-friendly syntax
- Content-addressed parcel builds with `manifest.json` and `parcel.lock`
- Parcel signing (`dispatch parcel sign`) and verification (`dispatch parcel verify`)
- Schema publication at `https://serenorg.github.io/dispatch/schemas/parcel.v1.json`

### Couriers

- Native: host-process model-backed execution
- Docker: sandboxed local tool execution inside containers
- WASM: component-model courier using the Dispatch WIT ABI
- Plugins: external JSONL courier protocol via subprocesses

### Model Backends

- OpenAI (Responses API)
- Anthropic (Messages API)
- Gemini (generateContent)
- OpenAI-compatible (Chat Completions)
- Claude CLI (local `claude` binary using local CLI auth, config, and env)
- Codex (`codex app-server` JSON-RPC with PTY transport on Unix)
- Plugin backends (`dispatch-backend-<provider>`)
- Model fallback routing with configurable policy
- Parcel-level `MODEL`, `FALLBACK`, and `PROVIDER` directives
- Shared background reader threads for subprocess-backed backends

### Runtime

- Detached runs via `dispatch run --detach --job` and `--heartbeat`
- `dispatch serve` for long-lived service execution
- Shared cross-platform subprocess layer for detached runtime helpers, tool execution, and subprocess-backed model backends
- Persisted cron schedules (`--schedule`, parcel `SCHEDULE` directive)
- Local HTTP ingress (`--listen`, parcel `LISTEN` directive)
- Ingress controls for path filtering, method filtering, shared-secret auth, and request size limits
- Shared-secret auth with SHA-256 digest-only persistence and constant-time comparison
- Auth header redaction in forwarded payloads
- Graceful shutdown via SIGTERM and SIGINT handling
- Atomic run record persistence with platform-safe replace semantics
- Authoritative detached terminal-state snapshots for daemonless lifecycle reconciliation
- Clock-jump guard in schedule evaluation
- Run management: `dispatch ps`, `logs`, `wait`, `stop`, `restart`, `prune`, `rm`, `inspect-run`
- `dispatch wait` distinguishes successful exit from explicitly stopped or incomplete detached runs
- Docker-style aliases: `dispatch container ls`, `ps`, `logs`, `wait`, `stop`, `restart`, `prune`, `rm`, `inspect`

### Secrets

- Repo-local encrypted secret store under `.dispatch/secrets/`
- AES-256-GCM encrypted envelope with a base64-encoded key file
- `dispatch secret init`, `dispatch secret set`, `dispatch secret rm`, `dispatch secret ls`
- Secret resolution order: environment first, local store second
- Runtime integration for parcel secrets, local tools, A2A auth, and `LISTEN_SECRET` shared-secret hashing

### Eval

- Dataset-driven eval fanout via `--dataset <path>` with repo-local TOML datasets that override inputs while keeping packaged assertions
- Structured JSON trace artifacts via `--trace-dir <path>` with per-case traces under `<trace-dir>/evals/<parcel-digest>/`
- Eval summary counts in both human-readable and JSON output

### Built-in Tools

- `memory_put`, `memory_get`, `memory_list`, `memory_range` (SQLite-backed)
- `checkpoint_put`, `checkpoint_get`, `checkpoint_list`
- A2A remote tools with bearer, header, and basic auth

### Depot

- File-backed and HTTP depot transports
- `dispatch push` and `dispatch pull` with signature verification
- Tag-based parcel references

### CLI

- `dispatch build` to build an Agentfile into a parcel
- `dispatch run` to execute a parcel with courier selection
- `dispatch inspect` to display parcel metadata
- `dispatch parcel` commands for eval, list, verify, keygen, and sign
- `dispatch parcel eval` with `--dataset` and `--trace-dir`
- `dispatch secret` commands for the local encrypted store
- `dispatch skill validate` and `dispatch skill run`
- `dispatch state` to inspect parcel runtime state
- `--interactive`, `--session-file`, `--print-prompt`, `--list-tools`, and `--tool-approval`
