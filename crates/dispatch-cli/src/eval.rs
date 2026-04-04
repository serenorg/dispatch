use anyhow::{Context, Result, bail};
use dispatch_core::{
    BuildOptions, BuiltinCourier, CourierBackend, CourierEvent, CourierOperation, CourierRequest,
    DockerCourier, EvalSpec, JsonlCourierPlugin, LoadedParcel, NativeCourier, ResolvedCourier,
    ToolExitExpectation, ToolRunResult, ToolTextExpectation, WasmCourier, build_agentfile,
    load_parcel, load_parcel_evals, resolve_courier,
};
use futures::executor::block_on;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct EvalCaseResult {
    name: String,
    packaged_path: String,
    entrypoint: String,
    passed: bool,
    tool_calls: Vec<String>,
    tool_results: Vec<ToolRunResult>,
    assistant_messages: Vec<String>,
    failures: Vec<String>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct EvalReport {
    parcel_digest: String,
    courier: String,
    results: Vec<EvalCaseResult>,
}

pub(crate) fn eval(
    path: PathBuf,
    courier_name: &str,
    registry: Option<PathBuf>,
    emit_json: bool,
    output_dir: Option<PathBuf>,
    policy: crate::CliA2aPolicy,
) -> Result<()> {
    crate::with_cli_a2a_policy(policy, || {
        let parcel = load_or_build_parcel_for_eval(path, output_dir)?;
        match resolve_courier(courier_name, registry.as_deref())? {
            ResolvedCourier::Builtin(courier) => {
                eval_with_builtin_courier(courier, &parcel, courier_name, emit_json)
            }
            ResolvedCourier::Plugin(plugin) => eval_with_courier(
                JsonlCourierPlugin::new(plugin),
                &parcel,
                courier_name,
                emit_json,
            ),
        }
    })
}

fn load_or_build_parcel_for_eval(
    path: PathBuf,
    output_dir: Option<PathBuf>,
) -> Result<LoadedParcel> {
    if is_agentfile_target(&path) {
        let agentfile_path = resolve_agentfile_path(path);
        let context_dir = agentfile_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let output_root = output_dir.unwrap_or_else(|| context_dir.join(".dispatch/parcels"));
        let built = build_agentfile(
            &agentfile_path,
            &BuildOptions {
                output_root: output_root.clone(),
            },
        )
        .with_context(|| format!("failed to build {}", agentfile_path.display()))?;
        return load_parcel(&built.parcel_dir)
            .with_context(|| format!("failed to load parcel {}", built.parcel_dir.display()));
    }

    load_parcel(&path).with_context(|| format!("failed to load parcel {}", path.display()))
}

fn is_agentfile_target(path: &Path) -> bool {
    if path.is_dir() {
        return path.join("Agentfile").exists();
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "Agentfile")
}

fn resolve_agentfile_path(path: PathBuf) -> PathBuf {
    if path.is_dir() {
        path.join("Agentfile")
    } else {
        path
    }
}

fn eval_with_builtin_courier(
    courier: BuiltinCourier,
    parcel: &LoadedParcel,
    courier_name: &str,
    emit_json: bool,
) -> Result<()> {
    match courier {
        BuiltinCourier::Native => {
            eval_with_courier(NativeCourier::default(), parcel, courier_name, emit_json)
        }
        BuiltinCourier::Docker => {
            eval_with_courier(DockerCourier::default(), parcel, courier_name, emit_json)
        }
        BuiltinCourier::Wasm => {
            eval_with_courier(WasmCourier::default(), parcel, courier_name, emit_json)
        }
    }
}

fn eval_with_courier<R: CourierBackend>(
    courier: R,
    parcel: &LoadedParcel,
    courier_name: &str,
    emit_json: bool,
) -> Result<()> {
    let evals = load_parcel_evals(parcel).context("failed to load parcel evals")?;
    if evals.is_empty() {
        bail!("parcel does not declare any EVAL files");
    }

    let results = evals
        .iter()
        .map(|(packaged_path, spec)| run_eval_case(&courier, parcel, packaged_path, spec))
        .collect::<Vec<_>>();
    let report = EvalReport {
        parcel_digest: parcel.config.digest.clone(),
        courier: courier_name.to_string(),
        results,
    };

    if emit_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_eval_report(&report);
    }

    if report.results.iter().all(|result| result.passed) {
        Ok(())
    } else {
        bail!("eval failed")
    }
}

fn run_eval_case<R: CourierBackend>(
    courier: &R,
    parcel: &LoadedParcel,
    packaged_path: &str,
    spec: &EvalSpec,
) -> EvalCaseResult {
    let entrypoint = spec
        .entrypoint
        .clone()
        .or_else(|| parcel.config.entrypoint.clone())
        .unwrap_or_else(|| "chat".to_string());
    let mut result = EvalCaseResult {
        name: spec.name.clone(),
        packaged_path: packaged_path.to_string(),
        entrypoint: entrypoint.clone(),
        passed: false,
        tool_calls: Vec::new(),
        tool_results: Vec::new(),
        assistant_messages: Vec::new(),
        failures: Vec::new(),
        error: None,
    };

    let operation = match entrypoint.as_str() {
        "chat" => CourierOperation::Chat {
            input: spec.input.clone(),
        },
        "job" => CourierOperation::Job {
            payload: spec.input.clone(),
        },
        "heartbeat" => CourierOperation::Heartbeat {
            payload: if spec.input.is_empty() {
                None
            } else {
                Some(spec.input.clone())
            },
        },
        other => {
            result.error = Some(format!("unsupported eval entrypoint `{other}`"));
            apply_eval_expectations(&mut result, spec, &[]);
            return result;
        }
    };

    let session = match block_on(courier.open_session(parcel)) {
        Ok(session) => session,
        Err(error) => {
            result.error = Some(error.to_string());
            apply_eval_expectations(&mut result, spec, &[]);
            return result;
        }
    };
    if session.parcel_digest != parcel.config.digest {
        result.error = Some(format!(
            "courier returned session for parcel {} while evaluating {}",
            session.parcel_digest, parcel.config.digest
        ));
        apply_eval_expectations(&mut result, spec, &[]);
        return result;
    }

    let response = match block_on(courier.run(parcel, CourierRequest { session, operation })) {
        Ok(response) => response,
        Err(error) => {
            result.error = Some(error.to_string());
            apply_eval_expectations(&mut result, spec, &[]);
            return result;
        }
    };
    let mut text_observations = Vec::new();
    for event in &response.events {
        match event {
            CourierEvent::ToolCallStarted { invocation, .. } => {
                result.tool_calls.push(invocation.name.clone());
            }
            CourierEvent::ToolCallFinished {
                result: tool_result,
            } => {
                result.tool_results.push(tool_result.clone());
            }
            CourierEvent::Message { role, content } if role == "assistant" => {
                result.assistant_messages.push(content.clone());
                text_observations.push(content.clone());
            }
            CourierEvent::TextDelta { content } => text_observations.push(content.clone()),
            _ => {}
        }
    }
    if response.session.parcel_digest != parcel.config.digest {
        result.error = Some(format!(
            "courier returned response session for parcel {} while evaluating {}",
            response.session.parcel_digest, parcel.config.digest
        ));
        apply_eval_expectations(&mut result, spec, &text_observations);
        return result;
    }

    apply_eval_expectations(&mut result, spec, &text_observations);
    result
}

fn apply_eval_expectations(
    result: &mut EvalCaseResult,
    spec: &EvalSpec,
    text_observations: &[String],
) {
    let mut expected_tools = spec.expects_tools.clone();
    if let Some(expected_tool) = &spec.expects_tool {
        expected_tools.push(expected_tool.clone());
    }

    for expected_tool in expected_tools {
        if !result.tool_calls.iter().any(|tool| tool == &expected_tool) {
            result.failures.push(format!(
                "expected tool `{expected_tool}` but saw [{}]",
                result.tool_calls.join(", ")
            ));
        }
    }

    if let Some(expected_tool_count) = spec.expects_tool_count
        && result.tool_calls.len() != expected_tool_count
    {
        result.failures.push(format!(
            "expected {expected_tool_count} tool call(s) but saw {}",
            result.tool_calls.len()
        ));
    }

    if let Some(expected_text) = &spec.expects_text
        && !text_observations
            .iter()
            .any(|observed| observed.contains(expected_text))
    {
        result.failures.push(format!(
            "expected assistant text containing `{expected_text}`"
        ));
    }

    if let Some(expected_text_exact) = &spec.expects_text_exact
        && !text_observations
            .iter()
            .any(|observed| observed == expected_text_exact)
    {
        result.failures.push(format!(
            "expected assistant text exactly `{expected_text_exact}`"
        ));
    }

    if let Some(unexpected_text) = &spec.expects_text_not_contains
        && text_observations
            .iter()
            .any(|observed| observed.contains(unexpected_text))
    {
        result.failures.push(format!(
            "expected assistant text not to contain `{unexpected_text}`"
        ));
    }

    if let Some(expected_stdout) = &spec.expects_tool_stdout_contains
        && !tool_text_expectation_satisfied(&result.tool_results, expected_stdout, |tool_result| {
            &tool_result.stdout
        })
    {
        result.failures.push(match expected_stdout {
            ToolTextExpectation::Contains(expected) => {
                format!("expected tool stdout containing `{expected}`")
            }
            ToolTextExpectation::Scoped { tool, contains } => {
                format!("expected tool `{tool}` stdout containing `{contains}`")
            }
        });
    }

    if let Some(expected_stderr) = &spec.expects_tool_stderr_contains
        && !tool_text_expectation_satisfied(&result.tool_results, expected_stderr, |tool_result| {
            &tool_result.stderr
        })
    {
        result.failures.push(match expected_stderr {
            ToolTextExpectation::Contains(expected) => {
                format!("expected tool stderr containing `{expected}`")
            }
            ToolTextExpectation::Scoped { tool, contains } => {
                format!("expected tool `{tool}` stderr containing `{contains}`")
            }
        });
    }

    if let Some(expected_exit_code) = &spec.expects_tool_exit_code
        && !tool_exit_expectation_satisfied(&result.tool_results, expected_exit_code)
    {
        result.failures.push(match expected_exit_code {
            ToolExitExpectation::ExitCode(exit_code) => {
                format!("expected tool exit code `{exit_code}`")
            }
            ToolExitExpectation::Scoped { tool, exit_code } => {
                format!("expected tool `{tool}` exit code `{exit_code}`")
            }
        });
    }

    let error_expectation_satisfied = if let Some(expected_error) = &spec.expects_error_contains {
        match &result.error {
            Some(error) if error.contains(expected_error) => true,
            Some(error) => {
                result.failures.push(format!(
                    "expected error containing `{expected_error}` but saw `{error}`"
                ));
                false
            }
            None => {
                result.failures.push(format!(
                    "expected error containing `{expected_error}` but no error occurred"
                ));
                false
            }
        }
    } else {
        result.error.is_none()
    };

    result.passed = error_expectation_satisfied && result.failures.is_empty();
}

pub(crate) fn tool_text_expectation_satisfied(
    tool_results: &[ToolRunResult],
    expectation: &ToolTextExpectation,
    field: impl Fn(&ToolRunResult) -> &str,
) -> bool {
    match expectation {
        ToolTextExpectation::Contains(expected) => tool_results
            .iter()
            .any(|tool_result| field(tool_result).contains(expected)),
        ToolTextExpectation::Scoped { tool, contains } => tool_results
            .iter()
            .any(|tool_result| tool_result.tool == *tool && field(tool_result).contains(contains)),
    }
}

pub(crate) fn tool_exit_expectation_satisfied(
    tool_results: &[ToolRunResult],
    expectation: &ToolExitExpectation,
) -> bool {
    match expectation {
        ToolExitExpectation::ExitCode(exit_code) => tool_results
            .iter()
            .any(|tool_result| tool_result.exit_code == *exit_code),
        ToolExitExpectation::Scoped { tool, exit_code } => tool_results
            .iter()
            .any(|tool_result| tool_result.tool == *tool && tool_result.exit_code == *exit_code),
    }
}

fn print_eval_report(report: &EvalReport) {
    println!(
        "Parcel {} on courier `{}`",
        report.parcel_digest, report.courier
    );
    for result in &report.results {
        let status = if result.passed { "PASS" } else { "FAIL" };
        println!("{status} {} ({})", result.name, result.packaged_path);
        if !result.tool_calls.is_empty() {
            println!("tools: {}", result.tool_calls.join(", "));
        }
        for tool_result in &result.tool_results {
            println!(
                "tool-result: {} exit={} stdout={} stderr={}",
                tool_result.tool, tool_result.exit_code, tool_result.stdout, tool_result.stderr
            );
        }
        if !result.assistant_messages.is_empty() {
            println!("assistant: {}", result.assistant_messages.join(" | "));
        }
        if let Some(error) = &result.error {
            println!("error: {error}");
        }
        for failure in &result.failures {
            println!("failure: {failure}");
        }
    }
}
