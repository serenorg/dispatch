use anyhow::{Context, Result};
use dispatch_core::{
    BuiltinCourier, CourierBackend, CourierCapabilities, CourierCatalogEntry, CourierInspection,
    DockerCourier, JsonlCourierPlugin, LocalToolTarget, NativeCourier, ParcelManifest,
    ResolvedCourier, WasmCourier, load_parcel, resolve_courier,
};
use futures::executor::block_on;
use std::{
    fs,
    path::{Path, PathBuf},
};

pub(crate) fn inspect(
    path: PathBuf,
    courier: Option<String>,
    registry: Option<PathBuf>,
    emit_json: bool,
) -> Result<()> {
    let manifest_path = resolve_manifest_path(path);
    let source = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let parcel: ParcelManifest = serde_json::from_str(&source)
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;

    if emit_json {
        println!("{}", serde_json::to_string_pretty(&parcel)?);
        return Ok(());
    }

    println!("Digest: {}", parcel.digest);
    println!("Name: {}", parcel.name.as_deref().unwrap_or("<unnamed>"));
    println!(
        "Version: {}",
        parcel.version.as_deref().unwrap_or("<unspecified>")
    );
    println!("Courier Target: {}", parcel.courier.reference());
    println!(
        "Entrypoint: {}",
        parcel.entrypoint.as_deref().unwrap_or("<none>")
    );
    println!("Instruction files: {}", parcel.instructions.len());
    println!("Packaged files: {}", parcel.files.len());

    if let Some(courier) = courier {
        println!();
        inspect_for_courier_name(&courier, registry.as_deref(), &manifest_path)?;
    }

    Ok(())
}

fn resolve_manifest_path(path: PathBuf) -> PathBuf {
    if path.is_dir() {
        path.join("manifest.json")
    } else {
        path
    }
}

fn inspect_for_courier_name(
    courier_name: &str,
    registry: Option<&Path>,
    parcel_path: &Path,
) -> Result<()> {
    match resolve_courier(courier_name, registry)? {
        ResolvedCourier::Builtin(courier) => inspect_for_builtin_courier(courier, parcel_path),
        ResolvedCourier::Plugin(plugin) => {
            inspect_for_courier(JsonlCourierPlugin::new(plugin), parcel_path)
        }
    }
}

fn inspect_for_builtin_courier(courier: BuiltinCourier, parcel_path: &Path) -> Result<()> {
    match courier {
        BuiltinCourier::Native => inspect_for_courier(NativeCourier::default(), parcel_path),
        BuiltinCourier::Docker => inspect_for_courier(DockerCourier::default(), parcel_path),
        BuiltinCourier::Wasm => inspect_for_courier(WasmCourier::new()?, parcel_path),
    }
}

fn inspect_for_courier<R: CourierBackend>(courier: R, parcel_path: &Path) -> Result<()> {
    let parcel = load_parcel(parcel_path)
        .with_context(|| format!("failed to load parcel {}", parcel_path.display()))?;
    block_on(courier.validate_parcel(&parcel)).with_context(|| {
        format!(
            "courier `{}` is incompatible with parcel {}",
            courier.id(),
            parcel_path.display()
        )
    })?;
    let capabilities = block_on(courier.capabilities()).with_context(|| {
        format!(
            "failed to query courier capabilities for `{}`",
            courier.id()
        )
    })?;
    let inspection = block_on(courier.inspect(&parcel)).with_context(|| {
        format!(
            "failed to inspect parcel {} for courier",
            parcel_path.display()
        )
    })?;

    print_courier_capabilities(&capabilities);
    print_courier_inspection(&inspection);
    Ok(())
}

fn print_courier_capabilities(capabilities: &CourierCapabilities) {
    println!("Courier Backend: {}", capabilities.courier_id);
    println!("Courier Kind: {:?}", capabilities.kind);
    println!("Supports Chat: {}", capabilities.supports_chat);
    println!("Supports Job: {}", capabilities.supports_job);
    println!("Supports Heartbeat: {}", capabilities.supports_heartbeat);
    println!(
        "Supports Local Tools: {}",
        capabilities.supports_local_tools
    );
}

fn print_courier_inspection(inspection: &CourierInspection) {
    println!(
        "Validated Entrypoint: {}",
        inspection.entrypoint.as_deref().unwrap_or("<none>")
    );
    println!("Declared Secrets: {}", inspection.required_secrets.len());
    println!("Declared Mounts: {}", inspection.mounts.len());
    println!("Declared Local Tools: {}", inspection.local_tools.len());
    for secret in &inspection.required_secrets {
        println!("  secret: {secret}");
    }
    for mount in &inspection.mounts {
        println!("  mount: {:?} {}", mount.kind, mount.driver);
    }
    for tool in &inspection.local_tools {
        match &tool.target {
            LocalToolTarget::Local {
                packaged_path,
                command,
                args,
            } => {
                let arg_suffix = if args.is_empty() {
                    String::new()
                } else {
                    format!(" {}", args.join(" "))
                };
                let skill_suffix = tool
                    .skill_source
                    .as_deref()
                    .map(|source| format!(" skill={source}"))
                    .unwrap_or_default();
                println!(
                    "  tool: {} local path={} runner={}{}{}",
                    tool.alias, packaged_path, command, arg_suffix, skill_suffix
                );
            }
            LocalToolTarget::A2a {
                endpoint_url,
                endpoint_mode,
                auth,
                expected_agent_name,
                expected_card_sha256,
            } => {
                let mut details = vec![format!("url={endpoint_url}")];
                if let Some(mode) = endpoint_mode {
                    details.push(format!("discovery={mode:?}").to_ascii_lowercase());
                }
                if let Some(auth) = auth {
                    details.push(crate::tool_display::format_a2a_auth_summary(auth));
                }
                if let Some(name) = expected_agent_name {
                    details.push(format!("expected_agent_name={name}"));
                }
                if let Some(digest) = expected_card_sha256 {
                    details.push(format!("expected_card_sha256={digest}"));
                }
                println!("  tool: {} a2a {}", tool.alias, details.join(" "));
            }
        }
    }
}

pub(crate) fn builtin_catalog_entry(courier: BuiltinCourier) -> CourierCatalogEntry {
    CourierCatalogEntry::Builtin {
        name: courier.name().to_string(),
        kind: courier.kind(),
        description: courier.description().to_string(),
    }
}
