use super::{
    ChatTurnResult, CourierError, CourierEvent, CourierSession, HostTurnContext, InstructionKind,
    LoadedParcel, ModelGeneration, ModelStreamEvent, ModelToolOutput, NativeTurnMode,
    ToolInvocation, build_builtin_tool_approval_request, build_local_tool_approval_request,
    build_model_requests, check_tool_approval, codex_backend_state, configured_tool_round_limit,
    denied_tool_run_result, effective_llm_timeout_ms, execute_builtin_tool,
    execute_host_local_tool, handle_native_memory_command, is_codex_backend_id, list_local_tools,
    list_native_builtin_tools, normalize_local_tool_input, resolve_prompt_text,
    select_chat_backend, truncate_tool_output,
};

pub(super) fn execute_host_turn(
    image: &LoadedParcel,
    session: &CourierSession,
    input: &str,
    mode: NativeTurnMode,
    context: HostTurnContext<'_>,
) -> Result<ChatTurnResult, CourierError> {
    let trimmed = input.trim();
    let local_tools = list_local_tools(image);
    let builtin_tools = list_native_builtin_tools(image);
    let mut events = Vec::new();

    if matches!(mode, NativeTurnMode::Chat) && trimmed.eq_ignore_ascii_case("/prompt") {
        return Ok(ChatTurnResult {
            reply: resolve_prompt_text(image)?,
            events: Vec::new(),
            streamed_reply: false,
            backend_state: session.backend_state.clone(),
        });
    }

    if matches!(mode, NativeTurnMode::Chat) && trimmed.eq_ignore_ascii_case("/tools") {
        if local_tools.is_empty() {
            return Ok(ChatTurnResult {
                reply: "No local tools are declared for this image.".to_string(),
                events: Vec::new(),
                streamed_reply: false,
                backend_state: session.backend_state.clone(),
            });
        }

        let names = local_tools
            .iter()
            .map(|tool| tool.alias.clone())
            .collect::<Vec<_>>()
            .join(", ");
        return Ok(ChatTurnResult {
            reply: format!("Declared local tools: {names}"),
            events: Vec::new(),
            streamed_reply: false,
            backend_state: session.backend_state.clone(),
        });
    }

    if matches!(mode, NativeTurnMode::Chat) && trimmed.eq_ignore_ascii_case("/help") {
        return Ok(ChatTurnResult {
            reply: format!(
                "{} chat is a reference backend. Available commands: /prompt, /tools, /memory, /help.",
                context.host_label
            ),
            events: Vec::new(),
            streamed_reply: false,
            backend_state: session.backend_state.clone(),
        });
    }

    if matches!(mode, NativeTurnMode::Chat) && trimmed.starts_with("/memory") {
        return Ok(ChatTurnResult {
            reply: handle_native_memory_command(session, trimmed)?,
            events: Vec::new(),
            streamed_reply: false,
            backend_state: session.backend_state.clone(),
        });
    }

    let requests = build_model_requests(
        image,
        &session.history,
        &local_tools,
        context.run_deadline,
        session.backend_state.as_deref(),
    )?;
    if let Some(mut request) = requests.first().cloned() {
        let mut remaining_requests = requests.into_iter().skip(1).collect::<Vec<_>>();
        for secret in &image.config.secrets {
            if secret.required && std::env::var(&secret.name).is_err() {
                return Err(CourierError::MissingSecret {
                    name: secret.name.clone(),
                });
            }
        }
        const DEFAULT_MAX_TOOL_ROUNDS: u32 = 8;
        let max_tool_rounds =
            configured_tool_round_limit(&image.config.limits).unwrap_or(DEFAULT_MAX_TOOL_ROUNDS);
        let mut rounds = 0u32;
        let mut executed_tool_calls = 0u32;
        let mut backend = select_chat_backend(context.chat_backend_override, &request);
        let mut candidate_locked = false;
        let mut streamed_reply = false;
        loop {
            if rounds >= max_tool_rounds {
                events.push(CourierEvent::BackendFallback {
                    backend: backend.id().to_string(),
                    error: format!(
                        "tool call loop reached {} rounds without a final reply; falling back to local reference reply",
                        max_tool_rounds
                    ),
                });
                break;
            }
            rounds += 1;

            request.llm_timeout_ms =
                effective_llm_timeout_ms(&image.config.timeouts, context.run_deadline)?;
            let mut streamed_deltas = Vec::new();
            let reply = match backend.generate_with_events(&request, &mut |event| match event {
                ModelStreamEvent::TextDelta { content } => streamed_deltas.push(content),
            }) {
                Ok(ModelGeneration::Reply(reply)) => reply,
                Ok(ModelGeneration::NotConfigured {
                    backend: not_configured_backend,
                    reason,
                }) => {
                    if !candidate_locked
                        && let Some(next_request) = remaining_requests.first().cloned()
                    {
                        events.push(CourierEvent::BackendFallback {
                            backend: not_configured_backend,
                            error: format!(
                                "{reason}; trying fallback model `{}`",
                                next_request.model
                            ),
                        });
                        request = next_request;
                        backend = select_chat_backend(context.chat_backend_override, &request);
                        remaining_requests.remove(0);
                        continue;
                    }
                    events.push(CourierEvent::BackendFallback {
                        backend: not_configured_backend,
                        error: reason,
                    });
                    break;
                }
                Err(error) => {
                    if !candidate_locked
                        && let Some(next_request) = remaining_requests.first().cloned()
                    {
                        events.push(CourierEvent::BackendFallback {
                            backend: backend.id().to_string(),
                            error: format!(
                                "{error}; trying fallback model `{}`",
                                next_request.model
                            ),
                        });
                        request = next_request;
                        backend = select_chat_backend(context.chat_backend_override, &request);
                        remaining_requests.remove(0);
                        continue;
                    }
                    events.push(CourierEvent::BackendFallback {
                        backend: backend.id().to_string(),
                        error: error.to_string(),
                    });
                    break;
                }
            };

            if reply.tool_calls.is_empty() && !streamed_deltas.is_empty() {
                streamed_reply = true;
                events.extend(
                    streamed_deltas
                        .into_iter()
                        .map(|content| CourierEvent::TextDelta { content }),
                );
            }

            if !reply.tool_calls.is_empty() {
                candidate_locked = true;
                if let Some(limit) = request.tool_call_limit {
                    let attempted =
                        executed_tool_calls.saturating_add(reply.tool_calls.len() as u32);
                    if attempted > limit {
                        return Err(CourierError::ToolCallLimitExceeded { limit, attempted });
                    }
                }
                let reply_tool_calls = reply.tool_calls.clone();
                let mut tool_outputs = Vec::with_capacity(reply.tool_calls.len());
                for tool_call in reply.tool_calls {
                    let invocation = ToolInvocation {
                        name: tool_call.name.clone(),
                        input: Some(tool_call.input.clone()),
                    };
                    let tool_result = if let Some(tool) =
                        local_tools.iter().find(|t| t.matches_name(&tool_call.name))
                    {
                        let normalized_input =
                            normalize_local_tool_input(tool, tool_call.input.as_str())?;
                        events.push(CourierEvent::ToolCallStarted {
                            invocation,
                            command: tool.command().to_string(),
                            args: tool.args().to_vec(),
                        });
                        if let Some(request) =
                            build_local_tool_approval_request(tool, Some(normalized_input.as_ref()))
                        {
                            if check_tool_approval(&request)? {
                                execute_host_local_tool(
                                    image,
                                    tool,
                                    Some(normalized_input.as_ref()),
                                    context.tool_runner,
                                    context.run_deadline,
                                )?
                            } else {
                                denied_tool_run_result(&request)
                            }
                        } else {
                            execute_host_local_tool(
                                image,
                                tool,
                                Some(normalized_input.as_ref()),
                                context.tool_runner,
                                context.run_deadline,
                            )?
                        }
                    } else if let Some(tool) = builtin_tools
                        .iter()
                        .find(|tool| tool.capability == tool_call.name)
                    {
                        events.push(CourierEvent::ToolCallStarted {
                            invocation,
                            command: "dispatch-builtin".to_string(),
                            args: vec![tool.capability.clone()],
                        });
                        if let Some(request) = build_builtin_tool_approval_request(
                            tool,
                            Some(tool_call.input.as_str()),
                        ) {
                            if check_tool_approval(&request)? {
                                execute_builtin_tool(session, &tool.capability, &tool_call.input)?
                            } else {
                                denied_tool_run_result(&request)
                            }
                        } else {
                            execute_builtin_tool(session, &tool.capability, &tool_call.input)?
                        }
                    } else {
                        return Err(CourierError::UnknownLocalTool {
                            tool: tool_call.name.clone(),
                        });
                    };
                    let combined_output = if tool_result.exit_code == 0 {
                        if tool_result.stderr.trim().is_empty() {
                            tool_result.stdout.clone()
                        } else if tool_result.stdout.trim().is_empty() {
                            tool_result.stderr.clone()
                        } else {
                            format!(
                                "stdout:\n{}\n\nstderr:\n{}",
                                tool_result.stdout, tool_result.stderr
                            )
                        }
                    } else {
                        format!(
                            "tool_failed exit_code={}\nstdout:\n{}\n\nstderr:\n{}",
                            tool_result.exit_code, tool_result.stdout, tool_result.stderr
                        )
                    };
                    let combined_output =
                        truncate_tool_output(combined_output, request.tool_output_limit);
                    events.push(CourierEvent::ToolCallFinished {
                        result: tool_result,
                    });
                    tool_outputs.push(ModelToolOutput {
                        call_id: tool_call.call_id,
                        name: tool_call.name,
                        output: combined_output,
                        kind: tool_call.kind,
                    });
                    executed_tool_calls = executed_tool_calls.saturating_add(1);
                }

                if backend.supports_previous_response_id() {
                    request.messages.clear();
                    request.pending_tool_calls.clear();
                    request.tool_outputs = tool_outputs;
                    request.previous_response_id = reply.response_id;
                } else {
                    request.pending_tool_calls = reply_tool_calls;
                    request.tool_outputs = tool_outputs;
                    request.previous_response_id = None;
                }
                continue;
            }

            if let Some(text) = reply.text {
                return Ok(ChatTurnResult {
                    reply: text,
                    events,
                    streamed_reply,
                    backend_state: if is_codex_backend_id(backend.id()) {
                        reply.response_id.as_deref().map(codex_backend_state)
                    } else {
                        None
                    },
                });
            }
            break;
        }
    }

    let prompt_sections = image
        .config
        .instructions
        .iter()
        .filter(|instruction| {
            matches!(
                instruction.kind,
                InstructionKind::Soul
                    | InstructionKind::Identity
                    | InstructionKind::Skill
                    | InstructionKind::Agents
                    | InstructionKind::User
                    | InstructionKind::Tools
                    | InstructionKind::Memory
                    | InstructionKind::Heartbeat
            )
        })
        .count()
        + usize::from(!image.config.inline_prompts.is_empty());
    let tool_count = local_tools.len() + builtin_tools.len();
    let prior_messages = session.history.len().saturating_sub(1);

    Ok(ChatTurnResult {
        reply: format!(
            "{} {} reference reply for turn {}. Loaded {} prompt section(s) and {} tool(s). Prior messages in session: {}. Input: {}",
            context.host_label,
            native_turn_mode_name(mode),
            session.turn_count,
            prompt_sections,
            tool_count,
            prior_messages,
            input
        ),
        events,
        streamed_reply: false,
        backend_state: None,
    })
}

fn native_turn_mode_name(mode: NativeTurnMode) -> &'static str {
    match mode {
        NativeTurnMode::Chat => "chat",
        NativeTurnMode::Job => "job",
        NativeTurnMode::Heartbeat => "heartbeat",
    }
}

pub(super) fn format_job_payload(payload: &str) -> String {
    format!("Job payload:\n{payload}")
}

pub(super) fn format_heartbeat_payload(payload: Option<&str>) -> String {
    match payload {
        Some(payload) if !payload.trim().is_empty() => format!("Heartbeat payload:\n{payload}"),
        _ => "Heartbeat tick".to_string(),
    }
}
