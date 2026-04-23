use super::*;
use crate::resolve_provider_plugin;
use dispatch_provider_protocol::{
    PROVIDER_PLUGIN_PROTOCOL_VERSION, PluginRequest, PluginRequestEnvelope, PluginResponse,
    RequestId, parse_jsonrpc_message, request_to_jsonrpc,
};
#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
use std::collections::HashMap;
use std::{
    io::{BufReader, Read, Write},
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(crate) struct PluginModelBackend {
    provider: String,
    session_capable: bool,
}

impl PluginModelBackend {
    pub(crate) fn new(provider: impl Into<String>, session_capable: bool) -> Self {
        Self {
            provider: provider.into(),
            session_capable,
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_PLUGIN_BINARY_OVERRIDES: RefCell<HashMap<String, String>> =
        RefCell::new(HashMap::new());
}

#[cfg(all(test, unix))]
pub(crate) fn clear_test_plugin_binary_override(provider: &str) {
    TEST_PLUGIN_BINARY_OVERRIDES.with(|slot| {
        slot.borrow_mut().remove(provider);
    });
}

impl ChatModelBackend for PluginModelBackend {
    fn id(&self) -> &str {
        &self.provider
    }

    fn supports_previous_response_id(&self) -> bool {
        self.session_capable
    }

    fn generate(&self, request: &ModelRequest) -> Result<ModelGeneration, CourierError> {
        generate_with_noop_events(self, request)
    }

    fn generate_with_events(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent),
    ) -> Result<ModelGeneration, CourierError> {
        let Some(mut spawned) = spawn_plugin_process(&self.provider, request)? else {
            return Ok(ModelGeneration::NotConfigured {
                backend: self.provider.clone(),
                reason: format!(
                    "no provider plugin `{}` found in the installed registry, bundled backends, or DISPATCH_BACKEND_{}",
                    self.provider,
                    self.provider.to_ascii_uppercase(),
                ),
            });
        };

        collect_plugin_output(
            &mut spawned,
            &self.provider,
            self.session_capable,
            request,
            on_event,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PluginWireProtocol {
    LegacyBackend,
    ProviderJsonRpc,
}

#[derive(Debug)]
struct PluginLaunch {
    program: String,
    args: Vec<String>,
    wire_protocol: PluginWireProtocol,
}

struct SpawnedPlugin {
    child: Child,
    wire_protocol: PluginWireProtocol,
    request_id: Option<RequestId>,
}

fn resolve_plugin_launch(provider: &str, working_directory: Option<&str>) -> Option<PluginLaunch> {
    #[cfg(test)]
    if let Some(path) =
        TEST_PLUGIN_BINARY_OVERRIDES.with(|slot| slot.borrow().get(provider).cloned())
    {
        return Some(PluginLaunch {
            program: path,
            args: Vec::new(),
            wire_protocol: PluginWireProtocol::LegacyBackend,
        });
    }

    let env_key = format!("DISPATCH_BACKEND_{}", provider.to_ascii_uppercase());
    if let Some(val) = env_var(&env_key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        // This env override is intentionally simple: a binary path followed by
        // whitespace-separated args. Paths with embedded spaces should use the
        // bundled lookup or PATH instead of relying on shell-style quoting here.
        let mut parts = val.splitn(2, ' ');
        let program = parts.next()?.to_string();
        let args = parts
            .next()
            .map(|rest| rest.split_whitespace().map(ToString::to_string).collect())
            .unwrap_or_default();
        // If the same provider is also registered as an installed provider
        // plugin, honor that wire protocol: operators use the env override to
        // point at a locally-built binary during development, and the binary
        // speaks the same JSON-RPC shape as the installed plugin it shadows.
        // Legacy `dispatch-backend-*` binaries without a matching installed
        // registry entry keep the legacy line-protocol behavior.
        let wire_protocol =
            if installed_provider_plugin_launch(provider, working_directory).is_some() {
                PluginWireProtocol::ProviderJsonRpc
            } else {
                PluginWireProtocol::LegacyBackend
            };
        return Some(PluginLaunch {
            program,
            args,
            wire_protocol,
        });
    }

    if let Some(launch) = installed_provider_plugin_launch(provider, working_directory) {
        return Some(launch);
    }

    if let Some(launch) = bundled_plugin_launch(provider) {
        return Some(launch);
    }

    // Fall back to PATH lookup - spawn will return NotFound if not present.
    Some(PluginLaunch {
        program: plugin_binary_name(provider),
        args: Vec::new(),
        wire_protocol: PluginWireProtocol::LegacyBackend,
    })
}

fn installed_provider_plugin_launch(
    provider: &str,
    working_directory: Option<&str>,
) -> Option<PluginLaunch> {
    let registry_path = provider_registry_path_for_lookup(working_directory)?;
    let plugin = resolve_provider_plugin(provider, Some(&registry_path)).ok()?;
    Some(PluginLaunch {
        program: plugin.exec.command,
        args: plugin.exec.args,
        wire_protocol: PluginWireProtocol::ProviderJsonRpc,
    })
}

fn provider_registry_path_for_lookup(working_directory: Option<&str>) -> Option<PathBuf> {
    if let Some(path) = env_var("DISPATCH_PROVIDER_REGISTRY")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return Some(PathBuf::from(path));
    }

    find_project_provider_registry(working_directory)
        .or_else(|| crate::default_provider_registry_path().ok())
}

fn find_project_provider_registry(working_directory: Option<&str>) -> Option<PathBuf> {
    let start = working_directory
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())?;
    let mut cursor = if start.is_dir() {
        start
    } else {
        start.parent()?.to_path_buf()
    };

    loop {
        let candidate = cursor.join(".dispatch/registries/providers.json");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !cursor.pop() {
            break;
        }
    }

    None
}

fn plugin_binary_name(provider: &str) -> String {
    let base = format!("dispatch-backend-{provider}");
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base
    }
}

fn bundled_plugin_launch(provider: &str) -> Option<PluginLaunch> {
    let current_exe = std::env::current_exe().ok()?;
    let bin_dir = current_exe.parent()?;
    let exe_name = plugin_binary_name(provider);
    let candidates = [
        bin_dir.join(&exe_name),
        bin_dir.join("libexec").join(&exe_name),
    ];
    candidates
        .iter()
        .find(|path| path.is_file())
        .map(|path| PluginLaunch {
            program: path.display().to_string(),
            args: Vec::new(),
            wire_protocol: PluginWireProtocol::LegacyBackend,
        })
}

fn spawn_plugin_process(
    provider: &str,
    request: &ModelRequest,
) -> Result<Option<SpawnedPlugin>, CourierError> {
    let Some(launch) = resolve_plugin_launch(provider, request.working_directory.as_deref()) else {
        return Ok(None);
    };

    let (serialized, request_id) = serialize_plugin_request(provider, request, &launch)?;

    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if launch.wire_protocol == PluginWireProtocol::LegacyBackend {
        command.env("DISPATCH_BACKEND_PROTOCOL", "1");
    }

    if let Some(dir) = request.working_directory.as_deref() {
        command.current_dir(dir);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(CourierError::ModelBackendRequest(format!(
                "failed to launch `dispatch-backend-{provider}`: {error}"
            )));
        }
    };

    let Some(mut stdin) = child.stdin.take() else {
        return Err(CourierError::ModelBackendRequest(format!(
            "`dispatch-backend-{provider}` stdin was not captured"
        )));
    };
    stdin.write_all(&serialized).map_err(|error| {
        plugin_request_error(
            provider,
            launch.wire_protocol,
            format!("failed to write request: {error}"),
        )
    })?;
    drop(stdin);

    Ok(Some(SpawnedPlugin {
        child,
        wire_protocol: launch.wire_protocol,
        request_id,
    }))
}

fn collect_plugin_output(
    spawned: &mut SpawnedPlugin,
    provider: &str,
    session_capable: bool,
    request: &ModelRequest,
    on_event: &mut dyn FnMut(ModelStreamEvent),
) -> Result<ModelGeneration, CourierError> {
    match spawned.wire_protocol {
        PluginWireProtocol::LegacyBackend => collect_legacy_plugin_output(
            &mut spawned.child,
            provider,
            session_capable,
            request,
            on_event,
        ),
        PluginWireProtocol::ProviderJsonRpc => collect_provider_plugin_output(
            &mut spawned.child,
            provider,
            session_capable,
            request,
            spawned
                .request_id
                .as_ref()
                .expect("provider plugin requests always carry a request id"),
            on_event,
        ),
    }
}

fn collect_legacy_plugin_output(
    child: &mut Child,
    provider: &str,
    session_capable: bool,
    request: &ModelRequest,
    on_event: &mut dyn FnMut(ModelStreamEvent),
) -> Result<ModelGeneration, CourierError> {
    let deadline = plugin_timeout_deadline(request);
    let process_label = legacy_process_label(provider);
    let stderr_capture = spawn_stderr_capture(child, &process_label)?;
    let stdout = child.stdout.take().ok_or_else(|| {
        CourierError::ModelBackendRequest(format!(
            "`dispatch-backend-{provider}` stdout was not captured"
        ))
    })?;
    let (stdout_receiver, stdout_reader) = spawn_line_reader(BufReader::new(stdout));

    let mut streamed_text = String::new();
    let mut result_text: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut result_error: Option<String> = None;
    let mut not_configured: Option<String> = None;
    let mut tool_calls: Vec<ModelToolCall> = Vec::new();
    let mut output_error: Option<CourierError> = None;

    while let Some((bytes, line)) =
        read_line_with_timeout(&stdout_receiver, child, deadline, &process_label)?
    {
        if bytes == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => {
                output_error = Some(CourierError::ModelBackendResponse(format!(
                    "failed to parse `dispatch-backend-{provider}` output: {error}"
                )));
                let _ = child.kill();
                break;
            }
        };

        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("text_delta") => {
                if let Some(text) = value.get("content").and_then(serde_json::Value::as_str)
                    && !text.is_empty()
                {
                    streamed_text.push_str(text);
                    on_event(ModelStreamEvent::TextDelta {
                        content: text.to_string(),
                    });
                }
            }
            Some("tool_call") => {
                if let (Some(id), Some(name), Some(input)) = (
                    value.get("id").and_then(serde_json::Value::as_str),
                    value.get("name").and_then(serde_json::Value::as_str),
                    value.get("input").and_then(serde_json::Value::as_str),
                ) {
                    tool_calls.push(ModelToolCall {
                        call_id: id.to_string(),
                        name: name.to_string(),
                        input: input.to_string(),
                        kind: ModelToolKind::Function,
                    });
                }
            }
            Some("session") => {
                session_id = session_id.or_else(|| {
                    value
                        .get("session_id")
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string)
                });
            }
            Some("result") => {
                result_text = value
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .filter(|t| !t.is_empty())
                    .map(ToString::to_string)
                    .or(result_text);
                if let Some(id) = value
                    .get("response_id")
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    session_id = Some(id.to_string());
                }
                if let Some(calls) = value
                    .get("tool_calls")
                    .and_then(serde_json::Value::as_array)
                {
                    for call in calls {
                        if let (Some(id), Some(name), Some(input)) = (
                            call.get("id").and_then(serde_json::Value::as_str),
                            call.get("name").and_then(serde_json::Value::as_str),
                            call.get("input").and_then(serde_json::Value::as_str),
                        ) && !tool_calls.iter().any(|tc| tc.call_id == id)
                        {
                            tool_calls.push(ModelToolCall {
                                call_id: id.to_string(),
                                name: name.to_string(),
                                input: input.to_string(),
                                kind: ModelToolKind::Function,
                            });
                        }
                    }
                }
            }
            Some("not_configured") => {
                not_configured = Some(
                    value
                        .get("reason")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("plugin reported not_configured without a reason")
                        .to_string(),
                );
            }
            Some("error") => {
                result_error = Some(
                    value
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("plugin reported an unknown error")
                        .to_string(),
                );
            }
            _ => {}
        }
    }

    let status = match wait_for_exit(child, deadline, &process_label) {
        Ok(status) => status,
        Err(error) => {
            let _ = join_plugin_stdout_reader(stdout_reader, &process_label);
            let _ = join_stderr_capture(stderr_capture, &process_label);
            return Err(error);
        }
    };
    join_plugin_stdout_reader(stdout_reader, &process_label)?;
    let stderr_text = join_stderr_capture(stderr_capture, &process_label)?;

    if let Some(error) = output_error {
        return Err(error);
    }

    if let Some(reason) = not_configured {
        return Ok(ModelGeneration::NotConfigured {
            backend: provider.to_string(),
            reason,
        });
    }

    if let Some(message) = result_error {
        return Err(CourierError::ModelBackendRequest(message));
    }

    if !status.success() {
        let detail = stderr_text.trim();
        let suffix = if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        };
        return Err(CourierError::ModelBackendRequest(format!(
            "`dispatch-backend-{provider}` exited with status {status}{suffix}"
        )));
    }

    Ok(ModelGeneration::Reply(ModelReply {
        text: result_text.or({
            if streamed_text.is_empty() {
                None
            } else {
                Some(streamed_text)
            }
        }),
        backend: provider.to_string(),
        response_id: if session_capable { session_id } else { None },
        tool_calls,
    }))
}

fn collect_provider_plugin_output(
    child: &mut Child,
    provider: &str,
    session_capable: bool,
    request: &ModelRequest,
    request_id: &RequestId,
    on_event: &mut dyn FnMut(ModelStreamEvent),
) -> Result<ModelGeneration, CourierError> {
    let deadline = plugin_timeout_deadline(request);
    let process_label = provider_process_label(provider);
    let stderr_capture = spawn_stderr_capture(child, &process_label)?;
    let stdout = child.stdout.take().ok_or_else(|| {
        plugin_request_error(
            provider,
            PluginWireProtocol::ProviderJsonRpc,
            "stdout was not captured",
        )
    })?;
    let (stdout_receiver, stdout_reader) = spawn_line_reader(BufReader::new(stdout));

    let mut completion_response: Option<serde_json::Value> = None;
    let mut structured_error: Option<(String, String)> = None;
    let mut output_error: Option<CourierError> = None;

    while let Some((bytes, line)) =
        read_line_with_timeout(&stdout_receiver, child, deadline, &process_label)?
    {
        if bytes == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let (response_id, response) = match parse_jsonrpc_message(line) {
            Ok(message) => message,
            Err(error) => {
                output_error = Some(CourierError::ModelBackendResponse(format!(
                    "failed to parse provider plugin `{provider}` output: {error}"
                )));
                let _ = child.kill();
                break;
            }
        };

        if let Some(response_id) = response_id.as_ref()
            && response_id != request_id
        {
            output_error = Some(CourierError::ModelBackendResponse(format!(
                "provider plugin `{provider}` replied to request id `{:?}`; expected `{:?}`",
                response_id, request_id
            )));
            let _ = child.kill();
            break;
        }

        match response {
            PluginResponse::Event { event } => {
                emit_provider_stream_event(on_event, &event);
            }
            PluginResponse::Completion { response } => {
                completion_response = Some(response);
            }
            PluginResponse::Error { error } => {
                structured_error = Some((error.code, error.message));
            }
            other => {
                output_error = Some(CourierError::ModelBackendResponse(format!(
                    "provider plugin `{provider}` returned unexpected response: {other:?}"
                )));
                let _ = child.kill();
                break;
            }
        }
    }

    let status = match wait_for_exit(child, deadline, &process_label) {
        Ok(status) => status,
        Err(error) => {
            let _ = join_plugin_stdout_reader(stdout_reader, &process_label);
            let _ = join_stderr_capture(stderr_capture, &process_label);
            return Err(error);
        }
    };
    join_plugin_stdout_reader(stdout_reader, &process_label)?;
    let stderr_text = join_stderr_capture(stderr_capture, &process_label)?;

    if let Some(error) = output_error {
        return Err(error);
    }

    if let Some((code, message)) = structured_error {
        return Err(CourierError::ModelBackendRequest(format!(
            "provider plugin `{provider}` returned `{code}`: {message}"
        )));
    }

    if !status.success() {
        let detail = stderr_text.trim();
        let suffix = if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        };
        return Err(CourierError::ModelBackendRequest(format!(
            "provider plugin `{provider}` exited with status {status}{suffix}"
        )));
    }

    let response = completion_response.ok_or_else(|| {
        CourierError::ModelBackendResponse(format!(
            "provider plugin `{provider}` exited without returning a completion"
        ))
    })?;
    provider_completion_to_generation(provider, session_capable, &response)
}

fn serialize_plugin_request(
    provider: &str,
    request: &ModelRequest,
    launch: &PluginLaunch,
) -> Result<(Vec<u8>, Option<RequestId>), CourierError> {
    match launch.wire_protocol {
        PluginWireProtocol::LegacyBackend => {
            let serialized = serde_json::to_vec(request).map_err(|error| {
                CourierError::ModelBackendRequest(format!(
                    "failed to serialize request for `dispatch-backend-{provider}`: {error}"
                ))
            })?;
            Ok((serialized, None))
        }
        PluginWireProtocol::ProviderJsonRpc => {
            let request_id = RequestId::integer(1);
            let envelope = PluginRequestEnvelope {
                protocol_version: PROVIDER_PLUGIN_PROTOCOL_VERSION,
                request: PluginRequest::Complete {
                    params: provider_complete_params(request),
                },
            };
            let jsonrpc = request_to_jsonrpc(request_id.clone(), &envelope).map_err(|error| {
                CourierError::ModelBackendRequest(format!(
                    "failed to encode provider.complete request for `{provider}`: {error}"
                ))
            })?;
            let mut serialized = serde_json::to_vec(&jsonrpc).map_err(|error| {
                CourierError::ModelBackendRequest(format!(
                    "failed to serialize provider.complete request for `{provider}`: {error}"
                ))
            })?;
            serialized.push(b'\n');
            Ok((serialized, Some(request_id)))
        }
    }
}

fn provider_complete_params(request: &ModelRequest) -> serde_json::Value {
    let mut parameters = serde_json::Map::new();
    for key in [
        "temperature",
        "top_p",
        "presence_penalty",
        "frequency_penalty",
        "max_tokens",
        "max_output_tokens",
    ] {
        if let Some(value) = request.model_options.get(key) {
            parameters.insert(key.to_string(), coerce_model_option_value(value));
        }
    }

    serde_json::json!({
        "model": request.model,
        "messages": provider_messages(request),
        "tools": request
            .tools
            .iter()
            .map(provider_tool_definition)
            .collect::<Vec<_>>(),
        "tool_choice": if request.tools.is_empty() { "none" } else { "auto" },
        "parameters": parameters,
        "metadata": {
            "provider": request.provider,
            "previous_response_id": request.previous_response_id,
            "context_token_limit": request.context_token_limit,
            "tool_call_limit": request.tool_call_limit,
            "tool_output_limit": request.tool_output_limit,
            "working_directory": request.working_directory,
        }
    })
}

fn provider_messages(request: &ModelRequest) -> Vec<serde_json::Value> {
    let mut messages = Vec::new();
    if !request.instructions.trim().is_empty() {
        messages.push(serde_json::json!({
            "role": "system",
            "content": [
                {
                    "kind": "text",
                    "text": request.instructions,
                }
            ],
        }));
    }
    messages.extend(request.messages.iter().map(|message| {
        serde_json::json!({
            "role": message.role,
            "content": [
                {
                    "kind": "text",
                    "text": message.content,
                }
            ],
        })
    }));
    if !request.pending_tool_calls.is_empty() {
        messages.push(serde_json::json!({
            "role": "assistant",
            "content": request
                .pending_tool_calls
                .iter()
                .map(|call| serde_json::json!({
                    "kind": "tool_use",
                    "id": call.call_id,
                    "name": call.name,
                    "input": serde_json::from_str::<serde_json::Value>(&call.input)
                        .unwrap_or_else(|_| serde_json::Value::String(call.input.clone())),
                }))
                .collect::<Vec<_>>(),
        }));
    }
    messages.extend(request.tool_outputs.iter().map(|output| {
        serde_json::json!({
            "role": "user",
            "content": [
                {
                    "kind": "tool_result",
                    "tool_use_id": output.call_id,
                    "content": [
                        {
                            "kind": "text",
                            "text": output.output,
                        }
                    ],
                }
            ],
        })
    }));
    messages
}

fn provider_tool_definition(tool: &ModelToolDefinition) -> serde_json::Value {
    serde_json::json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": function_parameters_for_tool(tool),
    })
}

fn coerce_model_option_value(value: &str) -> serde_json::Value {
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(value) {
        return parsed;
    }
    if let Ok(parsed) = value.parse::<i64>() {
        return serde_json::Value::from(parsed);
    }
    if let Ok(parsed) = value.parse::<f64>() {
        return serde_json::Value::from(parsed);
    }
    match value.trim().to_ascii_lowercase().as_str() {
        "true" => serde_json::Value::Bool(true),
        "false" => serde_json::Value::Bool(false),
        _ => serde_json::Value::String(value.to_string()),
    }
}

fn emit_provider_stream_event(
    on_event: &mut dyn FnMut(ModelStreamEvent),
    event: &serde_json::Value,
) {
    if event.get("kind").and_then(serde_json::Value::as_str) != Some("content_delta") {
        return;
    }
    let Some(delta) = event.get("delta") else {
        return;
    };
    let text = match delta.get("kind").and_then(serde_json::Value::as_str) {
        Some("text") => delta.get("text").and_then(serde_json::Value::as_str),
        _ => None,
    };
    if let Some(text) = text.filter(|text| !text.is_empty()) {
        on_event(ModelStreamEvent::TextDelta {
            content: text.to_string(),
        });
    }
}

fn provider_completion_to_generation(
    provider: &str,
    session_capable: bool,
    response: &serde_json::Value,
) -> Result<ModelGeneration, CourierError> {
    let response_id = response
        .get("response_id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| response.get("id").and_then(serde_json::Value::as_str))
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let text = provider_completion_text(response);
    let tool_calls = provider_completion_tool_calls(response)?;

    Ok(ModelGeneration::Reply(ModelReply {
        text,
        backend: provider.to_string(),
        response_id: if session_capable { response_id } else { None },
        tool_calls,
    }))
}

fn provider_completion_text(response: &serde_json::Value) -> Option<String> {
    if let Some(text) = response.get("text").and_then(serde_json::Value::as_str)
        && !text.is_empty()
    {
        return Some(text.to_string());
    }

    let text = response
        .get("content")
        .and_then(serde_json::Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| {
                    block.get("kind").and_then(serde_json::Value::as_str) == Some("text")
                })
                .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str))
                .collect::<String>()
        })
        .unwrap_or_default();

    if text.is_empty() { None } else { Some(text) }
}

fn provider_completion_tool_calls(
    response: &serde_json::Value,
) -> Result<Vec<ModelToolCall>, CourierError> {
    let mut tool_calls = Vec::new();

    if let Some(blocks) = response
        .get("content")
        .and_then(serde_json::Value::as_array)
    {
        for block in blocks {
            if block.get("kind").and_then(serde_json::Value::as_str) != Some("tool_use") {
                continue;
            }
            tool_calls.push(provider_tool_call_from_value(block)?);
        }
    }

    if let Some(calls) = response
        .get("tool_calls")
        .and_then(serde_json::Value::as_array)
    {
        for call in calls {
            let parsed = provider_tool_call_from_value(call)?;
            if !tool_calls
                .iter()
                .any(|existing| existing.call_id == parsed.call_id)
            {
                tool_calls.push(parsed);
            }
        }
    }

    Ok(tool_calls)
}

fn provider_tool_call_from_value(value: &serde_json::Value) -> Result<ModelToolCall, CourierError> {
    let call_id = value
        .get("id")
        .or_else(|| value.get("tool_call_id"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            CourierError::ModelBackendResponse(
                "provider tool call missing `id` or `tool_call_id`".to_string(),
            )
        })?;
    let name = value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            CourierError::ModelBackendResponse("provider tool call missing `name`".to_string())
        })?;
    let input = value
        .get("input")
        .or_else(|| value.get("arguments"))
        .map(provider_tool_call_input)
        .ok_or_else(|| {
            CourierError::ModelBackendResponse(
                "provider tool call missing `input` or `arguments`".to_string(),
            )
        })?;

    Ok(ModelToolCall {
        call_id: call_id.to_string(),
        name: name.to_string(),
        input,
        kind: ModelToolKind::Function,
    })
}

fn provider_tool_call_input(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn legacy_process_label(provider: &str) -> String {
    format!("`dispatch-backend-{provider}`")
}

fn provider_process_label(provider: &str) -> String {
    format!("provider plugin `{provider}`")
}

fn plugin_request_error(
    provider: &str,
    wire_protocol: PluginWireProtocol,
    message: impl AsRef<str>,
) -> CourierError {
    let label = match wire_protocol {
        PluginWireProtocol::LegacyBackend => legacy_process_label(provider),
        PluginWireProtocol::ProviderJsonRpc => provider_process_label(provider),
    };
    CourierError::ModelBackendRequest(format!("{} {}", label, message.as_ref()))
}

fn plugin_timeout_deadline(request: &ModelRequest) -> Option<Instant> {
    request
        .llm_timeout_ms
        .map(Duration::from_millis)
        .and_then(|timeout| Instant::now().checked_add(timeout))
}

fn spawn_stderr_capture(
    child: &mut Child,
    process_label: &str,
) -> Result<JoinHandle<String>, CourierError> {
    let stderr = child.stderr.take().ok_or_else(|| {
        CourierError::ModelBackendRequest(format!("{process_label} stderr was not captured"))
    })?;
    Ok(thread::spawn(move || {
        let mut text = String::new();
        let mut stderr = BufReader::new(stderr);
        let _ = stderr.read_to_string(&mut text);
        text
    }))
}

fn join_stderr_capture(
    handle: JoinHandle<String>,
    process_label: &str,
) -> Result<String, CourierError> {
    handle.join().map_err(|_| {
        CourierError::ModelBackendRequest(format!("{process_label} stderr reader panicked"))
    })
}

fn join_plugin_stdout_reader(
    handle: JoinHandle<()>,
    process_label: &str,
) -> Result<(), CourierError> {
    join_line_reader(
        handle,
        CourierError::ModelBackendRequest(format!("{process_label} stdout reader panicked")),
    )
}

fn plugin_timeout_error(process_label: &str) -> CourierError {
    CourierError::ModelBackendRequest(format!("{process_label} request timed out"))
}

fn remaining_timeout(
    deadline: Option<Instant>,
    process_label: &str,
) -> Result<Option<Duration>, CourierError> {
    let Some(deadline) = deadline else {
        return Ok(None);
    };
    deadline
        .checked_duration_since(Instant::now())
        .map(Some)
        .ok_or_else(|| plugin_timeout_error(process_label))
}

fn read_line_with_timeout(
    receiver: &std::sync::mpsc::Receiver<LineReadResult>,
    child: &mut Child,
    deadline: Option<Instant>,
    process_label: &str,
) -> Result<Option<(usize, String)>, CourierError> {
    recv_line_with_timeout(
        receiver,
        child,
        remaining_timeout(deadline, process_label)?,
        plugin_timeout_error(process_label),
        CourierError::ModelBackendRequest(format!(
            "{process_label} output reader disconnected unexpectedly"
        )),
    )
}

fn wait_for_exit(
    child: &mut Child,
    deadline: Option<Instant>,
    process_label: &str,
) -> Result<ExitStatus, CourierError> {
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| CourierError::ModelBackendRequest(error.to_string()))?
        {
            return Ok(status);
        }

        let sleep_for = match remaining_timeout(deadline, process_label)? {
            Some(timeout) if timeout.is_zero() => {
                let _ = child.kill();
                return Err(plugin_timeout_error(process_label));
            }
            Some(timeout) => timeout.min(Duration::from_millis(25)),
            None => Duration::from_millis(25),
        };
        thread::sleep(sleep_for);
    }
}
