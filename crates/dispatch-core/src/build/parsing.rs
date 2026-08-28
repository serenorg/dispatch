//! Validators applied to the authored `[agent]` table during a build.
//!
//! Serde enforces shape and unknown fields; these checks enforce the value
//! constraints serde cannot express.

use super::{BuildError, InstructionConfig};
use crate::manifest::{
    A2aAuthConfig, CommandSpec, CourierTarget, InstructionKind, TestSpec, ToolConfig,
};
use std::{fs, path::Path};

pub(super) fn validate_courier_requirements(courier: &CourierTarget) -> Result<(), BuildError> {
    if courier.is_wasm() && courier.component().is_none() {
        return Err(BuildError::Validation(
            "a wasm `agent.courier_reference` target requires an `agent.component` path"
                .to_string(),
        ));
    }

    Ok(())
}

pub(super) fn validate_entrypoint_value(value: &str, context: &str) -> Result<String, BuildError> {
    match value {
        "chat" | "job" | "heartbeat" => Ok(value.to_string()),
        _ => Err(BuildError::Validation(format!(
            "{context} must be one of `chat`, `job`, or `heartbeat`, got `{value}`"
        ))),
    }
}

pub(super) fn validate_listener_path(value: &str, context: &str) -> Result<String, BuildError> {
    if value.starts_with('/') {
        Ok(if value == "/" {
            "/".to_string()
        } else {
            value.trim_end_matches('/').to_string()
        })
    } else {
        Err(BuildError::Validation(format!(
            "{context} must start with `/`, got `{value}`"
        )))
    }
}

pub(super) fn validate_listener_method(value: &str, context: &str) -> Result<String, BuildError> {
    let normalized = value.trim().to_ascii_uppercase();
    if normalized.is_empty()
        || !normalized
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte == b'-')
    {
        return Err(BuildError::Validation(format!(
            "{context} must be an uppercase HTTP method token, got `{value}`"
        )));
    }
    Ok(normalized)
}

pub(super) fn validate_test_specs(
    tests: &[TestSpec],
    tools: &[ToolConfig],
) -> Result<(), BuildError> {
    for test in tests {
        match test {
            TestSpec::Tool { tool } => {
                let declared = tools.iter().any(|candidate| match candidate {
                    ToolConfig::Local(local) => local.alias == *tool,
                    ToolConfig::A2a(a2a) => a2a.alias == *tool,
                    ToolConfig::Builtin(_) | ToolConfig::Mcp(_) => false,
                });
                if !declared {
                    return Err(BuildError::Validation(format!(
                        "`agent.tests.tool = \"{tool}\"` references an unknown local or A2A tool alias"
                    )));
                }
            }
        }
    }
    Ok(())
}

pub(super) fn validate_heartbeat_entrypoint(
    entrypoint: Option<&str>,
    instructions: &[InstructionConfig],
) -> Result<(), BuildError> {
    let has_heartbeat = instructions
        .iter()
        .any(|instruction| instruction.kind == InstructionKind::Heartbeat);
    if has_heartbeat && entrypoint != Some("heartbeat") {
        return Err(BuildError::Validation(
            "`agent.instructions.heartbeat` requires `agent.entrypoint = \"heartbeat\"`"
                .to_string(),
        ));
    }
    Ok(())
}

pub(super) fn validate_card_sha256(value: &str) -> Result<String, BuildError> {
    let valid = value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit());
    if !valid {
        return Err(BuildError::Validation(
            "`expect_card_sha256` must be a 64-character hex SHA256 digest".to_string(),
        ));
    }
    Ok(value.to_ascii_lowercase())
}

pub(super) fn validate_model_option(name: &str, value: &str) -> Result<String, BuildError> {
    match name {
        "persist-thread" => match value.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok("true".to_string()),
            "false" | "0" | "no" | "off" => Ok("false".to_string()),
            other => Err(BuildError::Validation(format!(
                "model option `persist-thread` must be `true` or `false`, got `{other}`"
            ))),
        },
        "reasoning-effort" => Ok(value.to_string()),
        _ => Err(BuildError::Validation(format!(
            "unsupported model option `{name}`"
        ))),
    }
}

pub(super) fn validate_timeout_duration(raw: &str, field: &str) -> Result<(), BuildError> {
    let trimmed = raw.trim();
    let value = if let Some(value) = trimmed.strip_suffix("ms") {
        value
    } else if let Some(value) = trimmed.strip_suffix('s') {
        value
    } else if let Some(value) = trimmed.strip_suffix('m') {
        value
    } else if let Some(value) = trimmed.strip_suffix('h') {
        value
    } else {
        return Err(invalid_timeout(raw, field));
    };
    if value
        .trim()
        .parse::<u64>()
        .is_ok_and(|duration| duration > 0)
    {
        Ok(())
    } else {
        Err(invalid_timeout(raw, field))
    }
}

fn invalid_timeout(raw: &str, field: &str) -> BuildError {
    BuildError::Validation(format!(
        "invalid `{field}` duration `{raw}`; expected a positive integer ending in ms, s, m, or h"
    ))
}

pub(super) fn validate_http_header_name(header_name: &str) -> Result<(), BuildError> {
    if header_name.is_empty()
        || !header_name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return Err(BuildError::Validation(format!(
            "invalid A2A auth header name `{header_name}`; expected ASCII letters, digits, or `-`"
        )));
    }
    Ok(())
}

pub(super) fn a2a_auth_secret_names(auth: &A2aAuthConfig) -> Vec<&str> {
    match auth {
        A2aAuthConfig::Bearer { secret_name } => vec![secret_name.as_str()],
        A2aAuthConfig::Header { secret_name, .. } => vec![secret_name.as_str()],
        A2aAuthConfig::Basic {
            username_secret_name,
            password_secret_name,
        } => vec![username_secret_name.as_str(), password_secret_name.as_str()],
    }
}

/// Runner inferred from a local tool file extension.
pub(super) fn infer_runner(packaged_path: &str) -> CommandSpec {
    let extension = Path::new(packaged_path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();

    match extension {
        "py" => CommandSpec {
            command: "python3".to_string(),
            args: Vec::new(),
        },
        "js" => CommandSpec {
            command: "node".to_string(),
            args: Vec::new(),
        },
        "ts" => CommandSpec {
            command: "tsx".to_string(),
            args: Vec::new(),
        },
        "sh" => CommandSpec {
            command: "sh".to_string(),
            args: Vec::new(),
        },
        "cmd" | "bat" => CommandSpec {
            command: "cmd".to_string(),
            args: vec![
                "/C".to_string(),
                format!(".\\{}", packaged_path.replace('/', "\\")),
            ],
        },
        _ => CommandSpec {
            command: packaged_path.to_string(),
            args: Vec::new(),
        },
    }
}

pub(super) fn validate_tool_schema(path: &Path, tool: &str) -> Result<(), BuildError> {
    let bytes = fs::read(path).map_err(|source| BuildError::ReadFile {
        path: path.display().to_string(),
        source,
    })?;
    let schema: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|source| BuildError::InvalidToolSchema {
            tool: tool.to_string(),
            path: path.display().to_string(),
            message: source.to_string(),
        })?;
    if !schema.is_object() {
        return Err(BuildError::InvalidToolSchema {
            tool: tool.to_string(),
            path: path.display().to_string(),
            message: "schema root must be a JSON object".to_string(),
        });
    }

    Ok(())
}
