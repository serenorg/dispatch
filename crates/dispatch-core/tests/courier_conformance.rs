use dispatch_core::{
    BuildOptions, CourierBackend, CourierError, CourierEvent, CourierKind, CourierOperation,
    CourierRequest, CourierResponse, DockerCourier, LocalToolTarget, MountKind, NativeCourier,
    ToolConfig, build_agentfile, load_parcel,
};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    thread,
    time::Duration,
};
use tempfile::tempdir;

struct FixtureImage {
    _dir: tempfile::TempDir,
    image: dispatch_core::LoadedParcel,
}

fn demo_tool_relative_path() -> &'static str {
    if cfg!(windows) {
        "tools/demo.cmd"
    } else {
        "tools/demo.sh"
    }
}

fn demo_tool_body() -> &'static str {
    if cfg!(windows) {
        "@echo off\r\necho ok\r\n"
    } else {
        "#!/bin/sh\ncat >/dev/null\nprintf 'ok\\n'\n"
    }
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

struct TestA2aServer {
    base_url: String,
    shutdown: mpsc::Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for TestA2aServer {
    fn drop(&mut self) {
        let _ = self.shutdown.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn start_test_a2a_server() -> TestA2aServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
    let server_base_url = base_url.clone();
    let thread = thread::spawn(move || {
        loop {
            if shutdown_rx.try_recv().is_ok() {
                break;
            }
            match listener.accept() {
                Ok((stream, _)) => handle_test_a2a_connection(stream, &server_base_url),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("failed to accept A2A connection: {error}"),
            }
        }
    });

    TestA2aServer {
        base_url,
        shutdown: shutdown_tx,
        thread: Some(thread),
    }
}

fn handle_test_a2a_connection(stream: TcpStream, base_url: &str) {
    stream.set_nonblocking(false).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap() == 0 {
        return;
    }
    let request_line = request_line.trim_end();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let mut content_length = 0usize;
    loop {
        let mut header_line = String::new();
        reader.read_line(&mut header_line).unwrap();
        let header_line = header_line.trim_end();
        if header_line.is_empty() {
            break;
        }
        if let Some((name, value)) = header_line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).unwrap();

    match (method, target) {
        ("GET", "/.well-known/agent.json") => write_test_http_response(
            &mut writer,
            200,
            "application/json",
            serde_json::to_vec(&serde_json::json!({
                "name": "conformance-a2a",
                "url": format!("{base_url}/a2a")
            }))
            .unwrap()
            .as_slice(),
        ),
        ("POST", path) if path.ends_with("/a2a") => {
            let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let text = payload
                .pointer("/params/message/parts/0/text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            write_test_http_response(
                &mut writer,
                200,
                "application/json",
                serde_json::to_vec(&serde_json::json!({
                    "jsonrpc":"2.0",
                    "id":"1",
                    "result":{
                        "id":"task-1",
                        "status":{"state":"completed","message":"ok"},
                        "artifacts":[{"parts":[{"kind":"text","text":format!("echo:{text}")}]}]
                    }
                }))
                .unwrap()
                .as_slice(),
            );
        }
        _ => write_test_http_response(&mut writer, 404, "text/plain", b"not found"),
    }
}

fn write_test_http_response(writer: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "OK",
    };
    let headers = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = writer.write_all(headers.as_bytes());
    let _ = writer.write_all(body);
    let _ = writer.flush();
}

#[test]
fn native_courier_conformance_supports_prompt_tools_and_chat() {
    let fixture = build_fixture(
        &format!(
            "\
FROM dispatch/native:latest
SOUL SOUL.md
TOOL LOCAL {} AS demo
ENTRYPOINT chat
",
            demo_tool_relative_path()
        ),
        &[
            ("SOUL.md", "Soul body"),
            (demo_tool_relative_path(), demo_tool_body()),
        ],
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
        &format!(
            "\
FROM dispatch/native:latest
SOUL SOUL.md
TOOL LOCAL {} AS demo
ENTRYPOINT job
",
            demo_tool_relative_path()
        ),
        &[
            ("SOUL.md", "Soul body"),
            (demo_tool_relative_path(), demo_tool_body()),
        ],
    );
    let heartbeat_fixture = build_fixture(
        &format!(
            "\
FROM dispatch/native:latest
SOUL SOUL.md
TOOL LOCAL {} AS demo
ENTRYPOINT heartbeat
",
            demo_tool_relative_path()
        ),
        &[
            ("SOUL.md", "Soul body"),
            (demo_tool_relative_path(), demo_tool_body()),
        ],
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
fn native_courier_conformance_supports_a2a_tools() {
    let server = start_test_a2a_server();
    let fixture = build_fixture(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {} DISCOVERY card EXPECT_AGENT_NAME conformance-a2a
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );
    let courier = NativeCourier::default();

    let inspection = futures::executor::block_on(courier.inspect(&fixture.image)).unwrap();
    assert_eq!(inspection.local_tools.len(), 1);
    assert!(matches!(
        inspection.local_tools[0].target,
        LocalToolTarget::A2a { .. }
    ));

    let session = futures::executor::block_on(courier.open_session(&fixture.image)).unwrap();
    let tool = futures::executor::block_on(courier.run(
        &fixture.image,
        CourierRequest {
            session,
            operation: CourierOperation::InvokeTool {
                invocation: dispatch_core::ToolInvocation {
                    name: "broker".to_string(),
                    input: Some("hello a2a".to_string()),
                },
            },
        },
    ))
    .unwrap();
    assert!(tool.events.iter().any(|event| {
        matches!(
            event,
            CourierEvent::ToolCallFinished { result }
                if result.tool == "broker" && result.stdout.contains("echo:hello a2a")
        )
    }));
    assert_done(&tool);
}

#[test]
fn docker_courier_conformance_supports_prompt_tools_and_chat() {
    let fixture = build_fixture(
        &format!(
            "\
FROM dispatch/docker:latest
SOUL SOUL.md
TOOL LOCAL {} AS demo
ENTRYPOINT chat
",
            demo_tool_relative_path()
        ),
        &[
            ("SOUL.md", "Soul body"),
            (demo_tool_relative_path(), demo_tool_body()),
        ],
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
fn docker_courier_conformance_supports_a2a_tools() {
    let server = start_test_a2a_server();
    let fixture = build_fixture(
        &format!(
            "\
FROM dispatch/docker:latest
TOOL A2A broker URL {} DISCOVERY card EXPECT_AGENT_NAME conformance-a2a
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );
    let courier = DockerCourier::default();

    let inspection = futures::executor::block_on(courier.inspect(&fixture.image)).unwrap();
    assert_eq!(inspection.local_tools.len(), 1);
    assert!(matches!(
        inspection.local_tools[0].target,
        LocalToolTarget::A2a { .. }
    ));

    let session = futures::executor::block_on(courier.open_session(&fixture.image)).unwrap();
    let tool = futures::executor::block_on(courier.run(
        &fixture.image,
        CourierRequest {
            session,
            operation: CourierOperation::InvokeTool {
                invocation: dispatch_core::ToolInvocation {
                    name: "broker".to_string(),
                    input: Some("hello a2a".to_string()),
                },
            },
        },
    ))
    .unwrap();
    assert!(tool.events.iter().any(|event| {
        matches!(
            event,
            CourierEvent::ToolCallFinished { result }
                if result.tool == "broker" && result.stdout.contains("echo:hello a2a")
        )
    }));
    assert_done(&tool);
}

#[test]
fn docker_courier_conformance_supports_job_heartbeat_and_direct_tools() {
    let job_fixture = build_fixture(
        &format!(
            "\
FROM dispatch/docker:latest
SOUL SOUL.md
TOOL LOCAL {} AS demo
ENTRYPOINT job
",
            demo_tool_relative_path()
        ),
        &[
            ("SOUL.md", "Soul body"),
            (demo_tool_relative_path(), demo_tool_body()),
        ],
    );
    let heartbeat_fixture = build_fixture(
        &format!(
            "\
FROM dispatch/docker:latest
SOUL SOUL.md
TOOL LOCAL {} AS demo
ENTRYPOINT heartbeat
",
            demo_tool_relative_path()
        ),
        &[
            ("SOUL.md", "Soul body"),
            (demo_tool_relative_path(), demo_tool_body()),
        ],
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
        &format!(
            "\
FROM dispatch/native:latest
MODEL gpt-5-mini PROVIDER openai
TOOL LOCAL {} AS demo SCHEMA schemas/demo.json DESCRIPTION \"Look up a record by id.\"
ENTRYPOINT chat
",
            demo_tool_relative_path()
        ),
        &[
            (demo_tool_relative_path(), demo_tool_body()),
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
