use anyhow::{Context, Result, bail};
use chrono::Utc;
use dispatch_core::{
    ChannelPluginManifest, ChannelPluginRequest, ChannelPluginResponse, DeliveryReceipt,
    InboundEventEnvelope, IngressCallbackReply, IngressPayload, build_channel_reply_message,
    call_channel_plugin, channel_event_session_file, default_channel_registry_path,
    extract_assistant_reply, install_channel_plugin, list_channel_catalog,
    match_channel_ingress_endpoint, render_inbound_event_chat_input, resolve_channel_plugin,
    resolve_channel_plugin_for_ingress, verify_host_managed_ingress_trust,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};
use tiny_http::{Header, Request, Response, Server, StatusCode};

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

struct ChannelParcelBridge {
    parcel_path: PathBuf,
    parcel_digest: String,
    courier: String,
    courier_registry: Option<PathBuf>,
    session_root: PathBuf,
    tool_approval: crate::CliToolApprovalMode,
    deliver_replies: bool,
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
    let config = load_json_value(args.config_json, args.config_file, "channel config")?;
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

fn channel_listen(args: ChannelListenArgs<'_>) -> Result<()> {
    let plugin = resolve_channel_plugin(args.name, args.registry)?;
    let config = load_json_value(args.config_json, args.config_file, "channel config")?;
    let parcel_bridge = prepare_channel_parcel_bridge(
        args.parcel,
        args.courier,
        args.courier_registry,
        args.session_root,
        args.tool_approval,
        args.deliver_replies,
    )?;
    let server = Server::http(args.listen)
        .map_err(|error| anyhow::anyhow!("failed to bind {}: {error}", args.listen))?;
    println!("Listening on {}", server.server_addr());
    if let Some(parcel_bridge) = &parcel_bridge {
        println!(
            "Parcel bridge: {} via {} (sessions under {})",
            parcel_bridge.parcel_path.display(),
            parcel_bridge.courier,
            parcel_bridge.session_root.display()
        );
    }

    loop {
        handle_channel_listener_connection(
            &plugin,
            &config,
            server.recv().context("failed to accept connection")?,
            parcel_bridge.as_ref(),
            args.emit_json,
        )?;
        if args.once {
            break;
        }
    }

    Ok(())
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
        } => {
            println!("Events: {}", events.len());
            if let Some(callback_reply) = callback_reply {
                println!("Callback Reply:");
                println!("{}", serde_json::to_string_pretty(&callback_reply)?);
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

fn load_json_value(
    value_json: Option<&str>,
    value_file: Option<&Path>,
    description: &str,
) -> Result<Value> {
    match (value_json, value_file) {
        (Some(_), Some(_)) => bail!("use either inline JSON or a file for {description}, not both"),
        (None, None) => Ok(json!({})),
        (Some(value_json), None) => serde_json::from_str(value_json)
            .with_context(|| format!("failed to parse {description} JSON")),
        (None, Some(value_file)) => {
            let body = fs::read_to_string(value_file)
                .with_context(|| format!("failed to read {}", value_file.display()))?;
            serde_json::from_str(&body)
                .with_context(|| format!("failed to parse {}", value_file.display()))
        }
    }
}

fn build_ingress_request(parts: IngressRequestParts<'_>) -> Result<ChannelPluginRequest> {
    let headers = parse_key_value_pairs(parts.headers, "header")?;
    let query = parse_key_value_pairs(parts.query, "query")?;
    let raw_query = (!parts.query.is_empty()).then(|| parts.query.join("&"));
    let body = load_body(parts.body, parts.body_file)?;

    Ok(ChannelPluginRequest::IngressEvent {
        config: parts.config,
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
    plugin: &ChannelPluginManifest,
    config: &Value,
    mut request: Request,
    parcel_bridge: Option<&ChannelParcelBridge>,
    emit_json: bool,
) -> Result<()> {
    let remote_addr = request
        .remote_addr()
        .map(ToString::to_string)
        .unwrap_or_else(|| "<unknown>".to_string());
    let parsed = match parse_http_request(&mut request, 1024 * 1024) {
        Ok(request) => request,
        Err(error) => {
            return respond_http_request(
                request,
                400,
                Some("text/plain; charset=utf-8"),
                &format!("invalid request: {error}\n"),
            );
        }
    };

    let matched_endpoint =
        match match_channel_ingress_endpoint(plugin, &parsed.method, &parsed.path) {
            Some(endpoint) => endpoint,
            None => {
                return respond_http_request(
                    request,
                    404,
                    Some("text/plain; charset=utf-8"),
                    "request did not match an installed ingress endpoint\n",
                );
            }
        };
    let trust_verified = match verify_host_managed_ingress_trust(plugin, &parsed.headers) {
        Ok(verified) => verified,
        Err(error) => {
            return respond_http_request(
                request,
                error.status_code,
                Some("text/plain; charset=utf-8"),
                &format!("{}\n", error.message),
            );
        }
    };

    let response = call_channel_plugin(
        plugin,
        ChannelPluginRequest::IngressEvent {
            config: config.clone(),
            payload: IngressPayload {
                endpoint_id: Some(format!("{}:{}", plugin.name, matched_endpoint.path)),
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
    );

    match response {
        Ok(ChannelPluginResponse::IngressEventsReceived {
            events,
            callback_reply,
        }) => {
            let parcel_runs = if let Some(parcel_bridge) = parcel_bridge {
                execute_channel_parcel_runs(plugin, parcel_bridge, config, &events)?
            } else {
                Vec::new()
            };
            if emit_json {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "plugin": plugin.name,
                        "events": events,
                        "parcel_runs": parcel_runs,
                    }))?
                );
            } else {
                println!(
                    "Ingress {} {} from {} -> {} event(s)",
                    parsed.method,
                    parsed.path,
                    remote_addr,
                    events.len()
                );
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
            )
        }
        Ok(ChannelPluginResponse::Error { error }) => respond_http_request(
            request,
            502,
            Some("text/plain; charset=utf-8"),
            &format!("channel plugin error {}: {}\n", error.code, error.message),
        ),
        Ok(other) => respond_http_request(
            request,
            502,
            Some("text/plain; charset=utf-8"),
            &format!(
                "channel plugin returned unexpected response variant for ingress: {:?}\n",
                response_kind(&other)
            ),
        ),
        Err(error) => respond_http_request(
            request,
            502,
            Some("text/plain; charset=utf-8"),
            &format!("failed to call channel plugin: {error}\n"),
        ),
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

fn execute_channel_parcel_runs(
    plugin: &ChannelPluginManifest,
    parcel_bridge: &ChannelParcelBridge,
    config: &Value,
    events: &[InboundEventEnvelope],
) -> Result<Vec<ChannelParcelRunResult>> {
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
            if let Some(reply_text) = extract_assistant_reply(&response.events) {
                deliver_channel_reply(plugin, event, config, &reply_text)?
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
    plugin: &ChannelPluginManifest,
    event: &InboundEventEnvelope,
    config: &Value,
    reply_text: &str,
) -> Result<Option<DeliveryReceipt>> {
    let message = serde_json::to_value(build_channel_reply_message(event, reply_text))
        .context("failed to serialize channel reply envelope")?;

    let response = call_channel_plugin(
        plugin,
        ChannelPluginRequest::Deliver {
            config: config.clone(),
            message,
        },
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

fn parse_query_string(query: Option<&str>) -> BTreeMap<String, String> {
    let mut parsed = BTreeMap::new();
    for pair in query.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (name, value) = match pair.split_once('=') {
            Some((name, value)) => (name, value),
            None => (pair, ""),
        };
        if !name.is_empty() {
            parsed.insert(name.to_string(), value.to_string());
        }
    }
    parsed
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
        ChannelPluginResponse::Error { .. } => "error",
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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

        let ChannelPluginRequest::IngressEvent { config, payload } = request else {
            panic!("expected ingress request");
        };
        assert_eq!(config, json!({}));
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
        let query = parse_query_string(Some("conversation_id=abc&thread_id=42&flag"));

        assert_eq!(
            query.get("conversation_id").map(String::as_str),
            Some("abc")
        );
        assert_eq!(query.get("thread_id").map(String::as_str), Some("42"));
        assert_eq!(query.get("flag").map(String::as_str), Some(""));
    }

    #[test]
    fn format_duration_literal_prefers_readable_units() {
        assert_eq!(format_duration_literal(Duration::from_millis(250)), "250ms");
        assert_eq!(format_duration_literal(Duration::from_secs(45)), "45s");
        assert_eq!(format_duration_literal(Duration::from_secs(120)), "2m");
        assert_eq!(format_duration_literal(Duration::from_secs(7200)), "2h");
    }
}
