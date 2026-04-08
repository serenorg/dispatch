use anyhow::{Context, Result, bail};
use dispatch_core::eval::{ToolA2aEndpointExpectation, ToolSchemaExpectation};
use dispatch_core::{
    BuiltinCourier, CourierBackend, CourierEvent, CourierOperation, CourierRequest,
    DISPATCH_TRACE_VERSION, DispatchTraceArtifact, DispatchTraceStep, DockerCourier,
    EvalDatasetDocument, EvalSpec, JsonlCourierPlugin, LoadedParcel, NativeCourier,
    ResolvedCourier, TestSpec, ToolConfig, ToolExitExpectation, ToolInvocation, ToolRunResult,
    ToolTextExpectation, WasmCourier, load_eval_dataset, load_parcel, load_parcel_evals,
    load_parcel_tests, resolve_courier,
};
use futures::executor::block_on;
use jsonschema::Validator;
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

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
    trace_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct EvalReport {
    parcel_digest: String,
    courier: String,
    dataset: Option<String>,
    summary: EvalSummary,
    results: Vec<EvalCaseResult>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct EvalSummary {
    total: usize,
    passed: usize,
    failed: usize,
}

pub(crate) struct EvalOptions {
    pub emit_json: bool,
    pub output_dir: Option<PathBuf>,
    pub dataset: Option<PathBuf>,
    pub trace_dir: Option<PathBuf>,
    pub tool_approval: crate::CliToolApprovalMode,
    pub policy: crate::CliA2aPolicy,
}

type LoadedEvalCases = (Vec<(String, EvalSpec)>, Option<String>);

pub(crate) fn eval(
    path: PathBuf,
    courier_name: &str,
    registry: Option<PathBuf>,
    options: EvalOptions,
) -> Result<()> {
    crate::with_cli_a2a_policy(options.policy, || {
        crate::with_cli_tool_approval(options.tool_approval, || {
            let parcel = load_or_build_parcel_for_eval(path, options.output_dir)?;
            match resolve_courier(courier_name, registry.as_deref())? {
                ResolvedCourier::Builtin(courier) => eval_with_builtin_courier(
                    courier,
                    &parcel,
                    courier_name,
                    options.emit_json,
                    options.dataset.as_deref(),
                    options.trace_dir.as_deref(),
                ),
                ResolvedCourier::Plugin(plugin) => eval_with_courier(
                    JsonlCourierPlugin::new(plugin),
                    &parcel,
                    courier_name,
                    options.emit_json,
                    options.dataset.as_deref(),
                    options.trace_dir.as_deref(),
                ),
            }
        })
    })
}

fn load_or_build_parcel_for_eval(
    path: PathBuf,
    output_dir: Option<PathBuf>,
) -> Result<LoadedParcel> {
    if crate::is_agentfile_target(&path) {
        return crate::build_parcel_from_source(path, output_dir);
    }

    load_parcel(&path).with_context(|| format!("failed to load parcel {}", path.display()))
}

fn eval_with_builtin_courier(
    courier: BuiltinCourier,
    parcel: &LoadedParcel,
    courier_name: &str,
    emit_json: bool,
    dataset: Option<&Path>,
    trace_dir: Option<&Path>,
) -> Result<()> {
    match courier {
        BuiltinCourier::Native => eval_with_courier(
            NativeCourier::default(),
            parcel,
            courier_name,
            emit_json,
            dataset,
            trace_dir,
        ),
        BuiltinCourier::Docker => eval_with_courier(
            DockerCourier::default(),
            parcel,
            courier_name,
            emit_json,
            dataset,
            trace_dir,
        ),
        BuiltinCourier::Wasm => eval_with_courier(
            WasmCourier::new()?,
            parcel,
            courier_name,
            emit_json,
            dataset,
            trace_dir,
        ),
    }
}

fn eval_with_courier<R: CourierBackend>(
    courier: R,
    parcel: &LoadedParcel,
    courier_name: &str,
    emit_json: bool,
    dataset: Option<&Path>,
    trace_dir: Option<&Path>,
) -> Result<()> {
    let (evals, dataset_label) = load_eval_cases(parcel, dataset)?;
    let tests = load_parcel_tests(parcel);
    if evals.is_empty() && tests.is_empty() {
        bail!("parcel does not declare any EVAL files or TEST cases");
    }

    let mut results = evals
        .iter()
        .map(|(packaged_path, spec)| {
            run_eval_case(
                &courier,
                parcel,
                courier_name,
                dataset_label.as_deref(),
                trace_dir,
                packaged_path,
                spec,
            )
        })
        .collect::<Vec<_>>();
    results.extend(tests.iter().map(|spec| {
        run_tool_test_case(
            &courier,
            parcel,
            courier_name,
            dataset_label.as_deref(),
            trace_dir,
            spec,
        )
    }));
    let passed_count = results.iter().filter(|r| r.passed).count();
    let total_count = results.len();
    let report = EvalReport {
        parcel_digest: parcel.config.digest.clone(),
        courier: courier_name.to_string(),
        dataset: dataset_label,
        summary: EvalSummary {
            total: total_count,
            passed: passed_count,
            failed: total_count - passed_count,
        },
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

fn load_eval_cases(parcel: &LoadedParcel, dataset: Option<&Path>) -> Result<LoadedEvalCases> {
    let evals = load_parcel_evals(parcel).context("failed to load parcel evals")?;
    let Some(dataset_path) = dataset else {
        return Ok((evals, None));
    };
    let dataset_doc = load_eval_dataset(dataset_path)
        .with_context(|| format!("failed to load eval dataset {}", dataset_path.display()))?;
    let expanded = apply_eval_dataset(evals, &dataset_doc)?;
    Ok((expanded, Some(dataset_path.display().to_string())))
}

fn apply_eval_dataset(
    evals: Vec<(String, EvalSpec)>,
    dataset: &EvalDatasetDocument,
) -> Result<Vec<(String, EvalSpec)>> {
    let mut indexed = BTreeMap::new();
    for (packaged_path, spec) in evals {
        let key = (packaged_path.clone(), spec.name.clone());
        if indexed.insert(key.clone(), spec).is_some() {
            bail!(
                "dataset fanout requires unique packaged eval keys, but `{}` case `{}` appeared more than once",
                key.0,
                key.1
            );
        }
    }

    let mut expanded = Vec::with_capacity(dataset.cases.len());
    for case in &dataset.cases {
        let Some(base_spec) = indexed.get(&(case.source.clone(), case.case.clone())) else {
            bail!(
                "dataset case `{}` references missing packaged eval `{}` case `{}`",
                case.name,
                case.source,
                case.case
            );
        };
        let mut spec = base_spec.clone();
        spec.name = case.name.clone();
        spec.input = case.input.clone();
        if let Some(entrypoint) = &case.entrypoint {
            spec.entrypoint = Some(entrypoint.clone());
        }
        expanded.push((case.source.clone(), spec));
    }

    Ok(expanded)
}

fn run_eval_case<R: CourierBackend>(
    courier: &R,
    parcel: &LoadedParcel,
    courier_name: &str,
    dataset: Option<&str>,
    trace_dir: Option<&Path>,
    packaged_path: &str,
    spec: &EvalSpec,
) -> EvalCaseResult {
    let started_at_ms = current_time_ms();
    let entrypoint = spec
        .entrypoint
        .clone()
        .or_else(|| parcel.config.entrypoint.clone())
        .unwrap_or_else(|| "chat".to_string());
    let mut trace_steps = vec![DispatchTraceStep::Operation {
        operation: entrypoint.clone(),
        input: Some(spec.input.clone()),
    }];
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
        trace_path: None,
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
            apply_eval_expectations(&mut result, parcel, packaged_path, spec, &[]);
            finalize_trace(
                &mut result,
                parcel,
                courier_name,
                dataset,
                started_at_ms,
                trace_dir,
                trace_steps,
            );
            return result;
        }
    };

    let session = match block_on(courier.open_session(parcel)) {
        Ok(session) => session,
        Err(error) => {
            trace_steps.push(DispatchTraceStep::SessionOpen {
                session_id: None,
                status: "error".to_string(),
                error: Some(error.to_string()),
            });
            result.error = Some(error.to_string());
            apply_eval_expectations(&mut result, parcel, packaged_path, spec, &[]);
            finalize_trace(
                &mut result,
                parcel,
                courier_name,
                dataset,
                started_at_ms,
                trace_dir,
                trace_steps,
            );
            return result;
        }
    };
    trace_steps.push(DispatchTraceStep::SessionOpen {
        session_id: Some(session.id.clone()),
        status: "ok".to_string(),
        error: None,
    });
    if session.parcel_digest != parcel.config.digest {
        result.error = Some(format!(
            "courier returned session for parcel {} while evaluating {}",
            session.parcel_digest, parcel.config.digest
        ));
        apply_eval_expectations(&mut result, parcel, packaged_path, spec, &[]);
        finalize_trace(
            &mut result,
            parcel,
            courier_name,
            dataset,
            started_at_ms,
            trace_dir,
            trace_steps,
        );
        return result;
    }

    let response = match block_on(courier.run(parcel, CourierRequest { session, operation })) {
        Ok(response) => response,
        Err(error) => {
            result.error = Some(error.to_string());
            apply_eval_expectations(&mut result, parcel, packaged_path, spec, &[]);
            finalize_trace(
                &mut result,
                parcel,
                courier_name,
                dataset,
                started_at_ms,
                trace_dir,
                trace_steps,
            );
            return result;
        }
    };
    let mut text_observations = Vec::new();
    for event in &response.events {
        trace_steps.push(DispatchTraceStep::Event {
            event: event.clone(),
        });
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
        apply_eval_expectations(&mut result, parcel, packaged_path, spec, &text_observations);
        finalize_trace(
            &mut result,
            parcel,
            courier_name,
            dataset,
            started_at_ms,
            trace_dir,
            trace_steps,
        );
        return result;
    }

    apply_eval_expectations(&mut result, parcel, packaged_path, spec, &text_observations);
    finalize_trace(
        &mut result,
        parcel,
        courier_name,
        dataset,
        started_at_ms,
        trace_dir,
        trace_steps,
    );
    result
}

fn run_tool_test_case<R: CourierBackend>(
    courier: &R,
    parcel: &LoadedParcel,
    courier_name: &str,
    dataset: Option<&str>,
    trace_dir: Option<&Path>,
    spec: &TestSpec,
) -> EvalCaseResult {
    let started_at_ms = current_time_ms();
    let (name, expected_tool) = match spec {
        TestSpec::Tool { tool } => (format!("tool:{tool}"), tool.as_str()),
    };
    let mut trace_steps = vec![DispatchTraceStep::Operation {
        operation: "tool".to_string(),
        input: None,
    }];
    let mut result = EvalCaseResult {
        name,
        packaged_path: parcel.config.source_agentfile.clone(),
        entrypoint: "tool".to_string(),
        passed: false,
        tool_calls: Vec::new(),
        tool_results: Vec::new(),
        assistant_messages: Vec::new(),
        failures: Vec::new(),
        error: None,
        trace_path: None,
    };

    let session = match block_on(courier.open_session(parcel)) {
        Ok(session) => session,
        Err(error) => {
            trace_steps.push(DispatchTraceStep::SessionOpen {
                session_id: None,
                status: "error".to_string(),
                error: Some(error.to_string()),
            });
            result.error = Some(error.to_string());
            apply_tool_test_expectations(&mut result, expected_tool);
            finalize_trace(
                &mut result,
                parcel,
                courier_name,
                dataset,
                started_at_ms,
                trace_dir,
                trace_steps,
            );
            return result;
        }
    };
    trace_steps.push(DispatchTraceStep::SessionOpen {
        session_id: Some(session.id.clone()),
        status: "ok".to_string(),
        error: None,
    });
    if session.parcel_digest != parcel.config.digest {
        result.error = Some(format!(
            "courier returned session for parcel {} while evaluating {}",
            session.parcel_digest, parcel.config.digest
        ));
        apply_tool_test_expectations(&mut result, expected_tool);
        finalize_trace(
            &mut result,
            parcel,
            courier_name,
            dataset,
            started_at_ms,
            trace_dir,
            trace_steps,
        );
        return result;
    }

    let response = match block_on(courier.run(
        parcel,
        CourierRequest {
            session,
            operation: CourierOperation::InvokeTool {
                invocation: ToolInvocation {
                    name: expected_tool.to_string(),
                    input: None,
                },
            },
        },
    )) {
        Ok(response) => response,
        Err(error) => {
            result.error = Some(error.to_string());
            apply_tool_test_expectations(&mut result, expected_tool);
            finalize_trace(
                &mut result,
                parcel,
                courier_name,
                dataset,
                started_at_ms,
                trace_dir,
                trace_steps,
            );
            return result;
        }
    };
    for event in &response.events {
        trace_steps.push(DispatchTraceStep::Event {
            event: event.clone(),
        });
        match event {
            CourierEvent::ToolCallStarted { invocation, .. } => {
                result.tool_calls.push(invocation.name.clone());
            }
            CourierEvent::ToolCallFinished {
                result: tool_result,
            } => {
                result.tool_results.push(tool_result.clone());
            }
            _ => {}
        }
    }
    if response.session.parcel_digest != parcel.config.digest {
        result.error = Some(format!(
            "courier returned response session for parcel {} while evaluating {}",
            response.session.parcel_digest, parcel.config.digest
        ));
    }

    apply_tool_test_expectations(&mut result, expected_tool);
    finalize_trace(
        &mut result,
        parcel,
        courier_name,
        dataset,
        started_at_ms,
        trace_dir,
        trace_steps,
    );
    result
}

fn finalize_trace(
    result: &mut EvalCaseResult,
    parcel: &LoadedParcel,
    courier: &str,
    dataset: Option<&str>,
    started_at_ms: u64,
    trace_dir: Option<&Path>,
    steps: Vec<DispatchTraceStep>,
) {
    let Some(trace_dir) = trace_dir else {
        return;
    };
    let artifact = DispatchTraceArtifact {
        version: DISPATCH_TRACE_VERSION,
        kind: "parcel_eval_case".to_string(),
        parcel_digest: parcel.config.digest.clone(),
        courier: courier.to_string(),
        dataset: dataset.map(ToString::to_string),
        case_name: result.name.clone(),
        packaged_path: result.packaged_path.clone(),
        entrypoint: result.entrypoint.clone(),
        started_at_ms,
        finished_at_ms: current_time_ms(),
        passed: result.passed,
        failures: result.failures.clone(),
        error: result.error.clone(),
        steps,
    };
    match write_trace_artifact(trace_dir, &artifact) {
        Ok(path) => {
            result.trace_path = Some(path.display().to_string());
        }
        Err(error) => {
            result
                .failures
                .push(format!("failed to write trace artifact: {error}"));
            result.passed = false;
        }
    }
}

fn write_trace_artifact(trace_root: &Path, artifact: &DispatchTraceArtifact) -> Result<PathBuf> {
    let dir = trace_root.join("evals").join(&artifact.parcel_digest);
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create trace directory {}", dir.display()))?;
    let file_name = format!(
        "{}-{}.dispatch-trace.json",
        artifact.started_at_ms,
        trace_slug(&artifact.case_name)
    );
    let path = dir.join(file_name);
    let body = serde_json::to_string_pretty(artifact)?;
    fs::write(&path, body).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

fn trace_slug(value: &str) -> String {
    let mut slug = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if (ch.is_ascii_whitespace() || ch == '-' || ch == '_') && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "trace".to_string()
    } else {
        slug.to_string()
    }
}

fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn apply_tool_test_expectations(result: &mut EvalCaseResult, expected_tool: &str) {
    let matching_results = result
        .tool_results
        .iter()
        .filter(|tool_result| tool_result.tool == expected_tool)
        .collect::<Vec<_>>();
    let invoked =
        result.tool_calls.iter().any(|tool| tool == expected_tool) || !matching_results.is_empty();
    if !invoked {
        result.failures.push(format!(
            "expected tool `{expected_tool}` to be invoked but saw [{}]",
            result.tool_calls.join(", ")
        ));
    }
    if matching_results.is_empty() {
        result.failures.push(format!(
            "expected tool `{expected_tool}` to produce a result"
        ));
    }
    for tool_result in matching_results {
        if tool_result.exit_code != 0 {
            result.failures.push(format!(
                "expected tool `{expected_tool}` to exit with code 0 but saw {}",
                tool_result.exit_code
            ));
        }
    }

    result.passed = result.error.is_none() && result.failures.is_empty();
}

fn apply_eval_expectations(
    result: &mut EvalCaseResult,
    parcel: &LoadedParcel,
    packaged_eval_path: &str,
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

    if spec.expects_no_tool && !result.tool_calls.is_empty() {
        result.failures.push(format!(
            "expected no tool calls but saw [{}]",
            result.tool_calls.join(", ")
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

    if let Some(expected_schema) = &spec.expects_tool_stdout_matches_schema
        && let Err(message) = validate_tool_stdout_schema(
            parcel,
            packaged_eval_path,
            &result.tool_results,
            expected_schema,
        )
    {
        result.failures.push(message);
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

    if let Some(expected_endpoint) = &spec.expects_a2a_endpoint
        && let Err(message) =
            validate_a2a_endpoint_expectation(parcel, &result.tool_calls, expected_endpoint)
    {
        result.failures.push(message);
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

fn validate_tool_stdout_schema(
    parcel: &LoadedParcel,
    packaged_eval_path: &str,
    tool_results: &[ToolRunResult],
    expectation: &ToolSchemaExpectation,
) -> Result<(), String> {
    let (tool_name, schema_path) = match expectation {
        ToolSchemaExpectation::Schema(schema) => (None, schema.as_str()),
        ToolSchemaExpectation::Scoped { tool, schema } => (Some(tool.as_str()), schema.as_str()),
    };
    let Some(tool_result) = tool_results.iter().find(|tool_result| match tool_name {
        Some(tool) => tool_result.tool == tool,
        None => true,
    }) else {
        return Err(match tool_name {
            Some(tool) => format!("expected tool `{tool}` output for schema validation"),
            None => "expected at least one tool output for schema validation".to_string(),
        });
    };

    let stdout_json: serde_json::Value =
        serde_json::from_str(&tool_result.stdout).map_err(|error| match tool_name {
            Some(tool) => format!("expected tool `{tool}` stdout to be JSON: {error}"),
            None => format!("expected tool stdout to be JSON: {error}"),
        })?;
    let schema = load_eval_relative_json(parcel, packaged_eval_path, schema_path)
        .map_err(|error| format!("failed to load schema `{schema_path}`: {error}"))?;
    let validator = jsonschema::validator_for(&schema)
        .map_err(|error| format!("invalid schema `{schema_path}`: {error}"))?;
    validate_json_against_schema(
        &validator,
        &stdout_json,
        tool_name.unwrap_or(tool_result.tool.as_str()),
    )
}

fn validate_json_against_schema(
    validator: &Validator,
    value: &serde_json::Value,
    label: &str,
) -> Result<(), String> {
    let mut errors = validator.iter_errors(value);
    match errors.next() {
        Some(error) => Err(format!(
            "expected tool `{label}` stdout to match schema: {error}"
        )),
        None => Ok(()),
    }
}

fn validate_a2a_endpoint_expectation(
    parcel: &LoadedParcel,
    tool_calls: &[String],
    expectation: &ToolA2aEndpointExpectation,
) -> Result<(), String> {
    let (tool_name, expected_url) = match expectation {
        ToolA2aEndpointExpectation::Url(url) => (None, url.as_str()),
        ToolA2aEndpointExpectation::Scoped { tool, url } => (Some(tool.as_str()), url.as_str()),
    };
    let Some(a2a_alias) = parcel.config.tools.iter().find_map(|tool| match tool {
        ToolConfig::A2a(a2a)
            if match tool_name {
                Some(expected_tool_name) => a2a.alias == expected_tool_name,
                None => true,
            } && a2a.url == expected_url
                && tool_calls.iter().any(|called| called == &a2a.alias) =>
        {
            Some(a2a.alias.as_str())
        }
        _ => None,
    }) else {
        return Err(match tool_name {
            Some(tool) => format!("expected A2A tool `{tool}` to call endpoint `{expected_url}`"),
            None => format!("expected an A2A tool call to endpoint `{expected_url}`"),
        });
    };
    if let Some(tool_name) = tool_name
        && a2a_alias != tool_name
    {
        return Err(format!(
            "expected A2A tool `{tool_name}` to call endpoint `{expected_url}`"
        ));
    }
    Ok(())
}

fn load_eval_relative_json(
    parcel: &LoadedParcel,
    packaged_eval_path: &str,
    relative_path: &str,
) -> Result<serde_json::Value> {
    let context_root = parcel.parcel_dir.join("context");
    let context_root = context_root
        .canonicalize()
        .map_err(|error| anyhow::anyhow!("failed to access parcel context: {error}"))?;
    let eval_dir = Path::new(packaged_eval_path)
        .parent()
        .map(PathBuf::from)
        .unwrap_or_default();
    let candidate = context_root.join(eval_dir).join(relative_path);
    let resolved = candidate.canonicalize().map_err(|error| {
        anyhow::anyhow!("failed to resolve path {}: {error}", candidate.display())
    })?;
    if !resolved.starts_with(&context_root) {
        bail!("path `{relative_path}` resolves outside parcel context");
    }
    let source = fs::read_to_string(&resolved)
        .with_context(|| format!("failed to read {}", resolved.display()))?;
    serde_json::from_str(&source)
        .with_context(|| format!("failed to parse JSON from {}", resolved.display()))
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
    print!("{}", render_eval_report(report));
}

fn render_eval_report(report: &EvalReport) -> String {
    let mut output = String::new();
    let _ = writeln!(
        output,
        "Parcel {} on courier `{}`",
        report.parcel_digest, report.courier
    );
    for result in &report.results {
        let status = if result.passed { "PASS" } else { "FAIL" };
        let _ = writeln!(
            output,
            "{status} {} ({})",
            result.name, result.packaged_path
        );
        if !result.tool_calls.is_empty() {
            let _ = writeln!(output, "tools: {}", result.tool_calls.join(", "));
        }
        for tool_result in &result.tool_results {
            let _ = writeln!(
                output,
                "tool-result: {} exit={} stdout={} stderr={}",
                tool_result.tool, tool_result.exit_code, tool_result.stdout, tool_result.stderr
            );
        }
        if !result.assistant_messages.is_empty() {
            let _ = writeln!(
                output,
                "assistant: {}",
                result.assistant_messages.join(" | ")
            );
        }
        if let Some(error) = &result.error {
            let _ = writeln!(output, "error: {error}");
        }
        if let Some(trace_path) = &result.trace_path {
            let _ = writeln!(output, "trace: {trace_path}");
        }
        for failure in &result.failures {
            let _ = writeln!(output, "failure: {failure}");
        }
    }
    let _ = writeln!(
        output,
        "\n{} passed, {} failed, {} total",
        report.summary.passed, report.summary.failed, report.summary.total
    );
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use dispatch_core::{BuildOptions, build_agentfile};
    use tempfile::tempdir;

    fn sample_eval_report() -> EvalReport {
        EvalReport {
            parcel_digest: "abc123".to_string(),
            courier: "native".to_string(),
            dataset: Some("regression.dataset.toml".to_string()),
            summary: EvalSummary {
                total: 2,
                passed: 1,
                failed: 1,
            },
            results: vec![
                EvalCaseResult {
                    name: "pass-case".to_string(),
                    packaged_path: "evals/pass.eval".to_string(),
                    entrypoint: "chat".to_string(),
                    passed: true,
                    tool_calls: vec![],
                    tool_results: vec![],
                    assistant_messages: vec!["done".to_string()],
                    failures: vec![],
                    error: None,
                    trace_path: Some(
                        ".dispatch/traces/evals/abc123/pass.dispatch-trace.json".to_string(),
                    ),
                },
                EvalCaseResult {
                    name: "fail-case".to_string(),
                    packaged_path: "evals/fail.eval".to_string(),
                    entrypoint: "chat".to_string(),
                    passed: false,
                    tool_calls: vec!["demo".to_string()],
                    tool_results: vec![],
                    assistant_messages: vec![],
                    failures: vec!["expected tool".to_string()],
                    error: Some("eval failed".to_string()),
                    trace_path: None,
                },
            ],
        }
    }

    #[test]
    fn eval_report_json_includes_summary_counts() {
        let json = serde_json::to_value(sample_eval_report()).unwrap();

        assert_eq!(json["summary"]["total"], 2);
        assert_eq!(json["summary"]["passed"], 1);
        assert_eq!(json["summary"]["failed"], 1);
    }

    #[test]
    fn render_eval_report_includes_summary_line() {
        let rendered = render_eval_report(&sample_eval_report());

        assert!(rendered.contains("1 passed, 1 failed, 2 total"));
        assert!(rendered.contains("PASS pass-case (evals/pass.eval)"));
        assert!(rendered.contains("FAIL fail-case (evals/fail.eval)"));
    }

    fn smoke_test_tool_path() -> &'static str {
        if cfg!(windows) {
            "scripts/demo.cmd"
        } else {
            "scripts/demo.sh"
        }
    }

    fn smoke_test_tool_success_body() -> &'static str {
        if cfg!(windows) {
            "@echo off\r\necho ok\r\n"
        } else {
            "#!/bin/sh\necho ok\n"
        }
    }

    fn smoke_test_tool_failure_body() -> &'static str {
        if cfg!(windows) {
            "@echo off\r\nexit /b 7\r\n"
        } else {
            "#!/bin/sh\nexit 7\n"
        }
    }

    fn build_tool_test_parcel(script_body: &str) -> (tempfile::TempDir, LoadedParcel) {
        let agentfile = format!(
            "FROM dispatch/native:latest\nTOOL LOCAL {} AS demo\nTEST tool:demo\nENTRYPOINT chat\n",
            smoke_test_tool_path()
        );
        build_eval_parcel(&agentfile, &[(smoke_test_tool_path(), script_body)])
    }

    fn build_eval_parcel(
        agentfile: &str,
        files: &[(&str, &str)],
    ) -> (tempfile::TempDir, LoadedParcel) {
        let dir = tempdir().unwrap();
        let context_dir = dir.path().join("image");
        fs::create_dir_all(&context_dir).unwrap();
        fs::write(context_dir.join("Agentfile"), agentfile).unwrap();
        for (path, contents) in files {
            let full = context_dir.join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(full, contents).unwrap();
        }
        let built = build_agentfile(
            &context_dir.join("Agentfile"),
            &BuildOptions {
                output_root: context_dir.join(".dispatch/parcels"),
            },
        )
        .unwrap();
        let parcel = load_parcel(&built.parcel_dir).unwrap();
        (dir, parcel)
    }

    #[test]
    fn load_eval_cases_applies_dataset_file() {
        let (dir, parcel) = build_eval_parcel(
            concat!(
                "FROM dispatch/native:latest\n",
                "EVAL evals/smoke.eval\n",
                "ENTRYPOINT chat\n",
            ),
            &[("evals/smoke.eval", "name = \"smoke\"\ninput = \"base\"\n")],
        );
        let dataset_path = dir.path().join("regression.dataset.toml");
        fs::write(
            &dataset_path,
            concat!(
                "version = 1\n\n",
                "[[cases]]\n",
                "name = \"dataset-smoke\"\n",
                "source = \"evals/smoke.eval\"\n",
                "case = \"smoke\"\n",
                "input = \"dataset input\"\n",
            ),
        )
        .unwrap();

        let (evals, dataset_label) = load_eval_cases(&parcel, Some(&dataset_path)).unwrap();

        assert_eq!(
            dataset_label.as_deref(),
            Some(dataset_path.to_str().unwrap())
        );
        assert_eq!(evals.len(), 1);
        assert_eq!(evals[0].0, "evals/smoke.eval");
        assert_eq!(evals[0].1.name, "dataset-smoke");
        assert_eq!(evals[0].1.input, "dataset input");
    }

    #[test]
    fn apply_eval_dataset_overrides_input_and_entrypoint() {
        let expanded = apply_eval_dataset(
            vec![(
                "evals/smoke.eval".to_string(),
                EvalSpec {
                    name: "smoke".to_string(),
                    input: "base".to_string(),
                    entrypoint: Some("chat".to_string()),
                    expects_tool: Some("system_time".to_string()),
                    expects_text: None,
                    expects_text_exact: None,
                    expects_text_not_contains: None,
                    expects_tool_count: None,
                    expects_tools: Vec::new(),
                    expects_no_tool: false,
                    expects_tool_stdout_contains: None,
                    expects_tool_stdout_matches_schema: None,
                    expects_tool_stderr_contains: None,
                    expects_tool_exit_code: None,
                    expects_a2a_endpoint: None,
                    expects_error_contains: None,
                },
            )],
            &EvalDatasetDocument {
                version: 1,
                cases: vec![dispatch_core::EvalDatasetCase {
                    name: "utc-smoke".to_string(),
                    source: "evals/smoke.eval".to_string(),
                    case: "smoke".to_string(),
                    input: "what time is it in UTC?".to_string(),
                    entrypoint: Some("job".to_string()),
                }],
            },
        )
        .unwrap();

        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0].0, "evals/smoke.eval");
        assert_eq!(expanded[0].1.name, "utc-smoke");
        assert_eq!(expanded[0].1.input, "what time is it in UTC?");
        assert_eq!(expanded[0].1.entrypoint.as_deref(), Some("job"));
        assert_eq!(expanded[0].1.expects_tool.as_deref(), Some("system_time"));
    }

    #[test]
    fn apply_eval_dataset_rejects_missing_packaged_case() {
        let error = apply_eval_dataset(
            vec![(
                "evals/smoke.eval".to_string(),
                EvalSpec {
                    name: "smoke".to_string(),
                    input: "base".to_string(),
                    entrypoint: Some("chat".to_string()),
                    expects_tool: None,
                    expects_text: None,
                    expects_text_exact: None,
                    expects_text_not_contains: None,
                    expects_tool_count: None,
                    expects_tools: Vec::new(),
                    expects_no_tool: false,
                    expects_tool_stdout_contains: None,
                    expects_tool_stdout_matches_schema: None,
                    expects_tool_stderr_contains: None,
                    expects_tool_exit_code: None,
                    expects_a2a_endpoint: None,
                    expects_error_contains: None,
                },
            )],
            &EvalDatasetDocument {
                version: 1,
                cases: vec![dispatch_core::EvalDatasetCase {
                    name: "missing".to_string(),
                    source: "evals/other.eval".to_string(),
                    case: "missing".to_string(),
                    input: "hello".to_string(),
                    entrypoint: None,
                }],
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("references missing packaged eval `evals/other.eval` case `missing`")
        );
    }

    #[test]
    fn apply_eval_expectations_validates_tool_stdout_against_schema() {
        let (_dir, parcel) = build_eval_parcel(
            concat!(
                "FROM dispatch/native:latest\n",
                "TOOL LOCAL scripts/demo.sh AS demo SCHEMA schemas/output.json\n",
                "EVAL evals/output.eval\n",
                "ENTRYPOINT chat\n",
            ),
            &[
                ("evals/output.eval", "name = \"demo\"\ninput = \"hi\"\n"),
                ("scripts/demo.sh", "#!/bin/sh\necho ok\n"),
                (
                    "schemas/output.json",
                    r#"{"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false}"#,
                ),
            ],
        );
        let mut result = EvalCaseResult {
            name: "demo".to_string(),
            packaged_path: "evals/output.eval".to_string(),
            entrypoint: "chat".to_string(),
            passed: false,
            tool_calls: vec!["demo".to_string()],
            tool_results: vec![ToolRunResult {
                tool: "demo".to_string(),
                command: "sh".to_string(),
                args: Vec::new(),
                exit_code: 0,
                stdout: r#"{"ok":true}"#.to_string(),
                stderr: String::new(),
            }],
            assistant_messages: Vec::new(),
            failures: Vec::new(),
            error: None,
            trace_path: None,
        };
        let spec = EvalSpec {
            name: "demo".to_string(),
            input: "hi".to_string(),
            entrypoint: Some("chat".to_string()),
            expects_tool: None,
            expects_text: None,
            expects_text_exact: None,
            expects_text_not_contains: None,
            expects_tool_count: None,
            expects_tools: Vec::new(),
            expects_no_tool: false,
            expects_tool_stdout_contains: None,
            expects_tool_stdout_matches_schema: Some(ToolSchemaExpectation::Scoped {
                tool: "demo".to_string(),
                schema: "../schemas/output.json".to_string(),
            }),
            expects_tool_stderr_contains: None,
            expects_tool_exit_code: None,
            expects_a2a_endpoint: None,
            expects_error_contains: None,
        };

        apply_eval_expectations(&mut result, &parcel, "evals/output.eval", &spec, &[]);
        assert!(result.failures.is_empty(), "{:?}", result.failures);
        assert!(result.passed);
    }

    #[test]
    fn apply_eval_expectations_rejects_wrong_a2a_endpoint() {
        let (_dir, parcel) = build_eval_parcel(
            concat!(
                "FROM dispatch/native:latest\n",
                "TOOL A2A broker URL https://broker.example.com\n",
                "EVAL evals/output.eval\n",
                "ENTRYPOINT chat\n",
            ),
            &[("evals/output.eval", "name = \"demo\"\ninput = \"hi\"\n")],
        );
        let mut result = EvalCaseResult {
            name: "demo".to_string(),
            packaged_path: "evals/output.eval".to_string(),
            entrypoint: "chat".to_string(),
            passed: false,
            tool_calls: vec!["broker".to_string()],
            tool_results: Vec::new(),
            assistant_messages: Vec::new(),
            failures: Vec::new(),
            error: None,
            trace_path: None,
        };
        let spec = EvalSpec {
            name: "demo".to_string(),
            input: "hi".to_string(),
            entrypoint: Some("chat".to_string()),
            expects_tool: None,
            expects_text: None,
            expects_text_exact: None,
            expects_text_not_contains: None,
            expects_tool_count: None,
            expects_tools: Vec::new(),
            expects_no_tool: false,
            expects_tool_stdout_contains: None,
            expects_tool_stdout_matches_schema: None,
            expects_tool_stderr_contains: None,
            expects_tool_exit_code: None,
            expects_a2a_endpoint: Some(ToolA2aEndpointExpectation::Scoped {
                tool: "broker".to_string(),
                url: "https://other.example.com".to_string(),
            }),
            expects_error_contains: None,
        };

        apply_eval_expectations(&mut result, &parcel, "evals/output.eval", &spec, &[]);
        assert_eq!(
            result.failures,
            vec!["expected A2A tool `broker` to call endpoint `https://other.example.com`"]
        );
        assert!(!result.passed);
    }

    #[test]
    fn run_tool_test_case_passes_for_successful_tool_smoke_test() {
        let (_dir, parcel) = build_tool_test_parcel(smoke_test_tool_success_body());

        let result = run_tool_test_case(
            &NativeCourier::default(),
            &parcel,
            "native",
            None,
            None,
            &TestSpec::Tool {
                tool: "demo".to_string(),
            },
        );

        assert!(result.passed, "{result:?}");
        assert_eq!(result.tool_calls, vec!["demo".to_string()]);
        assert_eq!(result.tool_results.len(), 1);
        assert_eq!(result.tool_results[0].exit_code, 0);
    }

    #[test]
    fn run_tool_test_case_fails_when_tool_returns_nonzero_exit() {
        let (_dir, parcel) = build_tool_test_parcel(smoke_test_tool_failure_body());

        let result = run_tool_test_case(
            &NativeCourier::default(),
            &parcel,
            "native",
            None,
            None,
            &TestSpec::Tool {
                tool: "demo".to_string(),
            },
        );

        assert!(!result.passed);
        assert_eq!(
            result.failures,
            vec!["expected tool `demo` to exit with code 0 but saw 7".to_string()]
        );
    }

    #[test]
    fn eval_with_courier_accepts_test_only_parcels() {
        let (_dir, parcel) = build_tool_test_parcel(smoke_test_tool_success_body());

        let outcome = eval_with_courier(
            NativeCourier::default(),
            &parcel,
            "native",
            true,
            None,
            None,
        );

        assert!(outcome.is_ok(), "{outcome:?}");
    }

    #[test]
    fn run_tool_test_case_writes_trace_artifact() {
        let (dir, parcel) = build_tool_test_parcel(smoke_test_tool_success_body());
        let trace_dir = dir.path().join("traces");

        let result = run_tool_test_case(
            &NativeCourier::default(),
            &parcel,
            "native",
            Some("evals/regression.dataset.toml"),
            Some(&trace_dir),
            &TestSpec::Tool {
                tool: "demo".to_string(),
            },
        );

        assert!(result.passed, "{result:?}");
        let trace_path = PathBuf::from(result.trace_path.as_ref().unwrap());
        assert!(trace_path.is_file());

        let artifact: DispatchTraceArtifact =
            serde_json::from_str(&fs::read_to_string(&trace_path).unwrap()).unwrap();
        assert_eq!(artifact.kind, "parcel_eval_case");
        assert_eq!(
            artifact.dataset.as_deref(),
            Some("evals/regression.dataset.toml")
        );
        assert_eq!(artifact.case_name, "tool:demo");
        assert!(!artifact.steps.is_empty());
    }
}
