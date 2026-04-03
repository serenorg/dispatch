wit_bindgen::generate!({
    path: "../dispatch-wasm-abi/wit",
    world: "courier-guest",
});

use dispatch::courier::host;
use exports::dispatch::courier::guest::{
    BackendFallback, ConversationMessage, Guest, GuestEvent, GuestSession, Operation,
    ParcelContext, TurnResult,
};

struct Component;

impl Guest for Component {
    fn open_session(parcel: ParcelContext) -> Result<Option<String>, String> {
        Ok(Some(format!("opened:{}:{}", parcel.parcel_digest, 0)))
    }

    fn handle_operation(
        parcel: ParcelContext,
        session: GuestSession,
        operation: Operation,
    ) -> Result<TurnResult, String> {
        let next_turn = session.turn_count + 1;
        let next_state = format!("opened:{}:{next_turn}", parcel.parcel_digest);

        match operation {
            Operation::Chat(input) => handle_chat(parcel, session, input, next_state),
            Operation::Job(payload) => Ok(TurnResult {
                backend_state: Some(next_state),
                events: vec![message(
                    "assistant",
                    format!("job accepted: {}", payload.trim()),
                )],
            }),
            Operation::Heartbeat(payload) => Ok(TurnResult {
                backend_state: Some(next_state),
                events: vec![GuestEvent::TextDelta(match payload {
                    Some(payload) if !payload.is_empty() => format!("heartbeat:{payload}"),
                    _ => "heartbeat".to_string(),
                })],
            }),
        }
    }
}

fn handle_chat(
    parcel: ParcelContext,
    session: GuestSession,
    input: String,
    next_state: String,
) -> Result<TurnResult, String> {
    let trimmed = input.trim();

    if let Some(alias) = trimmed
        .strip_prefix("tool ")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let result = host::invoke_tool(&host::ToolInvocation {
            name: alias.to_string(),
            input: Some(format!(
                r#"{{"source":"reference-guest","input":"{trimmed}"}}"#
            )),
        })
        .map_err(|error| format!("tool import failed: {error}"))?;
        let content = if result.exit_code == 0 {
            format!("tool {} ok: {}", result.tool, result.stdout.trim())
        } else {
            format!(
                "tool {} failed (exit {}): {}",
                result.tool,
                result.exit_code,
                result.stderr.trim()
            )
        };
        return Ok(TurnResult {
            backend_state: Some(next_state),
            events: vec![message("assistant", content)],
        });
    }

    if trimmed == "model" {
        let mut messages = session.history;
        messages.push(ConversationMessage {
            role: "user".to_string(),
            content: input,
        });
        let tools = parcel
            .local_tools
            .iter()
            .cloned()
            .map(|tool| host::ModelTool {
                name: tool.alias,
                description: tool
                    .description
                    .unwrap_or_else(|| "Dispatch local tool".to_string()),
                kind: if tool.input_schema_json.is_some() {
                    host::ModelToolKind::Function
                } else {
                    host::ModelToolKind::Custom
                },
                input_schema_json: tool.input_schema_json,
            })
            .collect::<Vec<_>>();

        let mut previous_response_id = None;
        let mut tool_outputs = Vec::new();
        for _round in 0..8 {
            let reply = host::model_complete(&host::ModelRequest {
                model: parcel.primary_model.clone(),
                instructions: parcel.prompt.clone(),
                messages: if previous_response_id.is_some() {
                    Vec::new()
                } else {
                    messages.clone()
                },
                tools: tools.clone(),
                tool_outputs,
                previous_response_id: previous_response_id.clone(),
            })
            .map_err(|error| format!("model import failed: {error}"))?;

            if !reply.tool_calls.is_empty() {
                tool_outputs = reply
                    .tool_calls
                    .into_iter()
                    .map(|call| {
                        let result = host::invoke_tool(&host::ToolInvocation {
                            name: call.name,
                            input: Some(call.input),
                        })
                        .map_err(|error| format!("tool import failed: {error}"))?;
                        let output = if result.exit_code == 0 {
                            if result.stderr.trim().is_empty() {
                                result.stdout
                            } else if result.stdout.trim().is_empty() {
                                result.stderr
                            } else {
                                format!("stdout:\n{}\n\nstderr:\n{}", result.stdout, result.stderr)
                            }
                        } else {
                            format!(
                                "tool_failed exit_code={}\nstdout:\n{}\n\nstderr:\n{}",
                                result.exit_code, result.stdout, result.stderr
                            )
                        };
                        Ok(host::ModelToolOutput {
                            call_id: call.call_id,
                            output,
                            kind: call.kind,
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                previous_response_id = reply.response_id;
                continue;
            }

            let mut events = Vec::new();
            if reply.text.is_none() {
                events.push(GuestEvent::BackendFallback(BackendFallback {
                    backend: reply.backend.clone(),
                    error: "model returned no text".to_string(),
                }));
            }
            events.push(message(
                "assistant",
                reply
                    .text
                    .unwrap_or_else(|| "model returned no text".to_string()),
            ));
            return Ok(TurnResult {
                backend_state: Some(next_state),
                events,
            });
        }

        return Ok(TurnResult {
            backend_state: Some(next_state),
            events: vec![
                GuestEvent::BackendFallback(BackendFallback {
                    backend: "reference-guest".to_string(),
                    error: "tool call loop reached 8 rounds without a final reply".to_string(),
                }),
                message("assistant", "model returned no final reply".to_string()),
            ],
        });
    }

    Ok(TurnResult {
        backend_state: Some(next_state),
        events: vec![message(
            "assistant",
            format!("reference guest heard: {trimmed}"),
        )],
    })
}

fn message(role: &str, content: String) -> GuestEvent {
    GuestEvent::Message(ConversationMessage {
        role: role.to_string(),
        content,
    })
}

export!(Component);
