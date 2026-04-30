# Chat Provider Plugin Extraction Plan

This is a follow-up plan, not part of the deployment plugin category rename.

## Goal

Move Dispatch chat/model adapters out of `dispatch-core` and behind provider plugins, while keeping `dispatch-core` as the parcel runtime and plugin host. The native courier/runtime loop remains in core; provider plugins only own the model/completion layer.

## Category Boundaries

- `provider`: stateless or model-call scoped inference backends used by `ChatModelBackend`.
- `courier`: parcel/session/run runtime. Couriers own `open_session` and `run`.
- `deployment`: deployment lifecycle control plane. Deployment plugins own deploy/update/rollback/list/start/stop/delete/test-run and do not own `Run`.
- `channel`: inbound event source.
- `database`: database lifecycle and query access.

## Sequencing

1. Audit `PluginModelBackend` and `dispatch-provider-protocol` against the full `ChatModelBackend` surface: streaming deltas, tool calls and tool results, cancellation, response IDs, usage metadata, finish reasons, timeout behavior, error mapping, and structured diagnostics for subprocess-backed providers.
2. If the protocol has gaps, extend the provider protocol before extracting adapters.
3. Extract HTTP adapters as provider plugins: Anthropic Messages, Gemini, OpenAI Responses, and OpenAI Chat Completions.
4. Extract `ClaudeCliBackend` and `CodexAppServerBackend` as provider plugins first. They spawn subprocesses, but today they implement `ChatModelBackend` inside the native courier loop; extracting them as providers preserves existing runtime semantics.
5. Keep one provider plugin bundled or otherwise available as the default first-run path so `dispatch run` remains approachable.

## Non-Goals For The First Pass

- Do not turn Claude Code or Codex into courier plugins in the first extraction. Courier versions would be a separate product decision because they would hand parcel-level runtime/session/tool-loop ownership to the vendor CLI.
- Do not add `Run` to deployment plugins.
- Do not route local Claude/Codex provider plugins through Seren.

## Open Decisions

- Which provider plugin should be bundled as the default.
- Whether provider protocol payloads should remain `serde_json::Value` during extraction or be promoted to typed request/response structs first.
- Whether subprocess-backed providers should surface stderr as structured provider events or only in terminal error details.
