# Database Plugin Protocol

**Status:** draft for Dispatch v0.4.0. Subject to change until the first reference implementation lands.

Dispatch database plugins are external executables that expose a read+write database backend (PostgreSQL, MongoDB, Neon, Supabase, and similar) to Dispatch parcels as callable tools. A database plugin runs out-of-process, speaks JSON-RPC 2.0 over stdio, and declares a typed set of operations Dispatch-managed agents can invoke.

Database plugins are distinct from courier plugins (which execute parcels) and from provider plugins (which perform LLM inference). They cover traditional OLTP-style databases and document stores - read, write, and schema operations over a persistent store.

Other persistence-oriented plugin kinds (vector stores, full-text search indices, object storage, caches, queues) are not database plugins and will get dedicated kinds as they are needed. Keeping the `database` kind narrow avoids the "one size fits none" trap of a single umbrella plugin type.

## Scope

A database plugin answers one shape of request: "run this typed operation against the backing database." Dispatch does not prescribe the query model - the same protocol covers SQL engines and document stores.

A database plugin implements:

- `capabilities` - declare engine, supported operations, authentication modes
- `configure` - validate connection config and credentials
- `health` - verify connectivity and auth
- `describe` - return schema or collection metadata (optional but strongly recommended)
- `open_session` / `close_session` - manage a logical connection or transaction lifecycle
- `execute` - run a single typed operation against the database
- `shutdown` - allow a persistent process to exit cleanly

Database plugins do not receive parcel directories. They do not emit courier events. They operate on explicit, typed request payloads and return typed, paginated results.

## Transport

JSON-RPC 2.0 over stdio, framed as newline-delimited JSON.

- Dispatch writes one JSON request line at a time to plugin stdin.
- The plugin writes one JSON-RPC message per line to stdout.
- stderr is reserved for human-readable diagnostics and logs.

Dispatch does not currently use JSON-RPC batch requests. The host keeps at most one request in flight per plugin stream and expects each terminal response to echo the request `id`.

Dispatch keeps a database plugin process alive while a session is open. Stateless single-shot `execute` calls may also run through an ephemeral process.

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

Dispatch supports protocol version `1`. The `kind` field is required and must be `"database"` when present.

## Requests

Every host call is sent as a JSON-RPC request. The `method` identifies the database
operation and `params` contains the typed Dispatch request payload.

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

## Capabilities

`database.capabilities` declares what the database offers:

```json
{
  "kind": "capabilities",
  "capabilities": {
    "database_id": "seren-db",
    "protocol_version": 1,
    "engine": "postgres",
    "operations": ["query", "execute", "describe"],
    "supports_transactions": true,
    "supports_streaming_rows": true,
    "supports_schema_introspection": true,
    "auth_modes": ["bearer"]
  }
}
```

`engine` is a free-form string identifying the backend the plugin speaks to. Expected values include `postgres`, `mongodb`, `neon`, `supabase`, `mysql`, `sqlite`, and similar. Agents and tool layers match on this string to decide which database plugin to route a given operation through. Plugins that wrap a service with a compatible wire protocol (Neon and Supabase speak PostgreSQL) should declare the service-level identifier, so operators can distinguish between backends when multiple compatible plugins are installed.

## Configuration

`database.configure` validates connection parameters. Secrets and credentials are resolved by Dispatch before the request is sent.

```json
{
  "kind": "configure",
  "config": {
    "connection": {
      "host": "db.serendb.com",
      "database": "app_prod",
      "ssl_mode": "require"
    },
    "auth": { "mode": "bearer", "token": "seren_..." }
  }
}
```

A successful response carries typed database metadata:

```json
{
  "kind": "configured",
  "configuration": {
    "database_id": "seren-db",
    "server_version": "15.4",
    "effective_database": "app_prod"
  }
}
```

`database.health` performs the minimal round-trip required to confirm credentials and connectivity. The `health` request reuses the same `config` object shape as `configure`.

## Schema Introspection

`database.describe` returns schema or collection metadata. The response shape is engine-specific; the protocol only requires that it be a single JSON document.

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
  "options": { "read_only": false }
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

## Execute

`database.execute` carries one typed operation. The `operation` field discriminates on engine family:

**SQL query:**
```json
{
  "kind": "execute",
  "session_id": "sess_01HVC...",
  "operation": {
    "kind": "sql_query",
    "statement": "SELECT id, email FROM users WHERE created_at > $1",
    "parameters": ["2026-01-01T00:00:00Z"]
  },
  "limits": { "max_rows": 1000, "max_bytes": 1048576, "timeout_ms": 5000 }
}
```

**SQL mutation:**
```json
{
  "kind": "execute",
  "operation": {
    "kind": "sql_exec",
    "statement": "UPDATE users SET email = $1 WHERE id = $2",
    "parameters": ["user@example.com", "..."]
  }
}
```

**Document find:**
```json
{
  "kind": "execute",
  "operation": {
    "kind": "document_find",
    "collection": "users",
    "filter": { "tenant_id": "t_1" },
    "projection": null,
    "limit": 50
  }
}
```

**Document write:**
```json
{
  "kind": "execute",
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

Large result sets may be paginated with `continuation_token`. Passing the token back in a subsequent `execute` with an empty operation of the same kind returns the next page.

## Streaming Rows

Databases that set `supports_streaming_rows = true` may return rows as JSON-RPC notifications instead of bundling them into the terminal response. During an `execute` request the plugin emits `database.event` notifications:

| Kind | Fields | Purpose |
|---|---|---|
| `row_batch` | `columns`, `rows` | Incremental row batch |
| `document_batch` | `documents` | Incremental document batch |
| `progress` | `rows_sent`, `bytes_sent` | Optional progress update |

followed by a single terminal `result` whose payload is empty and whose count fields reflect the total streamed count. Clients that prefer buffered results may ignore the notifications and use only the terminal response, as long as the plugin still populates the full result in the terminal response.

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

## Implementation Guidance

- Keep a warm connection pool across sessions and single-shot calls.
- Enforce `limits.timeout_ms`, `limits.max_rows`, and `limits.max_bytes` inside the plugin - Dispatch does not clip results on its side.
- Return truncated result sets with `truncated = true` and `continuation_token != null` rather than failing.
- For SQL backends, use parameterized queries exclusively. The protocol does not provide a string-interpolation mode and plugins should reject any attempt to inline user-provided values into `statement`.
- For document backends, represent filter and projection as JSON objects matching the backend's native shape (e.g. MongoDB filter documents) to keep the surface familiar to plugin authors.

## Tool Surface for Parcels

Database plugins are exposed to parcels as callable tools. Dispatch maps each database plugin into a synthetic tool namespace derived from the plugin name (for example `seren-db.query`, `seren-db.execute`, `seren-db.describe`). The parcel tool layer translates tool invocations into `database.execute` requests with the appropriate operation kind.

The specific tool naming convention, parameter schema, and registration path live in the parcel tool documentation and are out of scope for this protocol doc; this section only notes that parcels interact with databases through tools, not through direct JSON-RPC.

## Trust Model

Installing a database plugin is an explicit trust action, equivalent to installing any other Dispatch plugin.

Databases typically receive:

- connection credentials for the backing database
- raw query text and parameters, including writes
- declared environment and secret values routed through configuration

For that reason Dispatch does not auto-discover arbitrary executables as database plugins. The capability-based trust work tracked in [`plugin-ecosystem.md`](./plugin-ecosystem.md) applies to databases once it lands, and is particularly important here because operations may mutate sensitive data.
