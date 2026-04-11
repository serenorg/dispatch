use anyhow::Result;
use dispatch_core::{
    ChannelPluginManifest, default_channel_registry_path, install_channel_plugin,
    list_channel_catalog, resolve_channel_plugin,
};
use std::path::{Path, PathBuf};

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
