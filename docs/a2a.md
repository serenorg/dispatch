# A2A Tools

Dispatch supports host-executed remote agent-to-agent tools through `TOOL A2A`.

## Parcel Contract

Basic examples:

```dockerfile
SECRET PLANNER_TOKEN
SECRET SEARCH_TOKEN
SECRET BACKOFFICE_USER
SECRET BACKOFFICE_PASSWORD

TOOL A2A planner URL https://planner.example.com DISCOVERY card AUTH bearer PLANNER_TOKEN EXPECT_AGENT_NAME planner-agent
TOOL A2A search URL https://search.example.com AUTH header X-Api-Key SEARCH_TOKEN
TOOL A2A backoffice URL https://backoffice.example.com AUTH basic BACKOFFICE_USER BACKOFFICE_PASSWORD
```

Supported clauses:

- `URL <endpoint>`
- `DISCOVERY auto|card|direct`
- `AUTH bearer <secret_name>`
- `AUTH header <header_name> <secret_name>`
- `AUTH basic <username_secret_name> <password_secret_name>`
- `EXPECT_AGENT_NAME <name>`
- `EXPECT_CARD_SHA256 <digest>`
- `SCHEMA <path>`
- `APPROVAL ...`
- `RISK ...`
- `DESCRIPTION "..."`

Semantics:

- the endpoint is declared statically in the parcel; the model does not choose arbitrary remote URLs
- `DISCOVERY auto` tries `/.well-known/agent.json` first, then falls back to `<url>/a2a`
- `DISCOVERY card` requires successful card discovery
- `DISCOVERY direct` skips discovery and targets the declared endpoint directly
- discovered agent cards may refine the RPC path, but they may not pivot execution onto a different origin than the declared `URL`
- `EXPECT_AGENT_NAME` fails closed if discovery succeeds without a matching `name`
- `EXPECT_CARD_SHA256` pins the raw discovered agent-card body by lowercase SHA256

## Security Defaults

Dispatch enforces these transport rules:

- non-loopback A2A endpoints must use `https://`
- plain `http://` is only accepted for loopback development targets like `localhost` or `127.0.0.1`
- A2A URLs must not embed credentials
- bearer/header/basic credentials must come from declared `SECRET`s

## Runtime Behavior

Dispatch currently exposes A2A as a synchronous tool surface:

- send JSON-RPC `message/send`
- if the remote returns a completed task, surface the result immediately
- if the remote returns an unfinished task, poll `tasks/get`
- if polling exceeds the effective tool timeout, Dispatch issues best-effort `tasks/cancel`

Timeout interaction:

- `TIMEOUT TOOL` applies to host-executed A2A calls
- `TIMEOUT RUN` can further cap the effective time available inside a turn

## Operator Controls

Dispatch supports both environment-level and CLI-scoped A2A policy overrides.

Environment controls:

- `DISPATCH_A2A_ALLOWED_ORIGINS`
- `DISPATCH_A2A_TRUST_POLICY`

CLI-scoped controls:

- `dispatch run ... --a2a-allowed-origins ... --a2a-trust-policy ...`
- `dispatch eval ... --a2a-allowed-origins ... --a2a-trust-policy ...`
- `dispatch courier conformance ... --a2a-allowed-origins ... --a2a-trust-policy ...`

The CLI flags apply only to that command invocation. They do not mutate process-global environment state.

When both are present, CLI-scoped overrides win over inherited environment variables for that invocation.

### `DISPATCH_A2A_ALLOWED_ORIGINS`

Comma-separated hostnames or exact origins:

```text
DISPATCH_A2A_ALLOWED_ORIGINS=https://planner.example.com,search.internal
```

Semantics:

- `https://planner.example.com` matches that exact origin
- `search.internal` matches that hostname on any allowed A2A URL

### `DISPATCH_A2A_TRUST_POLICY`

YAML policy file for structured allow/identity rules:

```yaml
rules:
  - origin_prefix: "https://planner.example.com"
    expected_agent_name: "planner-agent"
    expected_card_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
  - hostname: "search.internal"
```

Rule semantics:

- a rule must set `origin_prefix`, `hostname`, or both
- if both are set, both must match
- matching rules compose
- conflicting `expected_agent_name` or `expected_card_sha256` requirements fail closed
- if a matched rule requires card identity but card discovery does not succeed, Dispatch rejects the call
- if no rule matches, Dispatch rejects the call

## Inspection Surfaces

Dispatch exposes A2A tool metadata through normal CLI inspection:

- `dispatch inspect <parcel> --courier native`
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
