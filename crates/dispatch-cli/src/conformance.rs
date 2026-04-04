use anyhow::{Context, Result, bail};
use dispatch_core::{
    BuildOptions, BuiltinCourier, CourierBackend, CourierError, CourierEvent, CourierKind,
    CourierOperation, CourierRequest, CourierResponse, DockerCourier, JsonlCourierPlugin,
    LoadedParcel, MountKind, NativeCourier, ResolvedCourier, ToolInvocation, WasmCourier,
    build_agentfile, load_parcel, resolve_courier,
};
use futures::executor::block_on;
use serde::Serialize;
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::mpsc,
    thread,
    time::Duration,
};
use tempfile::TempDir;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct ConformanceCheck {
    name: String,
    passed: bool,
    skipped: bool,
    detail: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct ConformanceReport {
    courier: String,
    courier_id: String,
    kind: CourierKind,
    checks: Vec<ConformanceCheck>,
}

struct ConformanceFixtures {
    _dir: TempDir,
    compatible: LoadedParcel,
    incompatible: LoadedParcel,
    job: LoadedParcel,
    heartbeat: LoadedParcel,
    a2a: Option<LoadedParcel>,
    _a2a_server: Option<ConformanceA2aServer>,
}

struct ConformanceA2aServer {
    base_url: String,
    shutdown: mpsc::Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for ConformanceA2aServer {
    fn drop(&mut self) {
        let _ = self.shutdown.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(crate) fn courier_conformance(
    name: &str,
    registry: Option<&Path>,
    emit_json: bool,
) -> Result<()> {
    match resolve_courier(name, registry)? {
        ResolvedCourier::Builtin(courier) => {
            run_courier_conformance_with_builtin(courier, emit_json)
        }
        ResolvedCourier::Plugin(plugin) => run_courier_conformance_with(
            plugin.name.clone(),
            JsonlCourierPlugin::new(plugin),
            emit_json,
        ),
    }
}

fn run_courier_conformance_with_builtin(courier: BuiltinCourier, emit_json: bool) -> Result<()> {
    match courier {
        BuiltinCourier::Native => {
            run_courier_conformance_with("native".to_string(), NativeCourier::default(), emit_json)
        }
        BuiltinCourier::Docker => {
            run_courier_conformance_with("docker".to_string(), DockerCourier::default(), emit_json)
        }
        BuiltinCourier::Wasm => {
            run_courier_conformance_with("wasm".to_string(), WasmCourier::default(), emit_json)
        }
    }
}

fn run_courier_conformance_with<R: CourierBackend>(
    courier_name: String,
    courier: R,
    emit_json: bool,
) -> Result<()> {
    let capabilities = block_on(courier.capabilities())?;
    let fixtures = build_conformance_fixtures(capabilities.kind)?;
    let compatible_reference = fixtures.compatible.config.courier.reference().to_string();

    let mut checks = vec![ConformanceCheck {
        name: "capabilities".to_string(),
        passed: true,
        skipped: false,
        detail: format!(
            "courier_id={} kind={:?} chat={} job={} heartbeat={}",
            capabilities.courier_id,
            capabilities.kind,
            capabilities.supports_chat,
            capabilities.supports_job,
            capabilities.supports_heartbeat
        ),
    }];

    checks.push(
        match block_on(courier.validate_parcel(&fixtures.compatible)) {
            Ok(()) => ConformanceCheck {
                name: "validate-compatible".to_string(),
                passed: true,
                skipped: false,
                detail: compatible_reference,
            },
            Err(error) => ConformanceCheck {
                name: "validate-compatible".to_string(),
                passed: false,
                skipped: false,
                detail: error.to_string(),
            },
        },
    );

    checks.push(
        match block_on(courier.validate_parcel(&fixtures.incompatible)) {
            Ok(()) => ConformanceCheck {
                name: "reject-incompatible".to_string(),
                passed: false,
                skipped: false,
                detail: format!(
                    "unexpectedly accepted {}",
                    fixtures.incompatible.config.courier.reference()
                ),
            },
            Err(_) => ConformanceCheck {
                name: "reject-incompatible".to_string(),
                passed: true,
                skipped: false,
                detail: fixtures.incompatible.config.courier.reference().to_string(),
            },
        },
    );

    let inspection = block_on(courier.inspect(&fixtures.compatible));
    checks.push(match &inspection {
        Ok(inspection) if inspection.entrypoint.as_deref() == Some("chat") => ConformanceCheck {
            name: "inspect-entrypoint".to_string(),
            passed: true,
            skipped: false,
            detail: "entrypoint=chat".to_string(),
        },
        Ok(inspection) => ConformanceCheck {
            name: "inspect-entrypoint".to_string(),
            passed: false,
            skipped: false,
            detail: format!("unexpected entrypoint {:?}", inspection.entrypoint),
        },
        Err(error) => ConformanceCheck {
            name: "inspect-entrypoint".to_string(),
            passed: false,
            skipped: false,
            detail: error.to_string(),
        },
    });
    checks.push(match &inspection {
        Ok(inspection)
            if inspection
                .local_tools
                .iter()
                .any(|tool| tool.alias == "demo") =>
        {
            ConformanceCheck {
                name: "inspect-local-tools".to_string(),
                passed: true,
                skipped: false,
                detail: "declared tool alias `demo` visible".to_string(),
            }
        }
        Ok(inspection) => ConformanceCheck {
            name: "inspect-local-tools".to_string(),
            passed: false,
            skipped: false,
            detail: format!(
                "tool aliases=[{}]",
                inspection
                    .local_tools
                    .iter()
                    .map(|tool| tool.alias.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
        Err(error) => ConformanceCheck {
            name: "inspect-local-tools".to_string(),
            passed: false,
            skipped: false,
            detail: error.to_string(),
        },
    });

    let session = block_on(courier.open_session(&fixtures.compatible));
    checks.push(match &session {
        Ok(session)
            if session.parcel_digest == fixtures.compatible.config.digest
                && session.turn_count == 0 =>
        {
            ConformanceCheck {
                name: "open-session".to_string(),
                passed: true,
                skipped: false,
                detail: format!("session={} turn_count=0", session.id),
            }
        }
        Ok(session) => ConformanceCheck {
            name: "open-session".to_string(),
            passed: false,
            skipped: false,
            detail: format!(
                "parcel_digest={} turn_count={}",
                session.parcel_digest, session.turn_count
            ),
        },
        Err(error) => ConformanceCheck {
            name: "open-session".to_string(),
            passed: false,
            skipped: false,
            detail: error.to_string(),
        },
    });

    if let Ok(session) = &session {
        checks.push(run_conformance_operation_check(
            "resolve-prompt",
            block_on(courier.run(
                &fixtures.compatible,
                CourierRequest {
                    session: session.clone(),
                    operation: CourierOperation::ResolvePrompt,
                },
            )),
            |response| {
                matches!(
                    response.events.first(),
                    Some(CourierEvent::PromptResolved { text }) if text.contains("Soul body")
                )
            },
            "expected PromptResolved event containing packaged prompt text",
        ));

        checks.push(run_conformance_operation_check(
            "list-local-tools",
            block_on(courier.run(
                &fixtures.compatible,
                CourierRequest {
                    session: session.clone(),
                    operation: CourierOperation::ListLocalTools,
                },
            )),
            |response| {
                matches!(
                    response.events.first(),
                    Some(CourierEvent::LocalToolsListed { tools })
                        if tools.iter().any(|tool| tool.alias == "demo")
                )
            },
            "expected LocalToolsListed event with alias `demo`",
        ));

        let mut mismatched_session = session.clone();
        mismatched_session.parcel_digest = "deadbeef".repeat(8);
        checks.push(
            match block_on(courier.run(
                &fixtures.compatible,
                CourierRequest {
                    session: mismatched_session,
                    operation: CourierOperation::ResolvePrompt,
                },
            )) {
                Ok(_) => ConformanceCheck {
                    name: "reject-session-mismatch".to_string(),
                    passed: false,
                    skipped: false,
                    detail: "unexpectedly accepted mismatched session".to_string(),
                },
                Err(_) => ConformanceCheck {
                    name: "reject-session-mismatch".to_string(),
                    passed: true,
                    skipped: false,
                    detail: "mismatched session rejected".to_string(),
                },
            },
        );

        if capabilities.supports_chat {
            checks.push(run_conformance_operation_check(
                "chat",
                block_on(courier.run(
                    &fixtures.compatible,
                    CourierRequest {
                        session: session.clone(),
                        operation: CourierOperation::Chat {
                            input: "hello".to_string(),
                        },
                    },
                )),
                assistant_message_and_done,
                "expected assistant message and Done event",
            ));
        } else {
            checks.push(skipped_check(
                "chat",
                "courier does not advertise chat support",
            ));
        }

        if capabilities.supports_job {
            let job_response = match block_on(courier.open_session(&fixtures.job)) {
                Ok(job_session) => block_on(courier.run(
                    &fixtures.job,
                    CourierRequest {
                        session: job_session,
                        operation: CourierOperation::Job {
                            payload: "work item".to_string(),
                        },
                    },
                )),
                Err(error) => Err(error),
            };
            checks.push(run_conformance_operation_check(
                "job",
                job_response,
                assistant_message_and_done,
                "expected assistant message and Done event",
            ));
        } else {
            checks.push(skipped_check(
                "job",
                "courier does not advertise job support",
            ));
        }

        if capabilities.supports_heartbeat {
            let heartbeat_response = match block_on(courier.open_session(&fixtures.heartbeat)) {
                Ok(heartbeat_session) => block_on(courier.run(
                    &fixtures.heartbeat,
                    CourierRequest {
                        session: heartbeat_session,
                        operation: CourierOperation::Heartbeat {
                            payload: Some("tick".to_string()),
                        },
                    },
                )),
                Err(error) => Err(error),
            };
            checks.push(run_conformance_operation_check(
                "heartbeat",
                heartbeat_response,
                assistant_message_and_done,
                "expected assistant message and Done event",
            ));
        } else {
            checks.push(skipped_check(
                "heartbeat",
                "courier does not advertise heartbeat support",
            ));
        }

        if capabilities.supports_local_tools {
            checks.push(run_conformance_operation_check(
                "invoke-tool",
                block_on(courier.run(
                    &fixtures.compatible,
                    CourierRequest {
                        session: session.clone(),
                        operation: CourierOperation::InvokeTool {
                            invocation: ToolInvocation {
                                name: "demo".to_string(),
                                input: Some("hello".to_string()),
                            },
                        },
                    },
                )),
                |response| {
                    response.events.iter().any(|event| {
                        matches!(
                            event,
                            CourierEvent::ToolCallFinished { result }
                                if result.tool == "demo" && result.exit_code == 0
                        )
                    }) && matches!(response.events.last(), Some(CourierEvent::Done))
                },
                "expected successful tool execution and Done event",
            ));

            if let Some(a2a_fixture) = &fixtures.a2a {
                let a2a_response = match block_on(courier.open_session(a2a_fixture)) {
                    Ok(a2a_session) => block_on(courier.run(
                        a2a_fixture,
                        CourierRequest {
                            session: a2a_session,
                            operation: CourierOperation::InvokeTool {
                                invocation: ToolInvocation {
                                    name: "broker".to_string(),
                                    input: Some("hello a2a".to_string()),
                                },
                            },
                        },
                    )),
                    Err(error) => Err(error),
                };
                checks.push(run_conformance_operation_check(
                    "invoke-a2a-tool",
                    a2a_response,
                    |response| {
                        response.events.iter().any(|event| {
                            matches!(
                                event,
                                CourierEvent::ToolCallFinished { result }
                                    if result.tool == "broker"
                                        && result.exit_code == 0
                                        && result.stdout.contains("echo:hello a2a")
                            )
                        }) && matches!(response.events.last(), Some(CourierEvent::Done))
                    },
                    "expected successful A2A tool execution and Done event",
                ));
            } else {
                checks.push(skipped_check(
                    "invoke-a2a-tool",
                    "no A2A conformance fixture for this courier kind",
                ));
            }
        } else {
            checks.push(skipped_check(
                "invoke-tool",
                "courier does not advertise local tool support",
            ));
            checks.push(skipped_check(
                "invoke-a2a-tool",
                "courier does not advertise local tool support",
            ));
        }
    }

    if capabilities.supports_mounts.contains(&MountKind::Session)
        || capabilities.supports_mounts.contains(&MountKind::Memory)
        || capabilities.supports_mounts.contains(&MountKind::Artifacts)
    {
        checks.push(match block_on(courier.open_session(&fixtures.compatible)) {
            Ok(session) if !session.resolved_mounts.is_empty() => ConformanceCheck {
                name: "resolve-mounts".to_string(),
                passed: true,
                skipped: false,
                detail: format!("resolved {} mount(s)", session.resolved_mounts.len()),
            },
            Ok(_) => ConformanceCheck {
                name: "resolve-mounts".to_string(),
                passed: false,
                skipped: false,
                detail: "expected resolved mounts for declared mount fixture".to_string(),
            },
            Err(error) => ConformanceCheck {
                name: "resolve-mounts".to_string(),
                passed: false,
                skipped: false,
                detail: error.to_string(),
            },
        });
    } else {
        checks.push(skipped_check(
            "resolve-mounts",
            "courier does not advertise mount support",
        ));
    }

    let report = ConformanceReport {
        courier: courier_name,
        courier_id: capabilities.courier_id,
        kind: capabilities.kind,
        checks,
    };

    if emit_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_conformance_report(&report);
    }

    if report
        .checks
        .iter()
        .any(|check| !check.skipped && !check.passed)
    {
        bail!("courier conformance failed")
    }

    Ok(())
}

fn assistant_message_and_done(response: &CourierResponse) -> bool {
    response
        .events
        .iter()
        .any(|event| matches!(event, CourierEvent::Message { role, .. } if role == "assistant"))
        && matches!(response.events.last(), Some(CourierEvent::Done))
}

fn skipped_check(name: &str, detail: &str) -> ConformanceCheck {
    ConformanceCheck {
        name: name.to_string(),
        passed: true,
        skipped: true,
        detail: detail.to_string(),
    }
}

fn run_conformance_operation_check(
    name: &str,
    response: Result<CourierResponse, CourierError>,
    predicate: impl FnOnce(&CourierResponse) -> bool,
    failure_detail: &str,
) -> ConformanceCheck {
    match response {
        Ok(response) if predicate(&response) => ConformanceCheck {
            name: name.to_string(),
            passed: true,
            skipped: false,
            detail: "ok".to_string(),
        },
        Ok(_) => ConformanceCheck {
            name: name.to_string(),
            passed: false,
            skipped: false,
            detail: failure_detail.to_string(),
        },
        Err(error) => ConformanceCheck {
            name: name.to_string(),
            passed: false,
            skipped: false,
            detail: error.to_string(),
        },
    }
}

fn print_conformance_report(report: &ConformanceReport) {
    println!(
        "Courier `{}` ({:?}, id `{}`)",
        report.courier, report.kind, report.courier_id
    );
    for check in &report.checks {
        let status = if check.skipped {
            "SKIP"
        } else if check.passed {
            "PASS"
        } else {
            "FAIL"
        };
        println!("{status} {}\t{}", check.name, check.detail);
    }
}

fn build_conformance_fixtures(kind: CourierKind) -> Result<ConformanceFixtures> {
    let dir = tempfile::tempdir().context("failed to create conformance fixture directory")?;
    let compatible = build_conformance_fixture(
        dir.path(),
        "compatible",
        compatible_reference_for_kind(kind),
        "MOUNT SESSION sqlite\nMOUNT MEMORY sqlite\nMOUNT ARTIFACTS local\n",
        "chat",
    )?;
    let incompatible = build_conformance_fixture(
        dir.path(),
        "incompatible",
        incompatible_reference_for_kind(kind),
        "",
        "chat",
    )?;
    let job = build_conformance_fixture(
        dir.path(),
        "job",
        compatible_reference_for_kind(kind),
        "MOUNT SESSION sqlite\nMOUNT MEMORY sqlite\nMOUNT ARTIFACTS local\n",
        "job",
    )?;
    let heartbeat = build_conformance_fixture(
        dir.path(),
        "heartbeat",
        compatible_reference_for_kind(kind),
        "MOUNT SESSION sqlite\nMOUNT MEMORY sqlite\nMOUNT ARTIFACTS local\n",
        "heartbeat",
    )?;
    let (a2a, a2a_server) = match kind {
        CourierKind::Native | CourierKind::Docker => {
            let server = start_conformance_a2a_server()?;
            let fixture = build_conformance_a2a_fixture(
                dir.path(),
                "a2a",
                compatible_reference_for_kind(kind),
                &server.base_url,
            )?;
            (Some(fixture), Some(server))
        }
        CourierKind::Wasm | CourierKind::Custom => (None, None),
    };
    Ok(ConformanceFixtures {
        _dir: dir,
        compatible,
        incompatible,
        job,
        heartbeat,
        a2a,
        _a2a_server: a2a_server,
    })
}

fn build_conformance_fixture(
    root: &Path,
    name: &str,
    courier_reference: &str,
    extra_lines: &str,
    entrypoint: &str,
) -> Result<LoadedParcel> {
    let context_dir = root.join(name);
    fs::create_dir_all(context_dir.join("tools"))
        .with_context(|| format!("failed to create {}", context_dir.display()))?;
    fs::write(
        context_dir.join("Agentfile"),
        format!(
            "FROM {courier_reference}\n\
NAME conformance-{name}\n\
VERSION 0.1.0\n\
SOUL SOUL.md\n\
TOOL LOCAL tools/demo.sh AS demo\n\
{extra_lines}\
ENTRYPOINT {entrypoint}\n"
        ),
    )
    .with_context(|| {
        format!(
            "failed to write {}",
            context_dir.join("Agentfile").display()
        )
    })?;
    fs::write(context_dir.join("SOUL.md"), "Soul body\n")
        .with_context(|| format!("failed to write {}", context_dir.join("SOUL.md").display()))?;
    fs::write(context_dir.join("tools/demo.sh"), "printf ok\n").with_context(|| {
        format!(
            "failed to write {}",
            context_dir.join("tools/demo.sh").display()
        )
    })?;
    let built = build_agentfile(
        &context_dir.join("Agentfile"),
        &BuildOptions {
            output_root: context_dir.join(".dispatch/parcels"),
        },
    )
    .with_context(|| {
        format!(
            "failed to build {}",
            context_dir.join("Agentfile").display()
        )
    })?;
    load_parcel(&built.parcel_dir)
        .with_context(|| format!("failed to load parcel {}", built.parcel_dir.display()))
}

fn build_conformance_a2a_fixture(
    root: &Path,
    name: &str,
    courier_reference: &str,
    endpoint_url: &str,
) -> Result<LoadedParcel> {
    let context_dir = root.join(name);
    fs::create_dir_all(&context_dir)
        .with_context(|| format!("failed to create {}", context_dir.display()))?;
    fs::write(
        context_dir.join("Agentfile"),
        format!(
            "FROM {courier_reference}\n\
NAME conformance-{name}\n\
VERSION 0.1.0\n\
SOUL SOUL.md\n\
TOOL A2A broker URL {endpoint_url} DISCOVERY card EXPECT_AGENT_NAME conformance-a2a\n\
ENTRYPOINT job\n"
        ),
    )
    .with_context(|| {
        format!(
            "failed to write {}",
            context_dir.join("Agentfile").display()
        )
    })?;
    fs::write(context_dir.join("SOUL.md"), "Soul body\n")
        .with_context(|| format!("failed to write {}", context_dir.join("SOUL.md").display()))?;
    let built = build_agentfile(
        &context_dir.join("Agentfile"),
        &BuildOptions {
            output_root: context_dir.join(".dispatch/parcels"),
        },
    )
    .with_context(|| {
        format!(
            "failed to build {}",
            context_dir.join("Agentfile").display()
        )
    })?;
    load_parcel(&built.parcel_dir)
        .with_context(|| format!("failed to load parcel {}", built.parcel_dir.display()))
}

fn start_conformance_a2a_server() -> Result<ConformanceA2aServer> {
    let listener = TcpListener::bind("127.0.0.1:0").context("failed to bind A2A test server")?;
    listener
        .set_nonblocking(true)
        .context("failed to configure A2A test server")?;
    let base_url = format!(
        "http://{}",
        listener
            .local_addr()
            .context("failed to read A2A test server address")?
    );
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
    let server_base_url = base_url.clone();
    let thread = thread::spawn(move || {
        loop {
            if shutdown_rx.try_recv().is_ok() {
                break;
            }
            match listener.accept() {
                Ok((stream, _)) => handle_conformance_a2a_connection(stream, &server_base_url),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("failed to accept A2A conformance connection: {error}"),
            }
        }
    });
    Ok(ConformanceA2aServer {
        base_url,
        shutdown: shutdown_tx,
        thread: Some(thread),
    })
}

fn handle_conformance_a2a_connection(stream: TcpStream, base_url: &str) {
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
        ("GET", "/.well-known/agent.json") => write_conformance_http_response(
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
            write_conformance_http_response(
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
        _ => write_conformance_http_response(&mut writer, 404, "text/plain", b"not found"),
    }
}

fn write_conformance_http_response(
    writer: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) {
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

fn compatible_reference_for_kind(kind: CourierKind) -> &'static str {
    match kind {
        CourierKind::Native => "dispatch/native:latest",
        CourierKind::Docker => "dispatch/docker:latest",
        CourierKind::Wasm => "dispatch/wasm:latest",
        CourierKind::Custom => "dispatch/custom:latest",
    }
}

fn incompatible_reference_for_kind(kind: CourierKind) -> &'static str {
    match kind {
        CourierKind::Native => "dispatch/docker:latest",
        CourierKind::Docker => "dispatch/native:latest",
        CourierKind::Wasm => "dispatch/native:latest",
        CourierKind::Custom => "dispatch/native:latest",
    }
}
