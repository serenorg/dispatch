use anyhow::{Context, Result, bail};
use dispatch_core::{
    BuiltinCourier, CourierBackend, CourierEvent, CourierOperation, CourierRequest, CourierSession,
    DockerCourier, LoadedParcel, LocalToolSpec, LocalToolTarget, NativeCourier, ResolvedCourier,
    ToolInvocation, WasmCourier, load_parcel, resolve_courier,
};
use futures::executor::block_on;
use std::{
    fs,
    io::{self, Write as _},
    path::Path,
};

pub(crate) fn run(args: crate::RunArgs) -> Result<()> {
    let policy = crate::CliA2aPolicy {
        allowed_origins: args.a2a_allowed_origins.clone(),
        trust_policy: args.a2a_trust_policy.clone(),
    };
    let courier_name = args.courier.clone();
    crate::with_cli_a2a_policy(policy, || {
        match resolve_courier(&courier_name, args.registry.as_deref())? {
            ResolvedCourier::Builtin(courier) => run_with_builtin_courier(courier, args),
            ResolvedCourier::Plugin(plugin) => {
                run_with_courier(dispatch_core::JsonlCourierPlugin::new(plugin), args)
            }
        }
    })
}

fn run_with_builtin_courier(courier: BuiltinCourier, args: crate::RunArgs) -> Result<()> {
    match courier {
        BuiltinCourier::Native => run_with_courier(NativeCourier::default(), args),
        BuiltinCourier::Docker => run_with_courier(DockerCourier::default(), args),
        BuiltinCourier::Wasm => run_with_courier(WasmCourier::new()?, args),
    }
}

fn run_with_courier<R: CourierBackend>(courier: R, args: crate::RunArgs) -> Result<()> {
    let crate::RunArgs {
        path,
        courier: _,
        registry: _,
        session_file,
        chat,
        job,
        heartbeat,
        interactive,
        print_prompt,
        list_tools,
        tool,
        input,
        a2a_allowed_origins: _,
        a2a_trust_policy: _,
    } = args;
    let parcel =
        load_parcel(&path).with_context(|| format!("failed to load parcel {}", path.display()))?;
    let mut session = load_or_open_session(&courier, &parcel, session_file.as_deref())?;

    if interactive {
        return run_interactive_chat(&courier, &parcel, &mut session, session_file.as_deref());
    }

    if let Some(chat_input) = chat {
        let response = block_on(courier.run(
            &parcel,
            CourierRequest {
                session: session.clone(),
                operation: CourierOperation::Chat { input: chat_input },
            },
        ))
        .with_context(|| "failed to execute chat turn")?;
        persist_session(session_file.as_deref(), &response.session)?;
        print_courier_events(&response.events);
        return Ok(());
    }

    if let Some(job_payload) = job {
        let response = block_on(courier.run(
            &parcel,
            CourierRequest {
                session: session.clone(),
                operation: CourierOperation::Job {
                    payload: job_payload,
                },
            },
        ))
        .with_context(|| "failed to execute job turn")?;
        persist_session(session_file.as_deref(), &response.session)?;
        print_courier_events(&response.events);
        return Ok(());
    }

    if let Some(heartbeat_payload) = heartbeat {
        let payload = if heartbeat_payload.is_empty() {
            None
        } else {
            Some(heartbeat_payload)
        };
        let response = block_on(courier.run(
            &parcel,
            CourierRequest {
                session: session.clone(),
                operation: CourierOperation::Heartbeat { payload },
            },
        ))
        .with_context(|| "failed to execute heartbeat turn")?;
        persist_session(session_file.as_deref(), &response.session)?;
        print_courier_events(&response.events);
        return Ok(());
    }

    if print_prompt {
        let response = block_on(courier.run(
            &parcel,
            CourierRequest {
                session: session.clone(),
                operation: CourierOperation::ResolvePrompt,
            },
        ))
        .with_context(|| "failed to resolve prompt stack")?;
        persist_session(session_file.as_deref(), &response.session)?;
        print_courier_events(&response.events);
        return Ok(());
    }

    if list_tools {
        let response = block_on(courier.run(
            &parcel,
            CourierRequest {
                session: session.clone(),
                operation: CourierOperation::ListLocalTools,
            },
        ))
        .with_context(|| "failed to list local tools")?;
        persist_session(session_file.as_deref(), &response.session)?;
        print_courier_events(&response.events);
        return Ok(());
    }

    if let Some(tool) = tool {
        let response = block_on(courier.run(
            &parcel,
            CourierRequest {
                session: session.clone(),
                operation: CourierOperation::InvokeTool {
                    invocation: ToolInvocation {
                        name: tool.clone(),
                        input,
                    },
                },
            },
        ))
        .with_context(|| format!("failed to run local tool `{tool}`"))?;
        persist_session(session_file.as_deref(), &response.session)?;
        print_courier_events(&response.events);
        return Ok(());
    }

    bail!(
        "`dispatch run` currently requires one of `--interactive`, `--chat <text>`, `--job <payload>`, `--heartbeat [payload]`, `--print-prompt`, `--list-tools`, or `--tool <name>`"
    )
}

fn run_interactive_chat<R: CourierBackend>(
    courier: &R,
    parcel: &LoadedParcel,
    session: &mut CourierSession,
    session_file: Option<&Path>,
) -> Result<()> {
    println!("Interactive chat started. Type /exit or /quit to stop.");

    let stdin = io::stdin();
    let mut line = String::new();

    loop {
        print!("you> ");
        io::stdout()
            .flush()
            .with_context(|| "failed to flush prompt")?;

        line.clear();
        let bytes = stdin
            .read_line(&mut line)
            .with_context(|| "failed to read chat input")?;
        if bytes == 0 {
            break;
        }

        let input = line.trim_end().to_string();
        if input.is_empty() {
            continue;
        }
        if matches!(input.as_str(), "/exit" | "/quit") {
            break;
        }

        let response = block_on(courier.run(
            parcel,
            CourierRequest {
                session: session.clone(),
                operation: CourierOperation::Chat { input },
            },
        ))
        .with_context(|| "failed to execute chat turn")?;

        *session = response.session;
        persist_session(session_file, session)?;
        print_courier_events(&response.events);
    }

    Ok(())
}

fn load_or_open_session(
    courier: &impl CourierBackend,
    parcel: &LoadedParcel,
    session_file: Option<&Path>,
) -> Result<CourierSession> {
    if let Some(path) = session_file
        && path.exists()
    {
        return load_session(path);
    }

    let session = block_on(courier.open_session(parcel))
        .with_context(|| "failed to open dispatch session")?;
    persist_session(session_file, &session)?;
    Ok(session)
}

pub(crate) fn load_session(path: &Path) -> Result<CourierSession> {
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&source)
        .with_context(|| format!("failed to parse session {}", path.display()))
}

pub(crate) fn persist_session(path: Option<&Path>, session: &CourierSession) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let payload = serde_json::to_string_pretty(session)?;
    fs::write(path, payload).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn print_courier_events(events: &[CourierEvent]) {
    let mut streamed_assistant_reply = false;
    let mut stream_line_open = false;
    for event in events {
        if stream_line_open && !matches!(event, CourierEvent::TextDelta { .. }) {
            println!();
            stream_line_open = false;
        }
        match event {
            CourierEvent::PromptResolved { text } => println!("{text}"),
            CourierEvent::LocalToolsListed { tools } => {
                for tool in tools {
                    println!("{}", format_listed_tool(tool));
                }
            }
            CourierEvent::BackendFallback { backend, error } => {
                println!("backend fallback ({backend}): {error}");
            }
            CourierEvent::ToolCallStarted {
                invocation,
                command,
                args,
            } => {
                println!("Tool: {}", invocation.name);
                println!("Command: {command}");
                if !args.is_empty() {
                    println!("Args: {}", args.join(" "));
                }
            }
            CourierEvent::ToolCallFinished { result } => {
                println!("Exit: {}", result.exit_code);
                if !result.stdout.is_empty() {
                    println!("Stdout:\n{}", result.stdout.trim_end());
                }
                if !result.stderr.is_empty() {
                    println!("Stderr:\n{}", result.stderr.trim_end());
                }
            }
            CourierEvent::Message { role, content } => {
                if streamed_assistant_reply && role == "assistant" {
                    continue;
                }
                println!("{role}: {content}");
            }
            CourierEvent::TextDelta { content } => {
                streamed_assistant_reply = true;
                stream_line_open = true;
                print!("{content}");
                let _ = io::stdout().flush();
            }
            CourierEvent::Done => {
                if stream_line_open {
                    println!();
                    stream_line_open = false;
                }
            }
        }
    }
}

fn format_listed_tool(tool: &LocalToolSpec) -> String {
    match &tool.target {
        LocalToolTarget::Local { packaged_path, .. } => {
            format!("{} -> {} [local]", tool.alias, packaged_path)
        }
        LocalToolTarget::A2a {
            endpoint_url,
            endpoint_mode,
            auth,
            expected_agent_name,
            expected_card_sha256,
        } => {
            let mut parts = vec!["a2a".to_string()];
            if let Some(mode) = endpoint_mode {
                parts.push(format!("discovery={mode:?}").to_ascii_lowercase());
            }
            if let Some(auth) = auth {
                parts.push(crate::tool_display::format_a2a_auth_summary(auth));
            }
            if let Some(name) = expected_agent_name {
                parts.push(format!("expected_agent_name={name}"));
            }
            if let Some(digest) = expected_card_sha256 {
                parts.push(format!("expected_card_sha256={digest}"));
            }
            format!("{} -> {} [{}]", tool.alias, endpoint_url, parts.join(" "))
        }
    }
}
