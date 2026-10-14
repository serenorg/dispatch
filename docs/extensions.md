# Dispatch Extensions

Dispatch supports the following extension categories:

| Category | Purpose | Status |
|---|---|---|
| Courier plugins | Alternate runtime backends for parcel execution | Stable |
| Channel plugins | Messaging and webhook transport integrations | Provisional |
| Provider plugins | External LLM inference backends | Provisional |
| Database plugins | Read+write database backends exposed to parcels as tools | Provisional |
| Deployment plugins | Managed deployment lifecycle control planes | Provisional |
| Connector bundles | Reusable tool/provider packages | Superseded by Provider + Database plugins |

Extensions live outside the core repository and communicate with Dispatch over JSON-RPC 2.0 messages framed as line-delimited JSON on stdio.

Dispatch has first-class install/runtime support for courier plugins, channel plugins, and deployment plugins today. See [`deployment-plugin-protocol.md`](./deployment-plugin-protocol.md) for the deployment wire protocol. Provider and database plugins are still provisional; see [`provider-plugin-protocol.md`](./provider-plugin-protocol.md) and [`database-plugin-protocol.md`](./database-plugin-protocol.md) for the in-progress specifications. The previously-planned "connector bundles" category is subsumed by these concrete plugin kinds.

For discovering third-party extensions published in separate repositories, see [Discovery via catalogs](#discovery-via-catalogs) below. The broader ecosystem roadmap lives in [`plugin-ecosystem.md`](./plugin-ecosystem.md).

## Category boundary

The extension category describes the operation Dispatch is asking the plugin to perform, not the vendor or product name behind it.

- A **deployment plugin** provisions and manages durable remote resources. It owns lifecycle operations such as validate, deploy, upsert, update, rollback, start, stop, delete, and list. If a plugin uploads an artifact, creates a cloud workload, or reconciles a remote deployment by name, it is a deployment plugin.
- A **courier plugin** talks to an execution runtime for an already-selected parcel or deployment. It owns runtime session operations such as validate parcel compatibility, inspect runtime requirements, open or resume a session, run turns, stream events, cancel, and shut down. If a plugin sends a turn to a running sandbox, local process, container, or remote session, it is a courier plugin.
- A **channel plugin** talks to external messaging systems. It owns ingress and outbound delivery, not parcel execution and not remote deployment lifecycle.
- A **provider plugin** handles LLM inference calls. It is a model backend, not a parcel runtime and not a deployment control plane.
- A **database plugin** exposes database operations as tools or sessions. It is data access, not deployment lifecycle.

The practical rule is: deployment provisions the thing; courier runs or talks to the thing. A cloud product may expose both concerns, but those concerns remain separate protocol surfaces. Dispatch should choose the plugin kind by the method being invoked rather than by the product label.

## Layering

Dispatch keeps authored parcel source separate from host-managed extension inventory.

- the agent definition in `dispatch.toml` is the canonical authored source for a parcel. It defines the agent's prompt stack, tools, model policy, mounts, and generic runtime intent that survives build, review, and signing.
- installed courier plugins, channel plugins, and deployment plugins are host inventory. They are local runtime capabilities available to an operator on a specific machine or environment.
- extension install commands do not mutate parcel source. They populate a host registry that runtime commands can resolve by name.

That split is intentional. Courier binaries, channel adapter binaries, webhook URLs, and platform credentials are deployment concerns, not parcel build inputs.

## Extension categories

### Courier plugins

Courier plugins are runtime backends. They execute parcels.

Built-in couriers: `native`, `docker`, `wasm`

A courier plugin implements:

- `capabilities` - declare what the courier supports
- `validate_parcel` - check whether a parcel is compatible
- `inspect` - report parcel requirements (secrets, mounts, tools)
- `open_session` / `resume_session` - manage execution sessions
- `run` - execute a turn and stream events
- `shutdown` - clean up

Examples: local processes, Docker or WASM sandboxes, self-hosted cluster runners, SSH-backed sessions, browser automation sessions, and remote runtimes that already exist before the courier opens a session.

### Channel plugins

Channel plugins are messaging adapters. They translate between external platforms and Dispatch's inbound/outbound event model.

A channel plugin implements:

- `capabilities` - declare platform, ingress modes, threading model
- `configure` - validate credentials and report channel metadata
- `health` - verify connectivity and account identity
- `poll_ingress` - perform a single host-driven polling fetch when requested
- `start_ingress` / `stop_ingress` - begin/end an ingress session
- `ingress_event` - parse a raw webhook payload into normalized events
- `deliver` - send a reply to an existing conversation
- `push` - send a proactive message (broadcast, alert, scheduled)
- `get_message` / `get_permalink` - read a delivered message, or its canonical permalink, back by the receipt-bound reference returned from `deliver` or `push` (optional; plugins without provider read-back return an `unsupported_request` error)
- `status` - relay agent progress to the conversation (typing indicators, etc.)
- `shutdown` - allow the host to terminate a persistent ingress process cleanly

Examples: Telegram, Discord, Slack, WhatsApp, Signal, Twilio SMS, generic webhooks.

Channel plugins vary by platform, but the protocol covers the full inbound and outbound lifecycle: configuration, health, ingress setup, webhook forwarding or background receive loops, delivery, push, and status frames.

Dispatch keeps channel plugins alive during active listeners, pollers, and websocket connections. Plugins send inbound batches as `channel.event` JSON-RPC requests. The host returns an exact receipt only after it saves the complete batch durably. One-shot polling and webhook calls use the same receipt exchange before their control response.

For channel ingress, `start_ingress` is the primary session-oriented contract. `poll_ingress` remains as the auxiliary one-shot receive primitive used when the host explicitly wants a single fetch cycle rather than a long-lived ingress session.

That separation is intentional. Dispatch controls the plugin lifecycle and the stdio framing; the plugin is free to translate that into whatever upstream transport best matches the platform, including repeated HTTP polling, upstream websockets, upstream event sockets, or daemon-backed local IPC.

As a design rule, Dispatch defaults plugin interactions to one-shot request/response cycles and opts into persistence only when the domain is inherently sessioned or event-driven. Channel plugins fall into that second category: they may need to hold an upstream connection open, maintain ingress state across calls, or emit events on the platform's schedule rather than the host's schedule.

For channel plugins specifically:

- The long-lived process is scoped to an active channel ingress session, not to a single agent turn and not to the entire host forever.
- Host-initiated operations such as `configure`, `health`, `deliver`, `push`, `status`, `poll_ingress`, `stop_ingress`, and `shutdown` remain ordinary JSON-RPC requests with terminal responses.
- Spontaneous inbound activity uses `channel.event` requests so the plugin can confirm durable host acceptance.

### Provider plugins

Provider plugins are external LLM inference backends. A courier routes completion and streaming requests through a provider plugin when the target model is served by an endpoint Dispatch does not natively link against.

A provider plugin implements:

- `capabilities` - declare supported models, modalities, tool-use, streaming
- `configure` - validate credentials and endpoint
- `health` - verify connectivity and auth
- `complete` - one-shot non-streaming completion
- `stream` - streaming completion with event notifications
- `cancel` - best-effort cancellation of an in-flight stream
- `shutdown` - clean up

Examples: hosted model gateways, self-hosted inference servers, specialized endpoints for fine-tuned or private models.

See [`provider-plugin-protocol.md`](./provider-plugin-protocol.md) for the wire protocol.

### Database plugins

Database plugins are read+write database backends exposed to parcels as callable tools. They cover traditional OLTP-style databases and document stores under a shared protocol.

A database plugin implements:

- `capabilities` - declare engine, supported operations, authentication modes
- `configure` - validate connection and credentials
- `health` - verify connectivity
- `describe` - return schema or collection introspection metadata
- `open_session` / `close_session` - manage connection or transaction lifecycle
- `execute` - run a typed operation
- `shutdown` - clean up

Examples: PostgreSQL, SerenDB, Neon, Supabase, MongoDB, MySQL, SQLite.

Vector stores, full-text search indices, object storage, caches, and queues are out of scope for the database kind; they will get dedicated plugin kinds as they are needed.

See [`database-plugin-protocol.md`](./database-plugin-protocol.md) for the wire protocol.

### Deployment plugins

Deployment plugins are managed deployment control planes. They create, update, roll back, list, start, stop, and delete deployments, but they do not execute runtime turns. Runtime turns remain the responsibility of courier plugins, usually addressed by the `deployment_id` returned by a deployment plugin.

A deployment plugin implements:

- `capabilities` - declare which lifecycle features and runtime artifact targets the backend supports; backend-specific catalog data (templates, model policies, regions, instance classes, runtimes) is carried under `extensions`
- `configure` - validate credentials and endpoint
- `health` - verify connectivity
- `validate` - check a candidate deployment spec without side effects
- `test_run` - run a draft preflight without creating a long-lived deployment
- `deploy`, `update`, `preview_update` - manage deployment revisions
- `list`, `get`, `list_revisions` - inspect deployment state
- `preview_rollback`, `rollback` - inspect and apply revision rollback
- `start`, `stop`, `delete` - manage lifecycle state
- `shutdown` - clean up

The `spec` and `patch` payloads are `serde_json::Value` so each backend defines its own deployment shape. Backends that do not support a given lifecycle (revisions, rollback, scheduled execution, test runs) advertise that through the `supports_*` capability booleans for discovery and planning, and should still return a clear unsupported-operation error if callers invoke an unsupported method.

Deployment capabilities also include `runtime_targets` for deployment backends that execute packaged artifacts. These targets describe the remote runtime environment, not the plugin binary installed on the operator's laptop. Native targets use Rust-style triples such as `aarch64-unknown-linux-gnu`; WASM targets are advertised by Dispatch ABI/version because a generic `wasm32-wasi` artifact is not enough to prove compatibility with Dispatch host functions. Callers that package runtime plugins should prefer a compatible WASM artifact when the deployment backend advertises one, otherwise choose a native release asset matching the backend's preferred native target.

Examples: managed-agent control planes such as `seren-agent`; general workload deployers for serverless functions, edge workers, sandboxed dev environments, and container platforms.

See [`deployment-plugin-protocol.md`](./deployment-plugin-protocol.md) for the wire protocol.

### Connector bundles

The previously-planned "connector bundles" category is superseded. Inference-oriented integrations are now modeled as provider plugins; database-oriented integrations are now modeled as database plugins.

## Managing host extension inventory

### Courier plugins

```sh
dispatch courier install path/to/courier-plugin.json
dispatch courier ls
dispatch courier inspect <name>
```

### Deployment plugins

```sh
dispatch deployment list
dispatch deployment inspect <name>
dispatch deployment deploy <name> spec.json
dispatch deployment rollback <name> <deployment-id> <revision-id>
```

### Channel plugins

```sh
dispatch channel install path/to/channel-plugin.json
dispatch channel ls
dispatch channel inspect <name>
dispatch channel call <name> --request-json '{"kind":"capabilities"}'
dispatch channel call channel-telegram --request-file telegram-deliver.json
dispatch channel ingress --path /telegram/updates --header X-Telegram-Bot-Api-Secret-Token=... --body-file update.json
dispatch channel listen channel-telegram --listen 127.0.0.1:8787 --config-file telegram-config.toml
dispatch channel listen channel-telegram --listen 127.0.0.1:8787 --config-file telegram-config.toml --parcel ./dispatch.toml --session-root ./.dispatch/channel-sessions --deliver-replies
dispatch channel poll channel-telegram --config-file telegram-config.toml --once
dispatch up
```

These commands manage the local host registry, not parcel source. They are the operator-facing way to install and inspect runtime extensions that exist outside any one parcel.

Channel plugin subprocess calls use a fixed 30s host-side timeout.

`dispatch channel call` forwards the request JSON directly to the plugin. That is the most direct way to exercise channel-specific delivery features such as attachments without depending on the parcel reply bridge conventions.

`dispatch channel listen` and `dispatch channel poll` are low-level runtime commands. They are useful for development, testing, and direct operator control, but they are not the canonical authored configuration surface for project-specific channel wiring. For polling-style channels, Dispatch persists the latest plugin-reported ingress state under `.dispatch/channel-state/` in the current working directory so repeated `dispatch channel poll --once` runs resume from the last source cursor instead of replaying old events. Delete that directory (or the specific `<plugin>/<label>-<hash>.json` file) to reset the cursor and reprocess events on the next poll.

Common ingress patterns:

- Telegram polling uses the Bot API `getUpdates` HTTP flow inside the plugin.
- Signal polling uses either upstream HTTP receive (`native` / `normal`) or websocket-backed receive (`json-rpc`) inside the plugin.
- Slack polling uses Socket Mode through `apps.connections.open`. The plugin saves each consumed envelope in its pending journal before provider acknowledgment. It sends normalized batches through the exact durable host receipt exchange.

`dispatch up` is the project-level runtime binding command. It reads `dispatch.toml`, reconciles declared extension manifests into project-local registries under `.dispatch/registries/`, validates or applies declared deployment bindings, and starts the configured channel bindings without mutating the global registries under `~/.config/dispatch/`. Use `dispatch up --dry-run` to preview installs, deployment bindings, and channel bindings without mutating registries, invoking deployment plugins, or starting listeners/pollers.

Minimal `dispatch.toml` example:

```toml
parcel = "./agent.parcel"
courier = "native"

[[extensions]]
manifest = "../dispatch-plugins/channels/telegram/channel-plugin.json"

[[extensions]]
manifest = "../dispatch-seren-plugins/deployments/seren-agent/deployment-plugin.json"

[[deployments]]
name = "research-monitor"
plugin = "seren-agent"
reconcile = "upsert"
config_file = "./config/seren-agent.toml"
spec_file = "./deployments/research-monitor.json"

[[channels]]
plugin = "channel-telegram"
deployment = "research-monitor"
mode = "listen"
listen = "127.0.0.1:8787"
deliver_replies = true
config_file = "./config/telegram.toml"
```

Channel and deployment binding config files may be JSON or TOML. Inline `config = { ... }` tables in `dispatch.toml` are intended only for non-secret settings. The project loader rejects common credential-like keys in inline channel config. Keep all credential-bearing binding config in a separate `config_file`, because `dispatch.toml` can also be the shareable agent source. Keep those files outside directories referenced by `agent.files` or other parcel paths; explicit parcel references still package their targets.

Deployment bindings carry two distinct payloads. `config` (or `config_file`) is the auth/endpoint setup sent through `deployment.configure` once before each operation, e.g. API base URL, API key, account id, workspace. `spec` (or `spec_file`) is the deployment definition the plugin validates, test-runs, deploys, or reconciles. Both accept inline tables or external JSON/TOML files.

For deployment specs that use the conventional `code.parcel_dir` or `code.bundle_path` helper shape, `dispatch up` materializes the local source into a project-local cached bundle under `.dispatch/state/bundles/`, keyed by SHA-256. The plugin receives `code.cached_bundle = { path, sha256, size_bytes, source_kind = "tar_gz" }` instead of the original source path. This keeps archive/hash work in the project orchestration layer, lets repeated `dispatch up` runs reuse unchanged bundles, and gives deployment plugins a stable artifact path they can upload or inspect without walking the user's source tree again.

If that bundle contains runtime plugin executables, those executables must match the deployment plugin's `runtime_targets`, not the host machine that ran `dispatch up`. For example, a macOS user deploying to Seren Cloud should package a Linux arm64 Discord channel plugin asset, not the local macOS build. Dispatch's parcel format is portable only when its contents are portable; native binaries inside the parcel remain OS/architecture-specific.

A deployment binding's `reconcile` mode controls what `dispatch up` does:

- `validate` (default) checks the authored spec without creating or modifying remote resources.
- `test_run` invokes plugin-defined preflight execution (often a one-shot dry run against the backend).
- `deploy` calls `deployment.deploy`, which creates a managed deployment. Plugin-defined idempotency rules apply; if the underlying plugin treats `deploy` as create-only, repeated `dispatch up` runs will create duplicate deployments. Use `upsert` when you want compose-style reconcile-by-name semantics.
- `upsert` calls `deployment.upsert` with a stable name, asking the plugin to reconcile by name (create on first run, then return or update the existing deployment on subsequent runs depending on plugin semantics). Plugins that do not implement upsert respond with `unimplemented`.

`dispatch up` records the produced `deployment_id` and `revision_id` for every successful `deploy` or `upsert` in `.dispatch/state/deployments.json`, keyed by `(plugin, name)`. The state file is informational; subsequent runs use it to print the previously-known id during `--dry-run` but do not skip plugin invocation.

Bindings with `reconcile = "deploy"` or `reconcile = "upsert"` are gated by an interactive confirmation prompt unless `dispatch up --yes` is passed. Bindings with `reconcile = "validate"` or `reconcile = "test_run"` run unprompted.

Channel bindings may reference a deployment binding by name with `deployment = "research-monitor"`. After deployment reconciliation, Dispatch resolves the recorded deployment state, creates a runtime copy of the parcel, and injects generic labels such as `dispatch.deployment.id`, `dispatch.deployment.name`, and `dispatch.deployment.plugin` into that copy before starting the channel. Couriers can use those labels to connect runtime sessions to an existing deployment without requiring users to edit built parcel manifests by hand.

`deliver_replies = true` requires a project-level `parcel = "..."` entry or a direct `--parcel` argument on the low-level channel commands. Reply delivery is a parcel-runtime bridge, not a standalone channel feature.

`[[extensions]]` entries can omit `kind` when the referenced manifest declares its own `kind`, or when it uses a conventional filename such as `channel-plugin.json`, `courier-plugin.json`, or `deployment-plugin.json`.

A concrete example lives at `examples/runtime/telegram-bot/dispatch.toml`.

## Extension manifest format

Dispatch has two manifest shapes:

- Courier plugins use a compact `courier-plugin.json` consumed by the courier registry installer.
- Deployment plugins use a compact `deployment-plugin.json` consumed by the deployment registry installer.
- Channel plugins use a richer `channel-plugin.json` that includes bootstrap, auth, capability, ingress, and requirement metadata.

Channel plugin manifest example (`channel-plugin.json`):

```json
{
  "kind": "channel",
  "name": "channel-telegram",
  "version": "0.1.0",
  "protocol": "jsonl",
  "protocol_version": 2,
  "description": "Telegram channel plugin for Dispatch.",
  "entrypoint": {
    "command": "./target/release/channel-telegram",
    "args": []
  },
  "bootstrap": {
    "credentials": [
      {
        "name": "TELEGRAM_BOT_TOKEN",
        "prompt": "Enter the Telegram bot token from BotFather."
      }
    ],
    "setup_url": "https://t.me/BotFather",
    "verification_endpoint": "https://api.telegram.org/bot{TELEGRAM_BOT_TOKEN}/getMe"
  },
  "capabilities": {
    "channel": {
      "platform": "telegram",
      "ingress_modes": ["webhook", "polling", "websocket"],
      "outbound_message_types": ["text"],
      "threading_model": "chat_or_topic",
      "attachment_support": true,
      "reply_verification_support": true,
      "account_scoped_config": true,
      "allow_polling": true,
      "webhook_secret_support": true,
      "allowed_paths": ["/telegram/updates"],
      "ingress": {
        "endpoints": [
          {
            "path": "/telegram/updates",
            "methods": ["POST"],
            "host_managed": true
          }
        ],
        "trust": {
          "mode": "shared_secret_header",
          "header_name": "X-Telegram-Bot-Api-Secret-Token",
          "secret_name": "TELEGRAM_WEBHOOK_SECRET",
          "host_managed": true
        },
        "polling": {
          "min_interval_ms": 100,
          "default_interval_ms": 250
        }
      },
      "delivery": {
        "push": true,
        "status_frames": true
      }
    }
  },
  "requirements": {
    "secrets": ["TELEGRAM_BOT_TOKEN"],
    "optional_secrets": ["TELEGRAM_WEBHOOK_SECRET"],
    "network_domains": ["api.telegram.org"]
  }
}
```

Courier plugin manifest example (`courier-plugin.json`):

```json
{
  "kind": "courier",
  "name": "my-remote-courier",
  "version": "0.1.0",
  "protocol_version": 1,
  "transport": "jsonl",
  "description": "Remote courier for my infrastructure.",
  "exec": {
    "command": "./my-courier",
    "args": ["--stdio"]
  }
}
```

The courier `kind` field is optional, but when present it must be `"courier"`.

## Channel plugin protocol

Channel plugins communicate over stdio using JSON-RPC 2.0 messages, framed as one JSON value per line.

### Wire format

Each request from the host is a JSON-RPC request line:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "channel.capabilities",
  "params": {
    "protocol_version": 2,
    "kind": "capabilities"
  }
}
```

Each non-error response from the plugin is a JSON-RPC success response whose `result` carries the existing typed channel payload:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "kind": "capabilities",
    "capabilities": {
      "plugin_id": "telegram",
      "platform": "telegram"
    }
  }
}
```

Errors are returned as JSON-RPC error responses. Dispatch-specific structured error details are carried in `error.data.dispatch_error`.

Dispatch keeps the manifest transport value as `"jsonl"` because the framing is still one JSON message per line on stdio. JSON-RPC defines the message shape; the manifest transport defines how those messages are carried between host and plugin.

Dispatch does not use JSON-RPC batch requests for channel plugins. Each direction has at most one active request: the host sends one control request at a time with increasing integer IDs, and the plugin has at most one unacknowledged `channel.event` request. The plugin's stdin reader separates host requests from event receipts by their checked message shape and exact ID (`parse_plugin_input`). A plugin that waits for an event receipt must keep reading stdin and must still answer host control requests, because a host can send a control request before it acknowledges a batch. After any failed exchange on a persistent session, the host stops that plugin process instead of reusing a stream whose position is unknown. Ordinary requests, responses, errors, and heartbeats keep accepting additional top-level JSON-RPC members and additional result members, as before. Only `channel.event` requests and their receipts must contain exactly their defined members.

### Request types

| Kind | Fields | Purpose |
|---|---|---|
| `capabilities` | (none) | Query plugin features |
| `configure` | `config` | Validate config, report metadata |
| `health` | `config` | Verify credentials and connectivity |
| `poll_ingress` | `config`, `storage`, `state?` | Perform one polling cycle through the durable batch exchange |
| `start_ingress` | `config`, `storage`, `state?` | Start or resume an ingress session |
| `stop_ingress` | `config`, `state?` | Deregister webhook / stop listening |
| `ingress_event` | `config`, `storage`, `state?`, `payload` | Parse a raw webhook through the durable batch exchange |
| `deliver` | `config`, `message` | Send reply to existing conversation |
| `push` | `config`, `message` | Send proactive/broadcast message |
| `status` | `config`, `update` | Relay agent progress to channel |
| `shutdown` | (none) | Request clean process shutdown |

`deliver` and `push` share the same outbound message shape:

```json
{
  "content": "Dispatch says hello.",
  "content_type": "text/plain",
  "attachments": [
    {
      "name": "report.txt",
      "mime_type": "text/plain",
      "data_base64": "aGVsbG8="
    }
  ],
  "metadata": {
    "conversation_id": "chat-123",
    "thread_id": "7",
    "reply_to_message_id": "1"
  }
}
```

`attachments` is optional. Each attachment may provide one of:

- `data_base64` for inline upload
- `url` for channels that can fetch media by URL
- `storage_key` for staged-media flows defined by a specific channel or host

### Response types

These are the typed response payloads carried in JSON-RPC success responses:

| Kind | Fields | Purpose |
|---|---|---|
| `capabilities` | `capabilities` | Plugin feature declaration |
| `configured` | `configuration` | Channel metadata and policy |
| `health` | `health` | Connectivity report |
| `ingress_started` | `state` | Webhook registered |
| `ingress_stopped` | `state` | Webhook deregistered |
| `ingress_events_received` | `callback_reply?`, `state?`, `poll_after_ms?` | Control response after the batch receipt |
| `delivered` | `delivery` | Delivery receipt |
| `pushed` | `delivery` | Push delivery receipt |
| `status_accepted` | `status` | Status acknowledgment |
| `ok` | (none) | Generic success for shutdown/ack-only operations |

### Durable event requests

`ChannelEventRequest` contains `protocol_version`, `batch_digest`, `batch`, and optional `previous_acceptance`. The batch contains ordered `events`, optional original `state`, and optional `poll_after_ms`. The request ID is exactly `channel:<batch_digest>`. Channel protocol 2 replaces the released inline-notification contract: rebuild plugins against this protocol and declare `protocol_version: 2`; the host rejects older manifests before launch.

The digest is the lowercase hexadecimal SHA-256 of the canonical batch bytes: compact `serde_json` serialization of the typed `ChannelEventBatch` with recursively sorted object keys, raw UTF-8 strings, and unchanged array order. The predecessor receipt does not affect the content digest, but it belongs to the immutable original request. The wire request must equal its typed shape: include default fields, omit absent optional fields instead of sending `null`, and omit unknown fields. Producers outside Rust must reproduce these bytes, including `serde_json` string escaping; the host ends a persistent session when the frame differs.

The host returns `ChannelEventAcceptance::Accepted` with `kind: "accepted"`, the exact digest, the exact ordered `event_ids`, and a required positive `sequence`. The response ID matches the request ID. This receipt confirms durable storage only. It does not confirm execution or delivery, and it does not permit the host to erase accepted work. Unknown fields, partial receipts, and mismatched IDs fail closed. The host assigns sequence 1 to the first head. After checking the actual durable predecessor, it assigns the checked predecessor sequence plus 1 to a successor; an exhausted sequence fails without a receipt. An exact current-head replay keeps its original sequence. Identical batch content has an identical request ID, but a later recurrence gets a new sequence, so delayed intermediate frames cannot match the current receipt. Because occurrences can share a request ID, a producer applies a receipt only when it equals the full expected receipt of its outstanding frame, including `sequence`; a receipt for an earlier occurrence is not an acknowledgment of the current one. A producer may compute the expected receipt with `ChannelEventRequest::acceptance()`, which returns a `Result`; that calculation does not authorize acceptance or choose host state.

Each batch has at most 32 events, unique non-empty event IDs, and 2 MiB of canonical JSON. The host limits a batch request line to 2 MiB plus 36 KiB, including the bounded predecessor receipt; ordinary requests and responses keep their existing size contract. The generic SDK reader reads a complete line before the batch parser enforces that limit, so it does not promise bounded allocation for stdout from a local plugin. Empty progress batches require a durable receipt before cursor progress. The producer retains one original pending batch and replays it without changing its predecessor, state, order, or grouping. After receiving the exact receipt, the producer persists the accepted original request and receipt before clearing pending work or advancing its cursor. A successor must carry the exact current-head receipt. The host returns the same receipt only for an exact current-head replay and rejects old frames after a successor replaces the head. A successor can reuse a content digest already present in the current head or pending work: its new sequence owns the occurrence, and event receipts prevent reoffering prior work. The host preserves older pending originals and stores a duplicate-only occurrence as a retired head. Pruned older digests are not remembered, so permanent duplicate protection comes from the per-event receipts described under the CLI acceptance inbox. The sequence identifies each accepted occurrence even when earlier completed batch records have been pruned; a delayed frame with an older predecessor receipt is refused. An unchanged poll has the same digest as the current head; producers replay the saved original for efficiency rather than constructing a new occurrence. In a `poll_ingress` or `ingress_event` call, it replays the saved original head frame and waits for its receipt, because the host rejects those control responses without a batch receipt. In a persistent session, it may instead send nothing or a liveness heartbeat. A host does not bound how long it takes to return a receipt; the producer waits for the session or call to end rather than timing out and re-sending, and treats a repeated exact sequence-bearing receipt as idempotent. If the producer journal holds an accepted head but `storage.recovery` is absent, the host has no matching head (for example, after its inbox was lost) and refuses every successor. The producer then fails closed, keeping its journal, until the owner restores the host inbox or explicitly resets the producer journal. The producer must save consumed provider data before the provider acknowledges it, or rely on a verified provider replay guarantee. This SDK cannot recover data that a provider library acknowledges before the plugin's first durable journal write.

`storage` is a required `ChannelIngressStorage` object with an absolute `journal_path` and optional checked `recovery` in `start_ingress`, `poll_ingress`, and `ingress_event` requests. `ChannelIngressRecovery` carries the host's accepted original `request`, matching `acceptance`, and effective accepted `state`. The host commits that state with each head: use the original batch state when present, or retain the preceding durable head's accepted state when the original omits it. Recovery validation requires equality when the original includes state; an absent effective state means no accepted cursor. Producers restore this checked accepted state, including inherited state, rather than using a control-response cursor. The host derives the storage path and recovery from its authenticated storage scope; provider configuration and request JSON cannot choose them. The producer restores an accepted original and cursor only when it has no conflicting pending original. A pending original that matches the recovered request can accept that same receipt; a pending successor whose predecessor matches the recovered receipt remains pending. Other conflicts fail without erasing the journal. Relative paths, parent traversal, control characters, and paths without a file name are rejected. The plugin journal needs storage that survives the failures its provider cannot replay. A generic host cannot infer whether a deployment volume is durable or encrypted, so the deployment owner must provide that guarantee.

`channel.heartbeat` is a notification containing `protocol_version` and string `metadata` only. It reports liveness, never advances a cursor, and never confirms acceptance. A busy host may drop heartbeats. An `ingress_events_received` result that carries inline `events`, and notification-form `channel.event`, are rejected; other result members are ignored as before.

Generic SDK hosts implement `DurableChannelEventSink`. Its `accept` method returns the exact receipt only after the complete original batch is durable; an error leaves the batch unacknowledged. The host writes receipt bytes only after `accept` returns a matching receipt:

- `call_channel_plugin_with_sink` runs one request in a fresh plugin process and keeps plugin stdin open until the control response, so the plugin can wait for each receipt.
- `call_persistent_channel_plugin_with_sink` accepts batches that arrive during any persistent control request, including `start_ingress` and `deliver`.
- `call_persistent_channel_plugin` queues batches that arrive during a control request without acknowledging them; the host acknowledges them later with `accept_persistent_channel_event`.
- `recv_persistent_channel_ingress` returns spontaneous batches and heartbeats.

A call accepts at most 32 batches. A `poll_ingress` or `ingress_event` success without at least one batch receipt is rejected, and those requests require a sink. A batch sent during a one-shot call without a sink is a protocol error.

### Ingress event flow

When the host receives a webhook POST for a channel:

1. Host sends `ingress_event` with the raw HTTP payload
2. Host may validate a declared host-managed trust policy and set `trust_verified`
3. Plugin validates the signature only when `trust_verified` is false
4. Plugin parses the platform-specific payload
5. Plugin sends the original normalized batch, which may be empty, in an ID-bearing `channel.event` request
6. Host saves the complete batch and returns its exact receipt
7. Plugin returns the control response
8. Host handles the accepted events and returns the optional `callback_reply` to the webhook caller

Installed channel manifests may also retain ingress endpoint declarations so the host can match a request path and method before forwarding the payload to the plugin.

When using `dispatch channel listen`, the host sends `start_ingress` before entering the HTTP serve loop, forwards webhook payloads through `ingress_event` requests, and finally sends `stop_ingress` followed by `shutdown` when the listener exits cleanly.

For long-running polling or websocket bindings, the host sends `start_ingress` with its journal storage and the last accepted opaque state. It then receives `channel.event` requests. Each batch carries:

- `events` - zero or more normalized inbound events
- `state?` - updated opaque ingress state such as cursors
- `poll_after_ms?` - plugin-provided advisory pacing metadata

The host saves cursor progress only from a durably accepted original batch. State in `ingress_started`, `ingress_stopped`, `ingress_events_received`, or a heartbeat is not acceptance evidence and never becomes the saved cursor.

For one-shot polling, Dispatch keeps plugin stdin open while it saves and acknowledges batches. The plugin then returns `ingress_events_received` and exits. A replayed batch receives the same exact receipt without starting another parcel turn.

`callback_reply` is only valid for webhook `ingress_event` handling. Polling and websocket control responses leave it unset.

### CLI acceptance inbox

The CLI host stores accepted batches in an encrypted SQLite inbox. Run `dispatch secret init` for the project before receiving channel batches; the inbox uses that existing Dispatch secrets key and never creates a key. The key resolves from the inbox path like other project secrets: the nearest `.dispatch` directory above the inbox, or `DISPATCH_SECRET_STORE_ROOT` when set. Use the same setting for the host and for owner commands. Replacing the key, for example with `dispatch secret init --force`, makes the existing inbox unreadable.

Each binding stores its cursor state at `<project>/.dispatch/channel-state/<plugin>/<label>-<config-hash>.json`. The inbox is the same path with the `.inbox.sqlite` extension, and the plugin journal path sent as `storage.journal_path` uses `.producer.sqlite`. On Unix, the host keeps that `<plugin>` scope directory private to its owner (mode 0700) because it also holds the producer journal, and the inbox file must stay private (mode 0600) and must not be a symbolic link. Each encrypted record is authenticated against its location below the Dispatch root and its batch digest, so the project directory can move, but an inbox file copied to another binding path cannot be read there. Preserve the inbox, the plugin journal, and the secrets key across host restarts. A temporary filesystem does not provide restart durability, and restoring an older inbox backup rolls back its acceptance history.

When the host accepts a batch, it commits the encrypted original, receipt, event-identity receipts, and effective cursor in one transaction before returning the receipt. That committed head is the durable cursor. The host then projects it to the state file; if that projection fails, the host warns and the next open repairs it. Once a head exists, the state file mirrors the head's effective cursor exactly: opening the inbox rewrites the file, or removes it when the head has no accepted cursor, and bindings start from the head's cursor. The first head inherits a cursor that a host saved in the state file before the inbox existed when its original batch has no state. The host handles newly accepted events even when the control response fails afterwards. After it emits the events and confirms successful configured parcel runs and reply delivery, it retires their pending payload. The inbox retains one encrypted current head, including the full original and receipt, for recovery and exact replay. A successor replaces that head and removes completed predecessor records. Empty progress therefore retains one head rather than a growing batch history.

Batches whose handling failed or was interrupted stay pending. The CLI never reruns them automatically, because an interrupted turn can have unknown side effects, and it warns about pending batches when a binding starts. If the owner retires a batch before the host that accepted it handles it, that host skips the batch with a warning. `list` and `inspect` open the inbox read-only; after a host crash leaves an SQLite rollback journal, they can fail until a host or `retire` opens the inbox read-write. The inbox retains at most 256 pending batches. When it is full, occurrences with new executable events are refused without a receipt, so the producer keeps them, until the owner retires pending work. Descriptor pages contain at most 32 records and no batch content; each descriptor exposes its positive receipt `sequence` and content digest. Owner inspection and retirement use that sequence because several retained occurrences can share a digest. Every encrypted record authenticates its digest and sequence.

Inspect pending work before retiring it:

```sh
dispatch channel inbox list .dispatch/channel-state/<plugin>/<label>-<config-hash>.inbox.sqlite --limit 32
dispatch channel inbox inspect .dispatch/channel-state/<plugin>/<label>-<config-hash>.inbox.sqlite <sequence>
```

Retirement clears pending work. The current head retains its full encrypted original for producer recovery until a successor replaces it. Before you retire a batch, verify that its work finished or decide explicitly to abandon it:

```sh
dispatch channel inbox retire .dispatch/channel-state/<plugin>/<label>-<config-hash>.inbox.sqlite <sequence>
```

Retirement restores pending capacity. It preserves the full current head until a successor replaces it, and removes completed older batch records entirely, so no batch-level marker remains for them. Event-identity and content receipts remain as durable side-effect idempotency receipts. They are HMAC-SHA256 tags under a key derived from the existing Dispatch key, so they reveal nothing about event content without that key, but they are not encrypted records: anyone who can modify the inbox file can delete or corrupt them. Those rows grow with actual events, not empty polls, and the CLI has no command to prune them. A batch refused for changed content under an accepted event ID stops that binding until the producer or owner resolves it. If a later successor regroups an identical accepted event, the host receipts the full batch but does not offer that event for execution again. It refuses changed original content under an accepted event ID. `canonical_channel_event_content_bytes` defines event retry identity as the compact canonical JSON of the complete typed event object, excluding only its top-level `received_at` host observation. It retains actor, routing, activation, content, attachments, and all metadata, including nested observation-like fields. Hosts apply their keyed event-content tag to those bytes. A later observation of the same semantic event adds no executable work and does not replace its first stored original. Full canonical batch bytes, batch digests, and original frames still include `received_at` and remain exact. Producers retain stable provider fields and metadata across retries. Preserve the inbox file to preserve those receipts; restoring an older backup also rolls back its acceptance history. `dispatch channel call` and `dispatch channel ingress` use the binding scope whose label is the plugin name, so they share the inbox and cursor of `dispatch channel poll <plugin>`. The `events` field in CLI JSON output is local output, not part of the plugin wire response.

### Parcel reply bridge

When `dispatch channel listen ... --deliver-replies` or `dispatch channel poll ... --deliver-replies` is used, Dispatch runs the configured parcel for each inbound event and then sends the assistant reply back through the originating channel.

By default that bridge forwards plain assistant text:

- streamed text deltas and final assistant message text are forwarded
- reply routing metadata (`conversation_id`, `thread_id`, `reply_to_message_id`) is preserved

Courier plugins can emit a first-class `channel_reply` event directly. Host-backed Dispatch couriers also upgrade a tagged JSON assistant reply into the same structured event:

```json
{
  "kind": "channel_reply",
  "content": "Dispatch attached the report.",
  "content_type": "text/plain",
  "attachments": [
    {
      "name": "report.txt",
      "mime_type": "text/plain",
      "data_base64": "aGVsbG8="
    }
  ],
  "metadata": {
    "custom": "value"
  }
}
```

When Dispatch sees `kind = "channel_reply"`:

- `attachments` is forwarded to the channel plugin
- custom metadata is preserved
- reply routing metadata from the inbound event still overrides any conflicting values in the assistant payload

When the reply bridge runs under `dispatch channel listen`, Dispatch can also stage inline `data_base64` attachments behind a listener-owned URL when the target channel only accepts URL-backed media. This requires `config.webhook_public_url` so the host can generate a public fetch URL for the staged media. Polling channels do not expose that listener-backed staging path, so `dispatch channel poll ... --deliver-replies` still requires the reply to use an attachment source the channel already accepts directly.

If a workflow needs stricter control over attachment delivery, call the channel plugin directly with an explicit `deliver` or `push` request that includes `attachments`, or emit the tagged `channel_reply` envelope shown above from the parcel.

### Status frame kinds

Dispatch defines a shared vocabulary for status updates:

| Kind | Meaning |
|---|---|
| `processing` | Agent is working on a response |
| `completed` | Agent finished |
| `cancelled` | Processing was cancelled or timed out |
| `operation_started` | An extension or courier action started |
| `operation_finished` | An extension or courier action finished |
| `approval_needed` | Action requires user/operator approval |
| `info` | Informational text to relay |
| `delivering` | Channel is actively sending |
| `auth_required` | End-user authentication needed |

Plugins may reject status kinds they don't support by responding with `accepted: false`.

## Security model

Channel plugin manifests may declare:

- Required and optional secrets
- Network domains they access
- Ingress endpoints they register
- Platforms they support

Dispatch does:

- Subprocess isolation (extensions run as separate processes)
- Artifact integrity (SHA-256 hash stored at install time)
- Explicit install into the local channel registry

Dispatch does not enforce, at the channel-plugin host layer shown here:

- Per-plugin network restrictions based on declared domains
- Per-plugin filesystem sandboxing
- Automatic secret injection based on manifest requirements
- Enable/disable or activation state beyond installation

Channel plugins run out-of-process and do not execute in-process host memory, but they inherit the normal process environment and OS access of the Dispatch process unless additional runtime isolation is added elsewhere.

## Repository layout

Official extensions live in a separate repository:

```
dispatch-plugins/
  channels/
    schema/         # Shared manifest and catalog schema crate
    telegram/       # channel-telegram
    discord/        # channel-discord
    slack/          # channel-slack
    webhook/        # channel-webhook
    whatsapp/       # channel-whatsapp
    twilio-sms/     # channel-twilio-sms
    signal/         # channel-signal
  catalog/
    extensions.json
```

Vendor-specific extensions live in their own repositories.

## Discovery via catalogs

Every plugin in the ecosystem does not live in `dispatch-plugins`. Third-party and vendor-specific extensions can ship in their own repositories. Dispatch discovers them through **catalogs** - JSON index documents published at stable URLs.

A catalog is just an `extensions.json` document listing entries. The canonical example lives at `https://raw.githubusercontent.com/serenorg/dispatch-plugins/master/catalog/extensions.json`. Any 3rd-party plugin repository can publish the same schema. Remote catalogs must declare a top-level `catalog_id`, and every entry must declare an immutable `id`. Those ids use lowercase ASCII letters, digits, `.`, `-`, and `_`, are limited to 80 characters, and cannot start or end with `.` or whitespace.

### Registering a catalog

```bash
# Register the main dispatch-plugins catalog
dispatch extension catalog add \
  https://raw.githubusercontent.com/serenorg/dispatch-plugins/master/catalog/extensions.json

# Register a vendor-specific catalog
dispatch extension catalog add \
  https://raw.githubusercontent.com/example/my-dispatch-extensions/main/catalog/extensions.json

# See what you have
dispatch extension catalog ls

# Populate the local cache
dispatch extension catalog refresh
```

Catalogs are stored in `~/.config/dispatch/catalogs.toml` and fetched JSON is cached under `~/.config/dispatch/catalog-cache/<name>.json`. Cache refresh is explicit - search and show read the cache, they do not fetch on every call.

### Searching and inspecting

```bash
# Free-text search across all cached catalogs
dispatch extension search telegram

# Filter by kind
dispatch extension search --kind courier

# JSON output for scripts
dispatch extension search --json

# Show the full entry for a specific extension
dispatch extension show seren-cloud
```

`dispatch extension show` prints the id, name, version, catalog, catalog_id, description, install hint, requirements, manifest location, and any machine-installable source metadata.

If a catalog entry publishes a `source` block with a direct GitHub release binary, Dispatch can install it by name:

```bash
dispatch extension install <name>
```

The current install-by-name flow is intentionally narrow:

- it only handles direct GitHub release binaries, not archive extraction
- it requires an absolute, version-pinned `manifest_url` in the catalog entry
- it still ends by calling the normal category-specific install flow with a rewritten manifest

Capability-based trust remains follow-up work; see [`plugin-ecosystem.md`](./plugin-ecosystem.md).

## Design principles

1. **Separate extension categories explicitly.** Couriers, channels, providers, databases, and deployments have different trust, lifecycle, and runtime requirements.

2. **Keep the parcel contract primary.** Extensions work around parcels and runs. They do not replace the parcel as the core unit of portability.

3. **Stay vendor-neutral.** Dispatch defines protocols, not cloud-provider semantics. No provider gets special treatment in core.

4. **Prefer capability declaration.** Extensions advertise what they support rather than relying on special names.

5. **Keep the core repo narrow.** Standards, protocols, reference implementations, and install mechanics live in core. Platform adapters and provider integrations live outside.
