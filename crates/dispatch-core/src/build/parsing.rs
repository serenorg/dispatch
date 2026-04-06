use super::{BuildError, InstructionConfig, scalar_at, scalars};
use crate::{
    ParsedAgentfile, Value,
    manifest::{
        A2aAuthConfig, A2aEndpointMode, A2aToolConfig, BuiltinToolConfig, CommandSpec,
        CompactionConfig, CourierTarget, EnvVar, FrameworkProvenance, InstructionKind, LimitSpec,
        LocalToolConfig, McpToolConfig, ModelReference, MountConfig, MountKind, NetworkRule,
        TestSpec, TimeoutSpec, ToolApprovalPolicy, ToolConfig, ToolInputSchemaRef, ToolRiskLevel,
        Visibility,
    },
    validate::{Level, validate_agentfile},
};
use std::collections::BTreeMap;
use std::{fs, path::Path};

pub(super) fn validate_for_build(parsed: &ParsedAgentfile) -> Result<(), BuildError> {
    let report = validate_agentfile(parsed);
    if report.is_ok() {
        return Ok(());
    }

    let details = report
        .diagnostics
        .into_iter()
        .filter(|diagnostic| diagnostic.level == Level::Error)
        .map(|diagnostic| format!("line {}: {}", diagnostic.line, diagnostic.message))
        .collect::<Vec<_>>()
        .join("\n");

    Err(BuildError::Validation(details))
}

pub(super) fn validate_courier_requirements(courier: &CourierTarget) -> Result<(), BuildError> {
    if courier.is_wasm() && courier.component().is_none() {
        return Err(BuildError::Validation(
            "line 1: `FROM dispatch/wasm...` requires a `COMPONENT <path>` instruction".to_string(),
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

pub(super) fn parse_listener_size_limit(value: &str, context: &str) -> Result<usize, BuildError> {
    let parsed = value.parse::<usize>().map_err(|_| {
        BuildError::Validation(format!(
            "{context} must be a positive integer, got `{value}`"
        ))
    })?;
    if parsed == 0 {
        return Err(BuildError::Validation(format!(
            "{context} must be greater than zero"
        )));
    }
    Ok(parsed)
}

pub(super) fn parse_test_spec(args: &[Value], line: usize) -> Result<TestSpec, BuildError> {
    let target = scalar_at(args, 0);
    let Some(tool) = target.strip_prefix("tool:") else {
        return Err(BuildError::Validation(format!(
            "line {line}: `TEST` currently only supports `tool:<alias>` targets"
        )));
    };
    if tool.is_empty() {
        return Err(BuildError::Validation(format!(
            "line {line}: `TEST tool:<alias>` requires a non-empty tool alias"
        )));
    }
    Ok(TestSpec::Tool {
        tool: tool.to_string(),
    })
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
                        "`TEST tool:{tool}` references an unknown local or A2A tool alias"
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
            "`HEARTBEAT` requires `ENTRYPOINT heartbeat`".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn parse_visibility(value: &str) -> Option<Visibility> {
    match value {
        "open" => Some(Visibility::Open),
        "opaque" => Some(Visibility::Opaque),
        _ => None,
    }
}

pub(super) fn parse_framework(args: &[Value]) -> Option<FrameworkProvenance> {
    let tokens = scalars(args);
    let name = tokens.first()?.clone();
    let mut version = None;
    let mut target = None;

    let mut index = 1;
    while index < tokens.len() {
        match tokens[index].as_str() {
            "VERSION" if index + 1 < tokens.len() => {
                version = Some(tokens[index + 1].clone());
                index += 2;
            }
            "TARGET" if index + 1 < tokens.len() => {
                target = Some(tokens[index + 1].clone());
                index += 2;
            }
            _ => index += 1,
        }
    }

    Some(FrameworkProvenance {
        name,
        version,
        target,
    })
}

pub(super) fn parse_model_reference(
    args: &[Value],
    line: usize,
) -> Result<Option<ModelReference>, BuildError> {
    let tokens = scalars(args);
    let Some(id) = tokens.first().cloned() else {
        return Ok(None);
    };
    let mut provider = None;
    let mut options = BTreeMap::new();

    let mut index = 1;
    while index < tokens.len() {
        let token = &tokens[index];
        match token.as_str() {
            "PROVIDER" if index + 1 < tokens.len() => {
                provider = Some(tokens[index + 1].clone());
                index += 2;
            }
            "PROVIDER" => {
                return Err(BuildError::Validation(format!(
                    "line {line}: `PROVIDER` requires a value"
                )));
            }
            legacy if legacy.eq_ignore_ascii_case("PERSIST_HISTORY") => {
                return Err(BuildError::Validation(format!(
                    "line {line}: `PERSIST_HISTORY` is no longer supported; use `--persist-thread=<true|false>`"
                )));
            }
            token if token.starts_with("--") => {
                let (name, raw_value) = parse_model_option_flag(token, line)?;
                let canonical_value = validate_model_option(&name, &raw_value, line)?;
                if options.insert(name.clone(), canonical_value).is_some() {
                    return Err(BuildError::Validation(format!(
                        "line {line}: duplicate model option `--{name}`"
                    )));
                }
                index += 1;
            }
            _ => {
                return Err(BuildError::Validation(format!(
                    "line {line}: unexpected model token `{token}`; expected `PROVIDER <backend>` or `--flag=value`"
                )));
            }
        }
    }

    Ok(Some(ModelReference {
        id,
        provider,
        options,
    }))
}

pub(super) fn parse_env_var(args: &[Value]) -> Option<EnvVar> {
    let joined = join_scalars(args);
    let (name, value) = joined.split_once('=')?;
    Some(EnvVar {
        name: name.to_string(),
        value: value.to_string(),
    })
}

pub(super) fn parse_mount(args: &[Value]) -> Option<MountConfig> {
    let tokens = scalars(args);
    if tokens.len() < 2 {
        return None;
    }

    Some(MountConfig {
        kind: match tokens[0].as_str() {
            "SESSION" => MountKind::Session,
            "MEMORY" => MountKind::Memory,
            "ARTIFACTS" => MountKind::Artifacts,
            _ => return None,
        },
        driver: tokens[1].clone(),
        options: tokens[2..].to_vec(),
    })
}

pub(super) fn parse_tool(args: &[Value]) -> Result<Option<ToolConfig>, BuildError> {
    let tokens = scalars(args);
    let Some(first) = tokens.first() else {
        return Ok(None);
    };
    match first.as_str() {
        "LOCAL" => parse_local_tool(&tokens).map(|tool| tool.map(ToolConfig::Local)),
        "BUILTIN" => tokens
            .get(1)
            .map(|capability| {
                Ok(ToolConfig::Builtin(BuiltinToolConfig {
                    capability: capability.clone(),
                    approval: parse_tool_approval(&tokens)?,
                    risk: parse_tool_risk(&tokens)?,
                    description: parse_named_value(&tokens, "DESCRIPTION"),
                }))
            })
            .transpose(),
        "MCP" => tokens
            .get(1)
            .map(|server| {
                Ok(ToolConfig::Mcp(McpToolConfig {
                    server: server.clone(),
                    approval: parse_tool_approval(&tokens)?,
                    risk: parse_tool_risk(&tokens)?,
                    description: parse_named_value(&tokens, "DESCRIPTION"),
                }))
            })
            .transpose(),
        "A2A" => parse_a2a_tool(&tokens).map(|tool| tool.map(ToolConfig::A2a)),
        _ => Ok(None),
    }
}

pub(super) struct ParsedComponent {
    pub(super) packaged_path: String,
}

pub(super) fn parse_component(args: &[Value]) -> ParsedComponent {
    let tokens = scalars(args);
    let packaged_path = tokens.first().cloned().unwrap_or_default();
    ParsedComponent { packaged_path }
}

fn parse_local_tool(tokens: &[String]) -> Result<Option<LocalToolConfig>, BuildError> {
    let Some(packaged_path) = tokens.get(1).cloned() else {
        return Ok(None);
    };
    let alias = parse_named_value(tokens, "AS").unwrap_or_else(|| {
        Path::new(&packaged_path)
            .file_stem()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| packaged_path.clone())
    });
    let approval = parse_tool_approval(tokens)?;
    let risk = parse_tool_risk(tokens)?;
    let description = parse_named_value(tokens, "DESCRIPTION");
    let input_schema =
        parse_named_value(tokens, "SCHEMA").map(|packaged_path| ToolInputSchemaRef {
            packaged_path,
            sha256: String::new(),
        });
    let runner = parse_tool_runner(tokens, &packaged_path);

    Ok(Some(LocalToolConfig {
        alias,
        packaged_path,
        runner,
        approval,
        risk,
        description,
        input_schema,
        skill_source: None,
    }))
}

fn parse_bool_token(value: &str, line: usize, field: &str) -> Result<bool, BuildError> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(BuildError::Validation(format!(
            "line {line}: `{field}` must be one of `true` or `false`, got `{value}`"
        ))),
    }
}

fn parse_model_option_flag(token: &str, line: usize) -> Result<(String, String), BuildError> {
    let Some(flag) = token.strip_prefix("--") else {
        unreachable!("model option flags must start with `--`");
    };
    let Some((name, value)) = flag.split_once('=') else {
        return Err(BuildError::Validation(format!(
            "line {line}: model flag `{token}` must use `--name=value` syntax"
        )));
    };
    if name.is_empty() || value.is_empty() {
        return Err(BuildError::Validation(format!(
            "line {line}: model flag `{token}` must include both a non-empty name and value"
        )));
    }
    Ok((name.to_string(), value.to_string()))
}

fn validate_model_option(name: &str, value: &str, line: usize) -> Result<String, BuildError> {
    match name {
        "persist-thread" => {
            let canonical = parse_bool_token(value, line, "--persist-thread")?;
            Ok(if canonical { "true" } else { "false" }.to_string())
        }
        "reasoning-effort" => Ok(value.to_string()),
        _ => Err(BuildError::Validation(format!(
            "line {line}: unsupported model flag `--{name}`"
        ))),
    }
}

fn parse_a2a_tool(tokens: &[String]) -> Result<Option<A2aToolConfig>, BuildError> {
    let Some(alias) = tokens.get(1).cloned() else {
        return Ok(None);
    };
    let url = parse_named_value(tokens, "URL").ok_or_else(|| {
        BuildError::Validation(format!("TOOL A2A `{alias}` requires `URL <endpoint>`"))
    })?;

    let input_schema =
        parse_named_value(tokens, "SCHEMA").map(|packaged_path| ToolInputSchemaRef {
            packaged_path,
            sha256: String::new(),
        });

    let endpoint_mode = parse_a2a_endpoint_mode(tokens)?;
    let expected_agent_name = parse_named_value(tokens, "EXPECT_AGENT_NAME");
    let expected_card_sha256 = parse_a2a_card_sha256(tokens)?;
    if matches!(endpoint_mode, Some(A2aEndpointMode::Direct))
        && (expected_agent_name.is_some() || expected_card_sha256.is_some())
    {
        return Err(BuildError::Validation(format!(
            "TOOL A2A `{alias}` cannot use `DISCOVERY direct` with `EXPECT_AGENT_NAME` or `EXPECT_CARD_SHA256`"
        )));
    }

    Ok(Some(A2aToolConfig {
        alias,
        url,
        endpoint_mode,
        auth: parse_a2a_auth(tokens)?,
        expected_agent_name,
        expected_card_sha256,
        approval: parse_tool_approval(tokens)?,
        risk: parse_tool_risk(tokens)?,
        description: parse_named_value(tokens, "DESCRIPTION"),
        input_schema,
    }))
}

fn parse_a2a_card_sha256(tokens: &[String]) -> Result<Option<String>, BuildError> {
    let Some(value) = parse_named_value(tokens, "EXPECT_CARD_SHA256") else {
        return Ok(None);
    };
    let valid = value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit());
    if !valid {
        return Err(BuildError::Validation(
            "EXPECT_CARD_SHA256 must be a 64-character lowercase or uppercase hex SHA256 digest"
                .to_string(),
        ));
    }
    Ok(Some(value.to_ascii_lowercase()))
}

fn parse_a2a_endpoint_mode(tokens: &[String]) -> Result<Option<A2aEndpointMode>, BuildError> {
    let Some(value) = parse_named_value(tokens, "DISCOVERY") else {
        return Ok(None);
    };
    match value.to_ascii_lowercase().as_str() {
        "auto" => Ok(Some(A2aEndpointMode::Auto)),
        "card" => Ok(Some(A2aEndpointMode::Card)),
        "direct" => Ok(Some(A2aEndpointMode::Direct)),
        other => Err(BuildError::Validation(format!(
            "invalid A2A discovery mode `{other}`; expected one of auto, card, direct"
        ))),
    }
}

fn parse_a2a_auth(tokens: &[String]) -> Result<Option<A2aAuthConfig>, BuildError> {
    let Some(auth_index) = tokens.iter().position(|token| token == "AUTH") else {
        return Ok(None);
    };
    let Some(scheme) = tokens.get(auth_index + 1) else {
        return Err(BuildError::Validation(
            "TOOL A2A `AUTH` requires `bearer <secret_name>`, `header <header_name> <secret_name>`, or `basic <username_secret_name> <password_secret_name>`".to_string(),
        ));
    };
    let auth = match scheme.to_ascii_lowercase().as_str() {
        "bearer" => {
            let Some(secret_name) = tokens.get(auth_index + 2) else {
                return Err(BuildError::Validation(
                    "TOOL A2A `AUTH bearer` requires `<secret_name>`".to_string(),
                ));
            };
            A2aAuthConfig::Bearer {
                secret_name: secret_name.clone(),
            }
        }
        "header" => {
            let Some(header_name) = tokens.get(auth_index + 2) else {
                return Err(BuildError::Validation(
                    "TOOL A2A `AUTH header` requires `<header_name> <secret_name>`".to_string(),
                ));
            };
            let Some(secret_name) = tokens.get(auth_index + 3) else {
                return Err(BuildError::Validation(
                    "TOOL A2A `AUTH header` requires `<header_name> <secret_name>`".to_string(),
                ));
            };
            validate_http_header_name(header_name)?;
            A2aAuthConfig::Header {
                header_name: header_name.clone(),
                secret_name: secret_name.clone(),
            }
        }
        "basic" => {
            let Some(username_secret_name) = tokens.get(auth_index + 2) else {
                return Err(BuildError::Validation(
                    "TOOL A2A `AUTH basic` requires `<username_secret_name> <password_secret_name>`".to_string(),
                ));
            };
            let Some(password_secret_name) = tokens.get(auth_index + 3) else {
                return Err(BuildError::Validation(
                    "TOOL A2A `AUTH basic` requires `<username_secret_name> <password_secret_name>`".to_string(),
                ));
            };
            A2aAuthConfig::Basic {
                username_secret_name: username_secret_name.clone(),
                password_secret_name: password_secret_name.clone(),
            }
        }
        other => {
            return Err(BuildError::Validation(format!(
                "invalid A2A auth scheme `{other}`; expected `bearer`, `header`, or `basic`"
            )));
        }
    };
    Ok(Some(auth))
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

fn validate_http_header_name(header_name: &str) -> Result<(), BuildError> {
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

fn parse_tool_runner(tokens: &[String], packaged_path: &str) -> CommandSpec {
    if let Some(using_index) = tokens.iter().position(|token| token == "USING") {
        let mut args = Vec::new();
        let mut index = using_index + 1;
        let command = tokens
            .get(index)
            .cloned()
            .unwrap_or_else(|| infer_runner(packaged_path).command);
        index += 1;

        while let Some(token) = tokens.get(index) {
            if token == "AS"
                || token == "APPROVAL"
                || token == "RISK"
                || token == "DESCRIPTION"
                || token == "SCHEMA"
            {
                break;
            }
            args.push(token.clone());
            index += 1;
        }

        return CommandSpec { command, args };
    }

    infer_runner(packaged_path)
}

fn parse_tool_approval(tokens: &[String]) -> Result<Option<ToolApprovalPolicy>, BuildError> {
    let Some(value) = parse_named_value(tokens, "APPROVAL") else {
        return Ok(None);
    };
    match value.to_ascii_lowercase().as_str() {
        "never" => Ok(Some(ToolApprovalPolicy::Never)),
        "always" => Ok(Some(ToolApprovalPolicy::Always)),
        "confirm" | "required" => Ok(Some(ToolApprovalPolicy::Confirm)),
        "audit" => Ok(Some(ToolApprovalPolicy::Audit)),
        other => Err(BuildError::Validation(format!(
            "invalid tool approval policy `{other}`; expected one of never, always, confirm, audit"
        ))),
    }
}

fn parse_tool_risk(tokens: &[String]) -> Result<Option<ToolRiskLevel>, BuildError> {
    let Some(value) = parse_named_value(tokens, "RISK") else {
        return Ok(None);
    };
    match value.to_ascii_lowercase().as_str() {
        "low" => Ok(Some(ToolRiskLevel::Low)),
        "medium" => Ok(Some(ToolRiskLevel::Medium)),
        "high" => Ok(Some(ToolRiskLevel::High)),
        other => Err(BuildError::Validation(format!(
            "invalid tool risk level `{other}`; expected one of low, medium, high"
        ))),
    }
}

pub(super) fn infer_runner(packaged_path: &str) -> CommandSpec {
    let extension = Path::new(packaged_path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();

    let command = match extension {
        "py" => "python3",
        "js" => "node",
        "ts" => "tsx",
        "sh" => "sh",
        _ => packaged_path,
    };

    CommandSpec {
        command: command.to_string(),
        args: Vec::new(),
    }
}

pub(super) fn parse_limit(args: &[Value]) -> Result<Option<LimitSpec>, BuildError> {
    let tokens = scalars(args);
    if tokens.len() < 2 {
        return Ok(None);
    }
    let scope = tokens[0].clone();
    if !matches!(
        scope.as_str(),
        "ITERATIONS" | "TOOL_CALLS" | "TOOL_OUTPUT" | "CONTEXT_TOKENS" | "TOOL_ROUNDS"
    ) {
        return Err(BuildError::Validation(format!(
            "invalid limit scope `{scope}`; expected one of ITERATIONS, TOOL_CALLS, TOOL_OUTPUT, CONTEXT_TOKENS, TOOL_ROUNDS"
        )));
    }
    Ok(Some(LimitSpec {
        scope,
        value: tokens[1].clone(),
        qualifiers: tokens[2..].to_vec(),
    }))
}

pub(super) fn parse_compaction(args: &[Value]) -> Option<CompactionConfig> {
    let tokens = scalars(args);
    let interval = tokens.first()?.clone();
    let overlap = tokens
        .windows(2)
        .find(|window| window[0] == "OVERLAP")
        .and_then(|window| window[1].parse::<u32>().ok());
    Some(CompactionConfig { interval, overlap })
}

pub(super) fn parse_timeout(args: &[Value]) -> Result<Option<TimeoutSpec>, BuildError> {
    let tokens = scalars(args);
    if tokens.len() < 2 {
        return Ok(None);
    }
    if !timeout_duration_is_valid(&tokens[1]) {
        return Err(BuildError::Validation(format!(
            "invalid timeout duration `{}`; expected a positive integer ending in ms, s, m, or h",
            tokens[1]
        )));
    }
    Ok(Some(TimeoutSpec {
        scope: tokens[0].clone(),
        duration: tokens[1].clone(),
        qualifiers: tokens[2..].to_vec(),
    }))
}

fn timeout_duration_is_valid(raw: &str) -> bool {
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
        return false;
    };
    value.trim().parse::<u64>().is_ok()
}

pub(super) fn parse_network_rule(args: &[Value]) -> Option<NetworkRule> {
    let tokens = scalars(args);
    if tokens.len() < 2 {
        return None;
    }
    Some(NetworkRule {
        action: tokens[0].clone(),
        target: tokens[1].clone(),
        qualifiers: tokens[2..].to_vec(),
    })
}

pub(super) fn parse_label(args: &[Value]) -> Option<(String, String)> {
    let joined = join_scalars(args);
    if let Some((key, value)) = joined.split_once('=') {
        return Some((key.to_string(), strip_matching_quotes(value)));
    }

    let tokens = scalars(args);
    if tokens.len() >= 2 {
        return Some((
            tokens[0].clone(),
            strip_matching_quotes(&tokens[1..].join(" ")),
        ));
    }

    None
}

fn parse_named_value(tokens: &[String], marker: &str) -> Option<String> {
    tokens
        .windows(2)
        .find(|window| window[0] == marker)
        .map(|window| window[1].clone())
}

fn strip_matching_quotes(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

fn join_scalars(args: &[Value]) -> String {
    scalars(args).join(" ")
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
