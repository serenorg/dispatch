use super::*;

#[test]
fn openai_tool_definition_uses_function_shape_for_schema_tools() {
    let value = openai_tool_definition(&ModelToolDefinition {
        name: "demo".to_string(),
        description: "Search by id".to_string(),
        format: ModelToolFormat::JsonSchema {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" }
                }
            }),
        },
    });

    assert_eq!(value["type"], "function");
    assert_eq!(value["name"], "demo");
    assert_eq!(value["parameters"]["type"], "object");
}

#[test]
fn default_chat_backend_selects_openai_compatible_from_env() {
    let backend = default_chat_backend_for_provider_with(None, |name| match name {
        "LLM_BACKEND" => Some("openai_compatible".to_string()),
        _ => None,
    });

    assert_eq!(backend.id(), "openai_compatible_chat_completions");
    assert!(!backend.supports_previous_response_id());
}

#[test]
fn default_chat_backend_selects_anthropic_from_env() {
    let backend = default_chat_backend_for_provider_with(None, |name| match name {
        "LLM_BACKEND" => Some("anthropic".to_string()),
        _ => None,
    });

    assert_eq!(backend.id(), "anthropic_messages");
    assert!(!backend.supports_previous_response_id());
}

#[test]
fn default_chat_backend_selects_gemini_from_env() {
    let backend = default_chat_backend_for_provider_with(None, |name| match name {
        "LLM_BACKEND" => Some("gemini".to_string()),
        _ => None,
    });

    assert_eq!(backend.id(), "google_gemini_generate_content");
    assert!(!backend.supports_previous_response_id());
}

#[test]
fn default_chat_backend_selects_codex_from_env() {
    let backend = default_chat_backend_for_provider_with(None, |name| match name {
        "LLM_BACKEND" => Some("codex".to_string()),
        _ => None,
    });

    assert_eq!(backend.id(), CODEX_BACKEND_ID);
    assert!(backend.supports_previous_response_id());
}

#[test]
fn default_chat_backend_prefers_model_provider_over_env() {
    let backend = default_chat_backend_for_provider_with(Some("anthropic"), |name| match name {
        "LLM_BACKEND" => Some("openai".to_string()),
        _ => None,
    });

    assert_eq!(backend.id(), "anthropic_messages");
}

#[test]
fn extract_openai_chat_completions_output_parses_tool_calls() {
    let body = serde_json::json!({
        "choices": [
            {
                "message": {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": "call_fn",
                            "type": "function",
                            "function": {
                                "name": "lookup",
                                "arguments": "{\"id\":\"123\"}"
                            }
                        },
                        {
                            "id": "call_custom",
                            "type": "custom",
                            "custom": {
                                "name": "shell",
                                "input": "echo hi"
                            }
                        }
                    ]
                }
            }
        ]
    });

    let reply = match extract_openai_chat_completions_output(&body).unwrap() {
        ModelGeneration::Reply(reply) => reply,
        ModelGeneration::NotConfigured { backend, reason } => {
            panic!("expected model reply, got unconfigured backend {backend}: {reason}");
        }
    };
    assert_eq!(reply.backend, "openai_compatible_chat_completions");
    assert!(reply.text.is_none());
    assert_eq!(reply.tool_calls.len(), 2);
    assert_eq!(reply.tool_calls[0].kind, ModelToolKind::Function);
    assert_eq!(reply.tool_calls[0].name, "lookup");
    assert_eq!(reply.tool_calls[1].kind, ModelToolKind::Custom);
    assert_eq!(reply.tool_calls[1].input, "echo hi");
}

#[test]
fn extract_anthropic_output_parses_tool_use_blocks() {
    let body = serde_json::json!({
        "id": "msg_123",
        "content": [
            { "type": "text", "text": "Let me check." },
            {
                "type": "tool_use",
                "id": "toolu_123",
                "name": "lookup",
                "input": { "id": "123" }
            }
        ]
    });

    let reply = match extract_anthropic_output(&body).unwrap() {
        ModelGeneration::Reply(reply) => reply,
        ModelGeneration::NotConfigured { backend, reason } => {
            panic!("expected anthropic reply, got unconfigured backend {backend}: {reason}");
        }
    };
    assert_eq!(reply.backend, "anthropic_messages");
    assert_eq!(reply.response_id.as_deref(), Some("msg_123"));
    assert_eq!(reply.text.as_deref(), Some("Let me check."));
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].name, "lookup");
    assert_eq!(reply.tool_calls[0].kind, ModelToolKind::Function);
    assert_eq!(reply.tool_calls[0].input, "{\"id\":\"123\"}");
}

#[test]
fn extract_gemini_output_parses_function_calls() {
    let body = serde_json::json!({
        "candidates": [
            {
                "content": {
                    "parts": [
                        { "text": "Checking..." },
                        {
                            "functionCall": {
                                "name": "lookup",
                                "args": { "id": "123" }
                            }
                        }
                    ]
                }
            }
        ]
    });

    let reply = match extract_gemini_output(&body).unwrap() {
        ModelGeneration::Reply(reply) => reply,
        ModelGeneration::NotConfigured { backend, reason } => {
            panic!("expected gemini reply, got unconfigured backend {backend}: {reason}");
        }
    };
    assert_eq!(reply.backend, "google_gemini_generate_content");
    assert_eq!(reply.text.as_deref(), Some("Checking..."));
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].name, "lookup");
    assert_eq!(reply.tool_calls[0].kind, ModelToolKind::Function);
    assert_eq!(reply.tool_calls[0].input, "{\"id\":\"123\"}");
}

#[test]
fn extract_openai_output_parses_function_calls() {
    let body = serde_json::json!({
        "output": [
            {
                "type": "function_call",
                "call_id": "call_1",
                "name": "demo",
                "arguments": "{\"id\":\"123\"}"
            }
        ]
    });

    let (text, tool_calls) = extract_openai_output(&body).unwrap();

    assert!(text.is_none());
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].name, "demo");
    assert_eq!(tool_calls[0].kind, ModelToolKind::Function);
    assert_eq!(tool_calls[0].input, "{\"id\":\"123\"}");
}

#[test]
fn anthropic_max_tokens_uses_context_token_limit_when_present() {
    let request = ModelRequest {
        model: "claude-sonnet-4".to_string(),
        provider: Some("anthropic".to_string()),
        llm_timeout_ms: None,
        context_token_limit: Some(16000),
        tool_call_limit: None,
        tool_output_limit: None,
        working_directory: None,
        instructions: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        pending_tool_calls: Vec::new(),
        tool_outputs: Vec::new(),
        previous_response_id: None,
    };

    assert_eq!(anthropic_max_tokens(&request), 16000);
}

#[test]
fn openai_chat_completions_messages_include_structured_tool_followup() {
    let request = ModelRequest {
        model: "gpt-5-mini".to_string(),
        provider: Some("openai_compatible".to_string()),
        llm_timeout_ms: None,
        context_token_limit: None,
        tool_call_limit: None,
        tool_output_limit: None,
        working_directory: None,
        instructions: "Be helpful.".to_string(),
        messages: vec![ConversationMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        }],
        tools: Vec::new(),
        pending_tool_calls: vec![ModelToolCall {
            call_id: "call_1".to_string(),
            name: "lookup".to_string(),
            input: "{\"id\":\"123\"}".to_string(),
            kind: ModelToolKind::Function,
        }],
        tool_outputs: vec![ModelToolOutput {
            call_id: "call_1".to_string(),
            name: "lookup".to_string(),
            output: "found".to_string(),
            kind: ModelToolKind::Function,
        }],
        previous_response_id: None,
    };

    let messages = openai_chat_completions_messages(&request);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["tool_calls"][0]["function"]["name"], "lookup");
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "call_1");
    assert_eq!(messages[3]["content"], "found");
}

#[test]
fn anthropic_messages_include_tool_use_and_tool_result_blocks() {
    let request = ModelRequest {
        model: "claude-sonnet-4".to_string(),
        provider: Some("anthropic".to_string()),
        llm_timeout_ms: None,
        context_token_limit: None,
        tool_call_limit: None,
        tool_output_limit: None,
        working_directory: None,
        instructions: String::new(),
        messages: vec![ConversationMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        }],
        tools: Vec::new(),
        pending_tool_calls: vec![ModelToolCall {
            call_id: "toolu_1".to_string(),
            name: "lookup".to_string(),
            input: "{\"id\":\"123\"}".to_string(),
            kind: ModelToolKind::Function,
        }],
        tool_outputs: vec![ModelToolOutput {
            call_id: "toolu_1".to_string(),
            name: "lookup".to_string(),
            output: "found".to_string(),
            kind: ModelToolKind::Function,
        }],
        previous_response_id: None,
    };

    let messages = anthropic_messages(&request);
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"][0]["type"], "tool_use");
    assert_eq!(messages[1]["content"][0]["name"], "lookup");
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"][0]["type"], "tool_result");
    assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_1");
}

#[test]
fn gemini_messages_include_function_call_and_response_parts() {
    let request = ModelRequest {
        model: "gemini-2.5-pro".to_string(),
        provider: Some("gemini".to_string()),
        llm_timeout_ms: None,
        context_token_limit: None,
        tool_call_limit: None,
        tool_output_limit: None,
        working_directory: None,
        instructions: String::new(),
        messages: vec![ConversationMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        }],
        tools: Vec::new(),
        pending_tool_calls: vec![ModelToolCall {
            call_id: "call_1".to_string(),
            name: "lookup".to_string(),
            input: "{\"id\":\"123\"}".to_string(),
            kind: ModelToolKind::Function,
        }],
        tool_outputs: vec![ModelToolOutput {
            call_id: "call_1".to_string(),
            name: "lookup".to_string(),
            output: "found".to_string(),
            kind: ModelToolKind::Function,
        }],
        previous_response_id: None,
    };

    let messages = gemini_messages(&request);
    assert_eq!(messages[1]["role"], "model");
    assert_eq!(messages[1]["parts"][0]["functionCall"]["name"], "lookup");
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(
        messages[2]["parts"][0]["functionResponse"]["name"],
        "lookup"
    );
}

#[test]
#[cfg(unix)]
fn codex_backend_streams_reply_and_resumes_previous_thread() {
    let _guard = lock_codex_backend_test();
    let dir = tempdir().unwrap();
    let log_path = dir.path().join("codex.log");
    let script_path = dir.path().join("codex-app-server");
    write_executable_script(
        &script_path,
        &format!(
            "#!/bin/sh\nLOG='{}'\nwhile IFS= read -r line; do\nprintf '%s\\n' \"$line\" >> \"$LOG\"\ncase \"$line\" in\n*'\"method\":\"initialize\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{}}}}' ;;\n*'\"method\":\"initialized\"'*) : ;;\n*'\"method\":\"thread/start\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"thread\":{{\"id\":\"thread-new\"}}}}}}' ;;\n*'\"method\":\"thread/resume\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"thread\":{{\"id\":\"thread-resumed\"}}}}}}' ;;\n*'\"method\":\"turn/start\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{{\"turn\":{{\"id\":\"turn-1\"}}}}}}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"item/agentMessage/delta\",\"params\":{{\"delta\":\"hello \"}}}}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"item/agentMessage/delta\",\"params\":{{\"delta\":\"world\"}}}}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"turn/completed\",\"params\":{{\"turn\":{{\"id\":\"turn-1\",\"status\":\"completed\"}}}}}}' ;;\nesac\ndone\n",
            log_path.display()
        ),
    );

    let backend =
        CodexAppServerBackend::with_binary_path_for_tests(script_path.display().to_string());

    let first = backend
        .generate_with_events(
            &ModelRequest {
                model: "gpt-5.4".to_string(),
                provider: Some("codex".to_string()),
                llm_timeout_ms: None,
                context_token_limit: None,
                tool_call_limit: None,
                tool_output_limit: None,
                working_directory: Some(dir.path().display().to_string()),
                instructions: "Be helpful.".to_string(),
                messages: vec![ConversationMessage {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                }],
                tools: Vec::new(),
                pending_tool_calls: Vec::new(),
                tool_outputs: Vec::new(),
                previous_response_id: None,
            },
            &mut |_| {},
        )
        .unwrap();
    let first = match first {
        ModelGeneration::Reply(reply) => reply,
        other => panic!("expected codex reply, got {other:?}"),
    };
    assert_eq!(first.text.as_deref(), Some("hello world"));
    let first_state = first
        .response_id
        .clone()
        .expect("codex state should persist");
    assert!(first_state.contains("thread-new"));

    let second = backend
        .generate_with_events(
            &ModelRequest {
                model: "gpt-5.4".to_string(),
                provider: Some("codex".to_string()),
                llm_timeout_ms: None,
                context_token_limit: None,
                tool_call_limit: None,
                tool_output_limit: None,
                working_directory: Some(dir.path().display().to_string()),
                instructions: "Be helpful.".to_string(),
                messages: vec![ConversationMessage {
                    role: "user".to_string(),
                    content: "follow up".to_string(),
                }],
                tools: Vec::new(),
                pending_tool_calls: Vec::new(),
                tool_outputs: Vec::new(),
                previous_response_id: Some(first_state),
            },
            &mut |_| {},
        )
        .unwrap();
    let second = match second {
        ModelGeneration::Reply(reply) => reply,
        other => panic!("expected codex reply, got {other:?}"),
    };
    let second_state = second.response_id.expect("codex state should persist");
    assert!(second_state.contains("thread-resumed"));

    let log = fs::read_to_string(&log_path).unwrap();
    assert!(log.contains("\"method\":\"thread/start\""));
    assert!(log.contains("\"method\":\"thread/resume\""));
    assert!(log.contains("\"persistExtendedHistory\":true"));
}

#[test]
#[cfg(unix)]
fn codex_backend_resume_failure_returns_error() {
    let _guard = lock_codex_backend_test();
    let dir = tempdir().unwrap();
    let script_path = dir.path().join("codex-app-server");
    write_executable_script(
        &script_path,
        "#!/bin/sh\nwhile IFS= read -r line; do\ncase \"$line\" in\n*'\"method\":\"initialize\"'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}' ;;\n*'\"method\":\"initialized\"'*) : ;;\n*'\"method\":\"thread/resume\"'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"error\":{\"code\":-32001,\"message\":\"thread expired\"}}' ;;\nesac\ndone\n",
    );

    let backend =
        CodexAppServerBackend::with_binary_path_for_tests(script_path.display().to_string());
    let error = backend
        .generate_with_events(
            &ModelRequest {
                model: "gpt-5.4".to_string(),
                provider: Some("codex".to_string()),
                llm_timeout_ms: None,
                context_token_limit: None,
                tool_call_limit: None,
                tool_output_limit: None,
                working_directory: Some(dir.path().display().to_string()),
                instructions: "Be helpful.".to_string(),
                messages: vec![ConversationMessage {
                    role: "user".to_string(),
                    content: "follow up".to_string(),
                }],
                tools: Vec::new(),
                pending_tool_calls: Vec::new(),
                tool_outputs: Vec::new(),
                previous_response_id: Some("thread-old".to_string()),
            },
            &mut |_| {},
        )
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("failed to resume codex thread `thread-old`")
    );
    assert!(error.to_string().contains("thread expired"));
}

#[test]
#[cfg(unix)]
fn codex_backend_respects_llm_timeout() {
    let _guard = lock_codex_backend_test();
    let dir = tempdir().unwrap();
    let script_path = dir.path().join("codex-app-server");
    write_executable_script(&script_path, "#!/bin/sh\nsleep 2\n");

    let backend =
        CodexAppServerBackend::with_binary_path_for_tests(script_path.display().to_string());
    let error = backend
        .generate_with_events(
            &ModelRequest {
                model: "gpt-5.4".to_string(),
                provider: Some("codex".to_string()),
                llm_timeout_ms: Some(50),
                context_token_limit: None,
                tool_call_limit: None,
                tool_output_limit: None,
                working_directory: Some(dir.path().display().to_string()),
                instructions: "Be helpful.".to_string(),
                messages: vec![ConversationMessage {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                }],
                tools: Vec::new(),
                pending_tool_calls: Vec::new(),
                tool_outputs: Vec::new(),
                previous_response_id: None,
            },
            &mut |_| {},
        )
        .unwrap_err();

    assert!(error.to_string().contains("timed out"));
}

#[test]
#[cfg(unix)]
fn codex_backend_preserves_existing_codex_home() {
    let _guard = lock_codex_backend_test();
    let previous = std::env::var_os("CODEX_HOME");
    let dir = tempdir().unwrap();
    let expected_home = dir.path().join("codex-home");
    fs::create_dir_all(&expected_home).unwrap();
    unsafe {
        std::env::set_var("CODEX_HOME", &expected_home);
    }

    let env_log_path = dir.path().join("codex-home.log");
    let script_path = dir.path().join("codex-app-server");
    write_executable_script(
        &script_path,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$CODEX_HOME\" > '{}'\nwhile IFS= read -r line; do\ncase \"$line\" in\n*'\"method\":\"initialize\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{}}}}' ;;\n*'\"method\":\"initialized\"'*) : ;;\n*'\"method\":\"thread/start\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"thread\":{{\"id\":\"thread-new\"}}}}}}' ;;\n*'\"method\":\"turn/start\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{{\"turn\":{{\"id\":\"turn-1\"}}}}}}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"item/agentMessage/delta\",\"params\":{{\"delta\":\"ok\"}}}}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"turn/completed\",\"params\":{{\"turn\":{{\"id\":\"turn-1\",\"status\":\"completed\"}}}}}}' ;;\nesac\ndone\n",
            env_log_path.display()
        ),
    );

    let backend =
        CodexAppServerBackend::with_binary_path_for_tests(script_path.display().to_string());
    let result = backend.generate_with_events(
        &ModelRequest {
            model: "gpt-5.4".to_string(),
            provider: Some("codex".to_string()),
            model_options: Default::default(),
            llm_timeout_ms: None,
            context_token_limit: None,
            tool_call_limit: None,
            tool_output_limit: None,
            working_directory: Some(dir.path().display().to_string()),
            instructions: "Be helpful.".to_string(),
            messages: vec![ConversationMessage {
                role: "user".to_string(),
                content: "hello".to_string(),
            }],
            tools: Vec::new(),
            pending_tool_calls: Vec::new(),
            tool_outputs: Vec::new(),
            previous_response_id: None,
        },
        &mut |_| {},
    );

    match previous {
        Some(value) => unsafe {
            std::env::set_var("CODEX_HOME", value);
        },
        None => unsafe {
            std::env::remove_var("CODEX_HOME");
        },
    }

    result.unwrap();

    let logged_home = fs::read_to_string(&env_log_path).unwrap();
    assert_eq!(logged_home.trim(), expected_home.display().to_string());
}
