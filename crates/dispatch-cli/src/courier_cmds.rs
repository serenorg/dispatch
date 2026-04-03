use anyhow::Result;
use dispatch_core::{
    CourierCatalogEntry, CourierPluginManifest, ResolvedCourier, default_courier_registry_path,
    install_courier_plugin, list_courier_catalog, resolve_courier,
};
use std::path::{Path, PathBuf};

pub(crate) fn courier_command(command: crate::CourierCommand) -> Result<()> {
    match command {
        crate::CourierCommand::Ls { json, registry } => courier_ls(registry.as_deref(), json),
        crate::CourierCommand::Inspect {
            name,
            json,
            registry,
        } => courier_inspect(&name, registry.as_deref(), json),
        crate::CourierCommand::Install { manifest, registry } => {
            courier_install(&manifest, registry.as_deref())
        }
        crate::CourierCommand::Conformance {
            name,
            registry,
            json,
        } => crate::conformance::courier_conformance(&name, registry.as_deref(), json),
    }
}

fn courier_ls(registry: Option<&Path>, emit_json: bool) -> Result<()> {
    let catalog = list_courier_catalog(registry)?;
    if emit_json {
        println!("{}", serde_json::to_string_pretty(&catalog)?);
        return Ok(());
    }

    for entry in catalog {
        match entry {
            CourierCatalogEntry::Builtin {
                name,
                kind,
                description,
            } => println!("{name}\tbuiltin\t{kind:?}\t{description}"),
            CourierCatalogEntry::Plugin {
                name,
                protocol_version,
                transport,
                command,
                ..
            } => println!("{name}\tplugin\tprotocol-v{protocol_version}/{transport:?}\t{command}"),
        }
    }

    Ok(())
}

fn courier_inspect(name: &str, registry: Option<&Path>, emit_json: bool) -> Result<()> {
    match resolve_courier(name, registry)? {
        ResolvedCourier::Builtin(courier) => {
            let entry = crate::inspect::builtin_catalog_entry(courier);
            if emit_json {
                println!("{}", serde_json::to_string_pretty(&entry)?);
            } else {
                print_courier_catalog_entry(&entry);
            }
        }
        ResolvedCourier::Plugin(plugin) => {
            if emit_json {
                println!("{}", serde_json::to_string_pretty(&plugin)?);
            } else {
                print_courier_plugin_manifest(&plugin);
            }
        }
    }

    Ok(())
}

fn courier_install(manifest: &Path, registry: Option<&Path>) -> Result<()> {
    let installed = install_courier_plugin(manifest, registry)?;
    let registry_path = registry
        .map(PathBuf::from)
        .or_else(|| default_courier_registry_path().ok())
        .unwrap_or_else(|| PathBuf::from("<unknown>"));

    println!("Installed courier plugin `{}`", installed.name);
    println!("Registry: {}", registry_path.display());
    Ok(())
}

fn print_courier_catalog_entry(entry: &CourierCatalogEntry) {
    match entry {
        CourierCatalogEntry::Builtin {
            name,
            kind,
            description,
        } => {
            println!("Name: {name}");
            println!("Source: builtin");
            println!("Kind: {kind:?}");
            println!("Description: {description}");
        }
        CourierCatalogEntry::Plugin {
            name,
            description,
            protocol_version,
            transport,
            command,
            args,
        } => {
            println!("Name: {name}");
            println!("Source: plugin");
            println!("Protocol: v{protocol_version}");
            println!("Transport: {transport:?}");
            println!("Command: {command}");
            if !args.is_empty() {
                println!("Args: {}", args.join(" "));
            }
            if let Some(description) = description {
                println!("Description: {description}");
            }
        }
    }
}

fn print_courier_plugin_manifest(plugin: &CourierPluginManifest) {
    println!("Name: {}", plugin.name);
    println!("Version: {}", plugin.version);
    println!("Protocol: v{}", plugin.protocol_version);
    println!("Transport: {:?}", plugin.transport);
    println!("Command: {}", plugin.exec.command);
    if !plugin.exec.args.is_empty() {
        println!("Args: {}", plugin.exec.args.join(" "));
    }
    if let Some(description) = &plugin.description {
        println!("Description: {description}");
    }
}
