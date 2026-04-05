use super::*;
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
};

pub(super) struct OpenAiResponsesBackend;
pub(super) struct OpenAiChatCompletionsBackend;

impl ChatModelBackend for OpenAiResponsesBackend {
    fn id(&self) -> &str {
        "openai_responses"
    }

    fn supports_previous_response_id(&self) -> bool {
        true
    }

    fn generate(&self, request: &ModelRequest) -> Result<ModelGeneration, CourierError> {
        generate_with_noop_events(self, request)
    }

    fn generate_with_events(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent),
    ) -> Result<ModelGeneration, CourierError> {
        let api_key = match model_api_key("LLM_API_KEY", "OPENAI_API_KEY") {
            Some(value) => value,
            None => {
                return Ok(ModelGeneration::NotConfigured {
                    backend: self.id().to_string(),
                    reason: "missing LLM_API_KEY or OPENAI_API_KEY".to_string(),
                });
            }
        };

        let base_url = model_base_url("LLM_BASE_URL", "OPENAI_BASE_URL", "https://api.openai.com");
        let url = format!("{}/v1/responses", base_url.trim_end_matches('/'));
        let payload = openai_responses_payload(request, true);

        let response = ureq::post(&url)
            .config()
            .http_status_as_error(false)
            .timeout_global(request_timeout(request))
            .build()
            .header("authorization", &format!("Bearer {api_key}"))
            .header("content-type", "application/json")
            .send_json(payload)
            .map_err(|error| CourierError::ModelBackendRequest(error.to_string()))?;

        read_openai_streaming_response(response, self.id(), on_event)
    }
}

impl ChatModelBackend for OpenAiChatCompletionsBackend {
    fn id(&self) -> &str {
        "openai_compatible_chat_completions"
    }

    fn generate(&self, request: &ModelRequest) -> Result<ModelGeneration, CourierError> {
        generate_with_noop_events(self, request)
    }

    fn generate_with_events(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent),
    ) -> Result<ModelGeneration, CourierError> {
        let api_key = match model_api_key("LLM_API_KEY", "OPENAI_API_KEY") {
            Some(value) => value,
            None => {
                return Ok(ModelGeneration::NotConfigured {
                    backend: self.id().to_string(),
                    reason: "missing LLM_API_KEY or OPENAI_API_KEY".to_string(),
                });
            }
        };

        let base_url = model_base_url("LLM_BASE_URL", "OPENAI_BASE_URL", "https://api.openai.com");
        let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));
        let payload = serde_json::json!({
            "model": request.model,
            "messages": openai_chat_completions_messages(request),
            "tools": request
                .tools
                .iter()
                .map(openai_chat_completions_tool_definition)
                .collect::<Vec<_>>(),
            "tool_choice": "auto",
            "stream": true,
        });

        let response = ureq::post(&url)
            .config()
            .http_status_as_error(false)
            .timeout_global(request_timeout(request))
            .build()
            .header("authorization", &format!("Bearer {api_key}"))
            .header("content-type", "application/json")
            .send_json(payload)
            .map_err(|error| CourierError::ModelBackendRequest(error.to_string()))?;

        read_openai_chat_completions_streaming_response(response, self.id(), on_event)
    }
}

fn openai_input_message(message: &ConversationMessage) -> serde_json::Value {
    serde_json::json!({
        "role": message.role,
        "content": [
            {
                "type": "input_text",
                "text": message.content,
            }
        ],
    })
}

fn openai_responses_payload(request: &ModelRequest, stream: bool) -> serde_json::Value {
    serde_json::json!({
        "model": request.model,
        "instructions": request.instructions,
        "input": if request.previous_response_id.is_some() {
            request
                .tool_outputs
                .iter()
                .map(openai_tool_output_item)
                .collect::<Vec<_>>()
        } else {
            request
                .messages
                .iter()
                .map(openai_input_message)
                .collect::<Vec<_>>()
        },
        "previous_response_id": request.previous_response_id,
        "parallel_tool_calls": false,
        "stream": stream,
        "tools": request
            .tools
            .iter()
            .map(openai_tool_definition)
            .collect::<Vec<_>>(),
    })
}

fn openai_chat_completions_message(message: &ConversationMessage) -> serde_json::Value {
    serde_json::json!({
        "role": message.role,
        "content": message.content,
    })
}

pub(crate) fn openai_chat_completions_messages(request: &ModelRequest) -> Vec<serde_json::Value> {
    let mut messages = Vec::with_capacity(request.messages.len() + 1);
    if !request.instructions.trim().is_empty() {
        messages.push(serde_json::json!({
            "role": "system",
            "content": request.instructions,
        }));
    }
    messages.extend(request.messages.iter().map(openai_chat_completions_message));
    if !request.pending_tool_calls.is_empty() {
        messages.push(serde_json::json!({
            "role": "assistant",
            "tool_calls": request
                .pending_tool_calls
                .iter()
                .map(openai_chat_completions_tool_call)
                .collect::<Vec<_>>(),
        }));
    }
    messages.extend(
        request
            .tool_outputs
            .iter()
            .map(openai_chat_completions_tool_output_message),
    );
    messages
}

pub(crate) fn openai_tool_definition(tool: &ModelToolDefinition) -> serde_json::Value {
    match &tool.format {
        ModelToolFormat::Text => serde_json::json!({
            "type": "custom",
            "name": tool.name,
            "description": tool.description,
            "format": { "type": "text" },
        }),
        ModelToolFormat::JsonSchema { schema } => serde_json::json!({
            "type": "function",
            "name": tool.name,
            "description": tool.description,
            "parameters": schema,
        }),
    }
}

fn openai_chat_completions_tool_definition(tool: &ModelToolDefinition) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": function_parameters_for_tool(tool),
        },
    })
}

fn openai_tool_output_item(output: &ModelToolOutput) -> serde_json::Value {
    match output.kind {
        ModelToolKind::Custom => serde_json::json!({
            "type": "custom_tool_call_output",
            "call_id": output.call_id,
            "output": output.output,
        }),
        ModelToolKind::Function => serde_json::json!({
            "type": "function_call_output",
            "call_id": output.call_id,
            "output": output.output,
        }),
    }
}

fn openai_chat_completions_tool_call(call: &ModelToolCall) -> serde_json::Value {
    serde_json::json!({
        "id": call.call_id,
        "type": "function",
        "function": {
            "name": call.name,
            "arguments": call.input,
        },
    })
}

fn openai_chat_completions_tool_output_message(output: &ModelToolOutput) -> serde_json::Value {
    serde_json::json!({
        "role": "tool",
        "tool_call_id": output.call_id,
        "content": output.output,
    })
}

pub(crate) fn read_openai_streaming_response(
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
    let body = parse_openai_streaming_events(reader, on_event)?;
    let (text, tool_calls) = extract_openai_output(&body)?;

    Ok(ModelGeneration::Reply(ModelReply {
        text,
        backend: backend.to_string(),
        response_id: body
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string),
        tool_calls,
    }))
}

pub(crate) fn parse_openai_streaming_events<R: BufRead>(
    mut reader: R,
    on_event: &mut dyn FnMut(ModelStreamEvent),
) -> Result<serde_json::Value, CourierError> {
    let mut event_data = String::new();
    let mut line = String::new();
    let mut completed_response = None;

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
            process_openai_stream_event(&event_data, on_event, &mut completed_response)?;
            event_data.clear();
            continue;
        }

        if let Some(data) = trimmed.strip_prefix("data:") {
            if !event_data.is_empty() {
                event_data.push('\n');
            }
            event_data.push_str(data.trim_start());
        }
    }

    process_openai_stream_event(&event_data, on_event, &mut completed_response)?;
    let body = completed_response.ok_or_else(|| {
        CourierError::ModelBackendResponse(
            "streaming response ended without a `response.completed` event".to_string(),
        )
    })?;
    Ok(body)
}

fn process_openai_stream_event(
    event_data: &str,
    on_event: &mut dyn FnMut(ModelStreamEvent),
    completed_response: &mut Option<serde_json::Value>,
) -> Result<(), CourierError> {
    let data = event_data.trim();
    if data.is_empty() || data == "[DONE]" {
        return Ok(());
    }

    let value = serde_json::from_str::<serde_json::Value>(data)
        .map_err(|error| CourierError::ModelBackendResponse(error.to_string()))?;
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("response.output_text.delta") => {
            if let Some(delta) = value.get("delta").and_then(serde_json::Value::as_str) {
                on_event(ModelStreamEvent::TextDelta {
                    content: delta.to_string(),
                });
            }
        }
        Some("response.failed") => {
            let message = value
                .pointer("/response/error/message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("streaming response failed");
            return Err(CourierError::ModelBackendRequest(message.to_string()));
        }
        Some("response.completed") => {
            if let Some(response) = value.get("response") {
                *completed_response = Some(response.clone());
            }
        }
        _ => {}
    }

    Ok(())
}

pub(crate) fn read_openai_chat_completions_streaming_response(
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
    let body = parse_openai_chat_completions_streaming_events(reader, on_event)?;
    extract_openai_chat_completions_output(&body)
}

pub(crate) fn parse_openai_chat_completions_streaming_events<R: BufRead>(
    reader: R,
    on_event: &mut dyn FnMut(ModelStreamEvent),
) -> Result<serde_json::Value, CourierError> {
    #[derive(Default)]
    struct ToolCallAccumulator {
        id: Option<String>,
        kind: Option<String>,
        function_name: Option<String>,
        function_arguments: String,
        custom_name: Option<String>,
        custom_input: String,
    }

    let mut response_id = None;
    let mut assistant_text = String::new();
    let mut tool_calls = BTreeMap::<usize, ToolCallAccumulator>::new();

    for event_data in parse_sse_data_events(reader)? {
        if event_data == "[DONE]" {
            break;
        }
        let value = serde_json::from_str::<serde_json::Value>(&event_data)
            .map_err(|error| CourierError::ModelBackendResponse(error.to_string()))?;
        if response_id.is_none() {
            response_id = value
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string);
        }

        let Some(choice) = value
            .get("choices")
            .and_then(serde_json::Value::as_array)
            .and_then(|choices| choices.first())
        else {
            continue;
        };
        let Some(delta) = choice.get("delta") else {
            continue;
        };

        match delta.get("content") {
            Some(serde_json::Value::String(content)) => {
                assistant_text.push_str(content);
                on_event(ModelStreamEvent::TextDelta {
                    content: content.clone(),
                });
            }
            Some(serde_json::Value::Array(parts)) => {
                for part in parts {
                    if let Some(text) = part.get("text").and_then(serde_json::Value::as_str) {
                        assistant_text.push_str(text);
                        on_event(ModelStreamEvent::TextDelta {
                            content: text.to_string(),
                        });
                    }
                }
            }
            _ => {}
        }

        if let Some(delta_calls) = delta
            .get("tool_calls")
            .and_then(serde_json::Value::as_array)
        {
            for delta_call in delta_calls {
                let index = delta_call
                    .get("index")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as usize;
                let entry = tool_calls.entry(index).or_default();
                if let Some(id) = delta_call.get("id").and_then(serde_json::Value::as_str) {
                    entry.id = Some(id.to_string());
                }
                if let Some(kind) = delta_call.get("type").and_then(serde_json::Value::as_str) {
                    entry.kind = Some(kind.to_string());
                }
                if let Some(function) = delta_call.get("function") {
                    if let Some(name) = function.get("name").and_then(serde_json::Value::as_str) {
                        entry.function_name = Some(name.to_string());
                    }
                    if let Some(arguments) = function
                        .get("arguments")
                        .and_then(serde_json::Value::as_str)
                    {
                        entry.function_arguments.push_str(arguments);
                    }
                }
                if let Some(custom) = delta_call.get("custom") {
                    if let Some(name) = custom.get("name").and_then(serde_json::Value::as_str) {
                        entry.custom_name = Some(name.to_string());
                    }
                    if let Some(input) = custom.get("input").and_then(serde_json::Value::as_str) {
                        entry.custom_input.push_str(input);
                    }
                }
            }
        }
    }

    let tool_calls = tool_calls
        .into_values()
        .map(|call| {
            let kind = call.kind.unwrap_or_else(|| "function".to_string());
            match kind.as_str() {
                "function" => serde_json::json!({
                    "id": call.id,
                    "type": "function",
                    "function": {
                        "name": call.function_name,
                        "arguments": call.function_arguments,
                    }
                }),
                "custom" => serde_json::json!({
                    "id": call.id,
                    "type": "custom",
                    "custom": {
                        "name": call.custom_name,
                        "input": call.custom_input,
                    }
                }),
                other => serde_json::json!({
                    "id": call.id,
                    "type": other,
                }),
            }
        })
        .collect::<Vec<_>>();

    Ok(serde_json::json!({
        "id": response_id,
        "choices": [{
            "message": {
                "content": if assistant_text.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::String(assistant_text)
                },
                "tool_calls": tool_calls,
            }
        }]
    }))
}

pub(crate) fn extract_openai_output(
    body: &serde_json::Value,
) -> Result<(Option<String>, Vec<ModelToolCall>), CourierError> {
    let outputs = body
        .get("output")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| CourierError::ModelBackendResponse("missing `output` array".to_string()))?;

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for output in outputs {
        match output.get("type").and_then(serde_json::Value::as_str) {
            Some("custom_tool_call") => {
                tool_calls.push(parse_openai_tool_call(output, ModelToolKind::Custom)?);
                continue;
            }
            Some("function_call") => {
                tool_calls.push(parse_openai_tool_call(output, ModelToolKind::Function)?);
                continue;
            }
            _ => {}
        }

        let Some(content) = output.get("content").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for item in content {
            if item.get("type").and_then(serde_json::Value::as_str) == Some("output_text")
                && let Some(value) = item.get("text").and_then(serde_json::Value::as_str)
            {
                text.push_str(value);
            }
        }
    }

    if !tool_calls.is_empty() {
        return Ok((None, tool_calls));
    }

    if text.is_empty() {
        return Err(CourierError::ModelBackendResponse(
            "response did not contain `output_text` content".to_string(),
        ));
    }

    Ok((Some(text), tool_calls))
}

fn parse_openai_tool_call(
    output: &serde_json::Value,
    kind: ModelToolKind,
) -> Result<ModelToolCall, CourierError> {
    let output_type = output
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("tool_call");
    let call_id = output
        .get("call_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            CourierError::ModelBackendResponse(format!("{output_type} missing `call_id`"))
        })?;
    let name = output
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            CourierError::ModelBackendResponse(format!("{output_type} missing `name`"))
        })?;
    let input_field = match kind {
        ModelToolKind::Custom => "input",
        ModelToolKind::Function => "arguments",
    };
    let input = output
        .get(input_field)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            CourierError::ModelBackendResponse(format!("{output_type} missing `{input_field}`"))
        })?;

    Ok(ModelToolCall {
        call_id: call_id.to_string(),
        name: name.to_string(),
        input: input.to_string(),
        kind,
    })
}

pub(crate) fn extract_openai_chat_completions_output(
    body: &serde_json::Value,
) -> Result<ModelGeneration, CourierError> {
    let choice = body
        .get("choices")
        .and_then(serde_json::Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| CourierError::ModelBackendResponse("missing `choices[0]`".to_string()))?;
    let message = choice.get("message").ok_or_else(|| {
        CourierError::ModelBackendResponse("missing `choices[0].message`".to_string())
    })?;

    let text = match message.get("content") {
        Some(serde_json::Value::String(text)) => Some(text.clone()),
        Some(serde_json::Value::Array(parts)) => {
            let text = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
                .collect::<String>();
            if text.is_empty() { None } else { Some(text) }
        }
        _ => None,
    };

    let tool_calls = message
        .get("tool_calls")
        .and_then(serde_json::Value::as_array)
        .map(|tool_calls| {
            tool_calls
                .iter()
                .map(parse_openai_chat_completions_tool_call)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();

    Ok(ModelGeneration::Reply(ModelReply {
        text,
        backend: "openai_compatible_chat_completions".to_string(),
        response_id: None,
        tool_calls,
    }))
}

fn parse_openai_chat_completions_tool_call(
    value: &serde_json::Value,
) -> Result<ModelToolCall, CourierError> {
    let call_id = value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| CourierError::ModelBackendResponse("tool call missing `id`".to_string()))?;
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("function") => {
            let function = value.get("function").ok_or_else(|| {
                CourierError::ModelBackendResponse(
                    "function tool call missing `function`".to_string(),
                )
            })?;
            let name = function
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    CourierError::ModelBackendResponse(
                        "function tool call missing `function.name`".to_string(),
                    )
                })?;
            let arguments = function
                .get("arguments")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    CourierError::ModelBackendResponse(
                        "function tool call missing `function.arguments`".to_string(),
                    )
                })?;
            Ok(ModelToolCall {
                call_id: call_id.to_string(),
                name: name.to_string(),
                input: arguments.to_string(),
                kind: ModelToolKind::Function,
            })
        }
        Some("custom") => {
            let custom = value.get("custom").ok_or_else(|| {
                CourierError::ModelBackendResponse("custom tool call missing `custom`".to_string())
            })?;
            let name = custom
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    CourierError::ModelBackendResponse(
                        "custom tool call missing `custom.name`".to_string(),
                    )
                })?;
            let input = custom
                .get("input")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    CourierError::ModelBackendResponse(
                        "custom tool call missing `custom.input`".to_string(),
                    )
                })?;
            Ok(ModelToolCall {
                call_id: call_id.to_string(),
                name: name.to_string(),
                input: input.to_string(),
                kind: ModelToolKind::Custom,
            })
        }
        Some(other) => Err(CourierError::ModelBackendResponse(format!(
            "unsupported chat completion tool call type `{other}`"
        ))),
        None => Err(CourierError::ModelBackendResponse(
            "tool call missing `type`".to_string(),
        )),
    }
}
