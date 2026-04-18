use anyhow::{Context, Result, bail};
use dispatch_core::{install_channel_plugin, install_courier_plugin, resolve_courier};
use serde::Deserialize;
use serde_json::Value;
use std::{
    fs,
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
    tool_approval: Option<crate::CliToolApprovalMode>,
    #[serde(default)]
    extensions: Vec<ExtensionInstallConfig>,
    #[serde(default)]
    channels: Vec<ChannelBindingConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExtensionKind {
    Channel,
    Courier,
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
    extensions: Vec<ResolvedExtensionInstall>,
    channels: Vec<crate::channel_cmds::ChannelRuntimeBindingArgs>,
}

#[derive(Debug, Clone)]
struct ResolvedExtensionInstall {
    kind: ExtensionKind,
    name: String,
    manifest: PathBuf,
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

    if args.dry_run {
        print_dry_run(&project);
        return Ok(());
    }

    reconcile_extensions(&project)?;

    resolve_courier(&project.courier, Some(&project.courier_registry)).with_context(|| {
        format!(
            "failed to resolve courier `{}` from {}",
            project.courier,
            project.courier_registry.display()
        )
    })?;

    if project.channels.is_empty() {
        bail!(
            "{} does not declare any [[channels]] bindings",
            project.config_path.display()
        );
    }

    run_channels(project)
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
            };
            println!("  - {kind}: {}", extension.manifest.display());
        }
    }

    print_dry_run_courier_status(project);

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
        channels,
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

    let config = load_binding_config(root_dir, binding.config, binding.config_file.as_deref())?;
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

fn load_binding_config(
    root_dir: &Path,
    inline: Option<toml::Value>,
    config_file: Option<&Path>,
) -> Result<Value> {
    match (inline, config_file) {
        (Some(_), Some(_)) => {
            bail!("use either `config` or `config_file` for a channel binding, not both")
        }
        (None, None) => Ok(serde_json::json!({})),
        (Some(value), None) => toml_value_to_json(value),
        (None, Some(path)) => crate::channel_cmds::load_structured_value_file(
            &resolve_relative_path(root_dir, path.to_path_buf()),
            "channel config",
        ),
    }
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
        None => match manifest.file_name().and_then(|value| value.to_str()) {
            Some("channel-plugin.json") => Ok(ExtensionKind::Channel),
            Some("courier-plugin.json") => Ok(ExtensionKind::Courier),
            _ => bail!(
                "extension manifest `{}` must declare `kind`, or use a conventional filename like `channel-plugin.json` or `courier-plugin.json`",
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
        }
    }
    Ok(())
}

fn run_channels(project: ResolvedDispatchProject) -> Result<()> {
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
