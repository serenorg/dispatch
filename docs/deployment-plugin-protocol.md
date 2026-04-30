# Deployment Plugin Protocol

**Status:** Protocol version 1. Dispatch has first-class install and runtime support for deployment plugins today. Reference implementations include `deployments/seren-agent` and `deployments/seren-cloud` in `dispatch-seren-plugins`.

Dispatch deployment plugins are external executables that own managed deployment lifecycle operations for a backend. They validate specs, create or reconcile durable remote resources, report status, list deployments, start or stop workloads, delete resources, and, when supported, manage revisions and rollback.

Deployment plugins are control-plane-only. They do not execute runtime turns, receive channel events, or perform LLM inference. Runtime conversations belong to courier plugins or the runtime system selected by a deployment. A deployment plugin may upload or package artifacts, but after it creates a deployment it returns a `deployment_id`; later runtime traffic is addressed to that deployment through the appropriate non-deployment surface.

## Scope

A deployment plugin can implement:

- `capabilities` - declare lifecycle support, scheduling support, revision support, rollback support, and runtime artifact targets
- `configure` - validate backend credentials and endpoint configuration
- `health` - verify backend reachability and auth
- `validate` - validate a candidate deployment spec without side effects
- `test_run` - perform a backend-defined preflight run without creating a long-lived deployment
- `deploy` - create a managed deployment
- `upsert` - reconcile a managed deployment by stable name
- `preview_update` / `update` - diff or apply a partial update
- `get` / `list` - read managed deployment state
- `list_revisions` - list revision history
- `preview_rollback` / `rollback` - diff or activate a previous revision
- `start` / `stop` - halt or resume a managed deployment without deletion
- `delete` - tear down a managed deployment
- `shutdown` - allow a persistent process to exit cleanly

The protocol intentionally keeps backend specs, patches, filters, preview diffs, and detailed deployment views as `serde_json::Value`. Dispatch owns the lifecycle envelope; each backend owns the shape of its authored spec and detailed state.

The current `seren-cloud` plugin supports `capabilities`, `configure`, `health`, `validate`, `deploy`, `upsert`, `get`, `list`, `start`, `stop`, `delete`, and `shutdown`. It advertises scheduled execution and a Linux arm64 glibc runtime target, but not test runs, revisions, or rollback. The current `seren-agent` plugin supports the broader managed-agent lifecycle, including test runs, revisions, rollback, and scheduled deployments.

## Transport

JSON-RPC 2.0 over stdio, framed as newline-delimited JSON.

- Dispatch writes one JSON request line at a time to plugin stdin.
- The plugin writes one JSON-RPC response line at a time to stdout.
- stderr is reserved for human-readable diagnostics and logs.

Dispatch does not currently use JSON-RPC batch requests. The host keeps at most one request in flight per plugin stream and expects each terminal response to echo the request `id`. Deployment plugins do not emit JSON-RPC notifications in protocol version 1.

## Plugin Manifest

Deployment plugins declare themselves in `deployment-plugin.json`:

```json
{
  "kind": "deployment",
  "name": "seren-cloud",
  "version": "0.1.0",
  "protocol_version": 1,
  "transport": "jsonl",
  "description": "Seren Cloud deployment plugin for Dispatch deployment-bundle workloads.",
  "exec": {
    "command": "./target/release/seren-cloud",
    "args": []
  }
}
```

Dispatch supports protocol version `1`. The `kind` field is optional for local manifests so conventional filenames and catalog metadata can infer the plugin kind, but when present it must be `"deployment"`.

## Requests

Every host call is sent as a JSON-RPC request. The `method` identifies the deployment operation and `params` contains the Dispatch request envelope. Request params always include `protocol_version` and `kind`.

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "deployment.capabilities",
  "params": {
    "protocol_version": 1,
    "kind": "capabilities"
  }
}
```

Deployment request methods:

- `deployment.capabilities`
- `deployment.configure`
- `deployment.health`
- `deployment.validate`
- `deployment.test_run`
- `deployment.deploy`
- `deployment.upsert`
- `deployment.preview_update`
- `deployment.update`
- `deployment.get`
- `deployment.list`
- `deployment.list_revisions`
- `deployment.preview_rollback`
- `deployment.rollback`
- `deployment.start`
- `deployment.stop`
- `deployment.delete`
- `deployment.shutdown`

## Capabilities

`deployment.capabilities` declares what the deployment backend supports:

```json
{
  "kind": "capabilities",
  "capabilities": {
    "deployment_plugin_id": "seren-cloud",
    "protocol_version": 1,
    "supports_test_run": false,
    "supports_revisions": false,
    "supports_rollback": false,
    "supports_scheduled": true,
    "runtime_targets": [
      {
        "kind": "native",
        "target_triple": "aarch64-unknown-linux-gnu",
        "os": "linux",
        "arch": "arm64",
        "libc": "glibc",
        "preferred": true
      }
    ],
    "extensions": {
      "publisher": "seren-cloud",
      "spec_forms": ["workload", "code.bundle_path", "code.parcel_dir", "code.cached_bundle"]
    }
  }
}
```

`runtime_targets` describes artifacts that will run in the remote deployment backend, not the deployment plugin binary running on the operator's machine. A macOS operator can run a macOS deployment plugin while the remote backend requires packaged native channel plugins to be `aarch64-unknown-linux-gnu`.

Deployment backends that can execute Dispatch host-compatible WebAssembly artifacts may advertise a `wasm` runtime target. A generic `wasm32-wasi` artifact is not enough unless the backend actually provides the Dispatch host ABI the artifact expects.

## Configuration

`deployment.configure` validates backend credentials and endpoint settings. The config object is plugin-specific. Seren deployment plugins accept Seren API configuration:

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

A successful response carries backend metadata:

```json
{
  "kind": "configured",
  "configuration": {
    "deployment_plugin_id": "seren-cloud",
    "extensions": {
      "base_url": "https://api.serendb.com/publishers/seren-cloud"
    }
  }
}
```

`deployment.health` performs the minimal backend round-trip required to confirm credentials and network reachability. It reuses the same `config` object shape as `configure`.

## Validation

`deployment.validate` checks a candidate spec without creating or mutating backend resources:

```json
{
  "kind": "validate",
  "protocol_version": 1,
  "spec": {
    "name": "daily-worker",
    "mode": "cron",
    "cron_schedule": "0 * * * *",
    "code": {
      "bundle_path": "./worker.tar.gz",
      "runtime_kind": "python"
    }
  }
}
```

The response contains a validation result. When `ok` is `false`, `issues` must contain at least one structured issue. Plugins may also return a backend-normalized spec.

```json
{
  "kind": "validation",
  "result": {
    "ok": false,
    "issues": [
      {
        "field": "cron_schedule",
        "code": "invalid_schedule",
        "message": "cron_schedule is required when mode is cron"
      }
    ]
  }
}
```

## Deploy And Upsert

`deployment.deploy` creates a managed deployment from a backend-defined `spec`:

```json
{
  "kind": "deploy",
  "protocol_version": 1,
  "spec": {
    "name": "daily-worker",
    "mode": "cron"
  }
}
```

`deployment.upsert` reconciles by stable `name`, creating on the first call and returning or updating the existing deployment on later calls according to plugin-defined drift rules:

```json
{
  "kind": "upsert",
  "protocol_version": 1,
  "name": "daily-worker",
  "spec": {
    "name": "daily-worker",
    "mode": "cron"
  }
}
```

Both methods return a deployment summary:

```json
{
  "kind": "deployment",
  "deployment": {
    "deployment_id": "dep_123",
    "status": "running",
    "revision_id": "rev_1",
    "detail": {
      "name": "daily-worker",
      "mode": "cron"
    }
  }
}
```

`upsert` semantics are intentionally backend-defined. A backend may perform an in-place update, return the current deployment unchanged when comparable fields match, replace on drift, or return `unimplemented` if idempotent reconcile is not supported.

## Updates And Rollback

Update and rollback methods are optional lifecycle extensions advertised by `supports_revisions` and `supports_rollback`.

`deployment.preview_update` returns a backend-defined diff without applying it. `deployment.update` applies a backend-defined partial patch and returns the new live deployment state. `deployment.list_revisions` returns revision history. `deployment.preview_rollback` returns the diff for activating an earlier revision. `deployment.rollback` activates that revision and returns the new live deployment state.

Backends that do not support these methods should return a structured `unimplemented` error. `seren-cloud` currently does that for updates, revisions, and rollback; `seren-agent` implements the broader revision lifecycle.

## Runtime State

`deployment.get` returns current state for one deployment. `deployment.list` returns backend-defined filtered lists. `deployment.start` and `deployment.stop` change runtime state without deleting resources. `deployment.delete` tears down the managed deployment.

`start`, `stop`, `delete`, and `shutdown` return `{ "kind": "ok" }` on success.

## Test Run

`deployment.test_run` lets a plugin perform a backend-defined preflight execution without creating a durable deployment. The request carries a `spec` plus optional `sample_input`:

```json
{
  "kind": "test_run",
  "protocol_version": 1,
  "spec": { "name": "draft-agent" },
  "sample_input": "hello"
}
```

The response shape is intentionally generic:

```json
{
  "kind": "test_run_result",
  "result": {
    "status": "completed",
    "output": "hello back"
  }
}
```

## Errors

Structured Dispatch errors are returned as JSON-RPC error responses. Dispatch-specific error details live in `error.data.dispatch_error`:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "error": {
    "code": -32000,
    "message": "invalid deployment spec",
    "data": {
      "dispatch_error": {
        "code": "invalid_spec",
        "message": "invalid deployment spec",
        "details": {
          "field": "mode"
        }
      }
    }
  }
}
```

Reserved error codes:

- `authentication_failed` - backend credentials invalid or missing
- `not_found` - deployment, revision, or backend resource does not exist
- `invalid_spec` - authored spec or patch is invalid
- `upstream_error` - transient backend failure
- `unimplemented` - protocol method is recognized but not implemented by this plugin

Plugins may return backend-specific error codes. Dispatch treats unknown codes as application errors.

## Dispatch Up Integration

`dispatch up` is the project-level orchestration command for deployment bindings. It reads `dispatch.toml`, reconciles declared extension manifests into project-local registries, validates or applies declared deployment bindings, and records produced deployment IDs in `.dispatch/state/deployments.json`.

Deployment bindings carry two distinct payloads:

- `config` - auth and endpoint setup sent through `deployment.configure`
- `spec` - backend-defined deployment definition sent through `validate`, `test_run`, `deploy`, or `upsert`

For specs using conventional source helpers such as `code.parcel_dir` or `code.bundle_path`, `dispatch up` may materialize the source into a cached bundle and pass a backend-ready `code.cached_bundle` shape to the plugin.

## Trust Model

Installing a deployment plugin is an explicit trust action, equivalent to installing any other Dispatch plugin.

Deployment plugins typically receive:

- backend credentials for creating, updating, and deleting remote resources
- deployment specs, bundled code paths, and package metadata
- declared environment and secret values routed through configuration or spec payloads

For that reason Dispatch does not auto-discover arbitrary executables as deployment plugins. Deployment plugins should validate specs before mutating backend state and return structured issues whenever possible.
