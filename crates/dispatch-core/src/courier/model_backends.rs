use super::*;
use std::{io::BufRead, time::Duration};

mod anthropic;
mod codex;
mod gemini;
mod openai;

use anthropic::AnthropicMessagesBackend;
#[cfg(test)]
pub(super) use anthropic::parse_anthropic_streaming_events;
#[cfg(test)]
pub(super) use anthropic::{anthropic_max_tokens, anthropic_messages, extract_anthropic_output};
pub(crate) use codex::CodexAppServerBackend;
#[cfg(test)]
pub(crate) use codex::clear_test_codex_binary_override;
use gemini::GeminiGenerateContentBackend;
#[cfg(test)]
pub(super) use gemini::parse_gemini_streaming_events;
#[cfg(test)]
pub(super) use gemini::{extract_gemini_output, gemini_messages};
use openai::{OpenAiChatCompletionsBackend, OpenAiResponsesBackend};
#[cfg(test)]
pub(super) use openai::{
    extract_openai_chat_completions_output, extract_openai_output,
    openai_chat_completions_messages, openai_tool_definition,
    parse_openai_chat_completions_streaming_events, parse_openai_streaming_events,
};

fn request_timeout(request: &ModelRequest) -> Option<Duration> {
    request.llm_timeout_ms.map(Duration::from_millis)
}

fn generate_with_noop_events(
    backend: &dyn ChatModelBackend,
    request: &ModelRequest,
) -> Result<ModelGeneration, CourierError> {
    backend.generate_with_events(request, &mut |_| {})
}

pub(super) const CODEX_BACKEND_ID: &str = "codex_app_server";
const CODEX_BACKEND_STATE_PREFIX: &str = "codex_thread:";

pub(super) fn is_codex_provider(provider: &str) -> bool {
    matches!(
        provider.to_ascii_lowercase().as_str(),
        "codex" | "codex_app_server" | "codex-app-server"
    )
}

pub(super) fn is_codex_backend_id(backend_id: &str) -> bool {
    backend_id == CODEX_BACKEND_ID
}

pub(super) fn codex_backend_state(thread_id: &str) -> String {
    format!("{CODEX_BACKEND_STATE_PREFIX}{thread_id}")
}

pub(super) fn codex_thread_id_from_backend_state(state: &str) -> Option<String> {
    state
        .strip_prefix(CODEX_BACKEND_STATE_PREFIX)
        .map(ToString::to_string)
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
        "codex" | "codex_app_server" | "codex-app-server" => Arc::new(CodexAppServerBackend),
        "anthropic" => Arc::new(AnthropicMessagesBackend),
        "gemini" | "google" | "google_gemini" => Arc::new(GeminiGenerateContentBackend),
        "openai_compatible" | "openrouter" | "together" | "fireworks" | "litellm" | "vllm"
        | "lm_studio" => Arc::new(OpenAiChatCompletionsBackend),
        _ => Arc::new(OpenAiResponsesBackend),
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
