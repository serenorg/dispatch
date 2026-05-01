use anyhow::{Context, Result, bail};
use dispatch_core::{
    install_channel_plugin, install_courier_plugin, install_database_plugin,
    install_deployment_plugin, install_provider_plugin, resolve_courier, resolve_deployment_plugin,
};
use dispatch_deployment_protocol::{PluginRequest, PluginResponse, ValidationIssue};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
};

const DEFAULT_DISPATCH_CONFIG_FILE: &str = "dispatch.toml";

#[derive(Debug, Deserialize)]
struct DispatchProjectConfig {
    #[serde(default)]
    parcel: Option<PathBuf>,
    #[serde(default = "default_courier_name")]
    courier: String,
    #[serde(default)]
    courier_registry: Option<PathBuf>,
    #[serde(default)]
    channel_registry: Option<PathBuf>,
    #[serde(default)]
    provider_registry: Option<PathBuf>,
    #[serde(default)]
    database_registry: Option<PathBuf>,
    #[serde(default)]
    deployment_registry: Option<PathBuf>,
    #[serde(default)]
    tool_approval: Option<crate::CliToolApprovalMode>,
    #[serde(default)]
    extensions: Vec<ExtensionInstallConfig>,
    #[serde(default)]
    deployments: Vec<DeploymentBindingConfig>,
    #[serde(default)]
    channels: Vec<ChannelBindingConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExtensionKind {
    Channel,
    Courier,
    Provider,
    Database,
    Deployment,
}

#[derive(Debug, Deserialize)]
struct ExtensionInstallConfig {
    #[serde(default)]
    kind: Option<ExtensionKind>,
    manifest: PathBuf,
}

#[derive(Debug, Deserialize)]
struct ExtensionManifestProbe {
    #[serde(default)]
    kind: Option<ExtensionManifestKind>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExtensionManifestKind {
    Channel,
    Courier,
    Connector,
    Provider,
    Database,
    Deployment,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DeploymentReconcileMode {
    #[default]
    Validate,
    TestRun,
    Deploy,
    Upsert,
}

impl DeploymentReconcileMode {
    fn label(self) -> &'static str {
        match self {
            Self::Validate => "validate",
            Self::TestRun => "test-run",
            Self::Deploy => "deploy",
            Self::Upsert => "upsert",
        }
    }

    fn mutates_remote_resources(self) -> bool {
        matches!(self, Self::Deploy | Self::Upsert)
    }
}

#[derive(Debug, Deserialize)]
struct DeploymentBindingConfig {
    #[serde(default)]
    name: Option<String>,
    plugin: String,
    #[serde(default)]
    reconcile: DeploymentReconcileMode,
    #[serde(default)]
    sample_input: Option<String>,
    #[serde(default)]
    config_file: Option<PathBuf>,
    #[serde(default)]
    config: Option<toml::Value>,
    #[serde(default)]
    spec_file: Option<PathBuf>,
    #[serde(default)]
    spec: Option<toml::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ChannelBindingMode {
    Listen,
    Poll,
}

#[derive(Debug, Deserialize)]
struct ChannelBindingConfig {
    #[serde(default)]
    name: Option<String>,
    plugin: String,
    mode: ChannelBindingMode,
    #[serde(default)]
    listen: Option<String>,
    #[serde(default)]
    interval_ms: Option<u64>,
    #[serde(default)]
    once: bool,
    #[serde(default)]
    deliver_replies: bool,
    #[serde(default)]
    session_root: Option<PathBuf>,
    #[serde(default)]
    config_file: Option<PathBuf>,
    #[serde(default)]
    config: Option<toml::Value>,
}

#[derive(Debug, Clone)]
struct ResolvedDispatchProject {
    config_path: PathBuf,
    root_dir: PathBuf,
    parcel: Option<PathBuf>,
    courier: String,
    courier_registry: PathBuf,
    channel_registry: PathBuf,
    provider_registry: PathBuf,
    database_registry: PathBuf,
    deployment_registry: PathBuf,
    deployment_state_path: PathBuf,
    extensions: Vec<ResolvedExtensionInstall>,
    deployments: Vec<ResolvedDeploymentBinding>,
    channels: Vec<crate::channel_cmds::ChannelRuntimeBindingArgs>,
}

#[derive(Debug, Clone)]
struct ResolvedExtensionInstall {
    kind: ExtensionKind,
    name: String,
    manifest: PathBuf,
}

#[derive(Debug, Clone)]
struct ResolvedDeploymentBinding {
    label: String,
    plugin: String,
    reconcile: DeploymentReconcileMode,
    config: Option<Value>,
    spec: Value,
    sample_input: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct DeploymentStateFile {
    #[serde(default)]
    deployments: BTreeMap<String, DeploymentStateEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeploymentStateEntry {
    plugin: String,
    name: String,
    deployment_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revision_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_reconciled_at: Option<String>,
}

fn deployment_state_key(plugin: &str, name: &str) -> String {
    format!("{plugin}::{name}")
}

fn default_courier_name() -> String {
    "native".to_string()
}

pub(crate) fn up(args: crate::UpArgs) -> Result<()> {
    let project = load_dispatch_project(&args.path)?;

    println!("Using config: {}", project.config_path.display());
    match &project.parcel {
        Some(parcel) => println!("Parcel: {}", parcel.display()),
        None => println!("Parcel: <none>"),
    }
    println!("Courier: {}", project.courier);
    println!("Courier Registry: {}", project.courier_registry.display());
    println!("Channel Registry: {}", project.channel_registry.display());
    println!("Provider Registry: {}", project.provider_registry.display());
    println!("Database Registry: {}", project.database_registry.display());
    println!(
        "Deployment Registry: {}",
        project.deployment_registry.display()
    );
    println!(
        "Deployment State: {}",
        project.deployment_state_path.display()
    );

    if args.dry_run {
        print_dry_run(&project);
        return Ok(());
    }

    reconcile_extensions(&project)?;

    if !project.deployments.is_empty() {
        confirm_remote_reconcile_or_bail(&project, args.yes)?;
        run_deployments(&project)?;
    }

    if project.channels.is_empty() {
        if project.deployments.is_empty() {
            bail!(
                "{} does not declare any [[channels]] or [[deployments]] bindings",
                project.config_path.display()
            );
        }
        return Ok(());
    }

    resolve_courier(&project.courier, Some(&project.courier_registry)).with_context(|| {
        format!(
            "failed to resolve courier `{}` from {}",
            project.courier,
            project.courier_registry.display()
        )
    })?;

    run_channel_bindings(project)
}

fn confirm_remote_reconcile_or_bail(project: &ResolvedDispatchProject, yes: bool) -> Result<()> {
    let mutating: Vec<&ResolvedDeploymentBinding> = project
        .deployments
        .iter()
        .filter(|binding| binding.reconcile.mutates_remote_resources())
        .collect();
    if mutating.is_empty() || yes {
        return Ok(());
    }

    println!("The following deployment bindings create or reconcile remote resources:");
    for binding in &mutating {
        println!(
            "  - {} via {} ({})",
            binding.label,
            binding.plugin,
            binding.reconcile.label()
        );
    }
    print!("Apply these bindings? [y/N] ");
    io::stdout()
        .flush()
        .context("failed to flush deployment confirmation prompt")?;
    let mut response = String::new();
    io::stdin()
        .lock()
        .read_line(&mut response)
        .context("failed to read deployment confirmation response")?;
    let trimmed = response.trim();
    if matches!(trimmed, "y" | "Y" | "yes" | "YES") {
        Ok(())
    } else {
        bail!("deployment reconcile aborted by user")
    }
}

fn run_deployments(project: &ResolvedDispatchProject) -> Result<()> {
    let mut state = load_deployment_state(&project.deployment_state_path)?;
    let mut state_changed = false;
    for deployment in &project.deployments {
        let plugin =
            resolve_deployment_plugin(&deployment.plugin, Some(&project.deployment_registry))
                .with_context(|| {
                    format!(
                        "failed to resolve deployment plugin `{}` from {}",
                        deployment.plugin,
                        project.deployment_registry.display()
                    )
                })?;
        let request = match deployment.reconcile {
            DeploymentReconcileMode::Validate => PluginRequest::Validate {
                spec: deployment.spec.clone(),
            },
            DeploymentReconcileMode::TestRun => PluginRequest::TestRun {
                spec: deployment.spec.clone(),
                sample_input: deployment.sample_input.clone(),
            },
            DeploymentReconcileMode::Deploy => PluginRequest::Deploy {
                spec: deployment.spec.clone(),
            },
            DeploymentReconcileMode::Upsert => PluginRequest::Upsert {
                name: deployment.label.clone(),
                spec: deployment.spec.clone(),
            },
        };
        let response = crate::deployment_cmds::invoke_deployment_plugin(
            &plugin,
            deployment.config.clone(),
            request,
        )
        .with_context(|| format!("deployment `{}` failed", deployment.label))?;

        state_changed |= handle_deployment_response(deployment, response, &mut state)?;
    }
    if state_changed {
        save_deployment_state(&project.deployment_state_path, &state)?;
    }
    Ok(())
}

fn handle_deployment_response(
    deployment: &ResolvedDeploymentBinding,
    response: PluginResponse,
    state: &mut DeploymentStateFile,
) -> Result<bool> {
    match (deployment.reconcile, response) {
        (DeploymentReconcileMode::Validate, PluginResponse::Validation { result }) => {
            if result.ok {
                println!("Deployment `{}` validated", deployment.label);
                Ok(false)
            } else {
                bail!(
                    "deployment `{}` failed validation: {}",
                    deployment.label,
                    format_validation_issues(&result.issues)
                )
            }
        }
        (DeploymentReconcileMode::TestRun, PluginResponse::TestRunResult { result }) => {
            println!(
                "Deployment `{}` test-run status: {}",
                deployment.label, result.status
            );
            Ok(false)
        }
        (DeploymentReconcileMode::Deploy, PluginResponse::Deployment { deployment: result })
        | (DeploymentReconcileMode::Upsert, PluginResponse::Deployment { deployment: result }) => {
            let key = deployment_state_key(&deployment.plugin, &deployment.label);
            let same_id_as_state = state
                .deployments
                .get(&key)
                .map(|entry| &entry.deployment_id)
                == Some(&result.deployment_id);
            let verb = match deployment.reconcile {
                DeploymentReconcileMode::Upsert if same_id_as_state => "reconciled",
                DeploymentReconcileMode::Upsert => "upserted",
                _ => "deployed",
            };
            println!(
                "Deployment `{}` {} as {}",
                deployment.label, verb, result.deployment_id
            );
            state.deployments.insert(
                key,
                DeploymentStateEntry {
                    plugin: deployment.plugin.clone(),
                    name: deployment.label.clone(),
                    deployment_id: result.deployment_id.clone(),
                    revision_id: result.revision_id.clone(),
                    last_reconciled_at: Some(chrono::Utc::now().to_rfc3339()),
                },
            );
            Ok(true)
        }
        (_, PluginResponse::Error { error }) => {
            bail!(
                "deployment `{}` plugin error: {}: {}",
                deployment.label,
                error.code,
                error.message
            )
        }
        (_, other) => {
            bail!(
                "deployment `{}` returned unexpected response: {other:?}",
                deployment.label
            )
        }
    }
}

fn load_deployment_state(path: &Path) -> Result<DeploymentStateFile> {
    if !path.exists() {
        return Ok(DeploymentStateFile::default());
    }
    let body =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    if body.trim().is_empty() {
        return Ok(DeploymentStateFile::default());
    }
    serde_json::from_str(&body)
        .with_context(|| format!("failed to parse deployment state {}", path.display()))
}

fn save_deployment_state(path: &Path, state: &DeploymentStateFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let body =
        serde_json::to_string_pretty(state).context("failed to serialize deployment state")?;
    fs::write(path, body).with_context(|| format!("failed to write {}", path.display()))
}

fn format_validation_issues(issues: &[ValidationIssue]) -> String {
    if issues.is_empty() {
        return "no issues returned".to_string();
    }

    issues
        .iter()
        .map(|issue| {
            let field = issue.field.as_deref().unwrap_or("<root>");
            format!("{field}: {}: {}", issue.code, issue.message)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn print_dry_run(project: &ResolvedDispatchProject) {
    println!("Dry Run: yes");
    if project.extensions.is_empty() {
        println!("Extension Installs: none");
    } else {
        println!("Extension Installs:");
        for extension in &project.extensions {
            let kind = match extension.kind {
                ExtensionKind::Channel => "channel",
                ExtensionKind::Courier => "courier",
                ExtensionKind::Provider => "provider",
                ExtensionKind::Database => "database",
                ExtensionKind::Deployment => "deployment",
            };
            println!("  - {kind}: {}", extension.manifest.display());
        }
    }

    print_dry_run_courier_status(project);

    if project.deployments.is_empty() {
        println!("Deployment Bindings: none");
    } else {
        println!("Deployment Bindings:");
        let state = load_deployment_state(&project.deployment_state_path).unwrap_or_default();
        for deployment in &project.deployments {
            let key = deployment_state_key(&deployment.plugin, &deployment.label);
            let known_id = state
                .deployments
                .get(&key)
                .map(|entry| entry.deployment_id.as_str())
                .unwrap_or("<none>");
            println!(
                "  - {} via {} ({}); known deployment_id={}",
                deployment.label,
                deployment.plugin,
                deployment.reconcile.label(),
                known_id
            );
        }
    }

    if project.channels.is_empty() {
        println!("Channel Bindings: none");
    } else {
        println!("Channel Bindings:");
        for binding in &project.channels {
            let mode = match &binding.mode {
                crate::channel_cmds::ChannelRuntimeMode::Listen { listen } => {
                    format!("listen {listen}")
                }
                crate::channel_cmds::ChannelRuntimeMode::Poll { interval_ms } => {
                    match interval_ms {
                        Some(interval_ms) => format!("poll every {interval_ms}ms"),
                        None => "poll plugin default interval".to_string(),
                    }
                }
            };
            println!("  - {} via {} ({mode})", binding.label, binding.plugin);
        }
    }
}

fn print_dry_run_courier_status(project: &ResolvedDispatchProject) {
    match resolve_courier(&project.courier, Some(&project.courier_registry)) {
        Ok(_) => {
            println!("Courier Status: `{}` resolves", project.courier);
        }
        Err(error) => {
            if project.extensions.iter().any(|ext| {
                matches!(ext.kind, ExtensionKind::Courier) && ext.name == project.courier
            }) {
                println!(
                    "Courier Status: `{}` will be installed via [[extensions]] at `dispatch up`",
                    project.courier
                );
            } else {
                println!(
                    "Courier Status: `{}` does not resolve ({error})",
                    project.courier
                );
            }
        }
    }
}

fn load_dispatch_project(path: &Path) -> Result<ResolvedDispatchProject> {
    let config_path = resolve_dispatch_config_path(path)?;
    let body = fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let parsed: DispatchProjectConfig = toml::from_str(&body)
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    let root_dir = config_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let parcel = parsed
        .parcel
        .map(|value| resolve_relative_path(&root_dir, value));
    let courier_registry = parsed
        .courier_registry
        .map(|value| resolve_relative_path(&root_dir, value))
        .unwrap_or_else(|| root_dir.join(".dispatch/registries/couriers.json"));
    let channel_registry = parsed
        .channel_registry
        .map(|value| resolve_relative_path(&root_dir, value))
        .unwrap_or_else(|| root_dir.join(".dispatch/registries/channels.json"));
    let provider_registry = parsed
        .provider_registry
        .map(|value| resolve_relative_path(&root_dir, value))
        .unwrap_or_else(|| root_dir.join(".dispatch/registries/providers.json"));
    let database_registry = parsed
        .database_registry
        .map(|value| resolve_relative_path(&root_dir, value))
        .unwrap_or_else(|| root_dir.join(".dispatch/registries/databases.json"));
    let deployment_registry = parsed
        .deployment_registry
        .map(|value| resolve_relative_path(&root_dir, value))
        .unwrap_or_else(|| root_dir.join(".dispatch/registries/deployments.json"));
    let deployment_state_path = root_dir.join(".dispatch/state/deployments.json");

    let mut deployments = Vec::with_capacity(parsed.deployments.len());
    for deployment in parsed.deployments {
        deployments.push(resolve_deployment_binding(&root_dir, deployment)?);
    }

    let mut channels = Vec::with_capacity(parsed.channels.len());
    for binding in parsed.channels {
        channels.push(resolve_channel_binding(
            &root_dir,
            parcel.as_deref(),
            &parsed.courier,
            &courier_registry,
            &channel_registry,
            parsed.tool_approval,
            binding,
        )?);
    }

    Ok(ResolvedDispatchProject {
        config_path,
        root_dir: root_dir.clone(),
        parcel,
        courier: parsed.courier,
        courier_registry,
        channel_registry,
        provider_registry,
        database_registry,
        deployment_registry,
        deployment_state_path,
        extensions: parsed
            .extensions
            .into_iter()
            .map(|extension| {
                let manifest = resolve_relative_path(&root_dir, extension.manifest);
                let probe = load_extension_manifest_probe(&manifest)?;
                Ok(ResolvedExtensionInstall {
                    kind: resolve_extension_kind(&manifest, extension.kind, &probe)?,
                    name: resolve_extension_name(&manifest, &probe)?,
                    manifest,
                })
            })
            .collect::<Result<Vec<_>>>()?,
        deployments,
        channels,
    })
}

fn resolve_deployment_binding(
    root_dir: &Path,
    binding: DeploymentBindingConfig,
) -> Result<ResolvedDeploymentBinding> {
    let label = binding
        .name
        .clone()
        .unwrap_or_else(|| binding.plugin.clone());
    let config = load_optional_structured_config(
        root_dir,
        binding.config,
        binding.config_file.as_deref(),
        StructuredConfigKeys {
            inline_key: "config",
            file_key: "config_file",
            label: "deployment config",
        },
    )?;
    let spec = load_required_structured_config(
        root_dir,
        binding.spec,
        binding.spec_file.as_deref(),
        StructuredConfigKeys {
            inline_key: "spec",
            file_key: "spec_file",
            label: "deployment spec",
        },
        &label,
    )?;

    Ok(ResolvedDeploymentBinding {
        label,
        plugin: binding.plugin,
        reconcile: binding.reconcile,
        config,
        spec,
        sample_input: binding.sample_input,
    })
}

fn resolve_channel_binding(
    root_dir: &Path,
    parcel: Option<&Path>,
    courier: &str,
    courier_registry: &Path,
    channel_registry: &Path,
    tool_approval: Option<crate::CliToolApprovalMode>,
    binding: ChannelBindingConfig,
) -> Result<crate::channel_cmds::ChannelRuntimeBindingArgs> {
    let label = binding
        .name
        .clone()
        .unwrap_or_else(|| binding.plugin.clone());

    if binding.deliver_replies && parcel.is_none() {
        bail!(
            "channel `{label}` sets `deliver_replies = true`, but dispatch.toml does not declare `parcel`"
        );
    }

    let config = load_channel_config(root_dir, binding.config, binding.config_file.as_deref())?;
    let session_root = binding
        .session_root
        .map(|value| resolve_relative_path(root_dir, value))
        .unwrap_or_else(|| root_dir.join(".dispatch/channel-sessions"));

    let mode = match binding.mode {
        ChannelBindingMode::Listen => {
            let listen = binding.listen.ok_or_else(|| {
                anyhow::anyhow!(
                    "channel `{label}` requires `listen = \"host:port\"` when mode = \"listen\""
                )
            })?;
            crate::channel_cmds::ChannelRuntimeMode::Listen { listen }
        }
        ChannelBindingMode::Poll => crate::channel_cmds::ChannelRuntimeMode::Poll {
            interval_ms: binding.interval_ms,
        },
    };

    Ok(crate::channel_cmds::ChannelRuntimeBindingArgs {
        label,
        plugin: binding.plugin,
        config,
        parcel: parcel.map(PathBuf::from),
        courier: courier.to_string(),
        courier_registry: Some(courier_registry.to_path_buf()),
        session_root: Some(session_root),
        tool_approval,
        deliver_replies: binding.deliver_replies,
        once: binding.once,
        emit_json: false,
        registry: Some(channel_registry.to_path_buf()),
        mode,
    })
}

#[derive(Debug, Clone, Copy)]
struct StructuredConfigKeys {
    inline_key: &'static str,
    file_key: &'static str,
    label: &'static str,
}

fn load_channel_config(
    root_dir: &Path,
    inline: Option<toml::Value>,
    config_file: Option<&Path>,
) -> Result<Value> {
    load_optional_structured_config(
        root_dir,
        inline,
        config_file,
        StructuredConfigKeys {
            inline_key: "config",
            file_key: "config_file",
            label: "channel config",
        },
    )
    .map(|value| value.unwrap_or_else(|| serde_json::json!({})))
}

fn load_optional_structured_config(
    root_dir: &Path,
    inline: Option<toml::Value>,
    config_file: Option<&Path>,
    keys: StructuredConfigKeys,
) -> Result<Option<Value>> {
    match (inline, config_file) {
        (Some(_), Some(_)) => {
            bail!(
                "use either `{}` or `{}` for {}, not both",
                keys.inline_key,
                keys.file_key,
                keys.label
            )
        }
        (None, None) => Ok(None),
        (Some(value), None) => toml_value_to_json(value).map(Some),
        (None, Some(path)) => crate::channel_cmds::load_structured_value_file(
            &resolve_relative_path(root_dir, path.to_path_buf()),
            keys.label,
        )
        .map(Some),
    }
}

fn load_required_structured_config(
    root_dir: &Path,
    inline: Option<toml::Value>,
    config_file: Option<&Path>,
    keys: StructuredConfigKeys,
    binding_label: &str,
) -> Result<Value> {
    load_optional_structured_config(root_dir, inline, config_file, keys)?.ok_or_else(|| {
        anyhow::anyhow!(
            "deployment `{binding_label}` requires either `{}` or `{}`",
            keys.inline_key,
            keys.file_key
        )
    })
}

fn toml_value_to_json(value: toml::Value) -> Result<Value> {
    serde_json::to_value(value).context("failed to convert TOML value into JSON-compatible config")
}

fn resolve_dispatch_config_path(path: &Path) -> Result<PathBuf> {
    let path = if path.is_dir() {
        path.join(DEFAULT_DISPATCH_CONFIG_FILE)
    } else {
        path.to_path_buf()
    };
    if !path.exists() {
        bail!("dispatch config `{}` does not exist", path.display());
    }
    Ok(path)
}

fn resolve_relative_path(root_dir: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        root_dir.join(path)
    }
}

fn load_extension_manifest_probe(manifest: &Path) -> Result<ExtensionManifestProbe> {
    let body = fs::read_to_string(manifest)
        .with_context(|| format!("failed to read extension manifest {}", manifest.display()))?;
    serde_json::from_str(&body)
        .with_context(|| format!("failed to parse extension manifest {}", manifest.display()))
}

fn resolve_extension_kind(
    manifest: &Path,
    explicit: Option<ExtensionKind>,
    probe: &ExtensionManifestProbe,
) -> Result<ExtensionKind> {
    if let Some(kind) = explicit {
        return Ok(kind);
    }

    match probe.kind {
        Some(ExtensionManifestKind::Channel) => Ok(ExtensionKind::Channel),
        Some(ExtensionManifestKind::Courier) => Ok(ExtensionKind::Courier),
        Some(ExtensionManifestKind::Connector) => bail!(
            "extension manifest `{}` declares unsupported kind `connector`",
            manifest.display()
        ),
        Some(ExtensionManifestKind::Provider) => Ok(ExtensionKind::Provider),
        Some(ExtensionManifestKind::Database) => Ok(ExtensionKind::Database),
        Some(ExtensionManifestKind::Deployment) => Ok(ExtensionKind::Deployment),
        None => match manifest.file_name().and_then(|value| value.to_str()) {
            Some("channel-plugin.json") => Ok(ExtensionKind::Channel),
            Some("courier-plugin.json") => Ok(ExtensionKind::Courier),
            Some("provider-plugin.json") => Ok(ExtensionKind::Provider),
            Some("database-plugin.json") => Ok(ExtensionKind::Database),
            Some("deployment-plugin.json") => Ok(ExtensionKind::Deployment),
            _ => bail!(
                "extension manifest `{}` must declare `kind`, or use a conventional filename like `channel-plugin.json`, `courier-plugin.json`, `provider-plugin.json`, `database-plugin.json`, or `deployment-plugin.json`",
                manifest.display()
            ),
        },
    }
}

fn resolve_extension_name(manifest: &Path, probe: &ExtensionManifestProbe) -> Result<String> {
    probe.name.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "extension manifest `{}` must declare `name`",
            manifest.display()
        )
    })
}

fn reconcile_extensions(project: &ResolvedDispatchProject) -> Result<()> {
    for extension in &project.extensions {
        match extension.kind {
            ExtensionKind::Channel => {
                let installed =
                    install_channel_plugin(&extension.manifest, Some(&project.channel_registry))
                        .with_context(|| {
                            format!(
                                "failed to install channel plugin from {}",
                                extension.manifest.display()
                            )
                        })?;
                println!("Installed channel plugin `{}`", installed.name);
            }
            ExtensionKind::Courier => {
                let installed =
                    install_courier_plugin(&extension.manifest, Some(&project.courier_registry))
                        .with_context(|| {
                            format!(
                                "failed to install courier plugin from {}",
                                extension.manifest.display()
                            )
                        })?;
                println!("Installed courier plugin `{}`", installed.name);
            }
            ExtensionKind::Provider => {
                let installed =
                    install_provider_plugin(&extension.manifest, Some(&project.provider_registry))
                        .with_context(|| {
                            format!(
                                "failed to install provider plugin from {}",
                                extension.manifest.display()
                            )
                        })?;
                println!("Installed provider plugin `{}`", installed.name);
            }
            ExtensionKind::Database => {
                let installed =
                    install_database_plugin(&extension.manifest, Some(&project.database_registry))
                        .with_context(|| {
                            format!(
                                "failed to install database plugin from {}",
                                extension.manifest.display()
                            )
                        })?;
                println!("Installed database plugin `{}`", installed.name);
            }
            ExtensionKind::Deployment => {
                let installed = install_deployment_plugin(
                    &extension.manifest,
                    Some(&project.deployment_registry),
                )
                .with_context(|| {
                    format!(
                        "failed to install deployment plugin from {}",
                        extension.manifest.display()
                    )
                })?;
                println!("Installed deployment plugin `{}`", installed.name);
            }
        }
    }
    Ok(())
}

fn run_channel_bindings(project: ResolvedDispatchProject) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let channel_count = project.channels.len();
    let one_shot_count = project
        .channels
        .iter()
        .filter(|binding| binding.once)
        .count();

    for binding in project.channels {
        println!(
            "Starting channel `{}` via plugin `{}`",
            binding.label, binding.plugin
        );
        let tx = tx.clone();
        thread::spawn(move || {
            let label = binding.label.clone();
            let once = binding.once;
            let result = crate::channel_cmds::run_channel_runtime_binding(binding)
                .map_err(|error| error.to_string());
            let _ = tx.send((label, once, result));
        });
    }
    drop(tx);

    let mut completed_one_shot = 0usize;
    let mut completed_total = 0usize;
    while let Ok((label, once, result)) = rx.recv() {
        completed_total += 1;
        match result {
            Ok(()) if once => {
                completed_one_shot += 1;
                println!("Channel `{label}` completed");
                if completed_total == channel_count {
                    return Ok(());
                }
                if one_shot_count == channel_count && completed_one_shot == one_shot_count {
                    return Ok(());
                }
            }
            Ok(()) => {
                bail!("channel `{label}` exited unexpectedly");
            }
            Err(error) => {
                bail!("channel `{label}` failed: {error}");
            }
        }
    }

    bail!(
        "dispatch up exited without any active channel bindings under {}",
        project.root_dir.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn path_string(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    fn toml_string_literal(value: &str) -> String {
        toml::Value::String(value.to_string()).to_string()
    }

    #[test]
    fn load_dispatch_project_uses_project_local_registries_by_default() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("dispatch.toml");
        fs::write(
            &config_path,
            r#"
parcel = "./Agentfile"

[[channels]]
plugin = "channel-test"
mode = "poll"
once = true
"#,
        )
        .unwrap();

        let project = load_dispatch_project(&config_path).unwrap();
        assert_eq!(project.parcel, Some(dir.path().join("Agentfile")));
        assert_eq!(
            project.courier_registry,
            dir.path().join(".dispatch/registries/couriers.json")
        );
        assert_eq!(
            project.channel_registry,
            dir.path().join(".dispatch/registries/channels.json")
        );
        assert_eq!(
            project.provider_registry,
            dir.path().join(".dispatch/registries/providers.json")
        );
        assert_eq!(
            project.database_registry,
            dir.path().join(".dispatch/registries/databases.json")
        );
        assert_eq!(
            project.deployment_registry,
            dir.path().join(".dispatch/registries/deployments.json")
        );
        assert_eq!(project.channels.len(), 1);
    }

    #[test]
    fn load_dispatch_project_rejects_channel_config_and_config_file_together() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("dispatch.toml");
        fs::write(
            &config_path,
            r#"
parcel = "./Agentfile"

[[channels]]
plugin = "channel-test"
mode = "poll"
config = { token = "abc" }
config_file = "./channel.toml"
"#,
        )
        .unwrap();

        let error = load_dispatch_project(&config_path).unwrap_err().to_string();
        assert!(error.contains("use either `config` or `config_file`"));
    }

    #[test]
    fn load_dispatch_project_rejects_deliver_replies_without_parcel() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("dispatch.toml");
        fs::write(
            &config_path,
            r#"
[[channels]]
plugin = "channel-test"
mode = "poll"
deliver_replies = true
"#,
        )
        .unwrap();

        let error = load_dispatch_project(&config_path).unwrap_err().to_string();
        assert!(error.contains("deliver_replies = true"));
        assert!(error.contains("does not declare `parcel`"));
    }

    #[test]
    fn load_dispatch_project_accepts_deployment_bindings() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("dispatch.toml");
        let spec_path = dir.path().join("deployment.json");
        fs::write(
            &spec_path,
            r#"{ "name": "research-monitor", "mode": "llm" }"#,
        )
        .unwrap();
        fs::write(
            &config_path,
            format!(
                r#"
deployment_registry = "./custom/deployments.json"

[[deployments]]
name = "research-monitor"
plugin = "seren-agent"
reconcile = "test_run"
sample_input = "hello"
config = {{ api_origin = "https://api.example.com", api_key = "seren_test" }}
spec_file = {}
"#,
                toml_string_literal(&path_string(&spec_path))
            ),
        )
        .unwrap();

        let project = load_dispatch_project(&config_path).unwrap();
        assert_eq!(
            project.deployment_registry,
            dir.path().join("custom/deployments.json")
        );
        assert_eq!(project.deployments.len(), 1);
        let deployment = &project.deployments[0];
        assert_eq!(deployment.label, "research-monitor");
        assert_eq!(deployment.plugin, "seren-agent");
        assert!(matches!(
            deployment.reconcile,
            DeploymentReconcileMode::TestRun
        ));
        assert_eq!(deployment.sample_input.as_deref(), Some("hello"));
        assert_eq!(
            deployment
                .config
                .as_ref()
                .and_then(|value| value.get("api_origin"))
                .and_then(Value::as_str),
            Some("https://api.example.com")
        );
        assert_eq!(
            deployment.spec.get("name").and_then(Value::as_str),
            Some("research-monitor")
        );
    }

    #[test]
    fn load_dispatch_project_rejects_deployment_without_spec() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("dispatch.toml");
        fs::write(
            &config_path,
            r#"
[[deployments]]
name = "research-monitor"
plugin = "seren-agent"
"#,
        )
        .unwrap();

        let error = load_dispatch_project(&config_path).unwrap_err().to_string();
        assert!(error.contains("requires either `spec` or `spec_file`"));
    }

    #[test]
    fn load_dispatch_project_infers_extension_kind_from_manifest() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("dispatch.toml");
        let manifest_path = dir.path().join("channel-plugin.json");
        fs::write(
            &manifest_path,
            r#"
{
    "kind": "channel",
    "name": "channel-test",
    "version": "0.1.0",
    "protocol": "jsonl",
    "protocol_version": 1,
    "entrypoint": { "command": "./channel-test", "args": [] }
}
"#,
        )
        .unwrap();
        fs::write(
            &config_path,
            format!(
                r#"
[[extensions]]
manifest = {}
"#,
                toml_string_literal(&path_string(&manifest_path))
            ),
        )
        .unwrap();

        let project = load_dispatch_project(&config_path).unwrap();
        assert_eq!(project.extensions.len(), 1);
        assert!(matches!(project.extensions[0].kind, ExtensionKind::Channel));
    }

    #[test]
    fn load_dispatch_project_accepts_provider_database_and_deployment_extension_kinds() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("dispatch.toml");
        let provider_manifest = dir.path().join("provider-plugin.json");
        let database_manifest = dir.path().join("database-plugin.json");
        let deployment_manifest = dir.path().join("deployment-plugin.json");
        fs::write(
            &provider_manifest,
            r#"
{
    "kind": "provider",
    "name": "seren-models",
    "version": "0.1.0",
    "transport": "jsonl",
    "protocol_version": 1,
    "exec": { "command": "./seren-models", "args": [] }
}
"#,
        )
        .unwrap();
        fs::write(
            &database_manifest,
            r#"
{
    "kind": "database",
    "name": "seren-db",
    "version": "0.1.0",
    "transport": "jsonl",
    "protocol_version": 1,
    "exec": { "command": "./seren-db", "args": [] }
}
"#,
        )
        .unwrap();
        fs::write(
            &deployment_manifest,
            r#"
{
    "kind": "deployment",
    "name": "seren-agent",
    "version": "0.1.0",
    "transport": "jsonl",
    "protocol_version": 1,
    "exec": { "command": "./seren-agent", "args": [] }
}
"#,
        )
        .unwrap();
        fs::write(
            &config_path,
            format!(
                r#"
[[extensions]]
manifest = {}

[[extensions]]
manifest = {}

[[extensions]]
manifest = {}
"#,
                toml_string_literal(&path_string(&provider_manifest)),
                toml_string_literal(&path_string(&database_manifest)),
                toml_string_literal(&path_string(&deployment_manifest))
            ),
        )
        .unwrap();

        let project = load_dispatch_project(&config_path).unwrap();
        assert_eq!(project.extensions.len(), 3);
        assert!(matches!(
            project.extensions[0].kind,
            ExtensionKind::Provider
        ));
        assert!(matches!(
            project.extensions[1].kind,
            ExtensionKind::Database
        ));
        assert!(matches!(
            project.extensions[2].kind,
            ExtensionKind::Deployment
        ));
    }

    #[test]
    fn load_dispatch_project_rejects_uninferrable_extension_kind() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("dispatch.toml");
        let manifest_path = dir.path().join("plugin.json");
        fs::write(
            &manifest_path,
            r#"
{
    "name": "plugin-test",
    "version": "0.1.0"
}
"#,
        )
        .unwrap();
        fs::write(
            &config_path,
            format!(
                r#"
[[extensions]]
manifest = {}
"#,
                toml_string_literal(&path_string(&manifest_path))
            ),
        )
        .unwrap();

        let error = load_dispatch_project(&config_path).unwrap_err().to_string();
        assert!(error.contains("must declare `kind`"));
    }
}
