use dispatch_core::{
    BuildOptions, CourierBackend, CourierError, CourierEvent, CourierKind, CourierOperation,
    CourierRequest, CourierResponse, DockerCourier, MountKind, NativeCourier, ToolConfig,
    build_agentfile, load_parcel,
};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

struct FixtureImage {
    _dir: tempfile::TempDir,
    image: dispatch_core::LoadedParcel,
}

fn build_fixture(agentfile: &str, files: &[(&str, &str)]) -> FixtureImage {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("Agentfile"), agentfile).unwrap();
    for (relative, body) in files {
        let path = dir.path().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    let built = build_agentfile(
        &dir.path().join("Agentfile"),
        &BuildOptions {
            output_root: dir.path().join(".dispatch/parcels"),
        },
    )
    .unwrap();

    FixtureImage {
        _dir: dir,
        image: load_parcel(&built.parcel_dir).unwrap(),
    }
}

fn assert_done(response: &CourierResponse) {
    assert!(matches!(response.events.last(), Some(CourierEvent::Done)));
}

fn assert_assistant_message(response: &CourierResponse) {
    assert!(
        response.events.iter().any(
            |event| matches!(event, CourierEvent::Message { role, .. } if role == "assistant")
        )
    );
}

#[test]
fn native_courier_conformance_supports_prompt_tools_and_chat() {
    let fixture = build_fixture(
        "\
FROM dispatch/native:latest
SOUL SOUL.md
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT chat
",
        &[("SOUL.md", "Soul body"), ("tools/demo.sh", "printf ok")],
    );
    let courier = NativeCourier::default();

    futures::executor::block_on(courier.validate_parcel(&fixture.image)).unwrap();
    let inspection = futures::executor::block_on(courier.inspect(&fixture.image)).unwrap();
    assert_eq!(inspection.kind, CourierKind::Native);
    assert_eq!(inspection.entrypoint.as_deref(), Some("chat"));
    assert_eq!(inspection.local_tools.len(), 1);

    let session = futures::executor::block_on(courier.open_session(&fixture.image)).unwrap();
    assert_eq!(session.parcel_digest, fixture.image.config.digest);
    assert_eq!(session.turn_count, 0);

    let prompt = futures::executor::block_on(courier.run(
        &fixture.image,
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
    assert_done(&prompt);

    let tools = futures::executor::block_on(courier.run(
        &fixture.image,
        CourierRequest {
            session: session.clone(),
            operation: CourierOperation::ListLocalTools,
        },
    ))
    .unwrap();
    assert!(matches!(
        tools.events.first(),
        Some(CourierEvent::LocalToolsListed { tools }) if tools.len() == 1 && tools[0].alias == "demo"
    ));
    assert_done(&tools);

    let chat = futures::executor::block_on(courier.run(
        &fixture.image,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "hello".to_string(),
            },
        },
    ))
    .unwrap();
    assert_assistant_message(&chat);
    assert_eq!(chat.session.turn_count, 1);
    assert_eq!(chat.session.history.len(), 2);
    assert_done(&chat);
}

#[test]
fn native_courier_conformance_supports_job_heartbeat_and_direct_tools() {
    let job_fixture = build_fixture(
        "\
FROM dispatch/native:latest
SOUL SOUL.md
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT job
",
        &[("SOUL.md", "Soul body"), ("tools/demo.sh", "printf ok")],
    );
    let heartbeat_fixture = build_fixture(
        "\
FROM dispatch/native:latest
SOUL SOUL.md
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT heartbeat
",
        &[("SOUL.md", "Soul body"), ("tools/demo.sh", "printf ok")],
    );
    let courier = NativeCourier::default();

    let job_session =
        futures::executor::block_on(courier.open_session(&job_fixture.image)).unwrap();
    let job = futures::executor::block_on(courier.run(
        &job_fixture.image,
        CourierRequest {
            session: job_session,
            operation: CourierOperation::Job {
                payload: "work item".to_string(),
            },
        },
    ))
    .unwrap();
    assert_assistant_message(&job);
    assert_done(&job);

    let heartbeat_session =
        futures::executor::block_on(courier.open_session(&heartbeat_fixture.image)).unwrap();
    let heartbeat = futures::executor::block_on(courier.run(
        &heartbeat_fixture.image,
        CourierRequest {
            session: heartbeat_session,
            operation: CourierOperation::Heartbeat {
                payload: Some("tick".to_string()),
            },
        },
    ))
    .unwrap();
    assert_assistant_message(&heartbeat);
    assert_done(&heartbeat);

    let tool_session =
        futures::executor::block_on(courier.open_session(&job_fixture.image)).unwrap();
    let tool = futures::executor::block_on(courier.run(
        &job_fixture.image,
        CourierRequest {
            session: tool_session,
            operation: CourierOperation::InvokeTool {
                invocation: dispatch_core::ToolInvocation {
                    name: "demo".to_string(),
                    input: Some("hello".to_string()),
                },
            },
        },
    ))
    .unwrap();
    assert!(tool.events.iter().any(|event| {
        matches!(
            event,
            CourierEvent::ToolCallFinished { result } if result.tool == "demo" && result.exit_code == 0
        )
    }));
    assert_done(&tool);
}

#[test]
fn docker_courier_conformance_supports_prompt_tools_and_chat() {
    let fixture = build_fixture(
        "\
FROM dispatch/docker:latest
SOUL SOUL.md
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT chat
",
        &[("SOUL.md", "Soul body"), ("tools/demo.sh", "printf ok")],
    );
    let courier = DockerCourier::default();

    futures::executor::block_on(courier.validate_parcel(&fixture.image)).unwrap();
    let inspection = futures::executor::block_on(courier.inspect(&fixture.image)).unwrap();
    assert_eq!(inspection.kind, CourierKind::Docker);
    assert_eq!(inspection.local_tools.len(), 1);

    let session = futures::executor::block_on(courier.open_session(&fixture.image)).unwrap();
    assert_eq!(session.parcel_digest, fixture.image.config.digest);

    let prompt = futures::executor::block_on(courier.run(
        &fixture.image,
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
    assert_done(&prompt);

    let tools = futures::executor::block_on(courier.run(
        &fixture.image,
        CourierRequest {
            session: session.clone(),
            operation: CourierOperation::ListLocalTools,
        },
    ))
    .unwrap();
    assert!(matches!(
        tools.events.first(),
        Some(CourierEvent::LocalToolsListed { tools }) if tools.len() == 1 && tools[0].alias == "demo"
    ));
    assert_done(&tools);

    let chat = futures::executor::block_on(courier.run(
        &fixture.image,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "hello".to_string(),
            },
        },
    ))
    .unwrap();
    assert_assistant_message(&chat);
    assert_eq!(chat.session.turn_count, 1);
    assert_eq!(chat.session.history.len(), 2);
    assert_done(&chat);
}

#[test]
fn docker_courier_conformance_supports_job_heartbeat_and_direct_tools() {
    let job_fixture = build_fixture(
        "\
FROM dispatch/docker:latest
SOUL SOUL.md
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT job
",
        &[("SOUL.md", "Soul body"), ("tools/demo.sh", "printf ok")],
    );
    let heartbeat_fixture = build_fixture(
        "\
FROM dispatch/docker:latest
SOUL SOUL.md
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT heartbeat
",
        &[("SOUL.md", "Soul body"), ("tools/demo.sh", "printf ok")],
    );
    #[cfg(unix)]
    let docker_bin_dir = tempdir().unwrap();
    #[cfg(unix)]
    let courier = {
        let docker_bin = docker_bin_dir.path().join("docker");
        fs::write(
            &docker_bin,
            "\
#!/bin/sh
printf 'ok\\n'
cat >/dev/null
",
        )
        .unwrap();
        fs::set_permissions(&docker_bin, fs::Permissions::from_mode(0o755)).unwrap();
        DockerCourier::new(&docker_bin, "python:3.13-alpine")
    };
    #[cfg(not(unix))]
    let courier = DockerCourier::default();

    let job_session =
        futures::executor::block_on(courier.open_session(&job_fixture.image)).unwrap();
    let job = futures::executor::block_on(courier.run(
        &job_fixture.image,
        CourierRequest {
            session: job_session,
            operation: CourierOperation::Job {
                payload: "work item".to_string(),
            },
        },
    ))
    .unwrap();
    assert_assistant_message(&job);
    assert_done(&job);

    let heartbeat_session =
        futures::executor::block_on(courier.open_session(&heartbeat_fixture.image)).unwrap();
    let heartbeat = futures::executor::block_on(courier.run(
        &heartbeat_fixture.image,
        CourierRequest {
            session: heartbeat_session,
            operation: CourierOperation::Heartbeat {
                payload: Some("tick".to_string()),
            },
        },
    ))
    .unwrap();
    assert_assistant_message(&heartbeat);
    assert_done(&heartbeat);

    let tool_session =
        futures::executor::block_on(courier.open_session(&job_fixture.image)).unwrap();
    let tool = futures::executor::block_on(courier.run(
        &job_fixture.image,
        CourierRequest {
            session: tool_session,
            operation: CourierOperation::InvokeTool {
                invocation: dispatch_core::ToolInvocation {
                    name: "demo".to_string(),
                    input: Some("hello".to_string()),
                },
            },
        },
    ))
    .unwrap();
    assert!(tool.events.iter().any(|event| {
        matches!(
            event,
            CourierEvent::ToolCallFinished { result } if result.tool == "demo" && result.exit_code == 0
        )
    }));
    assert_done(&tool);
}

#[test]
fn conformance_builds_schema_backed_local_tools_into_public_manifest_shape() {
    let fixture = build_fixture(
        "\
FROM dispatch/native:latest
MODEL gpt-5-mini PROVIDER openai
TOOL LOCAL tools/demo.sh AS demo SCHEMA schemas/demo.json DESCRIPTION \"Look up a record by id.\"
ENTRYPOINT chat
",
        &[
            ("tools/demo.sh", "printf ok"),
            (
                "schemas/demo.json",
                "{\n  \"type\": \"object\",\n  \"properties\": {\n    \"id\": { \"type\": \"string\" }\n  },\n  \"required\": [\"id\"]\n}",
            ),
        ],
    );

    assert_eq!(fixture.image.config.tools.len(), 1);
    match &fixture.image.config.tools[0] {
        ToolConfig::Local(local) => {
            assert_eq!(local.alias, "demo");
            assert_eq!(
                local.description.as_deref(),
                Some("Look up a record by id.")
            );
            let schema = local
                .input_schema
                .as_ref()
                .expect("expected input schema in built manifest");
            assert_eq!(schema.packaged_path, "schemas/demo.json");
            assert_eq!(schema.sha256.len(), 64);
        }
        other => panic!("expected local tool, got {other:?}"),
    }
}

#[test]
fn native_courier_conformance_resolves_declared_mounts_on_open_session() {
    let fixture = build_fixture(
        "\
FROM dispatch/native:latest
SOUL SOUL.md
MOUNT SESSION sqlite
MOUNT MEMORY sqlite
MOUNT ARTIFACTS local
ENTRYPOINT chat
",
        &[("SOUL.md", "Soul body")],
    );
    let courier = NativeCourier::default();

    let session = futures::executor::block_on(courier.open_session(&fixture.image)).unwrap();
    assert_eq!(session.resolved_mounts.len(), 3);
    assert!(
        session
            .resolved_mounts
            .iter()
            .any(|mount| mount.kind == MountKind::Session && mount.driver == "sqlite")
    );
    assert!(
        session
            .resolved_mounts
            .iter()
            .any(|mount| mount.kind == MountKind::Memory && mount.driver == "sqlite")
    );
    assert!(
        session
            .resolved_mounts
            .iter()
            .any(|mount| mount.kind == MountKind::Artifacts && mount.driver == "local")
    );
}

#[test]
fn conformance_validate_parcel_rejects_incompatible_courier_targets() {
    let native_fixture = build_fixture(
        "\
FROM dispatch/native:latest
SOUL SOUL.md
ENTRYPOINT chat
",
        &[("SOUL.md", "Soul body")],
    );
    let docker_fixture = build_fixture(
        "\
FROM dispatch/docker:latest
SOUL SOUL.md
ENTRYPOINT chat
",
        &[("SOUL.md", "Soul body")],
    );

    let native = NativeCourier::default();
    let docker = DockerCourier::default();

    let native_error =
        futures::executor::block_on(native.validate_parcel(&docker_fixture.image)).unwrap_err();
    assert!(matches!(
        native_error,
        CourierError::IncompatibleCourier {
            courier,
            parcel_courier,
            ..
        } if courier == "native" && parcel_courier == docker_fixture.image.config.courier.reference()
    ));

    let docker_error =
        futures::executor::block_on(docker.validate_parcel(&native_fixture.image)).unwrap_err();
    assert!(matches!(
        docker_error,
        CourierError::IncompatibleCourier {
            courier,
            parcel_courier,
            ..
        } if courier == "docker" && parcel_courier == native_fixture.image.config.courier.reference()
    ));
}

#[test]
fn conformance_run_rejects_sessions_bound_to_other_parcels() {
    let first = build_fixture(
        "\
FROM dispatch/native:latest
SOUL SOUL.md
ENTRYPOINT chat
",
        &[("SOUL.md", "First soul")],
    );
    let second = build_fixture(
        "\
FROM dispatch/native:latest
SOUL SOUL.md
ENTRYPOINT chat
",
        &[("SOUL.md", "Second soul")],
    );
    let courier = NativeCourier::default();

    let session = futures::executor::block_on(courier.open_session(&first.image)).unwrap();
    let error = futures::executor::block_on(courier.run(
        &second.image,
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
        CourierError::SessionParcelMismatch {
            session_parcel_digest,
            parcel_digest
        } if parcel_digest == second.image.config.digest && session_parcel_digest == first.image.config.digest
    ));
}
