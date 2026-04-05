use super::{
    BuildError, ParcelFileRecord, ResolvedAgentSpec, Value, package_path, parse_component,
    resolve_path,
};
use crate::DISPATCH_WASM_ABI;
use crate::manifest::WasmComponentConfig;
use std::{collections::BTreeMap, path::Path};

pub(super) fn process_component_instruction(
    context_dir: &Path,
    args: &[Value],
    line: usize,
    packaged: &mut BTreeMap<String, Vec<u8>>,
    files: &mut Vec<ParcelFileRecord>,
    resolved: &mut ResolvedAgentSpec,
) -> Result<(), BuildError> {
    let component = parse_component(args);
    let source_path = component.packaged_path.clone();
    let resolved_path = resolve_path(context_dir, &source_path)?;
    let file_record = package_path(context_dir, &resolved_path, packaged)?;
    let component_sha256 = file_record.sha256.clone();
    files.extend(file_record.expand());

    let courier = resolved.courier.as_mut().ok_or_else(|| {
        BuildError::Validation(format!(
            "line {line}: `COMPONENT` requires a preceding `FROM` instruction"
        ))
    })?;
    if !courier.is_wasm() {
        return Err(BuildError::Validation(
            "`COMPONENT` is only supported for `dispatch/wasm` courier targets".to_string(),
        ));
    }
    courier.set_component(WasmComponentConfig {
        packaged_path: source_path,
        sha256: component_sha256,
        abi: DISPATCH_WASM_ABI.to_string(),
    });
    Ok(())
}
