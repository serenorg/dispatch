use super::{
    BuiltinToolSpec, CourierError, CourierOperation, CourierSession, DockerCourier, HostToolRunner,
    Instant, LoadedParcel, LocalToolSpec, LocalToolTarget, ToolApprovalDecision,
    ToolApprovalPolicy, ToolApprovalRequest, ToolApprovalTargetKind, ToolRunResult,
    process_env_lookup,
};
use crate::manifest::TimeoutSpec;
use std::{
    process::{Child, Command, Stdio},
    time::Duration,
};

pub(super) fn configured_timeout_duration(
    timeouts: &[TimeoutSpec],
    scope: &str,
) -> Result<Option<Duration>, CourierError> {
    let Some(timeout) = timeouts
        .iter()
        .rev()
        .find(|timeout| timeout.scope.eq_ignore_ascii_case(scope))
    else {
        return Ok(None);
    };
    parse_timeout_duration(&timeout.duration)
        .map(Some)
        .ok_or_else(|| CourierError::InvalidTimeoutSpec {
            scope: timeout.scope.clone(),
            duration: timeout.duration.clone(),
        })
}

fn parse_timeout_duration(raw: &str) -> Option<Duration> {
    let trimmed = raw.trim();
    let (value, unit) = if let Some(value) = trimmed.strip_suffix("ms") {
        (value, "ms")
    } else if let Some(value) = trimmed.strip_suffix('s') {
        (value, "s")
    } else if let Some(value) = trimmed.strip_suffix('m') {
        (value, "m")
    } else if let Some(value) = trimmed.strip_suffix('h') {
        (value, "h")
    } else {
        return None;
    };
    let amount = value.trim().parse::<u64>().ok()?;
    match unit {
        "ms" => Some(Duration::from_millis(amount)),
        "s" => Some(Duration::from_secs(amount)),
        "m" => Some(Duration::from_secs(amount.saturating_mul(60))),
        "h" => Some(Duration::from_secs(amount.saturating_mul(60 * 60))),
        _ => None,
    }
}

fn wait_for_tool_output(
    mut child: Child,
    tool: &str,
    timeout_spec: Option<(&str, Duration)>,
) -> Result<std::process::Output, CourierError> {
    if let Some((timeout_label, timeout)) = timeout_spec {
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(CourierError::ToolTimedOut {
                        tool: tool.to_string(),
                        timeout: timeout_label.to_string(),
                    });
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(source) => {
                    return Err(CourierError::WaitTool {
                        tool: tool.to_string(),
                        source,
                    });
                }
            }
        }
    }
    child
        .wait_with_output()
        .map_err(|source| CourierError::WaitTool {
            tool: tool.to_string(),
            source,
        })
}

pub(super) fn ensure_run_timeout_budget(
    session: &CourierSession,
    timeouts: &[TimeoutSpec],
) -> Result<(), CourierError> {
    let Some((timeout_duration, timeout_literal)) =
        configured_timeout_duration_with_literal(timeouts, "RUN")?
    else {
        return Ok(());
    };
    let limit_ms = u64::try_from(timeout_duration.as_millis()).unwrap_or(u64::MAX);
    if session.elapsed_ms >= limit_ms {
        return Err(CourierError::RunTimedOut {
            session_id: session.id.clone(),
            timeout: timeout_literal,
        });
    }
    Ok(())
}

fn configured_timeout_duration_with_literal(
    timeouts: &[TimeoutSpec],
    scope: &str,
) -> Result<Option<(Duration, String)>, CourierError> {
    let Some(timeout_spec) = timeouts
        .iter()
        .rev()
        .find(|timeout| timeout.scope.eq_ignore_ascii_case(scope))
    else {
        return Ok(None);
    };
    let Some(timeout) = parse_timeout_duration(&timeout_spec.duration) else {
        return Err(CourierError::InvalidTimeoutSpec {
            scope: timeout_spec.scope.clone(),
            duration: timeout_spec.duration.clone(),
        });
    };
    Ok(Some((timeout, timeout_spec.duration.clone())))
}

fn remaining_run_budget_duration(
    session: &CourierSession,
    timeouts: &[TimeoutSpec],
) -> Result<Option<Duration>, CourierError> {
    let Some((run_timeout, _)) = configured_timeout_duration_with_literal(timeouts, "RUN")? else {
        return Ok(None);
    };
    let limit_ms = u64::try_from(run_timeout.as_millis()).unwrap_or(u64::MAX);
    let remaining_ms = limit_ms.saturating_sub(session.elapsed_ms);
    Ok(Some(Duration::from_millis(remaining_ms)))
}

pub(super) fn remaining_run_budget_with_literal(
    session: &CourierSession,
    timeouts: &[TimeoutSpec],
) -> Result<Option<(String, Duration)>, CourierError> {
    let Some((_, timeout_literal)) = configured_timeout_duration_with_literal(timeouts, "RUN")?
    else {
        return Ok(None);
    };
    Ok(remaining_run_budget_duration(session, timeouts)?
        .map(|remaining| (timeout_literal, remaining)))
}

pub(super) fn run_timeout_deadline(
    session: &CourierSession,
    timeouts: &[TimeoutSpec],
) -> Result<Option<Instant>, CourierError> {
    Ok(remaining_run_budget_duration(session, timeouts)?.map(|duration| Instant::now() + duration))
}

fn remaining_deadline_duration(deadline: Option<Instant>) -> Option<Duration> {
    deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()))
}

fn effective_timeout_spec(
    timeouts: &[TimeoutSpec],
    scope: &str,
    run_deadline: Option<Instant>,
) -> Result<Option<(&'static str, Duration)>, CourierError> {
    let configured = configured_timeout_duration_with_literal(timeouts, scope)?;
    let remaining_run = remaining_deadline_duration(run_deadline);
    Ok(match (configured, remaining_run) {
        (Some((configured_duration, _)), Some(remaining_run_duration)) => {
            if remaining_run_duration < configured_duration {
                Some(("RUN", remaining_run_duration))
            } else {
                Some((scope_to_timeout_label(scope), configured_duration))
            }
        }
        (Some((configured_duration, _)), None) => {
            Some((scope_to_timeout_label(scope), configured_duration))
        }
        (None, Some(remaining_run_duration)) => Some(("RUN", remaining_run_duration)),
        (None, None) => None,
    })
}

fn scope_to_timeout_label(scope: &str) -> &'static str {
    match scope {
        "TOOL" => "TOOL",
        "LLM" => "LLM",
        "RUN" => "RUN",
        _ => "TIMEOUT",
    }
}

pub(super) fn operation_counts_toward_run_budget(operation: &CourierOperation) -> bool {
    match operation {
        CourierOperation::InvokeTool { .. }
        | CourierOperation::Chat { .. }
        | CourierOperation::Job { .. }
        | CourierOperation::Heartbeat { .. } => true,
        CourierOperation::ResolvePrompt | CourierOperation::ListLocalTools => false,
    }
}

pub(super) fn apply_session_run_elapsed(session: &mut CourierSession, started_at: Instant) {
    let elapsed_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    session.elapsed_ms = session.elapsed_ms.saturating_add(elapsed_ms);
}

// Execute a tool whose spec has already been resolved. Callers are responsible
// for validating required secrets before calling this function.
fn execute_local_tool(
    parcel: &LoadedParcel,
    tool: &LocalToolSpec,
    input: Option<&str>,
) -> Result<ToolRunResult, CourierError> {
    execute_local_tool_with_env(parcel, tool, input, None, process_env_lookup)
}

pub(super) fn execute_local_tool_with_env<F>(
    parcel: &LoadedParcel,
    tool: &LocalToolSpec,
    input: Option<&str>,
    run_deadline: Option<Instant>,
    env_lookup: F,
) -> Result<ToolRunResult, CourierError>
where
    F: FnMut(&str) -> Option<String>,
{
    if matches!(tool.target, LocalToolTarget::A2a { .. }) {
        let timeout_spec = effective_timeout_spec(&parcel.config.timeouts, "TOOL", run_deadline)?;
        return super::a2a::execute_a2a_tool_with_env(tool, input, env_lookup, timeout_spec);
    }

    let packaged_path = tool.packaged_path().expect("local tool path");
    let tool_path = parcel.parcel_dir.join("context").join(packaged_path);
    if !tool_path.exists() {
        return Err(CourierError::MissingToolFile {
            tool: tool.alias.clone(),
            path: tool_path.display().to_string(),
        });
    }

    let mut command = Command::new(tool.command());
    command.args(tool.args());
    if tool.command() == packaged_path || tool.args().iter().any(|arg| arg == packaged_path) {
        command.current_dir(parcel.parcel_dir.join("context"));
    } else {
        command.arg(&tool_path);
        command.current_dir(parcel.parcel_dir.join("context"));
    }
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    // Clear the inherited environment so undeclared variables from the parent
    // process (API keys, personal config, etc.) do not leak into tool
    // subprocesses. Only declared ENV vars, the parcel's required secrets, and
    // the minimal system variables needed to locate interpreters are forwarded.
    command.env_clear();
    for (name, value) in forwarded_tool_env_with(parcel, input, env_lookup) {
        command.env(name, value);
    }

    let mut child = command.spawn().map_err(|source| CourierError::SpawnTool {
        tool: tool.alias.clone(),
        source,
    })?;

    if let Some(input) = input
        && let Some(stdin) = child.stdin.as_mut()
    {
        use std::io::Write as _;
        stdin
            .write_all(input.as_bytes())
            .map_err(|source| CourierError::WriteToolInput {
                tool: tool.alias.clone(),
                source,
            })?;
    }
    drop(child.stdin.take());

    let output = wait_for_tool_output(
        child,
        &tool.alias,
        effective_timeout_spec(&parcel.config.timeouts, "TOOL", run_deadline)?,
    )?;

    Ok(ToolRunResult {
        tool: tool.alias.clone(),
        command: tool.command().to_string(),
        args: tool.args().to_vec(),
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

pub(super) fn execute_local_tool_in_docker(
    parcel: &LoadedParcel,
    tool: &LocalToolSpec,
    input: Option<&str>,
    courier: &DockerCourier,
    run_deadline: Option<Instant>,
) -> Result<ToolRunResult, CourierError> {
    if matches!(tool.target, LocalToolTarget::A2a { .. }) {
        let timeout_spec = effective_timeout_spec(&parcel.config.timeouts, "TOOL", run_deadline)?;
        return super::a2a::execute_a2a_tool_with_env(
            tool,
            input,
            process_env_lookup,
            timeout_spec,
        );
    }

    let packaged_path = tool.packaged_path().expect("local tool path");
    let tool_path = parcel.parcel_dir.join("context").join(packaged_path);
    if !tool_path.exists() {
        return Err(CourierError::MissingToolFile {
            tool: tool.alias.clone(),
            path: tool_path.display().to_string(),
        });
    }

    let parcel_root =
        parcel
            .parcel_dir
            .canonicalize()
            .map_err(|source| CourierError::ReadFile {
                path: parcel.parcel_dir.display().to_string(),
                source,
            })?;
    let mount_arg = format!("{}:/workspace:ro", parcel_root.display());
    let mut command = Command::new(&courier.docker_bin);
    command
        .arg("run")
        .arg("--rm")
        .arg("-i")
        .arg("--workdir")
        .arg("/workspace/context")
        .arg("-v")
        .arg(mount_arg);
    for (name, value) in forwarded_tool_env(parcel, input) {
        command.arg("-e").arg(format!("{name}={value}"));
    }
    command.arg(&courier.helper_image);
    command.arg(tool.command());
    command.args(tool.args());
    if tool.command() != packaged_path {
        command.arg(packaged_path);
    }
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|source| CourierError::SpawnTool {
        tool: tool.alias.clone(),
        source,
    })?;

    if let Some(input) = input
        && let Some(stdin) = child.stdin.as_mut()
    {
        use std::io::Write as _;
        stdin
            .write_all(input.as_bytes())
            .map_err(|source| CourierError::WriteToolInput {
                tool: tool.alias.clone(),
                source,
            })?;
    }
    drop(child.stdin.take());

    let output = wait_for_tool_output(
        child,
        &tool.alias,
        effective_timeout_spec(&parcel.config.timeouts, "TOOL", run_deadline)?,
    )?;

    Ok(ToolRunResult {
        tool: tool.alias.clone(),
        command: tool.command().to_string(),
        args: tool.args().to_vec(),
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

pub(super) fn execute_host_local_tool(
    parcel: &LoadedParcel,
    tool: &LocalToolSpec,
    input: Option<&str>,
    runner: HostToolRunner<'_>,
    run_deadline: Option<Instant>,
) -> Result<ToolRunResult, CourierError> {
    match runner {
        HostToolRunner::Native if run_deadline.is_none() => execute_local_tool(parcel, tool, input),
        HostToolRunner::Native => {
            execute_local_tool_with_env(parcel, tool, input, run_deadline, process_env_lookup)
        }
        HostToolRunner::Docker(courier) => {
            execute_local_tool_in_docker(parcel, tool, input, courier, run_deadline)
        }
    }
}

pub(super) fn check_tool_approval(request: &ToolApprovalRequest) -> Result<bool, CourierError> {
    match request.approval {
        ToolApprovalPolicy::Never | ToolApprovalPolicy::Always | ToolApprovalPolicy::Audit => {
            Ok(true)
        }
        ToolApprovalPolicy::Confirm => super::TOOL_APPROVAL_HANDLER.with(|slot| {
            let Some(handler) = slot.borrow().as_ref().cloned() else {
                return Err(CourierError::ApprovalRequired {
                    tool: request.tool.clone(),
                });
            };
            match handler(request) {
                Ok(ToolApprovalDecision::Approve) => Ok(true),
                Ok(ToolApprovalDecision::Deny) => Ok(false),
                Err(message) => Err(CourierError::ApprovalFailed {
                    tool: request.tool.clone(),
                    message,
                }),
            }
        }),
    }
}

pub(super) fn build_local_tool_approval_request(
    tool: &LocalToolSpec,
    input: Option<&str>,
) -> Option<ToolApprovalRequest> {
    let approval = tool.approval?;
    Some(ToolApprovalRequest {
        tool: tool.alias.clone(),
        kind: tool.approval_kind(),
        command: tool.command().to_string(),
        args: tool.args().to_vec(),
        approval,
        risk: tool.risk,
        description: tool.description.clone(),
        skill_source: tool.skill_source.clone(),
        input: input.map(|value| value.to_string()),
    })
}

pub(super) fn build_builtin_tool_approval_request(
    tool: &BuiltinToolSpec,
    input: Option<&str>,
) -> Option<ToolApprovalRequest> {
    let approval = tool.approval?;
    Some(ToolApprovalRequest {
        tool: tool.capability.clone(),
        kind: ToolApprovalTargetKind::Builtin,
        command: "dispatch-builtin".to_string(),
        args: vec![tool.capability.clone()],
        approval,
        risk: tool.risk,
        description: tool.description.clone(),
        skill_source: None,
        input: input.map(|value| value.to_string()),
    })
}

pub(super) fn denied_tool_run_result(request: &ToolApprovalRequest) -> ToolRunResult {
    ToolRunResult {
        tool: request.tool.clone(),
        command: request.command.clone(),
        args: request.args.clone(),
        exit_code: 126,
        stdout: String::new(),
        stderr: format!("tool `{}` was denied by APPROVAL confirm", request.tool),
    }
}

fn forwarded_tool_env(parcel: &LoadedParcel, input: Option<&str>) -> Vec<(String, String)> {
    forwarded_tool_env_with(parcel, input, process_env_lookup)
}

fn forwarded_tool_env_with<F>(
    parcel: &LoadedParcel,
    input: Option<&str>,
    mut env_lookup: F,
) -> Vec<(String, String)>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut env = Vec::new();
    for var in ["PATH", "HOME", "TMPDIR", "TEMP", "TMP"] {
        if let Some(value) = env_lookup(var) {
            env.push((var.to_string(), value));
        }
    }
    for entry in &parcel.config.env {
        env.push((entry.name.clone(), entry.value.clone()));
    }
    for secret in &parcel.config.secrets {
        if let Some(value) = env_lookup(&secret.name) {
            env.push((secret.name.clone(), value));
        }
    }
    if let Some(input) = input {
        env.push(("TOOL_INPUT".to_string(), input.to_string()));
    }
    env
}

pub(super) fn effective_llm_timeout_ms(
    timeouts: &[TimeoutSpec],
    run_deadline: Option<Instant>,
) -> Result<Option<u64>, CourierError> {
    let configured_ms = configured_llm_timeout_ms(timeouts)?;
    let run_remaining_ms = remaining_deadline_duration(run_deadline)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));
    Ok(match (configured_ms, run_remaining_ms) {
        (Some(configured), Some(run_remaining)) => Some(configured.min(run_remaining)),
        (Some(configured), None) => Some(configured),
        (None, Some(run_remaining)) => Some(run_remaining),
        (None, None) => None,
    })
}

pub(super) fn configured_llm_timeout_ms(
    timeouts: &[TimeoutSpec],
) -> Result<Option<u64>, CourierError> {
    configured_timeout_duration(timeouts, "LLM")
        .map(|timeout| timeout.map(|duration| duration.as_millis() as u64))
}
