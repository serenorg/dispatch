# Changelog

All notable changes to Dispatch are documented in this file.

## [0.5.0] - 2026-08-24

Channel provenance release.

Inbound channel events can now carry the scope and activation evidence a host needs to re-authorize an event on its own, rather than trusting that the plugin already filtered it. Channel policies gained a workspace scope that is checked separately from the conversation scope, so widening one no longer widens the other.

The new channel protocol fields are additive and default when absent, so existing payloads and plugin manifests continue to parse. `CHANNEL_PLUGIN_PROTOCOL_VERSION` is deliberately unchanged: it is an independent wire generation number that plugin manifests are matched against by exact equality, and bumping it would reject every currently released plugin.

### Added

- `InboundConversationRef.workspace_id` identifies the provider workspace, server, or guild that owns a conversation. It sits one level above the conversation identifier and is not interchangeable with it when checking policy scope.
- `InboundConversationRef.parent_conversation_id` names the conversation a child thread descends from, so a host can resolve a thread back to the parent that policy was granted against.
- `InboundEventEnvelope.activation` carries an `InboundActivation` describing why the plugin considers an event addressed to the agent, including the provider account the plugin authenticated as and the author of a replied-to message. Absent when the plugin reports no evidence, so a host that requires evidence rejects the event instead of inferring a reason. Reason values are `direct_mention`, `reply_to_agent`, `slash_command`, `direct_message`, and `all_messages`.
- `ChannelPolicy.allowed_workspace_ids` scopes a binding to workspaces, servers, or guilds, checked separately from `allowed_conversation_ids`.
- `ChannelPolicy.allowed_outbound_conversation_ids` scopes publication destinations. Empty means outbound is no wider than the inbound conversation scope, so an unset value never grants a destination that inbound scope excludes.
- `ChannelPolicy.activation` and `ChannelPolicy.thread_policy` record the activation mode and child-thread treatment in force for a binding, so a host evaluates events against the mode the binding was granted rather than the mode a plugin reports.
- `dispatch.toml` channel bindings support `mode = "websocket"` for local runtimes and managed cloud deployments. Cloud-targeted channel bindings can be attached directly to the deployment spec without requiring a local parcel path.

### Changed

- `ChannelPolicy.allowed_conversation_ids` is now documented as conversation-scoped only. Populating it with workspace, server, or guild identifiers matches no conversation, or the wrong one where a provider reuses the value; use `allowed_workspace_ids` instead.
- Cloud-targeted channel bindings must reference a `seren-cloud` deployment whose `spec.workload.execution.type` is `"llm"`.

## [0.4.0] - 2026-05-04

Plugin protocol expansion release.

This release adds three new plugin categories alongside the existing courier and channel infrastructure: provider plugins for LLM inference backends, database plugin protocol and registry support for read+write database backends, and deployment plugins for managed deployment lifecycle control planes. It also extends `dispatch up` to reconcile project-local deployment declarations and channel bindings against managed cloud backends.

### Breaking

- Extension catalog documents now require a top-level `catalog_id`, and every catalog entry now requires a stable publisher-prefixed `id`; catalogs missing those identity fields are rejected on refresh or load

### Extensions

- Added `dispatch-provider-protocol`, `dispatch-database-protocol`, and `dispatch-deployment-protocol` crates with JSON-RPC 2.0 stdio framing
- Added provider and database plugin manifest validation, registry storage, catalog recognition, and `dispatch extension install` support
- Added installed-provider-plugin bridging from the native courier model-backend path; the `DISPATCH_BACKEND_<PROVIDER>` override resolves to the installed plugin when one is registered for that provider name
- Added the deployment plugin category, including manifest validation, registry storage, catalog recognition, and the `dispatch deployment` CLI for `validate`, `test-run`, `deploy`, `upsert`, `update`, `get`, `list`, `list-revisions`, `preview-rollback`, `rollback`, `start`, `stop`, and `delete` operations
- Added project-local deployment bindings to `dispatch.toml` with `validate`, `test_run`, `deploy`, and `upsert` reconcile modes
- Added confirmation gating for `dispatch up` deployment bindings that mutate remote resources, with `--yes` for non-interactive `deploy` and `upsert` runs
- Added `.dispatch/state/deployments.json` tracking for deployment IDs and active revision IDs produced by `dispatch up`
- Added cached deployment bundles so `dispatch up` materializes `code.parcel_dir` and `code.bundle_path` specs into reproducible bundles before sending them to the backend
- Added channel-to-deployment binding support so channel parcels receive `dispatch.deployment.*` metadata labels generated during reconciliation
- Added `dispatch up --target seren-cloud` support that resolves catalog-sourced extension artifacts for the remote target, embeds `dispatch.resolved_extensions` and `dispatch_channels` metadata in deployment specs, and skips local channel runtime startup for remote-target deployments
- Added a websocket ingress mode to the channel plugin protocol for plugins that hold an upstream socket open instead of polling
- Added provider, database, and deployment registry path configuration to project-local `dispatch.toml` resolution
- Added provider, database, and deployment plugin protocol documentation under `docs/`

### Changed

- `dispatch parcel inspect` now renders courier inspection extension metadata generically instead of special-casing Seren deployment IDs
- Connector bundles are superseded in extension documentation by the concrete provider and database plugin categories
- Deployment plugin capabilities carry backend-specific capability details under namespaced `extensions` keys
- Deployment `test_run` output may return either structured JSON or plain text
- Provider, database, and deployment plugin protocols follow the existing lenient wire-protocol policy for forward compatibility
- Plugin ecosystem documentation drops version anchors from tier status lines so the docs do not need to be updated each release

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
