use super::*;
use std::io::BufReader;

pub(super) struct GeminiGenerateContentBackend;

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

pub(crate) fn parse_gemini_streaming_events<R: BufRead>(
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

pub(crate) fn gemini_messages(request: &ModelRequest) -> Vec<serde_json::Value> {
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

pub(crate) fn extract_gemini_output(
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
