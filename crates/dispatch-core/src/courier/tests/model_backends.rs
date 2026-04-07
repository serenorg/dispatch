use super::*;

// These overrides are scoped to the current test thread. `lock_codex_backend_test()`
// exists to provide cleanup symmetry across codex/claude/plugin backend tests.
struct TestEnvOverride(&'static str);

impl TestEnvOverride {
    fn set(name: &'static str, value: Option<&str>) -> Self {
        set_test_env_override(name, value);
        Self(name)
    }
}

impl Drop for TestEnvOverride {
    fn drop(&mut self) {
        clear_test_env_override(self.0);
    }
}

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
fn default_chat_backend_selects_claude_from_env() {
    let backend = default_chat_backend_for_provider_with(None, |name| match name {
        "LLM_BACKEND" => Some("claude".to_string()),
        _ => None,
    });

    assert_eq!(backend.id(), "claude");
    assert!(backend.supports_previous_response_id());
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
fn default_chat_backend_selects_plugin_for_unknown_provider() {
    let backend = default_chat_backend_for_provider_with(Some("demo-plugin"), |_| None);

    assert_eq!(backend.id(), "demo-plugin");
    assert!(!backend.supports_previous_response_id());
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
        model_options: Default::default(),
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
        model_options: Default::default(),
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
        model_options: Default::default(),
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
        model_options: Default::default(),
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
fn claude_backend_streams_reply_and_resumes_previous_session() {
    let _guard = lock_codex_backend_test();
    let dir = tempdir().unwrap();
    let args_log_path = dir.path().join("claude-args.log");
    let stdin_log_path = dir.path().join("claude-stdin.log");
    let script_path = dir.path().join("claude");
    // Mock script emits stream-json NDJSON. Detects resume by checking for
    // --resume in the CLI args it received.
    write_executable_script(
        &script_path,
        &format!(
            concat!(
                "#!/bin/sh\n",
                "printf '%s\\n' \"$*\" >> '{log}'\n",
                "while IFS= read -r line; do\n",
                "  printf '%s\\n' \"$line\" >> '{stdin_log}'\n",
                "done\n",
                "case \"$*\" in\n",
                "*--resume\\ session-new*)\n",
                "  printf '%s\\n' '{{\"type\":\"stream_event\",\"event\":{{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":\"followed up\"}}}}}}'\n",
                "  printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"followed up\",\"session_id\":\"session-new\",\"is_error\":false}}'\n",
                "  ;;\n",
                "*)\n",
                "  printf '%s\\n' '{{\"type\":\"stream_event\",\"event\":{{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":\"hello \"}}}}}}'\n",
                "  printf '%s\\n' '{{\"type\":\"stream_event\",\"event\":{{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":\"world\"}}}}}}'\n",
                "  printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"hello world\",\"session_id\":\"session-new\",\"is_error\":false}}'\n",
                "  ;;\n",
                "esac\n",
            ),
            log = args_log_path.display(),
            stdin_log = stdin_log_path.display(),
        ),
    );

    let backend = ClaudeCliBackend::with_binary_path_for_tests(script_path.display().to_string());
    let mut first_deltas = Vec::new();
    let first = backend
        .generate_with_events(
            &ModelRequest {
                model: "claude-sonnet-4-6".to_string(),
                provider: Some("claude".to_string()),
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
            &mut |event| match event {
                ModelStreamEvent::TextDelta { content } => first_deltas.push(content),
            },
        )
        .unwrap();
    let first = match first {
        ModelGeneration::Reply(reply) => reply,
        other => panic!("expected claude reply, got {other:?}"),
    };
    assert_eq!(first.text.as_deref(), Some("hello world"));
    assert_eq!(
        first_deltas,
        vec!["hello ".to_string(), "world".to_string()]
    );
    let first_state = first
        .response_id
        .clone()
        .expect("claude state should persist");

    let second = backend
        .generate_with_events(
            &ModelRequest {
                model: "claude-sonnet-4-6".to_string(),
                provider: Some("claude".to_string()),
                model_options: Default::default(),
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
        other => panic!("expected claude reply, got {other:?}"),
    };
    assert_eq!(second.text.as_deref(), Some("followed up"));
    assert_eq!(second.response_id.as_deref(), Some("session-new"));

    let log = fs::read_to_string(&args_log_path).unwrap();
    assert!(
        !log.contains("--bare"),
        "--bare must not be used; it disables OAuth auth"
    );
    assert!(log.contains("--input-format stream-json"));
    assert!(log.contains("--output-format stream-json"));
    assert!(log.contains("--verbose"));
    assert!(log.contains("--setting-sources"));
    assert!(log.contains("--model claude-sonnet-4-6"));
    assert!(log.contains("--append-system-prompt Be helpful."));
    assert!(log.contains("--resume session-new"));

    let stdin_log = fs::read_to_string(&stdin_log_path).unwrap();
    let stdin_lines = stdin_log.lines().collect::<Vec<_>>();
    assert_eq!(
        stdin_lines.len(),
        2,
        "unexpected stdin payloads: {stdin_log}"
    );

    let first_payload: serde_json::Value = serde_json::from_str(stdin_lines[0]).unwrap();
    assert_eq!(first_payload["type"], "user");
    assert_eq!(first_payload["message"]["role"], "user");
    assert_eq!(first_payload["message"]["content"], "hello");
    assert_eq!(first_payload["session_id"], "");

    let second_payload: serde_json::Value = serde_json::from_str(stdin_lines[1]).unwrap();
    assert_eq!(second_payload["type"], "user");
    assert_eq!(second_payload["message"]["role"], "user");
    assert_eq!(second_payload["message"]["content"], "follow up");
    assert_eq!(second_payload["session_id"], "session-new");
}

#[test]
#[cfg(unix)]
fn claude_backend_can_disable_persistent_session_per_request() {
    let _guard = lock_codex_backend_test();
    let dir = tempdir().unwrap();
    let args_log_path = dir.path().join("claude-args.log");
    let stdin_log_path = dir.path().join("claude-stdin.log");
    let script_path = dir.path().join("claude");
    write_executable_script(
        &script_path,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nwhile IFS= read -r line; do\nprintf '%s\\n' \"$line\" >> '{}'\ndone\nprintf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"ok\",\"session_id\":\"some-session\",\"is_error\":false}}'\n",
            args_log_path.display(),
            stdin_log_path.display()
        ),
    );

    let backend = ClaudeCliBackend::with_binary_path_for_tests(script_path.display().to_string());
    let reply = backend
        .generate_with_events(
            &ModelRequest {
                model: "claude-sonnet-4-6".to_string(),
                provider: Some("claude".to_string()),
                model_options: [("persist-thread".to_string(), "false".to_string())]
                    .into_iter()
                    .collect(),
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
                previous_response_id: Some("session-old".to_string()),
            },
            &mut |_| {},
        )
        .unwrap();

    let reply = match reply {
        ModelGeneration::Reply(reply) => reply,
        other => panic!("expected claude reply, got {other:?}"),
    };
    // persist-thread=false -> no response_id returned, no --resume passed,
    // and --no-session-persistence is used to avoid writing a session file.
    assert!(reply.response_id.is_none());
    let log = fs::read_to_string(&args_log_path).unwrap();
    assert!(
        !log.contains("--resume"),
        "expected no --resume when persist-thread=false"
    );
    assert!(
        log.contains("--no-session-persistence"),
        "expected --no-session-persistence when persist-thread=false"
    );
    assert!(
        log.contains("--append-system-prompt Be helpful."),
        "expected system instructions on fresh ephemeral session: {log}"
    );

    let stdin_log = fs::read_to_string(&stdin_log_path).unwrap();
    let stdin_lines = stdin_log.lines().collect::<Vec<_>>();
    assert_eq!(
        stdin_lines.len(),
        1,
        "unexpected stdin payloads: {stdin_log}"
    );
    let payload: serde_json::Value = serde_json::from_str(stdin_lines[0]).unwrap();
    assert_eq!(payload["message"]["content"], "hello");
    assert_eq!(payload["session_id"], "");
}

#[test]
#[cfg(unix)]
fn claude_backend_uses_reasoning_effort_model_option() {
    let _guard = lock_codex_backend_test();
    let dir = tempdir().unwrap();
    let args_log_path = dir.path().join("claude-args.log");
    let script_path = dir.path().join("claude");
    write_executable_script(
        &script_path,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"ok\",\"session_id\":\"s1\",\"is_error\":false}}'\n",
            args_log_path.display()
        ),
    );

    let backend = ClaudeCliBackend::with_binary_path_for_tests(script_path.display().to_string());
    let _ = backend
        .generate_with_events(
            &ModelRequest {
                model: "claude-sonnet-4-6".to_string(),
                provider: Some("claude".to_string()),
                model_options: [("reasoning-effort".to_string(), "high".to_string())]
                    .into_iter()
                    .collect(),
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

    let log = fs::read_to_string(&args_log_path).unwrap();
    assert!(
        log.contains("--effort high"),
        "expected --effort high in args: {log}"
    );
}

#[test]
#[cfg(unix)]
fn claude_backend_uses_reasoning_effort_env_override() {
    let _guard = lock_codex_backend_test();
    let _env = TestEnvOverride::set("DISPATCH_REASONING_EFFORT", Some("low"));

    let dir = tempdir().unwrap();
    let args_log_path = dir.path().join("claude-args.log");
    let script_path = dir.path().join("claude");
    write_executable_script(
        &script_path,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"ok\",\"session_id\":\"s1\",\"is_error\":false}}'\n",
            args_log_path.display()
        ),
    );

    let backend = ClaudeCliBackend::with_binary_path_for_tests(script_path.display().to_string());
    let result = backend.generate_with_events(
        &ModelRequest {
            model: "claude-sonnet-4-6".to_string(),
            provider: Some("claude".to_string()),
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

    result.unwrap();

    let log = fs::read_to_string(&args_log_path).unwrap();
    assert!(
        log.contains("--effort low"),
        "expected env-supplied --effort low in args: {log}"
    );
}

#[test]
#[cfg(unix)]
fn claude_backend_model_option_takes_precedence_over_reasoning_effort_env() {
    let _guard = lock_codex_backend_test();
    let _env = TestEnvOverride::set("DISPATCH_REASONING_EFFORT", Some("low"));

    let dir = tempdir().unwrap();
    let args_log_path = dir.path().join("claude-args.log");
    let script_path = dir.path().join("claude");
    write_executable_script(
        &script_path,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"ok\",\"session_id\":\"s1\",\"is_error\":false}}'\n",
            args_log_path.display()
        ),
    );

    let backend = ClaudeCliBackend::with_binary_path_for_tests(script_path.display().to_string());
    let result = backend.generate_with_events(
        &ModelRequest {
            model: "claude-sonnet-4-6".to_string(),
            provider: Some("claude".to_string()),
            model_options: [("reasoning-effort".to_string(), "high".to_string())]
                .into_iter()
                .collect(),
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

    result.unwrap();

    let log = fs::read_to_string(&args_log_path).unwrap();
    assert!(
        log.contains("--effort high"),
        "parcel model option should override env var: {log}"
    );
    assert!(
        !log.contains("--effort low"),
        "unexpected env reasoning effort when parcel option is set: {log}"
    );
}

#[test]
fn claude_backend_rejects_unsupported_reasoning_effort_model_option() {
    let backend = ClaudeCliBackend;
    let error = backend
        .generate_with_events(
            &ModelRequest {
                model: "claude-sonnet-4-6".to_string(),
                provider: Some("claude".to_string()),
                model_options: [("reasoning-effort".to_string(), "turbo".to_string())]
                    .into_iter()
                    .collect(),
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
                pending_tool_calls: Vec::new(),
                tool_outputs: Vec::new(),
                previous_response_id: None,
            },
            &mut |_| {},
        )
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("claude reasoning effort `turbo` is not supported"),
        "unexpected invalid-effort error: {error}"
    );
}

#[test]
fn claude_backend_rejects_unsupported_reasoning_effort_env_override() {
    let _env = TestEnvOverride::set("DISPATCH_REASONING_EFFORT", Some("turbo"));

    let backend = ClaudeCliBackend;
    let result = backend.generate_with_events(
        &ModelRequest {
            model: "claude-sonnet-4-6".to_string(),
            provider: Some("claude".to_string()),
            model_options: Default::default(),
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
            pending_tool_calls: Vec::new(),
            tool_outputs: Vec::new(),
            previous_response_id: None,
        },
        &mut |_| {},
    );

    let error = result.unwrap_err().to_string();
    assert!(
        error.contains("claude reasoning effort `turbo` is not supported"),
        "unexpected invalid-effort error: {error}"
    );
}

#[test]
#[cfg(unix)]
fn claude_backend_returns_not_configured_when_binary_is_missing() {
    let _guard = lock_codex_backend_test();
    let backend = ClaudeCliBackend::with_binary_path_for_tests(
        "/definitely/missing/dispatch-test-claude".to_string(),
    );

    let result = backend
        .generate_with_events(
            &ModelRequest {
                model: "claude-sonnet-4-6".to_string(),
                provider: Some("claude".to_string()),
                model_options: Default::default(),
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
                pending_tool_calls: Vec::new(),
                tool_outputs: Vec::new(),
                previous_response_id: None,
            },
            &mut |_| {},
        )
        .unwrap();

    match result {
        ModelGeneration::NotConfigured { backend, reason } => {
            assert_eq!(backend, "claude");
            assert!(
                reason.contains("CLAUDE_BINARY"),
                "unexpected reason: {reason}"
            );
        }
        other => panic!("expected NotConfigured, got {other:?}"),
    }
}

#[test]
#[cfg(unix)]
fn claude_backend_surfaces_result_errors() {
    let _guard = lock_codex_backend_test();
    let dir = tempdir().unwrap();
    let script_path = dir.path().join("claude");
    write_executable_script(
        &script_path,
        "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"result\",\"subtype\":\"error\",\"result\":\"auth failed\",\"is_error\":true}'\n",
    );

    let backend = ClaudeCliBackend::with_binary_path_for_tests(script_path.display().to_string());
    let error = backend
        .generate_with_events(
            &ModelRequest {
                model: "claude-sonnet-4-6".to_string(),
                provider: Some("claude".to_string()),
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
        )
        .unwrap_err();

    assert!(error.to_string().contains("auth failed"));
}

#[test]
#[cfg(unix)]
fn claude_backend_includes_stderr_on_non_zero_exit() {
    let _guard = lock_codex_backend_test();
    let dir = tempdir().unwrap();
    let script_path = dir.path().join("claude");
    write_executable_script(
        &script_path,
        "#!/bin/sh\nprintf '%s\\n' 'fatal from stderr' >&2\nexit 7\n",
    );

    let backend = ClaudeCliBackend::with_binary_path_for_tests(script_path.display().to_string());
    let error = backend
        .generate_with_events(
            &ModelRequest {
                model: "claude-sonnet-4-6".to_string(),
                provider: Some("claude".to_string()),
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
        )
        .unwrap_err();

    let message = error.to_string();
    assert!(
        message.contains("status exit status: 7"),
        "unexpected error: {message}"
    );
    assert!(
        message.contains("fatal from stderr"),
        "stderr detail missing from error: {message}"
    );
}

#[test]
#[cfg(unix)]
fn claude_backend_respects_llm_timeout() {
    let _guard = lock_codex_backend_test();
    let dir = tempdir().unwrap();
    let script_path = dir.path().join("claude");
    write_executable_script(&script_path, "#!/bin/sh\nsleep 2\n");

    let backend = ClaudeCliBackend::with_binary_path_for_tests(script_path.display().to_string());
    let error = backend
        .generate_with_events(
            &ModelRequest {
                model: "claude-sonnet-4-6".to_string(),
                provider: Some("claude".to_string()),
                model_options: Default::default(),
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
                model_options: Default::default(),
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

    let start_line = log
        .lines()
        .find(|l| l.contains("\"method\":\"thread/start\""))
        .expect("thread/start should appear in log");
    assert!(
        start_line.contains("\"persistExtendedHistory\":true"),
        "thread/start should carry persistExtendedHistory:true: {start_line}"
    );

    let resume_line = log
        .lines()
        .find(|l| l.contains("\"method\":\"thread/resume\""))
        .expect("thread/resume should appear in log");
    assert!(
        resume_line.contains("\"persistExtendedHistory\":true"),
        "thread/resume should carry persistExtendedHistory:true: {resume_line}"
    );
}

#[test]
#[cfg(unix)]
fn codex_backend_can_disable_persistent_history_per_request() {
    let _guard = lock_codex_backend_test();
    let dir = tempdir().unwrap();
    let log_path = dir.path().join("codex.log");
    let script_path = dir.path().join("codex-app-server");
    write_executable_script(
        &script_path,
        &format!(
            "#!/bin/sh\nLOG='{}'\nwhile IFS= read -r line; do\nprintf '%s\\n' \"$line\" >> \"$LOG\"\ncase \"$line\" in\n*'\"method\":\"initialize\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{}}}}' ;;\n*'\"method\":\"initialized\"'*) : ;;\n*'\"method\":\"thread/start\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"thread\":{{\"id\":\"thread-ephemeral\"}}}}}}' ;;\n*'\"method\":\"turn/start\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{{\"turn\":{{\"id\":\"turn-1\"}}}}}}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"item/agentMessage/delta\",\"params\":{{\"delta\":\"hello\"}}}}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"turn/completed\",\"params\":{{\"turn\":{{\"id\":\"turn-1\",\"status\":\"completed\"}}}}}}' ;;\nesac\ndone\n",
            log_path.display()
        ),
    );

    let backend =
        CodexAppServerBackend::with_binary_path_for_tests(script_path.display().to_string());
    let reply = backend
        .generate_with_events(
            &ModelRequest {
                model: "gpt-5.4".to_string(),
                provider: Some("codex".to_string()),
                model_options: [("persist-thread".to_string(), "false".to_string())]
                    .into_iter()
                    .collect(),
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
                previous_response_id: Some("thread-old".to_string()),
            },
            &mut |_| {},
        )
        .unwrap();
    let reply = match reply {
        ModelGeneration::Reply(reply) => reply,
        other => panic!("expected codex reply, got {other:?}"),
    };

    assert!(reply.response_id.is_none());
    let log = fs::read_to_string(&log_path).unwrap();
    assert!(log.contains("\"method\":\"thread/start\""));
    assert!(!log.contains("\"method\":\"thread/resume\""));
    assert!(log.contains("\"persistExtendedHistory\":false"));
}

#[test]
#[cfg(unix)]
fn codex_backend_env_override_disables_persistent_history() {
    let _guard = lock_codex_backend_test();
    let _env = TestEnvOverride::set("DISPATCH_PERSIST_THREAD", Some("0"));

    let dir = tempdir().unwrap();
    let log_path = dir.path().join("codex.log");
    let script_path = dir.path().join("codex-app-server");
    write_executable_script(
        &script_path,
        &format!(
            "#!/bin/sh\nLOG='{}'\nwhile IFS= read -r line; do\nprintf '%s\\n' \"$line\" >> \"$LOG\"\ncase \"$line\" in\n*'\"method\":\"initialize\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{}}}}' ;;\n*'\"method\":\"initialized\"'*) : ;;\n*'\"method\":\"thread/start\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"thread\":{{\"id\":\"thread-ephemeral\"}}}}}}' ;;\n*'\"method\":\"turn/start\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{{\"turn\":{{\"id\":\"turn-1\"}}}}}}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"item/agentMessage/delta\",\"params\":{{\"delta\":\"hello\"}}}}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"turn/completed\",\"params\":{{\"turn\":{{\"id\":\"turn-1\",\"status\":\"completed\"}}}}}}' ;;\nesac\ndone\n",
            log_path.display()
        ),
    );

    let backend =
        CodexAppServerBackend::with_binary_path_for_tests(script_path.display().to_string());
    let result = backend.generate_with_events(
        &ModelRequest {
            model: "gpt-5.4".to_string(),
            provider: Some("codex".to_string()),
            model_options: [("persist-thread".to_string(), "true".to_string())]
                .into_iter()
                .collect(),
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

    let reply = match result.unwrap() {
        ModelGeneration::Reply(reply) => reply,
        other => panic!("expected codex reply, got {other:?}"),
    };
    assert!(reply.response_id.is_none());

    let log = fs::read_to_string(&log_path).unwrap();
    assert!(log.contains("\"persistExtendedHistory\":false"));
}

#[test]
#[cfg(unix)]
fn codex_backend_uses_reasoning_effort_model_option() {
    let _guard = lock_codex_backend_test();
    let dir = tempdir().unwrap();
    let log_path = dir.path().join("codex.log");
    let script_path = dir.path().join("codex-app-server");
    write_executable_script(
        &script_path,
        &format!(
            "#!/bin/sh\nLOG='{}'\nwhile IFS= read -r line; do\nprintf '%s\\n' \"$line\" >> \"$LOG\"\ncase \"$line\" in\n*'\"method\":\"initialize\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{}}}}' ;;\n*'\"method\":\"initialized\"'*) : ;;\n*'\"method\":\"model/list\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"data\":[{{\"id\":\"gpt-5.4\",\"model\":\"gpt-5.4\",\"displayName\":\"GPT-5.4\",\"description\":\"\",\"supportedReasoningEfforts\":[{{\"reasoningEffort\":\"low\",\"description\":\"\"}},{{\"reasoningEffort\":\"medium\",\"description\":\"\"}},{{\"reasoningEffort\":\"high\",\"description\":\"\"}}],\"defaultReasoningEffort\":\"medium\",\"inputModalities\":[\"text\"],\"supportsPersonality\":false,\"isDefault\":true}}]}}}}' ;;\n*'\"method\":\"thread/start\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{{\"thread\":{{\"id\":\"thread-new\"}}}}}}' ;;\n*'\"method\":\"turn/start\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{{\"turn\":{{\"id\":\"turn-1\"}}}}}}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"item/agentMessage/delta\",\"params\":{{\"delta\":\"ok\"}}}}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"turn/completed\",\"params\":{{\"turn\":{{\"id\":\"turn-1\",\"status\":\"completed\"}}}}}}' ;;\nesac\ndone\n",
            log_path.display()
        ),
    );

    let backend =
        CodexAppServerBackend::with_binary_path_for_tests(script_path.display().to_string());
    let _ = backend
        .generate_with_events(
            &ModelRequest {
                model: "gpt-5.4".to_string(),
                provider: Some("codex".to_string()),
                model_options: [("reasoning-effort".to_string(), "high".to_string())]
                    .into_iter()
                    .collect(),
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

    let log = fs::read_to_string(&log_path).unwrap();
    let turn_start_line = log
        .lines()
        .find(|line| line.contains("\"method\":\"turn/start\""))
        .expect("turn/start call should appear in log");
    assert!(
        turn_start_line.contains("\"effort\":\"high\""),
        "reasoning effort missing from turn/start params: {turn_start_line}"
    );
}

#[test]
#[cfg(unix)]
fn codex_backend_env_override_sets_reasoning_effort() {
    let _guard = lock_codex_backend_test();
    let _env = TestEnvOverride::set("DISPATCH_REASONING_EFFORT", Some("low"));

    let dir = tempdir().unwrap();
    let log_path = dir.path().join("codex.log");
    let script_path = dir.path().join("codex-app-server");
    write_executable_script(
        &script_path,
        &format!(
            "#!/bin/sh\nLOG='{}'\nwhile IFS= read -r line; do\nprintf '%s\\n' \"$line\" >> \"$LOG\"\ncase \"$line\" in\n*'\"method\":\"initialize\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{}}}}' ;;\n*'\"method\":\"initialized\"'*) : ;;\n*'\"method\":\"model/list\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"data\":[{{\"id\":\"gpt-5.4\",\"model\":\"gpt-5.4\",\"displayName\":\"GPT-5.4\",\"description\":\"\",\"supportedReasoningEfforts\":[{{\"reasoningEffort\":\"low\",\"description\":\"\"}},{{\"reasoningEffort\":\"medium\",\"description\":\"\"}},{{\"reasoningEffort\":\"high\",\"description\":\"\"}}],\"defaultReasoningEffort\":\"medium\",\"inputModalities\":[\"text\"],\"supportsPersonality\":false,\"isDefault\":true}}]}}}}' ;;\n*'\"method\":\"thread/start\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{{\"thread\":{{\"id\":\"thread-new\"}}}}}}' ;;\n*'\"method\":\"turn/start\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{{\"turn\":{{\"id\":\"turn-1\"}}}}}}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"item/agentMessage/delta\",\"params\":{{\"delta\":\"ok\"}}}}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"turn/completed\",\"params\":{{\"turn\":{{\"id\":\"turn-1\",\"status\":\"completed\"}}}}}}' ;;\nesac\ndone\n",
            log_path.display()
        ),
    );

    let backend =
        CodexAppServerBackend::with_binary_path_for_tests(script_path.display().to_string());
    let result = backend.generate_with_events(
        &ModelRequest {
            model: "gpt-5.4".to_string(),
            provider: Some("codex".to_string()),
            // No --reasoning-effort in model_options; env var should supply it.
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

    result.unwrap();

    let log = fs::read_to_string(&log_path).unwrap();
    let turn_start_line = log
        .lines()
        .find(|line| line.contains("\"method\":\"turn/start\""))
        .expect("turn/start call should appear in log");
    assert!(
        turn_start_line.contains("\"effort\":\"low\""),
        "env-supplied reasoning effort missing from turn/start params: {turn_start_line}"
    );
}

#[test]
#[cfg(unix)]
fn codex_backend_model_option_takes_precedence_over_reasoning_effort_env() {
    let _guard = lock_codex_backend_test();
    let _env = TestEnvOverride::set("DISPATCH_REASONING_EFFORT", Some("low"));

    let dir = tempdir().unwrap();
    let log_path = dir.path().join("codex.log");
    let script_path = dir.path().join("codex-app-server");
    write_executable_script(
        &script_path,
        &format!(
            "#!/bin/sh\nLOG='{}'\nwhile IFS= read -r line; do\nprintf '%s\\n' \"$line\" >> \"$LOG\"\ncase \"$line\" in\n*'\"method\":\"initialize\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{}}}}' ;;\n*'\"method\":\"initialized\"'*) : ;;\n*'\"method\":\"model/list\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"data\":[{{\"id\":\"gpt-5.4\",\"model\":\"gpt-5.4\",\"displayName\":\"GPT-5.4\",\"description\":\"\",\"supportedReasoningEfforts\":[{{\"reasoningEffort\":\"low\",\"description\":\"\"}},{{\"reasoningEffort\":\"medium\",\"description\":\"\"}},{{\"reasoningEffort\":\"high\",\"description\":\"\"}}],\"defaultReasoningEffort\":\"medium\",\"inputModalities\":[\"text\"],\"supportsPersonality\":false,\"isDefault\":true}}]}}}}' ;;\n*'\"method\":\"thread/start\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{{\"thread\":{{\"id\":\"thread-new\"}}}}}}' ;;\n*'\"method\":\"turn/start\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{{\"turn\":{{\"id\":\"turn-1\"}}}}}}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"item/agentMessage/delta\",\"params\":{{\"delta\":\"ok\"}}}}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"turn/completed\",\"params\":{{\"turn\":{{\"id\":\"turn-1\",\"status\":\"completed\"}}}}}}' ;;\nesac\ndone\n",
            log_path.display()
        ),
    );

    let backend =
        CodexAppServerBackend::with_binary_path_for_tests(script_path.display().to_string());
    let result = backend.generate_with_events(
        &ModelRequest {
            model: "gpt-5.4".to_string(),
            provider: Some("codex".to_string()),
            // Parcel option is "high"; env var is "low" - parcel wins.
            model_options: [("reasoning-effort".to_string(), "high".to_string())]
                .into_iter()
                .collect(),
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

    result.unwrap();

    let log = fs::read_to_string(&log_path).unwrap();
    let turn_start_line = log
        .lines()
        .find(|line| line.contains("\"method\":\"turn/start\""))
        .expect("turn/start call should appear in log");
    assert!(
        turn_start_line.contains("\"effort\":\"high\""),
        "parcel model option should override env var: {turn_start_line}"
    );
}

#[test]
#[cfg(unix)]
fn codex_backend_omits_reasoning_effort_when_unset() {
    let _guard = lock_codex_backend_test();
    let _env = TestEnvOverride::set("DISPATCH_REASONING_EFFORT", None);
    let dir = tempdir().unwrap();
    let log_path = dir.path().join("codex.log");
    let script_path = dir.path().join("codex-app-server");
    write_executable_script(
        &script_path,
        &format!(
            "#!/bin/sh\nLOG='{}'\nwhile IFS= read -r line; do\nprintf '%s\\n' \"$line\" >> \"$LOG\"\ncase \"$line\" in\n*'\"method\":\"initialize\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{}}}}' ;;\n*'\"method\":\"initialized\"'*) : ;;\n*'\"method\":\"thread/start\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"thread\":{{\"id\":\"thread-new\"}}}}}}' ;;\n*'\"method\":\"turn/start\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{{\"turn\":{{\"id\":\"turn-1\"}}}}}}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"item/agentMessage/delta\",\"params\":{{\"delta\":\"ok\"}}}}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"turn/completed\",\"params\":{{\"turn\":{{\"id\":\"turn-1\",\"status\":\"completed\"}}}}}}' ;;\nesac\ndone\n",
            log_path.display()
        ),
    );

    let backend =
        CodexAppServerBackend::with_binary_path_for_tests(script_path.display().to_string());
    let _ = backend
        .generate_with_events(
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
        )
        .unwrap();

    let log = fs::read_to_string(&log_path).unwrap();
    assert!(
        !log.contains("\"method\":\"model/list\""),
        "model/list should not be called when no reasoning effort override is set: {log}"
    );
    let turn_start_line = log
        .lines()
        .find(|line| line.contains("\"method\":\"turn/start\""))
        .expect("turn/start call should appear in log");
    assert!(
        !turn_start_line.contains("\"effort\":"),
        "turn/start should omit effort when no override is set: {turn_start_line}"
    );
}

#[test]
#[cfg(unix)]
fn codex_backend_rejects_unsupported_reasoning_effort() {
    let _guard = lock_codex_backend_test();
    let dir = tempdir().unwrap();
    let log_path = dir.path().join("codex.log");
    let script_path = dir.path().join("codex-app-server");
    write_executable_script(
        &script_path,
        &format!(
            "#!/bin/sh\nLOG='{}'\nwhile IFS= read -r line; do\nprintf '%s\\n' \"$line\" >> \"$LOG\"\ncase \"$line\" in\n*'\"method\":\"initialize\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{}}}}' ;;\n*'\"method\":\"initialized\"'*) : ;;\n*'\"method\":\"model/list\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"data\":[{{\"id\":\"gpt-5.4\",\"model\":\"gpt-5.4\",\"displayName\":\"GPT-5.4\",\"description\":\"\",\"supportedReasoningEfforts\":[{{\"reasoningEffort\":\"low\",\"description\":\"\"}},{{\"reasoningEffort\":\"medium\",\"description\":\"\"}}],\"defaultReasoningEffort\":\"medium\",\"inputModalities\":[\"text\"],\"supportsPersonality\":false,\"isDefault\":true}}]}}}}' ;;\n*'\"method\":\"thread/start\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{{\"thread\":{{\"id\":\"thread-new\"}}}}}}' ;;\n*'\"method\":\"turn/start\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{{\"turn\":{{\"id\":\"turn-1\"}}}}}}' ;;\nesac\ndone\n",
            log_path.display()
        ),
    );

    let backend =
        CodexAppServerBackend::with_binary_path_for_tests(script_path.display().to_string());
    let error = backend
        .generate_with_events(
            &ModelRequest {
                model: "gpt-5.4".to_string(),
                provider: Some("codex".to_string()),
                model_options: [("reasoning-effort".to_string(), "high".to_string())]
                    .into_iter()
                    .collect(),
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
        .unwrap_err();

    let message = error.to_string();
    assert!(
        message.contains("does not support reasoning effort `high`"),
        "unexpected unsupported-effort error: {message}"
    );

    let log = fs::read_to_string(&log_path).unwrap();
    let model_list_line = log
        .lines()
        .find(|line| line.contains("\"method\":\"model/list\""))
        .expect("model/list should appear in log");
    assert!(
        model_list_line.contains("\"includeHidden\":true"),
        "model/list should request hidden models during effort validation: {model_list_line}"
    );
    assert!(
        !log.contains("\"method\":\"thread/start\""),
        "thread/start should not run after reasoning effort validation fails: {log}"
    );
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
                model_options: Default::default(),
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
                model_options: Default::default(),
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
    let dir = tempdir().unwrap();
    let expected_home = dir.path().join("codex-home");
    fs::create_dir_all(&expected_home).unwrap();
    let _env = TestEnvOverride::set("CODEX_HOME", Some(&expected_home.to_string_lossy()));

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

    result.unwrap();

    let logged_home = fs::read_to_string(&env_log_path).unwrap();
    assert_eq!(logged_home.trim(), expected_home.display().to_string());
}

#[test]
#[cfg(unix)]
fn codex_backend_forwards_rollout_path_to_thread_resume() {
    // When a previous turn returned a rollout_path in the thread state, the next
    // thread/resume call must include "path" in its params so the Codex server can
    // load history from the rollout file rather than relying on thread ID alone.
    let _guard = lock_codex_backend_test();
    let dir = tempdir().unwrap();
    let log_path = dir.path().join("codex.log");
    let script_path = dir.path().join("codex-app-server");
    // thread/start returns a thread with both id and path.
    write_executable_script(
        &script_path,
        &format!(
            "#!/bin/sh\nLOG='{}'\nwhile IFS= read -r line; do\nprintf '%s\\n' \"$line\" >> \"$LOG\"\ncase \"$line\" in\n*'\"method\":\"initialize\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{}}}}' ;;\n*'\"method\":\"initialized\"'*) : ;;\n*'\"method\":\"thread/start\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"thread\":{{\"id\":\"thread-with-path\",\"path\":\"/tmp/rollout.json\"}}}}}}' ;;\n*'\"method\":\"thread/resume\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"thread\":{{\"id\":\"thread-resumed\"}}}}}}' ;;\n*'\"method\":\"turn/start\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{{\"turn\":{{\"id\":\"turn-1\"}}}}}}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"item/agentMessage/delta\",\"params\":{{\"delta\":\"ok\"}}}}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"turn/completed\",\"params\":{{\"turn\":{{\"id\":\"turn-1\",\"status\":\"completed\"}}}}}}' ;;\nesac\ndone\n",
            log_path.display()
        ),
    );

    let backend =
        CodexAppServerBackend::with_binary_path_for_tests(script_path.display().to_string());

    // First turn: start a thread - response includes a rollout path.
    let first = backend
        .generate_with_events(
            &ModelRequest {
                model: "gpt-5.4".to_string(),
                provider: Some("codex".to_string()),
                model_options: Default::default(),
                llm_timeout_ms: None,
                context_token_limit: None,
                tool_call_limit: None,
                tool_output_limit: None,
                working_directory: Some(dir.path().display().to_string()),
                instructions: String::new(),
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
    let first_state = match first {
        ModelGeneration::Reply(ref reply) => reply
            .response_id
            .clone()
            .expect("first turn should have a response_id"),
        other => panic!("expected Reply, got {other:?}"),
    };
    assert!(
        first_state.contains("/tmp/rollout.json"),
        "state: {first_state}"
    );

    let _ = backend
        .generate_with_events(
            &ModelRequest {
                model: "gpt-5.4".to_string(),
                provider: Some("codex".to_string()),
                model_options: Default::default(),
                llm_timeout_ms: None,
                context_token_limit: None,
                tool_call_limit: None,
                tool_output_limit: None,
                working_directory: Some(dir.path().display().to_string()),
                instructions: String::new(),
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

    let log = fs::read_to_string(&log_path).unwrap();
    let resume_line = log
        .lines()
        .find(|line| line.contains("\"method\":\"thread/resume\""))
        .expect("thread/resume call should appear in log");
    assert!(
        resume_line.contains("/tmp/rollout.json"),
        "rollout path missing from thread/resume params: {resume_line}"
    );
}
