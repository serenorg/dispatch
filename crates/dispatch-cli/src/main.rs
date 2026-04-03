use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use dispatch_core::{
    BuildOptions, BuiltinCourier, CourierBackend, CourierCapabilities, CourierCatalogEntry,
    CourierEvent, CourierInspection, CourierKind, CourierOperation, CourierPluginManifest,
    CourierRequest, CourierSession, DockerCourier, JsonlCourierPlugin, Level, LoadedParcel,
    NativeCourier, ParcelManifest, ResolvedCourier, SignatureVerification, ToolInvocation,
    ToolRunResult, VerificationReport, WasmCourier, build_agentfile, default_courier_registry_path,
    generate_keypair_files, install_courier_plugin, list_courier_catalog, load_parcel,
    parse_agentfile, parse_depot_reference, pull_parcel, push_parcel, resolve_courier, sign_parcel,
    validate_agentfile, verify_parcel, verify_parcel_signature,
};
use futures::executor::block_on;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
};
use tempfile::TempDir;

#[derive(Debug, Parser)]
#[command(name = "dispatch")]
#[command(about = "Build and dispatch Agentfile-based agent parcels")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate an Agentfile
    Lint {
        /// Path to an Agentfile or a directory containing one
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Print the parsed AST as JSON
        #[arg(long)]
        json: bool,
    },
    /// Build an immutable agent parcel
    Build {
        /// Path to an Agentfile or a directory containing one
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Output directory for built parcels
        #[arg(long)]
        output_dir: Option<PathBuf>,
    },
    /// Execute packaged evals against a live courier
    Eval {
        /// Path to a built parcel, Agentfile, or directory containing one
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Courier backend to use for eval execution
        #[arg(long = "courier", default_value = "native")]
        courier: String,
        /// Override the courier plugin registry path
        #[arg(long)]
        registry: Option<PathBuf>,
        /// Print full eval report as JSON
        #[arg(long)]
        json: bool,
        /// Output directory for built parcels when evaluating an Agentfile source
        #[arg(long)]
        output_dir: Option<PathBuf>,
    },
    /// Inspect a built parcel
    Inspect {
        /// Path to a parcel directory or a `manifest.json` file
        path: PathBuf,
        /// Validate and inspect the parcel against a specific courier backend
        #[arg(long = "courier")]
        courier: Option<String>,
        /// Override the courier plugin registry path
        #[arg(long)]
        registry: Option<PathBuf>,
        /// Print full JSON instead of a summary
        #[arg(long)]
        json: bool,
    },
    /// Verify parcel digest, lockfile, and packaged file integrity
    Verify {
        /// Path to a parcel directory or a `manifest.json` file
        path: PathBuf,
        /// Verify a detached parcel signature with the given public key file.
        #[arg(long = "public-key")]
        public_keys: Vec<PathBuf>,
        /// Print full verification report as JSON
        #[arg(long)]
        json: bool,
    },
    /// Generate an Ed25519 signing keypair for parcel signatures
    Keygen {
        /// Stable key identifier used in detached signature filenames
        #[arg(long)]
        key_id: String,
        /// Output directory for generated key files
        #[arg(long)]
        output_dir: Option<PathBuf>,
    },
    /// Sign a parcel with a detached signature file
    Sign {
        /// Path to a parcel directory or a `manifest.json` file
        path: PathBuf,
        /// Path to a generated secret key JSON file
        #[arg(long = "secret-key")]
        secret_key: PathBuf,
    },
    /// Push a built parcel to a depot reference
    Push {
        /// Path to a parcel directory or a `manifest.json` file
        path: PathBuf,
        /// Depot reference, e.g. `file:///tmp/depot::org/parcel:v1`
        reference: String,
    },
    /// Pull a parcel from a depot reference
    Pull {
        /// Depot reference, e.g. `file:///tmp/depot::org/parcel:v1`
        reference: String,
        /// Output directory for pulled parcels
        #[arg(long)]
        output_dir: Option<PathBuf>,
        /// Verify detached parcel signatures immediately after pull
        #[arg(long = "public-key")]
        public_keys: Vec<PathBuf>,
    },
    /// Execute part of a built parcel locally
    Run(RunArgs),
    /// Manage installed courier backends
    Courier {
        #[command(subcommand)]
        command: CourierCommand,
    },
    /// Manage parcel-scoped built-in courier state
    State {
        #[command(subcommand)]
        command: StateCommand,
    },
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Path to a parcel directory or a `manifest.json` file
    path: PathBuf,
    /// Courier backend to use for inspection and execution
    #[arg(long = "courier", default_value = "native")]
    courier: String,
    /// Override the courier plugin registry path
    #[arg(long)]
    registry: Option<PathBuf>,
    /// Persist and resume dispatch state from a JSON file
    #[arg(long)]
    session_file: Option<PathBuf>,
    /// Send a chat message through the courier
    #[arg(long)]
    chat: Option<String>,
    /// Execute the parcel job entrypoint with a payload
    #[arg(long)]
    job: Option<String>,
    /// Execute the parcel heartbeat entrypoint with an optional payload.
    /// Pass `--heartbeat` with no value to send an empty tick.
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    heartbeat: Option<String>,
    /// Start an interactive multi-turn chat session
    #[arg(long)]
    interactive: bool,
    /// Print the resolved prompt stack
    #[arg(long)]
    print_prompt: bool,
    /// List declared local tools
    #[arg(long)]
    list_tools: bool,
    /// Execute a declared local tool by alias
    #[arg(long)]
    tool: Option<String>,
    /// Pass raw input to the tool via stdin and `TOOL_INPUT`
    #[arg(long)]
    input: Option<String>,
}

#[derive(Debug, Subcommand)]
enum CourierCommand {
    /// List built-in and installed courier backends
    Ls {
        /// Print full courier catalog as JSON
        #[arg(long)]
        json: bool,
        /// Override the courier plugin registry path
        #[arg(long)]
        registry: Option<PathBuf>,
    },
    /// Inspect one built-in or installed courier backend
    Inspect {
        /// Courier backend name
        name: String,
        /// Print full courier entry as JSON
        #[arg(long)]
        json: bool,
        /// Override the courier plugin registry path
        #[arg(long)]
        registry: Option<PathBuf>,
    },
    /// Install a courier plugin manifest into the local registry
    Install {
        /// Path to a courier plugin manifest JSON file
        manifest: PathBuf,
        /// Override the courier plugin registry path
        #[arg(long)]
        registry: Option<PathBuf>,
    },
    /// Run the public courier contract checks against one courier backend
    Conformance {
        /// Courier backend name
        name: String,
        /// Override the courier plugin registry path
        #[arg(long)]
        registry: Option<PathBuf>,
        /// Print the full conformance report as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum StateCommand {
    /// List local state directories keyed by parcel digest
    Ls {
        /// Override the state root; defaults to `DISPATCH_STATE_ROOT` or `./.dispatch/state`
        #[arg(long)]
        root: Option<PathBuf>,
        /// Override the parcels root used to determine whether state is still live
        #[arg(long)]
        parcels_root: Option<PathBuf>,
        /// Print full state inventory as JSON
        #[arg(long)]
        json: bool,
    },
    /// Remove state directories that do not have a matching local parcel
    Gc {
        /// Override the state root; defaults to `DISPATCH_STATE_ROOT` or `./.dispatch/state`
        #[arg(long)]
        root: Option<PathBuf>,
        /// Override the parcels root used to determine whether state is still live
        #[arg(long)]
        parcels_root: Option<PathBuf>,
        /// Show what would be removed without deleting anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Copy parcel state from one digest to another
    Migrate {
        /// Source parcel digest
        source_digest: String,
        /// Target parcel digest
        target_digest: String,
        /// Override the state root; defaults to `DISPATCH_STATE_ROOT` or `./.dispatch/state`
        #[arg(long)]
        root: Option<PathBuf>,
        /// Replace an existing target state directory
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct StateEntry {
    digest: String,
    path: PathBuf,
    parcel_present: bool,
    name: Option<String>,
    version: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct StateGcReport {
    root: PathBuf,
    parcels_root: PathBuf,
    removed: Vec<StateEntry>,
    kept: Vec<StateEntry>,
    dry_run: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum EvalDocument {
    Single(Box<EvalSpec>),
    Cases { cases: Vec<EvalSpec> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct EvalSpec {
    name: String,
    input: String,
    #[serde(default)]
    entrypoint: Option<String>,
    #[serde(default)]
    expects_tool: Option<String>,
    #[serde(
        default,
        alias = "expects_output_contains",
        alias = "expects_message_contains",
        alias = "expects_text_contains"
    )]
    expects_text: Option<String>,
    #[serde(default)]
    expects_text_exact: Option<String>,
    #[serde(default, alias = "rejects_text_contains")]
    expects_text_not_contains: Option<String>,
    #[serde(default)]
    expects_tool_count: Option<usize>,
    #[serde(default)]
    expects_tools: Vec<String>,
    #[serde(default)]
    expects_tool_stdout_contains: Option<String>,
    #[serde(default)]
    expects_tool_stderr_contains: Option<String>,
    #[serde(default)]
    expects_tool_exit_code: Option<i32>,
    #[serde(default)]
    expects_error_contains: Option<String>,
}

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
    mounts: LoadedParcel,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Lint { path, json } => lint(path, json),
        Command::Build { path, output_dir } => build(path, output_dir),
        Command::Eval {
            path,
            courier,
            registry,
            json,
            output_dir,
        } => eval(path, &courier, registry, json, output_dir),
        Command::Inspect {
            path,
            courier,
            registry,
            json,
        } => inspect(path, courier, registry, json),
        Command::Verify {
            path,
            public_keys,
            json,
        } => verify(path, public_keys, json),
        Command::Keygen { key_id, output_dir } => keygen(&key_id, output_dir),
        Command::Sign { path, secret_key } => sign(path, &secret_key),
        Command::Push { path, reference } => push(path, &reference),
        Command::Pull {
            reference,
            output_dir,
            public_keys,
        } => pull(&reference, output_dir, public_keys),
        Command::Run(args) => run(args),
        Command::Courier { command } => courier_command(command),
        Command::State { command } => state_command(command),
    }
}

fn lint(path: PathBuf, emit_json: bool) -> Result<()> {
    let path = resolve_agentfile_path(path);
    let source =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;

    let parsed =
        parse_agentfile(&source).with_context(|| format!("failed to parse {}", path.display()))?;
    let report = validate_agentfile(&parsed);

    if emit_json {
        println!("{}", serde_json::to_string_pretty(&parsed)?);
    }

    if report.diagnostics.is_empty() {
        println!("OK {}", path.display());
        return Ok(());
    }

    for diagnostic in &report.diagnostics {
        let level = match diagnostic.level {
            Level::Error => "error",
            Level::Warning => "warning",
        };
        println!(
            "{level}: {}:{}: {}",
            path.display(),
            diagnostic.line,
            diagnostic.message
        );
    }

    if report.is_ok() {
        println!("OK {}", path.display());
        Ok(())
    } else {
        bail!("lint failed")
    }
}

fn resolve_agentfile_path(path: PathBuf) -> PathBuf {
    if path.is_dir() {
        path.join("Agentfile")
    } else {
        path
    }
}

fn build(path: PathBuf, output_dir: Option<PathBuf>) -> Result<()> {
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

    println!("Built parcel {}", built.digest);
    println!("Parcel dir: {}", built.parcel_dir.display());
    println!("Manifest: {}", built.manifest_path.display());
    println!("Lockfile: {}", built.lockfile_path.display());
    Ok(())
}

fn eval(
    path: PathBuf,
    courier_name: &str,
    registry: Option<PathBuf>,
    emit_json: bool,
    output_dir: Option<PathBuf>,
) -> Result<()> {
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
    let evals = load_eval_specs(parcel)?;
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

fn load_eval_specs(parcel: &LoadedParcel) -> Result<Vec<(String, EvalSpec)>> {
    let mut evals = Vec::new();
    for instruction in &parcel.config.instructions {
        if !matches!(instruction.kind, dispatch_core::InstructionKind::Eval) {
            continue;
        }
        let path = parcel
            .parcel_dir
            .join("context")
            .join(&instruction.packaged_path);
        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let parsed: EvalDocument = serde_yaml::from_str(&source)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        match parsed {
            EvalDocument::Single(spec) => evals.push((instruction.packaged_path.clone(), *spec)),
            EvalDocument::Cases { cases } => {
                for spec in cases {
                    evals.push((instruction.packaged_path.clone(), spec));
                }
            }
        }
    }
    Ok(evals)
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
        && !result
            .tool_results
            .iter()
            .any(|tool_result| tool_result.stdout.contains(expected_stdout))
    {
        result.failures.push(format!(
            "expected tool stdout containing `{expected_stdout}`"
        ));
    }

    if let Some(expected_stderr) = &spec.expects_tool_stderr_contains
        && !result
            .tool_results
            .iter()
            .any(|tool_result| tool_result.stderr.contains(expected_stderr))
    {
        result.failures.push(format!(
            "expected tool stderr containing `{expected_stderr}`"
        ));
    }

    if let Some(expected_exit_code) = spec.expects_tool_exit_code
        && !result
            .tool_results
            .iter()
            .any(|tool_result| tool_result.exit_code == expected_exit_code)
    {
        result
            .failures
            .push(format!("expected tool exit code `{expected_exit_code}`"));
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

fn inspect(
    path: PathBuf,
    courier: Option<String>,
    registry: Option<PathBuf>,
    emit_json: bool,
) -> Result<()> {
    let manifest_path = resolve_manifest_path(path);
    let source = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let image: ParcelManifest = serde_json::from_str(&source)
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;

    if emit_json {
        println!("{}", serde_json::to_string_pretty(&image)?);
        return Ok(());
    }

    println!("Digest: {}", image.digest);
    println!("Name: {}", image.name.as_deref().unwrap_or("<unnamed>"));
    println!(
        "Version: {}",
        image.version.as_deref().unwrap_or("<unspecified>")
    );
    println!("Courier Target: {}", image.courier.reference());
    println!(
        "Entrypoint: {}",
        image.entrypoint.as_deref().unwrap_or("<none>")
    );
    println!("Instruction files: {}", image.instructions.len());
    println!("Packaged files: {}", image.files.len());
    println!("Tools: {}", image.tools.len());
    println!("Mounts: {}", image.mounts.len());
    println!("Config: {}", manifest_path.display());

    if let Some(courier) = courier {
        inspect_for_courier_name(&courier, registry.as_deref(), &manifest_path)?;
    }

    Ok(())
}

fn resolve_manifest_path(path: PathBuf) -> PathBuf {
    if path.is_dir() {
        path.join("manifest.json")
    } else {
        path
    }
}

fn verify(path: PathBuf, public_keys: Vec<PathBuf>, emit_json: bool) -> Result<()> {
    let report =
        verify_parcel(&path).with_context(|| format!("failed to verify {}", path.display()))?;
    let signature_checks = public_keys
        .iter()
        .map(|public_key| {
            verify_parcel_signature(&path, public_key).with_context(|| {
                format!(
                    "failed to verify detached signature for {} with {}",
                    path.display(),
                    public_key.display()
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;

    if emit_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "integrity": report,
                "signatures": signature_checks,
            }))?
        );
    } else {
        print_verification_report(&report);
        if !signature_checks.is_empty() {
            print_signature_verifications(&signature_checks);
        }
    }

    let signatures_ok = signature_checks.iter().all(SignatureVerification::is_ok);
    if report.is_ok() && signatures_ok {
        Ok(())
    } else {
        bail!("verification failed")
    }
}

fn keygen(key_id: &str, output_dir: Option<PathBuf>) -> Result<()> {
    let output_dir = output_dir.unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".dispatch/keys")
    });
    let generated = generate_keypair_files(&output_dir, key_id)?;
    println!("Secret key: {}", generated.secret_key_path.display());
    println!("Public key: {}", generated.public_key_path.display());
    Ok(())
}

fn sign(path: PathBuf, secret_key: &Path) -> Result<()> {
    let signature = sign_parcel(&path, secret_key).with_context(|| {
        format!(
            "failed to sign {} with {}",
            path.display(),
            secret_key.display()
        )
    })?;
    println!("Signature: {}", signature.display());
    Ok(())
}

fn push(path: PathBuf, reference: &str) -> Result<()> {
    let parcel =
        load_parcel(&path).with_context(|| format!("failed to load parcel {}", path.display()))?;
    let reference = parse_depot_reference(reference)
        .with_context(|| format!("invalid depot reference `{reference}`"))?;
    let pushed = push_parcel(&parcel, &reference)?;

    println!("Pushed parcel {}", pushed.digest);
    println!("Blob: {}", pushed.blob_location);
    println!("Tag: {}", pushed.tag_location);
    Ok(())
}

fn pull(reference: &str, output_dir: Option<PathBuf>, public_keys: Vec<PathBuf>) -> Result<()> {
    let reference = parse_depot_reference(reference)
        .with_context(|| format!("invalid depot reference `{reference}`"))?;
    let output_root = output_dir.unwrap_or_else(default_pull_output_root);
    let pulled = pull_parcel(&reference, &output_root)?;
    if !public_keys.is_empty() {
        let integrity = verify_parcel(&pulled.parcel_dir).with_context(|| {
            format!(
                "failed to verify pulled parcel {}",
                pulled.parcel_dir.display()
            )
        })?;
        let signature_checks = public_keys
            .iter()
            .map(|public_key| {
                verify_parcel_signature(&pulled.parcel_dir, public_key).with_context(|| {
                    format!(
                        "failed to verify detached signature for {} with {}",
                        pulled.parcel_dir.display(),
                        public_key.display()
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let signatures_ok = signature_checks.iter().all(SignatureVerification::is_ok);
        if !integrity.is_ok() || !signatures_ok {
            print_verification_report(&integrity);
            print_signature_verifications(&signature_checks);
            bail!("pulled parcel failed verification");
        }
    }

    println!("Pulled parcel {}", pulled.digest);
    println!("Parcel dir: {}", pulled.parcel_dir.display());
    println!("Manifest: {}", pulled.manifest_path.display());
    Ok(())
}

fn default_pull_output_root() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".dispatch/parcels")
}

fn courier_command(command: CourierCommand) -> Result<()> {
    match command {
        CourierCommand::Ls { json, registry } => courier_ls(registry.as_deref(), json),
        CourierCommand::Inspect {
            name,
            json,
            registry,
        } => courier_inspect(&name, registry.as_deref(), json),
        CourierCommand::Install { manifest, registry } => {
            courier_install(&manifest, registry.as_deref())
        }
        CourierCommand::Conformance {
            name,
            registry,
            json,
        } => courier_conformance(&name, registry.as_deref(), json),
    }
}

fn state_command(command: StateCommand) -> Result<()> {
    match command {
        StateCommand::Ls {
            root,
            parcels_root,
            json,
        } => state_ls(root, parcels_root, json),
        StateCommand::Gc {
            root,
            parcels_root,
            dry_run,
        } => state_gc(root, parcels_root, dry_run),
        StateCommand::Migrate {
            source_digest,
            target_digest,
            root,
            force,
        } => state_migrate(&source_digest, &target_digest, root, force),
    }
}

fn state_ls(root: Option<PathBuf>, parcels_root: Option<PathBuf>, emit_json: bool) -> Result<()> {
    let root = resolve_state_root(root)?;
    let parcels_root = resolve_parcels_root(parcels_root)?;
    let entries = collect_state_entries(&root, &parcels_root)?;

    if emit_json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    if entries.is_empty() {
        println!("No parcel state directories found under {}", root.display());
        return Ok(());
    }

    for entry in entries {
        let status = if entry.parcel_present {
            "live"
        } else {
            "orphaned"
        };
        let name = entry.name.as_deref().unwrap_or("<unknown>");
        let version = entry.version.as_deref().unwrap_or("<unspecified>");
        println!(
            "{}\t{}\t{}\t{}\t{}",
            entry.digest,
            status,
            name,
            version,
            entry.path.display()
        );
    }

    Ok(())
}

fn state_gc(root: Option<PathBuf>, parcels_root: Option<PathBuf>, dry_run: bool) -> Result<()> {
    let root = resolve_state_root(root)?;
    let parcels_root = resolve_parcels_root(parcels_root)?;
    let entries = collect_state_entries(&root, &parcels_root)?;
    let mut removed = Vec::new();
    let mut kept = Vec::new();

    for entry in entries {
        if entry.parcel_present {
            kept.push(entry);
            continue;
        }
        if !dry_run {
            fs::remove_dir_all(&entry.path)
                .with_context(|| format!("failed to remove {}", entry.path.display()))?;
        }
        removed.push(entry);
    }

    let report = StateGcReport {
        root,
        parcels_root,
        removed,
        kept,
        dry_run,
    };

    if report.removed.is_empty() {
        println!("No orphaned parcel state found.");
        return Ok(());
    }

    let action = if report.dry_run {
        "Would remove"
    } else {
        "Removed"
    };
    for entry in &report.removed {
        println!("{action} {}\t{}", entry.digest, entry.path.display());
    }
    println!(
        "{} {} orphaned state director{}.",
        action,
        report.removed.len(),
        if report.removed.len() == 1 {
            "y"
        } else {
            "ies"
        }
    );
    Ok(())
}

fn state_migrate(
    source_digest: &str,
    target_digest: &str,
    root: Option<PathBuf>,
    force: bool,
) -> Result<()> {
    let root = resolve_state_root(root)?;
    let source = root.join(source_digest);
    let target = root.join(target_digest);

    if !source.exists() {
        bail!(
            "state for digest `{source_digest}` does not exist at {}",
            source.display()
        );
    }
    if target.exists() {
        if !force {
            bail!(
                "state for digest `{target_digest}` already exists at {} (pass --force to replace it)",
                target.display()
            );
        }
        fs::remove_dir_all(&target)
            .with_context(|| format!("failed to remove {}", target.display()))?;
    }

    copy_dir_recursive(&source, &target)?;
    println!(
        "Migrated parcel state from {} to {}",
        source.display(),
        target.display()
    );
    Ok(())
}

fn resolve_state_root(root: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(root) = root {
        return Ok(root);
    }
    if let Some(root) = std::env::var_os("DISPATCH_STATE_ROOT") {
        return Ok(PathBuf::from(root));
    }
    Ok(std::env::current_dir()
        .context("failed to resolve current working directory")?
        .join(".dispatch/state"))
}

fn resolve_parcels_root(parcels_root: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(root) = parcels_root {
        return Ok(root);
    }
    Ok(std::env::current_dir()
        .context("failed to resolve current working directory")?
        .join(".dispatch/parcels"))
}

fn collect_state_entries(root: &Path, parcels_root: &Path) -> Result<Vec<StateEntry>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut entries = fs::read_dir(root)
        .with_context(|| format!("failed to read {}", root.display()))?
        .map(|entry| {
            let entry = entry.with_context(|| format!("failed to inspect {}", root.display()))?;
            let path = entry.path();
            if !path.is_dir() {
                return Ok(None);
            }
            let digest = entry.file_name().to_string_lossy().to_string();
            let manifest_path = parcels_root.join(&digest).join("manifest.json");
            let manifest = if manifest_path.exists() {
                let body = fs::read_to_string(&manifest_path)
                    .with_context(|| format!("failed to read {}", manifest_path.display()))?;
                Some(
                    serde_json::from_str::<ParcelManifest>(&body)
                        .with_context(|| format!("failed to parse {}", manifest_path.display()))?,
                )
            } else {
                None
            };
            Ok(Some(StateEntry {
                digest,
                path,
                parcel_present: manifest.is_some(),
                name: manifest.as_ref().and_then(|manifest| manifest.name.clone()),
                version: manifest
                    .as_ref()
                    .and_then(|manifest| manifest.version.clone()),
            }))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    entries.sort_by(|left, right| left.digest.cmp(&right.digest));
    Ok(entries)
}

fn copy_dir_recursive(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target).with_context(|| format!("failed to create {}", target.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry.with_context(|| format!("failed to inspect {}", source.display()))?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", source_path.display()))?;
        if file_type.is_dir() {
            copy_dir_recursive(&source_path, &target_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &target_path).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    source_path.display(),
                    target_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn courier_ls(registry: Option<&Path>, emit_json: bool) -> Result<()> {
    let catalog = list_courier_catalog(registry)?;
    if emit_json {
        println!("{}", serde_json::to_string_pretty(&catalog)?);
        return Ok(());
    }

    for entry in catalog {
        match entry {
            CourierCatalogEntry::Builtin {
                name,
                kind,
                description,
            } => println!("{name}\tbuiltin\t{kind:?}\t{description}"),
            CourierCatalogEntry::Plugin {
                name,
                protocol_version,
                transport,
                command,
                ..
            } => println!("{name}\tplugin\tprotocol-v{protocol_version}/{transport:?}\t{command}"),
        }
    }

    Ok(())
}

fn courier_inspect(name: &str, registry: Option<&Path>, emit_json: bool) -> Result<()> {
    match resolve_courier(name, registry)? {
        ResolvedCourier::Builtin(courier) => {
            let entry = builtin_catalog_entry(courier);
            if emit_json {
                println!("{}", serde_json::to_string_pretty(&entry)?);
            } else {
                print_courier_catalog_entry(&entry);
            }
        }
        ResolvedCourier::Plugin(plugin) => {
            if emit_json {
                println!("{}", serde_json::to_string_pretty(&plugin)?);
            } else {
                print_courier_plugin_manifest(&plugin);
            }
        }
    }

    Ok(())
}

fn courier_install(manifest: &Path, registry: Option<&Path>) -> Result<()> {
    let installed = install_courier_plugin(manifest, registry)?;
    let registry_path = registry
        .map(PathBuf::from)
        .or_else(|| default_courier_registry_path().ok())
        .unwrap_or_else(|| PathBuf::from("<unknown>"));

    println!("Installed courier plugin `{}`", installed.name);
    println!("Registry: {}", registry_path.display());
    Ok(())
}

fn courier_conformance(name: &str, registry: Option<&Path>, emit_json: bool) -> Result<()> {
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
                |response| {
                    response.events.iter().any(|event| {
                        matches!(event, CourierEvent::Message { role, .. } if role == "assistant")
                    }) && matches!(response.events.last(), Some(CourierEvent::Done))
                },
                "expected assistant message and Done event",
            ));
        } else {
            checks.push(ConformanceCheck {
                name: "chat".to_string(),
                passed: true,
                skipped: true,
                detail: "courier does not advertise chat support".to_string(),
            });
        }
    }

    if capabilities
        .supports_mounts
        .contains(&dispatch_core::MountKind::Session)
        || capabilities
            .supports_mounts
            .contains(&dispatch_core::MountKind::Memory)
        || capabilities
            .supports_mounts
            .contains(&dispatch_core::MountKind::Artifacts)
    {
        checks.push(match block_on(courier.open_session(&fixtures.mounts)) {
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
        checks.push(ConformanceCheck {
            name: "resolve-mounts".to_string(),
            passed: true,
            skipped: true,
            detail: "courier does not advertise mount support".to_string(),
        });
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

fn run_conformance_operation_check(
    name: &str,
    response: Result<dispatch_core::CourierResponse, dispatch_core::CourierError>,
    predicate: impl FnOnce(&dispatch_core::CourierResponse) -> bool,
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
    )?;
    let incompatible = build_conformance_fixture(
        dir.path(),
        "incompatible",
        incompatible_reference_for_kind(kind),
        "",
    )?;
    let mounts = build_conformance_fixture(
        dir.path(),
        "mounts",
        compatible_reference_for_kind(kind),
        "MOUNT SESSION sqlite\nMOUNT MEMORY sqlite\nMOUNT ARTIFACTS local\n",
    )?;
    Ok(ConformanceFixtures {
        _dir: dir,
        compatible,
        incompatible,
        mounts,
    })
}

fn build_conformance_fixture(
    root: &Path,
    name: &str,
    courier_reference: &str,
    extra_lines: &str,
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
ENTRYPOINT chat\n"
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

fn run(args: RunArgs) -> Result<()> {
    let courier_name = args.courier.clone();
    match resolve_courier(&courier_name, args.registry.as_deref())? {
        ResolvedCourier::Builtin(courier) => run_with_builtin_courier(courier, args),
        ResolvedCourier::Plugin(plugin) => run_with_courier(JsonlCourierPlugin::new(plugin), args),
    }
}

fn run_with_builtin_courier(courier: BuiltinCourier, args: RunArgs) -> Result<()> {
    match courier {
        BuiltinCourier::Native => run_with_courier(NativeCourier::default(), args),
        BuiltinCourier::Docker => run_with_courier(DockerCourier::default(), args),
        BuiltinCourier::Wasm => run_with_courier(WasmCourier::default(), args),
    }
}

fn run_with_courier<R: CourierBackend>(courier: R, args: RunArgs) -> Result<()> {
    let RunArgs {
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
        // `default_missing_value = ""` means a bare `--heartbeat` with no
        // argument produces an empty string; map that to None (empty tick).
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
    session_file: Option<&std::path::Path>,
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
    session_file: Option<&std::path::Path>,
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

fn load_session(path: &std::path::Path) -> Result<CourierSession> {
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&source)
        .with_context(|| format!("failed to parse session {}", path.display()))
}

fn persist_session(path: Option<&std::path::Path>, session: &CourierSession) -> Result<()> {
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

fn inspect_for_courier_name(
    courier_name: &str,
    registry: Option<&Path>,
    image_path: &Path,
) -> Result<()> {
    match resolve_courier(courier_name, registry)? {
        ResolvedCourier::Builtin(courier) => inspect_for_builtin_courier(courier, image_path),
        ResolvedCourier::Plugin(plugin) => {
            inspect_for_courier(JsonlCourierPlugin::new(plugin), image_path)
        }
    }
}

fn inspect_for_builtin_courier(courier: BuiltinCourier, image_path: &Path) -> Result<()> {
    match courier {
        BuiltinCourier::Native => inspect_for_courier(NativeCourier::default(), image_path),
        BuiltinCourier::Docker => inspect_for_courier(DockerCourier::default(), image_path),
        BuiltinCourier::Wasm => inspect_for_courier(WasmCourier::default(), image_path),
    }
}

fn inspect_for_courier<R: CourierBackend>(courier: R, image_path: &std::path::Path) -> Result<()> {
    let parcel = load_parcel(image_path)
        .with_context(|| format!("failed to load parcel {}", image_path.display()))?;
    block_on(courier.validate_parcel(&parcel)).with_context(|| {
        format!(
            "courier `{}` is incompatible with parcel {}",
            courier.id(),
            image_path.display()
        )
    })?;
    let capabilities = block_on(courier.capabilities()).with_context(|| {
        format!(
            "failed to query courier capabilities for `{}`",
            courier.id()
        )
    })?;
    let inspection = block_on(courier.inspect(&parcel)).with_context(|| {
        format!(
            "failed to inspect parcel {} for courier",
            image_path.display()
        )
    })?;

    print_courier_capabilities(&capabilities);
    print_courier_inspection(&inspection);
    Ok(())
}

fn builtin_catalog_entry(courier: BuiltinCourier) -> CourierCatalogEntry {
    CourierCatalogEntry::Builtin {
        name: courier.name().to_string(),
        kind: courier.kind(),
        description: courier.description().to_string(),
    }
}

fn print_courier_catalog_entry(entry: &CourierCatalogEntry) {
    match entry {
        CourierCatalogEntry::Builtin {
            name,
            kind,
            description,
        } => {
            println!("Name: {name}");
            println!("Source: builtin");
            println!("Kind: {kind:?}");
            println!("Description: {description}");
        }
        CourierCatalogEntry::Plugin {
            name,
            description,
            protocol_version,
            transport,
            command,
            args,
        } => {
            println!("Name: {name}");
            println!("Source: plugin");
            println!("Protocol: v{protocol_version}");
            println!("Transport: {transport:?}");
            println!("Command: {command}");
            if !args.is_empty() {
                println!("Args: {}", args.join(" "));
            }
            if let Some(description) = description {
                println!("Description: {description}");
            }
        }
    }
}

fn print_courier_plugin_manifest(plugin: &CourierPluginManifest) {
    println!("Name: {}", plugin.name);
    println!("Version: {}", plugin.version);
    println!("Protocol: v{}", plugin.protocol_version);
    println!("Transport: {:?}", plugin.transport);
    println!("Command: {}", plugin.exec.command);
    if !plugin.exec.args.is_empty() {
        println!("Args: {}", plugin.exec.args.join(" "));
    }
    if let Some(description) = &plugin.description {
        println!("Description: {description}");
    }
}

fn print_courier_capabilities(capabilities: &CourierCapabilities) {
    println!("Courier Backend: {}", capabilities.courier_id);
    println!("Courier Kind: {:?}", capabilities.kind);
    println!("Supports Chat: {}", capabilities.supports_chat);
    println!("Supports Job: {}", capabilities.supports_job);
    println!("Supports Heartbeat: {}", capabilities.supports_heartbeat);
    println!(
        "Supports Local Tools: {}",
        capabilities.supports_local_tools
    );
}

fn print_courier_inspection(inspection: &CourierInspection) {
    println!(
        "Validated Entrypoint: {}",
        inspection.entrypoint.as_deref().unwrap_or("<none>")
    );
    println!("Declared Secrets: {}", inspection.required_secrets.len());
    println!("Declared Mounts: {}", inspection.mounts.len());
    println!("Declared Local Tools: {}", inspection.local_tools.len());
}

fn print_verification_report(report: &VerificationReport) {
    println!("Digest: {}", report.digest);
    println!(
        "Manifest Digest Matches: {}",
        report.manifest_digest_matches
    );
    println!(
        "Lockfile Digest Matches: {}",
        report.lockfile_digest_matches
    );
    println!(
        "Lockfile Layout Matches: {}",
        report.lockfile_layout_matches
    );
    println!("Lockfile Files Match: {}", report.lockfile_files_match);
    println!("Verified Files: {}", report.verified_files);

    if !report.missing_files.is_empty() {
        println!("Missing Files:");
        for path in &report.missing_files {
            println!("  {path}");
        }
    }

    if !report.modified_files.is_empty() {
        println!("Modified Files:");
        for path in &report.modified_files {
            println!("  {path}");
        }
    }
}

fn print_signature_verifications(verifications: &[SignatureVerification]) {
    println!("Detached Signatures:");
    for verification in verifications {
        println!("  Key ID: {}", verification.key_id);
        println!("  Algorithm: {}", verification.algorithm);
        println!("  Signature Found: {}", verification.signature_found);
        println!("  Digest Matches: {}", verification.digest_matches);
        println!("  Signature Matches: {}", verification.signature_matches);
    }
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
                    println!("{} -> {}", tool.alias, tool.packaged_path);
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

#[cfg(test)]
mod tests {
    use super::{
        Cli, Command, CourierCommand, StateCommand, collect_state_entries, load_session,
        persist_session,
    };
    use clap::Parser;
    use dispatch_core::{
        BuildOptions, ConversationMessage, CourierPluginExec, CourierPluginManifest,
        CourierSession, PluginTransport, build_agentfile,
    };
    use std::{
        fs,
        path::{Path, PathBuf},
    };
    use tempfile::tempdir;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn sample_session() -> CourierSession {
        CourierSession {
            id: "native-demo-1".to_string(),
            parcel_digest: "digest-123".to_string(),
            entrypoint: Some("chat".to_string()),
            turn_count: 2,
            history: vec![
                ConversationMessage {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                },
                ConversationMessage {
                    role: "assistant".to_string(),
                    content: "world".to_string(),
                },
            ],
            resolved_mounts: Vec::new(),
            backend_state: None,
        }
    }

    #[test]
    fn persist_session_round_trips_json_state() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.json");
        let session = sample_session();

        persist_session(Some(&path), &session).unwrap();
        let loaded = load_session(&path).unwrap();

        assert_eq!(loaded, session);
    }

    #[test]
    fn persist_session_creates_parent_directories() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/session.json");
        let session = sample_session();

        persist_session(Some(&path), &session).unwrap();

        assert!(path.exists());
        assert_eq!(load_session(&path).unwrap(), session);
    }

    #[test]
    fn persist_session_is_noop_without_path() {
        persist_session(None, &sample_session()).unwrap();
    }

    #[test]
    fn cli_parses_run_courier_selection() {
        let cli = Cli::try_parse_from([
            "dispatch",
            "run",
            "manifest.json",
            "--courier",
            "docker",
            "--registry",
            "/tmp/plugins.json",
            "--print-prompt",
        ])
        .unwrap();

        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(args.courier, "docker");
        assert_eq!(
            args.registry.as_deref(),
            Some(Path::new("/tmp/plugins.json"))
        );
        assert!(args.print_prompt);
    }

    #[test]
    fn cli_parses_push_command() {
        let cli = Cli::try_parse_from([
            "dispatch",
            "push",
            "manifest.json",
            "file:///tmp/depot::acme/monitor:v1",
        ])
        .unwrap();

        let Command::Push { path, reference } = cli.command else {
            panic!("expected push command");
        };
        assert_eq!(path, Path::new("manifest.json"));
        assert_eq!(reference, "file:///tmp/depot::acme/monitor:v1");
    }

    #[test]
    fn cli_parses_pull_command_with_output_dir() {
        let cli = Cli::try_parse_from([
            "dispatch",
            "pull",
            "file:///tmp/depot::acme/monitor:v1",
            "--output-dir",
            "/tmp/parcels",
        ])
        .unwrap();

        let Command::Pull {
            reference,
            output_dir,
            public_keys,
        } = cli.command
        else {
            panic!("expected pull command");
        };
        assert_eq!(reference, "file:///tmp/depot::acme/monitor:v1");
        assert_eq!(output_dir.as_deref(), Some(Path::new("/tmp/parcels")));
        assert!(public_keys.is_empty());
    }

    #[test]
    fn cli_parses_verify_with_public_keys() {
        let cli = Cli::try_parse_from([
            "dispatch",
            "verify",
            "manifest.json",
            "--public-key",
            "release.dispatch-public.json",
            "--public-key",
            "ops.dispatch-public.json",
        ])
        .unwrap();

        let Command::Verify {
            path,
            public_keys,
            json,
        } = cli.command
        else {
            panic!("expected verify command");
        };
        assert_eq!(path, Path::new("manifest.json"));
        assert_eq!(
            public_keys,
            vec![
                PathBuf::from("release.dispatch-public.json"),
                PathBuf::from("ops.dispatch-public.json")
            ]
        );
        assert!(!json);
    }

    #[test]
    fn cli_parses_keygen_and_sign_commands() {
        let keygen = Cli::try_parse_from([
            "dispatch",
            "keygen",
            "--key-id",
            "release",
            "--output-dir",
            "/tmp/keys",
        ])
        .unwrap();
        let Command::Keygen { key_id, output_dir } = keygen.command else {
            panic!("expected keygen command");
        };
        assert_eq!(key_id, "release");
        assert_eq!(output_dir.as_deref(), Some(Path::new("/tmp/keys")));

        let sign = Cli::try_parse_from([
            "dispatch",
            "sign",
            "manifest.json",
            "--secret-key",
            "release.dispatch-secret.json",
        ])
        .unwrap();
        let Command::Sign { path, secret_key } = sign.command else {
            panic!("expected sign command");
        };
        assert_eq!(path, Path::new("manifest.json"));
        assert_eq!(secret_key, PathBuf::from("release.dispatch-secret.json"));
    }

    #[test]
    fn cli_defaults_run_courier_to_native() {
        let cli =
            Cli::try_parse_from(["dispatch", "run", "manifest.json", "--print-prompt"]).unwrap();

        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(args.courier, "native");
    }

    #[test]
    fn cli_accepts_bare_heartbeat_flag() {
        let cli = Cli::try_parse_from(["dispatch", "run", "manifest.json", "--heartbeat"]).unwrap();

        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(args.heartbeat.as_deref(), Some(""));
    }

    #[test]
    fn cli_parses_courier_subcommands() {
        let cli = Cli::try_parse_from(["dispatch", "courier", "inspect", "docker"]).unwrap();

        let Command::Courier { command } = cli.command else {
            panic!("expected courier command");
        };
        let CourierCommand::Inspect {
            name,
            json,
            registry,
        } = command
        else {
            panic!("expected courier inspect subcommand");
        };
        assert_eq!(name, "docker");
        assert!(!json);
        assert!(registry.is_none());
    }

    #[test]
    fn cli_parses_state_subcommands() {
        let ls = Cli::try_parse_from(["dispatch", "state", "ls", "--json"]).unwrap();
        let Command::State { command } = ls.command else {
            panic!("expected state command");
        };
        let StateCommand::Ls {
            root,
            parcels_root,
            json,
        } = command
        else {
            panic!("expected state ls subcommand");
        };
        assert!(root.is_none());
        assert!(parcels_root.is_none());
        assert!(json);

        let gc = Cli::try_parse_from([
            "dispatch",
            "state",
            "gc",
            "--root",
            "/tmp/state",
            "--parcels-root",
            "/tmp/parcels",
            "--dry-run",
        ])
        .unwrap();
        let Command::State { command } = gc.command else {
            panic!("expected state command");
        };
        let StateCommand::Gc {
            root,
            parcels_root,
            dry_run,
        } = command
        else {
            panic!("expected state gc subcommand");
        };
        assert_eq!(root.as_deref(), Some(Path::new("/tmp/state")));
        assert_eq!(parcels_root.as_deref(), Some(Path::new("/tmp/parcels")));
        assert!(dry_run);

        let migrate = Cli::try_parse_from([
            "dispatch",
            "state",
            "migrate",
            "digest-old",
            "digest-new",
            "--force",
        ])
        .unwrap();
        let Command::State { command } = migrate.command else {
            panic!("expected state command");
        };
        let StateCommand::Migrate {
            source_digest,
            target_digest,
            root,
            force,
        } = command
        else {
            panic!("expected state migrate subcommand");
        };
        assert_eq!(source_digest, "digest-old");
        assert_eq!(target_digest, "digest-new");
        assert!(root.is_none());
        assert!(force);
    }

    #[test]
    fn cli_parses_parcel_inspect_registry_override() {
        let cli = Cli::try_parse_from([
            "dispatch",
            "inspect",
            "manifest.json",
            "--courier",
            "remote-demo",
            "--registry",
            "/tmp/plugins.json",
        ])
        .unwrap();

        let Command::Inspect {
            path,
            courier,
            registry,
            json,
        } = cli.command
        else {
            panic!("expected inspect command");
        };
        assert_eq!(path, Path::new("manifest.json"));
        assert_eq!(courier.as_deref(), Some("remote-demo"));
        assert_eq!(registry.as_deref(), Some(Path::new("/tmp/plugins.json")));
        assert!(!json);
    }

    #[test]
    fn cli_parses_courier_conformance_subcommand() {
        let cli = Cli::try_parse_from([
            "dispatch",
            "courier",
            "conformance",
            "native",
            "--json",
            "--registry",
            "/tmp/plugins.json",
        ])
        .unwrap();

        let Command::Courier { command } = cli.command else {
            panic!("expected courier command");
        };
        let CourierCommand::Conformance {
            name,
            registry,
            json,
        } = command
        else {
            panic!("expected courier conformance subcommand");
        };
        assert_eq!(name, "native");
        assert_eq!(registry.as_deref(), Some(Path::new("/tmp/plugins.json")));
        assert!(json);
    }

    #[cfg(unix)]
    #[test]
    fn run_uses_plugin_from_custom_registry() {
        let dir = tempdir().unwrap();
        let (parcel_dir, parcel_digest) = build_test_image(dir.path());
        let registry_path = dir.path().join("plugins.json");
        let session_path = dir.path().join("session.json");
        let plugin_name = "demo-jsonl";
        install_test_plugin(dir.path(), &registry_path, plugin_name, &parcel_digest);

        super::run(super::RunArgs {
            path: parcel_dir,
            courier: plugin_name.to_string(),
            registry: Some(registry_path),
            session_file: Some(session_path.clone()),
            chat: Some("hello".to_string()),
            job: None,
            heartbeat: None,
            interactive: false,
            print_prompt: false,
            list_tools: false,
            tool: None,
            input: None,
        })
        .unwrap();

        let session = load_session(&session_path).unwrap();
        assert_eq!(session.id, "demo-jsonl-session");
        assert_eq!(session.turn_count, 2);
        assert_eq!(session.history.len(), 2);
        assert_eq!(session.history[1].content, "plugin reply");
    }

    #[cfg(unix)]
    #[test]
    fn inspect_uses_plugin_from_custom_registry() {
        let dir = tempdir().unwrap();
        let (parcel_dir, parcel_digest) = build_test_image(dir.path());
        let registry_path = dir.path().join("plugins.json");
        let plugin_name = "demo-jsonl";
        install_test_plugin(dir.path(), &registry_path, plugin_name, &parcel_digest);

        super::inspect(
            parcel_dir,
            Some(plugin_name.to_string()),
            Some(registry_path),
            false,
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn eval_runs_packaged_evals_against_live_courier() {
        let dir = tempdir().unwrap();
        let source_dir = build_test_eval_source(dir.path());
        let built = build_agentfile(
            &source_dir.join("Agentfile"),
            &BuildOptions {
                output_root: source_dir.join(".dispatch/parcels"),
            },
        )
        .unwrap();
        let registry_path = dir.path().join("plugins.json");
        install_eval_test_plugin(
            dir.path(),
            &registry_path,
            "demo-eval-plugin",
            &built.digest,
        );

        super::eval(
            source_dir,
            "demo-eval-plugin",
            Some(registry_path),
            false,
            None,
        )
        .unwrap();
    }

    #[test]
    fn courier_conformance_runs_against_native() {
        super::courier_conformance("native", None, false).unwrap();
    }

    #[test]
    fn push_and_pull_round_trip_through_file_depot() {
        let dir = tempdir().unwrap();
        let (parcel_dir, parcel_digest) = build_test_image(dir.path());
        let depot_ref = format!(
            "file://{}::acme/cli-fixture:v1",
            dir.path().join("depot").display()
        );
        let pull_root = dir.path().join("pulled");

        super::push(parcel_dir.clone(), &depot_ref).unwrap();
        super::pull(&depot_ref, Some(pull_root.clone()), Vec::new()).unwrap();

        let pulled_manifest = pull_root.join(&parcel_digest).join("manifest.json");
        assert!(pulled_manifest.exists());
        let sessionless = dispatch_core::load_parcel(&pull_root.join(&parcel_digest)).unwrap();
        assert_eq!(sessionless.config.digest, parcel_digest);
    }

    #[test]
    fn collect_state_entries_marks_live_and_orphaned_state() {
        let dir = tempdir().unwrap();
        let state_root = dir.path().join(".dispatch/state");
        let (parcel_dir, parcel_digest) = build_test_image(dir.path());
        let parcels_root = parcel_dir.parent().unwrap().to_path_buf();
        let orphan_digest = "deadbeef".repeat(8);

        fs::create_dir_all(state_root.join(&parcel_digest)).unwrap();
        fs::create_dir_all(state_root.join(&orphan_digest)).unwrap();
        assert!(parcel_dir.exists());

        let entries = collect_state_entries(&state_root, &parcels_root).unwrap();
        assert_eq!(entries.len(), 2);
        let orphan = entries
            .iter()
            .find(|entry| entry.digest == orphan_digest)
            .unwrap();
        assert!(!orphan.parcel_present);
        let live = entries
            .iter()
            .find(|entry| entry.digest == parcel_digest)
            .unwrap();
        assert!(live.parcel_present);
        assert_eq!(live.name.as_deref(), Some("plugin-cli-test"));
    }

    #[test]
    fn state_gc_removes_orphaned_state_and_keeps_live_state() {
        let dir = tempdir().unwrap();
        let state_root = dir.path().join(".dispatch/state");
        let (parcel_dir, parcel_digest) = build_test_image(dir.path());
        let parcels_root = parcel_dir.parent().unwrap().to_path_buf();
        let orphan_digest = "feedface".repeat(8);

        fs::create_dir_all(state_root.join(&parcel_digest)).unwrap();
        fs::write(
            state_root.join(&parcel_digest).join("memory.sqlite"),
            "live",
        )
        .unwrap();
        fs::create_dir_all(state_root.join(&orphan_digest)).unwrap();
        fs::write(state_root.join(&orphan_digest).join("memory.sqlite"), "old").unwrap();

        super::state_gc(Some(state_root.clone()), Some(parcels_root), false).unwrap();

        assert!(state_root.join(&parcel_digest).exists());
        assert!(!state_root.join(&orphan_digest).exists());
    }

    #[test]
    fn state_migrate_copies_state_tree_between_digests() {
        let dir = tempdir().unwrap();
        let state_root = dir.path().join(".dispatch/state");
        let source_digest = "abc123".repeat(10);
        let target_digest = "def456".repeat(10);
        let source_dir = state_root.join(&source_digest).join("sessions/demo");

        fs::create_dir_all(&source_dir).unwrap();
        fs::write(source_dir.join("session.sqlite"), "session-data").unwrap();
        fs::write(
            state_root.join(&source_digest).join("memory.sqlite"),
            "memory-data",
        )
        .unwrap();

        super::state_migrate(
            &source_digest,
            &target_digest,
            Some(state_root.clone()),
            false,
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(
                state_root
                    .join(&target_digest)
                    .join("sessions/demo/session.sqlite")
            )
            .unwrap(),
            "session-data"
        );
        assert_eq!(
            fs::read_to_string(state_root.join(&target_digest).join("memory.sqlite")).unwrap(),
            "memory-data"
        );
    }

    #[test]
    fn sign_and_verify_round_trip() {
        let dir = tempdir().unwrap();
        let (parcel_dir, _) = build_test_image(dir.path());
        let keys_dir = dir.path().join("keys");

        super::keygen("release", Some(keys_dir.clone())).unwrap();
        let secret_key = keys_dir.join("release.dispatch-secret.json");
        let public_key = keys_dir.join("release.dispatch-public.json");

        super::sign(parcel_dir.clone(), &secret_key).unwrap();
        super::verify(parcel_dir, vec![public_key], false).unwrap();
    }

    #[test]
    fn pull_can_verify_signatures_during_fetch() {
        let dir = tempdir().unwrap();
        let (parcel_dir, _) = build_test_image(dir.path());
        let keys_dir = dir.path().join("keys");
        let depot_ref = format!(
            "file://{}::acme/signed-fixture:v1",
            dir.path().join("depot").display()
        );
        let pull_root = dir.path().join("pulled");

        super::keygen("release", Some(keys_dir.clone())).unwrap();
        let secret_key = keys_dir.join("release.dispatch-secret.json");
        let public_key = keys_dir.join("release.dispatch-public.json");
        super::sign(parcel_dir.clone(), &secret_key).unwrap();
        super::push(parcel_dir, &depot_ref).unwrap();

        super::pull(&depot_ref, Some(pull_root.clone()), vec![public_key]).unwrap();
    }

    #[cfg(unix)]
    fn build_test_image(root: &Path) -> (std::path::PathBuf, String) {
        let context_dir = root.join("image");
        fs::create_dir_all(&context_dir).unwrap();
        fs::write(
            context_dir.join("Agentfile"),
            "FROM dispatch/native:latest\n\
NAME plugin-cli-test\n\
VERSION 0.1.0\n\
SKILL SKILL.md\n\
ENTRYPOINT chat\n",
        )
        .unwrap();
        fs::write(context_dir.join("SKILL.md"), "You are a test agent.\n").unwrap();

        let built = build_agentfile(
            &context_dir.join("Agentfile"),
            &BuildOptions {
                output_root: context_dir.join(".dispatch/parcels"),
            },
        )
        .unwrap();
        (built.parcel_dir, built.digest)
    }

    #[cfg(unix)]
    fn install_test_plugin(root: &Path, registry_path: &Path, name: &str, parcel_digest: &str) {
        let script_path = root.join("demo-plugin.sh");
        fs::write(
            &script_path,
            format!(
                concat!(
                    "#!/bin/sh\n",
                    "set -eu\n",
                    "while IFS= read -r line; do\n",
                    "if printf '%s' \"$line\" | grep -q '\"kind\":\"capabilities\"'; then\n",
                    "  printf '%s\\n' '{{\"kind\":\"result\",\"capabilities\":{{\"courier_id\":\"demo-jsonl\",\"kind\":\"custom\",\"supports_chat\":true,\"supports_job\":false,\"supports_heartbeat\":false,\"supports_local_tools\":false,\"supports_mounts\":[]}}}}'\n",
                    "elif printf '%s' \"$line\" | grep -q '\"kind\":\"validate_parcel\"'; then\n",
                    "  printf '%s\\n' '{{\"kind\":\"result\"}}'\n",
                    "elif printf '%s' \"$line\" | grep -q '\"kind\":\"inspect\"'; then\n",
                    "  printf '%s\\n' '{{\"kind\":\"result\",\"inspection\":{{\"courier_id\":\"demo-jsonl\",\"kind\":\"custom\",\"entrypoint\":\"chat\",\"required_secrets\":[],\"mounts\":[],\"local_tools\":[]}}}}'\n",
                    "elif printf '%s' \"$line\" | grep -q '\"kind\":\"open_session\"'; then\n",
                    "  printf '%s\\n' '{{\"kind\":\"result\",\"session\":{{\"id\":\"demo-jsonl-session\",\"parcel_digest\":\"{parcel_digest}\",\"entrypoint\":\"chat\",\"turn_count\":1,\"history\":[]}}}}'\n",
                    "elif printf '%s' \"$line\" | grep -q '\"kind\":\"run\"'; then\n",
                    "  printf '%s\\n' '{{\"kind\":\"event\",\"event\":{{\"kind\":\"message\",\"role\":\"assistant\",\"content\":\"plugin reply\"}}}}'\n",
                    "  printf '%s\\n' '{{\"kind\":\"done\",\"session\":{{\"id\":\"demo-jsonl-session\",\"parcel_digest\":\"{parcel_digest}\",\"entrypoint\":\"chat\",\"turn_count\":2,\"history\":[{{\"role\":\"user\",\"content\":\"hello\"}},{{\"role\":\"assistant\",\"content\":\"plugin reply\"}}]}}}}'\n",
                    "else\n",
                    "  printf '%s\\n' '{{\"kind\":\"error\",\"error\":{{\"code\":\"unexpected_request\",\"message\":\"unhandled request\"}}}}'\n",
                    "  exit 1\n",
                    "fi\n",
                    "done\n"
                ),
                parcel_digest = parcel_digest
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();

        let manifest_path = root.join("demo-plugin.json");
        fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&CourierPluginManifest {
                name: name.to_string(),
                version: "0.1.0".to_string(),
                protocol_version: 1,
                transport: PluginTransport::Jsonl,
                description: Some("Demo JSONL plugin for CLI tests".to_string()),
                exec: CourierPluginExec {
                    command: script_path.display().to_string(),
                    args: Vec::new(),
                },
                installed_sha256: None,
            })
            .unwrap(),
        )
        .unwrap();

        dispatch_core::install_courier_plugin(&manifest_path, Some(registry_path)).unwrap();
    }

    #[cfg(unix)]
    fn build_test_eval_source(root: &Path) -> PathBuf {
        let context_dir = root.join("eval-source");
        fs::create_dir_all(context_dir.join("evals")).unwrap();
        fs::write(
            context_dir.join("Agentfile"),
            "FROM dispatch/native:latest\n\
NAME eval-fixture\n\
VERSION 0.1.0\n\
SKILL SKILL.md\n\
EVAL evals/smoke.eval\n\
ENTRYPOINT chat\n",
        )
        .unwrap();
        fs::write(context_dir.join("SKILL.md"), "You are an eval fixture.\n").unwrap();
        fs::write(
            context_dir.join("evals/smoke.eval"),
            concat!(
                "cases:\n",
                "  - name: smoke\n",
                "    input: \"What time is it?\"\n",
                "    expects_tool: \"system_time\"\n",
                "    expects_text_contains: \"plugin reply\"\n",
                "  - name: exact\n",
                "    input: \"What time is it?\"\n",
                "    expects_tools: [\"system_time\"]\n",
                "    expects_tool_count: 1\n",
                "    expects_tool_stdout_contains: \"2026-04-03\"\n",
                "    expects_tool_exit_code: 0\n",
                "    expects_text_exact: \"plugin reply\"\n",
                "    expects_text_not_contains: \"wrong\"\n",
                "  - name: invalid-entrypoint\n",
                "    input: \"\"\n",
                "    entrypoint: unsupported\n",
                "    expects_error_contains: \"unsupported eval entrypoint\"\n",
            ),
        )
        .unwrap();
        context_dir
    }

    #[cfg(unix)]
    fn install_eval_test_plugin(
        root: &Path,
        registry_path: &Path,
        name: &str,
        parcel_digest: &str,
    ) {
        let script_path = root.join("eval-plugin.sh");
        let script = concat!(
            "#!/bin/sh\n",
            "set -eu\n",
            "while IFS= read -r line; do\n",
            "if printf '%s' \"$line\" | grep -q '\"kind\":\"capabilities\"'; then\n",
            "  printf '%s\\n' '{\"kind\":\"result\",\"capabilities\":{\"courier_id\":\"demo-eval-plugin\",\"kind\":\"custom\",\"supports_chat\":true,\"supports_job\":false,\"supports_heartbeat\":false,\"supports_local_tools\":false,\"supports_mounts\":[]}}'\n",
            "elif printf '%s' \"$line\" | grep -q '\"kind\":\"validate_parcel\"'; then\n",
            "  printf '%s\\n' '{\"kind\":\"result\"}'\n",
            "elif printf '%s' \"$line\" | grep -q '\"kind\":\"inspect\"'; then\n",
            "  printf '%s\\n' '{\"kind\":\"result\",\"inspection\":{\"courier_id\":\"demo-eval-plugin\",\"kind\":\"custom\",\"entrypoint\":\"chat\",\"required_secrets\":[],\"mounts\":[],\"local_tools\":[]}}'\n",
            "elif printf '%s' \"$line\" | grep -q '\"kind\":\"open_session\"'; then\n",
            "  printf '%s\\n' '{\"kind\":\"result\",\"session\":{\"id\":\"demo-eval-session\",\"parcel_digest\":\"__DIGEST__\",\"entrypoint\":\"chat\",\"turn_count\":0,\"history\":[]}}'\n",
            "elif printf '%s' \"$line\" | grep -q '\"kind\":\"run\"'; then\n",
            "  printf '%s\\n' '{\"kind\":\"event\",\"event\":{\"kind\":\"tool_call_started\",\"invocation\":{\"name\":\"system_time\",\"input\":null},\"command\":\"builtin\",\"args\":[]}}'\n",
            "  printf '%s\\n' '{\"kind\":\"event\",\"event\":{\"kind\":\"tool_call_finished\",\"result\":{\"tool\":\"system_time\",\"command\":\"builtin\",\"args\":[],\"exit_code\":0,\"stdout\":\"2026-04-03T00:00:00Z\",\"stderr\":\"\"}}}'\n",
            "  printf '%s\\n' '{\"kind\":\"event\",\"event\":{\"kind\":\"message\",\"role\":\"assistant\",\"content\":\"plugin reply\"}}'\n",
            "  printf '%s\\n' '{\"kind\":\"done\",\"session\":{\"id\":\"demo-eval-session\",\"parcel_digest\":\"__DIGEST__\",\"entrypoint\":\"chat\",\"turn_count\":1,\"history\":[{\"role\":\"user\",\"content\":\"What time is it?\"},{\"role\":\"assistant\",\"content\":\"plugin reply\"}]}}'\n",
            "else\n",
            "  printf '%s\\n' '{\"kind\":\"error\",\"error\":{\"code\":\"unexpected_request\",\"message\":\"unhandled request\"}}'\n",
            "  exit 1\n",
            "fi\n",
            "done\n"
        )
        .replace("__DIGEST__", parcel_digest);
        fs::write(&script_path, script).unwrap();
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();

        let manifest_path = root.join("eval-plugin.json");
        fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&CourierPluginManifest {
                name: name.to_string(),
                version: "0.1.0".to_string(),
                protocol_version: 1,
                transport: PluginTransport::Jsonl,
                description: Some("Demo eval plugin for CLI tests".to_string()),
                exec: CourierPluginExec {
                    command: script_path.display().to_string(),
                    args: Vec::new(),
                },
                installed_sha256: None,
            })
            .unwrap(),
        )
        .unwrap();

        dispatch_core::install_courier_plugin(&manifest_path, Some(registry_path)).unwrap();
    }
}
