# Dispatch Extensions

Dispatch supports three categories of extensions:

| Category | Purpose | Status |
|---|---|---|
| Courier plugins | Alternate runtime backends for parcel execution | Stable |
| Channel plugins | Messaging and webhook transport integrations | Provisional |
| Connector bundles | Reusable tool/provider packages | Future |

Extensions live outside the core repository and communicate with Dispatch over
JSONL subprocess protocols.

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
- `deliver` - send a reply to an existing conversation
- `push` - send a proactive message (broadcast, alert, scheduled)
- `status` - relay agent progress to the conversation (typing indicators, etc.)

Examples: Telegram, Discord, Slack, WhatsApp, Twilio SMS, generic
webhooks.

### Connector bundles (future)

Reusable tool packages for specific providers (Gmail, GitHub, Google Drive).
No first-class extension type exists yet. These remain packaged local tools
until reuse patterns emerge.

## Installing extensions

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
```

## Extension manifest format

Both courier and channel plugins use a JSON manifest file that declares
metadata, entrypoint, capabilities, and requirements.

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
      "ingress_modes": ["webhook"],
      "outbound_message_types": ["text"],
      "threading_model": "chat_or_topic",
      "attachment_support": false,
      "reply_verification_support": true,
      "account_scoped_config": true,
      "allow_polling": false,
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
| `deliver` | `config`, `message` | Send reply to existing conversation |
| `push` | `config`, `message` | Send proactive/broadcast message |
| `status` | `config`, `update` | Relay agent progress to channel |

### Response types

| Kind | Fields | Purpose |
|---|---|---|
| `capabilities` | `capabilities` | Plugin feature declaration |
| `configured` | `configuration` | Channel metadata and policy |
| `health` | `health` | Connectivity report |
| `ingress_started` | `state` | Webhook registered |
| `ingress_stopped` | `state` | Webhook deregistered |
| `ingress_events_received` | `events`, `callback_reply?` | Parsed inbound events |
| `delivered` | `delivery` | Delivery receipt |
| `pushed` | `delivery` | Push delivery receipt |
| `status_accepted` | `status` | Status acknowledgment |
| `error` | `error` | Error with code and message |

### Ingress event flow

When the host receives a webhook POST for a channel:

1. Host sends `ingress_event` with the raw HTTP payload
2. Plugin validates the signature (unless `trust_verified` is true)
3. Plugin parses the platform-specific payload
4. Plugin responds with normalized `InboundEventEnvelope`s
5. Host returns the optional `callback_reply` to the webhook caller

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

Dispatch currently does:

- Subprocess isolation (extensions run as separate processes)
- Artifact integrity (SHA-256 hash stored at install time)
- Explicit install into the local channel registry

Dispatch does not yet enforce, at the channel-plugin host layer shown here:

- Per-plugin network restrictions based on declared domains
- Per-plugin filesystem sandboxing
- Automatic secret injection based on manifest requirements
- Enable/disable or activation state beyond installation

Channel plugins still run out-of-process and do not execute in-process host
memory, but they currently inherit the normal process environment and OS access
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
