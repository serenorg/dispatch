use super::*;
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
};

pub(super) struct AnthropicMessagesBackend;

const DEFAULT_ANTHROPIC_MAX_TOKENS: u32 = 2048;

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

pub(crate) fn anthropic_max_tokens(request: &ModelRequest) -> u32 {
    request
        .context_token_limit
        .unwrap_or(DEFAULT_ANTHROPIC_MAX_TOKENS)
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

pub(crate) fn parse_anthropic_streaming_events<R: BufRead>(
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

pub(crate) fn anthropic_messages(request: &ModelRequest) -> Vec<serde_json::Value> {
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

pub(crate) fn extract_anthropic_output(
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
