use super::*;
use rusqlite::Connection;
use serde_json::Value;

#[test]
fn native_courier_memory_sqlite_persists_across_sessions() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MOUNT SESSION sqlite
MOUNT MEMORY sqlite
ENTRYPOINT chat
",
        &[],
    );
    let courier = NativeCourier::default();
    let first_session =
        futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

    let first_response = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session: first_session,
            operation: CourierOperation::Chat {
                input: "/memory put profile:name Christian".to_string(),
            },
        },
    ))
    .unwrap();
    assert!(matches!(
        first_response.events.first(),
        Some(CourierEvent::Message { content, .. }) if content == "Stored memory profile:name"
    ));

    let second_session =
        futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
    let second_response = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session: second_session,
            operation: CourierOperation::Chat {
                input: "/memory get profile:name".to_string(),
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
fn native_courier_memory_put_reports_updates_after_first_write() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MOUNT SESSION sqlite
MOUNT MEMORY sqlite
ENTRYPOINT chat
",
        &[],
    );
    let courier = NativeCourier::default();
    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

    let first = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "/memory put profile:name Christian".to_string(),
            },
        },
    ))
    .unwrap();
    assert!(matches!(
        first.events.first(),
        Some(CourierEvent::Message { content, .. }) if content == "Stored memory profile:name"
    ));

    let second = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session: first.session,
            operation: CourierOperation::Chat {
                input: "/memory put profile:name Chris".to_string(),
            },
        },
    ))
    .unwrap();
    assert!(matches!(
        second.events.first(),
        Some(CourierEvent::Message { content, .. }) if content == "Updated memory profile:name"
    ));

    let third = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session: second.session,
            operation: CourierOperation::Chat {
                input: "/memory get profile:name".to_string(),
            },
        },
    ))
    .unwrap();
    assert!(matches!(
        third.events.first(),
        Some(CourierEvent::Message { content, .. }) if content == "profile:name = Chris"
    ));
}

#[test]
fn native_memory_list_treats_underscore_as_literal_prefix_character() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MOUNT SESSION sqlite
MOUNT MEMORY sqlite
ENTRYPOINT chat
",
        &[],
    );
    let courier = NativeCourier::default();
    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

    let session = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "/memory put default:user_1 first".to_string(),
            },
        },
    ))
    .unwrap()
    .session;
    let session = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "/memory put default:userA second".to_string(),
            },
        },
    ))
    .unwrap()
    .session;
    let response = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "/memory list default:user_".to_string(),
            },
        },
    ))
    .unwrap();

    let CourierEvent::Message { content, .. } = response.events.first().unwrap() else {
        panic!("expected message event");
    };
    assert!(content.contains("default:user_1 = first"));
    assert!(!content.contains("default:userA = second"));
}

#[test]
fn native_courier_chat_executes_builtin_memory_tools() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL BUILTIN memory_put
TOOL BUILTIN memory_get
MOUNT MEMORY sqlite
ENTRYPOINT chat
",
        &[],
    );
    let backend = Arc::new(FakeChatBackend::with_replies(vec![
        Some(ModelReply {
            text: None,
            backend: "fake".to_string(),
            response_id: Some("resp_1".to_string()),
            tool_calls: vec![ModelToolCall {
                call_id: "call_1".to_string(),
                name: "memory_put".to_string(),
                input: "{\"namespace\":\"profile\",\"key\":\"name\",\"value\":\"Christian\"}"
                    .to_string(),
                kind: ModelToolKind::Function,
            }],
        }),
        Some(ModelReply {
            text: None,
            backend: "fake".to_string(),
            response_id: Some("resp_2".to_string()),
            tool_calls: vec![ModelToolCall {
                call_id: "call_2".to_string(),
                name: "memory_get".to_string(),
                input: "{\"namespace\":\"profile\",\"key\":\"name\"}".to_string(),
                kind: ModelToolKind::Function,
            }],
        }),
        Some(ModelReply {
            text: Some("memory complete".to_string()),
            backend: "fake".to_string(),
            response_id: Some("resp_3".to_string()),
            tool_calls: Vec::new(),
        }),
    ]));
    let courier = NativeCourier::with_chat_backend(backend.clone());
    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "remember my name".to_string(),
            },
        },
    ))
    .unwrap();

    let calls = backend.calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].tools.len(), 2);
    assert_eq!(calls[0].tools[0].name, "memory_put");
    assert_eq!(calls[0].tools[1].name, "memory_get");
    assert!(matches!(
        calls[0].tools[0].format,
        ModelToolFormat::JsonSchema { .. }
    ));
    assert_eq!(calls[1].previous_response_id.as_deref(), Some("resp_1"));
    assert!(calls[1].messages.is_empty());
    assert_eq!(calls[1].tool_outputs.len(), 1);
    assert_eq!(calls[1].tool_outputs[0].name, "memory_put");
    assert!(
        calls[1].tool_outputs[0]
            .output
            .contains("Stored memory profile:name")
    );
    assert_eq!(calls[2].previous_response_id.as_deref(), Some("resp_2"));
    assert_eq!(calls[2].tool_outputs.len(), 1);
    assert_eq!(calls[2].tool_outputs[0].name, "memory_get");
    assert!(
        calls[2].tool_outputs[0]
            .output
            .contains("profile:name = Christian")
    );
    drop(calls);

    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::ToolCallStarted { invocation, command, args })
            if invocation.name == "memory_put"
                && command == "dispatch-builtin"
                && args == &vec!["memory_put".to_string()]
    ));
    assert!(matches!(
        response.events.get(1),
        Some(CourierEvent::ToolCallFinished { result })
            if result.tool == "memory_put"
                && result.command == "dispatch-builtin"
                && result.stdout.contains("Stored memory profile:name")
    ));
    assert!(matches!(
        response.events.get(2),
        Some(CourierEvent::ToolCallStarted { invocation, command, args })
            if invocation.name == "memory_get"
                && command == "dispatch-builtin"
                && args == &vec!["memory_get".to_string()]
    ));
    assert!(matches!(
        response.events.get(3),
        Some(CourierEvent::ToolCallFinished { result })
            if result.tool == "memory_get"
                && result.stdout.contains("profile:name = Christian")
    ));
    assert!(matches!(
        response.events.get(4),
        Some(CourierEvent::Message { content, .. }) if content == "memory complete"
    ));
    assert!(matches!(response.events.last(), Some(CourierEvent::Done)));
}

#[test]
fn execute_builtin_memory_range_and_batch_tools() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MOUNT MEMORY sqlite
ENTRYPOINT chat
",
        &[],
    );
    let courier = NativeCourier::default();
    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

    let put_many = execute_builtin_tool(
        &session,
        "memory_put_many",
        r#"{"namespace":"profile","entries":[{"key":"a","value":"one"},{"key":"b","value":"two"},{"key":"c","value":"three"}]}"#,
    )
    .unwrap();
    assert!(
        put_many
            .stdout
            .contains("Stored 3 and updated 0 memory entries in namespace `profile`.")
    );

    let range = execute_builtin_tool(
        &session,
        "memory_list_range",
        r#"{"namespace":"profile","start_key":"b","end_key":"d"}"#,
    )
    .unwrap();
    assert_eq!(range.stdout, "profile:b = two\nprofile:c = three");

    let delete = execute_builtin_tool(
        &session,
        "memory_delete_range",
        r#"{"namespace":"profile","start_key":"b","end_key":"c"}"#,
    )
    .unwrap();
    assert!(
        delete
            .stdout
            .contains("Deleted 1 memory entry from namespace `profile`.")
    );

    let remaining =
        execute_builtin_tool(&session, "memory_list_range", r#"{"namespace":"profile"}"#).unwrap();
    assert_eq!(remaining.stdout, "profile:a = one\nprofile:c = three");
}

#[test]
fn execute_builtin_checkpoint_tools() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MOUNT SESSION sqlite
ENTRYPOINT chat
",
        &[],
    );
    let courier = NativeCourier::default();
    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

    let put = execute_builtin_tool(
        &session,
        "checkpoint_put",
        r#"{"name":"fetch-users","value":"{\"page\":2}"}"#,
    )
    .unwrap();
    assert_eq!(put.stdout, "Stored checkpoint `fetch-users`");

    let get =
        execute_builtin_tool(&session, "checkpoint_get", r#"{"name":"fetch-users"}"#).unwrap();
    assert_eq!(get.stdout, r#"fetch-users = {"page":2}"#);

    let list = execute_builtin_tool(&session, "checkpoint_list", r#"{"prefix":"fetch"}"#).unwrap();
    assert_eq!(list.stdout, r#"fetch-users = {"page":2}"#);

    let delete =
        execute_builtin_tool(&session, "checkpoint_delete", r#"{"name":"fetch-users"}"#).unwrap();
    assert_eq!(delete.stdout, "Deleted checkpoint `fetch-users`");
}

#[test]
fn native_courier_inspect_reports_mounts_secrets_and_local_tools() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
TOOL LOCAL tools/demo.sh AS demo
MOUNT SESSION sqlite
SECRET CAST_SAMPLE_SECRET
ENTRYPOINT job
",
        &[("tools/demo.sh", "printf ok")],
    );
    let courier = NativeCourier::default();

    let inspection = futures::executor::block_on(courier.inspect(&test_image.image)).unwrap();

    assert_eq!(inspection.entrypoint.as_deref(), Some("job"));
    assert_eq!(inspection.required_secrets, vec!["CAST_SAMPLE_SECRET"]);
    assert_eq!(inspection.mounts.len(), 1);
    assert_eq!(inspection.mounts[0].driver, "sqlite");
    assert_eq!(inspection.local_tools.len(), 1);
    assert_eq!(inspection.local_tools[0].alias, "demo");
}

#[test]
fn load_parcel_rejects_manifests_that_fail_schema_validation() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
SOUL SOUL.md
ENTRYPOINT chat
",
        &[("SOUL.md", "You are schema-checked.")],
    );

    let manifest_path = test_image.image.parcel_dir.join("manifest.json");
    let mut manifest = serde_json::from_slice::<Value>(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["tools"] = Value::String("not-an-array".to_string());
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let error = load_parcel(&test_image.image.parcel_dir).unwrap_err();
    assert!(matches!(error, CourierError::InvalidParcelSchema { .. }));
}

#[test]
fn native_courier_persists_session_sqlite_mounts() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MOUNT SESSION sqlite
ENTRYPOINT chat
",
        &[],
    );
    let courier = NativeCourier::default();
    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

    let sqlite_mount = session
        .resolved_mounts
        .iter()
        .find(|mount| mount.kind == MountKind::Session && mount.driver == "sqlite")
        .expect("expected sqlite session mount");
    let connection = Connection::open(&sqlite_mount.target_path).unwrap();
    let (turn_count, payload_json): (i64, String) = connection
        .query_row(
            "SELECT turn_count, payload_json FROM dispatch_sessions WHERE session_id = ?1",
            [&session.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(turn_count, 0);
    let persisted: CourierSession = serde_json::from_str(&payload_json).unwrap();
    assert_eq!(persisted.id, session.id);

    let response = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "hello".to_string(),
            },
        },
    ))
    .unwrap();
    let updated_turn_count: i64 = connection
        .query_row(
            "SELECT turn_count FROM dispatch_sessions WHERE session_id = ?1",
            [&response.session.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(updated_turn_count, 1);
}
