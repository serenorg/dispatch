use anyhow::{Context, Result, bail};
use dispatch_core::ParcelManifest;
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct StateEntry {
    pub digest: String,
    pub path: PathBuf,
    pub parcel_present: bool,
    pub name: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct StateGcReport {
    pub root: PathBuf,
    pub parcels_root: PathBuf,
    pub removed: Vec<StateEntry>,
    pub kept: Vec<StateEntry>,
    pub dry_run: bool,
}

pub(crate) fn state_ls(
    root: Option<PathBuf>,
    parcels_root: Option<PathBuf>,
    emit_json: bool,
) -> Result<()> {
    let root = resolve_state_root(root)?;
    let parcels_root = resolve_parcels_root(parcels_root)?;
    let entries = collect_state_entries(&root, &parcels_root)?;

    if emit_json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    if entries.is_empty() {
        println!("No parcel state directories found under {}", root.display());
        return Ok(());
    }

    for entry in entries {
        let status = if entry.parcel_present {
            "live"
        } else {
            "orphaned"
        };
        let name = entry.name.as_deref().unwrap_or("<unknown>");
        let version = entry.version.as_deref().unwrap_or("<unspecified>");
        println!(
            "{}\t{}\t{}\t{}\t{}",
            entry.digest,
            status,
            name,
            version,
            entry.path.display()
        );
    }

    Ok(())
}

pub(crate) fn state_gc(
    root: Option<PathBuf>,
    parcels_root: Option<PathBuf>,
    dry_run: bool,
) -> Result<()> {
    let root = resolve_state_root(root)?;
    let parcels_root = resolve_parcels_root(parcels_root)?;
    let entries = collect_state_entries(&root, &parcels_root)?;
    let mut removed = Vec::new();
    let mut kept = Vec::new();

    for entry in entries {
        if entry.parcel_present {
            kept.push(entry);
            continue;
        }
        if !dry_run {
            fs::remove_dir_all(&entry.path)
                .with_context(|| format!("failed to remove {}", entry.path.display()))?;
        }
        removed.push(entry);
    }

    let report = StateGcReport {
        root,
        parcels_root,
        removed,
        kept,
        dry_run,
    };

    if report.removed.is_empty() {
        println!("No orphaned parcel state found.");
        return Ok(());
    }

    let action = if report.dry_run {
        "Would remove"
    } else {
        "Removed"
    };
    for entry in &report.removed {
        println!("{action} {}\t{}", entry.digest, entry.path.display());
    }
    println!(
        "{} {} orphaned state director{}.",
        action,
        report.removed.len(),
        if report.removed.len() == 1 {
            "y"
        } else {
            "ies"
        }
    );
    Ok(())
}

pub(crate) fn state_migrate(
    source_digest: &str,
    target_digest: &str,
    root: Option<PathBuf>,
    force: bool,
) -> Result<()> {
    let root = resolve_state_root(root)?;
    let source = root.join(source_digest);
    let target = root.join(target_digest);

    if !source.exists() {
        bail!(
            "state for digest `{source_digest}` does not exist at {}",
            source.display()
        );
    }
    if target.exists() {
        if !force {
            bail!(
                "state for digest `{target_digest}` already exists at {} (pass --force to replace it)",
                target.display()
            );
        }
        fs::remove_dir_all(&target)
            .with_context(|| format!("failed to remove {}", target.display()))?;
    }

    copy_dir_recursive(&source, &target)?;
    println!(
        "Migrated parcel state from {} to {}",
        source.display(),
        target.display()
    );
    Ok(())
}

fn resolve_state_root(root: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(root) = root {
        return Ok(root);
    }
    if let Some(root) = std::env::var_os("DISPATCH_STATE_ROOT") {
        return Ok(PathBuf::from(root));
    }
    Ok(std::env::current_dir()
        .context("failed to resolve current working directory")?
        .join(".dispatch/state"))
}

fn resolve_parcels_root(parcels_root: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(root) = parcels_root {
        return Ok(root);
    }
    Ok(std::env::current_dir()
        .context("failed to resolve current working directory")?
        .join(".dispatch/parcels"))
}

pub(crate) fn collect_state_entries(root: &Path, parcels_root: &Path) -> Result<Vec<StateEntry>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut entries = fs::read_dir(root)
        .with_context(|| format!("failed to read {}", root.display()))?
        .map(|entry| {
            let entry = entry.with_context(|| format!("failed to inspect {}", root.display()))?;
            let path = entry.path();
            if !path.is_dir() {
                return Ok(None);
            }
            let digest = entry.file_name().to_string_lossy().to_string();
            let manifest_path = parcels_root.join(&digest).join("manifest.json");
            let manifest = if manifest_path.exists() {
                let body = fs::read_to_string(&manifest_path)
                    .with_context(|| format!("failed to read {}", manifest_path.display()))?;
                Some(
                    serde_json::from_str::<ParcelManifest>(&body)
                        .with_context(|| format!("failed to parse {}", manifest_path.display()))?,
                )
            } else {
                None
            };
            Ok(Some(StateEntry {
                digest,
                path,
                parcel_present: manifest.is_some(),
                name: manifest.as_ref().and_then(|manifest| manifest.name.clone()),
                version: manifest
                    .as_ref()
                    .and_then(|manifest| manifest.version.clone()),
            }))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    entries.sort_by(|left, right| left.digest.cmp(&right.digest));
    Ok(entries)
}

fn copy_dir_recursive(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target).with_context(|| format!("failed to create {}", target.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry.with_context(|| format!("failed to inspect {}", source.display()))?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", source_path.display()))?;
        if file_type.is_dir() {
            copy_dir_recursive(&source_path, &target_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &target_path).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    source_path.display(),
                    target_path.display()
                )
            })?;
        }
    }
    Ok(())
}
