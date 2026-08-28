//! Lowering from the authored `[agent]` table into a `ResolvedAgentSpec`.
//!
//! Every declared field reaches the manifest or fails the build. There is no
//! silent-drop path: a value that cannot be lowered is a validation error.

use super::{
    BuildError, ResolvedAgentSpec, package_path, parsing, resolve_path,
    skill::{insert_resolved_tool, package_tool_config, process_skill_bundle},
};
use crate::agent_config::{AgentConfig, ToolConfigEntry};
use crate::manifest::{
    A2aToolConfig, BuiltinToolConfig, CommandSpec, CompactionConfig, CourierTarget, EnvVar,
    FrameworkProvenance, InstructionConfig, InstructionKind, LimitSpec, LocalToolConfig,
    McpToolConfig, ModelReference, MountConfig, NetworkRule, ParcelFileRecord, SecretSpec,
    TestSpec, TimeoutSpec, ToolConfig, ToolInputSchemaRef,
};
use std::collections::BTreeMap;
use std::path::Path;

pub(super) fn lower_agent_config(
    config: &AgentConfig,
    context_dir: &Path,
    config_path: &Path,
    packaged: &mut BTreeMap<String, Vec<u8>>,
    files: &mut Vec<ParcelFileRecord>,
    resolved: &mut ResolvedAgentSpec,
) -> Result<(), BuildError> {
    resolved.courier = Some(CourierTarget::from_reference(
        config.courier_reference.clone(),
    ));
    resolved.name = config.name.clone();
    resolved.version = config.version.clone();
    resolved.framework = config
        .framework
        .as_ref()
        .map(|framework| FrameworkProvenance {
            name: framework.name.clone(),
            version: framework.version.clone(),
            target: framework.target.clone(),
        });
    resolved.visibility = config.visibility;
    resolved.schedules = config.schedules.clone();
    resolved.listeners = config.listeners.clone();
    resolved.inline_prompts = config.prompts.clone();
    resolved.labels = config.labels.clone();

    if let Some(entrypoint) = &config.entrypoint {
        resolved.entrypoint = Some(parsing::validate_entrypoint_value(
            entrypoint,
            "`agent.entrypoint`",
        )?);
        resolved.entrypoint_declared = true;
    }

    if let Some(component) = &config.component {
        super::component::package_component(
            context_dir,
            config_path,
            component,
            packaged,
            files,
            resolved,
        )?;
    }

    if let Some(ingress) = &config.ingress {
        if let Some(path) = &ingress.path {
            resolved.ingress_path = Some(parsing::validate_listener_path(
                path,
                "`agent.ingress.path`",
            )?);
        }
        for method in &ingress.methods {
            resolved
                .ingress_methods
                .push(parsing::validate_listener_method(
                    method,
                    "`agent.ingress.methods`",
                )?);
        }
        resolved.ingress_secret_env = ingress.secret_env.clone();
        resolved.ingress_max_body_bytes =
            validate_size_limit(ingress.max_body_bytes, "`agent.ingress.max_body_bytes`")?;
        resolved.ingress_max_header_bytes =
            validate_size_limit(ingress.max_header_bytes, "`agent.ingress.max_header_bytes`")?;
    }

    resolved.env = config
        .env
        .iter()
        .map(|(name, value)| EnvVar {
            name: name.clone(),
            value: value.clone(),
        })
        .collect();

    resolved.secrets = config
        .secrets
        .iter()
        .map(|secret| SecretSpec {
            name: secret.name.clone(),
            required: secret.required,
        })
        .collect();

    resolved.mounts = config
        .mounts
        .iter()
        .map(|mount| MountConfig {
            kind: mount.kind,
            driver: mount.driver.clone(),
            options: mount.options.clone(),
        })
        .collect();

    resolved.network = config
        .network
        .iter()
        .map(|rule| NetworkRule {
            action: rule.action.clone(),
            target: rule.target.clone(),
            qualifiers: rule.qualifiers.clone(),
        })
        .collect();

    for (scope, value) in config.limits.entries() {
        resolved.limits.push(LimitSpec {
            scope: scope.to_string(),
            value: value.to_string(),
        });
    }

    for (scope, duration) in config.timeouts.entries() {
        let field = format!("agent.timeouts.{}", scope.to_ascii_lowercase());
        parsing::validate_timeout_duration(duration, &field)?;
        resolved.timeouts.push(TimeoutSpec {
            scope: scope.to_string(),
            duration: duration.to_string(),
        });
    }

    if let Some(compaction) = &config.compaction {
        resolved.compaction = Some(CompactionConfig {
            interval: compaction.interval.clone(),
            overlap: compaction.overlap,
        });
    }

    if let Some(model) = &config.model {
        resolved.models.routing = model.routing.clone();
        if let Some(id) = &model.id {
            resolved.models.primary = Some(ModelReference {
                id: id.clone(),
                provider: model.provider.clone(),
                options: validate_model_options(&model.options)?,
            });
        } else if model.provider.is_some() || !model.options.is_empty() {
            return Err(BuildError::Validation(
                "`agent.model.provider` and `agent.model.options` require `agent.model.id`"
                    .to_string(),
            ));
        }
        for fallback in &model.fallbacks {
            resolved.models.fallbacks.push(ModelReference {
                id: fallback.id.clone(),
                provider: fallback.provider.clone(),
                options: validate_model_options(&fallback.options)?,
            });
        }
    }

    let instructions = &config.instructions;
    lower_instruction_entries(
        [
            (InstructionKind::Identity, instructions.identity.as_deref()),
            (InstructionKind::Soul, instructions.soul.as_deref()),
            (InstructionKind::Skill, instructions.skill.as_deref()),
        ],
        context_dir,
        config_path,
        packaged,
        files,
        resolved,
    )?;

    for skill_dir in &config.skills {
        process_skill_bundle(
            context_dir,
            config_path,
            skill_dir,
            packaged,
            files,
            resolved,
        )?;
    }

    lower_instruction_entries(
        [
            (InstructionKind::Agents, instructions.agents.as_deref()),
            (InstructionKind::User, instructions.user.as_deref()),
            (InstructionKind::Tools, instructions.tools.as_deref()),
            (InstructionKind::Memory, instructions.memory.as_deref()),
            (
                InstructionKind::Heartbeat,
                instructions.heartbeat.as_deref(),
            ),
        ]
        .into_iter()
        .chain(
            config
                .evals
                .iter()
                .map(|path| (InstructionKind::Eval, Some(path.as_str()))),
        ),
        context_dir,
        config_path,
        packaged,
        files,
        resolved,
    )?;

    for entry in &config.tools {
        let mut tool = lower_tool(entry)?;
        package_tool_config(context_dir, config_path, packaged, files, &mut tool)?;
        insert_resolved_tool(&mut resolved.tools, &mut resolved.warnings, tool)?;
    }

    for test in &config.tests {
        if test.tool.trim().is_empty() {
            return Err(BuildError::Validation(
                "`agent.tests.tool` requires a non-empty tool alias".to_string(),
            ));
        }
        resolved.tests.push(TestSpec::Tool {
            tool: test.tool.clone(),
        });
    }

    for file in &config.files {
        let resolved_path = resolve_path(context_dir, file)?;
        let record = package_path(context_dir, config_path, &resolved_path, packaged)?;
        files.extend(record.expand());
    }

    Ok(())
}

fn lower_instruction_entries<'a>(
    entries: impl IntoIterator<Item = (InstructionKind, Option<&'a str>)>,
    context_dir: &Path,
    config_path: &Path,
    packaged: &mut BTreeMap<String, Vec<u8>>,
    files: &mut Vec<ParcelFileRecord>,
    resolved: &mut ResolvedAgentSpec,
) -> Result<(), BuildError> {
    for (kind, source_path) in entries
        .into_iter()
        .filter_map(|(kind, path)| path.map(|path| (kind, path)))
    {
        let resolved_path = resolve_path(context_dir, source_path)?;
        let record = package_path(context_dir, config_path, &resolved_path, packaged)?;
        resolved.instructions.push(InstructionConfig {
            kind,
            packaged_path: source_path.to_string(),
            sha256: record.sha256.clone(),
            skill_name: None,
            allowed_tools: None,
        });
        files.extend(record.expand());
    }

    Ok(())
}

fn lower_tool(entry: &ToolConfigEntry) -> Result<ToolConfig, BuildError> {
    match entry {
        ToolConfigEntry::Builtin(tool) => Ok(ToolConfig::Builtin(BuiltinToolConfig {
            capability: tool.name.clone(),
            approval: tool.approval,
            risk: tool.risk,
            description: tool.description.clone(),
        })),
        ToolConfigEntry::Mcp(tool) => Ok(ToolConfig::Mcp(McpToolConfig {
            server: tool.server.clone(),
            approval: tool.approval,
            risk: tool.risk,
            description: tool.description.clone(),
        })),
        ToolConfigEntry::Local(tool) => {
            let alias = tool.alias.clone().unwrap_or_else(|| {
                Path::new(&tool.path)
                    .file_stem()
                    .map(|value| value.to_string_lossy().to_string())
                    .unwrap_or_else(|| tool.path.clone())
            });
            let runner = match &tool.runner {
                Some(runner) => CommandSpec {
                    command: runner.command.clone(),
                    args: runner.args.clone(),
                },
                None => parsing::infer_runner(&tool.path),
            };
            Ok(ToolConfig::Local(LocalToolConfig {
                alias,
                packaged_path: tool.path.clone(),
                runner,
                approval: tool.approval,
                risk: tool.risk,
                description: tool.description.clone(),
                input_schema: tool.schema.clone().map(|packaged_path| ToolInputSchemaRef {
                    packaged_path,
                    sha256: String::new(),
                }),
                skill_source: None,
            }))
        }
        ToolConfigEntry::A2a(tool) => {
            let expected_card_sha256 = match &tool.expect_card_sha256 {
                Some(value) => Some(parsing::validate_card_sha256(value)?),
                None => None,
            };
            if matches!(
                tool.discovery,
                Some(crate::manifest::A2aEndpointMode::Direct)
            ) && (tool.expect_agent_name.is_some() || expected_card_sha256.is_some())
            {
                return Err(BuildError::Validation(format!(
                    "A2A tool `{}` cannot use `discovery = \"direct\"` with `expect_agent_name` or `expect_card_sha256`",
                    tool.alias
                )));
            }
            let auth = match tool.auth.clone() {
                Some(auth) => {
                    let auth = auth.into_manifest();
                    if let crate::manifest::A2aAuthConfig::Header { header_name, .. } = &auth {
                        parsing::validate_http_header_name(header_name)?;
                    }
                    Some(auth)
                }
                None => None,
            };
            Ok(ToolConfig::A2a(A2aToolConfig {
                alias: tool.alias.clone(),
                url: tool.url.clone(),
                endpoint_mode: tool.discovery,
                auth,
                expected_agent_name: tool.expect_agent_name.clone(),
                expected_card_sha256,
                approval: tool.approval,
                risk: tool.risk,
                description: tool.description.clone(),
                input_schema: tool.schema.clone().map(|packaged_path| ToolInputSchemaRef {
                    packaged_path,
                    sha256: String::new(),
                }),
            }))
        }
    }
}

fn validate_size_limit(value: Option<usize>, field: &str) -> Result<Option<usize>, BuildError> {
    match value {
        Some(0) => Err(BuildError::Validation(format!(
            "{field} must be greater than zero"
        ))),
        other => Ok(other),
    }
}

fn validate_model_options(
    options: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, BuildError> {
    let mut canonical = BTreeMap::new();
    for (name, value) in options {
        canonical.insert(name.clone(), parsing::validate_model_option(name, value)?);
    }
    Ok(canonical)
}
