use crate::{
    channel_plugin_protocol::{
        AttachmentSource, CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelEventAcceptance,
        ChannelEventRequest, ChannelHeartbeat, ChannelPluginRequest, ChannelPluginRequestEnvelope,
        ChannelPluginResponse, InboundEventEnvelope, OutboundMessageEnvelope, PluginMessage,
        PluginNotificationEnvelope, PluginRequestId, event_acceptance_to_jsonrpc,
        parse_jsonrpc_message, parse_tagged_channel_reply, request_to_jsonrpc,
    },
    courier::CourierEvent,
    plugins::{PluginRegistryError, PluginTransport, hash_file_sha256, resolve_plugin_exec_path},
};
use dispatch_process::{kill_child_and_wait, wait_for_child_timeout};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    io::{BufReader, Read as _, Write as _},
    path::{Path, PathBuf},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};
use thiserror::Error;

const CHANNEL_REGISTRY_RELATIVE_PATH: &str = ".config/dispatch/channels.json";
const CHANNEL_PLUGIN_CALL_TIMEOUT: Duration = Duration::from_secs(30);
const CHANNEL_PLUGIN_POLL_INTERVAL: Duration = Duration::from_millis(25);
const MAX_PENDING_CHANNEL_FRAMES: usize = 32;

fn user_home_dir() -> Result<PathBuf, PluginRegistryError> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or(PluginRegistryError::MissingHome)
}

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
    #[serde(default)]
    pub attachment_sources: Vec<AttachmentSource>,
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
    Provider,
    Database,
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
    delivery: Option<OnDiskChannelDelivery>,
    #[serde(default)]
    ingress: Option<OnDiskChannelIngress>,
}

#[derive(Debug, Clone, Deserialize)]
struct OnDiskChannelDelivery {
    #[serde(default)]
    attachment_sources: Vec<AttachmentSource>,
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
    #[error("durable sink refused channel plugin `{channel}` batch {batch_digest}: {source}")]
    DurableSink {
        channel: String,
        batch_digest: String,
        #[source]
        source: std::io::Error,
    },
    #[error("channel plugin `{channel}` exited with status {status}: {stderr}")]
    PluginExit {
        channel: String,
        status: i32,
        stderr: String,
    },
    #[error("channel plugin `{channel}` timed out after {timeout_ms}ms: {stderr}")]
    PluginTimedOut {
        channel: String,
        timeout_ms: u128,
        stderr: String,
    },
}

pub struct PersistentChannelPluginProcess {
    child: Child,
    stdin: ChildStdin,
    stderr: Arc<Mutex<String>>,
    stderr_reader: thread::JoinHandle<()>,
    outputs: mpsc::Receiver<Result<PluginMessage, ChannelPluginCallError>>,
    pending_ingress: VecDeque<ChannelIngressMessage>,
    next_request_id: i64,
    unusable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelIngressMessage {
    Events {
        id: PluginRequestId,
        request: ChannelEventRequest,
    },
    Heartbeat(PluginNotificationEnvelope<ChannelHeartbeat>),
}

/// Host storage that accepts complete channel batches.
///
/// `accept` returns the exact receipt only after the complete original batch
/// is durable. Returning an error leaves the batch unacknowledged.
/// Implementations atomically replace a binding's head only for a successor
/// proving its exact receipt, or return the receipt for an exact head replay.
/// The host assigns the checked next sequence after verifying that head and
/// commits it with the original before returning the receipt.
pub trait DurableChannelEventSink {
    fn accept(
        &mut self,
        request: &ChannelEventRequest,
    ) -> Result<ChannelEventAcceptance, std::io::Error>;
}

/// Store one received batch through `sink`, then write its exact receipt.
pub fn accept_persistent_channel_event<S: DurableChannelEventSink + ?Sized>(
    process: &mut PersistentChannelPluginProcess,
    manifest: &ChannelPluginManifest,
    request: &ChannelEventRequest,
    sink: &mut S,
) -> Result<(), ChannelPluginCallError> {
    ensure_usable_channel_process(process, &manifest.name)?;
    let result =
        write_durable_channel_acceptance(&mut process.stdin, &manifest.name, request, sink);
    if result.is_err() {
        mark_channel_process_unusable(process);
    }
    result
}

fn write_durable_channel_acceptance<W: std::io::Write, S: DurableChannelEventSink + ?Sized>(
    writer: &mut W,
    channel: &str,
    request: &ChannelEventRequest,
    sink: &mut S,
) -> Result<(), ChannelPluginCallError> {
    let protocol_error = |message: String| ChannelPluginCallError::PluginProtocol {
        channel: channel.to_owned(),
        message,
    };
    request
        .validate()
        .map_err(|error| protocol_error(error.to_string()))?;
    let receipt = sink
        .accept(request)
        .map_err(|source| ChannelPluginCallError::DurableSink {
            channel: channel.to_owned(),
            batch_digest: request.batch_digest.clone(),
            source,
        })?;
    if !receipt.matches(request) {
        return Err(protocol_error(
            "durable sink returned another channel batch receipt".into(),
        ));
    }
    let line = event_acceptance_to_jsonrpc(&request.request_id(), &receipt)
        .map_err(|error| protocol_error(error.to_string()))?;
    writer
        .write_all(line.as_bytes())
        .and_then(|()| writer.write_all(b"\n"))
        .and_then(|()| writer.flush())
        .map_err(|source| ChannelPluginCallError::WritePluginRequest {
            channel: channel.to_owned(),
            source,
        })
}

fn requires_durable_batch(request: &ChannelPluginRequest) -> bool {
    matches!(
        request,
        ChannelPluginRequest::PollIngress { .. } | ChannelPluginRequest::IngressEvent { .. }
    )
}

fn ensure_usable_channel_process(
    process: &PersistentChannelPluginProcess,
    channel: &str,
) -> Result<(), ChannelPluginCallError> {
    if process.unusable {
        return Err(ChannelPluginCallError::PluginProtocol {
            channel: channel.to_owned(),
            message: "channel plugin process was stopped after an earlier exchange failure".into(),
        });
    }
    Ok(())
}

// After a failed exchange the stream position is unknown, so the process
// cannot be reused without risking a stale response or receipt.
fn mark_channel_process_unusable(process: &mut PersistentChannelPluginProcess) {
    process.unusable = true;
    kill_child_and_wait(&mut process.child);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelIngressTrustFailure {
    pub status_code: u16,
    pub status_text: &'static str,
    pub message: String,
}

pub fn default_channel_registry_path() -> Result<PathBuf, PluginRegistryError> {
    Ok(user_home_dir()?.join(CHANNEL_REGISTRY_RELATIVE_PATH))
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
    let attachment_sources = on_disk
        .capabilities
        .as_ref()
        .and_then(|c| c.channel.as_ref())
        .and_then(|channel| channel.delivery.as_ref())
        .map(|delivery| delivery.attachment_sources.clone())
        .unwrap_or_default();

    let mut manifest = ChannelPluginManifest {
        name: on_disk.name,
        version: on_disk.version,
        protocol_version: on_disk.protocol_version,
        transport: on_disk.protocol,
        description: on_disk.description,
        exec: on_disk.entrypoint,
        platform,
        attachment_sources,
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
    call_channel_plugin_with_timeout(manifest, request, CHANNEL_PLUGIN_CALL_TIMEOUT)
}

impl PersistentChannelPluginProcess {
    fn write_request(
        &mut self,
        manifest: &ChannelPluginManifest,
        request: ChannelPluginRequest,
    ) -> Result<PluginRequestId, ChannelPluginCallError> {
        let request_id = PluginRequestId::Integer(self.next_request_id);
        self.next_request_id += 1;
        write_channel_plugin_request_to(
            &mut self.stdin,
            manifest.name.as_str(),
            request_id.clone(),
            request,
        )?;
        Ok(request_id)
    }
}

pub fn spawn_persistent_channel_plugin(
    manifest: &ChannelPluginManifest,
) -> Result<PersistentChannelPluginProcess, ChannelPluginCallError> {
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
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| ChannelPluginCallError::PluginProtocol {
            channel: manifest.name.clone(),
            message: "channel plugin stdin was not captured".to_string(),
        })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ChannelPluginCallError::PluginProtocol {
            channel: manifest.name.clone(),
            message: "channel plugin stdout was not captured".to_string(),
        })?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ChannelPluginCallError::PluginProtocol {
            channel: manifest.name.clone(),
            message: "channel plugin stderr was not captured".to_string(),
        })?;
    let (stderr, stderr_reader) = spawn_channel_plugin_stderr_reader(stderr);

    Ok(PersistentChannelPluginProcess {
        child,
        stdin,
        stderr,
        stderr_reader,
        outputs: spawn_channel_plugin_output_reader(stdout, manifest.name.clone()),
        pending_ingress: VecDeque::new(),
        next_request_id: 1,
        unusable: false,
    })
}

/// Send one control request on a persistent session without accepting batches.
///
/// Batches that arrive during the call are queued without a receipt. Use
/// [`call_persistent_channel_plugin_with_sink`] when the plugin may wait for a
/// batch receipt before it answers the control request.
pub fn call_persistent_channel_plugin(
    process: &mut PersistentChannelPluginProcess,
    manifest: &ChannelPluginManifest,
    request: ChannelPluginRequest,
    timeout: Duration,
) -> Result<ChannelPluginResponse, ChannelPluginCallError> {
    call_persistent_channel_plugin_inner(process, manifest, request, timeout, None)
}

/// Send one control request and store every batch received meanwhile through
/// `sink` before its receipt is written.
pub fn call_persistent_channel_plugin_with_sink(
    process: &mut PersistentChannelPluginProcess,
    manifest: &ChannelPluginManifest,
    request: ChannelPluginRequest,
    timeout: Duration,
    sink: &mut dyn DurableChannelEventSink,
) -> Result<ChannelPluginResponse, ChannelPluginCallError> {
    call_persistent_channel_plugin_inner(process, manifest, request, timeout, Some(sink))
}

fn call_persistent_channel_plugin_inner(
    process: &mut PersistentChannelPluginProcess,
    manifest: &ChannelPluginManifest,
    request: ChannelPluginRequest,
    timeout: Duration,
    mut sink: Option<&mut dyn DurableChannelEventSink>,
) -> Result<ChannelPluginResponse, ChannelPluginCallError> {
    ensure_usable_channel_process(process, &manifest.name)?;
    let requires_batch = requires_durable_batch(&request);
    if requires_batch && sink.is_none() {
        return Err(ChannelPluginCallError::PluginProtocol {
            channel: manifest.name.clone(),
            message: "channel ingress requires an explicit durable sink".into(),
        });
    }
    let deadline = Instant::now() + timeout;
    let result = (|| -> Result<ChannelPluginResponse, ChannelPluginCallError> {
        let request_id = process.write_request(manifest, request)?;
        let mut accepted_batches = 0usize;
        while let Some(message) =
            recv_channel_plugin_message(process, manifest.name.as_str(), Some(deadline))?
        {
            match message {
                PluginMessage::EventRequest { id, request } => match sink.as_deref_mut() {
                    Some(sink) => {
                        ensure_channel_acceptance_page(&manifest.name, accepted_batches)?;
                        write_durable_channel_acceptance(
                            &mut process.stdin,
                            &manifest.name,
                            &request,
                            sink,
                        )?;
                        accepted_batches += 1;
                    }
                    None => queue_channel_ingress(
                        process,
                        &manifest.name,
                        ChannelIngressMessage::Events { id, request },
                    )?,
                },
                PluginMessage::Heartbeat(_) => {}
                PluginMessage::Response { id, response } => {
                    ensure_matching_channel_request_id(manifest.name.as_str(), &request_id, &id)?;
                    ensure_durable_batch_received(
                        &manifest.name,
                        requires_batch,
                        accepted_batches,
                        &response,
                    )?;
                    return Ok(response);
                }
            }
        }

        // Kill before taking the stderr snapshot so a still-running child
        // cannot keep the timeout path waiting.
        kill_child_and_wait(&mut process.child);
        Err(ChannelPluginCallError::PluginTimedOut {
            channel: manifest.name.clone(),
            timeout_ms: timeout.as_millis(),
            stderr: collect_persistent_channel_stderr(process, manifest.name.as_str())?,
        })
    })();
    if result.is_err() {
        mark_channel_process_unusable(process);
    }
    result
}

fn ensure_channel_acceptance_page(
    channel: &str,
    accepted_batches: usize,
) -> Result<(), ChannelPluginCallError> {
    if accepted_batches >= MAX_PENDING_CHANNEL_FRAMES {
        return Err(ChannelPluginCallError::PluginProtocol {
            channel: channel.to_owned(),
            message: "channel ingress exceeded its bounded acceptance page".into(),
        });
    }
    Ok(())
}

fn ensure_durable_batch_received(
    channel: &str,
    requires_batch: bool,
    accepted_batches: usize,
    response: &ChannelPluginResponse,
) -> Result<(), ChannelPluginCallError> {
    if requires_batch
        && accepted_batches == 0
        && !matches!(response, ChannelPluginResponse::Error { .. })
    {
        return Err(ChannelPluginCallError::PluginProtocol {
            channel: channel.to_owned(),
            message: "ingress control response arrived without a durable batch acceptance".into(),
        });
    }
    Ok(())
}

pub fn drain_pending_channel_ingress(
    process: &mut PersistentChannelPluginProcess,
) -> Vec<ChannelIngressMessage> {
    process.pending_ingress.drain(..).collect()
}

pub fn recv_persistent_channel_ingress(
    process: &mut PersistentChannelPluginProcess,
    manifest: &ChannelPluginManifest,
    timeout: Option<Duration>,
) -> Result<Option<ChannelIngressMessage>, ChannelPluginCallError> {
    if let Some(message) = process.pending_ingress.pop_front() {
        return Ok(Some(message));
    }
    ensure_usable_channel_process(process, &manifest.name)?;

    let deadline = timeout.map(|duration| Instant::now() + duration);
    let result = match recv_channel_plugin_message(process, manifest.name.as_str(), deadline) {
        Ok(None) => Ok(None),
        Ok(Some(PluginMessage::EventRequest { id, request })) => {
            Ok(Some(ChannelIngressMessage::Events { id, request }))
        }
        Ok(Some(PluginMessage::Heartbeat(heartbeat))) => {
            Ok(Some(ChannelIngressMessage::Heartbeat(heartbeat)))
        }
        Ok(Some(PluginMessage::Response { response, .. })) => {
            Err(ChannelPluginCallError::PluginProtocol {
                channel: manifest.name.clone(),
                message: format!(
                    "channel plugin returned unexpected response while waiting for ingress events: {}",
                    describe_channel_plugin_response(&response)
                ),
            })
        }
        Err(error) => Err(error),
    };
    if result.is_err() {
        mark_channel_process_unusable(process);
    }
    result
}

fn queue_channel_ingress(
    process: &mut PersistentChannelPluginProcess,
    channel: &str,
    message: ChannelIngressMessage,
) -> Result<(), ChannelPluginCallError> {
    if process.pending_ingress.len() >= MAX_PENDING_CHANNEL_FRAMES {
        return Err(ChannelPluginCallError::PluginProtocol {
            channel: channel.into(),
            message: "pending channel ingress frame bound reached without durable acceptance"
                .into(),
        });
    }
    process.pending_ingress.push_back(message);
    Ok(())
}

pub fn shutdown_persistent_channel_plugin(
    process: &mut PersistentChannelPluginProcess,
    manifest: &ChannelPluginManifest,
) -> Result<(), ChannelPluginCallError> {
    if !process.unusable
        && process
            .write_request(manifest, ChannelPluginRequest::Shutdown)
            .is_ok()
    {
        let _ = recv_channel_plugin_message(
            process,
            manifest.name.as_str(),
            Some(Instant::now() + Duration::from_millis(200)),
        );
    }
    let _ = process.stdin.flush();
    kill_child_and_wait(&mut process.child);
    let _ = collect_persistent_channel_stderr(process, manifest.name.as_str())?;
    Ok(())
}

fn spawn_channel_plugin_output_reader(
    stdout: ChildStdout,
    channel: String,
) -> mpsc::Receiver<Result<PluginMessage, ChannelPluginCallError>> {
    let (tx, rx) = mpsc::sync_channel(MAX_PENDING_CHANNEL_FRAMES);
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_channel_plugin_message(&mut reader, channel.as_str()) {
                // Dropping the sender reports the closed stream.
                Ok(None) => break,
                // Liveness frames are dropped when the host is busy so they
                // never block plugin stdout behind host work.
                Ok(Some(PluginMessage::Heartbeat(heartbeat))) => {
                    if let Err(mpsc::TrySendError::Disconnected(_)) =
                        tx.try_send(Ok(PluginMessage::Heartbeat(heartbeat)))
                    {
                        break;
                    }
                }
                Ok(Some(message)) => {
                    if tx.send(Ok(message)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = tx.send(Err(error));
                    break;
                }
            }
        }
    });
    rx
}

fn read_channel_plugin_message<R: std::io::BufRead>(
    reader: &mut R,
    channel: &str,
) -> Result<Option<PluginMessage>, ChannelPluginCallError> {
    let mut line = String::new();
    // Ordinary responses retain their existing size contract. Batch bounds
    // are checked by the protocol parser after reading the complete line.
    let bytes = reader.read_line(&mut line).map_err(|source| {
        ChannelPluginCallError::ReadPluginResponse {
            channel: channel.to_string(),
            source,
        }
    })?;
    if bytes == 0 {
        return Ok(None);
    }
    let frame = line.strip_suffix('\n').unwrap_or(&line);
    parse_jsonrpc_message(frame.trim_end())
        .map(Some)
        .map_err(|message| ChannelPluginCallError::PluginProtocol {
            channel: channel.to_string(),
            message: message.to_string(),
        })
}

// Short-lived and consumed immediately, so the payload is not boxed.
#[allow(clippy::large_enum_variant)]
enum ChannelPluginFrame {
    Message(PluginMessage),
    TimedOut,
    Closed,
}

fn recv_channel_plugin_frame(
    process: &mut PersistentChannelPluginProcess,
    deadline: Option<Instant>,
) -> Result<ChannelPluginFrame, ChannelPluginCallError> {
    let received = match deadline {
        Some(deadline) => {
            match process
                .outputs
                .recv_timeout(remaining_channel_plugin_timeout(deadline))
            {
                Ok(result) => Some(result),
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(ChannelPluginFrame::TimedOut),
                Err(mpsc::RecvTimeoutError::Disconnected) => None,
            }
        }
        None => process.outputs.recv().ok(),
    };
    match received {
        Some(result) => result.map(ChannelPluginFrame::Message),
        None => Ok(ChannelPluginFrame::Closed),
    }
}

fn recv_channel_plugin_message(
    process: &mut PersistentChannelPluginProcess,
    channel: &str,
    deadline: Option<Instant>,
) -> Result<Option<PluginMessage>, ChannelPluginCallError> {
    match recv_channel_plugin_frame(process, deadline)? {
        ChannelPluginFrame::Message(message) => Ok(Some(message)),
        ChannelPluginFrame::TimedOut => Ok(None),
        ChannelPluginFrame::Closed => Err(ChannelPluginCallError::PluginProtocol {
            channel: channel.to_string(),
            message: "channel plugin produced no response".to_string(),
        }),
    }
}

fn collect_persistent_channel_stderr(
    process: &mut PersistentChannelPluginProcess,
    _channel: &str,
) -> Result<String, ChannelPluginCallError> {
    // The stderr pipe is drained asynchronously by a dedicated reader thread
    // (see `spawn_channel_plugin_stderr_reader`). We snapshot what has been
    // captured so far without blocking, which keeps timeouts prompt even if a
    // descendant process is still holding the pipe's write end open.
    Ok(snapshot_channel_stderr(&process.stderr))
}

fn snapshot_channel_stderr(stderr: &Mutex<String>) -> String {
    let stderr = match stderr.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    stderr.trim().to_string()
}

// Give the stderr reader a bounded window to observe pipe closure after the
// child exits; a descendant can keep the pipe open indefinitely.
fn settled_channel_stderr(
    stderr: &Mutex<String>,
    stderr_reader: &thread::JoinHandle<()>,
    window: Duration,
) -> String {
    let until = Instant::now() + window;
    while !stderr_reader.is_finished() && Instant::now() < until {
        thread::sleep(Duration::from_millis(5));
    }
    snapshot_channel_stderr(stderr)
}

fn spawn_channel_plugin_stderr_reader(
    mut stderr: ChildStderr,
) -> (Arc<Mutex<String>>, thread::JoinHandle<()>) {
    let buffer = Arc::new(Mutex::new(String::new()));
    let writer = Arc::clone(&buffer);
    let reader = thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    if let Ok(mut guard) = writer.lock() {
                        guard.push_str(&String::from_utf8_lossy(&chunk[..read]));
                    }
                }
                Err(_) => break,
            }
        }
    });
    (buffer, reader)
}

fn write_channel_plugin_request_to<W: std::io::Write>(
    mut writer: W,
    channel: &str,
    request_id: PluginRequestId,
    request: ChannelPluginRequest,
) -> Result<(), ChannelPluginCallError> {
    let rpc_request = request_to_jsonrpc(
        request_id,
        &ChannelPluginRequestEnvelope {
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            request,
        },
    )
    .map_err(|message| ChannelPluginCallError::PluginProtocol {
        channel: channel.to_string(),
        message: message.to_string(),
    })?;
    serde_json::to_writer(&mut writer, &rpc_request).map_err(|source| {
        ChannelPluginCallError::PluginProtocol {
            channel: channel.to_string(),
            message: format!("failed to serialize channel plugin request: {source}"),
        }
    })?;
    writer
        .write_all(b"\n")
        .map_err(|source| ChannelPluginCallError::WritePluginRequest {
            channel: channel.to_string(),
            source,
        })?;
    writer
        .flush()
        .map_err(|source| ChannelPluginCallError::WritePluginRequest {
            channel: channel.to_string(),
            source,
        })?;
    Ok(())
}

fn ensure_matching_channel_request_id(
    channel: &str,
    expected_id: &PluginRequestId,
    actual_id: &PluginRequestId,
) -> Result<(), ChannelPluginCallError> {
    if actual_id == expected_id {
        return Ok(());
    }

    Err(ChannelPluginCallError::PluginProtocol {
        channel: channel.to_string(),
        message: format!(
            "channel plugin response id `{}` did not match request id `{}`",
            format_channel_request_id(actual_id),
            format_channel_request_id(expected_id)
        ),
    })
}

fn format_channel_request_id(id: &PluginRequestId) -> String {
    match id {
        PluginRequestId::String(value) => format!("\"{value}\""),
        PluginRequestId::Integer(value) => value.to_string(),
    }
}

pub fn call_channel_plugin_with_timeout(
    manifest: &ChannelPluginManifest,
    request: ChannelPluginRequest,
    timeout: Duration,
) -> Result<ChannelPluginResponse, ChannelPluginCallError> {
    call_channel_plugin_once(manifest, request, timeout, None)
}

/// Run one request in a fresh plugin process, storing every ingress batch
/// through `sink` before its receipt and before the control response.
///
/// Plugin stdin stays open until the control response arrives, so the plugin
/// can wait for each receipt.
pub fn call_channel_plugin_with_sink(
    manifest: &ChannelPluginManifest,
    request: ChannelPluginRequest,
    timeout: Duration,
    sink: &mut dyn DurableChannelEventSink,
) -> Result<ChannelPluginResponse, ChannelPluginCallError> {
    call_channel_plugin_once(manifest, request, timeout, Some(sink))
}

const CHANNEL_PLUGIN_STDERR_SETTLE: Duration = Duration::from_secs(1);
const CHANNEL_PLUGIN_TIMEOUT_STDERR_SETTLE: Duration = Duration::from_millis(50);

#[allow(clippy::large_enum_variant)]
enum OneShotOutcome {
    Response(ChannelPluginResponse),
    Closed,
    TimedOut,
}

fn call_channel_plugin_once(
    manifest: &ChannelPluginManifest,
    request: ChannelPluginRequest,
    timeout: Duration,
    mut sink: Option<&mut dyn DurableChannelEventSink>,
) -> Result<ChannelPluginResponse, ChannelPluginCallError> {
    let channel = manifest.name.as_str();
    let requires_batch = requires_durable_batch(&request);
    if requires_batch && sink.is_none() {
        return Err(ChannelPluginCallError::PluginProtocol {
            channel: channel.to_owned(),
            message: "channel ingress requires an explicit durable sink".into(),
        });
    }
    let deadline = Instant::now() + timeout;
    let mut process = spawn_persistent_channel_plugin(manifest)?;
    let outcome = (|| -> Result<OneShotOutcome, ChannelPluginCallError> {
        let request_id = process.write_request(manifest, request)?;
        let mut accepted_batches = 0usize;
        loop {
            match recv_channel_plugin_frame(&mut process, Some(deadline))? {
                ChannelPluginFrame::Message(PluginMessage::EventRequest { request, .. }) => {
                    let Some(sink) = sink.as_deref_mut() else {
                        return Err(ChannelPluginCallError::PluginProtocol {
                            channel: channel.to_owned(),
                            message:
                                "channel plugin sent an ingress batch during a non-ingress call"
                                    .into(),
                        });
                    };
                    ensure_channel_acceptance_page(channel, accepted_batches)?;
                    write_durable_channel_acceptance(&mut process.stdin, channel, &request, sink)?;
                    accepted_batches += 1;
                }
                ChannelPluginFrame::Message(PluginMessage::Heartbeat(_)) => {}
                ChannelPluginFrame::Message(PluginMessage::Response { id, response }) => {
                    ensure_matching_channel_request_id(channel, &request_id, &id)?;
                    ensure_durable_batch_received(
                        channel,
                        requires_batch,
                        accepted_batches,
                        &response,
                    )?;
                    return Ok(OneShotOutcome::Response(response));
                }
                ChannelPluginFrame::Closed => return Ok(OneShotOutcome::Closed),
                ChannelPluginFrame::TimedOut => return Ok(OneShotOutcome::TimedOut),
            }
        }
    })();

    let PersistentChannelPluginProcess {
        mut child,
        stdin,
        stderr,
        stderr_reader,
        ..
    } = process;
    let response = match outcome {
        Ok(OneShotOutcome::Response(response)) => Some(response),
        Ok(OneShotOutcome::Closed) => None,
        Ok(OneShotOutcome::TimedOut) => {
            kill_child_and_wait(&mut child);
            return Err(ChannelPluginCallError::PluginTimedOut {
                channel: channel.to_owned(),
                timeout_ms: timeout.as_millis(),
                stderr: settled_channel_stderr(
                    &stderr,
                    &stderr_reader,
                    CHANNEL_PLUGIN_TIMEOUT_STDERR_SETTLE,
                ),
            });
        }
        Err(error) => {
            kill_child_and_wait(&mut child);
            return Err(error);
        }
    };

    // Closing stdin tells the plugin that no further requests or receipts follow.
    drop(stdin);
    let status = match wait_for_child_timeout(
        &mut child,
        Some(remaining_channel_plugin_timeout(deadline)),
        CHANNEL_PLUGIN_POLL_INTERVAL,
    ) {
        Ok(Some(status)) => status,
        Ok(None) => {
            kill_child_and_wait(&mut child);
            return Err(ChannelPluginCallError::PluginTimedOut {
                channel: channel.to_owned(),
                timeout_ms: timeout.as_millis(),
                stderr: settled_channel_stderr(
                    &stderr,
                    &stderr_reader,
                    CHANNEL_PLUGIN_TIMEOUT_STDERR_SETTLE,
                ),
            });
        }
        Err(source) => {
            kill_child_and_wait(&mut child);
            return Err(ChannelPluginCallError::WaitPlugin {
                channel: channel.to_owned(),
                source,
            });
        }
    };
    match (response, status.success()) {
        (Some(response), true) => Ok(response),
        (None, true) => Err(ChannelPluginCallError::PluginProtocol {
            channel: channel.to_owned(),
            message: "channel plugin produced no response".to_string(),
        }),
        (_, false) => Err(ChannelPluginCallError::PluginExit {
            channel: channel.to_owned(),
            status: status.code().unwrap_or(-1),
            stderr: settled_channel_stderr(&stderr, &stderr_reader, CHANNEL_PLUGIN_STDERR_SETTLE),
        }),
    }
}

fn remaining_channel_plugin_timeout(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn describe_channel_plugin_response(response: &ChannelPluginResponse) -> &'static str {
    match response {
        ChannelPluginResponse::Capabilities { .. } => "capabilities",
        ChannelPluginResponse::Configured { .. } => "configured",
        ChannelPluginResponse::Health { .. } => "health",
        ChannelPluginResponse::IngressStarted { .. } => "ingress_started",
        ChannelPluginResponse::IngressStopped { .. } => "ingress_stopped",
        ChannelPluginResponse::IngressEventsReceived { .. } => "ingress_events_received",
        ChannelPluginResponse::Delivered { .. } => "delivered",
        ChannelPluginResponse::Pushed { .. } => "pushed",
        ChannelPluginResponse::MessageFetched { .. } => "message_fetched",
        ChannelPluginResponse::MessageNotFound { .. } => "message_not_found",
        ChannelPluginResponse::PermalinkResolved { .. } => "permalink_resolved",
        ChannelPluginResponse::StatusAccepted { .. } => "status_accepted",
        ChannelPluginResponse::Ok => "ok",
        ChannelPluginResponse::Error { .. } => "error",
    }
}

pub fn channel_event_session_file(
    session_root: &Path,
    plugin_name: &str,
    parcel_digest: &str,
    event: &InboundEventEnvelope,
) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(plugin_name.as_bytes());
    hasher.update(b"\n");
    hasher.update(parcel_digest.as_bytes());
    hasher.update(b"\n");
    hasher.update(event.platform.as_bytes());
    hasher.update(b"\n");
    hasher.update(event.conversation.id.as_bytes());
    hasher.update(b"\n");
    hasher.update(
        event
            .conversation
            .thread_id
            .as_deref()
            .unwrap_or_default()
            .as_bytes(),
    );
    let digest = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    session_root
        .join(plugin_name)
        .join(format!("{}.session.json", &digest[..24]))
}

pub fn render_inbound_event_chat_input(
    plugin_name: &str,
    event: &InboundEventEnvelope,
) -> Result<String, serde_json::Error> {
    if should_render_plain_message_chat_input(event) {
        return Ok(event.message.content.clone());
    }

    let event_json = serde_json::to_string_pretty(event)?;
    Ok(format!(
        "Dispatch inbound channel event\nplugin: {}\nplatform: {}\nevent_type: {}\nconversation_id: {}\nthread_id: {}\nactor_id: {}\nactor_name: {}\nmessage_id: {}\ncontent_type: {}\n\nUser message:\n{}\n\nRaw event JSON:\n{}\n",
        plugin_name,
        event.platform,
        event.event_type,
        event.conversation.id,
        event.conversation.thread_id.as_deref().unwrap_or(""),
        event.actor.id,
        event
            .actor
            .display_name
            .as_deref()
            .or(event.actor.username.as_deref())
            .unwrap_or(""),
        event.message.id,
        event.message.content_type,
        event.message.content,
        event_json
    ))
}

fn should_render_plain_message_chat_input(event: &InboundEventEnvelope) -> bool {
    if event.message.content.trim().is_empty() {
        return false;
    }
    if !event.message.attachments.is_empty() {
        return false;
    }
    if !event.message.content_type.starts_with("text/") {
        return false;
    }

    // Accept the event types that existing channel plugins emit for a user
    // typing a text message: plain `message`, any `message.*` variant (covers
    // `created`, `received`, `edited`, `updated`), Telegram `channel_post.*`
    // posts, and Twilio SMS `incoming_message`. Other event types
    // (reactions, presence, interactions, etc.) still take the structured
    // fallback so the courier has enough context to interpret them.
    let event_type = event.event_type.as_str();
    event_type == "incoming_message"
        || event_type == "message"
        || event_type.starts_with("message.")
        || event_type.starts_with("channel_post")
}

pub fn extract_assistant_reply(events: &[CourierEvent]) -> Option<String> {
    let mut streamed = String::new();
    let mut latest_message = None;
    for event in events {
        match event {
            CourierEvent::TextDelta { content } => streamed.push_str(content),
            CourierEvent::ChannelReply { message } => {
                latest_message = Some(message.content.clone());
            }
            CourierEvent::Message { role, content } if role == "assistant" => {
                latest_message = Some(content.clone());
            }
            _ => {}
        }
    }
    if !streamed.is_empty() {
        Some(streamed)
    } else {
        latest_message
    }
}

pub fn extract_assistant_channel_reply(events: &[CourierEvent]) -> Option<OutboundMessageEnvelope> {
    let mut latest_channel_reply = None;
    for event in events {
        if let CourierEvent::ChannelReply { message } = event {
            latest_channel_reply = Some(message.clone());
        }
    }
    if latest_channel_reply.is_some() {
        return latest_channel_reply;
    }

    let reply_text = extract_assistant_reply(events)?;
    if let Some(tagged_reply) = parse_tagged_channel_reply(&reply_text) {
        return Some(tagged_reply);
    }

    Some(OutboundMessageEnvelope {
        content: reply_text,
        content_type: Some("text/plain".to_string()),
        attachments: Vec::new(),
        metadata: BTreeMap::new(),
    })
}

pub fn build_channel_reply_message(
    event: &InboundEventEnvelope,
    reply_text: &str,
) -> OutboundMessageEnvelope {
    build_channel_reply_envelope(
        event,
        OutboundMessageEnvelope {
            content: reply_text.to_string(),
            content_type: Some("text/plain".to_string()),
            attachments: Vec::new(),
            metadata: BTreeMap::new(),
        },
    )
}

pub fn build_channel_reply_envelope(
    event: &InboundEventEnvelope,
    mut message: OutboundMessageEnvelope,
) -> OutboundMessageEnvelope {
    if message.content_type.is_none() {
        message.content_type = Some("text/plain".to_string());
    }
    message
        .metadata
        .insert("conversation_id".to_string(), event.conversation.id.clone());
    if let Some(thread_id) = event.conversation.thread_id.as_deref() {
        message
            .metadata
            .insert("thread_id".to_string(), thread_id.to_string());
    }
    message
        .metadata
        .insert("reply_to_message_id".to_string(), event.message.id.clone());
    message
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
    if manifest.protocol_version != CHANNEL_PLUGIN_PROTOCOL_VERSION {
        return Err(PluginRegistryError::InvalidManifest {
            path: path.display().to_string(),
            message: format!(
                "protocol_version `{}` is unsupported; expected {}",
                manifest.protocol_version, CHANNEL_PLUGIN_PROTOCOL_VERSION
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
            Self::Provider => "provider",
            Self::Database => "database",
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
    match_channel_ingress_endpoint(plugin, method, path).is_some()
}

pub fn match_channel_ingress_endpoint<'a>(
    plugin: &'a ChannelPluginManifest,
    method: &str,
    path: &str,
) -> Option<&'a ChannelIngressEndpoint> {
    plugin.ingress.as_ref()?.endpoints.iter().find(|endpoint| {
        endpoint.path == path
            && (endpoint.methods.is_empty()
                || endpoint
                    .methods
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(method)))
    })
}

pub fn verify_host_managed_ingress_trust(
    plugin: &ChannelPluginManifest,
    headers: &BTreeMap<String, String>,
) -> Result<bool, ChannelIngressTrustFailure> {
    let Some(ingress) = &plugin.ingress else {
        return Ok(false);
    };
    let Some(trust) = &ingress.trust else {
        return Ok(false);
    };
    if !trust.host_managed {
        return Ok(false);
    }

    match trust.mode.as_str() {
        "shared_secret_header" => {
            let header_name =
                trust
                    .header_name
                    .as_deref()
                    .ok_or_else(|| ChannelIngressTrustFailure {
                        status_code: 500,
                        status_text: "Internal Server Error",
                        message: format!(
                            "channel plugin `{}` declares host-managed shared_secret_header trust without header_name",
                            plugin.name
                        ),
                    })?;
            let secret_name =
                trust
                    .secret_name
                    .as_deref()
                    .ok_or_else(|| ChannelIngressTrustFailure {
                        status_code: 500,
                        status_text: "Internal Server Error",
                        message: format!(
                            "channel plugin `{}` declares host-managed shared_secret_header trust without secret_name",
                            plugin.name
                        ),
                    })?;
            let header_key = header_name.to_ascii_lowercase();
            let actual_secret =
                headers
                    .get(&header_key)
                    .ok_or_else(|| ChannelIngressTrustFailure {
                        status_code: 403,
                        status_text: "Forbidden",
                        message: format!("missing required ingress trust header {header_name}"),
                    })?;
            let expected_secret =
                std::env::var(secret_name).map_err(|_| ChannelIngressTrustFailure {
                    status_code: 500,
                    status_text: "Internal Server Error",
                    message: format!("host-managed ingress trust secret {secret_name} is not set"),
                })?;
            if actual_secret != &expected_secret {
                return Err(ChannelIngressTrustFailure {
                    status_code: 403,
                    status_text: "Forbidden",
                    message: format!("ingress trust header {header_name} did not match"),
                });
            }
            Ok(true)
        }
        other => Err(ChannelIngressTrustFailure {
            status_code: 500,
            status_text: "Internal Server Error",
            message: format!(
                "channel plugin `{}` declares unsupported host-managed ingress trust mode `{other}`",
                plugin.name
            ),
        }),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::channel_plugin_protocol::MAX_CHANNEL_FRAME_BYTES;
    use crate::channel_plugin_protocol::{
        InboundActor, InboundConversationRef, InboundEventEnvelope, InboundMessage,
    };
    use serde_json::Value;
    use std::sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    };
    use tempfile::tempdir;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn empty_event_request() -> ChannelEventRequest {
        ChannelEventRequest::new(
            crate::channel_plugin_protocol::ChannelEventBatch {
                events: vec![],
                state: None,
                poll_after_ms: Some(100),
            },
            None,
        )
        .unwrap()
    }

    struct RecordingSink {
        committed: Arc<AtomicBool>,
        fail: bool,
        mismatch: bool,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                committed: Arc::new(AtomicBool::new(false)),
                fail: false,
                mismatch: false,
            }
        }

        fn committed(&self) -> bool {
            self.committed.load(Ordering::SeqCst)
        }
    }

    impl DurableChannelEventSink for RecordingSink {
        fn accept(
            &mut self,
            request: &ChannelEventRequest,
        ) -> Result<ChannelEventAcceptance, std::io::Error> {
            if self.fail {
                return Err(std::io::Error::other("durable write failed"));
            }
            self.committed.store(true, Ordering::SeqCst);
            if self.mismatch {
                let mut other = request.batch.clone();
                other.poll_after_ms = Some(999);
                return Ok(ChannelEventRequest::new(other, None)
                    .unwrap()
                    .acceptance()
                    .unwrap());
            }
            Ok(request.acceptance().unwrap())
        }
    }

    #[test]
    fn positive_receipt_bytes_follow_durable_sink_commit_and_failures_emit_nothing() {
        struct CommitCheckedWriter {
            committed: Arc<AtomicBool>,
            bytes: Vec<u8>,
        }
        impl std::io::Write for CommitCheckedWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                assert!(self.committed.load(Ordering::SeqCst));
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let request = empty_event_request();
        let mut sink = RecordingSink::new();
        let mut writer = CommitCheckedWriter {
            committed: sink.committed.clone(),
            bytes: vec![],
        };
        write_durable_channel_acceptance(&mut writer, "test", &request, &mut sink).unwrap();
        let parsed = crate::channel_plugin_protocol::parse_plugin_input::<Value, Value>(
            std::str::from_utf8(&writer.bytes).unwrap().trim_end(),
        )
        .unwrap();
        assert!(matches!(
            parsed,
            crate::channel_plugin_protocol::PluginInput::EventAcceptance { acceptance, .. }
                if acceptance.matches(&request)
        ));
        for (fail, mismatch) in [(true, false), (false, true)] {
            let mut bytes = Vec::new();
            let mut sink = RecordingSink {
                fail,
                mismatch,
                ..RecordingSink::new()
            };
            let error = write_durable_channel_acceptance(&mut bytes, "test", &request, &mut sink)
                .unwrap_err();
            assert_eq!(
                matches!(error, ChannelPluginCallError::DurableSink { .. }),
                fail
            );
            assert!(bytes.is_empty());
        }
    }

    fn handshake_manifest(script: &Path) -> ChannelPluginManifest {
        ChannelPluginManifest {
            name: "receipt-test".into(),
            version: "0.1.0".into(),
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            transport: PluginTransport::Jsonl,
            description: None,
            exec: ChannelPluginExec {
                command: "/bin/sh".into(),
                args: vec![script.display().to_string()],
            },
            platform: None,
            attachment_sources: vec![],
            ingress: None,
            installed_sha256: None,
        }
    }

    #[test]
    fn released_inline_notification_manifests_are_rejected_before_launch() {
        let mut manifest = handshake_manifest(Path::new("/tmp/unused-plugin"));
        manifest.protocol_version = 1;
        assert!(
            validate_channel_plugin_manifest(Path::new("channel-plugin.json"), &manifest).is_err()
        );
        manifest.protocol_version = CHANNEL_PLUGIN_PROTOCOL_VERSION;
        validate_channel_plugin_manifest(Path::new("channel-plugin.json"), &manifest).unwrap();
    }

    fn poll_request(directory: &Path) -> ChannelPluginRequest {
        ChannelPluginRequest::PollIngress {
            config: Value::Null,
            storage: crate::channel_plugin_protocol::ChannelIngressStorage {
                recovery: None,
                journal_path: directory.join("producer.sqlite").display().to_string(),
            },
            state: None,
        }
    }

    fn write_plugin_script(directory: &Path, body: &str) -> PathBuf {
        let script = directory.join("plugin.sh");
        fs::write(&script, body).unwrap();
        script
    }

    const READ_REQUEST_ID: &str = r#"read request
request_id=$(printf '%s' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
"#;

    #[test]
    fn one_shot_poll_keeps_stdin_open_until_empty_batch_is_durably_accepted() {
        let directory = tempdir().unwrap();
        let event = empty_event_request();
        let wire = crate::channel_plugin_protocol::event_request_to_jsonrpc(&event).unwrap();
        let receipt =
            event_acceptance_to_jsonrpc(&event.request_id(), &event.acceptance().unwrap()).unwrap();
        let program = format!(
            r#"{READ_REQUEST_ID}printf '%s\n' '{wire}'
read receipt || exit 3
[ "$receipt" = '{receipt}' ] || exit 4
printf '%s\n' "{{\"jsonrpc\":\"2.0\",\"id\":$request_id,\"result\":{{\"kind\":\"ingress_events_received\"}}}}"
"#
        );
        let manifest = handshake_manifest(&write_plugin_script(directory.path(), &program));
        let mut sink = RecordingSink::new();
        let response = call_channel_plugin_with_sink(
            &manifest,
            poll_request(directory.path()),
            Duration::from_secs(5),
            &mut sink,
        )
        .unwrap();
        assert!(matches!(
            response,
            ChannelPluginResponse::IngressEventsReceived { .. }
        ));
        assert!(sink.committed());
    }

    #[test]
    fn one_shot_poll_rejects_control_only_success_without_any_batch_receipt() {
        let directory = tempdir().unwrap();
        let program = format!(
            r#"{READ_REQUEST_ID}printf '%s\n' "{{\"jsonrpc\":\"2.0\",\"id\":$request_id,\"result\":{{\"kind\":\"ingress_events_received\"}}}}"
"#
        );
        let manifest = handshake_manifest(&write_plugin_script(directory.path(), &program));
        let mut sink = RecordingSink::new();
        let error = call_channel_plugin_with_sink(
            &manifest,
            poll_request(directory.path()),
            Duration::from_secs(5),
            &mut sink,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ChannelPluginCallError::PluginProtocol { ref message, .. }
                if message.contains("without a durable batch acceptance")
        ));
        assert!(!sink.committed());
    }

    #[test]
    fn one_shot_ingress_without_sink_and_batch_during_ordinary_call_are_refused() {
        let directory = tempdir().unwrap();
        let event = empty_event_request();
        let wire = crate::channel_plugin_protocol::event_request_to_jsonrpc(&event).unwrap();
        let program = format!(
            r#"{READ_REQUEST_ID}printf '%s\n' '{wire}'
read receipt && exit 5
printf '%s\n' "{{\"jsonrpc\":\"2.0\",\"id\":$request_id,\"result\":{{\"kind\":\"ok\"}}}}"
"#
        );
        let manifest = handshake_manifest(&write_plugin_script(directory.path(), &program));
        let error = call_channel_plugin_with_timeout(
            &manifest,
            poll_request(directory.path()),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ChannelPluginCallError::PluginProtocol { ref message, .. }
                if message.contains("explicit durable sink")
        ));
        let error = call_channel_plugin_with_timeout(
            &manifest,
            ChannelPluginRequest::Health {
                config: Value::Null,
            },
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ChannelPluginCallError::PluginProtocol { ref message, .. }
                if message.contains("during a non-ingress call")
        ));
    }

    #[test]
    fn persistent_control_preserves_interleaved_original_batch_until_explicit_sink_commit() {
        let directory = tempdir().unwrap();
        let event = empty_event_request();
        let wire = crate::channel_plugin_protocol::event_request_to_jsonrpc(&event).unwrap();
        let receipt =
            event_acceptance_to_jsonrpc(&event.request_id(), &event.acceptance().unwrap()).unwrap();
        let program = format!(
            r#"{READ_REQUEST_ID}printf '%s\n' '{wire}'
printf '%s\n' "{{\"jsonrpc\":\"2.0\",\"id\":$request_id,\"result\":{{\"kind\":\"health\",\"health\":{{\"ok\":true,\"status\":\"healthy\"}}}}}}"
read receipt || exit 3
[ "$receipt" = '{receipt}' ] || exit 4
read request || exit 5
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"kind":"ok"}}}}'
"#
        );
        let manifest = handshake_manifest(&write_plugin_script(directory.path(), &program));
        let mut process = spawn_persistent_channel_plugin(&manifest).unwrap();
        let response = call_persistent_channel_plugin(
            &mut process,
            &manifest,
            ChannelPluginRequest::Health {
                config: Value::Null,
            },
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(matches!(response, ChannelPluginResponse::Health { .. }));
        let pending = drain_pending_channel_ingress(&mut process);
        assert_eq!(pending.len(), 1);
        let ChannelIngressMessage::Events { request, .. } = &pending[0] else {
            panic!("original batch disappeared");
        };
        assert_eq!(request, &event);
        let mut sink = RecordingSink::new();
        assert!(!sink.committed());
        accept_persistent_channel_event(&mut process, &manifest, request, &mut sink).unwrap();
        assert!(sink.committed());
        shutdown_persistent_channel_plugin(&mut process, &manifest).unwrap();
    }

    #[test]
    fn persistent_sink_call_writes_receipt_before_waiting_for_control_response() {
        let directory = tempdir().unwrap();
        let event = empty_event_request();
        let wire = crate::channel_plugin_protocol::event_request_to_jsonrpc(&event).unwrap();
        let receipt =
            event_acceptance_to_jsonrpc(&event.request_id(), &event.acceptance().unwrap()).unwrap();
        // The plugin blocks on the receipt before it answers the control request.
        let program = format!(
            r#"{READ_REQUEST_ID}printf '%s\n' '{wire}'
read receipt || exit 3
[ "$receipt" = '{receipt}' ] || exit 4
printf '%s\n' "{{\"jsonrpc\":\"2.0\",\"id\":$request_id,\"result\":{{\"kind\":\"health\",\"health\":{{\"ok\":true,\"status\":\"healthy\"}}}}}}"
read request
"#
        );
        let manifest = handshake_manifest(&write_plugin_script(directory.path(), &program));
        let mut process = spawn_persistent_channel_plugin(&manifest).unwrap();
        let mut sink = RecordingSink::new();
        let response = call_persistent_channel_plugin_with_sink(
            &mut process,
            &manifest,
            ChannelPluginRequest::Health {
                config: Value::Null,
            },
            Duration::from_secs(5),
            &mut sink,
        )
        .unwrap();
        assert!(matches!(response, ChannelPluginResponse::Health { .. }));
        assert!(sink.committed());
        assert!(drain_pending_channel_ingress(&mut process).is_empty());
        shutdown_persistent_channel_plugin(&mut process, &manifest).unwrap();
    }

    #[test]
    fn persistent_heartbeats_do_not_consume_pending_batch_capacity() {
        let directory = tempdir().unwrap();
        let heartbeat =
            crate::channel_plugin_protocol::heartbeat_to_jsonrpc(&PluginNotificationEnvelope {
                protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
                notification: ChannelHeartbeat::default(),
            })
            .unwrap();
        let program = format!(
            r#"{READ_REQUEST_ID}i=0
while [ "$i" -lt 80 ]; do printf '%s\n' '{heartbeat}'; i=$((i + 1)); done
printf '%s\n' "{{\"jsonrpc\":\"2.0\",\"id\":$request_id,\"result\":{{\"kind\":\"health\",\"health\":{{\"ok\":true,\"status\":\"healthy\"}}}}}}"
read request
"#
        );
        let manifest = handshake_manifest(&write_plugin_script(directory.path(), &program));
        let mut process = spawn_persistent_channel_plugin(&manifest).unwrap();
        let response = call_persistent_channel_plugin(
            &mut process,
            &manifest,
            ChannelPluginRequest::Health {
                config: Value::Null,
            },
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(matches!(response, ChannelPluginResponse::Health { .. }));
        assert!(drain_pending_channel_ingress(&mut process).is_empty());
        shutdown_persistent_channel_plugin(&mut process, &manifest).unwrap();
    }

    #[test]
    fn persistent_sink_failure_sends_no_receipt_and_retires_the_process() {
        let directory = tempdir().unwrap();
        let event = empty_event_request();
        let wire = crate::channel_plugin_protocol::event_request_to_jsonrpc(&event).unwrap();
        let receipt_log = directory.path().join("receipt.log");
        let program = format!(
            r#"{READ_REQUEST_ID}printf '%s\n' '{wire}'
read receipt && printf '%s\n' "$receipt" >> '{log}'
sleep 5
"#,
            log = receipt_log.display()
        );
        let manifest = handshake_manifest(&write_plugin_script(directory.path(), &program));
        let mut process = spawn_persistent_channel_plugin(&manifest).unwrap();
        let mut sink = RecordingSink {
            fail: true,
            ..RecordingSink::new()
        };
        let error = call_persistent_channel_plugin_with_sink(
            &mut process,
            &manifest,
            poll_request(directory.path()),
            Duration::from_secs(5),
            &mut sink,
        )
        .unwrap_err();
        assert!(matches!(error, ChannelPluginCallError::DurableSink { .. }));
        assert!(!receipt_log.exists());
        let error = call_persistent_channel_plugin(
            &mut process,
            &manifest,
            ChannelPluginRequest::Health {
                config: Value::Null,
            },
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ChannelPluginCallError::PluginProtocol { ref message, .. }
                if message.contains("earlier exchange failure")
        ));
        shutdown_persistent_channel_plugin(&mut process, &manifest).unwrap();
    }

    #[test]
    fn invalid_plugin_lines_are_refused_and_ordinary_responses_keep_their_size_contract() {
        let mut reader = std::io::Cursor::new(vec![b'x'; MAX_CHANNEL_FRAME_BYTES + 20]);
        assert!(matches!(
            read_channel_plugin_message(&mut reader, "test"),
            Err(ChannelPluginCallError::PluginProtocol { .. })
        ));
        assert_eq!(reader.position(), (MAX_CHANNEL_FRAME_BYTES + 20) as u64);

        let mut maximal = vec![b' '; MAX_CHANNEL_FRAME_BYTES];
        let frame = br#"{"jsonrpc":"2.0","id":1,"result":{"kind":"ok"}}"#;
        maximal[..frame.len()].copy_from_slice(frame);
        maximal.push(b'\n');
        let mut reader = std::io::Cursor::new(maximal);
        assert!(matches!(
            read_channel_plugin_message(&mut reader, "test"),
            Ok(Some(PluginMessage::Response { .. }))
        ));
    }

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
"protocol_version": 2,
"description": "Demo channel plugin for Telegram",
"entrypoint": {{
    "command": "{}",
    "args": ["--stdio"]
}},
"capabilities": {{
    "channel": {{
        "platform": "telegram",
        "delivery": {{
            "attachment_sources": ["data_base64", "url"]
        }},
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
        assert_eq!(
            installed.attachment_sources,
            vec![AttachmentSource::DataBase64, AttachmentSource::Url]
        );

        let registry = load_channel_registry(Some(&registry_path)).unwrap();
        assert_eq!(registry.plugins.len(), 1);
        assert_eq!(registry.plugins[0].name, "telegram-bridge");
        assert_eq!(registry.plugins[0].transport, PluginTransport::Jsonl);
        assert_eq!(registry.plugins[0].platform.as_deref(), Some("telegram"));
        assert_eq!(
            registry.plugins[0].attachment_sources,
            vec![AttachmentSource::DataBase64, AttachmentSource::Url]
        );
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
"protocol_version": 2,
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
"protocol_version": 2,
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
                    protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
                    transport: PluginTransport::Jsonl,
                    description: None,
                    exec: ChannelPluginExec {
                        command: "/usr/bin/true".to_string(),
                        args: vec![],
                    },
                    platform: Some("telegram".to_string()),
                    attachment_sources: vec![],
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
                    protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
                    transport: PluginTransport::Jsonl,
                    description: None,
                    exec: ChannelPluginExec {
                        command: "/usr/bin/true".to_string(),
                        args: vec![],
                    },
                    platform: Some("slack".to_string()),
                    attachment_sources: vec![],
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
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            transport: PluginTransport::Jsonl,
            description: None,
            exec: ChannelPluginExec {
                command: "/usr/bin/true".to_string(),
                args: vec![],
            },
            platform: None,
            attachment_sources: vec![],
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
            attachment_sources: vec![],
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
"protocol_version": 2,
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
"protocol_version": 2,
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
request_id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
[ -n "$request_id" ] || request_id=1
printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$request_id,\"result\":{\"kind\":\"capabilities\",\"capabilities\":{\"plugin_id\":\"telegram\",\"platform\":\"telegram\",\"ingress_modes\":[\"webhook\"],\"outbound_message_types\":[\"text\"],\"threading_model\":\"chat_or_topic\",\"attachment_support\":false,\"reply_verification_support\":true,\"account_scoped_config\":true,\"accepts_push\":true,\"accepts_status_frames\":true}}}"
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();

        let manifest = ChannelPluginManifest {
            name: "telegram-demo".to_string(),
            version: "0.1.0".to_string(),
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            transport: PluginTransport::Jsonl,
            description: None,
            exec: ChannelPluginExec {
                command: script_path.display().to_string(),
                args: vec![],
            },
            platform: Some("telegram".to_string()),
            attachment_sources: vec![],
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
    fn call_channel_plugin_rejects_mismatched_jsonrpc_id() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let script_path = dir.path().join("channel-mismatched-id.sh");
        fs::write(
            &script_path,
            r#"#!/bin/sh
read line
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"kind":"capabilities","capabilities":{"plugin_id":"telegram","platform":"telegram","ingress_modes":["webhook"],"outbound_message_types":["text"],"threading_model":"chat_or_topic","attachment_support":false,"reply_verification_support":true,"account_scoped_config":true,"accepts_push":true,"accepts_status_frames":true}}}'
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();

        let manifest = ChannelPluginManifest {
            name: "telegram-mismatched-id".to_string(),
            version: "0.1.0".to_string(),
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            transport: PluginTransport::Jsonl,
            description: None,
            exec: ChannelPluginExec {
                command: script_path.display().to_string(),
                args: vec![],
            },
            platform: Some("telegram".to_string()),
            attachment_sources: vec![],
            ingress: None,
            installed_sha256: None,
        };

        let error = call_channel_plugin(&manifest, ChannelPluginRequest::Capabilities).unwrap_err();
        assert!(matches!(
            error,
            ChannelPluginCallError::PluginProtocol { message, .. }
                if message.contains("did not match request id")
        ));
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
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            transport: PluginTransport::Jsonl,
            description: None,
            exec: ChannelPluginExec {
                command: script_path.display().to_string(),
                args: vec![],
            },
            platform: None,
            attachment_sources: vec![],
            ingress: None,
            installed_sha256: None,
        };

        let error = call_channel_plugin(&manifest, ChannelPluginRequest::Capabilities).unwrap_err();
        assert!(matches!(
            error,
            ChannelPluginCallError::PluginProtocol { message, .. } if message.contains("invalid JSON-RPC message")
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
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            transport: PluginTransport::Jsonl,
            description: None,
            exec: ChannelPluginExec {
                command: script_path.display().to_string(),
                args: vec![],
            },
            platform: None,
            attachment_sources: vec![],
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

    #[test]
    fn call_channel_plugin_handles_large_stderr_without_deadlock() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let script_path = dir.path().join("channel-chatty-stderr.sh");
        fs::write(
            &script_path,
            r#"#!/bin/sh
read line
dd if=/dev/zero bs=131072 count=1 2>/dev/null | tr '\000' x >&2
printf '\n' >&2
request_id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
[ -n "$request_id" ] || request_id=1
printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$request_id,\"result\":{\"kind\":\"capabilities\",\"capabilities\":{\"plugin_id\":\"telegram\",\"platform\":\"telegram\",\"ingress_modes\":[\"webhook\"],\"outbound_message_types\":[\"text\"],\"threading_model\":\"chat_or_topic\",\"attachment_support\":false,\"reply_verification_support\":true,\"account_scoped_config\":true,\"accepts_push\":true,\"accepts_status_frames\":true}}}"
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();

        let manifest = ChannelPluginManifest {
            name: "chatty-stderr-demo".to_string(),
            version: "0.1.0".to_string(),
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            transport: PluginTransport::Jsonl,
            description: None,
            exec: ChannelPluginExec {
                command: script_path.display().to_string(),
                args: vec![],
            },
            platform: Some("telegram".to_string()),
            attachment_sources: vec![],
            ingress: None,
            installed_sha256: None,
        };

        let response = call_channel_plugin(&manifest, ChannelPluginRequest::Capabilities).unwrap();
        let ChannelPluginResponse::Capabilities { capabilities } = response else {
            panic!("unexpected response variant");
        };
        assert_eq!(capabilities.plugin_id, "telegram");
    }

    #[test]
    fn call_channel_plugin_times_out_when_plugin_never_replies() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let script_path = dir.path().join("channel-hangs-before-response.sh");
        fs::write(
            &script_path,
            r#"#!/bin/sh
read line
sleep 5
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();

        let manifest = ChannelPluginManifest {
            name: "hangs-before-response-demo".to_string(),
            version: "0.1.0".to_string(),
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            transport: PluginTransport::Jsonl,
            description: None,
            exec: ChannelPluginExec {
                command: script_path.display().to_string(),
                args: vec![],
            },
            platform: None,
            attachment_sources: vec![],
            ingress: None,
            installed_sha256: None,
        };

        let error = call_channel_plugin_with_timeout(
            &manifest,
            ChannelPluginRequest::Capabilities,
            Duration::from_millis(50),
        )
        .unwrap_err();
        assert!(
            matches!(&error, ChannelPluginCallError::PluginTimedOut { timeout_ms, .. } if *timeout_ms == 50),
            "expected a 50 ms timeout from the silent plugin, got {error:?}"
        );
    }

    #[test]
    fn call_channel_plugin_times_out_when_plugin_is_fully_silent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let script_path = dir.path().join("channel-hangs-silently.sh");
        fs::write(
            &script_path,
            r#"#!/bin/sh
sleep 5
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();

        let manifest = ChannelPluginManifest {
            name: "silent-hang-demo".to_string(),
            version: "0.1.0".to_string(),
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            transport: PluginTransport::Jsonl,
            description: None,
            exec: ChannelPluginExec {
                command: script_path.display().to_string(),
                args: vec![],
            },
            platform: None,
            attachment_sources: vec![],
            ingress: None,
            installed_sha256: None,
        };

        let error = call_channel_plugin_with_timeout(
            &manifest,
            ChannelPluginRequest::Capabilities,
            Duration::from_millis(50),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ChannelPluginCallError::PluginTimedOut { timeout_ms, .. } if timeout_ms == 50
        ));
    }

    #[test]
    fn call_channel_plugin_times_out_when_plugin_never_exits_after_reply() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let script_path = dir.path().join("channel-hangs-after-response.sh");
        fs::write(
            &script_path,
            r#"#!/bin/sh
read line
request_id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
[ -n "$request_id" ] || request_id=1
printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$request_id,\"result\":{\"kind\":\"capabilities\",\"capabilities\":{\"plugin_id\":\"telegram\",\"platform\":\"telegram\",\"ingress_modes\":[\"webhook\"],\"outbound_message_types\":[\"text\"],\"threading_model\":\"chat_or_topic\",\"attachment_support\":false,\"reply_verification_support\":true,\"account_scoped_config\":true,\"accepts_push\":true,\"accepts_status_frames\":true}}}"
sleep 5
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();

        let manifest = ChannelPluginManifest {
            name: "hangs-after-response-demo".to_string(),
            version: "0.1.0".to_string(),
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            transport: PluginTransport::Jsonl,
            description: None,
            exec: ChannelPluginExec {
                command: script_path.display().to_string(),
                args: vec![],
            },
            platform: Some("telegram".to_string()),
            attachment_sources: vec![],
            ingress: None,
            installed_sha256: None,
        };

        let error = call_channel_plugin_with_timeout(
            &manifest,
            ChannelPluginRequest::Capabilities,
            Duration::from_millis(50),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ChannelPluginCallError::PluginTimedOut { timeout_ms, .. } if timeout_ms == 50
        ));
    }

    #[test]
    fn call_persistent_channel_plugin_times_out_without_hanging_on_stderr() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let script_path = dir.path().join("persistent-hangs.sh");
        fs::write(
            &script_path,
            r#"#!/bin/sh
# Hold the process alive indefinitely after reading each request so the host
# has to kill us on timeout. If the host tries to drain stderr on a still-
# running child, this test will hang past the measured deadline.
while IFS= read -r _line; do
    sleep 5
done
sleep 5
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();

        let manifest = ChannelPluginManifest {
            name: "persistent-hangs-demo".to_string(),
            version: "0.1.0".to_string(),
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            transport: PluginTransport::Jsonl,
            description: None,
            exec: ChannelPluginExec {
                command: script_path.display().to_string(),
                args: vec![],
            },
            platform: None,
            attachment_sources: vec![],
            ingress: None,
            installed_sha256: None,
        };

        let mut process = spawn_persistent_channel_plugin(&manifest).expect("spawn plugin");
        let start = Instant::now();
        let error = call_persistent_channel_plugin(
            &mut process,
            &manifest,
            ChannelPluginRequest::Capabilities,
            Duration::from_millis(50),
        )
        .expect_err("call should time out");
        // The fix for the stderr-drain hang ensures we return well within the
        // plugin's long `sleep 5`. Use a generous upper bound so we only fail
        // if the timeout path actually regresses to blocking on stderr.
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "persistent channel call took {elapsed:?} to time out; likely hanging on stderr drain"
        );
        assert!(matches!(
            error,
            ChannelPluginCallError::PluginTimedOut { timeout_ms, .. } if timeout_ms == 50
        ));
        // A late response could otherwise answer the next request.
        let error = call_persistent_channel_plugin(
            &mut process,
            &manifest,
            ChannelPluginRequest::Capabilities,
            Duration::from_millis(50),
        )
        .expect_err("timed-out process must not be reused");
        assert!(matches!(
            error,
            ChannelPluginCallError::PluginProtocol { ref message, .. }
                if message.contains("earlier exchange failure")
        ));
    }

    #[test]
    fn channel_event_session_file_is_stable_for_same_thread() {
        let event = sample_inbound_event();

        let first = channel_event_session_file(
            Path::new("/tmp/channel-sessions"),
            "channel-telegram",
            "digest-123",
            &event,
        );
        let second = channel_event_session_file(
            Path::new("/tmp/channel-sessions"),
            "channel-telegram",
            "digest-123",
            &event,
        );

        assert_eq!(first, second);
        assert!(
            first
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".session.json"))
        );
    }

    #[test]
    fn render_inbound_event_chat_input_uses_plain_message_for_text_events() {
        let rendered = render_inbound_event_chat_input("channel-telegram", &sample_inbound_event())
            .expect("render");

        assert_eq!(rendered, "hello from telegram");
    }

    #[test]
    fn render_inbound_event_chat_input_falls_back_to_structured_context_for_non_message_events() {
        let mut event = sample_inbound_event();
        event.event_type = "reaction.added".to_string();

        let rendered = render_inbound_event_chat_input("channel-telegram", &event).expect("render");

        assert!(rendered.contains("plugin: channel-telegram"));
        assert!(rendered.contains("conversation_id: chat-123"));
        assert!(rendered.contains("User message:\nhello from telegram"));
        assert!(rendered.contains("\"event_id\": \"evt-1\""));
    }

    #[test]
    fn render_inbound_event_chat_input_uses_plain_message_for_known_event_type_aliases() {
        for event_type in [
            "message",
            "message.created",
            "message.received",
            "message.edited",
            "message.updated",
            "channel_post.received",
            "channel_post.edited",
            "incoming_message",
        ] {
            let mut event = sample_inbound_event();
            event.event_type = event_type.to_string();
            let rendered = render_inbound_event_chat_input("channel-test", &event).expect("render");
            assert_eq!(
                rendered, "hello from telegram",
                "event_type `{event_type}` should render as plain text content"
            );
        }
    }

    #[test]
    fn render_inbound_event_chat_input_falls_back_to_structured_context_for_attachments() {
        let mut event = sample_inbound_event();
        event
            .message
            .attachments
            .push(crate::channel_plugin_protocol::InboundAttachment {
                id: None,
                kind: "file".to_string(),
                url: None,
                mime_type: Some("text/plain".to_string()),
                size_bytes: None,
                name: Some("report.txt".to_string()),
                storage_key: None,
                extracted_text: None,
                extras: BTreeMap::new(),
            });

        let rendered = render_inbound_event_chat_input("channel-telegram", &event).expect("render");

        assert!(rendered.contains("Dispatch inbound channel event"));
        assert!(rendered.contains("\"attachments\": ["));
    }

    #[test]
    fn extract_assistant_reply_prefers_streamed_deltas() {
        let reply = extract_assistant_reply(&[
            CourierEvent::TextDelta {
                content: "hello ".to_string(),
            },
            CourierEvent::TextDelta {
                content: "world".to_string(),
            },
            CourierEvent::Done,
        ]);

        assert_eq!(reply.as_deref(), Some("hello world"));
    }

    #[test]
    fn extract_assistant_channel_reply_parses_tagged_json_envelope() {
        let reply = extract_assistant_channel_reply(&[CourierEvent::Message {
            role: "assistant".to_string(),
            content: serde_json::to_string(&serde_json::json!({
                "kind": "channel_reply",
                "content": "reply text",
                "content_type": "text/plain",
                "attachments": [{
                    "name": "report.txt",
                    "mime_type": "text/plain",
                    "data_base64": "aGVsbG8="
                }],
                "metadata": {
                    "custom": "value"
                }
            }))
            .expect("serialize channel reply"),
        }])
        .expect("assistant reply");

        assert_eq!(reply.content, "reply text");
        assert_eq!(reply.content_type.as_deref(), Some("text/plain"));
        assert_eq!(reply.attachments.len(), 1);
        assert_eq!(reply.attachments[0].name, "report.txt");
        assert_eq!(
            reply.attachments[0].data_base64.as_deref(),
            Some("aGVsbG8=")
        );
        assert_eq!(
            reply.metadata.get("custom").map(String::as_str),
            Some("value")
        );
    }

    #[test]
    fn extract_assistant_channel_reply_prefers_first_class_event() {
        let reply = extract_assistant_channel_reply(&[
            CourierEvent::Message {
                role: "assistant".to_string(),
                content: "plain fallback".to_string(),
            },
            CourierEvent::ChannelReply {
                message: OutboundMessageEnvelope {
                    content: "structured reply".to_string(),
                    content_type: Some("text/plain".to_string()),
                    attachments: Vec::new(),
                    metadata: BTreeMap::from([("custom".to_string(), "value".to_string())]),
                },
            },
        ])
        .expect("assistant reply");

        assert_eq!(reply.content, "structured reply");
        assert_eq!(
            reply.metadata.get("custom").map(String::as_str),
            Some("value")
        );
    }

    #[test]
    fn build_channel_reply_envelope_preserves_attachments_and_overwrites_routing() {
        let message = build_channel_reply_envelope(
            &sample_inbound_event(),
            OutboundMessageEnvelope {
                content: "reply text".to_string(),
                content_type: None,
                attachments: vec![crate::channel_plugin_protocol::OutboundAttachment {
                    name: "report.txt".to_string(),
                    mime_type: "text/plain".to_string(),
                    data_base64: Some("aGVsbG8=".to_string()),
                    url: None,
                    storage_key: None,
                }],
                metadata: BTreeMap::from([
                    ("custom".to_string(), "value".to_string()),
                    ("conversation_id".to_string(), "wrong".to_string()),
                ]),
            },
        );

        assert_eq!(message.content, "reply text");
        assert_eq!(message.content_type.as_deref(), Some("text/plain"));
        assert_eq!(message.attachments.len(), 1);
        assert_eq!(
            message.metadata.get("custom").map(String::as_str),
            Some("value")
        );
        assert_eq!(
            message.metadata.get("conversation_id").map(String::as_str),
            Some("chat-123")
        );
        assert_eq!(
            message.metadata.get("thread_id").map(String::as_str),
            Some("7")
        );
        assert_eq!(
            message
                .metadata
                .get("reply_to_message_id")
                .map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn build_channel_reply_message_includes_reply_metadata() {
        let message = build_channel_reply_message(&sample_inbound_event(), "reply text");

        assert_eq!(message.content, "reply text");
        assert_eq!(message.content_type.as_deref(), Some("text/plain"));
        assert!(message.attachments.is_empty());
        assert_eq!(
            message.metadata.get("conversation_id").map(String::as_str),
            Some("chat-123")
        );
        assert_eq!(
            message.metadata.get("thread_id").map(String::as_str),
            Some("7")
        );
        assert_eq!(
            message
                .metadata
                .get("reply_to_message_id")
                .map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn match_channel_ingress_endpoint_respects_method_and_path() {
        let plugin = ChannelPluginManifest {
            name: "channel-webhook".to_string(),
            version: "0.1.0".to_string(),
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            transport: PluginTransport::Jsonl,
            description: None,
            exec: ChannelPluginExec {
                command: "/usr/bin/true".to_string(),
                args: vec![],
            },
            platform: Some("webhook".to_string()),
            attachment_sources: vec![],
            ingress: Some(ChannelPluginIngress {
                endpoints: vec![ChannelIngressEndpoint {
                    path: "/webhook/inbound".to_string(),
                    methods: vec!["POST".to_string()],
                    host_managed: true,
                }],
                trust: None,
            }),
            installed_sha256: None,
        };

        assert!(match_channel_ingress_endpoint(&plugin, "POST", "/webhook/inbound").is_some());
        assert!(match_channel_ingress_endpoint(&plugin, "GET", "/webhook/inbound").is_none());
        assert!(match_channel_ingress_endpoint(&plugin, "POST", "/other").is_none());
    }

    #[test]
    fn verify_host_managed_ingress_trust_accepts_matching_shared_secret_header() {
        let _env_guard = env_lock().lock().unwrap();
        let plugin = ChannelPluginManifest {
            name: "channel-telegram".to_string(),
            version: "0.1.0".to_string(),
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            transport: PluginTransport::Jsonl,
            description: None,
            exec: ChannelPluginExec {
                command: "/usr/bin/true".to_string(),
                args: vec![],
            },
            platform: Some("telegram".to_string()),
            attachment_sources: vec![],
            ingress: Some(ChannelPluginIngress {
                endpoints: vec![ChannelIngressEndpoint {
                    path: "/telegram/updates".to_string(),
                    methods: vec!["POST".to_string()],
                    host_managed: true,
                }],
                trust: Some(ChannelIngressTrust {
                    mode: "shared_secret_header".to_string(),
                    header_name: Some("X-Telegram-Bot-Api-Secret-Token".to_string()),
                    secret_name: Some("DISPATCH_TEST_TELEGRAM_WEBHOOK_SECRET".to_string()),
                    host_managed: true,
                }),
            }),
            installed_sha256: None,
        };
        let headers = BTreeMap::from([(
            "x-telegram-bot-api-secret-token".to_string(),
            "expected-secret".to_string(),
        )]);

        unsafe {
            std::env::set_var("DISPATCH_TEST_TELEGRAM_WEBHOOK_SECRET", "expected-secret");
        }
        let verified = verify_host_managed_ingress_trust(&plugin, &headers)
            .expect("host-managed ingress trust should verify");
        unsafe {
            std::env::remove_var("DISPATCH_TEST_TELEGRAM_WEBHOOK_SECRET");
        }

        assert!(verified);
    }

    #[test]
    fn verify_host_managed_ingress_trust_rejects_mismatched_shared_secret_header() {
        let _env_guard = env_lock().lock().unwrap();
        let plugin = ChannelPluginManifest {
            name: "channel-telegram".to_string(),
            version: "0.1.0".to_string(),
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            transport: PluginTransport::Jsonl,
            description: None,
            exec: ChannelPluginExec {
                command: "/usr/bin/true".to_string(),
                args: vec![],
            },
            platform: Some("telegram".to_string()),
            attachment_sources: vec![],
            ingress: Some(ChannelPluginIngress {
                endpoints: vec![ChannelIngressEndpoint {
                    path: "/telegram/updates".to_string(),
                    methods: vec!["POST".to_string()],
                    host_managed: true,
                }],
                trust: Some(ChannelIngressTrust {
                    mode: "shared_secret_header".to_string(),
                    header_name: Some("X-Telegram-Bot-Api-Secret-Token".to_string()),
                    secret_name: Some("DISPATCH_TEST_TELEGRAM_WEBHOOK_SECRET".to_string()),
                    host_managed: true,
                }),
            }),
            installed_sha256: None,
        };
        let headers = BTreeMap::from([(
            "x-telegram-bot-api-secret-token".to_string(),
            "wrong-secret".to_string(),
        )]);

        unsafe {
            std::env::set_var("DISPATCH_TEST_TELEGRAM_WEBHOOK_SECRET", "expected-secret");
        }
        let error = verify_host_managed_ingress_trust(&plugin, &headers)
            .expect_err("mismatched host-managed ingress trust should fail");
        unsafe {
            std::env::remove_var("DISPATCH_TEST_TELEGRAM_WEBHOOK_SECRET");
        }

        assert_eq!(error.status_code, 403);
        assert!(error.message.contains("did not match"));
    }

    fn sample_inbound_event() -> InboundEventEnvelope {
        InboundEventEnvelope {
            event_id: "evt-1".to_string(),
            platform: "telegram".to_string(),
            event_type: "message".to_string(),
            received_at: "2026-04-11T00:00:00Z".to_string(),
            conversation: InboundConversationRef {
                id: "chat-123".to_string(),
                kind: "private".to_string(),
                thread_id: Some("7".to_string()),
                parent_message_id: None,
                workspace_id: None,
                parent_conversation_id: None,
            },
            actor: InboundActor {
                id: "user-1".to_string(),
                display_name: Some("Alice".to_string()),
                username: Some("alice".to_string()),
                is_bot: false,
                metadata: BTreeMap::new(),
            },
            message: InboundMessage {
                id: "1".to_string(),
                content: "hello from telegram".to_string(),
                content_type: "text/plain".to_string(),
                reply_to_message_id: None,
                attachments: Vec::new(),
                metadata: BTreeMap::new(),
            },
            account_id: None,
            activation: None,
            metadata: BTreeMap::new(),
        }
    }
}
