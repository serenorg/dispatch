use crate::manifest::{A2aAuthScheme, A2aEndpointMode};
use crate::trust::{A2aTrustPolicy, A2aTrustRequirement};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use url::Url;

use super::{CourierError, LocalToolSpec, ToolRunResult, encode_hex};

static A2A_REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

fn a2a_request_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = A2A_REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("dispatch-a2a-{now}-{sequence}")
}

fn normalize_a2a_rpc_endpoint(endpoint_url: &str) -> String {
    let trimmed = endpoint_url.trim_end_matches('/');
    if let Ok(parsed) = ureq::http::Uri::try_from(endpoint_url) {
        let path = parsed.path().trim_end_matches('/');
        if path.is_empty() || path == "/" {
            return format!("{trimmed}/a2a");
        }
    }
    trimmed.to_string()
}

fn normalize_a2a_discovery_base(endpoint_url: &str) -> String {
    endpoint_url
        .trim_end_matches('/')
        .strip_suffix("/a2a")
        .unwrap_or_else(|| endpoint_url.trim_end_matches('/'))
        .to_string()
}

fn format_a2a_error_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed)
        && let Some(message) = value
            .pointer("/error/message")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                value
                    .pointer("/message")
                    .and_then(serde_json::Value::as_str)
            })
    {
        return message.to_string();
    }
    trimmed.to_string()
}

fn read_a2a_json_response(
    mut response: ureq::http::Response<ureq::Body>,
    tool: &str,
) -> Result<serde_json::Value, CourierError> {
    let status = response.status();
    if !status.is_success() {
        let body = response.body_mut().read_to_string().unwrap_or_default();
        let detail = format_a2a_error_body(&body);
        let message = if detail.is_empty() {
            format!("HTTP {}", status.as_u16())
        } else {
            format!("HTTP {}: {detail}", status.as_u16())
        };
        return Err(CourierError::A2aToolRequest {
            tool: tool.to_string(),
            message,
        });
    }
    response
        .body_mut()
        .read_json()
        .map_err(|error| CourierError::A2aToolRequest {
            tool: tool.to_string(),
            message: format!("failed to parse A2A response: {error}"),
        })
}

fn send_a2a_json_rpc_with_env<F>(
    tool: &LocalToolSpec,
    endpoint: &str,
    payload: serde_json::Value,
    timeout: Option<Duration>,
    timeout_label: Option<&str>,
    env_lookup: &mut F,
) -> Result<serde_json::Value, CourierError>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut request = ureq::post(endpoint)
        .config()
        .http_status_as_error(false)
        .timeout_global(timeout)
        .build()
        .header("content-type", "application/json");
    request = apply_a2a_auth_headers(tool, request, env_lookup)?;
    let response = request.send_json(payload).map_err(|error| {
        if error.to_string().to_ascii_lowercase().contains("timeout") {
            CourierError::ToolTimedOut {
                tool: tool.alias.clone(),
                timeout: timeout_label.unwrap_or("TOOL").to_string(),
            }
        } else {
            CourierError::A2aToolRequest {
                tool: tool.alias.clone(),
                message: error.to_string(),
            }
        }
    })?;
    read_a2a_json_response(response, &tool.alias)
}

fn resolve_a2a_rpc_endpoint_with_env<F>(
    tool: &LocalToolSpec,
    mut env_lookup: F,
    timeout: Option<Duration>,
) -> Result<(String, Option<String>), CourierError>
where
    F: FnMut(&str) -> Option<String>,
{
    let url = tool
        .endpoint_url()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| CourierError::MissingA2aToolUrl {
            tool: tool.alias.clone(),
        })?;
    validate_a2a_url_allowed(tool, url, &mut env_lookup)?;
    if matches!(tool.endpoint_mode(), Some(A2aEndpointMode::Direct)) {
        return Ok((normalize_a2a_rpc_endpoint(url), None));
    }
    let discovery_url = format!(
        "{}/.well-known/agent.json",
        normalize_a2a_discovery_base(url)
    );
    let mut request = ureq::get(&discovery_url)
        .config()
        .http_status_as_error(false)
        .timeout_global(timeout)
        .build();
    request = apply_a2a_auth_headers(tool, request, &mut env_lookup)?;
    let response = request
        .call()
        .map_err(|error| CourierError::A2aToolRequest {
            tool: tool.alias.clone(),
            message: error.to_string(),
        })?;
    let status = response.status();
    if status.is_success() {
        let mut response = response;
        let body =
            response
                .body_mut()
                .read_to_string()
                .map_err(|error| CourierError::A2aToolRequest {
                    tool: tool.alias.clone(),
                    message: format!("failed to read agent card: {error}"),
                })?;
        let actual_sha256 = encode_hex(Sha256::digest(body.as_bytes()));
        let card = serde_json::from_str::<serde_json::Value>(&body).map_err(|error| {
            CourierError::A2aToolRequest {
                tool: tool.alias.clone(),
                message: format!("failed to parse agent card: {error}"),
            }
        })?;
        let endpoint = card
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| normalize_a2a_rpc_endpoint(url));
        let trust_requirement = resolve_a2a_trust_requirement(tool, &endpoint, &mut env_lookup)?;
        if let Some(expected_sha256) = tool.expected_card_sha256().or_else(|| {
            trust_requirement
                .as_ref()
                .and_then(|requirement| requirement.expected_card_sha256.as_deref())
        }) && expected_sha256 != actual_sha256
        {
            return Err(CourierError::A2aToolRequest {
                tool: tool.alias.clone(),
                message: format!(
                    "agent card digest mismatch: expected `{expected_sha256}`, got `{actual_sha256}`"
                ),
            });
        }
        if !a2a_urls_share_origin(url, &endpoint) {
            return Err(CourierError::A2aToolRequest {
                tool: tool.alias.clone(),
                message: format!(
                    "discovered agent card URL must stay on the declared origin: declared `{url}`, discovered `{endpoint}`"
                ),
            });
        }
        validate_a2a_url_allowed(tool, &endpoint, &mut env_lookup)?;
        let agent_name = card
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string);
        if let Some(expected) = tool.expected_agent_name().or_else(|| {
            trust_requirement
                .as_ref()
                .and_then(|requirement| requirement.expected_agent_name.as_deref())
        }) {
            match agent_name.as_deref() {
                Some(actual) if expected == actual => {}
                Some(actual) => {
                    return Err(CourierError::A2aToolRequest {
                        tool: tool.alias.clone(),
                        message: format!(
                            "agent card name mismatch: expected `{expected}`, got `{actual}`"
                        ),
                    });
                }
                None => {
                    return Err(CourierError::A2aToolRequest {
                        tool: tool.alias.clone(),
                        message: format!(
                            "agent card did not include `name`, but `{expected}` was required"
                        ),
                    });
                }
            }
        }
        return Ok((endpoint, agent_name));
    }
    if matches!(tool.endpoint_mode(), Some(A2aEndpointMode::Card)) {
        return Err(CourierError::A2aToolRequest {
            tool: tool.alias.clone(),
            message: "agent card discovery failed for required `DISCOVERY card` mode".to_string(),
        });
    }
    if let Some(requirement) =
        resolve_a2a_trust_requirement(tool, &normalize_a2a_rpc_endpoint(url), &mut env_lookup)?
        && (requirement.expected_agent_name.is_some() || requirement.expected_card_sha256.is_some())
    {
        return Err(CourierError::A2aToolRequest {
            tool: tool.alias.clone(),
            message:
                "A2A trust policy requires discovered agent-card identity, but card discovery did not succeed"
                    .to_string(),
        });
    }
    Ok((normalize_a2a_rpc_endpoint(url), None))
}

fn apply_a2a_auth_headers<State, F>(
    tool: &LocalToolSpec,
    mut request: ureq::RequestBuilder<State>,
    env_lookup: &mut F,
) -> Result<ureq::RequestBuilder<State>, CourierError>
where
    F: FnMut(&str) -> Option<String>,
{
    let (Some(secret_name), Some(auth_scheme)) = (tool.auth_secret_name(), tool.auth_scheme())
    else {
        return Ok(request);
    };
    let secret_value = env_lookup(secret_name).ok_or_else(|| CourierError::A2aToolRequest {
        tool: tool.alias.clone(),
        message: format!("configured A2A auth secret `{secret_name}` is not available"),
    })?;
    request = match auth_scheme {
        A2aAuthScheme::Bearer => request.header("authorization", &format!("Bearer {secret_value}")),
        A2aAuthScheme::Header => {
            let header_name =
                tool.auth_header_name()
                    .ok_or_else(|| CourierError::A2aToolRequest {
                        tool: tool.alias.clone(),
                        message: "configured A2A header auth is missing a header name".to_string(),
                    })?;
            request.header(header_name, &secret_value)
        }
    };
    Ok(request)
}

fn normalize_a2a_allowlist_entry(entry: &str) -> Option<String> {
    let trimmed = entry.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains("://") {
        let parsed = Url::parse(trimmed).ok()?;
        return a2a_origin(&parsed);
    }
    Some(trimmed.to_ascii_lowercase())
}

pub(crate) fn a2a_origin(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    let scheme = url.scheme();
    let default_port = match scheme {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    };
    let port = url.port_or_known_default();
    match (port, default_port) {
        (Some(port), Some(default)) if port != default => Some(format!("{scheme}://{host}:{port}")),
        (Some(_), _) | (None, _) => Some(format!("{scheme}://{host}")),
    }
}

fn is_loopback_host(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(addr)) => addr.is_loopback(),
        Some(url::Host::Ipv6(addr)) => addr.is_loopback(),
        None => false,
    }
}

fn validate_a2a_url_security(tool: &LocalToolSpec, url: &Url) -> Result<(), CourierError> {
    match url.scheme() {
        "https" => {}
        "http" if is_loopback_host(url) => {}
        "http" => {
            return Err(CourierError::A2aToolRequest {
                tool: tool.alias.clone(),
                message: format!(
                    "A2A URL `{}` must use https unless it targets a loopback host",
                    url
                ),
            });
        }
        scheme => {
            return Err(CourierError::A2aToolRequest {
                tool: tool.alias.clone(),
                message: format!("A2A URL must use http or https, received `{scheme}`"),
            });
        }
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(CourierError::A2aToolRequest {
            tool: tool.alias.clone(),
            message: format!("A2A URL `{}` must not embed credentials", url),
        });
    }
    Ok(())
}

fn is_a2a_url_allowed(url: &Url, allowlist: &[String]) -> bool {
    if allowlist.is_empty() {
        return true;
    }
    let host = url
        .host_str()
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();
    let origin = a2a_origin(url);
    allowlist.iter().any(|entry| {
        if entry.contains("://") {
            origin.as_ref().is_some_and(|candidate| candidate == entry)
        } else {
            host == *entry
        }
    })
}

fn a2a_urls_share_origin(left: &str, right: &str) -> bool {
    let Ok(left) = Url::parse(left) else {
        return false;
    };
    let Ok(right) = Url::parse(right) else {
        return false;
    };
    a2a_origin(&left) == a2a_origin(&right)
}

fn validate_a2a_url_allowed<F>(
    tool: &LocalToolSpec,
    url: &str,
    env_lookup: &mut F,
) -> Result<(), CourierError>
where
    F: FnMut(&str) -> Option<String>,
{
    let parsed = Url::parse(url).map_err(|error| CourierError::A2aToolRequest {
        tool: tool.alias.clone(),
        message: format!("invalid A2A URL `{url}`: {error}"),
    })?;
    validate_a2a_url_security(tool, &parsed)?;
    let Some(raw_allowlist) = env_lookup("DISPATCH_A2A_ALLOWED_ORIGINS") else {
        return validate_a2a_url_trust_policy(tool, &parsed, env_lookup);
    };
    let allowlist = raw_allowlist
        .split(',')
        .filter_map(normalize_a2a_allowlist_entry)
        .collect::<Vec<_>>();
    if !is_a2a_url_allowed(&parsed, &allowlist) {
        return Err(CourierError::A2aToolRequest {
            tool: tool.alias.clone(),
            message: format!("A2A URL `{url}` is not allowed by DISPATCH_A2A_ALLOWED_ORIGINS"),
        });
    }
    validate_a2a_url_trust_policy(tool, &parsed, env_lookup)
}

fn validate_a2a_url_trust_policy<F>(
    tool: &LocalToolSpec,
    parsed: &Url,
    env_lookup: &mut F,
) -> Result<(), CourierError>
where
    F: FnMut(&str) -> Option<String>,
{
    let Some(requirement) = resolve_a2a_trust_requirement_for_url(tool, parsed, env_lookup)? else {
        return Ok(());
    };
    if requirement.matched {
        Ok(())
    } else {
        Err(CourierError::A2aToolRequest {
            tool: tool.alias.clone(),
            message: format!(
                "A2A URL `{}` is not allowed by DISPATCH_A2A_TRUST_POLICY",
                parsed
            ),
        })
    }
}

fn resolve_a2a_trust_requirement_for_url<F>(
    tool: &LocalToolSpec,
    parsed: &Url,
    env_lookup: &mut F,
) -> Result<Option<A2aTrustRequirement>, CourierError>
where
    F: FnMut(&str) -> Option<String>,
{
    let Some(policy_path) = env_lookup("DISPATCH_A2A_TRUST_POLICY") else {
        return Ok(None);
    };
    let policy = A2aTrustPolicy::from_path(Path::new(&policy_path)).map_err(|error| {
        CourierError::A2aToolRequest {
            tool: tool.alias.clone(),
            message: format!("failed to load A2A trust policy `{policy_path}`: {error}"),
        }
    })?;
    let requirement =
        policy
            .resolve_for_url(parsed)
            .map_err(|error| CourierError::A2aToolRequest {
                tool: tool.alias.clone(),
                message: format!("A2A trust policy conflict for `{}`: {error}", parsed),
            })?;
    Ok(Some(requirement))
}

fn resolve_a2a_trust_requirement<F>(
    tool: &LocalToolSpec,
    endpoint: &str,
    env_lookup: &mut F,
) -> Result<Option<A2aTrustRequirement>, CourierError>
where
    F: FnMut(&str) -> Option<String>,
{
    let parsed = Url::parse(endpoint).map_err(|error| CourierError::A2aToolRequest {
        tool: tool.alias.clone(),
        message: format!("invalid A2A URL `{endpoint}`: {error}"),
    })?;
    resolve_a2a_trust_requirement_for_url(tool, &parsed, env_lookup)
}

fn collect_a2a_artifact_parts(
    artifact: &serde_json::Value,
    texts: &mut Vec<String>,
    data_parts: &mut Vec<serde_json::Value>,
) {
    let Some(parts) = artifact.get("parts").and_then(serde_json::Value::as_array) else {
        return;
    };
    for part in parts {
        match part.get("kind").and_then(serde_json::Value::as_str) {
            Some("text") => {
                if let Some(text) = part.get("text").and_then(serde_json::Value::as_str) {
                    texts.push(text.to_string());
                }
            }
            Some("data") => {
                if let Some(data) = part.get("data") {
                    data_parts.push(data.clone());
                }
            }
            Some("file") => {
                if let Some(file) = part.get("file") {
                    data_parts.push(serde_json::json!({ "file": file }));
                }
            }
            _ => {}
        }
    }
}

fn extract_a2a_task_output(value: &serde_json::Value) -> serde_json::Value {
    let task = if value.get("id").is_some() {
        value
    } else {
        value.get("task").unwrap_or(value)
    };
    let mut texts = Vec::new();
    let mut data_parts = Vec::new();
    if let Some(artifacts) = task.get("artifacts").and_then(serde_json::Value::as_array) {
        for artifact in artifacts {
            collect_a2a_artifact_parts(artifact, &mut texts, &mut data_parts);
        }
    }
    if data_parts.len() == 1 {
        return data_parts.into_iter().next().unwrap();
    }
    if !data_parts.is_empty() {
        return serde_json::Value::Array(data_parts);
    }
    if texts.len() == 1 {
        return serde_json::Value::String(texts.into_iter().next().unwrap());
    }
    if !texts.is_empty() {
        return serde_json::json!({ "text": texts.join("\n") });
    }
    if let Some(status_message) = task
        .pointer("/status/message")
        .and_then(serde_json::Value::as_str)
    {
        return serde_json::Value::String(status_message.to_string());
    }
    serde_json::Value::Null
}

fn validate_a2a_task_state(
    tool: &LocalToolSpec,
    value: &serde_json::Value,
) -> Result<(), CourierError> {
    let task = if value.get("id").is_some() {
        value
    } else {
        value.get("task").unwrap_or(value)
    };
    let Some(state) = task
        .pointer("/status/state")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(());
    };
    if state == "completed" {
        return Ok(());
    }
    let status_message = task
        .pointer("/status/message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("no status message");
    Err(CourierError::A2aToolRequest {
        tool: tool.alias.clone(),
        message: format!(
            "remote A2A task did not complete synchronously: state=`{state}` message=`{status_message}`"
        ),
    })
}

fn a2a_task_id(value: &serde_json::Value) -> Option<&str> {
    if value.get("id").is_some() {
        value.get("id").and_then(serde_json::Value::as_str)
    } else {
        value
            .pointer("/task/id")
            .and_then(serde_json::Value::as_str)
    }
}

fn a2a_task_state(value: &serde_json::Value) -> Option<&str> {
    if value.get("id").is_some() {
        value
            .pointer("/status/state")
            .and_then(serde_json::Value::as_str)
    } else {
        value
            .pointer("/task/status/state")
            .and_then(serde_json::Value::as_str)
    }
}

fn poll_a2a_task_until_complete<F>(
    tool: &LocalToolSpec,
    endpoint: &str,
    initial_result: serde_json::Value,
    timeout_spec: Option<(&str, Duration)>,
    env_lookup: &mut F,
) -> Result<serde_json::Value, CourierError>
where
    F: FnMut(&str) -> Option<String>,
{
    if matches!(a2a_task_state(&initial_result), Some("completed")) {
        return Ok(initial_result);
    }
    let Some(task_id) = a2a_task_id(&initial_result).map(ToString::to_string) else {
        validate_a2a_task_state(tool, &initial_result)?;
        return Ok(initial_result);
    };
    let started_at = Instant::now();
    let deadline = timeout_spec.map(|(_, duration)| started_at + duration);
    loop {
        if let Some(deadline) = deadline
            && Instant::now() >= deadline
        {
            let timeout_label = timeout_spec.map(|(label, _)| label).unwrap_or("TOOL");
            best_effort_cancel_a2a_task(tool, endpoint, &task_id, env_lookup);
            return Err(CourierError::ToolTimedOut {
                tool: tool.alias.clone(),
                timeout: timeout_label.to_string(),
            });
        }
        std::thread::sleep(Duration::from_millis(50));
        let remaining = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "tasks/get",
            "id": a2a_request_id(),
            "params": {
                "taskId": task_id,
                "historyLength": null
            }
        });
        let body = match send_a2a_json_rpc_with_env(
            tool,
            endpoint,
            payload,
            remaining,
            timeout_spec.map(|(label, _)| label),
            env_lookup,
        ) {
            Ok(body) => body,
            Err(timeout_error @ CourierError::ToolTimedOut { .. }) => {
                best_effort_cancel_a2a_task(tool, endpoint, &task_id, env_lookup);
                return Err(timeout_error);
            }
            Err(error) => return Err(error),
        };
        if let Some(error) = body.get("error") {
            let code = error
                .get("code")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or_default();
            let message = error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown A2A JSON-RPC error");
            let data_suffix = error
                .get("data")
                .map(|value| format!(" data={}", value))
                .unwrap_or_default();
            return Err(CourierError::A2aToolRequest {
                tool: tool.alias.clone(),
                message: format!("JSON-RPC error {code}: {message}{data_suffix}"),
            });
        }
        let current = body.get("result").cloned().unwrap_or(body);
        if matches!(a2a_task_state(&current), Some("completed")) {
            return Ok(current);
        }
    }
}

fn best_effort_cancel_a2a_task<F>(
    tool: &LocalToolSpec,
    endpoint: &str,
    task_id: &str,
    env_lookup: &mut F,
) where
    F: FnMut(&str) -> Option<String>,
{
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "tasks/cancel",
        "id": a2a_request_id(),
        "params": {
            "taskId": task_id
        }
    });
    let _ = send_a2a_json_rpc_with_env(
        tool,
        endpoint,
        payload,
        Some(Duration::from_millis(500)),
        None,
        env_lookup,
    );
}

pub(super) fn execute_a2a_tool_with_env<F>(
    tool: &LocalToolSpec,
    input: Option<&str>,
    env_lookup: F,
    timeout_spec: Option<(&str, Duration)>,
) -> Result<ToolRunResult, CourierError>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut env_lookup = env_lookup;
    let timeout = timeout_spec.map(|(_, duration)| duration);
    let (endpoint, agent_name) = resolve_a2a_rpc_endpoint_with_env(tool, &mut env_lookup, timeout)?;
    let request_id = a2a_request_id();
    let input_value = input.unwrap_or_default();
    let part = if tool.input_schema_packaged_path.is_some() {
        let data = serde_json::from_str::<serde_json::Value>(input_value).map_err(|error| {
            CourierError::A2aToolRequest {
                tool: tool.alias.clone(),
                message: format!("A2A tool expected JSON input: {error}"),
            }
        })?;
        serde_json::json!({ "kind": "data", "data": data })
    } else {
        serde_json::json!({ "kind": "text", "text": input_value })
    };
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "message/send",
        "id": request_id,
        "params": {
            "message": {
                "messageId": a2a_request_id(),
                "role": "user",
                "parts": [part]
            }
        }
    });
    let body = send_a2a_json_rpc_with_env(
        tool,
        &endpoint,
        payload,
        timeout,
        timeout_spec.map(|(label, _)| label),
        &mut env_lookup,
    )?;
    if let Some(error) = body.get("error") {
        let code = error
            .get("code")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or_default();
        let message = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown A2A JSON-RPC error");
        let data_suffix = error
            .get("data")
            .map(|value| format!(" data={}", value))
            .unwrap_or_default();
        return Err(CourierError::A2aToolRequest {
            tool: tool.alias.clone(),
            message: format!("JSON-RPC error {code}: {message}{data_suffix}"),
        });
    }
    let result = body.get("result").cloned().unwrap_or(body);
    let result =
        poll_a2a_task_until_complete(tool, &endpoint, result, timeout_spec, &mut env_lookup)?;
    validate_a2a_task_state(tool, &result)?;
    let output = extract_a2a_task_output(&result);
    let stdout = match output {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(text) => text,
        other => serde_json::to_string_pretty(&other).unwrap_or_else(|_| other.to_string()),
    };
    let mut args = vec![endpoint.clone()];
    if let Some(name) = agent_name {
        args.push(name);
    }
    Ok(ToolRunResult {
        tool: tool.alias.clone(),
        command: tool.command().to_string(),
        args,
        exit_code: 0,
        stdout,
        stderr: String::new(),
    })
}
