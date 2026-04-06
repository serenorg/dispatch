use super::super::wasm_support::BoundedLruCache;
use super::*;

static REFERENCE_GUEST: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/dispatch-wasm-guest-reference.wasm"
));

#[test]
fn wasm_courier_accepts_component_backed_wasm_parcel() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/wasm:latest
COMPONENT components/assistant.wat
SOUL SOUL.md
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT chat
",
        &[
            ("SOUL.md", "Soul body"),
            ("components/assistant.wat", "(component)"),
            ("tools/demo.sh", "printf ok"),
        ],
    );
    let courier = WasmCourier::new().unwrap();

    futures::executor::block_on(courier.validate_parcel(&test_parcel.parcel)).unwrap();
    let inspection = futures::executor::block_on(courier.inspect(&test_parcel.parcel)).unwrap();
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    assert_eq!(inspection.courier_id, "wasm");
    assert_eq!(inspection.kind, CourierKind::Wasm);
    assert_eq!(inspection.local_tools.len(), 1);
    assert!(session.id.starts_with("wasm-"));
    assert_eq!(session.parcel_digest, test_parcel.parcel.config.digest);
    assert_eq!(session.backend_state, None);

    let prompt = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session: session.clone(),
            operation: CourierOperation::ResolvePrompt,
        },
    ))
    .unwrap();
    assert!(matches!(
        prompt.events.first(),
        Some(CourierEvent::PromptResolved { text }) if text.contains("Soul body")
    ));

    let tools = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::ListLocalTools,
        },
    ))
    .unwrap();
    assert!(matches!(
        tools.events.first(),
        Some(CourierEvent::LocalToolsListed { tools }) if tools.len() == 1 && tools[0].alias == "demo"
    ));
}

#[test]
fn wasm_courier_executes_reference_guest_chat_with_model_and_tool_imports() {
    let test_parcel = build_test_parcel_with_binary_files(
        "\
FROM dispatch/wasm:latest
COMPONENT components/reference.wasm
SOUL SOUL.md
MODEL gpt-5-mini
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT chat
",
        &[
            ("SOUL.md", "Soul body"),
            ("tools/demo.sh", "printf 'tool-output'"),
        ],
        &[("components/reference.wasm", REFERENCE_GUEST)],
    );
    let backend = Arc::new(FakeChatBackend::with_reply("backend reply"));
    let courier = WasmCourier::with_chat_backend(backend.clone());
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let model_response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "model".to_string(),
            },
        },
    ))
    .unwrap();

    assert_eq!(model_response.session.turn_count, 1);
    let expected_model_state = format!("opened:{}:1", test_parcel.parcel.config.digest);
    assert_eq!(
        model_response.session.backend_state.as_deref(),
        Some(expected_model_state.as_str())
    );
    assert!(matches!(
        model_response.events.first(),
        Some(CourierEvent::Message { role, content })
            if role == "assistant" && content == "backend reply"
    ));
    assert_eq!(model_response.session.history.len(), 2);
    assert_eq!(model_response.session.history[0].content, "model");
    assert_eq!(model_response.session.history[1].content, "backend reply");

    let calls = backend.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].model, "gpt-5-mini");
    assert!(calls[0].instructions.contains("Soul body"));
    assert_eq!(calls[0].messages.len(), 1);
    assert_eq!(calls[0].messages[0].content, "model");
    drop(calls);

    let tool_response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session: model_response.session,
            operation: CourierOperation::Chat {
                input: "tool demo".to_string(),
            },
        },
    ))
    .unwrap();

    assert_eq!(tool_response.session.turn_count, 2);
    let expected_tool_state = format!("opened:{}:2", test_parcel.parcel.config.digest);
    assert_eq!(
        tool_response.session.backend_state.as_deref(),
        Some(expected_tool_state.as_str())
    );
    assert!(matches!(
        tool_response.events.first(),
        Some(CourierEvent::Message { role, content })
            if role == "assistant" && content.contains("tool demo ok: tool-output")
    ));
    assert_eq!(tool_response.session.history.len(), 4);
    assert_eq!(tool_response.session.history[2].content, "tool demo");
}

#[test]
fn wasm_courier_supports_direct_tool_invocation() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/wasm:latest
COMPONENT components/assistant.wat
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT chat
",
        &[
            ("components/assistant.wat", "(component)"),
            ("tools/demo.sh", "printf 'direct-tool-ok'"),
        ],
    );
    let courier = WasmCourier::new().unwrap();
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::InvokeTool {
                invocation: ToolInvocation {
                    name: "demo".to_string(),
                    input: Some("hello".to_string()),
                },
            },
        },
    ))
    .unwrap();

    assert_eq!(response.session.turn_count, 1);
    assert!(matches!(
        response.events.as_slice(),
        [
            CourierEvent::ToolCallStarted { invocation, .. },
            CourierEvent::ToolCallFinished { result },
            CourierEvent::Done
        ] if invocation.name == "demo"
            && result.tool == "demo"
            && result.exit_code == 0
            && result.stdout.contains("direct-tool-ok")
    ));
}

#[test]
fn wasm_courier_host_model_complete_uses_fallback_models() {
    let test_parcel = build_test_parcel_with_binary_files(
        "\
FROM dispatch/wasm:latest
COMPONENT components/reference.wasm
SOUL SOUL.md
MODEL primary-model
FALLBACK fallback-model
ENTRYPOINT chat
",
        &[("SOUL.md", "Soul body")],
        &[("components/reference.wasm", REFERENCE_GUEST)],
    );
    let backend = Arc::new(FakeChatBackend::with_replies(vec![
        None,
        Some(ModelReply {
            text: Some("fallback wasm reply".to_string()),
            backend: "fake".to_string(),
            response_id: None,
            tool_calls: Vec::new(),
        }),
    ]));
    let courier = WasmCourier::with_chat_backend(backend.clone());
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "model".to_string(),
            },
        },
    ))
    .unwrap();

    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::Message { role, content })
            if role == "assistant" && content == "fallback wasm reply"
    ));

    let calls = backend.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].model, "primary-model");
    assert_eq!(calls[1].model, "fallback-model");
}

#[test]
fn wasm_courier_executes_reference_guest_job_and_heartbeat() {
    let job_parcel = build_test_parcel_with_binary_files(
        "\
FROM dispatch/wasm:latest
COMPONENT components/reference.wasm
ENTRYPOINT job
",
        &[],
        &[("components/reference.wasm", REFERENCE_GUEST)],
    );
    let heartbeat_parcel = build_test_parcel_with_binary_files(
        "\
FROM dispatch/wasm:latest
COMPONENT components/reference.wasm
ENTRYPOINT heartbeat
",
        &[],
        &[("components/reference.wasm", REFERENCE_GUEST)],
    );
    let courier = WasmCourier::new().unwrap();

    let job_session =
        futures::executor::block_on(courier.open_session(&job_parcel.parcel)).unwrap();
    let job_response = futures::executor::block_on(courier.run(
        &job_parcel.parcel,
        CourierRequest {
            session: job_session,
            operation: CourierOperation::Job {
                payload: "{\"task\":\"ping\"}".to_string(),
            },
        },
    ))
    .unwrap();
    assert!(matches!(
        job_response.events.first(),
        Some(CourierEvent::Message { role, content })
            if role == "assistant" && content == "job accepted: {\"task\":\"ping\"}"
    ));
    assert_eq!(job_response.session.turn_count, 1);
    let expected_job_state = format!("opened:{}:1", job_parcel.parcel.config.digest);
    assert_eq!(
        job_response.session.backend_state.as_deref(),
        Some(expected_job_state.as_str())
    );

    let heartbeat_session =
        futures::executor::block_on(courier.open_session(&heartbeat_parcel.parcel)).unwrap();
    let heartbeat_response = futures::executor::block_on(courier.run(
        &heartbeat_parcel.parcel,
        CourierRequest {
            session: heartbeat_session,
            operation: CourierOperation::Heartbeat {
                payload: Some("tick".to_string()),
            },
        },
    ))
    .unwrap();
    assert!(matches!(
        heartbeat_response.events.first(),
        Some(CourierEvent::TextDelta { content }) if content == "heartbeat:tick"
    ));
    assert_eq!(heartbeat_response.session.turn_count, 1);
    let expected_heartbeat_state = format!("opened:{}:1", heartbeat_parcel.parcel.config.digest);
    assert_eq!(
        heartbeat_response.session.backend_state.as_deref(),
        Some(expected_heartbeat_state.as_str())
    );
}

#[test]
fn wasm_courier_reference_guest_memory_persists_across_sessions() {
    let test_parcel = build_test_parcel_with_binary_files(
        "\
FROM dispatch/wasm:latest
COMPONENT components/reference.wasm
MOUNT SESSION sqlite
MOUNT MEMORY sqlite
ENTRYPOINT chat
",
        &[],
        &[("components/reference.wasm", REFERENCE_GUEST)],
    );
    let courier = WasmCourier::new().unwrap();

    let first_session =
        futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();
    let first_response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session: first_session,
            operation: CourierOperation::Chat {
                input: "remember profile:name Christian".to_string(),
            },
        },
    ))
    .unwrap();
    assert!(matches!(
        first_response.events.first(),
        Some(CourierEvent::Message { content, .. }) if content == "remembered profile:name"
    ));

    let second_session =
        futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();
    let second_response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session: second_session,
            operation: CourierOperation::Chat {
                input: "recall profile:name".to_string(),
            },
        },
    ))
    .unwrap();
    assert!(matches!(
        second_response.events.first(),
        Some(CourierEvent::Message { content, .. }) if content == "profile:name = Christian"
    ));
}

#[test]
fn wasm_courier_reference_guest_rejects_memory_ops_without_memory_mount() {
    let test_parcel = build_test_parcel_with_binary_files(
        "\
FROM dispatch/wasm:latest
COMPONENT components/reference.wasm
MOUNT MEMORY none
ENTRYPOINT chat
",
        &[],
        &[("components/reference.wasm", REFERENCE_GUEST)],
    );
    let courier = WasmCourier::new().unwrap();
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let error = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "remember profile:name Christian".to_string(),
            },
        },
    ))
    .unwrap_err();

    assert!(matches!(
        error,
        CourierError::WasmGuest { message, .. }
            if message.contains("memory put failed")
                && message.contains("does not declare a usable memory mount")
    ));
}

#[test]
fn bounded_lru_cache_evicts_least_recently_used_entries() {
    let mut cache = BoundedLruCache::new(2);
    cache.insert("a".to_string(), "one".to_string());
    cache.insert("b".to_string(), "two".to_string());

    assert_eq!(cache.get("a").as_deref(), Some("one"));

    cache.insert("c".to_string(), "three".to_string());

    assert_eq!(cache.get("a").as_deref(), Some("one"));
    assert_eq!(cache.get("b"), None);
    assert_eq!(cache.get("c").as_deref(), Some("three"));
    assert_eq!(cache.keys(), vec!["a".to_string(), "c".to_string()]);
}
