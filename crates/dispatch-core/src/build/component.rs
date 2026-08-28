use super::{BuildError, ParcelFileRecord, ResolvedAgentSpec, package_path, resolve_path};
use crate::DISPATCH_WASM_ABI;
use crate::manifest::WasmComponentConfig;
use std::{collections::BTreeMap, path::Path};

pub(super) fn package_component(
    context_dir: &Path,
    config_path: &Path,
    source_path: &str,
    packaged: &mut BTreeMap<String, Vec<u8>>,
    files: &mut Vec<ParcelFileRecord>,
    resolved: &mut ResolvedAgentSpec,
) -> Result<(), BuildError> {
    let source_path = source_path.to_string();
    let resolved_path = resolve_path(context_dir, &source_path)?;
    let file_record = package_path(context_dir, config_path, &resolved_path, packaged)?;
    let component_sha256 = file_record.sha256.clone();
    files.extend(file_record.expand());

    let courier = resolved.courier.as_mut().ok_or_else(|| {
        BuildError::Validation("`agent.component` requires `agent.courier_reference`".to_string())
    })?;
    if !courier.is_wasm() {
        return Err(BuildError::Validation(
            "`agent.component` is only supported when `agent.courier_reference` targets wasm"
                .to_string(),
        ));
    }
    courier.set_component(WasmComponentConfig {
        packaged_path: source_path,
        sha256: component_sha256,
        abi: DISPATCH_WASM_ABI.to_string(),
    });
    Ok(())
}
