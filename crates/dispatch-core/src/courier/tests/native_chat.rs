use super::*;

#[test]
fn native_courier_chat_uses_backend_when_model_is_declared() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
MODEL gpt-5-mini
ENTRYPOINT chat
",
        &[],
    );
    let backend = Arc::new(FakeChatBackend::with_reply("backend reply"));
    let courier = NativeCourier::with_chat_backend(backend.clone());
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "hello backend".to_string(),
            },
        },
    ))
    .unwrap();

    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::Message { content, .. }) if content == "backend reply"
    ));

    let calls = backend.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].model, "gpt-5-mini");
    assert_eq!(calls[0].messages[0].content, "hello backend");
}

#[test]
fn native_courier_chat_emits_first_class_channel_reply_for_tagged_envelope() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
MODEL gpt-5-mini
ENTRYPOINT chat
",
        &[],
    );
    let structured_reply = serde_json::json!({
        "kind": "channel_reply",
        "content": "backend reply",
        "content_type": "text/plain",
        "attachments": [{
            "name": "report.txt",
            "mime_type": "text/plain",
            "data_base64": "aGVsbG8="
        }],
        "metadata": {
            "custom": "value"
        }
    })
    .to_string();
    let backend = Arc::new(FakeChatBackend::with_reply(&structured_reply));
    let courier = NativeCourier::with_chat_backend(backend);
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "hello backend".to_string(),
            },
        },
    ))
    .unwrap();

    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::ChannelReply { message })
            if message.content == "backend reply" && message.attachments.len() == 1
    ));
    assert!(
        !response.events.iter().any(
            |event| matches!(event, CourierEvent::Message { role, .. } if role == "assistant")
        )
    );
    assert_eq!(
        response.session.history.last(),
        Some(&ConversationMessage {
            role: "assistant".to_string(),
            content: "backend reply".to_string(),
        })
    );
}

#[test]
fn native_courier_caps_llm_timeout_by_remaining_run_budget() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TIMEOUT RUN 100ms
TIMEOUT LLM 5s
ENTRYPOINT chat
",
        &[],
    );
    let backend = Arc::new(FakeChatBackend::with_reply("backend reply"));
    let courier = NativeCourier::with_chat_backend(backend.clone());
    let mut session =
        futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();
    session.elapsed_ms = 60;

    let _response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "hello backend".to_string(),
            },
        },
    ))
    .unwrap();

    let calls = backend.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let timeout_ms = calls[0].llm_timeout_ms.expect("expected llm timeout");
    assert!((1..=40).contains(&timeout_ms));
}

#[test]
fn native_courier_chat_streams_text_delta_without_duplicate_message() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
MODEL gpt-5-mini
ENTRYPOINT chat
",
        &[],
    );
    let backend = Arc::new(FakeChatBackend::with_streaming_reply(
        "streamed reply",
        vec!["streamed ", "reply"],
    ));
    let courier = NativeCourier::with_chat_backend(backend);
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "stream please".to_string(),
            },
        },
    ))
    .unwrap();

    assert_eq!(
        response
            .events
            .iter()
            .filter_map(|event| match event {
                CourierEvent::TextDelta { content } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["streamed ", "reply"]
    );
    assert!(matches!(response.events.last(), Some(CourierEvent::Done)));
    assert!(!response.events.iter().any(|event| matches!(
        event,
        CourierEvent::Message { role, content }
            if role == "assistant" && content == "streamed reply"
    )));
    assert_eq!(
        response.session.history.last(),
        Some(&ConversationMessage {
            role: "assistant".to_string(),
            content: "streamed reply".to_string(),
        })
    );
}

#[test]
#[cfg(unix)]
fn native_courier_codex_backend_resumes_threads_and_denies_app_server_approvals() {
    let _guard = lock_codex_backend_test();
    let dir = tempdir().unwrap();
    let log_path = dir.path().join("codex.log");
    let script_path = dir.path().join("codex-app-server");
    write_executable_script(
        &script_path,
        &format!(
            "#!/bin/sh\nLOG='{}'\nwhile IFS= read -r line; do\nprintf '%s\\n' \"$line\" >> \"$LOG\"\ncase \"$line\" in\n*'\"method\":\"initialize\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{}}}}' ;;\n*'\"method\":\"initialized\"'*) : ;;\n*'\"method\":\"thread/start\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"thread\":{{\"id\":\"thread-new\"}}}}}}' ;;\n*'\"method\":\"thread/resume\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"thread\":{{\"id\":\"thread-new\"}}}}}}' ;;\n*'\"method\":\"turn/start\"'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{{\"turn\":{{\"id\":\"turn-1\"}}}}}}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":90,\"method\":\"item/commandExecution/requestApproval\",\"params\":{{\"command\":\"ls\"}}}}'\nIFS= read -r approval\nprintf '%s\\n' \"$approval\" >> \"$LOG\"\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"item/agentMessage/delta\",\"params\":{{\"delta\":\"codex reply\"}}}}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"turn/completed\",\"params\":{{\"turn\":{{\"id\":\"turn-1\",\"status\":\"completed\"}}}}}}' ;;\nesac\ndone\n",
            log_path.display()
        ),
    );

    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
MODEL gpt-5.4 PROVIDER codex
ENTRYPOINT chat
",
        &[],
    );
    let backend = Arc::new(CodexAppServerBackend::with_binary_path_for_tests(
        script_path.display().to_string(),
    ));
    let courier = NativeCourier::with_chat_backend(backend);
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let first = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "hello".to_string(),
            },
        },
    ))
    .unwrap();

    let first_state = first
        .session
        .backend_state
        .clone()
        .expect("codex backend should persist state");
    assert!(first_state.contains("thread-new"));
    assert!(first.events.iter().any(|event| matches!(
        event,
        CourierEvent::TextDelta { content } if content == "codex reply"
    )));

    let second = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session: first.session,
            operation: CourierOperation::Chat {
                input: "follow up".to_string(),
            },
        },
    ))
    .unwrap();

    let second_state = second
        .session
        .backend_state
        .clone()
        .expect("codex backend should persist resumed state");
    assert!(second_state.contains("thread-new"));
    assert!(
        !second
            .events
            .iter()
            .any(|event| matches!(event, CourierEvent::BackendFallback { .. }))
    );
    let log = fs::read_to_string(&log_path).unwrap();
    let thread_start_count = log.matches("\"method\":\"thread/start\"").count();
    assert_eq!(thread_start_count, 1);
    assert!(log.contains("\"method\":\"thread/resume\""));
    assert!(log.contains("\"persistExtendedHistory\":true"));
    assert!(log.contains("\"decision\":\"decline\""));
}

#[test]
fn native_courier_chat_executes_tool_calls_then_continues_model_turn() {
    let tool_path = test_tool_relative_path("demo");
    let tool_body = test_tool_print_body("tool-output");
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL LOCAL {tool_path} AS demo
ENTRYPOINT chat
"
        ),
        &[(tool_path.as_str(), tool_body.as_str())],
    );
    let backend = Arc::new(FakeChatBackend::with_replies(vec![
        Some(ModelReply {
            text: None,
            backend: "fake".to_string(),
            response_id: Some("resp_1".to_string()),
            tool_calls: vec![ModelToolCall {
                call_id: "call_1".to_string(),
                name: "demo".to_string(),
                input: "{\"query\":\"ping\"}".to_string(),
                kind: ModelToolKind::Custom,
            }],
        }),
        Some(ModelReply {
            text: Some("final answer".to_string()),
            backend: "fake".to_string(),
            response_id: Some("resp_2".to_string()),
            tool_calls: Vec::new(),
        }),
    ]));
    let courier = NativeCourier::with_chat_backend(backend.clone());
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "use the tool".to_string(),
            },
        },
    ))
    .unwrap();

    let calls = backend.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].tools.len(), 1);
    assert_eq!(calls[0].tools[0].name, "demo");
    assert_eq!(calls[1].previous_response_id.as_deref(), Some("resp_1"));
    assert_eq!(calls[1].messages.len(), 0);
    assert_eq!(calls[1].tool_outputs.len(), 1);
    assert_eq!(calls[1].tool_outputs[0].call_id, "call_1");
    assert_eq!(calls[1].tool_outputs[0].kind, ModelToolKind::Custom);
    assert!(calls[1].tool_outputs[0].output.contains("tool-output"));
    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::ToolCallStarted { invocation, .. })
            if invocation.name == "demo"
                && invocation.input.as_deref() == Some("{\"query\":\"ping\"}")
    ));
    assert!(matches!(
        response.events.get(1),
        Some(CourierEvent::ToolCallFinished { result })
            if result.tool == "demo" && result.stdout.contains("tool-output")
    ));
    assert!(matches!(
        response.events.get(2),
        Some(CourierEvent::Message { content, .. }) if content == "final answer"
    ));
    assert!(matches!(response.events.last(), Some(CourierEvent::Done)));
}

#[test]
fn native_courier_chat_reconstructs_followup_without_response_threading() {
    let tool_path = test_tool_relative_path("demo");
    let tool_body = test_tool_print_body("tool-output");
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL LOCAL {tool_path} AS demo
ENTRYPOINT chat
"
        ),
        &[(tool_path.as_str(), tool_body.as_str())],
    );
    let backend = Arc::new(FakeChatBackend::with_replies_without_previous_response_id(
        vec![
            Some(ModelReply {
                text: None,
                backend: "fake".to_string(),
                response_id: Some("resp_1".to_string()),
                tool_calls: vec![ModelToolCall {
                    call_id: "call_1".to_string(),
                    name: "demo".to_string(),
                    input: "{\"query\":\"ping\"}".to_string(),
                    kind: ModelToolKind::Custom,
                }],
            }),
            Some(ModelReply {
                text: Some("final answer".to_string()),
                backend: "fake".to_string(),
                response_id: Some("resp_2".to_string()),
                tool_calls: Vec::new(),
            }),
        ],
    ));
    let courier = NativeCourier::with_chat_backend(backend.clone());
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "use the tool".to_string(),
            },
        },
    ))
    .unwrap();

    let calls = backend.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].previous_response_id.is_none());
    assert_eq!(calls[1].tool_outputs.len(), 1);
    assert_eq!(calls[1].pending_tool_calls.len(), 1);
    assert_eq!(calls[1].messages.len(), 1);
    assert_eq!(calls[1].messages[0].role, "user");
    assert_eq!(calls[1].messages[0].content, "use the tool");
    assert_eq!(calls[1].pending_tool_calls[0].call_id, "call_1");
    assert_eq!(calls[1].pending_tool_calls[0].name, "demo");
    assert!(calls[1].tool_outputs[0].output.contains("tool-output"));
    drop(calls);

    assert!(matches!(
        response.events.get(2),
        Some(CourierEvent::Message { content, .. }) if content == "final answer"
    ));
    assert!(matches!(response.events.last(), Some(CourierEvent::Done)));
}

#[test]
fn native_courier_chat_falls_back_when_backend_is_unavailable() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
MODEL gpt-5-mini
ENTRYPOINT chat
",
        &[],
    );
    let backend = Arc::new(FakeChatBackend::default());
    let courier = NativeCourier::with_chat_backend(backend.clone());
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "fallback please".to_string(),
            },
        },
    ))
    .unwrap();

    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::BackendFallback { backend, error })
            if backend == "fake" && error.contains("not configured")
    ));
    assert!(matches!(
        response.events.get(1),
        Some(CourierEvent::Message { content, .. })
            if content.contains("Native chat reply")
    ));
    assert_eq!(backend.calls.lock().unwrap().len(), 1);
}

#[test]
fn native_courier_chat_emits_backend_fallback_event_on_backend_error() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
MODEL gpt-5-mini
ENTRYPOINT chat
",
        &[],
    );
    let backend = Arc::new(FakeChatBackend::with_error("http status: 401"));
    let courier = NativeCourier::with_chat_backend(backend.clone());
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "fallback on error".to_string(),
            },
        },
    ))
    .unwrap();

    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::BackendFallback { backend, error })
            if backend == "fake" && error.contains("401")
    ));
    assert!(matches!(
        response.events.get(1),
        Some(CourierEvent::Message { content, .. })
            if content.contains("Native chat reply")
    ));
    assert_eq!(backend.calls.lock().unwrap().len(), 1);
}

#[test]
fn native_courier_chat_uses_fallback_model_after_primary_backend_error() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
MODEL primary-model
FALLBACK fallback-model
ENTRYPOINT chat
",
        &[],
    );
    let backend = Arc::new(FakeChatBackend {
        replies: Mutex::new(vec![
            Err("temporary backend failure".to_string()),
            Ok(ModelGeneration::Reply(ModelReply {
                text: Some("fallback answer".to_string()),
                backend: "fake".to_string(),
                response_id: None,
                tool_calls: Vec::new(),
            })),
        ]),
        streams: Mutex::new(vec![Vec::new(), Vec::new()]),
        calls: Mutex::new(Vec::new()),
        supports_previous_response_id: false,
    });
    let courier = NativeCourier::with_chat_backend(backend.clone());
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "fallback please".to_string(),
            },
        },
    ))
    .unwrap();

    let calls = backend.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].model, "primary-model");
    assert_eq!(calls[1].model, "fallback-model");
    drop(calls);
    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::BackendFallback { backend, error })
            if backend == "fake"
                && error.contains("temporary backend failure")
                && error.contains("fallback model `fallback-model`")
    ));
    assert!(matches!(
        response.events.get(1),
        Some(CourierEvent::Message { content, .. }) if content == "fallback answer"
    ));
}

#[test]
fn native_courier_chat_emits_backend_fallback_when_tool_loop_is_exhausted() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT chat
",
        &[("tools/demo.sh", "printf 'tool-output'")],
    );
    let backend = Arc::new(FakeChatBackend::with_replies(
        (0..8)
            .map(|index| {
                Some(ModelReply {
                    text: None,
                    backend: "fake".to_string(),
                    response_id: Some(format!("resp_{index}")),
                    tool_calls: vec![ModelToolCall {
                        call_id: format!("call_{index}"),
                        name: "demo".to_string(),
                        input: "{\"query\":\"ping\"}".to_string(),
                        kind: ModelToolKind::Custom,
                    }],
                })
            })
            .collect(),
    ));
    let courier = NativeCourier::with_chat_backend(backend.clone());
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "loop forever".to_string(),
            },
        },
    ))
    .unwrap();

    assert!(response.events.iter().any(|event| {
        matches!(
            event,
            CourierEvent::BackendFallback { backend, error }
                if backend == "fake" && error.contains("tool call loop reached 8 rounds")
        )
    }));
    assert!(matches!(
        response.events.iter().rev().nth(1),
        Some(CourierEvent::Message { content, .. })
            if content.contains("Native chat reply")
    ));
    assert_eq!(backend.calls.lock().unwrap().len(), 8);
}

#[test]
fn run_local_tool_requires_approval_handler_for_confirm_policy() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
TOOL LOCAL tools/demo.sh AS demo APPROVAL confirm
ENTRYPOINT chat
",
        &[("tools/demo.sh", "printf ok")],
    );

    let error = run_local_tool(&test_parcel.parcel, "demo", Some("hello")).unwrap_err();
    assert!(matches!(error, CourierError::ApprovalRequired { ref tool } if tool == "demo"));
}

#[test]
fn native_courier_respects_configured_tool_round_limit() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
MODEL fake-model PROVIDER fake
TOOL LOCAL tools/demo.sh AS demo
LIMIT TOOL_ROUNDS 3
ENTRYPOINT chat
",
        &[("tools/demo.sh", "printf ok")],
    );

    let backend = Arc::new(FakeChatBackend::with_replies(
        (0..6)
            .map(|index| {
                Some(ModelReply {
                    text: None,
                    backend: "fake".to_string(),
                    response_id: Some(format!("resp_{index}")),
                    tool_calls: vec![ModelToolCall {
                        call_id: format!("call_{index}"),
                        name: "demo".to_string(),
                        input: "{\"query\":\"ping\"}".to_string(),
                        kind: ModelToolKind::Custom,
                    }],
                })
            })
            .collect(),
    ));
    let courier = NativeCourier::with_chat_backend(backend.clone());
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "loop forever".to_string(),
            },
        },
    ))
    .unwrap();

    assert!(response.events.iter().any(|event| {
        matches!(
            event,
            CourierEvent::BackendFallback { backend, error }
                if backend == "fake" && error.contains("tool call loop reached 3 rounds")
        )
    }));
    assert_eq!(backend.calls.lock().unwrap().len(), 3);
}

#[test]
fn run_local_tool_can_be_denied_by_approval_handler() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
TOOL LOCAL tools/demo.sh AS demo APPROVAL confirm
ENTRYPOINT chat
",
        &[("tools/demo.sh", "printf ok")],
    );

    let error = with_tool_approval_handler(
        |_| Ok(ToolApprovalDecision::Deny),
        || run_local_tool(&test_parcel.parcel, "demo", Some("hello")),
    )
    .unwrap_err();
    assert!(matches!(error, CourierError::ApprovalDenied { ref tool } if tool == "demo"));
}

#[test]
fn native_courier_chat_reports_denied_tool_calls() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL LOCAL tools/demo.sh AS demo APPROVAL confirm
ENTRYPOINT chat
",
        &[("tools/demo.sh", "printf 'tool-output'")],
    );
    let backend = Arc::new(FakeChatBackend::with_replies(vec![
        Some(ModelReply {
            text: None,
            backend: "fake".to_string(),
            response_id: None,
            tool_calls: vec![ModelToolCall {
                call_id: "call_1".to_string(),
                name: "demo".to_string(),
                input: "{\"query\":\"ping\"}".to_string(),
                kind: ModelToolKind::Custom,
            }],
        }),
        Some(ModelReply {
            text: Some("final reply".to_string()),
            backend: "fake".to_string(),
            response_id: None,
            tool_calls: Vec::new(),
        }),
    ]));
    let courier = NativeCourier::with_chat_backend(backend);
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let response = with_tool_approval_handler(
        |_| Ok(ToolApprovalDecision::Deny),
        || {
            futures::executor::block_on(courier.run(
                &test_parcel.parcel,
                CourierRequest {
                    session,
                    operation: CourierOperation::Chat {
                        input: "try the tool".to_string(),
                    },
                },
            ))
        },
    )
    .unwrap();

    assert!(matches!(
        response.events.get(1),
        Some(CourierEvent::ToolCallFinished { result })
            if result.tool == "demo"
                && result.exit_code == 126
                && result.stderr.contains("denied by APPROVAL confirm")
    ));
    assert!(matches!(
        response.events.iter().rev().nth(1),
        Some(CourierEvent::Message { content, .. }) if content == "final reply"
    ));
}

#[test]
fn native_courier_chat_executes_schema_tool_calls_as_function_outputs() {
    let tool_path = test_tool_relative_path("demo");
    let tool_body = test_tool_print_body("tool-output");
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL LOCAL {tool_path} AS demo SCHEMA schemas/demo.json
ENTRYPOINT chat
"
        ),
        &[
            (tool_path.as_str(), tool_body.as_str()),
            (
                "schemas/demo.json",
                "{\n  \"type\": \"object\",\n  \"properties\": {\n    \"query\": { \"type\": \"string\" }\n  },\n  \"required\": [\"query\"]\n}",
            ),
        ],
    );
    let backend = Arc::new(FakeChatBackend::with_replies(vec![
        Some(ModelReply {
            text: None,
            backend: "fake".to_string(),
            response_id: Some("resp_1".to_string()),
            tool_calls: vec![ModelToolCall {
                call_id: "call_1".to_string(),
                name: "demo".to_string(),
                input: "{\"query\":\"ping\"}".to_string(),
                kind: ModelToolKind::Function,
            }],
        }),
        Some(ModelReply {
            text: Some("final answer".to_string()),
            backend: "fake".to_string(),
            response_id: Some("resp_2".to_string()),
            tool_calls: Vec::new(),
        }),
    ]));
    let courier = NativeCourier::with_chat_backend(backend.clone());
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "use the function tool".to_string(),
            },
        },
    ))
    .unwrap();

    let calls = backend.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(matches!(
        calls[0].tools[0].format,
        ModelToolFormat::JsonSchema { .. }
    ));
    assert_eq!(calls[1].tool_outputs.len(), 1);
    assert_eq!(calls[1].tool_outputs[0].kind, ModelToolKind::Function);
    assert!(calls[1].tool_outputs[0].output.contains("tool-output"));
    assert!(matches!(response.events.last(), Some(CourierEvent::Done)));
}

#[test]
fn native_courier_chat_preserves_history_across_turns() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
ENTRYPOINT chat
",
        &[],
    );
    let courier = NativeCourier::default();
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let first = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "first".to_string(),
            },
        },
    ))
    .unwrap();

    let second = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session: first.session,
            operation: CourierOperation::Chat {
                input: "second".to_string(),
            },
        },
    ))
    .unwrap();

    assert_eq!(second.session.turn_count, 2);
    assert_eq!(second.session.history.len(), 4);
    assert_eq!(second.session.history[2].content, "second");
    assert!(
        second.session.history[3]
            .content
            .contains("Prior messages in session: 2")
    );
}

#[test]
fn native_courier_chat_supports_prompt_command() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
SOUL SOUL.md
ENTRYPOINT chat
",
        &[("SOUL.md", "Soul body")],
    );
    let courier = NativeCourier::default();
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "/prompt".to_string(),
            },
        },
    ))
    .unwrap();

    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::Message { content, .. }) if content.contains("# SOUL")
    ));
}

#[test]
fn native_courier_chat_tools_command_lists_builtin_tools_when_no_local_tools() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
TOOL BUILTIN memory_get
TOOL BUILTIN checkpoint_list
ENTRYPOINT chat
",
        &[],
    );
    let courier = NativeCourier::default();
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "/tools".to_string(),
            },
        },
    ))
    .unwrap();

    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::Message { content, .. })
            if content.contains("Builtin tools: memory_get, checkpoint_list")
                && !content.contains("Local tools:")
    ));
}

#[test]
fn native_courier_chat_tools_command_lists_local_and_builtin_tools_separately() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
TOOL LOCAL tools/demo.sh AS demo
TOOL BUILTIN memory_get
ENTRYPOINT chat
",
        &[("tools/demo.sh", "printf demo")],
    );
    let courier = NativeCourier::default();
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "/tools".to_string(),
            },
        },
    ))
    .unwrap();

    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::Message { content, .. })
            if content.contains("Local tools: demo")
                && content.contains("Builtin tools: memory_get")
    ));
}
