# A2A Tools

Dispatch supports host-executed remote agent-to-agent tools through A2A tools declared as `[[agent.tools]]` entries with `kind = "a2a"`.

## Parcel Contract

Basic examples:

```toml
[[agent.secrets]]
name = "PLANNER_TOKEN"

[[agent.secrets]]
name = "SEARCH_TOKEN"

[[agent.secrets]]
name = "BACKOFFICE_USER"

[[agent.secrets]]
name = "BACKOFFICE_PASSWORD"

[[agent.tools]]
kind = "a2a"
alias = "planner"
url = "https://planner.example.com"
discovery = "card"
expect_agent_name = "planner-agent"

[agent.tools.auth]
scheme = "bearer"
secret_name = "PLANNER_TOKEN"

[[agent.tools]]
kind = "a2a"
alias = "search"
url = "https://search.example.com"

[agent.tools.auth]
scheme = "header"
header_name = "X-Api-Key"
secret_name = "SEARCH_TOKEN"

[[agent.tools]]
kind = "a2a"
alias = "backoffice"
url = "https://backoffice.example.com"

[agent.tools.auth]
scheme = "basic"
username_secret_name = "BACKOFFICE_USER"
password_secret_name = "BACKOFFICE_PASSWORD"
```

Supported fields:

- `url` - required endpoint
- `discovery` - `auto`, `card`, or `direct`
- `schema` - JSON Schema path for the tool input
- `expect_agent_name` and `expect_card_sha256` - discovered-card identity requirements
- `approval`, `risk`, `description`

Credentials bind through a nested `[agent.tools.auth]` table with a `scheme` of `bearer`, `header`, or `basic`. Every referenced secret must also be declared in `[[agent.secrets]]`.

Semantics:

- the endpoint is declared statically in the parcel; the model does not choose arbitrary remote URLs
- `discovery = "auto"` tries `/.well-known/agent.json` first, then falls back to `<url>/a2a`
- `discovery = "card"` requires successful card discovery
- `discovery = "direct"` skips discovery and targets the declared endpoint directly
- discovered agent cards may refine the RPC path, but they may not pivot execution onto a different origin than the declared `url`
- `expect_agent_name` fails closed if discovery succeeds without a matching `name`
- `expect_card_sha256` pins the raw discovered agent-card body by lowercase SHA256

## Security Defaults

Dispatch enforces these transport rules:

- non-loopback A2A endpoints must use `https://`
- plain `http://` is only accepted for loopback development targets like `localhost` or `127.0.0.1`
- A2A URLs must not embed credentials
- bearer/header/basic credentials must come from names declared in `[[agent.secrets]]`

## Runtime Behavior

Dispatch currently exposes A2A as a synchronous tool surface:

- send JSON-RPC `message/send`
- if the remote returns a completed task, surface the result immediately
- if the remote returns an unfinished task, poll `tasks/get`
- if polling exceeds the effective tool timeout, Dispatch issues best-effort `tasks/cancel`

Timeout interaction:

- `agent.timeouts.tool` applies to host-executed A2A calls
- the `run` timeout can further cap the effective time available inside a turn

## Operator Controls

Dispatch supports both environment-level and CLI-scoped A2A policy overrides.

Environment controls:

- `DISPATCH_A2A_ALLOWED_ORIGINS`
- `DISPATCH_A2A_TRUST_POLICY`

CLI-scoped controls:

- `dispatch run ... --a2a-allowed-origins ... --a2a-trust-policy ...`
- `dispatch parcel eval ... --a2a-allowed-origins ... --a2a-trust-policy ...`
- `dispatch courier conformance ... --a2a-allowed-origins ... --a2a-trust-policy ...`

The CLI flags apply only to that command invocation. They do not mutate process-global environment state.

When both are present, CLI-scoped overrides win over inherited environment variables for that invocation.

Implementation note:

- the current override mechanism is thread-local and intended for the single-threaded CLI command path
- code that moves courier execution onto other threads must propagate A2A operator policy explicitly instead of assuming the override will follow automatically

### `DISPATCH_A2A_ALLOWED_ORIGINS`

Comma-separated hostnames or exact origins:

```text
DISPATCH_A2A_ALLOWED_ORIGINS=https://planner.example.com,search.internal
```

Semantics:

- `https://planner.example.com` matches that exact origin
- `search.internal` matches that hostname on any allowed A2A URL

### `DISPATCH_A2A_TRUST_POLICY`

TOML policy file for structured allow/identity rules:

```toml
[[rules]]
origin_prefix = "https://planner.example.com"
expected_agent_name = "planner-agent"
expected_card_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

[[rules]]
hostname = "search.internal"
```

Rule semantics:

- a rule must set `origin_prefix`, `hostname`, or both
- if both are set, both must match
- matching rules compose
- conflicting `expected_agent_name` or `expected_card_sha256` requirements fail closed
- if a matched rule requires card identity but card discovery does not succeed, Dispatch rejects the call
- if no rule matches, Dispatch rejects the call

Discovery/auth note:

- configured A2A auth headers are sent on the discovery request before card identity can be verified
- `discovery = "direct"` cannot satisfy parcel or operator discovered-identity requirements such as `expect_agent_name` or `expect_card_sha256`

## Inspection Surfaces

Dispatch exposes A2A tool metadata through normal CLI inspection:

- `dispatch parcel inspect <parcel> --courier native`
- `dispatch run <parcel> --list-tools`

These surfaces include:

- endpoint URL
- discovery mode
- auth form and referenced secret names
- expected agent name
- expected card digest

## Current Scope

What Dispatch A2A does today:

- static parcel-declared remote endpoints
- card discovery
- bearer/header/basic auth
- sync `message/send`
- polling unfinished tasks
- operator allowlist and trust policy controls

What it does not do yet:

- arbitrary model-chosen remote endpoints
- OAuth flows or mTLS
- full async task lifecycle as a first-class tool contract
- remote origin pivots across agent-card discovery
