use anyhow::{Context, Result, bail};
use atomic_write_file::AtomicWriteFile;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use chrono::Utc;
use dispatch_core::{
    AttachmentSource, ChannelEventNotification, ChannelPluginManifest, ChannelPluginRequest,
    ChannelPluginResponse, DeliveryReceipt, InboundEventEnvelope, IngressCallbackReply,
    IngressMode, IngressPayload, IngressState, OutboundAttachment, OutboundMessageEnvelope,
    PersistentChannelPluginProcess, PluginNotificationEnvelope, build_channel_reply_envelope,
    call_channel_plugin, call_channel_plugin_with_timeout, call_persistent_channel_plugin,
    channel_event_session_file, default_channel_registry_path, drain_pending_channel_notifications,
    extract_assistant_channel_reply, install_channel_plugin, list_channel_catalog,
    match_channel_ingress_endpoint, recv_persistent_channel_notification,
    render_inbound_event_chat_input, resolve_channel_plugin, resolve_channel_plugin_for_ingress,
    shutdown_persistent_channel_plugin, spawn_persistent_channel_plugin,
    verify_host_managed_ingress_trust,
};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env, fs,
    io::{Read, Write as _},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tiny_http::{Header, Request, Response, Server, StatusCode};
use url::form_urlencoded;

const DISPATCH_MEDIA_ROUTE_PREFIX: &str = "/_dispatch/media/";
const CHANNEL_INGRESS_STATE_DIR: &str = ".dispatch/channel-state";
const CHANNEL_RUNTIME_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CHANNEL_POLL_REQUEST_GRACE: Duration = Duration::from_secs(15);
const CHANNEL_LISTEN_RECV_TIMEOUT: Duration = Duration::from_millis(100);

struct ChannelIngressArgs<'a> {
    name: Option<&'a str>,
    config_json: Option<&'a str>,
    config_file: Option<&'a Path>,
    method: &'a str,
    path: &'a str,
    headers: &'a [String],
    query: &'a [String],
    body: Option<&'a str>,
    body_file: Option<&'a Path>,
    endpoint_id: Option<String>,
    trust_verified: bool,
    received_at: Option<String>,
    registry: Option<&'a Path>,
    emit_json: bool,
}

struct IngressRequestParts<'a> {
    config: Value,
    method: &'a str,
    path: &'a str,
    headers: &'a [String],
    query: &'a [String],
    body: Option<&'a str>,
    body_file: Option<&'a Path>,
    endpoint_id: Option<String>,
    trust_verified: bool,
    received_at: Option<String>,
}

struct ChannelListenArgs<'a> {
    name: &'a str,
    config_json: Option<&'a str>,
    config_file: Option<&'a Path>,
    listen: &'a str,
    parcel: Option<&'a Path>,
    courier: &'a str,
    courier_registry: Option<&'a Path>,
    session_root: Option<&'a Path>,
    tool_approval: Option<crate::CliToolApprovalMode>,
    deliver_replies: bool,
    once: bool,
    emit_json: bool,
    registry: Option<&'a Path>,
}

struct ChannelPollArgs<'a> {
    name: &'a str,
    config_json: Option<&'a str>,
    config_file: Option<&'a Path>,
    interval_ms: Option<u64>,
    parcel: Option<&'a Path>,
    courier: &'a str,
    courier_registry: Option<&'a Path>,
    session_root: Option<&'a Path>,
    tool_approval: Option<crate::CliToolApprovalMode>,
    deliver_replies: bool,
    once: bool,
    emit_json: bool,
    registry: Option<&'a Path>,
}

struct ChannelParcelBridge {
    parcel_path: PathBuf,
    parcel_digest: String,
    courier: String,
    courier_registry: Option<PathBuf>,
    session_root: PathBuf,
    tool_approval: crate::CliToolApprovalMode,
    deliver_replies: bool,
}

struct ActiveChannelRuntimePlugin {
    plugin: ChannelPluginManifest,
    process: PersistentChannelPluginProcess,
    ingress_state: Option<IngressState>,
}

struct ChannelRuntimeEventContext<'a> {
    config: &'a Value,
    ingress_state_path: &'a Path,
    parcel_bridge: Option<&'a ChannelParcelBridge>,
    staged_media: &'a ListenerStagedMedia,
    emit_json: bool,
    source: &'a str,
}

#[derive(Debug, Clone)]
pub(crate) enum ChannelRuntimeMode {
    Listen { listen: String },
    Poll { interval_ms: Option<u64> },
}

#[derive(Debug, Clone)]
pub(crate) struct ChannelRuntimeBindingArgs {
    pub label: String,
    pub plugin: String,
    pub config: Value,
    pub parcel: Option<PathBuf>,
    pub courier: String,
    pub courier_registry: Option<PathBuf>,
    pub session_root: Option<PathBuf>,
    pub tool_approval: Option<crate::CliToolApprovalMode>,
    pub deliver_replies: bool,
    pub once: bool,
    pub emit_json: bool,
    pub registry: Option<PathBuf>,
    pub mode: ChannelRuntimeMode,
}

#[derive(Debug, Clone)]
struct ListenerStagedMedia {
    public_base_url: Option<String>,
    entries: Arc<Mutex<BTreeMap<String, StagedMediaEntry>>>,
}

#[derive(Debug, Clone)]
struct StagedMediaEntry {
    name: String,
    mime_type: String,
    body: Vec<u8>,
}

impl ListenerStagedMedia {
    fn from_config(config: &Value) -> Self {
        Self {
            public_base_url: config
                .get("webhook_public_url")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| value.trim_end_matches('/').to_string()),
            entries: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn stage_attachment(&self, attachment: &OutboundAttachment) -> Result<String> {
        let base_url = self.public_base_url.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "reply attachment `{}` requires URL staging, but config.webhook_public_url is not set",
                attachment.name
            )
        })?;
        let encoded = attachment.data_base64.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "reply attachment `{}` is missing data_base64 for URL staging",
                attachment.name
            )
        })?;
        let body = BASE64_STANDARD.decode(encoded).with_context(|| {
            format!("reply attachment `{}` is not valid base64", attachment.name)
        })?;

        let mut digest = Sha256::new();
        digest.update(attachment.name.as_bytes());
        digest.update([0]);
        digest.update(attachment.mime_type.as_bytes());
        digest.update([0]);
        digest.update(&body);
        let media_id = hex_encode(digest.finalize().as_slice());

        let mut entries = self
            .entries
            .lock()
            .map_err(|_| anyhow::anyhow!("staged media store is unavailable"))?;
        entries
            .entry(media_id.clone())
            .or_insert_with(|| StagedMediaEntry {
                name: attachment.name.clone(),
                mime_type: attachment.mime_type.clone(),
                body,
            });

        Ok(format!("{base_url}{DISPATCH_MEDIA_ROUTE_PREFIX}{media_id}"))
    }

    fn lookup(&self, media_id: &str) -> Result<Option<StagedMediaEntry>> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| anyhow::anyhow!("staged media store is unavailable"))?;
        Ok(entries.get(media_id).cloned())
    }
}

pub(crate) fn channel_command(command: crate::ChannelCommand) -> Result<()> {
    match command {
        crate::ChannelCommand::Ls { json, registry } => channel_ls(registry.as_deref(), json),
        crate::ChannelCommand::Inspect {
            name,
            json,
            registry,
        } => channel_inspect(&name, registry.as_deref(), json),
        crate::ChannelCommand::Install { manifest, registry } => {
            channel_install(&manifest, registry.as_deref())
        }
        crate::ChannelCommand::Call {
            name,
            request_json,
            request_file,
            json,
            registry,
        } => channel_call(
            &name,
            request_json.as_deref(),
            request_file.as_deref(),
            registry.as_deref(),
            json,
        ),
        crate::ChannelCommand::Ingress {
            name,
            config_json,
            config_file,
            method,
            path,
            headers,
            query,
            body,
            body_file,
            endpoint_id,
            trust_verified,
            received_at,
            json,
            registry,
        } => channel_ingress(ChannelIngressArgs {
            name: name.as_deref(),
            config_json: config_json.as_deref(),
            config_file: config_file.as_deref(),
            method: &method,
            path: &path,
            headers: &headers,
            query: &query,
            body: body.as_deref(),
            body_file: body_file.as_deref(),
            endpoint_id,
            trust_verified,
            received_at,
            registry: registry.as_deref(),
            emit_json: json,
        }),
        crate::ChannelCommand::Poll {
            name,
            config_json,
            config_file,
            interval_ms,
            parcel,
            courier,
            courier_registry,
            session_root,
            tool_approval,
            deliver_replies,
            once,
            json,
            registry,
        } => channel_poll(ChannelPollArgs {
            name: &name,
            config_json: config_json.as_deref(),
            config_file: config_file.as_deref(),
            interval_ms,
            parcel: parcel.as_deref(),
            courier: &courier,
            courier_registry: courier_registry.as_deref(),
            session_root: session_root.as_deref(),
            tool_approval,
            deliver_replies,
            once,
            emit_json: json,
            registry: registry.as_deref(),
        }),
        crate::ChannelCommand::Listen {
            name,
            config_json,
            config_file,
            listen,
            parcel,
            courier,
            courier_registry,
            session_root,
            tool_approval,
            deliver_replies,
            once,
            json,
            registry,
        } => channel_listen(ChannelListenArgs {
            name: &name,
            config_json: config_json.as_deref(),
            config_file: config_file.as_deref(),
            listen: &listen,
            parcel: parcel.as_deref(),
            courier: &courier,
            courier_registry: courier_registry.as_deref(),
            session_root: session_root.as_deref(),
            tool_approval,
            deliver_replies,
            once,
            emit_json: json,
            registry: registry.as_deref(),
        }),
    }
}

fn channel_ls(registry: Option<&Path>, emit_json: bool) -> Result<()> {
    let catalog = list_channel_catalog(registry)?;
    if emit_json {
        println!("{}", serde_json::to_string_pretty(&catalog)?);
        return Ok(());
    }

    if catalog.is_empty() {
        println!("No channel plugins installed.");
        println!("Install one with: dispatch channel install <manifest.json>");
        return Ok(());
    }

    for entry in catalog {
        let platform = entry.platform.as_deref().unwrap_or("-");
        let ingress = if entry.ingress_paths.is_empty() {
            "-".to_string()
        } else {
            entry.ingress_paths.join(",")
        };
        println!(
            "{}\t{}\tprotocol-v{}/{:?}\t{}\t{}",
            entry.name, platform, entry.protocol_version, entry.transport, ingress, entry.command
        );
    }

    Ok(())
}

fn channel_inspect(name: &str, registry: Option<&Path>, emit_json: bool) -> Result<()> {
    let plugin = resolve_channel_plugin(name, registry)?;
    let call_timeout = Duration::from_secs(30);
    if emit_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&ChannelInspectView {
                plugin: &plugin,
                call_timeout_ms: call_timeout.as_millis(),
                call_timeout_display: format_duration_literal(call_timeout),
            })?
        );
    } else {
        print_channel_plugin_manifest(&plugin, call_timeout);
    }
    Ok(())
}

fn channel_install(manifest: &Path, registry: Option<&Path>) -> Result<()> {
    let installed = install_channel_plugin(manifest, registry)?;
    let registry_path = registry
        .map(PathBuf::from)
        .or_else(|| default_channel_registry_path().ok())
        .unwrap_or_else(|| PathBuf::from("<unknown>"));

    println!("Installed channel plugin `{}`", installed.name);
    println!("Registry: {}", registry_path.display());
    Ok(())
}

fn channel_call(
    name: &str,
    request_json: Option<&str>,
    request_file: Option<&Path>,
    registry: Option<&Path>,
    emit_json: bool,
) -> Result<()> {
    let plugin = resolve_channel_plugin(name, registry)?;
    let request = load_request(request_json, request_file)?;
    let response = call_channel_plugin(&plugin, request)?;

    print_channel_response(response, emit_json)
}

fn channel_ingress(args: ChannelIngressArgs<'_>) -> Result<()> {
    let plugin = match args.name {
        Some(name) => resolve_channel_plugin(name, args.registry)?,
        None => resolve_channel_plugin_for_ingress(args.method, args.path, args.registry)?,
    };
    let config = load_structured_value(args.config_json, args.config_file, "channel config")?;
    let request = build_ingress_request(IngressRequestParts {
        config,
        method: args.method,
        path: args.path,
        headers: args.headers,
        query: args.query,
        body: args.body,
        body_file: args.body_file,
        endpoint_id: args.endpoint_id,
        trust_verified: args.trust_verified,
        received_at: args.received_at,
    })?;
    let response = call_channel_plugin(&plugin, request)?;

    if args.name.is_none() && !args.emit_json {
        println!("Resolved Plugin: {}", plugin.name);
    }

    print_channel_response(response, args.emit_json)
}

fn channel_poll(args: ChannelPollArgs<'_>) -> Result<()> {
    run_channel_runtime_binding(ChannelRuntimeBindingArgs {
        label: args.name.to_string(),
        plugin: args.name.to_string(),
        config: load_structured_value(args.config_json, args.config_file, "channel config")?,
        parcel: args.parcel.map(PathBuf::from),
        courier: args.courier.to_string(),
        courier_registry: args.courier_registry.map(PathBuf::from),
        session_root: args.session_root.map(PathBuf::from),
        tool_approval: args.tool_approval,
        deliver_replies: args.deliver_replies,
        once: args.once,
        emit_json: args.emit_json,
        registry: args.registry.map(PathBuf::from),
        mode: ChannelRuntimeMode::Poll {
            interval_ms: args.interval_ms,
        },
    })
}

fn channel_listen(args: ChannelListenArgs<'_>) -> Result<()> {
    run_channel_runtime_binding(ChannelRuntimeBindingArgs {
        label: args.name.to_string(),
        plugin: args.name.to_string(),
        config: load_structured_value(args.config_json, args.config_file, "channel config")?,
        parcel: args.parcel.map(PathBuf::from),
        courier: args.courier.to_string(),
        courier_registry: args.courier_registry.map(PathBuf::from),
        session_root: args.session_root.map(PathBuf::from),
        tool_approval: args.tool_approval,
        deliver_replies: args.deliver_replies,
        once: args.once,
        emit_json: args.emit_json,
        registry: args.registry.map(PathBuf::from),
        mode: ChannelRuntimeMode::Listen {
            listen: args.listen.to_string(),
        },
    })
}

pub(crate) fn run_channel_runtime_binding(args: ChannelRuntimeBindingArgs) -> Result<()> {
    let plugin = resolve_channel_plugin(&args.plugin, args.registry.as_deref())?;
    let parcel_bridge = prepare_channel_parcel_bridge(
        args.parcel.as_deref(),
        &args.courier,
        args.courier_registry.as_deref(),
        args.session_root.as_deref(),
        args.tool_approval,
        args.deliver_replies,
    )?;
    let ingress_state_path =
        default_channel_ingress_state_path(&args.label, &plugin.name, &args.config)?;
    let restored_state = load_channel_ingress_state(&ingress_state_path)?;

    if matches!(args.mode, ChannelRuntimeMode::Poll { .. }) && args.once {
        return run_channel_poll_once(
            &plugin,
            &args.config,
            restored_state,
            &ingress_state_path,
            parcel_bridge.as_ref(),
            args.emit_json,
        );
    }

    let mut runtime = start_channel_runtime_plugin(&plugin, &args.config, restored_state)?;
    if let Some(state) = &runtime.ingress_state {
        save_channel_ingress_state(&ingress_state_path, state)?;
    }

    match args.mode {
        ChannelRuntimeMode::Poll { .. } => {
            let staged_media = ListenerStagedMedia::from_config(&Value::Null);
            let event_context = ChannelRuntimeEventContext {
                config: &args.config,
                ingress_state_path: &ingress_state_path,
                parcel_bridge: parcel_bridge.as_ref(),
                staged_media: &staged_media,
                emit_json: args.emit_json,
                source: "Poll",
            };
            if !matches!(
                runtime.ingress_state.as_ref().map(|state| &state.mode),
                Some(IngressMode::Polling)
            ) {
                let stop_result = stop_channel_runtime_plugin(&mut runtime, &args.config);
                let mode_name = runtime
                    .ingress_state
                    .as_ref()
                    .map(|state| format!("{:?}", state.mode))
                    .unwrap_or_else(|| "<unknown>".to_string());
                let run_error = anyhow::anyhow!(
                    "channel plugin `{}` started ingress in {} mode; use listen bindings for webhook-style ingress",
                    plugin.name,
                    mode_name
                );
                return match stop_result {
                    Ok(()) => Err(run_error),
                    Err(stop_error) => Err(run_error
                        .context(format!("also failed to stop ingress cleanly: {stop_error}"))),
                };
            }

            println!("Polling {}", plugin.name);
            if let Some(parcel_bridge) = &parcel_bridge {
                println!(
                    "Parcel bridge: {} via {} (sessions under {})",
                    parcel_bridge.parcel_path.display(),
                    parcel_bridge.courier,
                    parcel_bridge.session_root.display()
                );
            }

            let run_result = (|| -> Result<()> {
                loop {
                    let Some(notification) = recv_persistent_channel_notification(
                        &mut runtime.process,
                        &runtime.plugin,
                        None,
                    )?
                    else {
                        bail!(
                            "channel plugin `{}` closed its ingress notification stream",
                            runtime.plugin.name
                        );
                    };

                    let _handled = handle_channel_runtime_notification(
                        &mut runtime,
                        notification,
                        &event_context,
                    )?;
                    let _handled_pending =
                        process_pending_channel_notifications(&mut runtime, &event_context)?;
                    if args.once {
                        break;
                    }
                }

                Ok(())
            })();

            let stop_result = stop_channel_runtime_plugin(&mut runtime, &args.config);
            match (run_result, stop_result) {
                (Ok(()), Ok(())) => Ok(()),
                (Err(error), Ok(())) => Err(error),
                (Ok(()), Err(error)) => Err(error),
                (Err(run_error), Err(stop_error)) => {
                    Err(run_error
                        .context(format!("also failed to stop ingress cleanly: {stop_error}")))
                }
            }
        }
        ChannelRuntimeMode::Listen { listen } => {
            let staged_media = ListenerStagedMedia::from_config(&args.config);
            let event_context = ChannelRuntimeEventContext {
                config: &args.config,
                ingress_state_path: &ingress_state_path,
                parcel_bridge: parcel_bridge.as_ref(),
                staged_media: &staged_media,
                emit_json: args.emit_json,
                source: "Ingress",
            };
            let server = Server::http(&listen)
                .map_err(|error| anyhow::anyhow!("failed to bind {listen}: {error}"))?;
            if matches!(
                runtime.ingress_state.as_ref().map(|state| &state.mode),
                Some(IngressMode::Polling)
            ) {
                let stop_result = stop_channel_runtime_plugin(&mut runtime, &args.config);
                let run_error = anyhow::anyhow!(
                    "channel plugin `{}` started ingress in polling mode; use poll bindings instead of listen bindings",
                    plugin.name
                );
                return match stop_result {
                    Ok(()) => Err(run_error),
                    Err(stop_error) => Err(run_error
                        .context(format!("also failed to stop ingress cleanly: {stop_error}"))),
                };
            }

            println!("Listening on {}", server.server_addr());
            if let Some(parcel_bridge) = &parcel_bridge {
                println!(
                    "Parcel bridge: {} via {} (sessions under {})",
                    parcel_bridge.parcel_path.display(),
                    parcel_bridge.courier,
                    parcel_bridge.session_root.display()
                );
            }

            let run_result = (|| -> Result<()> {
                loop {
                    let handled_notifications =
                        process_pending_channel_notifications(&mut runtime, &event_context)?;
                    if args.once && handled_notifications > 0 {
                        break;
                    }

                    let Some(request) = server
                        .recv_timeout(CHANNEL_LISTEN_RECV_TIMEOUT)
                        .context("failed to accept connection")?
                    else {
                        continue;
                    };

                    let handled_request =
                        handle_channel_listener_connection(&mut runtime, request, &event_context)?;
                    if args.once && handled_request {
                        break;
                    }

                    let handled_pending =
                        process_pending_channel_notifications(&mut runtime, &event_context)?;
                    if args.once && handled_pending > 0 {
                        break;
                    }
                }
                Ok(())
            })();

            let stop_result = stop_channel_runtime_plugin(&mut runtime, &args.config);
            match (run_result, stop_result) {
                (Ok(()), Ok(())) => Ok(()),
                (Err(error), Ok(())) => Err(error),
                (Ok(()), Err(error)) => Err(error),
                (Err(run_error), Err(stop_error)) => {
                    Err(run_error
                        .context(format!("also failed to stop ingress cleanly: {stop_error}")))
                }
            }
        }
    }
}

fn run_channel_poll_once(
    plugin: &ChannelPluginManifest,
    config: &Value,
    restored_state: Option<IngressState>,
    ingress_state_path: &Path,
    parcel_bridge: Option<&ChannelParcelBridge>,
    emit_json: bool,
) -> Result<()> {
    let staged_media = ListenerStagedMedia::from_config(&Value::Null);
    let timeout = poll_request_timeout(config);
    let response = call_channel_plugin_with_timeout(
        plugin,
        ChannelPluginRequest::PollIngress {
            config: config.clone(),
            state: restored_state,
        },
        timeout,
    )?;

    match response {
        ChannelPluginResponse::IngressEventsReceived {
            events,
            callback_reply: _,
            state,
            poll_after_ms,
        } => {
            if let Some(state) = &state {
                save_channel_ingress_state(ingress_state_path, state)?;
            }
            let parcel_runs =
                execute_channel_parcel_runs(plugin, parcel_bridge, &events, |event, reply| {
                    deliver_channel_reply_once(plugin, event, config, &staged_media, reply)
                })?;

            if emit_json {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "plugin": plugin.name,
                        "events": events,
                        "parcel_runs": parcel_runs,
                        "state": state,
                        "poll_after_ms": poll_after_ms,
                    }))?
                );
            } else {
                println!("Poll {} -> {} event(s)", plugin.name, events.len());
                for parcel_run in &parcel_runs {
                    println!(
                        "Parcel {} -> {}",
                        parcel_run.event_id,
                        parcel_run.session_file.display()
                    );
                    if let Some(delivery) = &parcel_run.delivery {
                        println!(
                            "Delivered reply: {} -> {}",
                            delivery.message_id, delivery.conversation_id
                        );
                    }
                    if !parcel_run.output.is_empty() {
                        print!("{}", parcel_run.output);
                    }
                }
            }
            Ok(())
        }
        ChannelPluginResponse::Error { error } => bail!(
            "channel plugin error {} while polling ingress: {}",
            error.code,
            error.message
        ),
        other => bail!(
            "channel plugin returned unexpected response variant for poll_ingress: {}",
            response_kind(&other)
        ),
    }
}

fn print_channel_response(response: ChannelPluginResponse, emit_json: bool) -> Result<()> {
    if emit_json {
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }

    match response {
        ChannelPluginResponse::Capabilities { capabilities } => {
            println!("Plugin ID: {}", capabilities.plugin_id);
            println!("Platform: {}", capabilities.platform);
            let modes: Vec<String> = capabilities
                .ingress_modes
                .iter()
                .map(|m| format!("{m:?}"))
                .collect();
            println!("Ingress Modes: {}", modes.join(", "));
            println!(
                "Outbound Types: {}",
                capabilities.outbound_message_types.join(", ")
            );
            println!("Threading: {:?}", capabilities.threading_model);
            println!("Accepts Push: {}", capabilities.accepts_push);
            println!("Accepts Status: {}", capabilities.accepts_status_frames);
        }
        ChannelPluginResponse::Configured { configuration } => {
            println!("{}", serde_json::to_string_pretty(&configuration)?);
        }
        ChannelPluginResponse::Health { health } => {
            println!("OK: {}", health.ok);
            println!("Status: {}", health.status);
            if let Some(account_id) = health.account_id {
                println!("Account ID: {account_id}");
            }
            if let Some(display_name) = health.display_name {
                println!("Display Name: {display_name}");
            }
            if !health.metadata.is_empty() {
                println!("Metadata:");
                println!("{}", serde_json::to_string_pretty(&health.metadata)?);
            }
        }
        ChannelPluginResponse::IngressStarted { state }
        | ChannelPluginResponse::IngressStopped { state } => {
            println!("{}", serde_json::to_string_pretty(&state)?);
        }
        ChannelPluginResponse::IngressEventsReceived {
            events,
            callback_reply,
            state,
            poll_after_ms,
        } => {
            println!("Events: {}", events.len());
            if let Some(callback_reply) = callback_reply {
                println!("Callback Reply:");
                println!("{}", serde_json::to_string_pretty(&callback_reply)?);
            }
            if let Some(state) = state {
                println!("Ingress State:");
                println!("{}", serde_json::to_string_pretty(&state)?);
            }
            if let Some(poll_after_ms) = poll_after_ms {
                println!("Poll After: {poll_after_ms}ms");
            }
            if !events.is_empty() {
                println!("{}", serde_json::to_string_pretty(&events)?);
            }
        }
        ChannelPluginResponse::Delivered { delivery }
        | ChannelPluginResponse::Pushed { delivery } => {
            println!("Message ID: {}", delivery.message_id);
            println!("Conversation ID: {}", delivery.conversation_id);
            if !delivery.metadata.is_empty() {
                println!("Metadata:");
                println!("{}", serde_json::to_string_pretty(&delivery.metadata)?);
            }
        }
        ChannelPluginResponse::StatusAccepted { status } => {
            println!("Accepted: {}", status.accepted);
            if !status.metadata.is_empty() {
                println!("Metadata:");
                println!("{}", serde_json::to_string_pretty(&status.metadata)?);
            }
        }
        ChannelPluginResponse::Ok => {
            println!("OK");
        }
        ChannelPluginResponse::Error { error } => {
            bail!("{}: {}", error.code, error.message);
        }
    }

    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedHttpRequest {
    method: String,
    target: String,
    path: String,
    query: Option<String>,
    headers: BTreeMap<String, String>,
    body: Option<String>,
}

fn load_request(
    request_json: Option<&str>,
    request_file: Option<&Path>,
) -> Result<ChannelPluginRequest> {
    match (request_json, request_file) {
        (Some(_), Some(_)) => bail!("use either --request-json or --request-file, not both"),
        (None, None) => bail!("channel call requires --request-json or --request-file"),
        (Some(request_json), None) => serde_json::from_str(request_json)
            .context("failed to parse --request-json as a channel request"),
        (None, Some(request_file)) => {
            let body = fs::read_to_string(request_file)
                .with_context(|| format!("failed to read {}", request_file.display()))?;
            serde_json::from_str(&body)
                .with_context(|| format!("failed to parse {}", request_file.display()))
        }
    }
}

fn load_structured_value(
    value_json: Option<&str>,
    value_file: Option<&Path>,
    description: &str,
) -> Result<Value> {
    match (value_json, value_file) {
        (Some(_), Some(_)) => bail!("use either inline JSON or a file for {description}, not both"),
        (None, None) => Ok(json!({})),
        (Some(value_json), None) => serde_json::from_str(value_json)
            .with_context(|| format!("failed to parse {description} JSON")),
        (None, Some(value_file)) => load_structured_value_file(value_file, description),
    }
}

pub(crate) fn load_structured_value_file(value_file: &Path, description: &str) -> Result<Value> {
    let body = fs::read_to_string(value_file)
        .with_context(|| format!("failed to read {}", value_file.display()))?;
    match value_file.extension().and_then(|value| value.to_str()) {
        Some("toml") => {
            let value = toml::from_str::<toml::Value>(&body)
                .with_context(|| format!("failed to parse {} as TOML", value_file.display()))?;
            serde_json::to_value(value).with_context(|| {
                format!(
                    "failed to convert TOML {} into JSON-compatible {description}",
                    value_file.display()
                )
            })
        }
        Some("json") => serde_json::from_str(&body)
            .with_context(|| format!("failed to parse {}", value_file.display())),
        _ => serde_json::from_str(&body)
            .or_else(|_| {
                toml::from_str::<toml::Value>(&body)
                    .context("TOML parse fallback failed")
                    .and_then(|value| {
                        serde_json::to_value(value)
                            .context("failed to convert TOML into JSON-compatible config")
                    })
            })
            .with_context(|| {
                format!(
                    "failed to parse {} as JSON or TOML for {description}",
                    value_file.display()
                )
            }),
    }
}

fn build_ingress_request(parts: IngressRequestParts<'_>) -> Result<ChannelPluginRequest> {
    let headers = parse_key_value_pairs(parts.headers, "header")?;
    let query = parse_key_value_pairs(parts.query, "query")?;
    let raw_query = (!parts.query.is_empty()).then(|| parts.query.join("&"));
    let body = load_body(parts.body, parts.body_file)?;

    Ok(ChannelPluginRequest::IngressEvent {
        config: parts.config,
        state: None,
        payload: IngressPayload {
            endpoint_id: parts.endpoint_id,
            method: parts.method.to_string(),
            path: parts.path.to_string(),
            headers,
            query,
            raw_query,
            body,
            trust_verified: parts.trust_verified,
            received_at: parts.received_at,
        },
    })
}

fn load_body(body: Option<&str>, body_file: Option<&Path>) -> Result<String> {
    match (body, body_file) {
        (Some(_), Some(_)) => bail!("use either --body or --body-file, not both"),
        (None, None) => Ok(String::new()),
        (Some(body), None) => Ok(body.to_string()),
        (None, Some(body_file)) => fs::read_to_string(body_file)
            .with_context(|| format!("failed to read {}", body_file.display())),
    }
}

fn parse_key_value_pairs(entries: &[String], field_name: &str) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for entry in entries {
        let Some((name, value)) = entry.split_once('=') else {
            bail!("{field_name} entry `{entry}` must use NAME=VALUE");
        };
        let name = name.trim();
        if name.is_empty() {
            bail!("{field_name} entry `{entry}` must have a non-empty name");
        }
        values.insert(name.to_string(), value.to_string());
    }
    Ok(values)
}

fn handle_channel_listener_connection(
    runtime: &mut ActiveChannelRuntimePlugin,
    mut request: Request,
    event_context: &ChannelRuntimeEventContext<'_>,
) -> Result<bool> {
    if let Some(media_id) = staged_media_request_id(request.url()).map(str::to_string) {
        serve_staged_media_request(request, event_context.staged_media, &media_id)?;
        return Ok(false);
    }

    let remote_addr = request
        .remote_addr()
        .map(ToString::to_string)
        .unwrap_or_else(|| "<unknown>".to_string());
    let parsed = match parse_http_request(&mut request, 1024 * 1024) {
        Ok(request) => request,
        Err(error) => {
            respond_http_request(
                request,
                400,
                Some("text/plain; charset=utf-8"),
                &format!("invalid request: {error}\n"),
            )?;
            return Ok(true);
        }
    };

    let matched_endpoint =
        match match_channel_ingress_endpoint(&runtime.plugin, &parsed.method, &parsed.path) {
            Some(endpoint) => endpoint,
            None => {
                respond_http_request(
                    request,
                    404,
                    Some("text/plain; charset=utf-8"),
                    "request did not match an installed ingress endpoint\n",
                )?;
                return Ok(true);
            }
        };
    let trust_verified = match verify_host_managed_ingress_trust(&runtime.plugin, &parsed.headers) {
        Ok(verified) => verified,
        Err(error) => {
            respond_http_request(
                request,
                error.status_code,
                Some("text/plain; charset=utf-8"),
                &format!("{}\n", error.message),
            )?;
            return Ok(true);
        }
    };

    let response = call_persistent_channel_plugin(
        &mut runtime.process,
        &runtime.plugin,
        ChannelPluginRequest::IngressEvent {
            config: event_context.config.clone(),
            state: runtime.ingress_state.clone(),
            payload: IngressPayload {
                endpoint_id: Some(format!("{}:{}", runtime.plugin.name, matched_endpoint.path)),
                method: parsed.method.clone(),
                path: parsed.path.clone(),
                headers: parsed.headers.clone(),
                query: parse_query_string(parsed.query.as_deref()),
                raw_query: parsed.query.clone(),
                body: parsed.body.clone().unwrap_or_default(),
                trust_verified,
                received_at: Some(Utc::now().to_rfc3339()),
            },
        },
        CHANNEL_RUNTIME_REQUEST_TIMEOUT,
    );

    match response {
        Ok(ChannelPluginResponse::IngressEventsReceived {
            events,
            callback_reply,
            state,
            ..
        }) => {
            if let Some(state) = state {
                save_channel_ingress_state(event_context.ingress_state_path, &state)?;
                runtime.ingress_state = Some(state);
            }
            let _handled_events = emit_channel_runtime_events(
                runtime,
                event_context,
                events,
                None,
                Some(format!(
                    "Ingress {} {} from {}",
                    parsed.method, parsed.path, remote_addr
                )),
            )?;

            let reply = callback_reply.unwrap_or(IngressCallbackReply {
                status: 200,
                content_type: Some("text/plain; charset=utf-8".to_string()),
                body: String::new(),
            });
            respond_http_request(
                request,
                reply.status,
                reply.content_type.as_deref(),
                &reply.body,
            )?;
            Ok(true)
        }
        Ok(ChannelPluginResponse::Error { error }) => {
            respond_http_request(
                request,
                502,
                Some("text/plain; charset=utf-8"),
                &format!("channel plugin error {}: {}\n", error.code, error.message),
            )?;
            Ok(true)
        }
        Ok(other) => {
            respond_http_request(
                request,
                502,
                Some("text/plain; charset=utf-8"),
                &format!(
                    "channel plugin returned unexpected response variant for ingress: {:?}\n",
                    response_kind(&other)
                ),
            )?;
            Ok(true)
        }
        Err(error) => {
            respond_http_request(
                request,
                502,
                Some("text/plain; charset=utf-8"),
                &format!("failed to call channel plugin: {error}\n"),
            )?;
            Ok(true)
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct ChannelParcelRunResult {
    event_id: String,
    session_file: PathBuf,
    output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    delivery: Option<DeliveryReceipt>,
}

fn prepare_channel_parcel_bridge(
    parcel: Option<&Path>,
    courier: &str,
    courier_registry: Option<&Path>,
    session_root: Option<&Path>,
    tool_approval: Option<crate::CliToolApprovalMode>,
    deliver_replies: bool,
) -> Result<Option<ChannelParcelBridge>> {
    if deliver_replies && parcel.is_none() {
        bail!("reply delivery requires a parcel; set `--parcel` before using `--deliver-replies`");
    }

    let Some(parcel) = parcel else {
        return Ok(None);
    };

    let loaded = crate::run::load_or_build_parcel_for_run(parcel.to_path_buf())?;
    let session_root = match session_root {
        Some(root) => root.to_path_buf(),
        None => env::current_dir()
            .context("failed to resolve current working directory")?
            .join(".dispatch/channel-sessions"),
    };

    Ok(Some(ChannelParcelBridge {
        parcel_path: loaded.parcel_dir.clone(),
        parcel_digest: loaded.config.digest.clone(),
        courier: courier.to_string(),
        courier_registry: courier_registry.map(PathBuf::from),
        session_root,
        tool_approval: crate::resolve_noninteractive_tool_approval_mode(tool_approval),
        deliver_replies,
    }))
}

fn start_channel_runtime_plugin(
    plugin: &ChannelPluginManifest,
    config: &Value,
    restored_state: Option<IngressState>,
) -> Result<ActiveChannelRuntimePlugin> {
    let mut process = spawn_persistent_channel_plugin(plugin)?;
    let response = call_persistent_channel_plugin(
        &mut process,
        plugin,
        ChannelPluginRequest::StartIngress {
            config: config.clone(),
            state: restored_state,
        },
        CHANNEL_RUNTIME_REQUEST_TIMEOUT,
    )?;
    match response {
        ChannelPluginResponse::IngressStarted { state } => Ok(ActiveChannelRuntimePlugin {
            plugin: plugin.clone(),
            process,
            ingress_state: Some(state),
        }),
        ChannelPluginResponse::Error { error } => bail!(
            "channel plugin error {} while starting ingress: {}",
            error.code,
            error.message
        ),
        other => bail!(
            "channel plugin returned unexpected response variant for start_ingress: {}",
            response_kind(&other)
        ),
    }
}

fn stop_channel_runtime_plugin(
    runtime: &mut ActiveChannelRuntimePlugin,
    config: &Value,
) -> Result<()> {
    let stop_result = (|| -> Result<()> {
        let response = call_persistent_channel_plugin(
            &mut runtime.process,
            &runtime.plugin,
            ChannelPluginRequest::StopIngress {
                config: config.clone(),
                state: runtime.ingress_state.clone(),
            },
            CHANNEL_RUNTIME_REQUEST_TIMEOUT,
        )?;
        match response {
            ChannelPluginResponse::IngressStopped { state } => {
                runtime.ingress_state = Some(state);
                Ok(())
            }
            ChannelPluginResponse::Error { error } => bail!(
                "channel plugin error {} while stopping ingress: {}",
                error.code,
                error.message
            ),
            other => bail!(
                "channel plugin returned unexpected response variant for stop_ingress: {}",
                response_kind(&other)
            ),
        }
    })();
    let shutdown_result = shutdown_persistent_channel_plugin(&mut runtime.process, &runtime.plugin);
    match (stop_result, shutdown_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(anyhow::anyhow!(error)),
        (Err(stop_error), Err(shutdown_error)) => Err(stop_error.context(format!(
            "also failed to shut down channel plugin cleanly: {shutdown_error}"
        ))),
    }
}

fn process_pending_channel_notifications(
    runtime: &mut ActiveChannelRuntimePlugin,
    event_context: &ChannelRuntimeEventContext<'_>,
) -> Result<usize> {
    let mut handled_events = 0;
    for notification in drain_pending_channel_notifications(&mut runtime.process) {
        handled_events +=
            handle_channel_runtime_notification(runtime, notification, event_context)?;
    }
    Ok(handled_events)
}

fn handle_channel_runtime_notification(
    runtime: &mut ActiveChannelRuntimePlugin,
    notification: PluginNotificationEnvelope<ChannelEventNotification>,
    event_context: &ChannelRuntimeEventContext<'_>,
) -> Result<usize> {
    if let Some(state) = notification.notification.state {
        save_channel_ingress_state(event_context.ingress_state_path, &state)?;
        runtime.ingress_state = Some(state);
    }
    emit_channel_runtime_events(
        runtime,
        event_context,
        notification.notification.events,
        notification.notification.poll_after_ms,
        Some(event_context.source.to_string()),
    )
}

fn emit_channel_runtime_events(
    runtime: &mut ActiveChannelRuntimePlugin,
    event_context: &ChannelRuntimeEventContext<'_>,
    events: Vec<InboundEventEnvelope>,
    poll_after_ms: Option<u64>,
    source: Option<String>,
) -> Result<usize> {
    let plugin = runtime.plugin.clone();
    let parcel_runs = execute_channel_parcel_runs(
        &plugin,
        event_context.parcel_bridge,
        &events,
        |event, reply| {
            deliver_channel_reply(
                runtime,
                event,
                event_context.config,
                event_context.staged_media,
                reply,
            )
        },
    )?;
    if event_context.emit_json {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "plugin": plugin.name,
                "events": events,
                "parcel_runs": parcel_runs,
                "state": runtime.ingress_state,
                "poll_after_ms": poll_after_ms,
            }))?
        );
    } else {
        let label = source.unwrap_or_else(|| "Ingress".to_string());
        println!("{label} {} -> {} event(s)", plugin.name, events.len());
        for parcel_run in &parcel_runs {
            println!(
                "Parcel {} -> {}",
                parcel_run.event_id,
                parcel_run.session_file.display()
            );
            if let Some(delivery) = &parcel_run.delivery {
                println!(
                    "Delivered reply: {} -> {}",
                    delivery.message_id, delivery.conversation_id
                );
            }
            if !parcel_run.output.is_empty() {
                print!("{}", parcel_run.output);
            }
        }
    }
    Ok(events.len())
}

fn execute_channel_parcel_runs(
    plugin: &ChannelPluginManifest,
    parcel_bridge: Option<&ChannelParcelBridge>,
    events: &[InboundEventEnvelope],
    mut deliver_reply: impl FnMut(
        &InboundEventEnvelope,
        OutboundMessageEnvelope,
    ) -> Result<Option<DeliveryReceipt>>,
) -> Result<Vec<ChannelParcelRunResult>> {
    let Some(parcel_bridge) = parcel_bridge else {
        return Ok(Vec::new());
    };

    let mut results = Vec::with_capacity(events.len());
    for event in events {
        let session_file = channel_event_session_file(
            &parcel_bridge.session_root,
            &plugin.name,
            &parcel_bridge.parcel_digest,
            event,
        );
        let input = render_inbound_event_chat_input(&plugin.name, event)?;
        let response = crate::run::execute_chat_turn(
            parcel_bridge.parcel_path.clone(),
            parcel_bridge.courier.clone(),
            parcel_bridge.courier_registry.clone(),
            Some(session_file.clone()),
            input,
            parcel_bridge.tool_approval,
            crate::CliA2aPolicy::default(),
        )?;
        let mut output = Vec::new();
        crate::run::print_courier_events(&mut output, &response.events)?;
        let delivery = if parcel_bridge.deliver_replies {
            if let Some(reply) = extract_assistant_channel_reply(&response.events) {
                deliver_reply(event, reply)?
            } else {
                None
            }
        } else {
            None
        };
        results.push(ChannelParcelRunResult {
            event_id: event.event_id.clone(),
            session_file,
            output: String::from_utf8_lossy(&output).into_owned(),
            delivery,
        });
    }
    Ok(results)
}

fn deliver_channel_reply(
    runtime: &mut ActiveChannelRuntimePlugin,
    event: &InboundEventEnvelope,
    config: &Value,
    staged_media: &ListenerStagedMedia,
    reply: OutboundMessageEnvelope,
) -> Result<Option<DeliveryReceipt>> {
    let reply = rewrite_reply_attachments_for_channel(&runtime.plugin, staged_media, reply)?;
    let message = serde_json::to_value(build_channel_reply_envelope(event, reply))
        .context("failed to serialize channel reply envelope")?;

    let response = call_persistent_channel_plugin(
        &mut runtime.process,
        &runtime.plugin,
        ChannelPluginRequest::Deliver {
            config: config.clone(),
            message,
        },
        CHANNEL_RUNTIME_REQUEST_TIMEOUT,
    )?;
    match response {
        ChannelPluginResponse::Delivered { delivery } => Ok(Some(delivery)),
        ChannelPluginResponse::Error { error } => bail!(
            "channel plugin error {} while delivering reply: {}",
            error.code,
            error.message
        ),
        other => bail!(
            "channel plugin returned unexpected response variant for delivery: {}",
            response_kind(&other)
        ),
    }
}

fn deliver_channel_reply_once(
    plugin: &ChannelPluginManifest,
    event: &InboundEventEnvelope,
    config: &Value,
    staged_media: &ListenerStagedMedia,
    reply: OutboundMessageEnvelope,
) -> Result<Option<DeliveryReceipt>> {
    let reply = rewrite_reply_attachments_for_channel(plugin, staged_media, reply)?;
    let message = serde_json::to_value(build_channel_reply_envelope(event, reply))
        .context("failed to serialize channel reply envelope")?;

    let response = call_channel_plugin_with_timeout(
        plugin,
        ChannelPluginRequest::Deliver {
            config: config.clone(),
            message,
        },
        CHANNEL_RUNTIME_REQUEST_TIMEOUT,
    )?;
    match response {
        ChannelPluginResponse::Delivered { delivery } => Ok(Some(delivery)),
        ChannelPluginResponse::Error { error } => bail!(
            "channel plugin error {} while delivering reply: {}",
            error.code,
            error.message
        ),
        other => bail!(
            "channel plugin returned unexpected response variant for delivery: {}",
            response_kind(&other)
        ),
    }
}

fn rewrite_reply_attachments_for_channel(
    plugin: &ChannelPluginManifest,
    staged_media: &ListenerStagedMedia,
    mut reply: OutboundMessageEnvelope,
) -> Result<OutboundMessageEnvelope> {
    if reply.attachments.is_empty() {
        return Ok(reply);
    }

    let attachment_sources = if plugin.attachment_sources.is_empty() {
        [AttachmentSource::DataBase64].as_slice()
    } else {
        plugin.attachment_sources.as_slice()
    };
    let supports_data_base64 = attachment_sources.contains(&AttachmentSource::DataBase64);
    let supports_url = attachment_sources.contains(&AttachmentSource::Url);
    let supports_storage_key = attachment_sources.contains(&AttachmentSource::StorageKey);

    let mut rewritten = Vec::with_capacity(reply.attachments.len());
    for attachment in reply.attachments {
        rewritten.push(rewrite_attachment_for_channel(
            plugin,
            staged_media,
            attachment,
            supports_data_base64,
            supports_url,
            supports_storage_key,
        )?);
    }
    reply.attachments = rewritten;
    Ok(reply)
}

fn poll_request_timeout(config: &Value) -> Duration {
    let poll_timeout_secs = config
        .get("poll_timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    std::cmp::max(
        CHANNEL_RUNTIME_REQUEST_TIMEOUT,
        Duration::from_secs(poll_timeout_secs).saturating_add(CHANNEL_POLL_REQUEST_GRACE),
    )
}

fn rewrite_attachment_for_channel(
    plugin: &ChannelPluginManifest,
    staged_media: &ListenerStagedMedia,
    mut attachment: OutboundAttachment,
    supports_data_base64: bool,
    supports_url: bool,
    supports_storage_key: bool,
) -> Result<OutboundAttachment> {
    if attachment.data_base64.is_none() || supports_data_base64 {
        return Ok(attachment);
    }

    if attachment.url.is_some() && supports_url {
        attachment.data_base64 = None;
        return Ok(attachment);
    }
    if attachment.storage_key.is_some() && supports_storage_key {
        attachment.data_base64 = None;
        return Ok(attachment);
    }
    if supports_url {
        attachment.url = Some(staged_media.stage_attachment(&attachment)?);
        attachment.data_base64 = None;
        return Ok(attachment);
    }

    bail!(
        "channel plugin `{}` cannot deliver attachment `{}` because it does not accept data_base64 and no supported fallback source is available",
        plugin.name,
        attachment.name
    );
}

fn staged_media_request_id(target: &str) -> Option<&str> {
    let path = target.split('?').next().unwrap_or(target);
    let media_id = path.strip_prefix(DISPATCH_MEDIA_ROUTE_PREFIX)?;
    if media_id.is_empty() || media_id.contains('/') {
        return None;
    }
    Some(media_id)
}

fn serve_staged_media_request(
    request: Request,
    staged_media: &ListenerStagedMedia,
    media_id: &str,
) -> Result<()> {
    let method = request.method().as_str().to_ascii_uppercase();
    if method != "GET" && method != "HEAD" {
        return respond_http_request(
            request,
            405,
            Some("text/plain; charset=utf-8"),
            "staged media only supports GET and HEAD\n",
        );
    }

    let Some(entry) = staged_media.lookup(media_id)? else {
        return respond_http_request(
            request,
            404,
            Some("text/plain; charset=utf-8"),
            "staged media not found\n",
        );
    };

    let mut response = if method == "HEAD" {
        Response::from_data(Vec::new())
    } else {
        Response::from_data(entry.body)
    }
    .with_status_code(StatusCode(200));
    let content_type = Header::from_bytes(b"Content-Type", entry.mime_type.as_bytes())
        .map_err(|_| anyhow::anyhow!("failed to build staged media content-type header"))?;
    response = response.with_header(content_type);
    let content_disposition = Header::from_bytes(
        b"Content-Disposition",
        format!("inline; filename=\"{}\"", entry.name).as_bytes(),
    )
    .map_err(|_| anyhow::anyhow!("failed to build staged media content-disposition header"))?;
    response = response.with_header(content_disposition);
    request
        .respond(response)
        .context("failed to write staged media response")
}

fn parse_query_string(query: Option<&str>) -> BTreeMap<String, String> {
    form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .filter(|(name, _)| !name.is_empty())
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push_str(&format!("{byte:02x}"));
    }
    encoded
}

fn parse_http_request(request: &mut Request, max_body_bytes: usize) -> Result<ParsedHttpRequest> {
    let method = request.method().as_str().to_ascii_uppercase();
    let target = request.url().to_string();
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), Some(query.to_string())),
        None => (target.clone(), None),
    };

    let headers = request
        .headers()
        .iter()
        .map(|header| {
            (
                header.field.to_string().to_ascii_lowercase(),
                header.value.to_string(),
            )
        })
        .collect::<BTreeMap<_, _>>();

    let mut body = Vec::new();
    request
        .as_reader()
        .take((max_body_bytes as u64) + 1)
        .read_to_end(&mut body)
        .context("failed to read request body")?;
    if body.len() > max_body_bytes {
        bail!("request body exceeds {max_body_bytes} bytes");
    }

    Ok(ParsedHttpRequest {
        method,
        target,
        path,
        query,
        headers,
        body: if body.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(&body).into_owned())
        },
    })
}

fn respond_http_request(
    request: Request,
    status_code: u16,
    content_type: Option<&str>,
    body: &str,
) -> Result<()> {
    let mut response =
        Response::from_string(body.to_string()).with_status_code(StatusCode(status_code));
    if let Some(content_type) = content_type.or(Some("text/plain; charset=utf-8")) {
        let header = Header::from_bytes(b"Content-Type", content_type.as_bytes())
            .map_err(|_| anyhow::anyhow!("failed to build response content-type header"))?;
        response = response.with_header(header);
    }
    request
        .respond(response)
        .context("failed to write ingress response")
}

fn response_kind(response: &ChannelPluginResponse) -> &'static str {
    match response {
        ChannelPluginResponse::Capabilities { .. } => "capabilities",
        ChannelPluginResponse::Configured { .. } => "configured",
        ChannelPluginResponse::Health { .. } => "health",
        ChannelPluginResponse::IngressStarted { .. } => "ingress_started",
        ChannelPluginResponse::IngressStopped { .. } => "ingress_stopped",
        ChannelPluginResponse::IngressEventsReceived { .. } => "ingress_events_received",
        ChannelPluginResponse::Delivered { .. } => "delivered",
        ChannelPluginResponse::Pushed { .. } => "pushed",
        ChannelPluginResponse::StatusAccepted { .. } => "status_accepted",
        ChannelPluginResponse::Ok => "ok",
        ChannelPluginResponse::Error { .. } => "error",
    }
}

fn enum_wire_name<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .expect("serialize enum")
        .as_str()
        .expect("enum wire name")
        .to_string()
}

#[derive(Debug, Serialize)]
struct ChannelInspectView<'a> {
    plugin: &'a ChannelPluginManifest,
    call_timeout_ms: u128,
    call_timeout_display: String,
}

fn print_channel_plugin_manifest(plugin: &ChannelPluginManifest, call_timeout: Duration) {
    println!("Name: {}", plugin.name);
    println!("Version: {}", plugin.version);
    println!("Protocol: v{}", plugin.protocol_version);
    println!("Transport: {:?}", plugin.transport);
    println!("Command: {}", plugin.exec.command);
    if !plugin.exec.args.is_empty() {
        println!("Args: {}", plugin.exec.args.join(" "));
    }
    if let Some(platform) = &plugin.platform {
        println!("Platform: {platform}");
    }
    if !plugin.attachment_sources.is_empty() {
        let sources = plugin
            .attachment_sources
            .iter()
            .map(enum_wire_name)
            .collect::<Vec<_>>();
        println!("Attachment Sources: {}", sources.join(", "));
    }
    if let Some(ingress) = &plugin.ingress {
        if !ingress.endpoints.is_empty() {
            println!("Ingress Endpoints:");
            for endpoint in &ingress.endpoints {
                let methods = if endpoint.methods.is_empty() {
                    "*".to_string()
                } else {
                    endpoint.methods.join(",")
                };
                println!(
                    "  {} [{}] host_managed={}",
                    endpoint.path, methods, endpoint.host_managed
                );
            }
        }
        if let Some(trust) = &ingress.trust {
            println!("Ingress Trust: {}", trust.mode);
            if let Some(header_name) = &trust.header_name {
                println!("Trust Header: {header_name}");
            }
            if let Some(secret_name) = &trust.secret_name {
                println!("Trust Secret: {secret_name}");
            }
            println!("Trust Host Managed: {}", trust.host_managed);
        }
    }
    if let Some(description) = &plugin.description {
        println!("Description: {description}");
    }
    println!("Call Timeout: {}", format_duration_literal(call_timeout));
}

fn format_duration_literal(duration: Duration) -> String {
    if duration.subsec_nanos() == 0 {
        if duration.as_secs().is_multiple_of(60 * 60) {
            return format!("{}h", duration.as_secs() / (60 * 60));
        }
        if duration.as_secs().is_multiple_of(60) {
            return format!("{}m", duration.as_secs() / 60);
        }
        return format!("{}s", duration.as_secs());
    }
    format!("{}ms", duration.as_millis())
}

fn default_channel_ingress_state_path(
    label: &str,
    plugin_name: &str,
    config: &Value,
) -> Result<PathBuf> {
    let cwd = env::current_dir().context("failed to resolve current working directory")?;
    let mut config_hasher = Sha256::new();
    config_hasher.update(
        serde_json::to_vec(config).context("failed to serialize channel config for poll state")?,
    );
    let config_hash = hex_encode(config_hasher.finalize().as_slice());
    Ok(cwd
        .join(CHANNEL_INGRESS_STATE_DIR)
        .join(plugin_name)
        .join(format!(
            "{}-{}.json",
            sanitize_path_component(label),
            &config_hash[..16]
        )))
}

fn sanitize_path_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => ch,
            _ => '-',
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "channel".to_string()
    } else {
        sanitized
    }
}

fn load_channel_ingress_state(path: &Path) -> Result<Option<IngressState>> {
    let body = match fs::read_to_string(path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };

    let state = serde_json::from_str(&body)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(state))
}

fn save_channel_ingress_state(path: &Path, state: &IngressState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let body =
        serde_json::to_vec_pretty(state).context("failed to serialize channel ingress state")?;
    let mut file = AtomicWriteFile::options()
        .open(path)
        .with_context(|| format!("failed to open {} for atomic write", path.display()))?;
    file.write_all(&body)
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.commit()
        .with_context(|| format!("failed to persist {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_plugin_with_attachment_sources(
        attachment_sources: Vec<AttachmentSource>,
    ) -> ChannelPluginManifest {
        ChannelPluginManifest {
            name: "channel-test".to_string(),
            version: "0.1.0".to_string(),
            protocol_version: 1,
            transport: dispatch_core::PluginTransport::Jsonl,
            description: None,
            exec: dispatch_core::ChannelPluginExec {
                command: "/usr/bin/true".to_string(),
                args: vec![],
            },
            platform: Some("test".to_string()),
            attachment_sources,
            ingress: None,
            installed_sha256: None,
        }
    }

    #[test]
    fn parse_key_value_pairs_accepts_repeated_cli_entries() {
        let values = parse_key_value_pairs(
            &[
                "Content-Type=application/json".to_string(),
                "X-Test=value".to_string(),
            ],
            "header",
        )
        .expect("parse key value pairs");

        assert_eq!(
            values.get("Content-Type").map(String::as_str),
            Some("application/json")
        );
        assert_eq!(values.get("X-Test").map(String::as_str), Some("value"));
    }

    #[test]
    fn parse_key_value_pairs_rejects_missing_separator() {
        let error = parse_key_value_pairs(&["Content-Type".to_string()], "header").unwrap_err();
        assert!(error.to_string().contains("must use NAME=VALUE"));
    }

    #[test]
    fn build_ingress_request_defaults_to_empty_body_and_config() {
        let request = build_ingress_request(IngressRequestParts {
            config: json!({}),
            method: "POST",
            path: "/hook",
            headers: &["Content-Type=application/json".to_string()],
            query: &["conversation_id=abc".to_string()],
            body: None,
            body_file: None,
            endpoint_id: Some("endpoint-1".to_string()),
            trust_verified: true,
            received_at: Some("2026-04-11T00:00:00Z".to_string()),
        })
        .expect("build ingress request");

        let ChannelPluginRequest::IngressEvent {
            config,
            state,
            payload,
        } = request
        else {
            panic!("expected ingress request");
        };
        assert_eq!(config, json!({}));
        assert_eq!(state, None);
        assert_eq!(payload.method, "POST");
        assert_eq!(payload.path, "/hook");
        assert_eq!(payload.body, "");
        assert_eq!(
            payload.headers.get("Content-Type").map(String::as_str),
            Some("application/json")
        );
        assert_eq!(
            payload.query.get("conversation_id").map(String::as_str),
            Some("abc")
        );
        assert_eq!(payload.raw_query.as_deref(), Some("conversation_id=abc"));
        assert_eq!(payload.endpoint_id.as_deref(), Some("endpoint-1"));
        assert!(payload.trust_verified);
    }

    #[test]
    fn parse_query_string_extracts_pairs() {
        let query = parse_query_string(Some(
            "conversation_id=abc&thread_id=42&flag&subject=hello%20world&name=dispatch%2Bbot",
        ));

        assert_eq!(
            query.get("conversation_id").map(String::as_str),
            Some("abc")
        );
        assert_eq!(query.get("thread_id").map(String::as_str), Some("42"));
        assert_eq!(query.get("flag").map(String::as_str), Some(""));
        assert_eq!(
            query.get("subject").map(String::as_str),
            Some("hello world")
        );
        assert_eq!(query.get("name").map(String::as_str), Some("dispatch+bot"));
    }

    #[test]
    fn format_duration_literal_prefers_readable_units() {
        assert_eq!(format_duration_literal(Duration::from_millis(250)), "250ms");
        assert_eq!(format_duration_literal(Duration::from_secs(45)), "45s");
        assert_eq!(format_duration_literal(Duration::from_secs(120)), "2m");
        assert_eq!(format_duration_literal(Duration::from_secs(7200)), "2h");
    }

    #[test]
    fn rewrite_reply_attachments_stages_inline_data_for_url_only_channel() {
        let plugin = test_plugin_with_attachment_sources(vec![AttachmentSource::Url]);
        let staged_media = ListenerStagedMedia::from_config(&json!({
            "webhook_public_url": "https://dispatch.example.test"
        }));
        let reply = OutboundMessageEnvelope {
            content: "reply".to_string(),
            content_type: Some("text/plain".to_string()),
            attachments: vec![OutboundAttachment {
                name: "report.txt".to_string(),
                mime_type: "text/plain".to_string(),
                data_base64: Some("aGVsbG8=".to_string()),
                url: None,
                storage_key: None,
            }],
            metadata: BTreeMap::new(),
        };

        let rewritten =
            rewrite_reply_attachments_for_channel(&plugin, &staged_media, reply).expect("rewrite");

        assert_eq!(rewritten.attachments.len(), 1);
        let attachment = &rewritten.attachments[0];
        assert!(attachment.data_base64.is_none());
        assert!(attachment.storage_key.is_none());
        let staged_url = attachment.url.as_deref().expect("staged media URL");
        assert!(staged_url.starts_with("https://dispatch.example.test/_dispatch/media/"));
        let media_id = staged_url
            .rsplit('/')
            .next()
            .filter(|value| !value.is_empty())
            .expect("reserved route ID");
        let stored = staged_media.lookup(media_id).expect("lookup");
        assert!(stored.is_some());
    }

    #[test]
    fn rewrite_reply_attachments_prefers_existing_url_when_channel_cannot_send_inline_data() {
        let plugin = test_plugin_with_attachment_sources(vec![AttachmentSource::Url]);
        let staged_media = ListenerStagedMedia::from_config(&json!({}));
        let reply = OutboundMessageEnvelope {
            content: "reply".to_string(),
            content_type: Some("text/plain".to_string()),
            attachments: vec![OutboundAttachment {
                name: "report.txt".to_string(),
                mime_type: "text/plain".to_string(),
                data_base64: Some("aGVsbG8=".to_string()),
                url: Some("https://files.example.test/report.txt".to_string()),
                storage_key: None,
            }],
            metadata: BTreeMap::new(),
        };

        let rewritten =
            rewrite_reply_attachments_for_channel(&plugin, &staged_media, reply).expect("rewrite");

        assert_eq!(rewritten.attachments.len(), 1);
        let attachment = &rewritten.attachments[0];
        assert!(attachment.data_base64.is_none());
        assert_eq!(
            attachment.url.as_deref(),
            Some("https://files.example.test/report.txt")
        );
    }

    #[test]
    fn rewrite_reply_attachments_defaults_to_inline_data_when_manifest_omits_sources() {
        let plugin = test_plugin_with_attachment_sources(Vec::new());
        let staged_media = ListenerStagedMedia::from_config(&json!({}));
        let reply = OutboundMessageEnvelope {
            content: "reply".to_string(),
            content_type: Some("text/plain".to_string()),
            attachments: vec![OutboundAttachment {
                name: "report.txt".to_string(),
                mime_type: "text/plain".to_string(),
                data_base64: Some("aGVsbG8=".to_string()),
                url: None,
                storage_key: None,
            }],
            metadata: BTreeMap::new(),
        };

        let rewritten =
            rewrite_reply_attachments_for_channel(&plugin, &staged_media, reply).expect("rewrite");

        assert_eq!(rewritten.attachments.len(), 1);
        let attachment = &rewritten.attachments[0];
        assert_eq!(attachment.data_base64.as_deref(), Some("aGVsbG8="));
        assert!(attachment.url.is_none());
    }

    #[test]
    fn rewrite_reply_attachments_rejects_inline_data_without_supported_fallback() {
        let plugin = test_plugin_with_attachment_sources(vec![AttachmentSource::StorageKey]);
        let staged_media = ListenerStagedMedia::from_config(&json!({
            "webhook_public_url": "https://dispatch.example.test"
        }));
        let reply = OutboundMessageEnvelope {
            content: "reply".to_string(),
            content_type: Some("text/plain".to_string()),
            attachments: vec![OutboundAttachment {
                name: "report.txt".to_string(),
                mime_type: "text/plain".to_string(),
                data_base64: Some("aGVsbG8=".to_string()),
                url: None,
                storage_key: None,
            }],
            metadata: BTreeMap::new(),
        };

        let error = rewrite_reply_attachments_for_channel(&plugin, &staged_media, reply)
            .expect_err("rewrite should fail");
        assert!(error.to_string().contains("cannot deliver attachment"));
    }

    #[test]
    fn staged_media_request_id_extracts_reserved_route() {
        assert_eq!(
            staged_media_request_id("/_dispatch/media/abc123?download=1"),
            Some("abc123")
        );
        assert_eq!(staged_media_request_id("/telegram/updates"), None);
        assert_eq!(staged_media_request_id("/_dispatch/media/"), None);
        assert_eq!(staged_media_request_id("/_dispatch/media/a/b"), None);
    }
}
