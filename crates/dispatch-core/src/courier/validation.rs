use super::{
    CourierError, CourierKind, CourierOperation, CourierSession, LoadedParcel, Path, PathBuf,
};

pub(super) fn validate_courier_reference(
    courier_name: &str,
    kind: CourierKind,
    reference: &str,
) -> Result<(), CourierError> {
    if courier_reference_matches(kind, reference) {
        return Ok(());
    }

    Err(CourierError::IncompatibleCourier {
        courier: courier_name.to_string(),
        parcel_courier: reference.to_string(),
        supported: supported_courier_references(kind).join(", "),
    })
}

fn courier_reference_matches(kind: CourierKind, reference: &str) -> bool {
    let reference = reference.to_ascii_lowercase();
    let reference = reference.as_str();
    match kind {
        CourierKind::Native => {
            reference == "native"
                || reference == "dispatch/native"
                || reference.starts_with("dispatch/native:")
                || reference.starts_with("dispatch/native@")
        }
        CourierKind::Docker => {
            reference == "docker"
                || reference == "dispatch/docker"
                || reference.starts_with("dispatch/docker:")
                || reference.starts_with("dispatch/docker@")
        }
        CourierKind::Wasm => {
            reference == "wasm"
                || reference == "dispatch/wasm"
                || reference.starts_with("dispatch/wasm:")
                || reference.starts_with("dispatch/wasm@")
        }
        CourierKind::Custom => {
            reference == "custom"
                || reference == "dispatch/custom"
                || reference.starts_with("dispatch/custom:")
                || reference.starts_with("dispatch/custom@")
        }
    }
}

fn supported_courier_references(kind: CourierKind) -> &'static [&'static str] {
    match kind {
        CourierKind::Native => &["dispatch/native", "dispatch/native:<tag>", "native"],
        CourierKind::Docker => &["dispatch/docker", "dispatch/docker:<tag>", "docker"],
        CourierKind::Wasm => &["dispatch/wasm", "dispatch/wasm:<tag>", "wasm"],
        CourierKind::Custom => &["dispatch/custom", "dispatch/custom:<tag>", "custom"],
    }
}

pub(super) fn ensure_session_matches_parcel(
    image: &LoadedParcel,
    session: &CourierSession,
) -> Result<(), CourierError> {
    if session.parcel_digest != image.config.digest {
        return Err(CourierError::SessionParcelMismatch {
            session_parcel_digest: session.parcel_digest.clone(),
            parcel_digest: image.config.digest.clone(),
        });
    }

    Ok(())
}

pub(super) fn ensure_operation_matches_entrypoint(
    session: &CourierSession,
    operation: &CourierOperation,
) -> Result<(), CourierError> {
    let Some(entrypoint) = session.entrypoint.as_deref() else {
        return Ok(());
    };

    let Some(operation_name) = operation_entrypoint_name(operation) else {
        return Ok(());
    };

    if entrypoint == operation_name {
        return Ok(());
    }

    Err(CourierError::EntrypointMismatch {
        entrypoint: entrypoint.to_string(),
        operation: operation_name.to_string(),
    })
}

fn operation_entrypoint_name(operation: &CourierOperation) -> Option<&'static str> {
    match operation {
        CourierOperation::Chat { .. } => Some("chat"),
        CourierOperation::Job { .. } => Some("job"),
        CourierOperation::Heartbeat { .. } => Some("heartbeat"),
        CourierOperation::ResolvePrompt
        | CourierOperation::ListLocalTools
        | CourierOperation::InvokeTool { .. } => None,
    }
}

pub(super) fn resolve_manifest_path(path: &Path) -> Result<PathBuf, CourierError> {
    if !path.exists() {
        return Err(CourierError::MissingParcelPath {
            path: path.display().to_string(),
        });
    }

    if path.is_dir() {
        Ok(path.join("manifest.json"))
    } else {
        Ok(path.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn courier_reference_matching_is_case_insensitive() {
        assert!(courier_reference_matches(
            CourierKind::Native,
            "Dispatch/Native:latest",
        ));
        assert!(courier_reference_matches(
            CourierKind::Docker,
            "DISPATCH/DOCKER@sha256:abc123",
        ));
    }
}
