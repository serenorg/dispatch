use super::*;

#[test]
fn native_courier_enforces_tool_timeout_for_local_tools() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
TIMEOUT TOOL 50ms
TOOL LOCAL tools/slow.py AS slow USING python3 -u
ENTRYPOINT job
",
        &[(
            "tools/slow.py",
            "import time\n\
time.sleep(0.2)\n\
print('done')\n",
        )],
    );

    let error = run_local_tool(&test_parcel.parcel, "slow", None).unwrap_err();
    assert!(matches!(
        error,
        CourierError::ToolTimedOut { ref tool, ref timeout } if tool == "slow" && timeout == "TOOL"
    ));
}

#[test]
fn native_courier_caps_tool_timeout_by_remaining_run_budget() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
TIMEOUT RUN 100ms
TOOL LOCAL tools/slow.py AS slow USING python3 -u
ENTRYPOINT job
",
        &[(
            "tools/slow.py",
            "import time\n\
time.sleep(0.2)\n\
print('done')\n",
        )],
    );
    let courier = NativeCourier::default();
    let mut session =
        futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();
    session.elapsed_ms = 60;

    let error = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::InvokeTool {
                invocation: ToolInvocation {
                    name: "slow".to_string(),
                    input: None,
                },
            },
        },
    ))
    .unwrap_err();

    assert!(matches!(
        error,
        CourierError::ToolTimedOut { ref tool, ref timeout }
            if tool == "slow" && timeout == "RUN"
    ));
}

#[test]
fn native_courier_open_session_sets_identity_and_zero_turns() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
ENTRYPOINT chat
",
        &[],
    );
    let courier = NativeCourier::default();
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    assert!(
        session
            .id
            .starts_with(&format!("native-{}", test_parcel.parcel.config.digest))
    );
    assert_eq!(session.parcel_digest, test_parcel.parcel.config.digest);
    assert_eq!(session.entrypoint.as_deref(), Some("chat"));
    assert_eq!(session.turn_count, 0);
    assert!(session.history.is_empty());
}

#[test]
fn native_courier_validate_parcel_rejects_foreign_courier_reference() {
    let test_parcel = build_test_parcel(
        "\
FROM example/remote-worker:latest
ENTRYPOINT chat
",
        &[],
    );
    let courier = NativeCourier::default();

    let error =
        futures::executor::block_on(courier.validate_parcel(&test_parcel.parcel)).unwrap_err();

    assert!(matches!(
        error,
        CourierError::IncompatibleCourier { courier, parcel_courier, .. }
            if courier == "native" && parcel_courier == "example/remote-worker:latest"
    ));
}

#[test]
fn native_courier_prompt_run_emits_events_and_increments_turns() {
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
            operation: CourierOperation::ResolvePrompt,
        },
    ))
    .unwrap();

    assert_eq!(response.session.turn_count, 1);
    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::PromptResolved { text }) if text.contains("Soul body")
    ));
    assert!(matches!(response.events.last(), Some(CourierEvent::Done)));
}

#[test]
fn native_courier_chat_rejects_mismatched_entrypoint() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
ENTRYPOINT job
",
        &[],
    );
    let courier = NativeCourier::default();
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let error = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "hello".to_string(),
            },
        },
    ))
    .unwrap_err();

    assert!(matches!(
        error,
        CourierError::EntrypointMismatch { entrypoint, operation }
            if entrypoint == "job" && operation == "chat"
    ));
}

#[test]
fn native_courier_run_rejects_session_for_different_parcel() {
    let first_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
ENTRYPOINT chat
",
        &[],
    );
    let second_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
SOUL SOUL.md
ENTRYPOINT chat
",
        &[("SOUL.md", "different")],
    );
    let courier = NativeCourier::default();
    let session = futures::executor::block_on(courier.open_session(&first_parcel.parcel)).unwrap();

    let error = futures::executor::block_on(courier.run(
        &second_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::ResolvePrompt,
        },
    ))
    .unwrap_err();

    assert!(matches!(
        error,
        CourierError::SessionParcelMismatch { session_parcel_digest, parcel_digest }
            if session_parcel_digest == first_parcel.parcel.config.digest
                && parcel_digest == second_parcel.parcel.config.digest
    ));
}

#[test]
fn native_courier_tool_run_emits_started_and_finished_events() {
    let tool_path = test_tool_relative_path("demo");
    let tool_body = test_tool_print_body("{\"ok\":true}");
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
TOOL LOCAL {tool_path} AS demo
ENTRYPOINT job
"
        ),
        &[(tool_path.as_str(), tool_body.as_str())],
    );
    let courier = NativeCourier::default();
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

    assert_eq!(response.session.turn_count, 1);
    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::ToolCallStarted { command, .. })
            if command == if cfg!(windows) { "cmd" } else { "sh" }
    ));
    assert!(matches!(
        response.events.get(1),
        Some(CourierEvent::ToolCallFinished { result }) if result.exit_code == 0 && result.stdout.contains("{\"ok\":true}")
    ));
    assert!(matches!(response.events.last(), Some(CourierEvent::Done)));
}

#[test]
fn native_courier_chat_emits_assistant_message_and_records_history() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
SOUL SOUL.md
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT chat
",
        &[("SOUL.md", "Soul body"), ("tools/demo.sh", "printf ok")],
    );
    let courier = NativeCourier::default();
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "hello courier".to_string(),
            },
        },
    ))
    .unwrap();

    assert_eq!(response.session.turn_count, 1);
    assert_eq!(response.session.history.len(), 2);
    assert_eq!(response.session.history[0].role, "user");
    assert_eq!(response.session.history[0].content, "hello courier");
    assert_eq!(response.session.history[1].role, "assistant");
    assert!(response.session.history[1].content.contains("turn 1"));
    assert!(response.session.history[1].content.contains("1 tool"));
    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::Message { role, content })
            if role == "assistant" && content.contains("hello courier")
    ));
}

#[test]
fn native_courier_job_emits_assistant_message_and_records_history() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
ENTRYPOINT job
",
        &[],
    );
    let courier = NativeCourier::default();
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Job {
                payload: "{\"task\":\"summarize\"}".to_string(),
            },
        },
    ))
    .unwrap();

    assert_eq!(response.session.turn_count, 1);
    assert_eq!(response.session.history.len(), 2);
    assert_eq!(
        response.session.history[0].content,
        "Job payload:\n{\"task\":\"summarize\"}"
    );
    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::Message { content, .. })
            if content.contains("Native job reply")
    ));
    assert!(matches!(response.events.last(), Some(CourierEvent::Done)));
}

#[test]
fn native_courier_heartbeat_emits_assistant_message_and_records_history() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
ENTRYPOINT heartbeat
",
        &[],
    );
    let courier = NativeCourier::default();
    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Heartbeat { payload: None },
        },
    ))
    .unwrap();

    assert_eq!(response.session.turn_count, 1);
    assert_eq!(response.session.history.len(), 2);
    assert_eq!(response.session.history[0].content, "Heartbeat tick");
    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::Message { content, .. })
            if content.contains("Native heartbeat reply")
    ));
    assert!(matches!(response.events.last(), Some(CourierEvent::Done)));
}

#[test]
fn open_session_sets_label_and_zero_elapsed_budget() {
    let test_parcel = build_test_parcel(
        "\
NAME demo
FROM dispatch/native:latest
ENTRYPOINT chat
",
        &[],
    );
    let courier = NativeCourier::default();

    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    assert_eq!(session.label.as_deref(), Some("demo"));
    assert_eq!(session.elapsed_ms, 0);
}

#[test]
fn native_courier_rejects_runs_that_exceed_timeout_budget() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
TIMEOUT RUN 100ms
ENTRYPOINT chat
",
        &[],
    );
    let courier = NativeCourier::default();
    let mut session =
        futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();
    session.elapsed_ms = 100;

    let error = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "hello".to_string(),
            },
        },
    ))
    .unwrap_err();

    assert!(matches!(
        error,
        CourierError::RunTimedOut { ref timeout, .. } if timeout == "100ms"
    ));
}

#[test]
fn native_courier_inspection_helpers_do_not_consume_run_budget() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
TIMEOUT RUN 100ms
ENTRYPOINT chat
",
        &[],
    );
    let courier = NativeCourier::default();
    let mut session =
        futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();
    session.elapsed_ms = 100;

    let response = futures::executor::block_on(courier.run(
        &test_parcel.parcel,
        CourierRequest {
            session,
            operation: CourierOperation::ListLocalTools,
        },
    ))
    .unwrap();

    assert_eq!(response.session.elapsed_ms, 100);
    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::LocalToolsListed { .. })
    ));
}

#[test]
fn run_local_tool_requires_declared_secrets() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
TOOL LOCAL tools/demo.sh AS demo
SECRET CAST_TEST_SECRET_DOES_NOT_EXIST
ENTRYPOINT job
",
        &[("tools/demo.sh", "printf ok")],
    );

    let error = run_local_tool(&test_parcel.parcel, "demo", None).unwrap_err();
    assert!(matches!(
        error,
        CourierError::MissingSecret { name } if name == "CAST_TEST_SECRET_DOES_NOT_EXIST"
    ));
}

#[test]
fn open_session_prefers_secret_validation_to_late_tool_failure() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
TOOL LOCAL tools/demo.sh AS demo
SECRET CAST_TEST_SECRET_DOES_NOT_EXIST
ENTRYPOINT chat
",
        &[("tools/demo.sh", "printf ok")],
    );
    let courier = NativeCourier::default();

    let error = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap_err();
    assert!(matches!(
        error,
        CourierError::MissingSecret { name } if name == "CAST_TEST_SECRET_DOES_NOT_EXIST"
    ));
}

#[test]
fn run_local_tool_resolves_declared_secret_from_store() {
    let tool_path = test_tool_relative_path("env");
    let tool_body = test_tool_env_body(&[("visible_secret", "CAST_VISIBLE_SECRET")]);
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
TOOL LOCAL {tool_path} AS envcheck
SECRET CAST_VISIBLE_SECRET
ENTRYPOINT job
"
        ),
        &[(tool_path.as_str(), tool_body.as_str())],
    );
    crate::init_secret_store(&test_parcel.parcel.parcel_dir, false).unwrap();
    crate::set_secret(
        &test_parcel.parcel.parcel_dir,
        "CAST_VISIBLE_SECRET",
        "secret-from-store",
    )
    .unwrap();

    let result = run_local_tool(&test_parcel.parcel, "envcheck", None).unwrap();

    assert!(result.stdout.contains("visible_secret=secret-from-store"));
}

#[test]
fn open_session_accepts_required_secret_from_store() {
    let test_parcel = build_test_parcel(
        "\
FROM dispatch/native:latest
SECRET CAST_TEST_SECRET_FROM_STORE
ENTRYPOINT chat
",
        &[],
    );
    crate::init_secret_store(&test_parcel.parcel.parcel_dir, false).unwrap();
    crate::set_secret(
        &test_parcel.parcel.parcel_dir,
        "CAST_TEST_SECRET_FROM_STORE",
        "stored-value",
    )
    .unwrap();
    let courier = NativeCourier::default();

    let session = futures::executor::block_on(courier.open_session(&test_parcel.parcel)).unwrap();

    assert_eq!(session.entrypoint.as_deref(), Some("chat"));
}

#[test]
fn run_local_tool_only_forwards_declared_environment() {
    let tool_path = test_tool_relative_path("env");
    let tool_body = test_tool_env_body(&[
        ("visible_env", "CAST_VISIBLE_ENV"),
        ("visible_secret", "CAST_VISIBLE_SECRET"),
        ("hidden_env", "CAST_HIDDEN_ENV"),
    ]);
    let test_parcel = build_test_parcel(
        &format!(
            "\
FROM dispatch/native:latest
TOOL LOCAL {tool_path} AS envcheck
ENV CAST_VISIBLE_ENV=visible
SECRET CAST_VISIBLE_SECRET
ENTRYPOINT job
"
        ),
        &[(tool_path.as_str(), tool_body.as_str())],
    );

    let host_env = BTreeMap::from([
        (
            "CAST_VISIBLE_SECRET".to_string(),
            "secret-value".to_string(),
        ),
        ("CAST_HIDDEN_ENV".to_string(), "hidden-value".to_string()),
        ("HOME".to_string(), "/home/secret".to_string()),
    ]);

    let result = run_local_tool_with_env(&test_parcel.parcel, "envcheck", None, |name| {
        host_env.get(name).cloned()
    })
    .unwrap();

    assert!(result.stdout.contains("visible_env=visible"));
    assert!(result.stdout.contains("visible_secret=secret-value"));
    assert!(result.stdout.contains("hidden_env="));
    assert!(!result.stdout.contains("hidden_env=hidden-value"));
    assert!(!result.stdout.contains("/home/secret"));
}
