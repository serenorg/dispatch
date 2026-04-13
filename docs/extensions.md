# Dispatch Extensions

Dispatch supports three categories of extensions:

| Category | Purpose | Status |
|---|---|---|
| Courier plugins | Alternate runtime backends for parcel execution | Stable |
| Channel plugins | Messaging and webhook transport integrations | Provisional |
| Connector bundles | Reusable tool/provider packages | Planned |

Extensions live outside the core repository and communicate with Dispatch over
JSONL subprocess protocols.

Dispatch has first-class install/runtime support for courier plugins and
channel plugins. Connector bundles are a planned category without a host
registry or execution model.

## Layering

Dispatch keeps authored parcel source separate from host-managed extension
inventory.

- `Agentfile` is the canonical authored source for a parcel. It defines the
  agent's prompt stack, tools, model policy, mounts, and generic runtime
  intent that should survive build, review, and signing.
- installed courier plugins and channel plugins are host inventory. They are
  local runtime capabilities available to an operator on a specific machine or
  environment.
- extension install commands do not mutate parcel source. They populate a host
  registry that runtime commands can resolve by name.

That split is intentional. Courier binaries, channel adapter binaries, webhook
URLs, and platform credentials are deployment concerns, not parcel build
inputs.

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

Examples: managed cloud runtimes, self-hosted cluster runners, specialized
sandbox backends.

### Channel plugins

Channel plugins are messaging adapters. They translate between external
platforms and Dispatch's inbound/outbound event model.

A channel plugin implements:

- `capabilities` - declare platform, ingress modes, threading model
- `configure` - validate credentials and report channel metadata
- `health` - verify connectivity and account identity
- `start_ingress` / `stop_ingress` - register/deregister webhook endpoints
- `ingress_event` - parse a raw webhook payload into normalized events
- `poll_ingress` - fetch inbound events for polling transports
- `deliver` - send a reply to an existing conversation
- `push` - send a proactive message (broadcast, alert, scheduled)
- `status` - relay agent progress to the conversation (typing indicators, etc.)

Examples: Telegram, Discord, Slack, WhatsApp, Signal, Twilio SMS, generic
webhooks.

Channel plugins vary by platform, but the protocol covers the full inbound and
outbound lifecycle: configuration, health, ingress setup, webhook forwarding or
polling, delivery, push, and status frames.

### Connector bundles

Reusable tool packages for specific providers (Gmail, GitHub, Google Drive).
No first-class extension type exists for connector bundles. These remain
packaged local tools until reuse patterns emerge.

## Managing host extension inventory

### Courier plugins

```sh
dispatch courier install path/to/courier-plugin.json
dispatch courier ls
dispatch courier inspect <name>
```

### Channel plugins

```sh
dispatch channel install path/to/channel-plugin.json
dispatch channel ls
dispatch channel inspect <name>
dispatch channel call <name> --request-json '{"kind":"capabilities"}'
dispatch channel call channel-telegram --request-file telegram-deliver.json
dispatch channel ingress --path /telegram/updates --header X-Telegram-Bot-Api-Secret-Token=... --body-file update.json
dispatch channel listen channel-telegram --listen 127.0.0.1:8787 --config-file telegram-config.json
dispatch channel listen channel-telegram --listen 127.0.0.1:8787 --config-file telegram-config.json --parcel ./Agentfile --session-root ./.dispatch/channel-sessions --deliver-replies
dispatch channel poll channel-telegram --config-file telegram-config.json --once
```

These commands manage the local host registry, not parcel source. They are the
operator-facing way to install and inspect runtime extensions that exist
outside any one parcel.

Channel plugin subprocess calls use a fixed 30s host-side timeout.

`dispatch channel call` forwards the request JSON directly to the plugin. That
is the most direct way to exercise channel-specific delivery features such as
attachments without depending on the parcel reply bridge conventions.

`dispatch channel listen` and `dispatch channel poll` are low-level runtime
commands. They are useful for development, testing, and direct operator
control, but they are not the canonical authored configuration surface for
project-specific channel wiring.

Dispatch still needs a separate declarative runtime binding layer for cases
such as "bind installed channel X to parcel Y with config Z". That binding
should live outside the `Agentfile` so parcel source stays portable and
reviewable without embedding local extension inventory details.

## Extension manifest format

Dispatch has two manifest shapes:

- Courier plugins use a compact `courier-plugin.json` consumed by the courier
  registry installer.
- Channel plugins use a richer `channel-plugin.json` that includes bootstrap,
  auth, capability, ingress, and requirement metadata.

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

Channel plugins communicate over newline-delimited JSON on stdin/stdout.

### Wire format

Each request from the host is a single JSON line:

```json
{"protocol_version":1,"request":{"kind":"capabilities"}}
```

Each response from the plugin is a single JSON line:

```json
{"kind":"capabilities","capabilities":{"plugin_id":"telegram","platform":"telegram",...}}
```

### Request types

| Kind | Fields | Purpose |
|---|---|---|
| `capabilities` | (none) | Query plugin features |
| `configure` | `config` | Validate config, report metadata |
| `health` | `config` | Verify credentials and connectivity |
| `start_ingress` | `config` | Register webhook / begin listening |
| `stop_ingress` | `config`, `state?` | Deregister webhook / stop listening |
| `ingress_event` | `config`, `payload` | Parse a raw webhook into events |
| `poll_ingress` | `config`, `state?` | Fetch inbound events for polling ingress |
| `deliver` | `config`, `message` | Send reply to existing conversation |
| `push` | `config`, `message` | Send proactive/broadcast message |
| `status` | `config`, `update` | Relay agent progress to channel |

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
| `error` | `error` | Error with code and message |

### Ingress event flow

When the host receives a webhook POST for a channel:

1. Host sends `ingress_event` with the raw HTTP payload
2. Host may validate a declared host-managed trust policy and set `trust_verified`
3. Plugin validates the signature only when `trust_verified` is false
4. Plugin parses the platform-specific payload
5. Plugin responds with normalized `InboundEventEnvelope`s
6. Host returns the optional `callback_reply` to the webhook caller

Installed channel manifests may also retain ingress endpoint declarations so
the host can match a request path and method before forwarding the payload to
the plugin.

When using `dispatch channel listen`, the host also sends `start_ingress`
before entering the HTTP serve loop and sends `stop_ingress` with the last
reported ingress state when the listener exits cleanly.

Polling channels use a similar lifecycle:

1. Host sends `start_ingress`
2. Plugin responds with `state.mode = "polling"` and any opaque cursor state
3. Host loops on `poll_ingress { state? }`
4. Plugin responds with `events`, updated `state`, and optional `poll_after_ms`
5. Host sends `stop_ingress` with the last state when polling stops

`callback_reply` is only valid for webhook `ingress_event` handling. Polling
responses should leave it unset.

### Parcel reply bridge

When `dispatch channel listen ... --deliver-replies` or
`dispatch channel poll ... --deliver-replies` is used, Dispatch runs the
configured parcel for each inbound event and then sends the assistant reply
back through the originating channel.

By default that bridge forwards plain assistant text:

- streamed text deltas and final assistant message text are forwarded
- reply routing metadata (`conversation_id`, `thread_id`, `reply_to_message_id`)
  is preserved

Courier plugins can emit a first-class `channel_reply` event directly. Host-backed
Dispatch couriers also upgrade a tagged JSON assistant reply into the same
structured event:

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
- reply routing metadata from the inbound event still overrides any conflicting
  values in the assistant payload

When the reply bridge runs under `dispatch channel listen`, Dispatch can also
stage inline `data_base64` attachments behind a listener-owned URL when the
target channel only accepts URL-backed media. This requires
`config.webhook_public_url` so the host can generate a public fetch URL for the
staged media. Polling channels do not expose that listener-backed staging path,
so `dispatch channel poll ... --deliver-replies` still requires the reply to use
an attachment source the channel already accepts directly.

If a workflow needs stricter control over attachment delivery, call the channel
plugin directly with an explicit `deliver` or `push` request that includes
`attachments`, or emit the tagged `channel_reply` envelope shown above from the
parcel.

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

Plugins may reject status kinds they don't support by responding with
`accepted: false`.

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

Channel plugins run out-of-process and do not execute in-process host memory,
but they inherit the normal process environment and OS access
of the Dispatch process unless additional runtime isolation is added elsewhere.

## Repository layout

Official extensions live in a separate repository:

```
dispatch-plugins/
  channels/
    proto/          # Shared protocol crate (channel-proto)
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

Vendor-specific extensions (e.g., seren-cloud courier) live in their own
repositories.

## Design principles

1. **Separate extension categories explicitly.** Couriers, channels, and
    connectors have different trust, lifecycle, and runtime requirements.

2. **Keep the parcel contract primary.** Extensions work around parcels and
    runs. They do not replace the parcel as the core unit of portability.

3. **Stay vendor-neutral.** Dispatch defines protocols, not cloud-provider
    semantics. No provider gets special treatment in core.

4. **Prefer capability declaration.** Extensions advertise what they support
    rather than relying on special names.

5. **Keep the core repo narrow.** Standards, protocols, reference
    implementations, and install mechanics live in core. Platform adapters
    and provider integrations live outside.
