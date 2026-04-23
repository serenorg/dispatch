# Database Plugin Protocol

**Status:** The first reference implementation is `databases/seren-db` in `dispatch-seren-plugins`; it currently implements the control-plane methods (`capabilities`, `configure`, `health`, and `shutdown`) and returns structured `unimplemented` errors for data-plane methods until the upstream SerenDB API is wired.

Dispatch database plugins are external executables that expose a database-like backend (PostgreSQL, MongoDB, Neon, Supabase, and similar) to Dispatch hosts. A database plugin runs out-of-process, speaks JSON-RPC 2.0 over stdio, and declares a typed set of operations a Dispatch host can invoke.

Database plugins are distinct from courier plugins (which execute parcels) and from provider plugins (which perform LLM inference). They cover traditional OLTP-style databases and document stores - read, write, and schema operations over a persistent store.

Other persistence-oriented plugin kinds (vector stores, full-text search indices, object storage, caches, queues) are not database plugins and will get dedicated kinds as they are needed. Keeping the `database` kind narrow avoids the "one size fits none" trap of a single umbrella plugin type.

## Scope

A database plugin answers one shape of request: "run this typed operation against the backing database." Dispatch does not prescribe the query model - the same protocol covers SQL engines and document stores.

A database plugin can implement:

- `capabilities` - declare engine, supported operations, authentication modes
- `configure` - validate connection config and credentials
- `health` - verify connectivity and auth
- `describe` - return schema or collection metadata
- `open_session` / `close_session` - manage a logical connection or transaction lifecycle
- `execute` - run a single typed operation against the database
- `shutdown` - allow a persistent process to exit cleanly

Database plugins do not receive parcel directories. They do not emit courier events. They operate on explicit request payloads and return structured JSON responses. The shared protocol intentionally keeps `describe`, session, and `execute` payloads as engine-specific JSON while the data-plane contract is still iterating.

The current `seren-db` reference plugin is a skeleton for protocol development and local host integration. It declares PostgreSQL-shaped capabilities, accepts Seren API configuration, verifies auth through the SerenDB publisher, and does not yet run queries.

## Transport

JSON-RPC 2.0 over stdio, framed as newline-delimited JSON.

- Dispatch writes one JSON request line at a time to plugin stdin.
- The plugin writes one JSON-RPC message per line to stdout.
- stderr is reserved for human-readable diagnostics and logs.

Dispatch does not currently use JSON-RPC batch requests. The host keeps at most one request in flight per plugin stream and expects each terminal response to echo the request `id`.

Dispatch installs database plugins through `dispatch extension install` into the configured database registry; `dispatch up` can also reconcile database plugin manifests into a project-local registry under `.dispatch/registries/`. Routing database operations into the main parcel execution flow as parcel-callable tools is still under development; the protocol supports persistent processes and session lifecycles for that use, but the host-side tool surface is not yet wired.

## Plugin Manifest

Database plugins declare themselves in `database-plugin.json`:

```json
{
  "kind": "database",
  "name": "seren-db",
  "version": "0.1.0",
  "protocol_version": 1,
  "transport": "jsonl",
  "description": "SerenDB database plugin for Dispatch.",
  "exec": {
    "command": "./target/release/seren-db",
    "args": []
  }
}
```

Dispatch supports protocol version `1`. The `kind` field is optional for local manifests so conventional filenames and catalog metadata can infer the plugin kind, but when present it must be `"database"`.

## Requests

Every host call is sent as a JSON-RPC request. The `method` identifies the database operation and `params` contains the Dispatch request envelope. Request params always include `protocol_version` and `kind`.

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "database.capabilities",
  "params": {
    "protocol_version": 1,
    "kind": "capabilities"
  }
}
```

Database request methods:

- `database.capabilities`
- `database.configure`
- `database.health`
- `database.describe`
- `database.open_session`
- `database.close_session`
- `database.execute`
- `database.shutdown`

For `configure` and `health`, the plugin-specific connection object is in `config`. For `describe`, `open_session`, and `execute`, the engine-specific operation object is nested under `params`. For `close_session`, the request carries `session_id`.

## Capabilities

`database.capabilities` declares what the database offers:

```json
{
  "kind": "capabilities",
  "capabilities": {
    "database_id": "seren-db",
    "engine": "postgres",
    "operations": ["query", "execute", "describe"],
    "supports_transactions": true,
    "supports_streaming_rows": false,
    "supports_schema_introspection": true,
    "auth_modes": ["bearer"]
  }
}
```

This example mirrors the current `seren-db` response. `supports_streaming_rows` is `false`; callers should also be prepared for `unimplemented` responses from `describe`, `open_session`, `close_session`, and `execute` while the reference plugin remains a skeleton.

`engine` is a free-form string identifying the backend the plugin speaks to. Expected values include `postgres`, `mongodb`, `neon`, `supabase`, `mysql`, `sqlite`, and similar. Future tool layers can match on this string to decide which database plugin is appropriate for a requested operation. Plugins that wrap a service with a compatible wire protocol, such as Neon or Supabase speaking PostgreSQL, should declare the service-level identifier so operators can distinguish between backends when multiple compatible plugins are installed.

## Configuration

`database.configure` validates connection parameters. The config object is plugin-specific. Generic SQL plugins may accept host/database/TLS settings; managed service plugins may accept service-specific API configuration.

The current `seren-db` plugin accepts:

- `api_origin` - optional Seren API origin, defaulting to `https://api.serendb.com`
- `api_key` - Seren API key with access to the SerenDB publisher

If `config` is null or an empty object, `seren-db` resolves the same values from `SEREN_API_ORIGIN` and `SEREN_API_KEY`. `SEREN_API_KEY` is required unless `api_key` is supplied in the request config.

```json
{
  "kind": "configure",
  "protocol_version": 1,
  "config": {
    "api_origin": "https://api.serendb.com",
    "api_key": "seren_..."
  }
}
```

A successful response carries typed database metadata:

```json
{
  "kind": "configured",
  "configuration": {
    "database_id": "seren-db",
    "extensions": {
      "base_url": "https://api.serendb.com/publishers/seren-db"
    }
  }
}
```

`server_version` and `effective_database` are optional. The current `seren-db` plugin returns neither because it talks to the SerenDB publisher API rather than a direct PostgreSQL wire endpoint.

`database.health` performs the minimal round-trip required to confirm credentials and connectivity. The `health` request reuses the same `config` object shape as `configure`. The current `seren-db` plugin does not have a publisher-level health route, so it verifies reachability with the narrowest authenticated publisher read available today.

## Schema Introspection

`database.describe` returns schema or collection metadata. The request's engine-specific body is nested under `params`; the response shape is engine-specific and only required to be a single JSON document.

The current `seren-db` plugin returns an `unimplemented` error for `database.describe`. The examples below describe the generic protocol direction for future SQL and document database plugins.

For SQL engines:

```json
{
  "kind": "schema",
  "schema": {
    "tables": [
      {
        "name": "users",
        "columns": [
          { "name": "id", "type": "uuid", "nullable": false },
          { "name": "email", "type": "text", "nullable": false }
        ],
        "primary_key": ["id"],
        "indexes": []
      }
    ]
  }
}
```

For document engines:

```json
{
  "kind": "schema",
  "schema": {
    "collections": [
      { "name": "users", "document_count": 12043, "indexes": [] }
    ]
  }
}
```

Databases that do not support introspection may return `{ "kind": "schema", "schema": null }`.

## Sessions

Dispatch calls `database.open_session` to obtain a `session_id` when a caller needs a stateful connection (transactions, prepared statements, cursors):

```json
{
  "kind": "open_session",
  "protocol_version": 1,
  "params": {
    "options": { "read_only": false }
  }
}
```

```json
{
  "kind": "session_opened",
  "session": { "id": "sess_01HVC...", "expires_in_ms": 60000 }
}
```

Subsequent `execute` calls pass the `session_id`. `database.close_session` releases the session; the plugin should also release sessions that have been idle past `expires_in_ms`.

Stateless usage is allowed: `execute` may be called with no `session_id`, in which case the plugin treats the call as auto-commit single-statement execution.

The current `seren-db` plugin returns an `unimplemented` error for `database.open_session` and `database.close_session`.

## Execute

`database.execute` carries one typed operation under `params`. The `operation` field inside that object discriminates on engine family.

The current `seren-db` plugin returns an `unimplemented` error for `database.execute`. The examples below describe the generic protocol direction for future SQL and document database plugins.

**SQL query:**
```json
{
  "kind": "execute",
  "protocol_version": 1,
  "params": {
    "session_id": "sess_01HVC...",
    "operation": {
      "kind": "sql_query",
      "statement": "SELECT id, email FROM users WHERE created_at > $1",
      "parameters": ["2026-01-01T00:00:00Z"]
    },
    "limits": { "max_rows": 1000, "max_bytes": 1048576, "timeout_ms": 5000 }
  }
}
```

**SQL mutation:**
```json
{
  "kind": "execute",
  "protocol_version": 1,
  "params": {
    "operation": {
      "kind": "sql_exec",
      "statement": "UPDATE users SET email = $1 WHERE id = $2",
      "parameters": ["user@example.com", "..."]
    }
  }
}
```

**Document find:**
```json
{
  "kind": "execute",
  "protocol_version": 1,
  "params": {
    "operation": {
      "kind": "document_find",
      "collection": "users",
      "filter": { "tenant_id": "t_1" },
      "projection": null,
      "limit": 50
    }
  }
}
```

**Document write:**
```json
{
  "kind": "execute",
  "protocol_version": 1,
  "params": {
    "operation": {
      "kind": "document_write",
      "collection": "users",
      "write": {
        "mode": "update",
        "filter": { "id": "u_1" },
        "set": { "email": "user@example.com" }
      }
    }
  }
}
```

Responses mirror the operation:

```json
{
  "kind": "result",
  "result": {
    "kind": "rows",
    "columns": [
      { "name": "id", "type": "uuid" },
      { "name": "email", "type": "text" }
    ],
    "rows": [
      ["...", "alice@example.com"],
      ["...", "bob@example.com"]
    ],
    "row_count": 2,
    "truncated": false,
    "continuation_token": null
  }
}
```

Other result kinds:

- `rows` - SQL `SELECT`-style results
- `affected` - SQL `INSERT` / `UPDATE` / `DELETE` `{ "rows_affected": n }`
- `documents` - document find results
- `document_write` - document write acknowledgment with `{ "matched": n, "modified": n, "inserted_ids": [...] }`

Large result sets may be paginated with `continuation_token`. The follow-up request shape is engine-specific and should be documented by each plugin, typically by passing the token back in a subsequent `execute` request for the same logical operation.

## Streaming Rows

Databases that set `supports_streaming_rows = true` may return rows as JSON-RPC notifications instead of bundling them into the terminal response. During an `execute` request the plugin emits `database.event` notifications:

| Kind | Fields | Purpose |
|---|---|---|
| `row_batch` | `columns`, `rows` | Incremental row batch |
| `document_batch` | `documents` | Incremental document batch |
| `progress` | `rows_sent`, `bytes_sent` | Optional progress update |

followed by a single terminal `result` whose payload may contain only summary metadata, such as total rows sent and truncation state. Clients that need buffered results should call plugins that return buffered terminal payloads or request a non-streaming mode when the plugin offers one; streaming clients must consume `database.event` notifications to receive streamed rows or documents.

## Errors

Structured Dispatch errors are returned as JSON-RPC error responses. Dispatch-specific
error details live in `error.data.dispatch_error`:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "error": {
    "code": -32000,
    "message": "database rejected query",
    "data": {
      "dispatch_error": {
        "code": "invalid_statement",
        "message": "database rejected query",
        "details": { "statement": "SELCT *" }
      }
    }
  }
}
```

Reserved error codes:

- `invalid_statement` - syntactically or semantically invalid operation
- `unsupported_operation` - operation kind not offered by this database
- `permission_denied` - credentials lack access to the target object
- `not_found` - target object (table, collection, document, key) does not exist
- `conflict` - write conflict under optimistic concurrency
- `timeout` - operation exceeded its `timeout_ms`
- `result_too_large` - result exceeded `max_rows` or `max_bytes` and cannot be paginated
- `upstream_error` - transient backing-database failure
- `unimplemented` - protocol method is recognized but not implemented by this plugin
- `authentication_failed` - plugin configuration or credentials could not be resolved or authenticated

## Implementation Guidance

- Keep a warm connection pool across sessions and single-shot calls.
- Enforce `limits.timeout_ms`, `limits.max_rows`, and `limits.max_bytes` inside the plugin - Dispatch does not clip results on its side.
- Return truncated result sets with `truncated = true` and `continuation_token != null` rather than failing.
- For SQL backends, use parameterized queries exclusively. The protocol does not provide a string-interpolation mode and plugins should reject any attempt to inline user-provided values into `statement`.
- For document backends, represent filter and projection as JSON objects matching the backend's native shape (e.g. MongoDB filter documents) to keep the surface familiar to plugin authors.

## Planned Tool Surface for Parcels

Database plugins are intended to be exposed to parcels as callable tools. Dispatch will map each database plugin into a synthetic tool namespace derived from the plugin name (for example `seren-db.query`, `seren-db.execute`, `seren-db.describe`). The parcel tool layer will translate tool invocations into `database.execute` requests with the appropriate operation kind.

Dispatch installs database plugins via `dispatch extension install` today, but does not yet route them as parcel-callable tools. The specific tool naming convention, parameter schema, and registration path will live in the parcel tool documentation and are out of scope for this protocol doc.

## Trust Model

Installing a database plugin is an explicit trust action, equivalent to installing any other Dispatch plugin.

Databases typically receive:

- connection credentials for the backing database
- raw query text and parameters, including writes
- declared environment and secret values routed through configuration

For that reason Dispatch does not auto-discover arbitrary executables as database plugins. The capability-based trust work tracked in [`plugin-ecosystem.md`](./plugin-ecosystem.md) applies to databases once it lands, and is particularly important here because operations may mutate sensitive data.
