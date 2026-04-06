use super::*;

#[test]
fn docker_courier_accepts_docker_image_reference() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/docker:latest
ENTRYPOINT job
",
        &[],
    );
    let courier = DockerCourier::default();

    futures::executor::block_on(courier.validate_parcel(&test_parcel.parcel)).unwrap();
    let inspection = futures::executor::block_on(courier.inspect(&test_parcel.parcel)).unwrap();
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    assert_eq!(inspection.courier_id, "docker");
    assert_eq!(inspection.kind, CourierKind::Docker);
    assert_eq!(session.entrypoint.as_deref(), Some("job"));
    assert!(session.id.starts_with("docker-"));
}

#[test]
fn docker_courier_rejects_native_image_reference() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
ENTRYPOINT chat
",
        &[],
    );
    let courier = DockerCourier::default();

    let error =
        futures::executor::block_on(courier.validate_parcel(&test_parcel.parcel)).unwrap_err();

    assert!(matches!(
        error,
        CourierError::IncompatibleCourier { courier, parcel_courier, .. }
            if courier == "docker" && parcel_courier == "dispatch/native:latest"
    ));
}

#[test]
fn docker_courier_can_resolve_prompt_and_list_tools() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/docker:latest
SOUL SOUL.md
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT chat
",
        &[("SOUL.md", "Soul body"), ("tools/demo.sh", "printf ok")],
    );
    let courier = DockerCourier::default();
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let prompt = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session: session.clone(),
            operation: CourierOperation::ResolvePrompt,
        },
    ))
    .unwrap();
    let tools = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::ListLocalTools,
        },
    ))
    .unwrap();

    assert!(matches!(
        prompt.events.first(),
        Some(CourierEvent::PromptResolved { text }) if text.contains("Soul body")
    ));
    assert!(matches!(
        tools.events.first(),
        Some(CourierEvent::LocalToolsListed { tools }) if tools.len() == 1 && tools[0].alias == "demo"
    ));
}

#[test]
fn docker_courier_chat_executes_local_reply_and_records_history() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/docker:latest
ENTRYPOINT chat
",
        &[],
    );
    let courier = DockerCourier::default();
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "hello".to_string(),
            },
        },
    ))
    .unwrap();

    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::Message { content, .. })
            if content.contains("Docker chat reply")
    ));
    assert_eq!(response.session.turn_count, 1);
    assert_eq!(response.session.history.len(), 2);
}

#[test]
#[cfg(unix)]
fn docker_courier_invokes_local_tools_via_docker_cli() {
    let dir = tempdir().unwrap();
    let docker_bin = dir.path().join("docker");
    fs::write(
        &docker_bin,
        "\
#!/bin/sh
index=1
for arg in \"$@\"; do
printf 'arg%d=%s\\n' \"$index\" \"$arg\"
index=$((index + 1))
done
cat >/dev/null
",
    )
    .unwrap();
    fs::set_permissions(&docker_bin, fs::Permissions::from_mode(0o755)).unwrap();

    let test_parcel = build_test_parcel(
        "\
FROM dispatch/docker:latest
TOOL LOCAL tools/demo.sh AS demo
ENV CAST_VISIBLE_ENV=visible
ENTRYPOINT job
",
        &[("tools/demo.sh", "printf ok")],
    );
    let courier = DockerCourier::new(&docker_bin, "python:3.13-alpine");
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::InvokeTool {
                invocation: ToolInvocation {
                    name: "demo".to_string(),
                    input: Some("{\"ping\":true}".to_string()),
                },
            },
        },
    ))
    .unwrap();

    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::ToolCallStarted { command, .. }) if command == "sh"
    ));
    let CourierEvent::ToolCallFinished { result } = &response.events[1] else {
        panic!("expected tool call finished event");
    };
    assert_eq!(result.tool, "demo");
    assert_eq!(result.command, "sh");
    assert!(result.stdout.contains("arg1=run"));
    assert!(result.stdout.contains("arg2=--rm"));
    assert!(result.stdout.contains("arg3=-i"));
    assert!(result.stdout.contains("arg4=--workdir"));
    assert!(result.stdout.contains("arg5=/workspace/context"));
    assert!(result.stdout.contains("CAST_VISIBLE_ENV=visible"));
    assert!(result.stdout.contains("TOOL_INPUT={\"ping\":true}"));
    assert!(result.stdout.contains("python:3.13-alpine"));
    assert!(result.stdout.contains("tools/demo.sh"));
    assert!(matches!(response.events.last(), Some(CourierEvent::Done)));
}

#[test]
#[cfg(unix)]
fn docker_courier_enforces_tool_timeout_for_local_tools() {
    let dir = tempdir().unwrap();
    let docker_bin = dir.path().join("docker");
    fs::write(&docker_bin, "#!/bin/sh\nsleep 0.2\ncat >/dev/null\n").unwrap();
    fs::set_permissions(&docker_bin, fs::Permissions::from_mode(0o755)).unwrap();

    let test_parcel = build_test_parcel(
        "\
FROM dispatch/docker:latest
TIMEOUT TOOL 50ms
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT job
",
        &[("tools/demo.sh", "printf ok")],
    );
    let courier = DockerCourier::new(&docker_bin, "python:3.13-alpine");
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let error = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::InvokeTool {
                invocation: ToolInvocation {
                    name: "demo".to_string(),
                    input: None,
                },
            },
        },
    ))
    .unwrap_err();

    assert!(matches!(
        error,
        CourierError::ToolTimedOut { ref tool, ref timeout }
            if tool == "demo" && timeout == "TOOL"
    ));
}

#[test]
#[cfg(unix)]
fn docker_courier_chat_executes_model_tool_calls_via_docker_cli() {
    let dir = tempdir().unwrap();
    let docker_bin = dir.path().join("docker");
    fs::write(
        &docker_bin,
        "#!/bin/sh\nprintf 'docker-tool-output'\ncat >/dev/null\n",
    )
    .unwrap();
    fs::set_permissions(&docker_bin, fs::Permissions::from_mode(0o755)).unwrap();

    let test_parcel = build_test_parcel(
        "\
FROM dispatch/docker:latest
MODEL gpt-5-mini
TOOL LOCAL tools/demo.sh AS demo SCHEMA schemas/demo.json
ENTRYPOINT chat
",
        &[
            ("tools/demo.sh", "printf ok"),
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
            text: Some("docker final answer".to_string()),
            backend: "fake".to_string(),
            response_id: Some("resp_2".to_string()),
            tool_calls: Vec::new(),
        }),
    ]));
    let courier =
        DockerCourier::new(&docker_bin, "python:3.13-alpine").with_chat_backend(backend.clone());
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
    assert_eq!(calls[1].tool_outputs.len(), 1);
    assert!(
        calls[1].tool_outputs[0]
            .output
            .contains("docker-tool-output")
    );
    drop(calls);

    assert!(matches!(
        response.events.get(1),
        Some(CourierEvent::ToolCallFinished { result })
            if result.tool == "demo" && result.stdout.contains("docker-tool-output")
    ));
    assert!(matches!(
        response.events.iter().rev().nth(1),
        Some(CourierEvent::Message { content, .. }) if content == "docker final answer"
    ));
}
