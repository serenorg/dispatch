use anyhow::{Context, Result, bail};
use dispatch_core::{
    ChannelPluginManifest, ChannelPluginRequest, ChannelPluginResponse, call_channel_plugin,
    default_channel_registry_path, install_channel_plugin, list_channel_catalog,
    resolve_channel_plugin,
};
use std::{
    fs,
    path::{Path, PathBuf},
};

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

    if emit_json {
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }

    match response {
        ChannelPluginResponse::Capabilities { capabilities } => {
            println!("Plugin ID: {}", capabilities.plugin_id);
            println!("Platform: {}", capabilities.platform);
            println!("Ingress Modes: {}", capabilities.ingress_modes.join(", "));
            println!(
                "Outbound Types: {}",
                capabilities.outbound_message_types.join(", ")
            );
            println!("Threading: {}", capabilities.threading_model);
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
