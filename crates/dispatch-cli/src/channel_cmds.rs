use anyhow::{Context, Result, bail};
use dispatch_core::{
    ChannelPluginManifest, ChannelPluginRequest, ChannelPluginResponse, IngressPayload,
    call_channel_plugin, default_channel_registry_path, install_channel_plugin,
    list_channel_catalog, resolve_channel_plugin,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

struct ChannelIngressArgs<'a> {
    name: &'a str,
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
            name: &name,
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
        println!(
            "{}\t{}\tprotocol-v{}/{:?}\t{}",
            entry.name, platform, entry.protocol_version, entry.transport, entry.command
        );
    }

    Ok(())
}

fn channel_inspect(name: &str, registry: Option<&Path>, emit_json: bool) -> Result<()> {
    let plugin = resolve_channel_plugin(name, registry)?;
    if emit_json {
        println!("{}", serde_json::to_string_pretty(&plugin)?);
    } else {
        print_channel_plugin_manifest(&plugin);
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
    let plugin = resolve_channel_plugin(args.name, args.registry)?;
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

    print_channel_response(response, args.emit_json)
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
    let body = load_body(parts.body, parts.body_file)?;

    Ok(ChannelPluginRequest::IngressEvent {
        config: parts.config,
        payload: IngressPayload {
            endpoint_id: parts.endpoint_id,
            method: parts.method.to_string(),
            path: parts.path.to_string(),
            headers,
            query,
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

fn print_channel_plugin_manifest(plugin: &ChannelPluginManifest) {
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
    if let Some(description) = &plugin.description {
        println!("Description: {description}");
    }
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
        assert_eq!(payload.endpoint_id.as_deref(), Some("endpoint-1"));
        assert!(payload.trust_verified);
    }
}
