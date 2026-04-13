use super::{
    BuildError, ParcelFileRecord, ResolvedAgentSpec, hex_digest, infer_runner, package_path,
    relative_display, resolve_path, validate_entrypoint_value, validate_tool_schema,
};
use crate::{
    manifest::{
        CommandSpec, InstructionConfig, InstructionKind, LocalToolConfig, ToolConfig,
        ToolInputSchemaRef,
    },
    skill::{
        AgentSkillFrontmatter, DispatchSkillManifest, DispatchSkillTool, allowed_tool_warnings,
        dispatch_skill_manifest_path, parse_skill_markdown, validate_agent_skill_frontmatter,
    },
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub(super) fn process_skill_instruction(
    context_dir: &Path,
    source_path: &str,
    packaged: &mut BTreeMap<String, Vec<u8>>,
    files: &mut Vec<ParcelFileRecord>,
    resolved: &mut ResolvedAgentSpec,
) -> Result<(), BuildError> {
    let resolved_path = resolve_path(context_dir, source_path)?;
    if resolved_path.is_file() {
        let file_record = package_path(context_dir, &resolved_path, packaged)?;
        resolved.instructions.push(InstructionConfig {
            kind: InstructionKind::Skill,
            packaged_path: source_path.to_string(),
            sha256: file_record.sha256.clone(),
            skill_name: None,
            allowed_tools: None,
        });
        files.extend(file_record.expand());
        return Ok(());
    }

    let skill_dir = resolved_path;
    let skill_md_path = skill_dir.join("SKILL.md");
    if !skill_md_path.exists() {
        return Err(BuildError::Validation(format!(
            "SKILL directory `{source_path}` must contain `SKILL.md`"
        )));
    }

    let skill_source =
        fs::read_to_string(&skill_md_path).map_err(|source| BuildError::ReadFile {
            path: skill_md_path.display().to_string(),
            source,
        })?;
    let parsed_skill = parse_skill_markdown(&skill_source).map_err(|source| {
        BuildError::Validation(format!(
            "failed to parse Agent Skills frontmatter in `{}`: {source}",
            skill_md_path.display()
        ))
    })?;
    validate_agent_skill_frontmatter(&skill_dir, &parsed_skill.frontmatter)
        .map_err(BuildError::Validation)?;

    let bundle_record = package_path(context_dir, &skill_dir, packaged)?;
    let skill_md_packaged_path = relative_display(context_dir, &skill_md_path);
    let skill_file_record = bundle_record
        .entries
        .iter()
        .find(|entry| entry.packaged_as == skill_md_packaged_path)
        .cloned()
        .ok_or_else(|| {
            BuildError::Validation(format!(
                "packaged skill bundle `{}` did not include `{}`",
                skill_dir.display(),
                skill_md_packaged_path
            ))
        })?;
    files.extend(bundle_record.expand());

    resolved.instructions.push(InstructionConfig {
        kind: InstructionKind::Skill,
        packaged_path: skill_md_packaged_path.clone(),
        sha256: skill_file_record.sha256.clone(),
        skill_name: Some(parsed_skill.frontmatter.name.clone()),
        allowed_tools: parsed_skill.frontmatter.allowed_tools.clone(),
    });

    if let Some(sidecar) =
        resolve_skill_dispatch_manifest_path(&skill_dir, &parsed_skill.frontmatter)?
    {
        let sidecar_path = sidecar.path();
        let sidecar_source =
            fs::read_to_string(sidecar_path).map_err(|source| BuildError::ReadFile {
                path: sidecar_path.display().to_string(),
                source,
            })?;
        let skill_manifest: DispatchSkillManifest =
            toml::from_str(&sidecar_source).map_err(|source| {
                let mut message = format!(
                    "failed to parse Dispatch skill manifest `{}`: {source}",
                    sidecar_path.display()
                );
                if sidecar.is_auto_detected() {
                    message.push_str(
                        "; `skill.toml` is reserved for skill sidecars and is auto-detected inside skill directories. Rename the file or set `metadata.dispatch-manifest` to an explicit sidecar path."
                    );
                }
                BuildError::Validation(message)
            })?;

        if let Some(entrypoint) = skill_manifest.entrypoint.as_deref() {
            let entrypoint = validate_entrypoint_value(
                entrypoint,
                &format!("skill sidecar `{}` entrypoint", sidecar_path.display()),
            )?;
            if !resolved.entrypoint_declared {
                match resolved.entrypoint.as_deref() {
                    None => resolved.entrypoint = Some(entrypoint),
                    Some(existing) if existing == entrypoint => {}
                    Some(existing) => {
                        return Err(BuildError::Validation(format!(
                            "skill sidecar `{}` entrypoint `{}` conflicts with previously resolved entrypoint `{existing}`",
                            sidecar_path.display(),
                            entrypoint
                        )));
                    }
                }
            }
        }

        resolved
            .skill_tool_aliases
            .entry(parsed_skill.frontmatter.name.clone())
            .or_default()
            .extend(skill_manifest.tools.iter().map(|tool| tool.name.clone()));

        for skill_tool in skill_manifest.tools {
            let mut tool = synthesize_skill_tool(
                context_dir,
                &skill_dir,
                &parsed_skill.frontmatter.name,
                &skill_tool,
            )?;
            package_tool_config(context_dir, packaged, files, &mut tool)?;
            insert_resolved_tool(&mut resolved.tools, &mut resolved.warnings, tool)?;
        }
    }

    Ok(())
}

enum SkillDispatchManifestSource {
    Explicit(PathBuf),
    AutoDetected(PathBuf),
}

impl SkillDispatchManifestSource {
    fn path(&self) -> &Path {
        match self {
            Self::Explicit(path) | Self::AutoDetected(path) => path,
        }
    }

    fn is_auto_detected(&self) -> bool {
        matches!(self, Self::AutoDetected(_))
    }
}

fn resolve_skill_dispatch_manifest_path(
    skill_dir: &Path,
    frontmatter: &AgentSkillFrontmatter,
) -> Result<Option<SkillDispatchManifestSource>, BuildError> {
    if let Some(path) = dispatch_skill_manifest_path(frontmatter) {
        return Ok(Some(SkillDispatchManifestSource::Explicit(
            resolve_skill_member_path(skill_dir, path)?,
        )));
    }
    let default = skill_dir.join("skill.toml");
    if default.is_file() {
        return Ok(Some(SkillDispatchManifestSource::AutoDetected(
            resolve_skill_member_path(skill_dir, "skill.toml")?,
        )));
    }
    Ok(None)
}

fn resolve_skill_member_path(skill_dir: &Path, relative: &str) -> Result<PathBuf, BuildError> {
    debug_assert!(skill_dir.is_absolute());
    let joined = skill_dir.join(relative);
    if !joined.exists() {
        return Err(BuildError::MissingPath {
            path: relative.to_string(),
        });
    }
    let resolved = joined
        .canonicalize()
        .map_err(|source| BuildError::ReadFile {
            path: joined.display().to_string(),
            source,
        })?;
    if !resolved.starts_with(skill_dir) {
        return Err(BuildError::Validation(format!(
            "skill path `{relative}` escapes skill directory `{}`",
            skill_dir.display()
        )));
    }
    Ok(resolved)
}

fn synthesize_skill_tool(
    context_dir: &Path,
    skill_dir: &Path,
    skill_source: &str,
    skill_tool: &DispatchSkillTool,
) -> Result<ToolConfig, BuildError> {
    let resolved_script = resolve_skill_member_path(skill_dir, &skill_tool.script)?;
    let packaged_path = relative_display(context_dir, &resolved_script);
    let runner = match &skill_tool.runner {
        Some(runner) => CommandSpec {
            command: runner.clone(),
            args: skill_tool.args.clone(),
        },
        None => {
            let mut inferred = infer_runner(&packaged_path);
            inferred.args = skill_tool.args.clone();
            inferred
        }
    };

    let input_schema = if let Some(schema_path) = &skill_tool.schema {
        let resolved_schema = resolve_skill_member_path(skill_dir, schema_path)?;
        validate_tool_schema(&resolved_schema, &skill_tool.name)?;
        let schema_bytes = fs::read(&resolved_schema).map_err(|source| BuildError::ReadFile {
            path: resolved_schema.display().to_string(),
            source,
        })?;
        Some(ToolInputSchemaRef {
            packaged_path: relative_display(context_dir, &resolved_schema),
            sha256: hex_digest(&schema_bytes),
        })
    } else {
        None
    };

    Ok(ToolConfig::Local(LocalToolConfig {
        alias: skill_tool.name.clone(),
        packaged_path,
        runner,
        approval: skill_tool.approval,
        risk: skill_tool.risk,
        description: skill_tool.description.clone(),
        input_schema,
        skill_source: Some(skill_source.to_string()),
    }))
}

pub(super) fn package_tool_config(
    context_dir: &Path,
    packaged: &mut BTreeMap<String, Vec<u8>>,
    files: &mut Vec<ParcelFileRecord>,
    tool: &mut ToolConfig,
) -> Result<(), BuildError> {
    match tool {
        ToolConfig::Local(local) => {
            let resolved_path = resolve_path(context_dir, &local.packaged_path)?;
            let file_record = package_path(context_dir, &resolved_path, packaged)?;
            files.extend(file_record.expand());
            if let Some(schema) = &local.input_schema {
                let resolved_schema_path = resolve_path(context_dir, &schema.packaged_path)?;
                validate_tool_schema(&resolved_schema_path, &local.alias)?;
                let schema_record = package_path(context_dir, &resolved_schema_path, packaged)?;
                local.input_schema = Some(ToolInputSchemaRef {
                    packaged_path: schema.packaged_path.clone(),
                    sha256: schema_record.sha256.clone(),
                });
                files.extend(schema_record.expand());
            }
        }
        ToolConfig::A2a(tool) => {
            if let Some(schema) = &tool.input_schema {
                let resolved_schema_path = resolve_path(context_dir, &schema.packaged_path)?;
                validate_tool_schema(&resolved_schema_path, &tool.alias)?;
                let schema_record = package_path(context_dir, &resolved_schema_path, packaged)?;
                tool.input_schema = Some(ToolInputSchemaRef {
                    packaged_path: schema.packaged_path.clone(),
                    sha256: schema_record.sha256.clone(),
                });
                files.extend(schema_record.expand());
            }
        }
        ToolConfig::Builtin(_) | ToolConfig::Mcp(_) => {}
    }
    Ok(())
}

pub(super) fn insert_resolved_tool(
    tools: &mut Vec<ToolConfig>,
    warnings: &mut Vec<String>,
    tool: ToolConfig,
) -> Result<(), BuildError> {
    let alias = tool_alias(&tool).map(ToOwned::to_owned);
    if let Some(alias) = alias
        && let Some(existing) = tools
            .iter_mut()
            .find(|existing| tool_alias(existing) == Some(alias.as_str()))
    {
        match (tool_skill_source(existing), tool_skill_source(&tool)) {
            (Some(previous_skill_source), Some(new_skill_source)) => {
                let message = if previous_skill_source == new_skill_source {
                    format!(
                        "tool `{alias}` is declared more than once by skill `{previous_skill_source}`"
                    )
                } else {
                    format!(
                        "tool `{alias}` is declared by multiple skills (`{previous_skill_source}` and `{new_skill_source}`)"
                    )
                };
                return Err(BuildError::Validation(message));
            }
            (Some(previous_skill_source), None) => {
                warnings.push(format!(
                    "tool `{alias}` from skill `{previous_skill_source}` overridden by an explicit Agentfile tool declaration"
                ));
                *existing = tool;
                return Ok(());
            }
            (None, Some(new_skill_source)) => {
                warnings.push(format!(
                    "tool `{alias}` from skill `{new_skill_source}` was shadowed by an explicit Agentfile tool declaration"
                ));
                return Ok(());
            }
            (None, None) => {
                return Err(BuildError::Validation(format!(
                    "tool `{alias}` is declared more than once in the Agentfile"
                )));
            }
        }
    }
    tools.push(tool);
    Ok(())
}

fn tool_alias(tool: &ToolConfig) -> Option<&str> {
    match tool {
        ToolConfig::Local(local) => Some(local.alias.as_str()),
        ToolConfig::A2a(tool) => Some(tool.alias.as_str()),
        ToolConfig::Builtin(_) | ToolConfig::Mcp(_) => None,
    }
}

fn tool_skill_source(tool: &ToolConfig) -> Option<&str> {
    match tool {
        ToolConfig::Local(local) => local.skill_source.as_deref(),
        ToolConfig::Builtin(_) | ToolConfig::Mcp(_) | ToolConfig::A2a(_) => None,
    }
}

pub(super) fn skill_allowed_tool_build_warnings(
    instructions: &[InstructionConfig],
    skill_tool_aliases: &BTreeMap<String, Vec<String>>,
    tools: &[ToolConfig],
) -> Vec<String> {
    let parcel_tool_names = tools
        .iter()
        .filter_map(model_visible_tool_name)
        .map(ToOwned::to_owned)
        .collect::<std::collections::BTreeSet<_>>();

    instructions
        .iter()
        .filter_map(|instruction| {
            instruction
                .skill_name
                .as_ref()
                .map(|skill_name| (skill_name, instruction.allowed_tools.as_deref()))
        })
        .flat_map(|(skill_name, allowed_tools)| {
            let own_aliases = skill_tool_aliases
                .get(skill_name)
                .map(|aliases| aliases.as_slice())
                .unwrap_or(&[]);
            allowed_tool_warnings(skill_name, allowed_tools, own_aliases, &parcel_tool_names)
        })
        .collect()
}

fn model_visible_tool_name(tool: &ToolConfig) -> Option<&str> {
    match tool {
        ToolConfig::Local(local) => Some(local.alias.as_str()),
        ToolConfig::Builtin(builtin) => Some(builtin.capability.as_str()),
        ToolConfig::A2a(tool) => Some(tool.alias.as_str()),
        ToolConfig::Mcp(_) => None,
    }
}
