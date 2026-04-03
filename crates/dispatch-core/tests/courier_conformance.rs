use dispatch_core::{
    BuildOptions, CourierBackend, CourierEvent, CourierKind, CourierOperation, CourierRequest,
    CourierResponse, DockerCourier, NativeCourier, ToolConfig, build_agentfile, load_parcel,
};
use std::fs;
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
    assert!(matches!(
        chat.events.first(),
        Some(CourierEvent::Message { role, .. }) if role == "assistant"
    ));
    assert_eq!(chat.session.turn_count, 1);
    assert_eq!(chat.session.history.len(), 2);
    assert_done(&chat);
}

#[test]
fn docker_courier_conformance_supports_inspection_and_non_executing_operations() {
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

    let error = futures::executor::block_on(courier.run(
        &fixture.image,
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
        dispatch_core::CourierError::UnsupportedOperation { courier, operation }
            if courier == "docker" && operation == "chat"
    ));
}

#[test]
fn conformance_builds_schema_backed_local_tools_into_public_manifest_shape() {
    let fixture = build_fixture(
        "\
FROM dispatch/native:latest
MODEL gpt-5-mini
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
            assert_eq!(schema.source, "schemas/demo.json");
            assert_eq!(schema.sha256.len(), 64);
        }
        other => panic!("expected local tool, got {other:?}"),
    }
}
