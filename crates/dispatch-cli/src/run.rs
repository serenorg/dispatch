use anyhow::{Context, Result, bail};
use dispatch_core::manifest::ToolInputSchemaRef;
use dispatch_core::{
    A2aToolConfig, BuiltinCourier, BuiltinToolConfig, CommandSpec, CourierBackend, CourierEvent,
    CourierOperation, CourierRequest, CourierSession, DockerCourier, LoadedParcel, LocalToolSpec,
    LocalToolTarget, McpToolConfig, NativeCourier, ResolvedCourier, ToolConfig, ToolInvocation,
    WasmCourier, list_native_builtin_tools, load_parcel, resolve_courier,
};
use futures::executor::block_on;
use serde::Serialize;
use std::{
    env, fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
};

pub(crate) fn run(args: crate::RunArgs) -> Result<()> {
    if args.exec.detach {
        return crate::runs::run_detached(args);
    }
    let stdout = io::stdout();
    let mut output = stdout.lock();
    run_into(args, &mut output)
}

pub(crate) fn run_into(args: crate::RunArgs, output: &mut impl io::Write) -> Result<()> {
    let policy = crate::CliA2aPolicy {
        allowed_origins: args.exec.a2a_allowed_origins.clone(),
        trust_policy: args.exec.a2a_trust_policy.clone(),
    };
    let courier_name = args.exec.courier.clone();
    crate::with_cli_a2a_policy(policy, || {
        match resolve_courier(&courier_name, args.exec.registry.as_deref())? {
            ResolvedCourier::Builtin(courier) => run_with_builtin_courier(courier, args, output),
            ResolvedCourier::Plugin(plugin) => {
                run_with_courier(dispatch_core::JsonlCourierPlugin::new(plugin), args, output)
            }
        }
    })
}

fn run_with_builtin_courier(
    courier: BuiltinCourier,
    args: crate::RunArgs,
    output: &mut impl io::Write,
) -> Result<()> {
    match courier {
        BuiltinCourier::Native => run_with_courier(NativeCourier::default(), args, output),
        BuiltinCourier::Docker => run_with_courier(DockerCourier::default(), args, output),
        BuiltinCourier::Wasm => run_with_courier(WasmCourier::new()?, args, output),
    }
}

fn run_with_courier<R: CourierBackend>(
    courier: R,
    args: crate::RunArgs,
    output: &mut impl io::Write,
) -> Result<()> {
    let crate::RunArgs { path, exec } = args;
    let crate::RunExecutionArgs {
        session_file,
        chat,
        job,
        heartbeat,
        interactive,
        print_prompt,
        list_tools,
        json,
        tool,
        input,
        tool_approval,
        detach: _,
        ..
    } = exec;
    let parcel = load_or_build_parcel_for_run(path)?;
    if list_tools && json {
        print_tool_manifest_json(output, &parcel)?;
        return Ok(());
    }

    let approval_mode = crate::resolve_run_tool_approval_mode(tool_approval);
    let mut session = load_or_open_session(&courier, &parcel, session_file.as_deref())?;

    crate::with_cli_tool_approval(approval_mode, || {
        if interactive {
            return run_interactive_chat(
                &courier,
                &parcel,
                &mut session,
                session_file.as_deref(),
                output,
            );
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
            print_courier_events(output, &response.events)?;
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
            print_courier_events(output, &response.events)?;
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
            print_courier_events(output, &response.events)?;
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
            print_courier_events(output, &response.events)?;
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
            print_courier_events(output, &response.events)?;
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
            print_courier_events(output, &response.events)?;
            return Ok(());
        }

        bail!(
            "`dispatch run` currently requires one of `--interactive`, `--chat <text>`, `--job <payload>`, `--heartbeat [payload]`, `--print-prompt`, `--list-tools`, or `--tool <name>`"
        )
    })
}

fn run_interactive_chat<R: CourierBackend>(
    courier: &R,
    parcel: &LoadedParcel,
    session: &mut CourierSession,
    session_file: Option<&Path>,
    output: &mut impl io::Write,
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
        print_courier_events(output, &response.events)?;
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

pub(crate) fn load_or_build_parcel_for_run(path: PathBuf) -> Result<LoadedParcel> {
    if crate::is_agentfile_target(&path) {
        return crate::build_parcel_from_source(path, None);
    }

    if path.exists() {
        return load_parcel(&path)
            .with_context(|| format!("failed to load parcel {}", path.display()));
    }

    if let Some((parcels_root, prefix)) = parcel_prefix_lookup(&path)? {
        let parcel_dir = crate::parcel_ops::resolve_parcel_prefix(&parcels_root, &prefix)?;
        return load_parcel(&parcel_dir)
            .with_context(|| format!("failed to load parcel {}", parcel_dir.display()));
    }

    load_parcel(&path).with_context(|| format!("failed to load parcel {}", path.display()))
}

fn parcel_prefix_lookup(path: &Path) -> Result<Option<(PathBuf, String)>> {
    let Some(prefix) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    if !prefix.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Ok(None);
    }

    let parcels_root = if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        if parent == Path::new(".") {
            crate::resolve_parcels_root(
                &env::current_dir().context("failed to resolve current working directory")?,
            )
        } else {
            crate::resolve_parcels_root(parent)
        }
    } else {
        crate::resolve_parcels_root(
            &env::current_dir().context("failed to resolve current working directory")?,
        )
    };

    Ok(Some((parcels_root, prefix.to_ascii_lowercase())))
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

pub(crate) fn print_courier_events(
    output: &mut impl io::Write,
    events: &[CourierEvent],
) -> Result<()> {
    let mut streamed_assistant_reply = false;
    let mut stream_line_open = false;
    for event in events {
        if stream_line_open && !matches!(event, CourierEvent::TextDelta { .. }) {
            writeln!(output)?;
            stream_line_open = false;
        }
        match event {
            CourierEvent::PromptResolved { text } => writeln!(output, "{text}")?,
            CourierEvent::LocalToolsListed { tools } => {
                for tool in tools {
                    writeln!(output, "{}", format_listed_tool(tool))?;
                }
            }
            CourierEvent::BackendFallback { backend, error } => {
                writeln!(output, "backend fallback ({backend}): {error}")?;
            }
            CourierEvent::ToolCallStarted {
                invocation,
                command,
                args,
            } => {
                writeln!(output, "Tool: {}", invocation.name)?;
                writeln!(output, "Command: {command}")?;
                if !args.is_empty() {
                    writeln!(output, "Args: {}", args.join(" "))?;
                }
            }
            CourierEvent::ToolCallFinished { result } => {
                writeln!(output, "Exit: {}", result.exit_code)?;
                if !result.stdout.is_empty() {
                    writeln!(output, "Stdout:\n{}", result.stdout.trim_end())?;
                }
                if !result.stderr.is_empty() {
                    writeln!(output, "Stderr:\n{}", result.stderr.trim_end())?;
                }
            }
            CourierEvent::Message { role, content } => {
                if streamed_assistant_reply && role == "assistant" {
                    continue;
                }
                writeln!(output, "{role}: {content}")?;
            }
            CourierEvent::TextDelta { content } => {
                streamed_assistant_reply = true;
                stream_line_open = true;
                write!(output, "{content}")?;
                output.flush()?;
            }
            CourierEvent::Done => {
                if stream_line_open {
                    writeln!(output)?;
                    stream_line_open = false;
                }
            }
        }
    }
    output.flush()?;
    Ok(())
}

fn format_listed_tool(tool: &LocalToolSpec) -> String {
    let skill_suffix = tool
        .skill_source
        .as_deref()
        .map(|source| format!(" skill={source}"))
        .unwrap_or_default();
    let policy_suffix = format_tool_policy_suffix(tool.approval, tool.risk);
    match &tool.target {
        LocalToolTarget::Local { packaged_path, .. } => {
            format!(
                "{} -> {} [local{}{}]",
                tool.alias, packaged_path, policy_suffix, skill_suffix
            )
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
            format!(
                "{} -> {} [{}{}{}]",
                tool.alias,
                endpoint_url,
                parts.join(" "),
                policy_suffix,
                skill_suffix
            )
        }
    }
}

fn format_tool_policy_suffix(
    approval: Option<dispatch_core::ToolApprovalPolicy>,
    risk: Option<dispatch_core::ToolRiskLevel>,
) -> String {
    let mut parts = Vec::new();
    if let Some(approval) = approval {
        parts.push(format!(
            "approval={}",
            format!("{approval:?}").to_ascii_lowercase()
        ));
    }
    if let Some(risk) = risk {
        parts.push(format!("risk={}", format!("{risk:?}").to_ascii_lowercase()));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" {}", parts.join(" "))
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ToolManifestEntry {
    Local {
        alias: String,
        approval: Option<dispatch_core::ToolApprovalPolicy>,
        risk: Option<dispatch_core::ToolRiskLevel>,
        description: Option<String>,
        skill_source: Option<String>,
        packaged_path: String,
        runner: CommandSpec,
        input_schema_packaged_path: Option<String>,
        input_schema_sha256: Option<String>,
        input_schema_json: Option<serde_json::Value>,
    },
    Builtin {
        capability: String,
        approval: Option<dispatch_core::ToolApprovalPolicy>,
        risk: Option<dispatch_core::ToolRiskLevel>,
        description: Option<String>,
        input_schema_json: serde_json::Value,
    },
    Mcp {
        server: String,
        approval: Option<dispatch_core::ToolApprovalPolicy>,
        risk: Option<dispatch_core::ToolRiskLevel>,
        description: Option<String>,
    },
    A2a {
        alias: String,
        approval: Option<dispatch_core::ToolApprovalPolicy>,
        risk: Option<dispatch_core::ToolRiskLevel>,
        description: Option<String>,
        url: String,
        endpoint_mode: Option<dispatch_core::A2aEndpointMode>,
        auth: Option<dispatch_core::A2aAuthConfig>,
        expected_agent_name: Option<String>,
        expected_card_sha256: Option<String>,
        input_schema_packaged_path: Option<String>,
        input_schema_sha256: Option<String>,
        input_schema_json: Option<serde_json::Value>,
    },
}

fn print_tool_manifest_json(output: &mut impl io::Write, parcel: &LoadedParcel) -> Result<()> {
    let payload = tool_manifest_entries(parcel)?;
    writeln!(output, "{}", serde_json::to_string_pretty(&payload)?)?;
    Ok(())
}

fn tool_manifest_entries(parcel: &LoadedParcel) -> Result<Vec<ToolManifestEntry>> {
    let builtin_specs = list_native_builtin_tools(parcel);
    let mut entries = Vec::with_capacity(parcel.config.tools.len());
    for tool in &parcel.config.tools {
        match tool {
            ToolConfig::Local(local) => entries.push(ToolManifestEntry::Local {
                alias: local.alias.clone(),
                approval: local.approval,
                risk: local.risk,
                description: local.description.clone(),
                skill_source: local.skill_source.clone(),
                packaged_path: local.packaged_path.clone(),
                runner: local.runner.clone(),
                input_schema_packaged_path: local
                    .input_schema
                    .as_ref()
                    .map(|schema| schema.packaged_path.clone()),
                input_schema_sha256: local
                    .input_schema
                    .as_ref()
                    .map(|schema| schema.sha256.clone()),
                input_schema_json: load_input_schema_json(parcel, local.input_schema.as_ref())?,
            }),
            ToolConfig::Builtin(BuiltinToolConfig {
                capability,
                approval,
                risk,
                description,
            }) => {
                if let Some(spec) = builtin_specs
                    .iter()
                    .find(|spec| spec.capability == *capability)
                {
                    entries.push(ToolManifestEntry::Builtin {
                        capability: capability.clone(),
                        approval: *approval,
                        risk: *risk,
                        description: description.clone(),
                        input_schema_json: spec.input_schema.clone(),
                    });
                }
            }
            ToolConfig::Mcp(McpToolConfig {
                server,
                approval,
                risk,
                description,
            }) => entries.push(ToolManifestEntry::Mcp {
                server: server.clone(),
                approval: *approval,
                risk: *risk,
                description: description.clone(),
            }),
            ToolConfig::A2a(A2aToolConfig {
                alias,
                url,
                endpoint_mode,
                auth,
                expected_agent_name,
                expected_card_sha256,
                approval,
                risk,
                description,
                input_schema,
            }) => entries.push(ToolManifestEntry::A2a {
                alias: alias.clone(),
                approval: *approval,
                risk: *risk,
                description: description.clone(),
                url: url.clone(),
                endpoint_mode: *endpoint_mode,
                auth: auth.clone(),
                expected_agent_name: expected_agent_name.clone(),
                expected_card_sha256: expected_card_sha256.clone(),
                input_schema_packaged_path: input_schema
                    .as_ref()
                    .map(|schema| schema.packaged_path.clone()),
                input_schema_sha256: input_schema.as_ref().map(|schema| schema.sha256.clone()),
                input_schema_json: load_input_schema_json(parcel, input_schema.as_ref())?,
            }),
        }
    }
    Ok(entries)
}

fn load_input_schema_json(
    parcel: &LoadedParcel,
    schema: Option<&ToolInputSchemaRef>,
) -> Result<Option<serde_json::Value>> {
    let Some(schema) = schema else {
        return Ok(None);
    };
    let source = fs::read_to_string(
        parcel
            .parcel_dir
            .join("context")
            .join(&schema.packaged_path),
    )
    .with_context(|| {
        format!(
            "failed to read tool schema {}",
            parcel
                .parcel_dir
                .join("context")
                .join(&schema.packaged_path)
                .display()
        )
    })?;
    let parsed = serde_json::from_str(&source)
        .with_context(|| format!("failed to parse tool schema {}", schema.packaged_path))?;
    Ok(Some(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dispatch_core::{BuildOptions, build_agentfile, load_parcel};
    use tempfile::tempdir;

    #[test]
    fn tool_manifest_entries_include_policy_and_schemas() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("source");
        fs::create_dir_all(root.join("tools")).unwrap();
        fs::create_dir_all(root.join("schemas")).unwrap();
        fs::write(
            root.join("Agentfile"),
            "\
FROM dispatch/native:latest
TOOL LOCAL tools/read_file.sh AS read_file SCHEMA schemas/read_file.json APPROVAL confirm RISK high DESCRIPTION \"Read a file\"
TOOL BUILTIN memory_get APPROVAL audit RISK low DESCRIPTION \"Read memory\"
TOOL A2A broker URL https://broker.example.com SCHEMA schemas/read_file.json APPROVAL confirm RISK medium DESCRIPTION \"Delegate to a broker\"
TOOL MCP github APPROVAL never RISK medium DESCRIPTION \"GitHub MCP\"
ENTRYPOINT chat
",
        )
        .unwrap();
        fs::write(root.join("tools/read_file.sh"), "printf ok").unwrap();
        fs::write(
            root.join("schemas/read_file.json"),
            "{\n  \"type\": \"object\",\n  \"properties\": {\"path\": {\"type\": \"string\"}},\n  \"required\": [\"path\"]\n}",
        )
        .unwrap();

        let built = build_agentfile(
            &root.join("Agentfile"),
            &BuildOptions {
                output_root: root.join(".dispatch/parcels"),
            },
        )
        .unwrap();
        let parcel = load_parcel(&built.parcel_dir).unwrap();
        let entries = tool_manifest_entries(&parcel).unwrap();
        let json = serde_json::to_value(&entries).unwrap();

        assert_eq!(entries.len(), 4);
        assert!(
            json.as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["kind"] == "local"
                    && entry["alias"] == "read_file"
                    && entry["approval"] == "confirm"
                    && entry["risk"] == "high"
                    && entry["input_schema_json"]["required"][0] == "path")
        );
        assert!(
            json.as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["kind"] == "builtin"
                    && entry["capability"] == "memory_get"
                    && entry["approval"] == "audit"
                    && entry["risk"] == "low")
        );
        assert!(
            json.as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["kind"] == "a2a"
                    && entry["alias"] == "broker"
                    && entry["approval"] == "confirm"
                    && entry["risk"] == "medium"
                    && entry["input_schema_json"]["type"] == "object")
        );
        assert!(
            json.as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["kind"] == "mcp"
                    && entry["server"] == "github"
                    && entry["approval"] == "never"
                    && entry["risk"] == "medium")
        );
    }

    #[test]
    fn load_or_build_parcel_for_run_builds_agentfile_source() {
        let dir = tempdir().unwrap();
        let source_dir = dir.path().join("agent");
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(
            source_dir.join("Agentfile"),
            "FROM dispatch/native:latest\nNAME run-source-test\nENTRYPOINT chat\n",
        )
        .unwrap();

        let parcel = load_or_build_parcel_for_run(source_dir.clone()).unwrap();

        assert_eq!(parcel.config.name.as_deref(), Some("run-source-test"));
        assert!(parcel.manifest_path.exists());
    }

    #[test]
    fn load_or_build_parcel_for_run_resolves_unique_digest_prefix() {
        let dir = tempdir().unwrap();
        let source_dir = dir.path().join("agent");
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(
            source_dir.join("Agentfile"),
            "FROM dispatch/native:latest\nNAME run-prefix-test\nENTRYPOINT chat\n",
        )
        .unwrap();

        let built = build_agentfile(
            &source_dir.join("Agentfile"),
            &BuildOptions {
                output_root: source_dir.join(".dispatch/parcels"),
            },
        )
        .unwrap();

        let prefix = built.digest.chars().take(8).collect::<String>();
        let parcel =
            load_or_build_parcel_for_run(source_dir.join(".dispatch/parcels").join(prefix))
                .unwrap();

        assert_eq!(parcel.config.digest, built.digest);
    }

    #[test]
    fn load_or_build_parcel_for_run_rejects_short_digest_prefix() {
        let dir = tempdir().unwrap();
        let source_dir = dir.path().join("agent");
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(
            source_dir.join("Agentfile"),
            "FROM dispatch/native:latest\nNAME run-prefix-short-test\nENTRYPOINT chat\n",
        )
        .unwrap();

        let built = build_agentfile(
            &source_dir.join("Agentfile"),
            &BuildOptions {
                output_root: source_dir.join(".dispatch/parcels"),
            },
        )
        .unwrap();

        let prefix = built.digest.chars().take(4).collect::<String>();
        let error = load_or_build_parcel_for_run(source_dir.join(".dispatch/parcels").join(prefix))
            .unwrap_err();

        assert!(error.to_string().contains("too short"));
    }
}
