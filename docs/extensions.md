# Dispatch Extensions

Dispatch supports the following extension categories:

| Category | Purpose | Status |
|---|---|---|
| Courier plugins | Alternate runtime backends for parcel execution | Stable |
| Channel plugins | Messaging and webhook transport integrations | Provisional |
| Provider plugins | External LLM inference backends | Draft (v0.4.0) |
| Database plugins | Read+write database backends exposed to parcels as tools | Draft (v0.4.0) |
| Deployment plugins | Managed deployment lifecycle control planes | Draft (v0.4.0) |
| Connector bundles | Reusable tool/provider packages | Superseded by Provider + Database plugins |

Extensions live outside the core repository and communicate with Dispatch over JSON-RPC 2.0 messages framed as line-delimited JSON on stdio.

Dispatch has first-class install/runtime support for courier plugins, channel plugins, and deployment plugins today. Provider and database plugins are in draft against v0.4.0; see [`provider-plugin-protocol.md`](./provider-plugin-protocol.md) and [`database-plugin-protocol.md`](./database-plugin-protocol.md) for the in-progress specifications. The previously-planned "connector bundles" category is subsumed by these concrete plugin kinds.

For discovering third-party extensions published in separate repositories, see [Discovery via catalogs](#discovery-via-catalogs) below. The broader ecosystem roadmap lives in [`plugin-ecosystem.md`](./plugin-ecosystem.md).

## Layering

Dispatch keeps authored parcel source separate from host-managed extension inventory.

- `Agentfile` is the canonical authored source for a parcel. It defines the agent's prompt stack, tools, model policy, mounts, and generic runtime intent that should survive build, review, and signing.
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

Examples: managed cloud runtimes, self-hosted cluster runners, specialized sandbox backends.

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
- `status` - relay agent progress to the conversation (typing indicators, etc.)
- `shutdown` - allow the host to terminate a persistent ingress process cleanly

Examples: Telegram, Discord, Slack, WhatsApp, Signal, Twilio SMS, generic webhooks.

Channel plugins vary by platform, but the protocol covers the full inbound and outbound lifecycle: configuration, health, ingress setup, webhook forwarding or background receive loops, delivery, push, and status frames.

Dispatch keeps channel plugins alive as persistent subprocesses while a listener or long-running poller is active. The host still speaks JSON-RPC 2.0 messages over JSONL stdio, but inbound channel activity is delivered back to the host as `channel.event` notifications during persistent ingress sessions. Dispatch also supports a one-shot `poll_ingress` request for explicit single-cycle polling.

For channel ingress, `start_ingress` is the primary session-oriented contract. `poll_ingress` remains as the auxiliary one-shot receive primitive used when the host explicitly wants a single fetch cycle rather than a long-lived ingress session.

That separation is intentional. Dispatch controls the plugin lifecycle and the stdio framing; the plugin is free to translate that into whatever upstream transport best matches the platform, including repeated HTTP polling, upstream websockets, or daemon-backed local IPC.

As a design rule, Dispatch defaults plugin interactions to one-shot request/response cycles and opts into persistence only when the domain is inherently sessioned or event-driven. Channel plugins fall into that second category: they may need to hold an upstream connection open, maintain ingress state across calls, or emit events on the platform's schedule rather than the host's schedule.

For channel plugins specifically:

- The long-lived process is scoped to an active channel ingress session, not to a single agent turn and not to the entire host forever.
- Host-initiated operations such as `configure`, `health`, `deliver`, `push`, `status`, `poll_ingress`, `stop_ingress`, and `shutdown` remain ordinary JSON-RPC requests with terminal responses.
- Spontaneous inbound activity is modeled as `channel.event` notifications because the host did not initiate those events at a specific moment.

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

- `capabilities` - declare lifecycle features and supported templates or policies
- `configure` - validate credentials and endpoint
- `health` - verify connectivity
- `validate` - check a candidate deployment spec without side effects
- `test_run` - run a draft preflight without creating a long-lived deployment
- `deploy`, `update`, `preview_update` - manage deployment revisions
- `list`, `get`, `list_revisions` - inspect deployment state
- `preview_rollback`, `rollback` - inspect and apply revision rollback
- `start`, `stop`, `delete` - manage lifecycle state
- `shutdown` - clean up

Examples: managed-agent control planes such as `seren-agent`.

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
dispatch channel listen channel-telegram --listen 127.0.0.1:8787 --config-file telegram-config.toml --parcel ./Agentfile --session-root ./.dispatch/channel-sessions --deliver-replies
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
- Slack polling uses Socket Mode: the plugin opens Slack's websocket via `apps.connections.open`, acknowledges each `envelope_id`, and emits normalized ingress notifications to the host.

`dispatch up` is the project-level runtime binding command. It reads `dispatch.toml`, reconciles declared extension manifests into project-local registries under `.dispatch/registries/`, and starts the configured channel bindings without mutating the global registries under `~/.config/dispatch/`. Use `dispatch up --dry-run` to preview installs and channel bindings without mutating registries or starting listeners/pollers.

Minimal `dispatch.toml` example:

```toml
parcel = "./Agentfile"
courier = "native"

[[extensions]]
manifest = "../dispatch-plugins/channels/telegram/channel-plugin.json"

[[channels]]
plugin = "channel-telegram"
mode = "listen"
listen = "127.0.0.1:8787"
deliver_replies = true
config_file = "./config/telegram.toml"
```

Channel binding config files may be JSON or TOML. Inline `config = { ... }` tables in `dispatch.toml` are also supported.

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
  "protocol_version": 1,
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
      "ingress_modes": ["webhook", "polling"],
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
    "protocol_version": 1,
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

Dispatch does not currently use JSON-RPC batch requests for channel plugins. The host keeps at most one request in flight per plugin stream and expects each terminal response `id` to match the active request.

### Request types

| Kind | Fields | Purpose |
|---|---|---|
| `capabilities` | (none) | Query plugin features |
| `configure` | `config` | Validate config, report metadata |
| `health` | `config` | Verify credentials and connectivity |
| `poll_ingress` | `config`, `state?` | Perform one polling cycle and return events directly |
| `start_ingress` | `config`, `state?` | Start or resume an ingress session |
| `stop_ingress` | `config`, `state?` | Deregister webhook / stop listening |
| `ingress_event` | `config`, `state?`, `payload` | Parse a raw webhook into events |
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
| `ingress_events_received` | `events`, `callback_reply?`, `state?`, `poll_after_ms?` | Parsed inbound events |
| `delivered` | `delivery` | Delivery receipt |
| `pushed` | `delivery` | Push delivery receipt |
| `status_accepted` | `status` | Status acknowledgment |
| `ok` | (none) | Generic success for shutdown/ack-only operations |

### Notification types

Channel plugins may emit JSON-RPC notifications between requests while an ingress session is active:

| Method | Params | Purpose |
|---|---|---|
| `channel.event` | `protocol_version`, `events`, `state?`, `poll_after_ms?` | Deliver normalized inbound events from a persistent ingress session |

Dispatch does not currently use JSON-RPC batch requests for channel plugins, and the host never pipelines multiple concurrent requests into the same plugin process. Notifications are therefore the only async message shape on the channel side of the protocol.

### Ingress event flow

When the host receives a webhook POST for a channel:

1. Host sends `ingress_event` with the raw HTTP payload
2. Host may validate a declared host-managed trust policy and set `trust_verified`
3. Plugin validates the signature only when `trust_verified` is false
4. Plugin parses the platform-specific payload
5. Plugin responds with normalized `InboundEventEnvelope`s
6. Host returns the optional `callback_reply` to the webhook caller

Installed channel manifests may also retain ingress endpoint declarations so the host can match a request path and method before forwarding the payload to the plugin.

When using `dispatch channel listen`, the host sends `start_ingress` before entering the HTTP serve loop, forwards webhook payloads through `ingress_event` requests, and finally sends `stop_ingress` followed by `shutdown` when the listener exits cleanly.

For long-running polling bindings, the host sends `start_ingress` with the last saved opaque state (if any), then waits for `channel.event` notifications from the plugin. Each notification carries:

- `events` - zero or more normalized inbound events
- `state?` - updated opaque ingress state such as cursors
- `poll_after_ms?` - plugin-provided advisory pacing metadata

The host checkpoints `state` between runs. This lets repeated `dispatch channel poll --once` invocations resume from the last plugin cursor without replaying old events.

For CLI-driven one-shot polling, Dispatch sends `poll_ingress` with the last saved opaque state, expects a single `ingress_events_received` response, saves any returned `state`, and exits. This path is useful when the plugin's upstream transport is itself long-poll or websocket-backed and a true one-shot fetch is more appropriate than starting a persistent ingress session only to tear it down immediately.

`callback_reply` is only valid for webhook `ingress_event` handling. Polling notifications should leave it unset because they are not a direct HTTP callback path.

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

Vendor-specific extensions (e.g., seren-cloud courier) live in their own repositories.

## Discovery via catalogs

Every plugin in the ecosystem does not live in `dispatch-plugins`. Third-party couriers and channels ship in their own repositories (e.g. `dispatch-courier-seren-cloud`). Dispatch discovers them through **catalogs** - JSON index documents published at stable URLs.

A catalog is just an `extensions.json` document listing entries. The canonical example lives at `https://raw.githubusercontent.com/serenorg/dispatch-plugins/master/catalog/extensions.json`. Any 3rd-party plugin repository can publish the same schema.

### Registering a catalog

```bash
# Register the main dispatch-plugins catalog
dispatch extension catalog add \
  https://raw.githubusercontent.com/serenorg/dispatch-plugins/master/catalog/extensions.json

# Register a vendor-specific catalog
dispatch extension catalog add \
  https://raw.githubusercontent.com/serenorg/dispatch-courier-seren-cloud/main/catalog/extensions.json

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

`dispatch extension show` prints the name, version, catalog, description, install hint, requirements, manifest location, and any machine-installable source metadata.

If a catalog entry publishes a `source` block with a direct GitHub release binary, Dispatch can install it by name:

```bash
dispatch extension install <name>
```

The current install-by-name flow is intentionally narrow:

- it only handles direct GitHub release binaries, not archive extraction
- it requires an absolute, version-pinned `manifest_url` in the catalog entry
- it still ends by calling the normal `dispatch courier install` or `dispatch channel install` flow with a rewritten manifest

Capability-based trust remains follow-up work; see [`plugin-ecosystem.md`](./plugin-ecosystem.md).

## Design principles

1. **Separate extension categories explicitly.** Couriers, channels, and connectors have different trust, lifecycle, and runtime requirements.

2. **Keep the parcel contract primary.** Extensions work around parcels and runs. They do not replace the parcel as the core unit of portability.

3. **Stay vendor-neutral.** Dispatch defines protocols, not cloud-provider semantics. No provider gets special treatment in core.

4. **Prefer capability declaration.** Extensions advertise what they support rather than relying on special names.

5. **Keep the core repo narrow.** Standards, protocols, reference implementations, and install mechanics live in core. Platform adapters and provider integrations live outside.
