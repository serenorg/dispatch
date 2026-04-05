use super::{CourierError, LoadedParcel, ResolvedMount};
use crate::manifest::{MountConfig, MountKind};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub(super) fn ensure_mounts_supported(
    courier_name: &str,
    mounts: &[MountConfig],
    supported_mounts: &[MountKind],
) -> Result<(), CourierError> {
    for mount in mounts {
        if !supported_mounts.contains(&mount.kind) {
            return Err(CourierError::UnsupportedMount {
                courier: courier_name.to_string(),
                kind: mount.kind,
                driver: mount.driver.clone(),
            });
        }
    }
    Ok(())
}

pub(super) fn resolve_builtin_mounts(
    parcel: &LoadedParcel,
    courier_name: &str,
    session_id: &str,
) -> Result<Vec<ResolvedMount>, CourierError> {
    let mut mounts = Vec::with_capacity(parcel.config.mounts.len());
    let parcel_state_root = resolve_parcel_state_root(parcel);
    let session_state_root = parcel_state_root.join("sessions").join(session_id);

    for mount in &parcel.config.mounts {
        let resolved = match (mount.kind, mount.driver.as_str()) {
            (MountKind::Session, "memory") => ResolvedMount {
                kind: mount.kind,
                driver: mount.driver.clone(),
                target_path: format!("dispatch://session/{session_id}"),
                metadata: BTreeMap::from([("storage".to_string(), "memory".to_string())]),
            },
            (MountKind::Session, "sqlite") => {
                let path = session_state_root.join("session.sqlite");
                ensure_parent_dir(&path)?;
                touch_file(&path)?;
                ResolvedMount {
                    kind: mount.kind,
                    driver: mount.driver.clone(),
                    target_path: path.display().to_string(),
                    metadata: BTreeMap::new(),
                }
            }
            (MountKind::Memory, "none") => ResolvedMount {
                kind: mount.kind,
                driver: mount.driver.clone(),
                target_path: "dispatch://memory/none".to_string(),
                metadata: BTreeMap::new(),
            },
            (MountKind::Memory, "sqlite") => {
                let path = parcel_state_root.join("memory.sqlite");
                ensure_parent_dir(&path)?;
                touch_file(&path)?;
                ResolvedMount {
                    kind: mount.kind,
                    driver: mount.driver.clone(),
                    target_path: path.display().to_string(),
                    metadata: BTreeMap::new(),
                }
            }
            (MountKind::Artifacts, "local") => {
                let path = parcel_state_root.join("artifacts");
                fs::create_dir_all(&path).map_err(|source| CourierError::CreateDir {
                    path: path.display().to_string(),
                    source,
                })?;
                ResolvedMount {
                    kind: mount.kind,
                    driver: mount.driver.clone(),
                    target_path: path.display().to_string(),
                    metadata: BTreeMap::new(),
                }
            }
            _ => {
                return Err(CourierError::UnsupportedMount {
                    courier: courier_name.to_string(),
                    kind: mount.kind,
                    driver: mount.driver.clone(),
                });
            }
        };
        mounts.push(resolved);
    }

    Ok(mounts)
}

fn resolve_parcel_state_root(parcel: &LoadedParcel) -> PathBuf {
    if let Some(root) = std::env::var_os("DISPATCH_STATE_ROOT") {
        return PathBuf::from(root).join(&parcel.config.digest);
    }

    let parcel_dir = parcel.parcel_dir.as_path();
    if let Some(parent) = parcel_dir.parent()
        && parent.file_name().is_some_and(|name| name == "parcels")
        && let Some(dispatch_root) = parent.parent()
    {
        return dispatch_root.join("state").join(&parcel.config.digest);
    }

    parcel_dir
        .parent()
        .unwrap_or(parcel_dir)
        .join(".dispatch-state")
        .join(&parcel.config.digest)
}

fn ensure_parent_dir(path: &Path) -> Result<(), CourierError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| CourierError::CreateDir {
            path: parent.display().to_string(),
            source,
        })?;
    }
    Ok(())
}

fn touch_file(path: &Path) -> Result<(), CourierError> {
    if path.exists() {
        return Ok(());
    }
    fs::write(path, []).map_err(|source| CourierError::WriteFile {
        path: path.display().to_string(),
        source,
    })
}
