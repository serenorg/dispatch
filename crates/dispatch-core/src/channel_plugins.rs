use crate::{
    channel_plugin_protocol::{
        CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelPluginRequest, ChannelPluginRequestEnvelope,
        ChannelPluginResponse,
    },
    plugins::{PluginRegistryError, PluginTransport, hash_file_sha256, resolve_plugin_exec_path},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{BufRead as _, BufReader, Write as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use thiserror::Error;

const CHANNEL_REGISTRY_RELATIVE_PATH: &str = ".config/dispatch/channels.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelPluginExec {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// Manifest for an installed channel plugin in the host registry.
///
/// The registry stores a normalised subset of the full channel-plugin.json
/// manifest.  During install the host reads the rich manifest, extracts the
/// fields it needs for resolution and process spawning, and stores this
/// compact form in `~/.config/dispatch/channels.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelPluginManifest {
    pub name: String,
    pub version: String,
    pub protocol_version: u32,
    pub transport: PluginTransport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub exec: ChannelPluginExec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress: Option<ChannelPluginIngress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelPluginIngress {
    #[serde(default)]
    pub endpoints: Vec<ChannelIngressEndpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<ChannelIngressTrust>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelIngressEndpoint {
    pub path: String,
    #[serde(default)]
    pub methods: Vec<String>,
    #[serde(default)]
    pub host_managed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelIngressTrust {
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_name: Option<String>,
    #[serde(default)]
    pub host_managed: bool,
}

/// The on-disk channel-plugin.json format shipped alongside channel plugin
/// binaries.  This is a superset of what the host stores -- it includes
/// bootstrap, auth, capabilities, and requirements blocks that the host reads
/// once at install time to extract the compact `ChannelPluginManifest`.
#[derive(Debug, Clone, Deserialize)]
struct ChannelPluginOnDiskManifest {
    #[serde(default)]
    kind: Option<OnDiskManifestKind>,
    name: String,
    version: String,
    protocol_version: u32,
    /// Channel manifests use `"protocol": "jsonl"` where courier manifests
    /// use `"transport": "jsonl"`.  Both map to `PluginTransport`.
    #[serde(alias = "transport")]
    protocol: PluginTransport,
    #[serde(default)]
    description: Option<String>,
    #[serde(alias = "exec")]
    entrypoint: ChannelPluginExec,
    #[serde(default)]
    capabilities: Option<OnDiskCapabilities>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OnDiskManifestKind {
    Channel,
    Courier,
    Connector,
}

#[derive(Debug, Clone, Deserialize)]
struct OnDiskCapabilities {
    #[serde(default)]
    channel: Option<OnDiskChannelCapability>,
}

#[derive(Debug, Clone, Deserialize)]
struct OnDiskChannelCapability {
    #[serde(default)]
    platform: Option<String>,
    #[serde(default)]
    allowed_paths: Vec<String>,
    #[serde(default)]
    ingress: Option<OnDiskChannelIngress>,
}

#[derive(Debug, Clone, Deserialize)]
struct OnDiskChannelIngress {
    #[serde(default)]
    endpoints: Vec<OnDiskChannelIngressEndpoint>,
    #[serde(default)]
    trust: Option<OnDiskChannelIngressTrust>,
}

#[derive(Debug, Clone, Deserialize)]
struct OnDiskChannelIngressEndpoint {
    path: String,
    #[serde(default)]
    methods: Vec<String>,
    #[serde(default)]
    host_managed: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct OnDiskChannelIngressTrust {
    mode: String,
    #[serde(default)]
    header_name: Option<String>,
    #[serde(default)]
    secret_name: Option<String>,
    #[serde(default)]
    host_managed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ChannelPluginRegistry {
    #[serde(default)]
    pub plugins: Vec<ChannelPluginManifest>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ChannelCatalogEntry {
    pub name: String,
    pub description: Option<String>,
    pub protocol_version: u32,
    pub transport: PluginTransport,
    pub platform: Option<String>,
    pub ingress_paths: Vec<String>,
    pub command: String,
    pub args: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ChannelPluginCallError {
    #[error("failed to spawn channel plugin `{channel}`: {source}")]
    SpawnPlugin {
        channel: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write request to channel plugin `{channel}`: {source}")]
    WritePluginRequest {
        channel: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read response from channel plugin `{channel}`: {source}")]
    ReadPluginResponse {
        channel: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to wait for channel plugin `{channel}`: {source}")]
    WaitPlugin {
        channel: String,
        #[source]
        source: std::io::Error,
    },
    #[error("channel plugin `{channel}` protocol error: {message}")]
    PluginProtocol { channel: String, message: String },
    #[error("channel plugin `{channel}` exited with status {status}: {stderr}")]
    PluginExit {
        channel: String,
        status: i32,
        stderr: String,
    },
}

pub fn default_channel_registry_path() -> Result<PathBuf, PluginRegistryError> {
    let home = std::env::var_os("HOME").ok_or(PluginRegistryError::MissingHome)?;
    Ok(PathBuf::from(home).join(CHANNEL_REGISTRY_RELATIVE_PATH))
}

pub fn load_channel_registry(
    path: Option<&Path>,
) -> Result<ChannelPluginRegistry, PluginRegistryError> {
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => default_channel_registry_path()?,
    };
    if !path.exists() {
        return Ok(ChannelPluginRegistry::default());
    }

    let body = fs::read_to_string(&path).map_err(|source| PluginRegistryError::ReadFile {
        path: path.display().to_string(),
        source,
    })?;
    serde_json::from_str(&body).map_err(|source| PluginRegistryError::ParseJson {
        path: path.display().to_string(),
        source,
    })
}

pub fn install_channel_plugin(
    manifest_path: &Path,
    registry_path: Option<&Path>,
) -> Result<ChannelPluginManifest, PluginRegistryError> {
    let body =
        fs::read_to_string(manifest_path).map_err(|source| PluginRegistryError::ReadFile {
            path: manifest_path.display().to_string(),
            source,
        })?;
    let on_disk: ChannelPluginOnDiskManifest =
        serde_json::from_str(&body).map_err(|source| PluginRegistryError::ParseJson {
            path: manifest_path.display().to_string(),
            source,
        })?;

    if let Some(kind) = &on_disk.kind
        && *kind != OnDiskManifestKind::Channel
    {
        return Err(PluginRegistryError::InvalidManifest {
            path: manifest_path.display().to_string(),
            message: format!("kind must be `channel`, got `{}`", kind.as_str()),
        });
    }

    let platform = on_disk
        .capabilities
        .as_ref()
        .and_then(|c| c.channel.as_ref())
        .and_then(|ch| ch.platform.clone());
    let ingress = on_disk
        .capabilities
        .as_ref()
        .and_then(|c| c.channel.as_ref())
        .and_then(extract_channel_ingress);

    let mut manifest = ChannelPluginManifest {
        name: on_disk.name,
        version: on_disk.version,
        protocol_version: on_disk.protocol_version,
        transport: on_disk.protocol,
        description: on_disk.description,
        exec: on_disk.entrypoint,
        platform,
        ingress,
        installed_sha256: None,
    };

    validate_channel_plugin_manifest(manifest_path, &manifest)?;

    let exec_path = resolve_plugin_exec_path(manifest_path, &manifest.exec.command)?;
    manifest.exec.command = exec_path.display().to_string();
    manifest.installed_sha256 = Some(hash_file_sha256(&exec_path)?);

    let registry_path = match registry_path {
        Some(path) => path.to_path_buf(),
        None => default_channel_registry_path()?,
    };
    let mut registry = load_channel_registry(Some(&registry_path))?;
    registry
        .plugins
        .retain(|plugin| plugin.name != manifest.name);
    registry.plugins.push(manifest.clone());
    registry
        .plugins
        .sort_by(|left, right| left.name.cmp(&right.name));

    if let Some(parent) = registry_path.parent() {
        fs::create_dir_all(parent).map_err(|source| PluginRegistryError::WriteFile {
            path: parent.display().to_string(),
            source,
        })?;
    }
    let payload = serde_json::to_string_pretty(&registry).map_err(|source| {
        PluginRegistryError::ParseJson {
            path: registry_path.display().to_string(),
            source,
        }
    })?;
    fs::write(&registry_path, payload).map_err(|source| PluginRegistryError::WriteFile {
        path: registry_path.display().to_string(),
        source,
    })?;

    Ok(manifest)
}

pub fn list_channel_catalog(
    registry_path: Option<&Path>,
) -> Result<Vec<ChannelCatalogEntry>, PluginRegistryError> {
    let registry = load_channel_registry(registry_path)?;
    let mut entries = registry
        .plugins
        .into_iter()
        .map(|plugin| ChannelCatalogEntry {
            name: plugin.name,
            description: plugin.description,
            protocol_version: plugin.protocol_version,
            transport: plugin.transport,
            platform: plugin.platform,
            ingress_paths: plugin
                .ingress
                .as_ref()
                .map(|ingress| {
                    ingress
                        .endpoints
                        .iter()
                        .map(|endpoint| endpoint.path.clone())
                        .collect()
                })
                .unwrap_or_default(),
            command: plugin.exec.command,
            args: plugin.exec.args,
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.name.cmp(&right.name));

    Ok(entries)
}

pub fn resolve_channel_plugin(
    name: &str,
    registry_path: Option<&Path>,
) -> Result<ChannelPluginManifest, PluginRegistryError> {
    let registry = load_channel_registry(registry_path)?;
    registry
        .plugins
        .into_iter()
        .find(|plugin| plugin.name == name)
        .ok_or_else(|| PluginRegistryError::UnknownChannel {
            name: name.to_string(),
        })
}

pub fn resolve_channel_plugin_for_ingress(
    method: &str,
    path: &str,
    registry_path: Option<&Path>,
) -> Result<ChannelPluginManifest, PluginRegistryError> {
    let registry = load_channel_registry(registry_path)?;
    let method = method.to_ascii_uppercase();
    let mut matches = registry
        .plugins
        .into_iter()
        .filter(|plugin| plugin_matches_ingress(plugin, &method, path))
        .collect::<Vec<_>>();

    match matches.len() {
        0 => Err(PluginRegistryError::NoChannelIngressMatch {
            method,
            path: path.to_string(),
        }),
        1 => Ok(matches.remove(0)),
        _ => Err(PluginRegistryError::AmbiguousChannelIngressMatch {
            method,
            path: path.to_string(),
            names: matches.into_iter().map(|plugin| plugin.name).collect(),
        }),
    }
}

pub fn call_channel_plugin(
    manifest: &ChannelPluginManifest,
    request: ChannelPluginRequest,
) -> Result<ChannelPluginResponse, ChannelPluginCallError> {
    let mut command = Command::new(&manifest.exec.command);
    command
        .args(&manifest.exec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|source| ChannelPluginCallError::SpawnPlugin {
            channel: manifest.name.clone(),
            source,
        })?;

    {
        let mut stdin =
            child
                .stdin
                .take()
                .ok_or_else(|| ChannelPluginCallError::PluginProtocol {
                    channel: manifest.name.clone(),
                    message: "channel plugin stdin was not captured".to_string(),
                })?;
        serde_json::to_writer(
            &mut stdin,
            &ChannelPluginRequestEnvelope {
                protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
                request,
            },
        )
        .map_err(|source| ChannelPluginCallError::PluginProtocol {
            channel: manifest.name.clone(),
            message: format!("failed to serialize channel plugin request: {source}"),
        })?;
        stdin
            .write_all(b"\n")
            .map_err(|source| ChannelPluginCallError::WritePluginRequest {
                channel: manifest.name.clone(),
                source,
            })?;
        stdin
            .flush()
            .map_err(|source| ChannelPluginCallError::WritePluginRequest {
                channel: manifest.name.clone(),
                source,
            })?;
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ChannelPluginCallError::PluginProtocol {
            channel: manifest.name.clone(),
            message: "channel plugin stdout was not captured".to_string(),
        })?;
    let mut stdout = BufReader::new(stdout);
    let mut line = String::new();
    let bytes = stdout.read_line(&mut line).map_err(|source| {
        ChannelPluginCallError::ReadPluginResponse {
            channel: manifest.name.clone(),
            source,
        }
    })?;

    let mut stderr = String::new();
    if let Some(mut stderr_pipe) = child.stderr.take() {
        use std::io::Read as _;
        stderr_pipe.read_to_string(&mut stderr).map_err(|source| {
            ChannelPluginCallError::ReadPluginResponse {
                channel: manifest.name.clone(),
                source,
            }
        })?;
    }
    let status = child
        .wait()
        .map_err(|source| ChannelPluginCallError::WaitPlugin {
            channel: manifest.name.clone(),
            source,
        })?;
    if bytes == 0 {
        if status.success() {
            return Err(ChannelPluginCallError::PluginProtocol {
                channel: manifest.name.clone(),
                message: "channel plugin produced no response".to_string(),
            });
        }

        return Err(ChannelPluginCallError::PluginExit {
            channel: manifest.name.clone(),
            status: status.code().unwrap_or(-1),
            stderr: stderr.trim().to_string(),
        });
    }

    let response =
        serde_json::from_str::<ChannelPluginResponse>(line.trim_end()).map_err(|source| {
            ChannelPluginCallError::PluginProtocol {
                channel: manifest.name.clone(),
                message: format!("invalid channel plugin JSON: {source}"),
            }
        })?;
    if status.success() {
        return Ok(response);
    }

    Err(ChannelPluginCallError::PluginExit {
        channel: manifest.name.clone(),
        status: status.code().unwrap_or(-1),
        stderr: stderr.trim().to_string(),
    })
}

pub fn validate_channel_plugin_manifest(
    path: &Path,
    manifest: &ChannelPluginManifest,
) -> Result<(), PluginRegistryError> {
    if manifest.name.trim().is_empty() {
        return Err(PluginRegistryError::InvalidManifest {
            path: path.display().to_string(),
            message: "name must not be empty".to_string(),
        });
    }
    if manifest.version.trim().is_empty() {
        return Err(PluginRegistryError::InvalidManifest {
            path: path.display().to_string(),
            message: "version must not be empty".to_string(),
        });
    }
    if manifest.protocol_version == 0 {
        return Err(PluginRegistryError::InvalidManifest {
            path: path.display().to_string(),
            message: "protocol_version must be greater than zero".to_string(),
        });
    }
    if manifest.protocol_version != 1 {
        return Err(PluginRegistryError::InvalidManifest {
            path: path.display().to_string(),
            message: format!(
                "protocol_version `{}` is unsupported; expected 1",
                manifest.protocol_version
            ),
        });
    }
    if manifest.exec.command.trim().is_empty() {
        return Err(PluginRegistryError::InvalidManifest {
            path: path.display().to_string(),
            message: "exec.command must not be empty".to_string(),
        });
    }
    if let Some(ingress) = &manifest.ingress {
        for endpoint in &ingress.endpoints {
            if endpoint.path.trim().is_empty() {
                return Err(PluginRegistryError::InvalidManifest {
                    path: path.display().to_string(),
                    message: "ingress endpoint path must not be empty".to_string(),
                });
            }
            if !endpoint.path.starts_with('/') {
                return Err(PluginRegistryError::InvalidManifest {
                    path: path.display().to_string(),
                    message: format!(
                        "ingress endpoint path `{}` must start with /",
                        endpoint.path
                    ),
                });
            }
        }
    }

    Ok(())
}

impl OnDiskManifestKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Channel => "channel",
            Self::Courier => "courier",
            Self::Connector => "connector",
        }
    }
}

fn extract_channel_ingress(channel: &OnDiskChannelCapability) -> Option<ChannelPluginIngress> {
    let mut endpoints = channel
        .ingress
        .as_ref()
        .map(|ingress| {
            ingress
                .endpoints
                .iter()
                .map(|endpoint| ChannelIngressEndpoint {
                    path: endpoint.path.clone(),
                    methods: endpoint
                        .methods
                        .iter()
                        .map(|method| method.to_ascii_uppercase())
                        .collect(),
                    host_managed: endpoint.host_managed,
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if endpoints.is_empty() {
        endpoints = channel
            .allowed_paths
            .iter()
            .map(|path| ChannelIngressEndpoint {
                path: path.clone(),
                methods: vec!["POST".to_string()],
                host_managed: true,
            })
            .collect();
    }

    if endpoints.is_empty() {
        return None;
    }

    let trust = channel
        .ingress
        .as_ref()
        .and_then(|ingress| ingress.trust.as_ref())
        .map(|trust| ChannelIngressTrust {
            mode: trust.mode.clone(),
            header_name: trust.header_name.clone(),
            secret_name: trust.secret_name.clone(),
            host_managed: trust.host_managed,
        });

    Some(ChannelPluginIngress { endpoints, trust })
}

fn plugin_matches_ingress(plugin: &ChannelPluginManifest, method: &str, path: &str) -> bool {
    plugin
        .ingress
        .as_ref()
        .map(|ingress| {
            ingress.endpoints.iter().any(|endpoint| {
                endpoint.path == path
                    && (endpoint.methods.is_empty()
                        || endpoint
                            .methods
                            .iter()
                            .any(|allowed| allowed.eq_ignore_ascii_case(method)))
            })
        })
        .unwrap_or(false)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn install_channel_plugin_round_trips_registry() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let script_path = dir.path().join("channel-demo.sh");
        fs::write(&script_path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();
        let manifest_path = dir.path().join("channel-plugin.json");
        let registry_path = dir.path().join("channels.json");
        // Uses the real on-disk format: "protocol" not "transport",
        // "entrypoint" not "exec", platform inside capabilities.channel.
        fs::write(
            &manifest_path,
            format!(
                r#"{{
"name": "telegram-bridge",
"version": "0.1.0",
"protocol": "jsonl",
"protocol_version": 1,
"description": "Demo channel plugin for Telegram",
"entrypoint": {{
    "command": "{}",
    "args": ["--stdio"]
}},
"capabilities": {{
    "channel": {{
        "platform": "telegram",
        "allowed_paths": ["/telegram/updates"],
        "ingress": {{
            "endpoints": [
                {{
                    "path": "/telegram/updates",
                    "methods": ["POST"],
                    "host_managed": true
                }}
            ],
            "trust": {{
                "mode": "shared_secret_header",
                "header_name": "X-Telegram-Bot-Api-Secret-Token",
                "secret_name": "TELEGRAM_WEBHOOK_SECRET",
                "host_managed": true
            }}
        }}
    }}
}}
}}"#,
                script_path.display()
            ),
        )
        .unwrap();

        let installed = install_channel_plugin(&manifest_path, Some(&registry_path)).unwrap();
        assert_eq!(installed.name, "telegram-bridge");
        assert!(installed.installed_sha256.is_some());
        assert_eq!(installed.platform.as_deref(), Some("telegram"));

        let registry = load_channel_registry(Some(&registry_path)).unwrap();
        assert_eq!(registry.plugins.len(), 1);
        assert_eq!(registry.plugins[0].name, "telegram-bridge");
        assert_eq!(registry.plugins[0].transport, PluginTransport::Jsonl);
        assert_eq!(registry.plugins[0].platform.as_deref(), Some("telegram"));
        let ingress = registry.plugins[0]
            .ingress
            .as_ref()
            .expect("ingress metadata should be preserved");
        assert_eq!(ingress.endpoints.len(), 1);
        assert_eq!(ingress.endpoints[0].path, "/telegram/updates");
        assert_eq!(ingress.endpoints[0].methods, vec!["POST".to_string()]);
        assert_eq!(
            ingress
                .trust
                .as_ref()
                .and_then(|trust| trust.header_name.as_deref()),
            Some("X-Telegram-Bot-Api-Secret-Token")
        );
        assert!(registry.plugins[0].installed_sha256.is_some());
    }

    #[test]
    fn resolve_channel_plugin_finds_installed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let script_path = dir.path().join("channel-slack.sh");
        fs::write(&script_path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();
        let manifest_path = dir.path().join("channel-plugin.json");
        let registry_path = dir.path().join("channels.json");
        fs::write(
            &manifest_path,
            format!(
                r#"{{
"name": "slack-bridge",
"version": "0.1.0",
"protocol": "jsonl",
"protocol_version": 1,
"description": "Slack channel plugin",
"entrypoint": {{
    "command": "{}",
    "args": []
}},
"capabilities": {{
    "channel": {{
        "platform": "slack",
        "allowed_paths": ["/slack/events"]
    }}
}}
}}"#,
                script_path.display()
            ),
        )
        .unwrap();

        install_channel_plugin(&manifest_path, Some(&registry_path)).unwrap();
        let resolved = resolve_channel_plugin("slack-bridge", Some(&registry_path)).unwrap();
        assert_eq!(resolved.name, "slack-bridge");
        assert_eq!(resolved.platform.as_deref(), Some("slack"));
        assert_eq!(
            resolved
                .ingress
                .as_ref()
                .and_then(|ingress| ingress.endpoints.first())
                .map(|endpoint| endpoint.path.as_str()),
            Some("/slack/events")
        );
    }

    #[test]
    fn resolve_channel_plugin_for_ingress_matches_installed_route() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let script_path = dir.path().join("channel-webhook.sh");
        fs::write(&script_path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();
        let manifest_path = dir.path().join("channel-plugin.json");
        let registry_path = dir.path().join("channels.json");
        fs::write(
            &manifest_path,
            format!(
                r#"{{
"kind": "channel",
"name": "webhook-demo",
"version": "0.1.0",
"protocol": "jsonl",
"protocol_version": 1,
"entrypoint": {{
    "command": "{}",
    "args": []
}},
"capabilities": {{
    "channel": {{
        "platform": "webhook",
        "allowed_paths": ["/webhook/inbound"]
    }}
}}
}}"#,
                script_path.display()
            ),
        )
        .unwrap();

        install_channel_plugin(&manifest_path, Some(&registry_path)).unwrap();
        let resolved =
            resolve_channel_plugin_for_ingress("POST", "/webhook/inbound", Some(&registry_path))
                .unwrap();
        assert_eq!(resolved.name, "webhook-demo");
    }

    #[test]
    fn resolve_channel_plugin_for_ingress_rejects_ambiguous_routes() {
        let registry = ChannelPluginRegistry {
            plugins: vec![
                ChannelPluginManifest {
                    name: "one".to_string(),
                    version: "0.1.0".to_string(),
                    protocol_version: 1,
                    transport: PluginTransport::Jsonl,
                    description: None,
                    exec: ChannelPluginExec {
                        command: "/usr/bin/true".to_string(),
                        args: vec![],
                    },
                    platform: Some("telegram".to_string()),
                    ingress: Some(ChannelPluginIngress {
                        endpoints: vec![ChannelIngressEndpoint {
                            path: "/shared".to_string(),
                            methods: vec!["POST".to_string()],
                            host_managed: true,
                        }],
                        trust: None,
                    }),
                    installed_sha256: None,
                },
                ChannelPluginManifest {
                    name: "two".to_string(),
                    version: "0.1.0".to_string(),
                    protocol_version: 1,
                    transport: PluginTransport::Jsonl,
                    description: None,
                    exec: ChannelPluginExec {
                        command: "/usr/bin/true".to_string(),
                        args: vec![],
                    },
                    platform: Some("slack".to_string()),
                    ingress: Some(ChannelPluginIngress {
                        endpoints: vec![ChannelIngressEndpoint {
                            path: "/shared".to_string(),
                            methods: vec!["POST".to_string()],
                            host_managed: true,
                        }],
                        trust: None,
                    }),
                    installed_sha256: None,
                },
            ],
        };
        let dir = tempdir().unwrap();
        let registry_path = dir.path().join("channels.json");
        fs::write(
            &registry_path,
            serde_json::to_string_pretty(&registry).unwrap(),
        )
        .unwrap();

        let error = resolve_channel_plugin_for_ingress("POST", "/shared", Some(&registry_path))
            .unwrap_err();
        assert!(matches!(
            error,
            PluginRegistryError::AmbiguousChannelIngressMatch { ref names, .. }
                if names == &vec!["one".to_string(), "two".to_string()]
        ));
    }

    #[test]
    fn resolve_channel_plugin_rejects_unknown() {
        let dir = tempdir().unwrap();
        let registry_path = dir.path().join("channels.json");
        let error = resolve_channel_plugin("nonexistent", Some(&registry_path)).unwrap_err();
        assert!(matches!(
            error,
            PluginRegistryError::UnknownChannel { name } if name == "nonexistent"
        ));
    }

    #[test]
    fn validate_rejects_empty_name() {
        let dir = tempdir().unwrap();
        let manifest_path = dir.path().join("bad.json");
        let manifest = ChannelPluginManifest {
            name: "".to_string(),
            version: "0.1.0".to_string(),
            protocol_version: 1,
            transport: PluginTransport::Jsonl,
            description: None,
            exec: ChannelPluginExec {
                command: "/usr/bin/true".to_string(),
                args: vec![],
            },
            platform: None,
            ingress: None,
            installed_sha256: None,
        };
        let error = validate_channel_plugin_manifest(&manifest_path, &manifest).unwrap_err();
        assert!(matches!(
            error,
            PluginRegistryError::InvalidManifest { message, .. } if message.contains("name")
        ));
    }

    #[test]
    fn validate_rejects_bad_protocol_version() {
        let dir = tempdir().unwrap();
        let manifest_path = dir.path().join("bad.json");
        let manifest = ChannelPluginManifest {
            name: "test".to_string(),
            version: "0.1.0".to_string(),
            protocol_version: 99,
            transport: PluginTransport::Jsonl,
            description: None,
            exec: ChannelPluginExec {
                command: "/usr/bin/true".to_string(),
                args: vec![],
            },
            platform: None,
            ingress: None,
            installed_sha256: None,
        };
        let error = validate_channel_plugin_manifest(&manifest_path, &manifest).unwrap_err();
        assert!(matches!(
            error,
            PluginRegistryError::InvalidManifest { message, .. } if message.contains("protocol_version")
        ));
    }

    #[test]
    fn install_channel_plugin_accepts_exec_alias() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let script_path = dir.path().join("channel-exec-alias.sh");
        fs::write(&script_path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();
        let manifest_path = dir.path().join("channel-plugin.json");
        let registry_path = dir.path().join("channels.json");
        fs::write(
            &manifest_path,
            format!(
                r#"{{
"kind": "channel",
"name": "exec-alias-demo",
"version": "0.1.0",
"protocol": "jsonl",
"protocol_version": 1,
"exec": {{
    "command": "{}",
    "args": ["--stdio"]
}},
"capabilities": {{
    "channel": {{
        "platform": "telegram"
    }}
}}
}}"#,
                script_path.display()
            ),
        )
        .unwrap();

        let installed = install_channel_plugin(&manifest_path, Some(&registry_path)).unwrap();
        assert_eq!(installed.name, "exec-alias-demo");
        assert_eq!(installed.platform.as_deref(), Some("telegram"));
        assert_eq!(installed.transport, PluginTransport::Jsonl);
    }

    #[test]
    fn install_channel_plugin_rejects_non_channel_kind() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let script_path = dir.path().join("dispatch-courier-shape.sh");
        fs::write(&script_path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();
        let manifest_path = dir.path().join("channel-plugin.json");
        let registry_path = dir.path().join("channels.json");
        fs::write(
            &manifest_path,
            format!(
                r#"{{
"kind": "courier",
"name": "wrong-kind",
"version": "0.1.0",
"protocol": "jsonl",
"protocol_version": 1,
"entrypoint": {{
    "command": "{}",
    "args": []
}}
}}"#,
                script_path.display()
            ),
        )
        .unwrap();

        let error = install_channel_plugin(&manifest_path, Some(&registry_path)).unwrap_err();
        assert!(matches!(
            error,
            PluginRegistryError::InvalidManifest { message, .. } if message.contains("kind must be `channel`")
        ));
    }

    #[test]
    fn call_channel_plugin_reads_capabilities_response() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let script_path = dir.path().join("channel-capabilities.sh");
        fs::write(
            &script_path,
            r#"#!/bin/sh
read line
printf '%s\n' '{"kind":"capabilities","capabilities":{"plugin_id":"telegram","platform":"telegram","ingress_modes":["webhook"],"outbound_message_types":["text"],"threading_model":"chat_or_topic","attachment_support":false,"reply_verification_support":true,"account_scoped_config":true,"accepts_push":true,"accepts_status_frames":true}}'
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();

        let manifest = ChannelPluginManifest {
            name: "telegram-demo".to_string(),
            version: "0.1.0".to_string(),
            protocol_version: 1,
            transport: PluginTransport::Jsonl,
            description: None,
            exec: ChannelPluginExec {
                command: script_path.display().to_string(),
                args: vec![],
            },
            platform: Some("telegram".to_string()),
            ingress: None,
            installed_sha256: None,
        };

        let response = call_channel_plugin(&manifest, ChannelPluginRequest::Capabilities).unwrap();
        let ChannelPluginResponse::Capabilities { capabilities } = response else {
            panic!("unexpected response variant");
        };
        assert_eq!(capabilities.plugin_id, "telegram");
        assert!(capabilities.accepts_status_frames);
    }

    #[test]
    fn call_channel_plugin_rejects_invalid_json() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let script_path = dir.path().join("channel-invalid-json.sh");
        fs::write(
            &script_path,
            r#"#!/bin/sh
read line
printf '%s\n' 'not-json'
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();

        let manifest = ChannelPluginManifest {
            name: "invalid-json-demo".to_string(),
            version: "0.1.0".to_string(),
            protocol_version: 1,
            transport: PluginTransport::Jsonl,
            description: None,
            exec: ChannelPluginExec {
                command: script_path.display().to_string(),
                args: vec![],
            },
            platform: None,
            ingress: None,
            installed_sha256: None,
        };

        let error = call_channel_plugin(&manifest, ChannelPluginRequest::Capabilities).unwrap_err();
        assert!(matches!(
            error,
            ChannelPluginCallError::PluginProtocol { message, .. } if message.contains("invalid channel plugin JSON")
        ));
    }

    #[test]
    fn call_channel_plugin_surfaces_nonzero_exit_without_response() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let script_path = dir.path().join("channel-no-response.sh");
        fs::write(
            &script_path,
            r#"#!/bin/sh
read line
printf '%s\n' 'plugin failed before replying' >&2
exit 7
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();

        let manifest = ChannelPluginManifest {
            name: "no-response-demo".to_string(),
            version: "0.1.0".to_string(),
            protocol_version: 1,
            transport: PluginTransport::Jsonl,
            description: None,
            exec: ChannelPluginExec {
                command: script_path.display().to_string(),
                args: vec![],
            },
            platform: None,
            ingress: None,
            installed_sha256: None,
        };

        let error = call_channel_plugin(&manifest, ChannelPluginRequest::Capabilities).unwrap_err();
        assert!(matches!(
            error,
            ChannelPluginCallError::PluginExit {
                status, ref stderr, ..
            } if status == 7 && stderr.contains("plugin failed before replying")
        ));
    }
}
