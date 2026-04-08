use super::{
    BuiltinToolSpec, CourierError, InstructionKind, LoadedParcel, LocalToolSpec, LocalToolTarget,
    ParcelManifest, ToolConfig, ToolRunResult, build_local_tool_approval_request,
    check_tool_approval, execute_local_tool_with_env, instruction_heading, process_env_lookup,
    resolve_manifest_path,
};
use crate::{
    manifest::BuiltinToolConfig, resolve_secret_from_store, skill::strip_skill_frontmatter,
};
use jsonschema::Validator;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

static PARCEL_SCHEMA_VALIDATOR: OnceLock<Validator> = OnceLock::new();

pub fn load_parcel(path: &Path) -> Result<LoadedParcel, CourierError> {
    let manifest_path = resolve_manifest_path(path)?
        .canonicalize()
        .map_err(|source| CourierError::ReadFile {
            path: path.display().to_string(),
            source,
        })?;
    let parcel_dir = manifest_path.parent().map(PathBuf::from).ok_or_else(|| {
        CourierError::MissingParcelPath {
            path: manifest_path.display().to_string(),
        }
    })?;
    let source = fs::read_to_string(&manifest_path).map_err(|source| CourierError::ReadFile {
        path: manifest_path.display().to_string(),
        source,
    })?;
    let manifest_json = serde_json::from_str::<serde_json::Value>(&source).map_err(|source| {
        CourierError::ParseParcelManifest {
            path: manifest_path.display().to_string(),
            source,
        }
    })?;
    validate_parcel_schema(&manifest_path, &manifest_json)?;
    let config = serde_json::from_value::<ParcelManifest>(manifest_json).map_err(|source| {
        CourierError::ParseParcelManifest {
            path: manifest_path.display().to_string(),
            source,
        }
    })?;
    if config.schema != crate::manifest::PARCEL_SCHEMA_URL {
        return Err(CourierError::UnsupportedParcelSchema {
            path: manifest_path.display().to_string(),
            found: config.schema.clone(),
            expected: crate::manifest::PARCEL_SCHEMA_URL.to_string(),
        });
    }
    if config.format_version != crate::manifest::PARCEL_FORMAT_VERSION {
        return Err(CourierError::UnsupportedParcelFormatVersion {
            path: manifest_path.display().to_string(),
            found: config.format_version,
            supported: crate::manifest::PARCEL_FORMAT_VERSION,
        });
    }

    Ok(LoadedParcel {
        parcel_dir,
        manifest_path,
        config,
    })
}

pub fn resolve_prompt_text(parcel: &LoadedParcel) -> Result<String, CourierError> {
    let mut sections = Vec::new();

    for instruction in &parcel.config.instructions {
        if !matches!(
            instruction.kind,
            InstructionKind::Identity
                | InstructionKind::Soul
                | InstructionKind::Skill
                | InstructionKind::Agents
                | InstructionKind::User
                | InstructionKind::Tools
                | InstructionKind::Memory
                | InstructionKind::Heartbeat
        ) {
            continue;
        }
        let path = parcel
            .parcel_dir
            .join("context")
            .join(&instruction.packaged_path);
        let body = fs::read_to_string(&path).map_err(|source| CourierError::ReadFile {
            path: path.display().to_string(),
            source,
        })?;
        let body = if matches!(instruction.kind, InstructionKind::Skill)
            && instruction.skill_name.is_some()
        {
            strip_skill_frontmatter(&body)
        } else {
            body.as_str()
        };
        sections.push(format!(
            "# {}\n\n{}",
            instruction_heading(instruction.kind),
            body.trim_end()
        ));
    }

    for prompt in &parcel.config.inline_prompts {
        sections.push(format!("# PROMPT\n\n{}", prompt.trim_end()));
    }

    Ok(sections.join("\n\n"))
}

pub fn collect_skill_allowed_tools(parcel: &LoadedParcel) -> BTreeMap<String, Vec<String>> {
    parcel
        .config
        .instructions
        .iter()
        .filter_map(|instruction| {
            instruction
                .skill_name
                .as_ref()
                .zip(instruction.allowed_tools.as_ref())
                .map(|(skill_name, allowed_tools)| (skill_name.clone(), allowed_tools.clone()))
        })
        .collect()
}

pub fn list_local_tools(parcel: &LoadedParcel) -> Vec<LocalToolSpec> {
    parcel
        .config
        .tools
        .iter()
        .filter_map(|tool| match tool {
            ToolConfig::Local(local) => Some(LocalToolSpec {
                alias: local.alias.clone(),
                approval: local.approval,
                risk: local.risk,
                description: local.description.clone(),
                input_schema_packaged_path: local
                    .input_schema
                    .as_ref()
                    .map(|schema| schema.packaged_path.clone()),
                input_schema_sha256: local
                    .input_schema
                    .as_ref()
                    .map(|schema| schema.sha256.clone()),
                skill_source: local.skill_source.clone(),
                target: LocalToolTarget::Local {
                    packaged_path: local.packaged_path.clone(),
                    command: local.runner.command.clone(),
                    args: local.runner.args.clone(),
                },
            }),
            ToolConfig::A2a(a2a) => Some(LocalToolSpec {
                alias: a2a.alias.clone(),
                approval: a2a.approval,
                risk: a2a.risk,
                description: a2a.description.clone(),
                input_schema_packaged_path: a2a
                    .input_schema
                    .as_ref()
                    .map(|schema| schema.packaged_path.clone()),
                input_schema_sha256: a2a
                    .input_schema
                    .as_ref()
                    .map(|schema| schema.sha256.clone()),
                skill_source: None,
                target: LocalToolTarget::A2a {
                    endpoint_url: a2a.url.clone(),
                    endpoint_mode: a2a.endpoint_mode,
                    auth: a2a.auth.clone(),
                    expected_agent_name: a2a.expected_agent_name.clone(),
                    expected_card_sha256: a2a.expected_card_sha256.clone(),
                },
            }),
            _ => None,
        })
        .collect()
}

pub fn list_native_builtin_tools(parcel: &LoadedParcel) -> Vec<BuiltinToolSpec> {
    parcel
        .config
        .tools
        .iter()
        .filter_map(|tool| match tool {
            ToolConfig::Builtin(builtin) => builtin_memory_tool_spec(builtin),
            _ => None,
        })
        .collect()
}

fn builtin_memory_tool_spec(tool: &BuiltinToolConfig) -> Option<BuiltinToolSpec> {
    let input_schema = builtin_memory_tool_schema(&tool.capability)?;
    Some(BuiltinToolSpec {
        capability: tool.capability.clone(),
        approval: tool.approval,
        risk: tool.risk,
        description: tool.description.clone(),
        input_schema,
    })
}

fn builtin_memory_tool_schema(capability: &str) -> Option<serde_json::Value> {
    match capability {
        "memory_get" => Some(serde_json::json!({
            "type": "object",
            "properties": {
                "namespace": { "type": "string" },
                "key": { "type": "string" }
            },
            "required": ["key"],
            "additionalProperties": false
        })),
        "memory_put" => Some(serde_json::json!({
            "type": "object",
            "properties": {
                "namespace": { "type": "string" },
                "key": { "type": "string" },
                "value": { "type": "string" }
            },
            "required": ["key", "value"],
            "additionalProperties": false
        })),
        "memory_delete" => Some(serde_json::json!({
            "type": "object",
            "properties": {
                "namespace": { "type": "string" },
                "key": { "type": "string" }
            },
            "required": ["key"],
            "additionalProperties": false
        })),
        "memory_list" => Some(serde_json::json!({
            "type": "object",
            "properties": {
                "namespace": { "type": "string" },
                "prefix": { "type": "string" }
            },
            "additionalProperties": false
        })),
        "memory_list_range" => Some(serde_json::json!({
            "type": "object",
            "properties": {
                "namespace": { "type": "string" },
                "start_key": { "type": "string" },
                "end_key": { "type": "string" },
                "limit": { "type": "integer", "minimum": 1 }
            },
            "additionalProperties": false
        })),
        "memory_delete_range" => Some(serde_json::json!({
            "type": "object",
            "properties": {
                "namespace": { "type": "string" },
                "start_key": { "type": "string" },
                "end_key": { "type": "string" }
            },
            "additionalProperties": false
        })),
        "memory_put_many" => Some(serde_json::json!({
            "type": "object",
            "properties": {
                "namespace": { "type": "string" },
                "entries": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "key": { "type": "string" },
                            "value": { "type": "string" }
                        },
                        "required": ["key", "value"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["entries"],
            "additionalProperties": false
        })),
        "checkpoint_get" => Some(serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            },
            "required": ["name"],
            "additionalProperties": false
        })),
        "checkpoint_put" => Some(serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "value": { "type": "string" }
            },
            "required": ["name", "value"],
            "additionalProperties": false
        })),
        "checkpoint_delete" => Some(serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            },
            "required": ["name"],
            "additionalProperties": false
        })),
        "checkpoint_list" => Some(serde_json::json!({
            "type": "object",
            "properties": {
                "prefix": { "type": "string" }
            },
            "additionalProperties": false
        })),
        _ => None,
    }
}

pub(super) fn builtin_memory_tool_description(tool: &BuiltinToolSpec) -> String {
    tool.description
        .clone()
        .unwrap_or_else(|| match tool.capability.as_str() {
            "memory_get" => {
                "Read a value from the configured Dispatch memory mount by key.".to_string()
            }
            "memory_put" => {
                "Store or update a value in the configured Dispatch memory mount.".to_string()
            }
            "memory_delete" => {
                "Delete a value from the configured Dispatch memory mount by key.".to_string()
            }
            "memory_list" => {
                "List stored values from the configured Dispatch memory mount by prefix."
                    .to_string()
            }
            "memory_list_range" => {
                "List stored values from the configured Dispatch memory mount over a key range."
                    .to_string()
            }
            "memory_delete_range" => {
                "Delete stored values from the configured Dispatch memory mount over a key range."
                    .to_string()
            }
            "memory_put_many" => {
                "Store or update multiple values in the configured Dispatch memory mount."
                    .to_string()
            }
            "checkpoint_get" => {
                "Read a durable named checkpoint from the configured Dispatch session mount."
                    .to_string()
            }
            "checkpoint_put" => {
                "Store or update a durable named checkpoint in the configured Dispatch session mount."
                    .to_string()
            }
            "checkpoint_delete" => {
                "Delete a durable named checkpoint from the configured Dispatch session mount."
                    .to_string()
            }
            "checkpoint_list" => {
                "List durable named checkpoints from the configured Dispatch session mount."
                    .to_string()
            }
            _ => format!("Dispatch builtin capability `{}`.", tool.capability),
        })
}

pub fn run_local_tool(
    parcel: &LoadedParcel,
    tool_name: &str,
    input: Option<&str>,
) -> Result<ToolRunResult, CourierError> {
    run_local_tool_with_env(parcel, tool_name, input, process_env_lookup)
}

pub(super) fn run_local_tool_with_env<F>(
    parcel: &LoadedParcel,
    tool_name: &str,
    input: Option<&str>,
    mut env_lookup: F,
) -> Result<ToolRunResult, CourierError>
where
    F: FnMut(&str) -> Option<String>,
{
    let tool = resolve_local_tool_with_env(parcel, tool_name, &mut env_lookup)?;
    if let Some(request) = build_local_tool_approval_request(&tool, input)
        && !check_tool_approval(&request)?
    {
        return Err(CourierError::ApprovalDenied { tool: request.tool });
    }

    execute_local_tool_with_env(parcel, &tool, input, None, env_lookup)
}

pub(super) fn resolve_local_tool(
    parcel: &LoadedParcel,
    tool_name: &str,
) -> Result<LocalToolSpec, CourierError> {
    resolve_local_tool_with_env(parcel, tool_name, process_env_lookup)
}

fn resolve_local_tool_with_env<F>(
    parcel: &LoadedParcel,
    tool_name: &str,
    env_lookup: F,
) -> Result<LocalToolSpec, CourierError>
where
    F: FnMut(&str) -> Option<String>,
{
    validate_required_secrets_with(parcel, env_lookup)?;
    list_local_tools(parcel)
        .into_iter()
        .find(|tool| tool.matches_name(tool_name))
        .ok_or_else(|| CourierError::UnknownLocalTool {
            tool: tool_name.to_string(),
        })
}

pub(super) fn validate_required_secrets(parcel: &LoadedParcel) -> Result<(), CourierError> {
    validate_required_secrets_with(parcel, process_env_lookup)
}

pub(super) fn resolve_parcel_env_with<F>(
    parcel: &LoadedParcel,
    name: &str,
    env_lookup: &mut F,
) -> Result<Option<String>, CourierError>
where
    F: FnMut(&str) -> Option<String>,
{
    if let Some(value) = env_lookup(name) {
        return Ok(Some(value));
    }
    if !parcel
        .config
        .secrets
        .iter()
        .any(|secret| secret.name == name)
    {
        return Ok(None);
    }
    resolve_secret_from_store(&parcel.parcel_dir, name).map_err(|error| {
        CourierError::SecretLookup {
            name: name.to_string(),
            message: error.to_string(),
        }
    })
}

fn validate_required_secrets_with<F>(
    parcel: &LoadedParcel,
    mut env_lookup: F,
) -> Result<(), CourierError>
where
    F: FnMut(&str) -> Option<String>,
{
    for secret in &parcel.config.secrets {
        if secret.required
            && resolve_parcel_env_with(parcel, &secret.name, &mut env_lookup)?.is_none()
        {
            return Err(CourierError::MissingSecret {
                name: secret.name.clone(),
            });
        }
    }

    Ok(())
}

fn validate_parcel_schema(
    manifest_path: &Path,
    manifest_json: &serde_json::Value,
) -> Result<(), CourierError> {
    let validator = parcel_schema_validator();
    let mut errors = validator.iter_errors(manifest_json);
    if let Some(first) = errors.next() {
        let mut messages = vec![format_schema_error(&first)];
        for error in errors.take(7) {
            messages.push(format_schema_error(&error));
        }
        return Err(CourierError::InvalidParcelSchema {
            path: manifest_path.display().to_string(),
            message: messages.join("; "),
        });
    }
    Ok(())
}

fn parcel_schema_validator() -> &'static Validator {
    PARCEL_SCHEMA_VALIDATOR.get_or_init(|| {
        let schema = serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../../schemas/parcel.v1.json"
        ))
        .expect("embedded parcel schema must be valid JSON");
        jsonschema::validator_for(&schema).expect("embedded parcel schema must compile")
    })
}

fn format_schema_error(error: &jsonschema::ValidationError<'_>) -> String {
    let path = error.instance_path().to_string();
    if path.is_empty() {
        error.to_string()
    } else {
        format!("{path}: {error}")
    }
}
