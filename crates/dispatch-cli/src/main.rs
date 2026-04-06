mod conformance;
mod courier_cmds;
mod eval;
mod inspect;
mod parcel_ops;
mod run;
mod skill_run;
mod state;
mod tool_display;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use dispatch_core::{
    A2aOperatorPolicyOverrides, BuildOptions, Level, build_agentfile, parse_agentfile,
    validate_agentfile_at_path, with_a2a_operator_policy_overrides,
};
use std::{
    fs,
    io::IsTerminal,
    path::{Path, PathBuf},
};

#[derive(Debug, Parser)]
#[command(name = "dispatch")]
#[command(about = "Build and dispatch Agentfile-based agent parcels")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Build, validate, and inspect agent parcels
    Parcel {
        #[command(subcommand)]
        command: ParcelCommand,
    },
    /// Manage parcel distribution to and from depots
    Depot {
        #[command(subcommand)]
        command: DepotCommand,
    },
    /// Execute part of a built parcel locally
    Run(RunArgs),
    /// Execute Agent Skills bundles and files
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
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
struct LintArgs {
    /// Path to an Agentfile or a directory containing one
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Print the parsed AST as JSON
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct BuildArgs {
    /// Path to an Agentfile or a directory containing one
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Output directory for built parcels
    #[arg(long)]
    output_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ParcelListArgs {
    /// Path to an Agentfile, directory containing one, parcel store root, or built parcel
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Print full parcel inventory as JSON
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct EvalArgs {
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
    /// How to handle tools declared with `APPROVAL confirm`
    #[arg(long, value_enum)]
    tool_approval: Option<CliToolApprovalMode>,
    /// Override allowed outbound A2A origins or hostnames for this command
    #[arg(long)]
    a2a_allowed_origins: Option<String>,
    /// Apply a structured A2A trust policy file for this command
    #[arg(long)]
    a2a_trust_policy: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct InspectArgs {
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
}

#[derive(Debug, Args)]
struct VerifyArgs {
    /// Path to a parcel directory or a `manifest.json` file
    path: PathBuf,
    /// Verify a detached parcel signature with the given public key file.
    #[arg(long = "public-key")]
    public_keys: Vec<PathBuf>,
    /// Print full verification report as JSON
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct KeygenArgs {
    /// Stable key identifier used in detached signature filenames
    #[arg(long)]
    key_id: String,
    /// Output directory for generated key files
    #[arg(long)]
    output_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct SignArgs {
    /// Path to a parcel directory or a `manifest.json` file
    path: PathBuf,
    /// Path to a generated secret key JSON file
    #[arg(long = "secret-key")]
    secret_key: PathBuf,
}

#[derive(Debug, Args)]
struct PushArgs {
    /// Path to a parcel directory or a `manifest.json` file
    path: PathBuf,
    /// Depot reference, e.g. `file:///tmp/depot::org/parcel:v1`
    reference: String,
    /// Print full JSON instead of a summary
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct PullArgs {
    /// Depot reference, e.g. `file:///tmp/depot::org/parcel:v1`
    reference: String,
    /// Output directory for pulled parcels
    #[arg(long)]
    output_dir: Option<PathBuf>,
    /// Verify detached parcel signatures immediately after pull
    #[arg(long = "public-key")]
    public_keys: Vec<PathBuf>,
    /// Apply a trust policy file that matches reference prefixes to public keys
    #[arg(long)]
    trust_policy: Option<PathBuf>,
    /// Print full JSON instead of a summary
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Path to a built parcel, Agentfile, directory containing one, or unique parcel id prefix
    path: PathBuf,
    #[command(flatten)]
    exec: RunExecutionArgs,
}

#[derive(Debug, Args)]
struct SkillSynthesisOverrideArgs {
    /// Primary model id override for synthesized skill execution
    #[arg(long)]
    model: Option<String>,
    /// Provider override paired with `--model`
    #[arg(long)]
    provider: Option<String>,
    /// Entrypoint override for synthesized skill execution
    #[arg(long)]
    entrypoint: Option<String>,
}

#[derive(Debug, Args)]
struct RunSkillArgs {
    /// Path to a SKILL.md file or an Agent Skills bundle directory
    path: PathBuf,
    #[command(flatten)]
    exec: RunExecutionArgs,
    #[command(flatten)]
    synthesis: SkillSynthesisOverrideArgs,
}

#[derive(Debug, Args)]
struct ValidateSkillArgs {
    /// Path to a SKILL.md file or an Agent Skills bundle directory
    path: PathBuf,
    /// Built-in courier to target when synthesizing the temporary parcel
    #[arg(long = "courier", default_value = "native")]
    courier: String,
    #[command(flatten)]
    synthesis: SkillSynthesisOverrideArgs,
}

#[derive(Debug, Args, Clone)]
struct RunExecutionArgs {
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
    /// Print machine-readable JSON when used with `--list-tools`
    #[arg(long)]
    json: bool,
    /// Execute a declared local tool by alias
    #[arg(long)]
    tool: Option<String>,
    /// Pass raw input to the tool via stdin and `TOOL_INPUT`
    #[arg(long)]
    input: Option<String>,
    /// How to handle tools declared with `APPROVAL confirm`
    #[arg(long, value_enum)]
    tool_approval: Option<CliToolApprovalMode>,
    /// Override allowed outbound A2A origins or hostnames for this command
    #[arg(long)]
    a2a_allowed_origins: Option<String>,
    /// Apply a structured A2A trust policy file for this command
    #[arg(long)]
    a2a_trust_policy: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum ParcelCommand {
    /// Validate an Agentfile
    Lint(LintArgs),
    /// Build an immutable agent parcel
    Build(BuildArgs),
    /// List locally built parcels
    #[command(visible_alias = "ls")]
    List(ParcelListArgs),
    /// Execute packaged evals against a live courier
    Eval(EvalArgs),
    /// Inspect a built parcel
    Inspect(InspectArgs),
    /// Verify parcel digest, lockfile, and packaged file integrity
    Verify(VerifyArgs),
    /// Generate an Ed25519 signing keypair for parcel signatures
    Keygen(KeygenArgs),
    /// Sign a parcel with a detached signature file
    Sign(SignArgs),
}

#[derive(Debug, Subcommand)]
enum SkillCommand {
    /// Validate a skill file or Agent Skills bundle without executing it
    Validate(Box<ValidateSkillArgs>),
    /// Execute a skill file or Agent Skills bundle without an authored Agentfile
    Run(Box<RunSkillArgs>),
}

#[derive(Debug, Subcommand)]
enum DepotCommand {
    /// Push a built parcel to a depot reference
    Push(PushArgs),
    /// Pull a parcel from a depot reference
    Pull(PullArgs),
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
        /// Override allowed outbound A2A origins or hostnames for this command
        #[arg(long)]
        a2a_allowed_origins: Option<String>,
        /// Apply a structured A2A trust policy file for this command
        #[arg(long)]
        a2a_trust_policy: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CliA2aPolicy {
    pub allowed_origins: Option<String>,
    pub trust_policy: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum CliToolApprovalMode {
    Ask,
    Always,
    Never,
}

pub(crate) fn resolve_run_tool_approval_mode(
    requested: Option<CliToolApprovalMode>,
) -> CliToolApprovalMode {
    requested.unwrap_or_else(|| {
        if std::io::stdin().is_terminal() {
            CliToolApprovalMode::Ask
        } else {
            CliToolApprovalMode::Never
        }
    })
}

pub(crate) fn resolve_noninteractive_tool_approval_mode(
    requested: Option<CliToolApprovalMode>,
) -> CliToolApprovalMode {
    requested.unwrap_or(CliToolApprovalMode::Never)
}

pub(crate) fn with_cli_tool_approval<T>(
    mode: CliToolApprovalMode,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    match mode {
        CliToolApprovalMode::Always => dispatch_core::with_tool_approval_handler(
            |_| Ok(dispatch_core::ToolApprovalDecision::Approve),
            f,
        ),
        CliToolApprovalMode::Never => dispatch_core::with_tool_approval_handler(
            |_| Ok(dispatch_core::ToolApprovalDecision::Deny),
            f,
        ),
        CliToolApprovalMode::Ask => dispatch_core::with_tool_approval_handler(
            |request| prompt_for_tool_approval(request).map_err(|error| error.to_string()),
            f,
        ),
    }
}

fn prompt_for_tool_approval(
    request: &dispatch_core::ToolApprovalRequest,
) -> Result<dispatch_core::ToolApprovalDecision> {
    use std::io::{self, Write as _};

    let risk = request
        .risk
        .map(|risk| format!("{risk:?}").to_ascii_lowercase())
        .unwrap_or_else(|| "unspecified".to_string());
    eprintln!("Tool `{}` requires approval.", request.tool);
    eprintln!("kind={:?} risk={risk}", request.kind);
    eprintln!("command={} {}", request.command, request.args.join(" "));
    if let Some(description) = &request.description {
        eprintln!("description={description}");
    }
    if let Some(skill_source) = &request.skill_source {
        eprintln!("skill_source={skill_source}");
    }
    if let Some(input) = &request.input
        && !input.is_empty()
    {
        eprintln!("input={}", truncate_tool_approval_input(input));
    }
    eprint!("Approve? [y/N] ");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let decision = match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => dispatch_core::ToolApprovalDecision::Approve,
        _ => dispatch_core::ToolApprovalDecision::Deny,
    };
    Ok(decision)
}

fn truncate_tool_approval_input(input: &str) -> String {
    const LIMIT: usize = 200;
    if input.chars().count() <= LIMIT {
        input.to_string()
    } else {
        let truncated = input.chars().take(LIMIT).collect::<String>();
        format!("{truncated}...")
    }
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

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Parcel { command } => parcel_command(command),
        Command::Depot { command } => depot_command(command),
        Command::Run(args) => run::run(args),
        Command::Skill { command } => match command {
            SkillCommand::Validate(args) => skill_run::validate_skill(*args),
            SkillCommand::Run(args) => skill_run::run_skill(*args),
        },
        Command::Courier { command } => courier_cmds::courier_command(command),
        Command::State { command } => state_command(command),
    }
}

fn parcel_command(command: ParcelCommand) -> Result<()> {
    match command {
        ParcelCommand::Lint(LintArgs { path, json }) => lint(path, json),
        ParcelCommand::Build(BuildArgs { path, output_dir }) => build(path, output_dir),
        ParcelCommand::List(ParcelListArgs { path, json }) => parcel_ops::list(path, json),
        ParcelCommand::Eval(EvalArgs {
            path,
            courier,
            registry,
            json,
            output_dir,
            tool_approval,
            a2a_allowed_origins,
            a2a_trust_policy,
        }) => eval::eval(
            path,
            &courier,
            registry,
            json,
            output_dir,
            resolve_noninteractive_tool_approval_mode(tool_approval),
            CliA2aPolicy {
                allowed_origins: a2a_allowed_origins,
                trust_policy: a2a_trust_policy,
            },
        ),
        ParcelCommand::Inspect(InspectArgs {
            path,
            courier,
            registry,
            json,
        }) => inspect::inspect(path, courier, registry, json),
        ParcelCommand::Verify(VerifyArgs {
            path,
            public_keys,
            json,
        }) => parcel_ops::verify(path, public_keys, json),
        ParcelCommand::Keygen(KeygenArgs { key_id, output_dir }) => {
            parcel_ops::keygen(&key_id, output_dir)
        }
        ParcelCommand::Sign(SignArgs { path, secret_key }) => parcel_ops::sign(path, &secret_key),
    }
}

fn depot_command(command: DepotCommand) -> Result<()> {
    match command {
        DepotCommand::Push(PushArgs {
            path,
            reference,
            json,
        }) => parcel_ops::push(path, &reference, json),
        DepotCommand::Pull(PullArgs {
            reference,
            output_dir,
            public_keys,
            trust_policy,
            json,
        }) => parcel_ops::pull(&reference, output_dir, public_keys, trust_policy, json),
    }
}

pub(crate) fn with_cli_a2a_policy<T>(policy: CliA2aPolicy, f: impl FnOnce() -> T) -> T {
    with_a2a_operator_policy_overrides(
        A2aOperatorPolicyOverrides {
            allowed_origins: policy.allowed_origins,
            trust_policy: policy.trust_policy.map(|path| path.display().to_string()),
        },
        f,
    )
}

fn lint(path: PathBuf, emit_json: bool) -> Result<()> {
    let path = resolve_agentfile_path(path);
    let source =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;

    let parsed =
        parse_agentfile(&source).with_context(|| format!("failed to parse {}", path.display()))?;
    let report = validate_agentfile_at_path(&path, &parsed);

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

pub(crate) fn is_agentfile_target(path: &Path) -> bool {
    if path.is_dir() {
        return path.join("Agentfile").exists();
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "Agentfile")
}

pub(crate) fn default_parcels_root_for_source(path: &Path) -> PathBuf {
    let agentfile_path = resolve_agentfile_path(path.to_path_buf());
    agentfile_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".dispatch/parcels")
}

pub(crate) fn resolve_parcels_root(path: &Path) -> PathBuf {
    if is_agentfile_target(path) {
        return default_parcels_root_for_source(path);
    }

    if path.is_file()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "manifest.json")
    {
        return path
            .parent()
            .and_then(Path::parent)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
    }

    if path.is_dir() {
        if path.join("manifest.json").exists() {
            return path
                .parent()
                .map(PathBuf::from)
                .unwrap_or_else(|| path.to_path_buf());
        }

        let nested = path.join(".dispatch/parcels");
        if nested.exists() {
            return nested;
        }
    }

    path.to_path_buf()
}

pub(crate) fn build_parcel_from_source(
    path: PathBuf,
    output_dir: Option<PathBuf>,
) -> Result<dispatch_core::LoadedParcel> {
    let agentfile_path = resolve_agentfile_path(path);
    let output_root =
        output_dir.unwrap_or_else(|| default_parcels_root_for_source(agentfile_path.as_path()));

    let built = build_agentfile(
        &agentfile_path,
        &BuildOptions {
            output_root: output_root.clone(),
        },
    )
    .with_context(|| format!("failed to build {}", agentfile_path.display()))?;

    dispatch_core::load_parcel(&built.parcel_dir)
        .with_context(|| format!("failed to load parcel {}", built.parcel_dir.display()))
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
    for warning in &built.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(())
}

fn state_command(command: StateCommand) -> Result<()> {
    match command {
        StateCommand::Ls {
            root,
            parcels_root,
            json,
        } => state::state_ls(root, parcels_root, json),
        StateCommand::Gc {
            root,
            parcels_root,
            dry_run,
        } => state::state_gc(root, parcels_root, dry_run),
        StateCommand::Migrate {
            source_digest,
            target_digest,
            root,
            force,
        } => state::state_migrate(&source_digest, &target_digest, root, force),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Cli, CliA2aPolicy, CliToolApprovalMode, Command, CourierCommand, DepotCommand, EvalArgs,
        InspectArgs, KeygenArgs, ParcelCommand, PullArgs, PushArgs, SignArgs, SkillCommand,
        SkillSynthesisOverrideArgs, StateCommand, ValidateSkillArgs, VerifyArgs,
    };
    use clap::Parser;
    use dispatch_core::{
        BuildOptions, ConversationMessage, CourierPluginExec, CourierPluginManifest,
        CourierSession, PluginTransport, ToolExitExpectation, ToolRunResult, ToolTextExpectation,
        build_agentfile,
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
            label: Some("demo".to_string()),
            turn_count: 2,
            elapsed_ms: 42,
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

        crate::run::persist_session(Some(&path), &session).unwrap();
        let loaded = crate::run::load_session(&path).unwrap();

        assert_eq!(loaded, session);
    }

    #[test]
    fn persist_session_creates_parent_directories() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/session.json");
        let session = sample_session();

        crate::run::persist_session(Some(&path), &session).unwrap();

        assert!(path.exists());
        assert_eq!(crate::run::load_session(&path).unwrap(), session);
    }

    #[test]
    fn persist_session_is_noop_without_path() {
        crate::run::persist_session(None, &sample_session()).unwrap();
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
            "--a2a-allowed-origins",
            "https://agents.example.com,broker.internal",
            "--a2a-trust-policy",
            "/tmp/a2a-policy.toml",
            "--print-prompt",
        ])
        .unwrap();

        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(args.exec.courier, "docker");
        assert_eq!(
            args.exec.registry.as_deref(),
            Some(Path::new("/tmp/plugins.json"))
        );
        assert_eq!(
            args.exec.a2a_allowed_origins.as_deref(),
            Some("https://agents.example.com,broker.internal")
        );
        assert_eq!(
            args.exec.a2a_trust_policy.as_deref(),
            Some(Path::new("/tmp/a2a-policy.toml"))
        );
        assert!(args.exec.print_prompt);
    }

    #[test]
    fn cli_parses_eval_a2a_policy_overrides() {
        let cli = Cli::try_parse_from([
            "dispatch",
            "parcel",
            "eval",
            ".",
            "--courier",
            "native",
            "--a2a-allowed-origins",
            "https://agents.example.com",
            "--a2a-trust-policy",
            "/tmp/a2a-policy.toml",
        ])
        .unwrap();

        let Command::Parcel { command } = cli.command else {
            panic!("expected parcel command");
        };
        let ParcelCommand::Eval(EvalArgs {
            a2a_allowed_origins,
            a2a_trust_policy,
            ..
        }) = command
        else {
            panic!("expected parcel eval command");
        };
        assert_eq!(
            a2a_allowed_origins.as_deref(),
            Some("https://agents.example.com")
        );
        assert_eq!(
            a2a_trust_policy.as_deref(),
            Some(Path::new("/tmp/a2a-policy.toml"))
        );
    }

    #[test]
    fn cli_parses_parcel_list_command() {
        let cli =
            Cli::try_parse_from(["dispatch", "parcel", "list", "examples/demo", "--json"]).unwrap();

        let Command::Parcel { command } = cli.command else {
            panic!("expected parcel command");
        };
        let ParcelCommand::List(args) = command else {
            panic!("expected parcel list command");
        };
        assert_eq!(args.path, Path::new("examples/demo"));
        assert!(args.json);
    }

    #[test]
    fn cli_accepts_parcel_ls_alias() {
        let cli = Cli::try_parse_from(["dispatch", "parcel", "ls"]).unwrap();

        let Command::Parcel { command } = cli.command else {
            panic!("expected parcel command");
        };
        let ParcelCommand::List(args) = command else {
            panic!("expected parcel list command");
        };
        assert_eq!(args.path, Path::new("."));
        assert!(!args.json);
    }

    #[test]
    fn resolve_parcels_root_prefers_source_context_store() {
        let dir = tempdir().unwrap();
        let source_dir = dir.path().join("agent");
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(
            source_dir.join("Agentfile"),
            "FROM dispatch/native:latest\n",
        )
        .unwrap();

        assert_eq!(
            crate::resolve_parcels_root(&source_dir),
            source_dir.join(".dispatch/parcels")
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_parcels_root_accepts_built_parcel_directory() {
        let dir = tempdir().unwrap();
        let (parcel_dir, _) = build_test_image(dir.path());

        assert_eq!(
            crate::resolve_parcels_root(&parcel_dir),
            parcel_dir.parent().unwrap()
        );
    }

    #[test]
    fn cli_parses_push_command() {
        let cli = Cli::try_parse_from([
            "dispatch",
            "depot",
            "push",
            "manifest.json",
            "file:///tmp/depot::acme/monitor:v1",
            "--json",
        ])
        .unwrap();

        let Command::Depot { command } = cli.command else {
            panic!("expected depot command");
        };
        let DepotCommand::Push(PushArgs {
            path,
            reference,
            json,
        }) = command
        else {
            panic!("expected depot push command");
        };
        assert_eq!(path, Path::new("manifest.json"));
        assert_eq!(reference, "file:///tmp/depot::acme/monitor:v1");
        assert!(json);
    }

    #[test]
    fn cli_parses_pull_command_with_output_dir() {
        let cli = Cli::try_parse_from([
            "dispatch",
            "depot",
            "pull",
            "file:///tmp/depot::acme/monitor:v1",
            "--output-dir",
            "/tmp/parcels",
            "--json",
        ])
        .unwrap();

        let Command::Depot { command } = cli.command else {
            panic!("expected depot command");
        };
        let DepotCommand::Pull(PullArgs {
            reference,
            output_dir,
            public_keys,
            trust_policy,
            json,
        }) = command
        else {
            panic!("expected depot pull command");
        };
        assert_eq!(reference, "file:///tmp/depot::acme/monitor:v1");
        assert_eq!(output_dir.as_deref(), Some(Path::new("/tmp/parcels")));
        assert!(public_keys.is_empty());
        assert!(trust_policy.is_none());
        assert!(json);
    }

    #[test]
    fn resolve_trust_policy_path_prefers_explicit_then_env() {
        let explicit = Some(PathBuf::from("/tmp/explicit.toml"));
        let env_path = crate::parcel_ops::resolve_trust_policy_path(explicit.clone(), |_| {
            Some(std::ffi::OsString::from("/tmp/env.toml"))
        });
        assert_eq!(env_path, explicit);

        let env_only = crate::parcel_ops::resolve_trust_policy_path(None, |_| {
            Some(std::ffi::OsString::from("/tmp/env.toml"))
        });
        assert_eq!(env_only, Some(PathBuf::from("/tmp/env.toml")));
    }

    #[test]
    fn merge_public_keys_preserves_explicit_keys_and_deduplicates_policy_keys() {
        let merged = crate::parcel_ops::merge_public_keys(
            vec![
                PathBuf::from("/tmp/explicit-a.pub"),
                PathBuf::from("/tmp/shared.pub"),
            ],
            vec![
                PathBuf::from("/tmp/shared.pub"),
                PathBuf::from("/tmp/policy-b.pub"),
            ],
        );
        assert_eq!(
            merged,
            vec![
                PathBuf::from("/tmp/explicit-a.pub"),
                PathBuf::from("/tmp/shared.pub"),
                PathBuf::from("/tmp/policy-b.pub"),
            ]
        );
    }

    #[test]
    fn scoped_tool_expectations_match_only_the_named_tool() {
        let tool_results = vec![
            ToolRunResult {
                tool: "search".to_string(),
                command: "search".to_string(),
                args: Vec::new(),
                exit_code: 0,
                stdout: "found result".to_string(),
                stderr: String::new(),
            },
            ToolRunResult {
                tool: "fetch".to_string(),
                command: "fetch".to_string(),
                args: Vec::new(),
                exit_code: 1,
                stdout: "found result".to_string(),
                stderr: "timed out".to_string(),
            },
        ];

        assert!(crate::eval::tool_text_expectation_satisfied(
            &tool_results,
            &ToolTextExpectation::Scoped {
                tool: "search".to_string(),
                contains: "result".to_string(),
            },
            |tool_result| &tool_result.stdout,
        ));
        assert!(!crate::eval::tool_text_expectation_satisfied(
            &tool_results,
            &ToolTextExpectation::Scoped {
                tool: "search".to_string(),
                contains: "timed out".to_string(),
            },
            |tool_result| &tool_result.stderr,
        ));
        assert!(crate::eval::tool_exit_expectation_satisfied(
            &tool_results,
            &ToolExitExpectation::Scoped {
                tool: "fetch".to_string(),
                exit_code: 1,
            },
        ));
        assert!(!crate::eval::tool_exit_expectation_satisfied(
            &tool_results,
            &ToolExitExpectation::Scoped {
                tool: "search".to_string(),
                exit_code: 1,
            },
        ));
    }

    #[test]
    fn cli_parses_verify_with_public_keys() {
        let cli = Cli::try_parse_from([
            "dispatch",
            "parcel",
            "verify",
            "manifest.json",
            "--public-key",
            "release.dispatch-public.json",
            "--public-key",
            "ops.dispatch-public.json",
        ])
        .unwrap();

        let Command::Parcel { command } = cli.command else {
            panic!("expected parcel command");
        };
        let ParcelCommand::Verify(VerifyArgs {
            path,
            public_keys,
            json,
        }) = command
        else {
            panic!("expected parcel verify command");
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
            "parcel",
            "keygen",
            "--key-id",
            "release",
            "--output-dir",
            "/tmp/keys",
        ])
        .unwrap();
        let Command::Parcel { command } = keygen.command else {
            panic!("expected parcel command");
        };
        let ParcelCommand::Keygen(KeygenArgs { key_id, output_dir }) = command else {
            panic!("expected parcel keygen command");
        };
        assert_eq!(key_id, "release");
        assert_eq!(output_dir.as_deref(), Some(Path::new("/tmp/keys")));

        let sign = Cli::try_parse_from([
            "dispatch",
            "parcel",
            "sign",
            "manifest.json",
            "--secret-key",
            "release.dispatch-secret.json",
        ])
        .unwrap();
        let Command::Parcel { command } = sign.command else {
            panic!("expected parcel command");
        };
        let ParcelCommand::Sign(SignArgs { path, secret_key }) = command else {
            panic!("expected parcel sign command");
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
        assert_eq!(args.exec.courier, "native");
    }

    #[test]
    fn cli_parses_run_skill_command() {
        let cli = Cli::try_parse_from([
            "dispatch",
            "skill",
            "run",
            "skills/file-analyst",
            "--courier",
            "docker",
            "--model",
            "gpt-5-mini",
            "--provider",
            "openai",
            "--list-tools",
        ])
        .unwrap();

        let Command::Skill { command } = cli.command else {
            panic!("expected skill command");
        };
        let SkillCommand::Run(args) = command else {
            panic!("expected skill run command");
        };
        assert_eq!(args.path, PathBuf::from("skills/file-analyst"));
        assert_eq!(args.exec.courier, "docker");
        assert_eq!(args.synthesis.model.as_deref(), Some("gpt-5-mini"));
        assert_eq!(args.synthesis.provider.as_deref(), Some("openai"));
        assert!(args.exec.list_tools);
    }

    #[test]
    fn cli_parses_validate_skill_command() {
        let cli = Cli::try_parse_from([
            "dispatch",
            "skill",
            "validate",
            "skills/file-analyst/SKILL.md",
            "--courier",
            "docker",
            "--model",
            "gpt-5-mini",
            "--provider",
            "openai",
            "--entrypoint",
            "chat",
        ])
        .unwrap();

        let Command::Skill { command } = cli.command else {
            panic!("expected skill command");
        };
        let SkillCommand::Validate(args) = command else {
            panic!("expected skill validate command");
        };
        let ValidateSkillArgs {
            path,
            courier,
            synthesis:
                SkillSynthesisOverrideArgs {
                    model,
                    provider,
                    entrypoint,
                },
        } = *args;
        assert_eq!(path, PathBuf::from("skills/file-analyst/SKILL.md"));
        assert_eq!(courier, "docker");
        assert_eq!(model.as_deref(), Some("gpt-5-mini"));
        assert_eq!(provider.as_deref(), Some("openai"));
        assert_eq!(entrypoint.as_deref(), Some("chat"));
    }

    #[test]
    fn cli_accepts_bare_heartbeat_flag() {
        let cli = Cli::try_parse_from(["dispatch", "run", "manifest.json", "--heartbeat"]).unwrap();

        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(args.exec.heartbeat.as_deref(), Some(""));
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
            "parcel",
            "inspect",
            "manifest.json",
            "--courier",
            "remote-demo",
            "--registry",
            "/tmp/plugins.json",
        ])
        .unwrap();

        let Command::Parcel { command } = cli.command else {
            panic!("expected parcel command");
        };
        let ParcelCommand::Inspect(InspectArgs {
            path,
            courier,
            registry,
            json,
        }) = command
        else {
            panic!("expected parcel inspect command");
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
            "--a2a-allowed-origins",
            "https://agents.example.com",
            "--a2a-trust-policy",
            "/tmp/a2a-policy.toml",
        ])
        .unwrap();

        let Command::Courier { command } = cli.command else {
            panic!("expected courier command");
        };
        let CourierCommand::Conformance {
            name,
            registry,
            json,
            a2a_allowed_origins,
            a2a_trust_policy,
        } = command
        else {
            panic!("expected courier conformance subcommand");
        };
        assert_eq!(name, "native");
        assert_eq!(registry.as_deref(), Some(Path::new("/tmp/plugins.json")));
        assert_eq!(
            a2a_allowed_origins.as_deref(),
            Some("https://agents.example.com")
        );
        assert_eq!(
            a2a_trust_policy.as_deref(),
            Some(Path::new("/tmp/a2a-policy.toml"))
        );
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

        crate::run::run(super::RunArgs {
            path: parcel_dir,
            exec: super::RunExecutionArgs {
                courier: plugin_name.to_string(),
                registry: Some(registry_path),
                session_file: Some(session_path.clone()),
                chat: Some("hello".to_string()),
                job: None,
                heartbeat: None,
                interactive: false,
                print_prompt: false,
                list_tools: false,
                json: false,
                tool: None,
                input: None,
                tool_approval: None,
                a2a_allowed_origins: None,
                a2a_trust_policy: None,
            },
        })
        .unwrap();

        let session = crate::run::load_session(&session_path).unwrap();
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

        crate::inspect::inspect(
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

        crate::eval::eval(
            source_dir,
            "demo-eval-plugin",
            Some(registry_path),
            false,
            None,
            CliToolApprovalMode::Never,
            CliA2aPolicy::default(),
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn eval_rejects_session_digest_mismatch() {
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
        install_eval_test_plugin_with_session_digests(
            dir.path(),
            &registry_path,
            "demo-eval-plugin-mismatch",
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            &built.digest,
        );

        let error = crate::eval::eval(
            source_dir,
            "demo-eval-plugin-mismatch",
            Some(registry_path),
            false,
            None,
            CliToolApprovalMode::Never,
            CliA2aPolicy::default(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("eval failed"));
    }

    #[test]
    fn courier_conformance_runs_against_native() {
        crate::conformance::courier_conformance("native", None, false, CliA2aPolicy::default())
            .unwrap();
    }

    #[test]
    fn courier_conformance_runs_against_wasm() {
        crate::conformance::courier_conformance("wasm", None, false, CliA2aPolicy::default())
            .unwrap();
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

        crate::parcel_ops::push(parcel_dir.clone(), &depot_ref, false).unwrap();
        crate::parcel_ops::pull(&depot_ref, Some(pull_root.clone()), Vec::new(), None, false)
            .unwrap();

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

        let entries = crate::state::collect_state_entries(&state_root, &parcels_root).unwrap();
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

        crate::state::state_gc(Some(state_root.clone()), Some(parcels_root), false).unwrap();

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

        crate::state::state_migrate(
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

        crate::parcel_ops::keygen("release", Some(keys_dir.clone())).unwrap();
        let secret_key = keys_dir.join("release.dispatch-secret.json");
        let public_key = keys_dir.join("release.dispatch-public.json");

        crate::parcel_ops::sign(parcel_dir.clone(), &secret_key).unwrap();
        crate::parcel_ops::verify(parcel_dir, vec![public_key], false).unwrap();
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

        crate::parcel_ops::keygen("release", Some(keys_dir.clone())).unwrap();
        let secret_key = keys_dir.join("release.dispatch-secret.json");
        let public_key = keys_dir.join("release.dispatch-public.json");
        crate::parcel_ops::sign(parcel_dir.clone(), &secret_key).unwrap();
        crate::parcel_ops::push(parcel_dir, &depot_ref, false).unwrap();

        crate::parcel_ops::pull(
            &depot_ref,
            Some(pull_root.clone()),
            vec![public_key],
            None,
            false,
        )
        .unwrap();
    }

    #[test]
    fn pull_can_verify_signatures_via_trust_policy() {
        let dir = tempdir().unwrap();
        let (parcel_dir, parcel_digest) = build_test_image(dir.path());
        let keys_dir = dir.path().join("keys");
        let depot_ref = format!(
            "file://{}::acme/trusted-fixture:v1",
            dir.path().join("depot").display()
        );
        let pull_root = dir.path().join("pulled");
        let policy_path = dir.path().join("trust-policy.toml");

        crate::parcel_ops::keygen("release", Some(keys_dir.clone())).unwrap();
        let secret_key = keys_dir.join("release.dispatch-secret.json");
        crate::parcel_ops::sign(parcel_dir.clone(), &secret_key).unwrap();
        crate::parcel_ops::push(parcel_dir, &depot_ref, false).unwrap();
        fs::write(
            &policy_path,
            "[[rules]]\nrepository_prefix = \"acme/trusted-fixture\"\nrequire_signatures = true\npublic_keys = [\"keys/release.dispatch-public.json\"]\n",
        )
        .unwrap();

        crate::parcel_ops::pull(
            &depot_ref,
            Some(pull_root.clone()),
            Vec::new(),
            Some(policy_path),
            false,
        )
        .unwrap();
        assert!(
            pull_root
                .join(&parcel_digest)
                .join("manifest.json")
                .exists()
        );
    }

    #[test]
    fn pull_fails_when_trust_policy_requires_signatures_but_none_exist() {
        let dir = tempdir().unwrap();
        let (parcel_dir, _) = build_test_image(dir.path());
        let keys_dir = dir.path().join("keys");
        let depot_ref = format!(
            "file://{}::acme/unsigned-fixture:v1",
            dir.path().join("depot").display()
        );
        let pull_root = dir.path().join("pulled");
        let policy_path = dir.path().join("trust-policy.toml");

        crate::parcel_ops::keygen("release", Some(keys_dir.clone())).unwrap();
        crate::parcel_ops::push(parcel_dir, &depot_ref, false).unwrap();
        fs::write(
            &policy_path,
            "[[rules]]\nrepository_prefix = \"acme/unsigned-fixture\"\nrequire_signatures = true\npublic_keys = [\"keys/release.dispatch-public.json\"]\n",
        )
        .unwrap();

        let error = crate::parcel_ops::pull(
            &depot_ref,
            Some(pull_root),
            Vec::new(),
            Some(policy_path),
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("signature verification failed"));
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
                    "  printf '%s\\n' '{{\"kind\":\"capabilities\",\"capabilities\":{{\"courier_id\":\"demo-jsonl\",\"kind\":\"custom\",\"supports_chat\":true,\"supports_job\":false,\"supports_heartbeat\":false,\"supports_local_tools\":false,\"supports_mounts\":[]}}}}'\n",
                    "elif printf '%s' \"$line\" | grep -q '\"kind\":\"validate_parcel\"'; then\n",
                    "  printf '%s\\n' '{{\"kind\":\"ok\"}}'\n",
                    "elif printf '%s' \"$line\" | grep -q '\"kind\":\"inspect\"'; then\n",
                    "  printf '%s\\n' '{{\"kind\":\"inspection\",\"inspection\":{{\"courier_id\":\"demo-jsonl\",\"kind\":\"custom\",\"entrypoint\":\"chat\",\"required_secrets\":[],\"mounts\":[],\"local_tools\":[]}}}}'\n",
                    "elif printf '%s' \"$line\" | grep -q '\"kind\":\"open_session\"'; then\n",
                    "  printf '%s\\n' '{{\"kind\":\"session\",\"session\":{{\"id\":\"demo-jsonl-session\",\"parcel_digest\":\"{parcel_digest}\",\"entrypoint\":\"chat\",\"turn_count\":1,\"history\":[],\"backend_state\":\"open\"}}}}'\n",
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
                "[[cases]]\n",
                "name = \"smoke\"\n",
                "input = \"What time is it?\"\n",
                "expects_tool = \"system_time\"\n",
                "expects_text_contains = \"plugin reply\"\n\n",
                "[[cases]]\n",
                "name = \"exact\"\n",
                "input = \"What time is it?\"\n",
                "expects_tools = [\"system_time\"]\n",
                "expects_tool_count = 1\n",
                "expects_tool_stdout_contains = \"2026-04-03\"\n",
                "expects_tool_exit_code = 0\n",
                "expects_text_exact = \"plugin reply\"\n",
                "expects_text_not_contains = \"wrong\"\n\n",
                "[[cases]]\n",
                "name = \"invalid-entrypoint\"\n",
                "input = \"\"\n",
                "entrypoint = \"unsupported\"\n",
                "expects_error_contains = \"unsupported eval entrypoint\"\n",
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
        install_eval_test_plugin_with_session_digests(
            root,
            registry_path,
            name,
            parcel_digest,
            parcel_digest,
        );
    }

    #[cfg(unix)]
    fn install_eval_test_plugin_with_session_digests(
        root: &Path,
        registry_path: &Path,
        name: &str,
        open_session_digest: &str,
        done_session_digest: &str,
    ) {
        let script_path = root.join("eval-plugin.sh");
        let script = concat!(
            "#!/bin/sh\n",
            "set -eu\n",
            "while IFS= read -r line; do\n",
            "if printf '%s' \"$line\" | grep -q '\"kind\":\"capabilities\"'; then\n",
            "  printf '%s\\n' '{\"kind\":\"capabilities\",\"capabilities\":{\"courier_id\":\"demo-eval-plugin\",\"kind\":\"custom\",\"supports_chat\":true,\"supports_job\":false,\"supports_heartbeat\":false,\"supports_local_tools\":false,\"supports_mounts\":[]}}'\n",
            "elif printf '%s' \"$line\" | grep -q '\"kind\":\"validate_parcel\"'; then\n",
            "  printf '%s\\n' '{\"kind\":\"ok\"}'\n",
            "elif printf '%s' \"$line\" | grep -q '\"kind\":\"inspect\"'; then\n",
            "  printf '%s\\n' '{\"kind\":\"inspection\",\"inspection\":{\"courier_id\":\"demo-eval-plugin\",\"kind\":\"custom\",\"entrypoint\":\"chat\",\"required_secrets\":[],\"mounts\":[],\"local_tools\":[]}}'\n",
            "elif printf '%s' \"$line\" | grep -q '\"kind\":\"open_session\"'; then\n",
            "  printf '%s\\n' '{\"kind\":\"session\",\"session\":{\"id\":\"demo-eval-session\",\"parcel_digest\":\"__OPEN_DIGEST__\",\"entrypoint\":\"chat\",\"turn_count\":0,\"history\":[],\"backend_state\":\"open\"}}'\n",
            "elif printf '%s' \"$line\" | grep -q '\"kind\":\"run\"'; then\n",
            "  printf '%s\\n' '{\"kind\":\"event\",\"event\":{\"kind\":\"tool_call_started\",\"invocation\":{\"name\":\"system_time\",\"input\":null},\"command\":\"builtin\",\"args\":[]}}'\n",
            "  printf '%s\\n' '{\"kind\":\"event\",\"event\":{\"kind\":\"tool_call_finished\",\"result\":{\"tool\":\"system_time\",\"command\":\"builtin\",\"args\":[],\"exit_code\":0,\"stdout\":\"2026-04-03T00:00:00Z\",\"stderr\":\"\"}}}'\n",
            "  printf '%s\\n' '{\"kind\":\"event\",\"event\":{\"kind\":\"message\",\"role\":\"assistant\",\"content\":\"plugin reply\"}}'\n",
            "  printf '%s\\n' '{\"kind\":\"done\",\"session\":{\"id\":\"demo-eval-session\",\"parcel_digest\":\"__DONE_DIGEST__\",\"entrypoint\":\"chat\",\"turn_count\":1,\"history\":[{\"role\":\"user\",\"content\":\"What time is it?\"},{\"role\":\"assistant\",\"content\":\"plugin reply\"}]}}'\n",
            "else\n",
            "  printf '%s\\n' '{\"kind\":\"error\",\"error\":{\"code\":\"unexpected_request\",\"message\":\"unhandled request\"}}'\n",
            "  exit 1\n",
            "fi\n",
            "done\n"
        )
        .replace("__OPEN_DIGEST__", open_session_digest)
        .replace("__DONE_DIGEST__", done_session_digest);
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
