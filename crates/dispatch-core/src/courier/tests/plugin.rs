use super::*;

#[test]
#[cfg(unix)]
fn jsonl_plugin_courier_supports_capabilities_inspect_and_run() {
    let test_image = build_test_image(
        "\
FROM dispatch/custom:latest
ENTRYPOINT chat
",
        &[],
    );
    let (courier, _) =
        build_test_plugin_courier(&test_image._dir, &test_image.image.config.digest, false);

    let capabilities = futures::executor::block_on(courier.capabilities()).unwrap();
    assert_eq!(capabilities.courier_id, "demo-plugin");
    assert_eq!(capabilities.kind, CourierKind::Custom);
    assert!(capabilities.supports_chat);

    futures::executor::block_on(courier.validate_parcel(&test_image.image)).unwrap();
    let inspection = futures::executor::block_on(courier.inspect(&test_image.image)).unwrap();
    assert_eq!(inspection.courier_id, "demo-plugin");
    assert_eq!(inspection.entrypoint.as_deref(), Some("chat"));

    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
    assert_eq!(session.id, "plugin-session");
    assert_eq!(session.parcel_digest, test_image.image.config.digest);

    let response = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "hello plugin".to_string(),
            },
        },
    ))
    .unwrap();

    assert_eq!(response.courier_id, "demo-plugin");
    assert_eq!(response.session.turn_count, 1);
    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::Message { role, content })
            if role == "assistant" && content == "from plugin"
    ));
}

#[test]
#[cfg(unix)]
fn jsonl_plugin_courier_surfaces_structured_errors() {
    let test_image = build_test_image(
        "\
FROM dispatch/custom:latest
ENTRYPOINT chat
",
        &[],
    );
    let (courier, _) =
        build_test_plugin_courier(&test_image._dir, &test_image.image.config.digest, true);

    let error = futures::executor::block_on(courier.inspect(&test_image.image)).unwrap_err();
    assert!(matches!(
        error,
        CourierError::PluginProtocol { courier, message }
            if courier == "demo-plugin" && message.contains("bad_request") && message.contains("plugin rejected request")
    ));
}

#[test]
#[cfg(unix)]
fn jsonl_plugin_reuses_persistent_process_across_turns() {
    let test_image = build_test_image(
        "\
FROM dispatch/custom:latest
ENTRYPOINT chat
",
        &[],
    );
    let (courier, _plugin_path, starts_path) =
        build_test_counting_plugin_courier(&test_image._dir, &test_image.image.config.digest);

    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
    let starts_after_open = fs::read_to_string(&starts_path).unwrap();
    assert_eq!(starts_after_open.lines().count(), 2);

    let first = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "first".to_string(),
            },
        },
    ))
    .unwrap();
    let starts_after_first = fs::read_to_string(&starts_path).unwrap();
    assert_eq!(starts_after_first.lines().count(), 2);
    assert_eq!(first.session.turn_count, 1);
    assert!(matches!(
        first.events.first(),
        Some(CourierEvent::Message { role, content })
            if role == "assistant" && content == "from plugin turn 1"
    ));

    let second = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session: first.session,
            operation: CourierOperation::Chat {
                input: "second".to_string(),
            },
        },
    ))
    .unwrap();
    let starts_after_second = fs::read_to_string(&starts_path).unwrap();
    assert_eq!(starts_after_second.lines().count(), 2);
    assert_eq!(second.session.turn_count, 2);
    assert!(matches!(
        second.events.first(),
        Some(CourierEvent::Message { role, content })
            if role == "assistant" && content == "from plugin turn 2"
    ));
}

#[test]
#[cfg(unix)]
fn jsonl_plugin_resumes_persistent_session_after_new_host_process() {
    let test_image = build_test_image(
        "\
FROM dispatch/custom:latest
ENTRYPOINT chat
",
        &[],
    );
    let (courier, _plugin_path, starts_path) =
        build_test_counting_plugin_courier(&test_image._dir, &test_image.image.config.digest);

    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
    let first = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "first".to_string(),
            },
        },
    ))
    .unwrap();
    assert_eq!(first.session.turn_count, 1);

    let manifest = courier.manifest.clone();
    drop(courier);

    let starts_after_restart = fs::read_to_string(&starts_path).unwrap();
    assert_eq!(starts_after_restart.lines().count(), 2);

    let resumed_courier = JsonlCourierPlugin::new(manifest);
    let second = futures::executor::block_on(resumed_courier.run(
        &test_image.image,
        CourierRequest {
            session: first.session,
            operation: CourierOperation::Chat {
                input: "second".to_string(),
            },
        },
    ))
    .unwrap();

    let starts_after_resume = fs::read_to_string(&starts_path).unwrap();
    assert_eq!(starts_after_resume.lines().count(), 3);
    assert_eq!(second.session.turn_count, 2);
    assert!(matches!(
        second.events.first(),
        Some(CourierEvent::Message { role, content })
            if role == "assistant" && content == "from plugin turn 2"
    ));
}

#[test]
#[cfg(unix)]
fn jsonl_plugin_sends_shutdown_to_persistent_process_on_drop() {
    let test_image = build_test_image(
        "\
FROM dispatch/custom:latest
ENTRYPOINT chat
",
        &[],
    );
    let dir = tempdir().unwrap();
    let (courier, shutdowns_path) =
        build_test_shutdown_plugin_courier(&dir, &test_image.image.config.digest);

    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
    let _ = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "hello".to_string(),
            },
        },
    ))
    .unwrap();

    drop(courier);

    let shutdowns = fs::read_to_string(shutdowns_path).unwrap();
    assert!(shutdowns.contains("shutdown"));
}

#[test]
#[cfg(unix)]
fn jsonl_plugin_run_timeout_uses_remaining_run_budget() {
    let test_image = build_test_image(
        "\
FROM dispatch/custom:latest
TIMEOUT RUN 50ms
ENTRYPOINT chat
",
        &[],
    );
    let dir = tempdir().unwrap();
    let courier = build_test_slow_plugin_courier(&dir, &test_image.image.config.digest);

    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
    let error = futures::executor::block_on(courier.run(
        &test_image.image,
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
        CourierError::RunTimedOut { ref timeout, .. } if timeout == "50ms"
    ));
}

#[test]
#[cfg(unix)]
fn jsonl_plugin_courier_detects_executable_drift() {
    let test_image = build_test_image(
        "\
FROM dispatch/custom:latest
ENTRYPOINT chat
",
        &[],
    );
    let (courier, plugin_path) =
        build_test_plugin_courier(&test_image._dir, &test_image.image.config.digest, false);
    fs::write(&plugin_path, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&plugin_path, fs::Permissions::from_mode(0o755)).unwrap();

    let error = futures::executor::block_on(courier.capabilities()).unwrap_err();
    assert!(matches!(
        error,
        CourierError::PluginExecutableChanged { courier, .. } if courier == "demo-plugin"
    ));
}
