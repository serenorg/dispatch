use super::{
    Arc, BuiltinToolSpec, ChatModelBackend, ConversationMessage, CourierError, Cow, Instant,
    LoadedParcel, LocalToolSpec, LocalToolTarget, ModelReference, ModelRequest,
    ModelToolDefinition, ModelToolFormat, Sha256, WasmModelRequestInput,
    builtin_memory_tool_description, default_chat_backend_for_provider, effective_llm_timeout_ms,
    encode_hex, is_provider_session_state_capable, list_native_builtin_tools, process_env_lookup,
    resolve_prompt_text,
};
use sha2::Digest;
use std::fs;

#[cfg(test)]
pub(super) fn build_model_request(
    image: &LoadedParcel,
    messages: &[ConversationMessage],
    local_tools: &[LocalToolSpec],
) -> Result<Option<ModelRequest>, CourierError> {
    Ok(
        build_model_requests(image, messages, local_tools, None, None)?
            .into_iter()
            .next(),
    )
}

pub(super) fn build_model_requests(
    image: &LoadedParcel,
    messages: &[ConversationMessage],
    local_tools: &[LocalToolSpec],
    run_deadline: Option<Instant>,
    backend_state: Option<&str>,
) -> Result<Vec<ModelRequest>, CourierError> {
    let model_refs = configured_model_references(&image.config.models);
    if model_refs.is_empty() {
        return Ok(Vec::new());
    }
    let builtin_tools = list_native_builtin_tools(image);
    let mut tools = local_tools
        .iter()
        .map(|tool| build_model_tool_definition(image, tool))
        .collect::<Result<Vec<_>, _>>()?;
    tools.extend(
        builtin_tools
            .iter()
            .map(build_builtin_model_tool_definition),
    );
    let instructions = resolve_prompt_text(image)?;
    let llm_timeout_ms = effective_llm_timeout_ms(&image.config.timeouts, run_deadline)?;
    let working_directory = model_working_directory(image);

    Ok(model_refs
        .into_iter()
        .map(|model| {
            // Only pass backend_state to providers whose previous_response_id is
            // used for Dispatch-managed session resume semantics. Hosted backends
            // use previous_response_id for provider-owned response continuation
            // and would reject a Dispatch-local session token.
            let uses_dispatch_session_state = model
                .provider
                .as_deref()
                .map(is_provider_session_state_capable)
                .or_else(|| {
                    process_env_lookup("LLM_BACKEND")
                        .map(|provider| is_provider_session_state_capable(&provider))
                })
                .unwrap_or(false);
            ModelRequest {
                model: model.id,
                provider: model.provider.clone(),
                model_options: model.options.clone(),
                llm_timeout_ms,
                context_token_limit: configured_context_token_limit(&image.config.limits),
                tool_call_limit: configured_tool_call_limit(&image.config.limits),
                tool_output_limit: configured_tool_output_limit(&image.config.limits),
                working_directory: working_directory.clone(),
                instructions: instructions.clone(),
                messages: messages.to_vec(),
                tools: tools.clone(),
                pending_tool_calls: Vec::new(),
                tool_outputs: Vec::new(),
                previous_response_id: if uses_dispatch_session_state {
                    backend_state.map(ToString::to_string)
                } else {
                    None
                },
            }
        })
        .collect())
}

fn configured_model_references(policy: &crate::manifest::ModelPolicy) -> Vec<ModelReference> {
    let mut models = Vec::new();
    if let Some(primary) = &policy.primary {
        models.push(primary.clone());
        models.extend(policy.fallbacks.iter().cloned());
        return models;
    }
    let Some(model) = configured_model_id(None) else {
        return models;
    };
    models.push(ModelReference {
        id: model,
        provider: std::env::var("LLM_BACKEND").ok(),
        options: Default::default(),
    });
    models
}

pub(super) fn build_wasm_model_requests(
    parcel: &LoadedParcel,
    input: WasmModelRequestInput,
) -> Result<Vec<ModelRequest>, CourierError> {
    let configured = configured_model_references(&parcel.config.models);
    let model_refs = match input.requested_model {
        Some(model) => {
            if let Some(index) = configured
                .iter()
                .position(|candidate| candidate.id == model)
            {
                configured[index..].to_vec()
            } else {
                vec![ModelReference {
                    id: model,
                    provider: None,
                    options: Default::default(),
                }]
            }
        }
        None => configured,
    };

    if model_refs.is_empty() {
        return Err(CourierError::ModelBackendRequest(
            "no model configured for wasm guest request".to_string(),
        ));
    }

    let llm_timeout_ms = effective_llm_timeout_ms(&parcel.config.timeouts, input.run_deadline)?;

    Ok(model_refs
        .into_iter()
        .map(|model| ModelRequest {
            model: model.id,
            provider: model.provider.clone(),
            model_options: model.options.clone(),
            llm_timeout_ms,
            context_token_limit: configured_context_token_limit(&parcel.config.limits),
            tool_call_limit: configured_tool_call_limit(&parcel.config.limits),
            tool_output_limit: configured_tool_output_limit(&parcel.config.limits),
            working_directory: model_working_directory(parcel),
            instructions: input.instructions.clone(),
            messages: input.messages.clone(),
            tools: input.tools.clone(),
            pending_tool_calls: Vec::new(),
            tool_outputs: input.tool_outputs.clone(),
            previous_response_id: input.previous_response_id.clone(),
        })
        .collect())
}

fn model_working_directory(parcel: &LoadedParcel) -> Option<String> {
    let context_dir = parcel.parcel_dir.join("context");
    if context_dir.is_dir() {
        return Some(context_dir.display().to_string());
    }
    Some(parcel.parcel_dir.display().to_string())
}

#[cfg(test)]
pub(super) fn configured_context_token_limit(limits: &[crate::manifest::LimitSpec]) -> Option<u32> {
    configured_limit_u32(limits, "CONTEXT_TOKENS")
}

#[cfg(not(test))]
pub(super) fn configured_context_token_limit(limits: &[crate::manifest::LimitSpec]) -> Option<u32> {
    configured_limit_u32(limits, "CONTEXT_TOKENS")
}

#[cfg(test)]
pub(super) fn configured_tool_call_limit(limits: &[crate::manifest::LimitSpec]) -> Option<u32> {
    configured_limit_u32(limits, "TOOL_CALLS")
}

#[cfg(not(test))]
pub(super) fn configured_tool_call_limit(limits: &[crate::manifest::LimitSpec]) -> Option<u32> {
    configured_limit_u32(limits, "TOOL_CALLS")
}

#[cfg(test)]
pub(super) fn configured_tool_output_limit(limits: &[crate::manifest::LimitSpec]) -> Option<usize> {
    configured_limit_u32(limits, "TOOL_OUTPUT").map(|value| value as usize)
}

#[cfg(not(test))]
pub(super) fn configured_tool_output_limit(limits: &[crate::manifest::LimitSpec]) -> Option<usize> {
    configured_limit_u32(limits, "TOOL_OUTPUT").map(|value| value as usize)
}

#[cfg(test)]
pub(super) fn configured_tool_round_limit(limits: &[crate::manifest::LimitSpec]) -> Option<u32> {
    configured_limit_u32(limits, "TOOL_ROUNDS")
}

#[cfg(not(test))]
pub(super) fn configured_tool_round_limit(limits: &[crate::manifest::LimitSpec]) -> Option<u32> {
    configured_limit_u32(limits, "TOOL_ROUNDS")
}

fn configured_limit_u32(limits: &[crate::manifest::LimitSpec], scope: &str) -> Option<u32> {
    limits
        .iter()
        .rev()
        .find(|limit| limit.scope.eq_ignore_ascii_case(scope))
        .and_then(|limit| limit.value.parse::<u32>().ok())
        .filter(|value| *value > 0)
}

#[cfg(test)]
pub(super) fn truncate_tool_output(output: String, limit: Option<usize>) -> String {
    truncate_tool_output_impl(output, limit)
}

#[cfg(not(test))]
pub(super) fn truncate_tool_output(output: String, limit: Option<usize>) -> String {
    truncate_tool_output_impl(output, limit)
}

fn truncate_tool_output_impl(output: String, limit: Option<usize>) -> String {
    const TRUNCATION_NOTE: &str = "\n\n[dispatch truncated tool output]";
    let Some(limit) = limit else {
        return output;
    };
    if output.len() <= limit {
        return output;
    }
    if limit <= TRUNCATION_NOTE.len() {
        return TRUNCATION_NOTE[..limit].to_string();
    }
    let keep = limit - TRUNCATION_NOTE.len();
    let mut truncated = String::with_capacity(limit);
    let mut used = 0usize;
    for ch in output.chars() {
        let ch_len = ch.len_utf8();
        if used + ch_len > keep {
            break;
        }
        truncated.push(ch);
        used += ch_len;
    }
    truncated.push_str(TRUNCATION_NOTE);
    truncated
}

pub(super) fn select_chat_backend(
    chat_backend_override: Option<&Arc<dyn ChatModelBackend>>,
    request: &ModelRequest,
) -> Arc<dyn ChatModelBackend> {
    match chat_backend_override {
        Some(backend) => backend.clone(),
        None => default_chat_backend_for_provider(request.provider.as_deref()),
    }
}

pub(super) fn build_model_tool_definition(
    image: &LoadedParcel,
    tool: &LocalToolSpec,
) -> Result<ModelToolDefinition, CourierError> {
    let description = tool.description.clone().unwrap_or_else(|| match &tool.target {
        LocalToolTarget::Local { packaged_path, .. } => format!(
            "Local Dispatch tool `{}` packaged at `{}`. Provide free-form text or JSON input appropriate for the tool.",
            tool.alias, packaged_path
        ),
        LocalToolTarget::A2a { .. } => format!(
            "Dispatch A2A tool `{}` delegates to the configured remote agent endpoint. Provide free-form text or JSON input appropriate for the remote agent.",
            tool.alias
        ),
    });
    let format = match (
        tool.input_schema_packaged_path.as_deref(),
        tool.input_schema_sha256.as_deref(),
    ) {
        (Some(source), expected_sha256) => ModelToolFormat::JsonSchema {
            schema: load_tool_schema(image, &tool.alias, source, expected_sha256)?,
        },
        (None, _) => ModelToolFormat::Text,
    };

    Ok(ModelToolDefinition {
        name: tool.alias.clone(),
        description,
        format,
    })
}

pub(super) fn build_builtin_model_tool_definition(tool: &BuiltinToolSpec) -> ModelToolDefinition {
    ModelToolDefinition {
        name: tool.capability.clone(),
        description: builtin_memory_tool_description(tool),
        format: ModelToolFormat::JsonSchema {
            schema: tool.input_schema.clone(),
        },
    }
}

#[cfg(test)]
pub(super) fn normalize_local_tool_input<'a>(
    tool: &LocalToolSpec,
    input: &'a str,
) -> Result<Cow<'a, str>, CourierError> {
    normalize_local_tool_input_impl(tool, input)
}

#[cfg(not(test))]
pub(super) fn normalize_local_tool_input<'a>(
    tool: &LocalToolSpec,
    input: &'a str,
) -> Result<Cow<'a, str>, CourierError> {
    normalize_local_tool_input_impl(tool, input)
}

fn normalize_local_tool_input_impl<'a>(
    tool: &LocalToolSpec,
    input: &'a str,
) -> Result<Cow<'a, str>, CourierError> {
    if tool.input_schema_packaged_path.is_some() {
        return Ok(Cow::Borrowed(input));
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(input) else {
        return Ok(Cow::Borrowed(input));
    };
    let Some(object) = value.as_object() else {
        return Ok(Cow::Borrowed(input));
    };
    if object.len() != 1 {
        return Ok(Cow::Borrowed(input));
    }
    match object.get("input").and_then(serde_json::Value::as_str) {
        Some(value) => Ok(Cow::Owned(value.to_string())),
        None => Ok(Cow::Borrowed(input)),
    }
}

pub(super) fn load_tool_schema(
    image: &LoadedParcel,
    tool: &str,
    packaged_path: &str,
    expected_sha256: Option<&str>,
) -> Result<serde_json::Value, CourierError> {
    let path = image.parcel_dir.join("context").join(packaged_path);
    let body = fs::read(&path).map_err(|source_error| CourierError::ReadFile {
        path: path.display().to_string(),
        source: source_error,
    })?;
    if let Some(expected_sha256) = expected_sha256 {
        let actual_sha256 = encode_hex(Sha256::digest(&body));
        if actual_sha256 != expected_sha256 {
            return Err(CourierError::ToolSchemaDigestMismatch {
                tool: tool.to_string(),
                path: path.display().to_string(),
                expected_sha256: expected_sha256.to_string(),
                actual_sha256,
            });
        }
    }
    let schema: serde_json::Value =
        serde_json::from_slice(&body).map_err(|source_error| CourierError::ParseToolSchema {
            tool: tool.to_string(),
            path: path.display().to_string(),
            source: source_error,
        })?;
    if !schema.is_object() {
        return Err(CourierError::ToolSchemaShape {
            tool: tool.to_string(),
            path: path.display().to_string(),
        });
    }

    Ok(schema)
}

pub(super) fn configured_model_id(primary: Option<&ModelReference>) -> Option<String> {
    configured_model_id_with(primary, process_env_lookup)
}

#[cfg(test)]
pub(super) fn configured_model_id_with<F>(
    primary: Option<&ModelReference>,
    mut env_lookup: F,
) -> Option<String>
where
    F: FnMut(&str) -> Option<String>,
{
    primary
        .map(|model| model.id.clone())
        .or_else(|| env_lookup("LLM_MODEL"))
}

#[cfg(not(test))]
fn configured_model_id_with<F>(
    primary: Option<&ModelReference>,
    mut env_lookup: F,
) -> Option<String>
where
    F: FnMut(&str) -> Option<String>,
{
    primary
        .map(|model| model.id.clone())
        .or_else(|| env_lookup("LLM_MODEL"))
}
