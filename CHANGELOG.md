# Changelog

All notable changes to Dispatch are documented in this file.

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
