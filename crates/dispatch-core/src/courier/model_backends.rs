use super::*;
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    time::Duration,
};

mod openai;

use openai::{OpenAiChatCompletionsBackend, OpenAiResponsesBackend};
#[cfg(test)]
pub(super) use openai::{
    extract_openai_chat_completions_output, extract_openai_output,
    openai_chat_completions_messages, openai_tool_definition,
    parse_openai_chat_completions_streaming_events, parse_openai_streaming_events,
};

struct AnthropicMessagesBackend;
struct GeminiGenerateContentBackend;

const DEFAULT_ANTHROPIC_MAX_TOKENS: u32 = 2048;

fn request_timeout(request: &ModelRequest) -> Option<Duration> {
    request.llm_timeout_ms.map(Duration::from_millis)
}

fn generate_with_noop_events(
    backend: &dyn ChatModelBackend,
    request: &ModelRequest,
) -> Result<ModelGeneration, CourierError> {
    backend.generate_with_events(request, &mut |_| {})
}

pub(super) fn default_chat_backend_for_provider(
    provider: Option<&str>,
) -> Arc<dyn ChatModelBackend> {
    default_chat_backend_for_provider_with(provider, process_env_lookup)
}

pub(super) fn default_chat_backend_for_provider_with<F>(
    provider: Option<&str>,
    mut env_lookup: F,
) -> Arc<dyn ChatModelBackend>
where
    F: FnMut(&str) -> Option<String>,
{
    match provider
        .map(ToString::to_string)
        .or_else(|| env_lookup("LLM_BACKEND"))
        .unwrap_or_else(|| "openai".to_string())
        .to_ascii_lowercase()
        .as_str()
    {
        "anthropic" => Arc::new(AnthropicMessagesBackend),
        "gemini" | "google" | "google_gemini" => Arc::new(GeminiGenerateContentBackend),
        "openai_compatible" | "openrouter" | "together" | "fireworks" | "litellm" | "vllm"
        | "lm_studio" => Arc::new(OpenAiChatCompletionsBackend),
        _ => Arc::new(OpenAiResponsesBackend),
    }
}

impl ChatModelBackend for AnthropicMessagesBackend {
    fn id(&self) -> &str {
        "anthropic_messages"
    }

    fn generate(&self, request: &ModelRequest) -> Result<ModelGeneration, CourierError> {
        generate_with_noop_events(self, request)
    }

    fn generate_with_events(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent),
    ) -> Result<ModelGeneration, CourierError> {
        let api_key = match model_api_key("LLM_API_KEY", "ANTHROPIC_API_KEY") {
            Some(value) => value,
            None => {
                return Ok(ModelGeneration::NotConfigured {
                    backend: self.id().to_string(),
                    reason: "missing LLM_API_KEY or ANTHROPIC_API_KEY".to_string(),
                });
            }
        };

        let base_url = model_base_url(
            "LLM_BASE_URL",
            "ANTHROPIC_BASE_URL",
            "https://api.anthropic.com",
        );
        let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));
        let payload = serde_json::json!({
            "model": request.model,
            "max_tokens": anthropic_max_tokens(request),
            "system": request.instructions,
            "messages": anthropic_messages(request),
            "stream": true,
            "tools": request
                .tools
                .iter()
                .map(anthropic_tool_definition)
                .collect::<Vec<_>>(),
        });

        let response = ureq::post(&url)
            .config()
            .http_status_as_error(false)
            .timeout_global(request_timeout(request))
            .build()
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .send_json(payload)
            .map_err(|error| CourierError::ModelBackendRequest(error.to_string()))?;

        read_anthropic_streaming_response(response, self.id(), on_event)
    }
}

pub(super) fn anthropic_max_tokens(request: &ModelRequest) -> u32 {
    request
        .context_token_limit
        .unwrap_or(DEFAULT_ANTHROPIC_MAX_TOKENS)
}

impl ChatModelBackend for GeminiGenerateContentBackend {
    fn id(&self) -> &str {
        "google_gemini_generate_content"
    }

    fn generate(&self, request: &ModelRequest) -> Result<ModelGeneration, CourierError> {
        generate_with_noop_events(self, request)
    }

    fn generate_with_events(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent),
    ) -> Result<ModelGeneration, CourierError> {
        let api_key = std::env::var("LLM_API_KEY")
            .ok()
            .or_else(|| std::env::var("GEMINI_API_KEY").ok())
            .or_else(|| std::env::var("GOOGLE_API_KEY").ok());
        let api_key = match api_key {
            Some(value) => value,
            None => {
                return Ok(ModelGeneration::NotConfigured {
                    backend: self.id().to_string(),
                    reason: "missing LLM_API_KEY, GEMINI_API_KEY, or GOOGLE_API_KEY".to_string(),
                });
            }
        };

        let base_url = std::env::var("LLM_BASE_URL")
            .ok()
            .or_else(|| std::env::var("GEMINI_BASE_URL").ok())
            .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".to_string());
        let model = if request.model.starts_with("models/") {
            request.model.clone()
        } else {
            format!("models/{}", request.model)
        };
        let url = format!(
            "{}/{model}:streamGenerateContent?alt=sse",
            base_url.trim_end_matches('/')
        );
        let mut payload = serde_json::json!({
            "systemInstruction": {
                "parts": [{ "text": request.instructions }]
            },
            "contents": gemini_messages(request),
        });
        if !request.tools.is_empty() {
            payload["tools"] = serde_json::json!([{
                "functionDeclarations": request
                    .tools
                    .iter()
                    .map(gemini_tool_definition)
                    .collect::<Vec<_>>()
            }]);
        }

        let response = ureq::post(&url)
            .config()
            .http_status_as_error(false)
            .timeout_global(request_timeout(request))
            .build()
            .header("x-goog-api-key", &api_key)
            .header("content-type", "application/json")
            .send_json(payload)
            .map_err(|error| CourierError::ModelBackendRequest(error.to_string()))?;

        read_gemini_streaming_response(response, self.id(), on_event)
    }
}

fn model_api_key(primary: &str, fallback: &str) -> Option<String> {
    std::env::var(primary)
        .ok()
        .or_else(|| std::env::var(fallback).ok())
}

fn model_base_url(primary: &str, fallback: &str, default: &str) -> String {
    std::env::var(primary)
        .ok()
        .or_else(|| std::env::var(fallback).ok())
        .unwrap_or_else(|| default.to_string())
}

fn read_anthropic_streaming_response(
    mut response: ureq::http::Response<ureq::Body>,
    backend: &str,
    on_event: &mut dyn FnMut(ModelStreamEvent),
) -> Result<ModelGeneration, CourierError> {
    let status = response.status();
    if !status.is_success() {
        let body = response.body_mut().read_to_string().unwrap_or_default();
        let detail = format_provider_error_body(&body);
        let message = if detail.is_empty() {
            format!("{backend} HTTP {}", status.as_u16())
        } else {
            format!("{backend} HTTP {}: {detail}", status.as_u16())
        };
        return Err(CourierError::ModelBackendRequest(message));
    }

    let reader = BufReader::new(response.body_mut().as_reader());
    let body = parse_anthropic_streaming_events(reader, on_event)?;
    extract_anthropic_output(&body)
}

fn read_gemini_streaming_response(
    mut response: ureq::http::Response<ureq::Body>,
    backend: &str,
    on_event: &mut dyn FnMut(ModelStreamEvent),
) -> Result<ModelGeneration, CourierError> {
    let status = response.status();
    if !status.is_success() {
        let body = response.body_mut().read_to_string().unwrap_or_default();
        let detail = format_provider_error_body(&body);
        let message = if detail.is_empty() {
            format!("{backend} HTTP {}", status.as_u16())
        } else {
            format!("{backend} HTTP {}: {detail}", status.as_u16())
        };
        return Err(CourierError::ModelBackendRequest(message));
    }

    let reader = BufReader::new(response.body_mut().as_reader());
    let body = parse_gemini_streaming_events(reader, on_event)?;
    extract_gemini_output(&body)
}

fn parse_anthropic_streaming_events<R: BufRead>(
    reader: R,
    on_event: &mut dyn FnMut(ModelStreamEvent),
) -> Result<serde_json::Value, CourierError> {
    #[derive(Default)]
    struct AnthropicToolUse {
        id: String,
        name: String,
        input_json: String,
    }

    let mut message_id = None;
    let mut text = String::new();
    let mut tool_uses = BTreeMap::<usize, AnthropicToolUse>::new();

    for event_data in parse_sse_events(reader)? {
        let data = event_data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let value = serde_json::from_str::<serde_json::Value>(data)
            .map_err(|error| CourierError::ModelBackendResponse(error.to_string()))?;

        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("message_start") => {
                message_id = value
                    .pointer("/message/id")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string);
            }
            Some("content_block_start") => {
                let Some(index) = value
                    .get("index")
                    .and_then(serde_json::Value::as_u64)
                    .map(|v| v as usize)
                else {
                    continue;
                };
                let Some(block) = value.get("content_block") else {
                    continue;
                };
                if block.get("type").and_then(serde_json::Value::as_str) == Some("tool_use") {
                    let id = block
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let input_json = block
                        .get("input")
                        .map(serde_json::to_string)
                        .transpose()
                        .map_err(|error| {
                            CourierError::ModelBackendResponse(format!(
                                "failed to serialize anthropic stream tool input: {error}"
                            ))
                        })?
                        .unwrap_or_default();
                    tool_uses.insert(
                        index,
                        AnthropicToolUse {
                            id,
                            name,
                            input_json,
                        },
                    );
                }
            }
            Some("content_block_delta") => {
                let Some(delta) = value.get("delta") else {
                    continue;
                };
                match delta.get("type").and_then(serde_json::Value::as_str) {
                    Some("text_delta") => {
                        if let Some(chunk) = delta.get("text").and_then(serde_json::Value::as_str) {
                            text.push_str(chunk);
                            on_event(ModelStreamEvent::TextDelta {
                                content: chunk.to_string(),
                            });
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(index) = value
                            .get("index")
                            .and_then(serde_json::Value::as_u64)
                            .map(|v| v as usize)
                            && let Some(tool_use) = tool_uses.get_mut(&index)
                            && let Some(partial_json) = delta
                                .get("partial_json")
                                .and_then(serde_json::Value::as_str)
                        {
                            tool_use.input_json.push_str(partial_json);
                        }
                    }
                    _ => {}
                }
            }
            Some("error") => {
                let message = value
                    .pointer("/error/message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("anthropic streaming response failed");
                return Err(CourierError::ModelBackendRequest(message.to_string()));
            }
            _ => {}
        }
    }

    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(serde_json::json!({
            "type": "text",
            "text": text,
        }));
    }
    content.extend(tool_uses.into_values().map(|tool_use| {
        let input = serde_json::from_str::<serde_json::Value>(&tool_use.input_json)
            .unwrap_or_else(|_| serde_json::json!({ "input": tool_use.input_json }));
        serde_json::json!({
            "type": "tool_use",
            "id": tool_use.id,
            "name": tool_use.name,
            "input": input,
        })
    }));

    Ok(serde_json::json!({
        "id": message_id,
        "content": content,
    }))
}

fn parse_gemini_streaming_events<R: BufRead>(
    reader: R,
    on_event: &mut dyn FnMut(ModelStreamEvent),
) -> Result<serde_json::Value, CourierError> {
    let mut text = String::new();
    let mut tool_calls = Vec::<serde_json::Value>::new();

    for event_data in parse_sse_data_events(reader)? {
        let data = event_data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let value = serde_json::from_str::<serde_json::Value>(data)
            .map_err(|error| CourierError::ModelBackendResponse(error.to_string()))?;
        let Some(candidate) = value
            .get("candidates")
            .and_then(serde_json::Value::as_array)
            .and_then(|candidates| candidates.first())
        else {
            continue;
        };
        let Some(parts) = candidate
            .get("content")
            .and_then(|content| content.get("parts"))
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        for part in parts {
            if let Some(chunk) = part.get("text").and_then(serde_json::Value::as_str) {
                text.push_str(chunk);
                on_event(ModelStreamEvent::TextDelta {
                    content: chunk.to_string(),
                });
            }
            if let Some(function_call) = part
                .get("functionCall")
                .or_else(|| part.get("function_call"))
            {
                tool_calls.push(serde_json::json!({
                    "functionCall": function_call.clone(),
                }));
            }
        }
    }

    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(serde_json::json!({ "text": text }));
    }
    parts.extend(tool_calls);

    Ok(serde_json::json!({
        "candidates": [{
            "content": {
                "parts": parts,
            }
        }]
    }))
}

fn parse_sse_data_events<R: BufRead>(reader: R) -> Result<Vec<String>, CourierError> {
    parse_sse_events(reader)
}

fn parse_sse_events<R: BufRead>(mut reader: R) -> Result<Vec<String>, CourierError> {
    let mut events = Vec::new();
    let mut current = String::new();
    let mut line = String::new();

    loop {
        line.clear();
        let bytes_read = reader
            .read_line(&mut line)
            .map_err(|error| CourierError::ModelBackendResponse(error.to_string()))?;
        if bytes_read == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if !current.is_empty() {
                events.push(std::mem::take(&mut current));
            }
            continue;
        }
        if let Some(data) = trimmed.strip_prefix("data:") {
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(data.trim_start());
        }
    }

    if !current.is_empty() {
        events.push(current);
    }

    Ok(events)
}

fn format_provider_error_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed)
        && let Some(message) = value
            .pointer("/error/message")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                value
                    .pointer("/message")
                    .and_then(serde_json::Value::as_str)
            })
    {
        return message.to_string();
    }
    trimmed.to_string()
}

fn function_parameters_for_tool(tool: &ModelToolDefinition) -> serde_json::Value {
    match &tool.format {
        ModelToolFormat::Text => serde_json::json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "Free-form text input for the tool."
                }
            },
            "required": ["input"],
            "additionalProperties": false
        }),
        ModelToolFormat::JsonSchema { schema } => schema.clone(),
    }
}

pub(super) fn anthropic_messages(request: &ModelRequest) -> Vec<serde_json::Value> {
    let mut messages = request
        .messages
        .iter()
        .map(|message| {
            serde_json::json!({
                "role": message.role,
                "content": [
                    {
                        "type": "text",
                        "text": message.content,
                    }
                ],
            })
        })
        .collect::<Vec<_>>();
    if !request.pending_tool_calls.is_empty() {
        messages.push(serde_json::json!({
            "role": "assistant",
            "content": request
                .pending_tool_calls
                .iter()
                .map(anthropic_tool_call_block)
                .collect::<Vec<_>>(),
        }));
    }
    if !request.tool_outputs.is_empty() {
        messages.push(serde_json::json!({
            "role": "user",
            "content": request
                .tool_outputs
                .iter()
                .map(anthropic_tool_result_block)
                .collect::<Vec<_>>(),
        }));
    }
    messages
}

fn anthropic_tool_definition(tool: &ModelToolDefinition) -> serde_json::Value {
    serde_json::json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": function_parameters_for_tool(tool),
    })
}

fn anthropic_tool_call_block(call: &ModelToolCall) -> serde_json::Value {
    let input = serde_json::from_str::<serde_json::Value>(&call.input)
        .unwrap_or_else(|_| serde_json::json!({ "input": call.input }));
    serde_json::json!({
        "type": "tool_use",
        "id": call.call_id,
        "name": call.name,
        "input": input,
    })
}

fn anthropic_tool_result_block(output: &ModelToolOutput) -> serde_json::Value {
    serde_json::json!({
        "type": "tool_result",
        "tool_use_id": output.call_id,
        "content": output.output,
    })
}

pub(super) fn gemini_messages(request: &ModelRequest) -> Vec<serde_json::Value> {
    let mut messages = request
        .messages
        .iter()
        .map(|message| {
            serde_json::json!({
                "role": if message.role == "assistant" { "model" } else { "user" },
                "parts": [
                    {
                        "text": message.content,
                    }
                ],
            })
        })
        .collect::<Vec<_>>();
    if !request.pending_tool_calls.is_empty() {
        messages.push(serde_json::json!({
            "role": "model",
            "parts": request
                .pending_tool_calls
                .iter()
                .map(gemini_tool_call_part)
                .collect::<Vec<_>>(),
        }));
    }
    if !request.tool_outputs.is_empty() {
        messages.push(serde_json::json!({
            "role": "user",
            "parts": request
                .tool_outputs
                .iter()
                .map(gemini_tool_response_part)
                .collect::<Vec<_>>(),
        }));
    }
    messages
}

fn gemini_tool_definition(tool: &ModelToolDefinition) -> serde_json::Value {
    serde_json::json!({
        "name": tool.name,
        "description": tool.description,
        "parameters": function_parameters_for_tool(tool),
    })
}

fn gemini_tool_call_part(call: &ModelToolCall) -> serde_json::Value {
    let args = serde_json::from_str::<serde_json::Value>(&call.input)
        .unwrap_or_else(|_| serde_json::json!({ "input": call.input }));
    serde_json::json!({
        "functionCall": {
            "name": call.name,
            "args": args,
        }
    })
}

fn gemini_tool_response_part(output: &ModelToolOutput) -> serde_json::Value {
    serde_json::json!({
        "functionResponse": {
            "name": output.name,
            "response": {
                "output": output.output,
            }
        }
    })
}

pub(super) fn extract_anthropic_output(
    body: &serde_json::Value,
) -> Result<ModelGeneration, CourierError> {
    let content = body
        .get("content")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| CourierError::ModelBackendResponse("missing `content` array".to_string()))?;

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for item in content {
        match item.get("type").and_then(serde_json::Value::as_str) {
            Some("text") => {
                if let Some(value) = item.get("text").and_then(serde_json::Value::as_str) {
                    text.push_str(value);
                }
            }
            Some("tool_use") => {
                let call_id = item
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        CourierError::ModelBackendResponse(
                            "anthropic tool_use missing `id`".to_string(),
                        )
                    })?;
                let name = item
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        CourierError::ModelBackendResponse(
                            "anthropic tool_use missing `name`".to_string(),
                        )
                    })?;
                let input = item
                    .get("input")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                tool_calls.push(ModelToolCall {
                    call_id: call_id.to_string(),
                    name: name.to_string(),
                    input: serde_json::to_string(&input).map_err(|error| {
                        CourierError::ModelBackendResponse(format!(
                            "failed to serialize anthropic tool input: {error}"
                        ))
                    })?,
                    kind: ModelToolKind::Function,
                });
            }
            _ => {}
        }
    }

    Ok(ModelGeneration::Reply(ModelReply {
        text: if text.is_empty() { None } else { Some(text) },
        backend: "anthropic_messages".to_string(),
        response_id: body
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string),
        tool_calls,
    }))
}

pub(super) fn extract_gemini_output(
    body: &serde_json::Value,
) -> Result<ModelGeneration, CourierError> {
    let candidate = body
        .get("candidates")
        .and_then(serde_json::Value::as_array)
        .and_then(|candidates| candidates.first())
        .ok_or_else(|| CourierError::ModelBackendResponse("missing `candidates[0]`".to_string()))?;
    let parts = candidate
        .get("content")
        .and_then(|content| content.get("parts"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            CourierError::ModelBackendResponse("missing `candidates[0].content.parts`".to_string())
        })?;

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for part in parts {
        if let Some(value) = part.get("text").and_then(serde_json::Value::as_str) {
            text.push_str(value);
        }
        if let Some(function_call) = part
            .get("functionCall")
            .or_else(|| part.get("function_call"))
        {
            let name = function_call
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    CourierError::ModelBackendResponse(
                        "gemini functionCall missing `name`".to_string(),
                    )
                })?;
            let args = function_call
                .get("args")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let call_id = function_call
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
                .unwrap_or_else(next_generated_tool_call_id);
            tool_calls.push(ModelToolCall {
                call_id,
                name: name.to_string(),
                input: serde_json::to_string(&args).map_err(|error| {
                    CourierError::ModelBackendResponse(format!(
                        "failed to serialize gemini tool args: {error}"
                    ))
                })?,
                kind: ModelToolKind::Function,
            });
        }
    }

    Ok(ModelGeneration::Reply(ModelReply {
        text: if text.is_empty() { None } else { Some(text) },
        backend: "google_gemini_generate_content".to_string(),
        response_id: None,
        tool_calls,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parse_openai_streaming_events_collects_text_deltas_and_completed_response() {
        let stream = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello \"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"world\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_123\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"hello world\"}]}]}}\n\n",
            "data: [DONE]\n\n"
        );
        let mut deltas = Vec::new();
        let body = parse_openai_streaming_events(Cursor::new(stream), &mut |event| match event {
            ModelStreamEvent::TextDelta { content } => deltas.push(content),
        })
        .unwrap();

        assert_eq!(deltas, vec!["hello ".to_string(), "world".to_string()]);
        assert_eq!(body["id"], "resp_123");
        let (text, tool_calls) = extract_openai_output(&body).unwrap();
        assert_eq!(text.as_deref(), Some("hello world"));
        assert!(tool_calls.is_empty());
    }

    #[test]
    fn parse_openai_chat_completions_streaming_events_collects_text_and_tool_calls() {
        let stream = concat!(
            "data: {\"id\":\"chatcmpl_123\",\"choices\":[{\"delta\":{\"content\":\"hello \"}}]}\n\n",
            "data: {\"id\":\"chatcmpl_123\",\"choices\":[{\"delta\":{\"content\":\"world\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":\"}}]}}]}\n\n",
            "data: {\"id\":\"chatcmpl_123\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"ping\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let mut deltas = Vec::new();
        let body =
            parse_openai_chat_completions_streaming_events(Cursor::new(stream), &mut |event| {
                match event {
                    ModelStreamEvent::TextDelta { content } => deltas.push(content),
                }
            })
            .unwrap();

        assert_eq!(deltas, vec!["hello ".to_string(), "world".to_string()]);
        assert_eq!(body["id"], "chatcmpl_123");
        let reply = extract_openai_chat_completions_output(&body).unwrap();
        let ModelGeneration::Reply(reply) = reply else {
            panic!("expected reply");
        };
        assert_eq!(reply.text.as_deref(), Some("hello world"));
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].call_id, "call_1");
        assert_eq!(reply.tool_calls[0].name, "lookup");
        assert_eq!(reply.tool_calls[0].input, "{\"q\":\"ping\"}");
    }

    #[test]
    fn parse_anthropic_streaming_events_collects_text_and_tool_use() {
        let stream = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_123\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello \"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"world\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"lookup\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"q\\\":\\\"ping\\\"}\"}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let mut deltas = Vec::new();
        let body =
            parse_anthropic_streaming_events(Cursor::new(stream), &mut |event| match event {
                ModelStreamEvent::TextDelta { content } => deltas.push(content),
            })
            .unwrap();

        assert_eq!(deltas, vec!["hello ".to_string(), "world".to_string()]);
        assert_eq!(body["id"], "msg_123");
        let reply = extract_anthropic_output(&body).unwrap();
        let ModelGeneration::Reply(reply) = reply else {
            panic!("expected reply");
        };
        assert_eq!(reply.text.as_deref(), Some("hello world"));
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].call_id, "toolu_1");
        assert_eq!(reply.tool_calls[0].name, "lookup");
        assert_eq!(reply.tool_calls[0].input, "{\"q\":\"ping\"}");
    }

    #[test]
    fn parse_gemini_streaming_events_collects_text_and_function_call_parts() {
        let stream = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hello \"}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"world\"},{\"functionCall\":{\"name\":\"lookup\",\"args\":{\"q\":\"ping\"}}}]}}]}\n\n"
        );
        let mut deltas = Vec::new();
        let body = parse_gemini_streaming_events(Cursor::new(stream), &mut |event| match event {
            ModelStreamEvent::TextDelta { content } => deltas.push(content),
        })
        .unwrap();

        assert_eq!(deltas, vec!["hello ".to_string(), "world".to_string()]);
        let reply = extract_gemini_output(&body).unwrap();
        let ModelGeneration::Reply(reply) = reply else {
            panic!("expected reply");
        };
        assert_eq!(reply.text.as_deref(), Some("hello world"));
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].name, "lookup");
        assert_eq!(reply.tool_calls[0].input, "{\"q\":\"ping\"}");
    }
}
