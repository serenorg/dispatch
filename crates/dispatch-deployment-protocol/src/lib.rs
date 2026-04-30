//! Shared wire protocol for Dispatch `deployment` plugins.
//!
//! Deployment plugins are control-plane-only: they own deployment lifecycle
//! operations (deploy, update, rollback, list, get, revisions, test-run) but
//! do NOT own runtime conversations. Runtime turns continue to flow through
//! the `courier` category, addressed by the `deployment_id` a deployment
//! plugin produces from `Deploy`.
//!
//! Spec payloads (`spec`, `patch`, etc.) are intentionally `serde_json::Value`
//! so the protocol stays backend-agnostic. The shape and semantics of those
//! payloads are between the plugin and its callers; Dispatch only carries
//! the envelope.
use dispatch_plugin_rpc::{
    JSONRPC_APPLICATION_ERROR, JSONRPC_INTERNAL_ERROR, JSONRPC_INVALID_PARAMS,
    JSONRPC_INVALID_REQUEST, JSONRPC_METHOD_NOT_FOUND, JSONRPC_PARSE_ERROR, JsonRpcErrorResponse,
    JsonRpcMessage, JsonRpcRequest, JsonRpcSuccessResponse, RequestId, ensure_jsonrpc_version,
    standard_error_code_name,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use dispatch_plugin_rpc::{
    JsonRpcErrorObject, JsonRpcMessageError, RequestId as PluginRequestId,
};

pub const DEPLOYMENT_PLUGIN_PROTOCOL_VERSION: u32 = 1;

pub const METHOD_CAPABILITIES: &str = "deployment.capabilities";
pub const METHOD_CONFIGURE: &str = "deployment.configure";
pub const METHOD_HEALTH: &str = "deployment.health";
pub const METHOD_VALIDATE: &str = "deployment.validate";
pub const METHOD_TEST_RUN: &str = "deployment.test_run";
pub const METHOD_DEPLOY: &str = "deployment.deploy";
pub const METHOD_PREVIEW_UPDATE: &str = "deployment.preview_update";
pub const METHOD_UPDATE: &str = "deployment.update";
pub const METHOD_GET: &str = "deployment.get";
pub const METHOD_LIST: &str = "deployment.list";
pub const METHOD_LIST_REVISIONS: &str = "deployment.list_revisions";
pub const METHOD_PREVIEW_ROLLBACK: &str = "deployment.preview_rollback";
pub const METHOD_ROLLBACK: &str = "deployment.rollback";
pub const METHOD_START: &str = "deployment.start";
pub const METHOD_STOP: &str = "deployment.stop";
pub const METHOD_DELETE: &str = "deployment.delete";
pub const METHOD_SHUTDOWN: &str = "deployment.shutdown";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginRequestEnvelope {
    pub protocol_version: u32,
    pub request: PluginRequest,
}

/// Control-plane requests a deployment plugin understands.
///
/// Notably absent: `Run`. Runtime turns belong on the courier category.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginRequest {
    Capabilities,
    Configure {
        config: Value,
    },
    Health {
        config: Value,
    },
    /// Schema + policy check on a candidate spec. No side effects.
    Validate {
        spec: Value,
    },
    /// Preflight a draft spec against the backend (e.g. one-shot test run
    /// against the model). Side-effects allowed but should not persist a
    /// long-lived deployment.
    TestRun {
        spec: Value,
        sample_input: Option<String>,
    },
    /// Create a managed deployment. Returns a `deployment_id` plus initial state.
    Deploy {
        spec: Value,
    },
    /// Diff a candidate update without applying it.
    PreviewUpdate {
        deployment_id: String,
        patch: Value,
    },
    /// Apply a partial update. Returns the new revision.
    Update {
        deployment_id: String,
        patch: Value,
    },
    /// Read the current managed deployment state.
    Get {
        deployment_id: String,
    },
    /// Enumerate managed deployments. Filters are backend-defined.
    List {
        filters: Option<Value>,
    },
    /// Read the revision history for a deployment.
    ListRevisions {
        deployment_id: String,
    },
    /// Diff a rollback target without applying it.
    PreviewRollback {
        deployment_id: String,
        revision_id: String,
    },
    /// Activate a prior revision. Produces a new revision.
    Rollback {
        deployment_id: String,
        revision_id: String,
    },
    /// Start a halted managed deployment.
    Start {
        deployment_id: String,
    },
    /// Halt a managed deployment without deletion.
    Stop {
        deployment_id: String,
    },
    /// Tear down a managed deployment.
    Delete {
        deployment_id: String,
    },
    /// Plugin lifecycle exit.
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginResponse {
    Capabilities {
        capabilities: DeploymentCapabilities,
    },
    Configured {
        configuration: DeploymentConfiguration,
    },
    Health {
        health: DeploymentHealth,
    },
    /// Validation result. `ok = false` MUST come with at least one issue.
    Validation {
        result: ValidationResult,
    },
    /// Result of a `TestRun`. Backend-defined `output` payload.
    TestRunResult {
        result: TestRunResult,
    },
    /// Result of `Deploy`, `Update`, `Rollback`. Returns the live state.
    Deployment {
        deployment: Deployment,
    },
    /// Result of `PreviewUpdate` / `PreviewRollback`. Diff payload is
    /// backend-defined.
    Preview {
        preview: DeploymentPreview,
    },
    /// Result of `Get`.
    DeploymentDetail {
        deployment: Deployment,
    },
    /// Result of `List`.
    DeploymentList {
        deployments: Vec<Deployment>,
    },
    /// Result of `ListRevisions`.
    Revisions {
        revisions: Vec<DeploymentRevision>,
    },
    /// Used for `Start`, `Stop`, `Delete`, `Shutdown`.
    Ok,
    Error {
        error: PluginErrorPayload,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginErrorPayload {
    pub code: String,
    pub message: String,
    /// Backend-defined structured failure detail. Carries shape that does
    /// not fit `code` + `message` (e.g. upstream HTTP status, retry-after,
    /// validation issue list, deployment id under conflict).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeploymentCapabilities {
    pub deployment_plugin_id: String,
    pub protocol_version: u32,
    /// Names of supported templates / presets / model policies the backend
    /// advertises. Backends without templates leave these empty.
    #[serde(default)]
    pub supported_templates: Vec<String>,
    #[serde(default)]
    pub supported_tool_presets: Vec<String>,
    #[serde(default)]
    pub supported_model_policies: Vec<String>,
    pub supports_test_run: bool,
    pub supports_revisions: bool,
    pub supports_rollback: bool,
    pub supports_scheduled: bool,
    /// Free-form extension blob for backend-specific capabilities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeploymentConfiguration {
    pub deployment_plugin_id: String,
    /// Backend-defined configuration echo (e.g. resolved base URL, account id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeploymentHealth {
    pub reachable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidationResult {
    pub ok: bool,
    #[serde(default)]
    pub issues: Vec<ValidationIssue>,
    /// Backend-defined normalized spec, if it produced one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized_spec: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidationIssue {
    pub field: Option<String>,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TestRunResult {
    /// Status reported by the backend (`completed`, `failed`, `awaiting_approval`, ...).
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Backend-defined trace / metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Deployment {
    pub deployment_id: String,
    pub status: String,
    /// Currently-active revision id, if the backend supports revisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_id: Option<String>,
    /// Backend-defined detailed view (resolved spec, schedule, alerts, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeploymentPreview {
    pub deployment_id: String,
    /// Backend-defined diff payload describing what would change.
    pub diff: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeploymentRevision {
    pub revision_id: String,
    pub created_at: Option<String>,
    pub created_by: Option<String>,
    /// `create`, `update`, `rollback`.
    pub change_kind: String,
    /// Backend-defined snapshot summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Value>,
}

pub fn plugin_error(code: &str, message: impl Into<String>) -> PluginResponse {
    plugin_error_with_details(code, message, None)
}

pub fn plugin_error_with_details(
    code: &str,
    message: impl Into<String>,
    details: Option<Value>,
) -> PluginResponse {
    PluginResponse::Error {
        error: PluginErrorPayload {
            code: code.to_string(),
            message: message.into(),
            details,
        },
    }
}

pub fn request_method(request: &PluginRequest) -> &'static str {
    match request {
        PluginRequest::Capabilities => METHOD_CAPABILITIES,
        PluginRequest::Configure { .. } => METHOD_CONFIGURE,
        PluginRequest::Health { .. } => METHOD_HEALTH,
        PluginRequest::Validate { .. } => METHOD_VALIDATE,
        PluginRequest::TestRun { .. } => METHOD_TEST_RUN,
        PluginRequest::Deploy { .. } => METHOD_DEPLOY,
        PluginRequest::PreviewUpdate { .. } => METHOD_PREVIEW_UPDATE,
        PluginRequest::Update { .. } => METHOD_UPDATE,
        PluginRequest::Get { .. } => METHOD_GET,
        PluginRequest::List { .. } => METHOD_LIST,
        PluginRequest::ListRevisions { .. } => METHOD_LIST_REVISIONS,
        PluginRequest::PreviewRollback { .. } => METHOD_PREVIEW_ROLLBACK,
        PluginRequest::Rollback { .. } => METHOD_ROLLBACK,
        PluginRequest::Start { .. } => METHOD_START,
        PluginRequest::Stop { .. } => METHOD_STOP,
        PluginRequest::Delete { .. } => METHOD_DELETE,
        PluginRequest::Shutdown => METHOD_SHUTDOWN,
    }
}

pub fn request_to_jsonrpc(
    id: RequestId,
    envelope: &PluginRequestEnvelope,
) -> Result<JsonRpcRequest, JsonRpcMessageError> {
    let mut params = serde_json::to_value(&envelope.request).map_err(|source| {
        JsonRpcMessageError::message(format!("failed to serialize request: {source}"))
    })?;
    let Value::Object(ref mut object) = params else {
        return Err(JsonRpcMessageError::message(
            "deployment request did not serialize to an object",
        ));
    };
    object.insert(
        "protocol_version".to_string(),
        Value::from(envelope.protocol_version),
    );
    Ok(JsonRpcRequest::new(
        id,
        request_method(&envelope.request),
        Some(params),
    ))
}

pub fn parse_jsonrpc_request(
    line: &str,
) -> Result<(RequestId, PluginRequestEnvelope), JsonRpcMessageError> {
    let message: JsonRpcMessage =
        serde_json::from_str(line).map_err(JsonRpcMessageError::invalid_json)?;
    let JsonRpcMessage::Request(request) = message else {
        return Err(JsonRpcMessageError::ExpectedRequest);
    };
    ensure_jsonrpc_version(&request.jsonrpc)?;
    let params = request.params.ok_or(JsonRpcMessageError::MissingParams)?;
    let envelope = decode_request_params(&request.method, params)?;
    Ok((request.id, envelope))
}

pub fn response_to_jsonrpc(
    id: &RequestId,
    response: &PluginResponse,
) -> Result<String, JsonRpcMessageError> {
    let message = match response {
        PluginResponse::Error { error } => JsonRpcMessage::Error(JsonRpcErrorResponse::new(
            Some(id.clone()),
            encode_dispatch_error_code(&error.code),
            error.message.clone(),
            Some(serde_json::json!({ "dispatch_error": error })),
        )),
        other => JsonRpcMessage::Response(JsonRpcSuccessResponse::new(
            id.clone(),
            serde_json::to_value(other).map_err(|source| {
                JsonRpcMessageError::message(format!(
                    "failed to serialize deployment response: {source}"
                ))
            })?,
        )),
    };
    serde_json::to_string(&message).map_err(|source| {
        JsonRpcMessageError::message(format!("failed to serialize JSON-RPC message: {source}"))
    })
}

pub fn parse_jsonrpc_message(
    line: &str,
) -> Result<(Option<RequestId>, PluginResponse), JsonRpcMessageError> {
    let message: JsonRpcMessage =
        serde_json::from_str(line).map_err(JsonRpcMessageError::invalid_json)?;
    match message {
        JsonRpcMessage::Response(response) => {
            ensure_jsonrpc_version(&response.jsonrpc)?;
            let id = response.id;
            let response = serde_json::from_value(response.result).map_err(|source| {
                JsonRpcMessageError::message(format!("invalid deployment result payload: {source}"))
            })?;
            Ok((Some(id), response))
        }
        JsonRpcMessage::Error(error) => {
            ensure_jsonrpc_version(&error.jsonrpc)?;
            let id = error.id.clone();
            Ok((
                id,
                PluginResponse::Error {
                    error: decode_dispatch_error(error),
                },
            ))
        }
        JsonRpcMessage::Notification(notification) => Err(
            JsonRpcMessageError::UnexpectedNotificationMethod(notification.method),
        ),
        JsonRpcMessage::Request(_) => Err(JsonRpcMessageError::message(
            "expected JSON-RPC response, got request",
        )),
    }
}

fn decode_request_params(
    method: &str,
    params: Value,
) -> Result<PluginRequestEnvelope, JsonRpcMessageError> {
    let Value::Object(mut object) = params else {
        return Err(JsonRpcMessageError::ParamsMustBeObject);
    };
    let protocol_version = object
        .remove("protocol_version")
        .ok_or(JsonRpcMessageError::MissingProtocolVersion)?
        .as_u64()
        .ok_or(JsonRpcMessageError::InvalidProtocolVersion)? as u32;
    let request: PluginRequest =
        serde_json::from_value(Value::Object(object)).map_err(|source| {
            JsonRpcMessageError::message(format!("invalid plugin request params: {source}"))
        })?;
    let expected_method = request_method(&request);
    if expected_method != method {
        return Err(JsonRpcMessageError::MethodMismatch {
            method: method.to_string(),
            expected: expected_method.to_string(),
        });
    }
    Ok(PluginRequestEnvelope {
        protocol_version,
        request,
    })
}

fn decode_dispatch_error(error: JsonRpcErrorResponse) -> PluginErrorPayload {
    let dispatch_error = error
        .error
        .data
        .as_ref()
        .and_then(|data| data.get("dispatch_error"))
        .and_then(|value| serde_json::from_value::<PluginErrorPayload>(value.clone()).ok());
    dispatch_error.unwrap_or_else(|| PluginErrorPayload {
        code: standard_error_code_name(error.error.code)
            .unwrap_or("jsonrpc_error")
            .to_string(),
        message: error.error.message,
        details: None,
    })
}

fn encode_dispatch_error_code(code: &str) -> i64 {
    match code {
        "parse_error" => JSONRPC_PARSE_ERROR,
        "invalid_request" => JSONRPC_INVALID_REQUEST,
        "method_not_found" | "unsupported_request" => JSONRPC_METHOD_NOT_FOUND,
        "invalid_params" | "bad_request" => JSONRPC_INVALID_PARAMS,
        "internal_error" => JSONRPC_INTERNAL_ERROR,
        _ => JSONRPC_APPLICATION_ERROR,
    }
}

/// Error codes the dispatch host knows about. Plugins may use these or
/// invent their own; the host treats unknown codes as application errors.
pub mod error_codes {
    pub const AUTHENTICATION_FAILED: &str = "authentication_failed";
    pub const NOT_FOUND: &str = "not_found";
    pub const INVALID_SPEC: &str = "invalid_spec";
    pub const UPSTREAM_ERROR: &str = "upstream_error";
    pub const UNIMPLEMENTED: &str = "unimplemented";
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deploy_request_round_trips_jsonrpc() {
        let request = PluginRequestEnvelope {
            protocol_version: DEPLOYMENT_PLUGIN_PROTOCOL_VERSION,
            request: PluginRequest::Deploy {
                spec: json!({
                    "name": "btc-watcher",
                    "mode": "always_on",
                    "workload": { "execution": { "type": "llm", "system_prompt": "watch" } }
                }),
            },
        };
        let rpc = request_to_jsonrpc(RequestId::integer(7), &request).unwrap();
        let line = serde_json::to_string(&rpc).unwrap();
        let (id, parsed) = parse_jsonrpc_request(&line).unwrap();
        assert_eq!(id, RequestId::integer(7));
        assert_eq!(parsed, request);
    }

    #[test]
    fn deployment_response_round_trips() {
        let response = PluginResponse::Deployment {
            deployment: Deployment {
                deployment_id: "abc".to_string(),
                status: "running".to_string(),
                revision_id: Some("rev-1".to_string()),
                detail: Some(json!({ "name": "btc-watcher" })),
            },
        };
        let line = response_to_jsonrpc(&RequestId::integer(7), &response).unwrap();
        let (_id, parsed) = parse_jsonrpc_message(&line).unwrap();
        assert_eq!(parsed, response);
    }

    #[test]
    fn error_response_round_trips_with_dispatch_code() {
        let response = PluginResponse::Error {
            error: PluginErrorPayload {
                code: "invalid_spec".to_string(),
                message: "missing system_prompt".to_string(),
                details: Some(json!({ "field": "workload.execution.system_prompt" })),
            },
        };
        let line = response_to_jsonrpc(&RequestId::integer(3), &response).unwrap();
        let (_id, parsed) = parse_jsonrpc_message(&line).unwrap();
        match parsed {
            PluginResponse::Error { error } => {
                assert_eq!(error.code, "invalid_spec");
                assert_eq!(error.message, "missing system_prompt");
                assert_eq!(
                    error.details,
                    Some(json!({ "field": "workload.execution.system_prompt" }))
                );
            }
            other => panic!("expected error response, got {other:?}"),
        }
    }

    #[test]
    fn parse_request_rejects_method_mismatch() {
        let line = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "deployment.deploy",
            "params": { "protocol_version": 1, "kind": "capabilities" }
        }))
        .unwrap();
        let error = parse_jsonrpc_request(&line).unwrap_err();
        assert!(matches!(error, JsonRpcMessageError::MethodMismatch { .. }));
    }

    #[test]
    fn parse_request_rejects_missing_protocol_version() {
        let line = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "deployment.capabilities",
            "params": { "kind": "capabilities" }
        }))
        .unwrap();
        let error = parse_jsonrpc_request(&line).unwrap_err();
        assert!(matches!(error, JsonRpcMessageError::MissingProtocolVersion));
    }

    #[test]
    fn parse_request_rejects_non_object_params() {
        let line = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "deployment.capabilities",
            "params": "not-an-object"
        }))
        .unwrap();
        let error = parse_jsonrpc_request(&line).unwrap_err();
        assert!(matches!(error, JsonRpcMessageError::ParamsMustBeObject));
    }

    #[test]
    fn capabilities_request_round_trips_without_payload() {
        let request = PluginRequestEnvelope {
            protocol_version: DEPLOYMENT_PLUGIN_PROTOCOL_VERSION,
            request: PluginRequest::Capabilities,
        };
        let rpc = request_to_jsonrpc(RequestId::integer(1), &request).unwrap();
        let line = serde_json::to_string(&rpc).unwrap();
        let (_id, parsed) = parse_jsonrpc_request(&line).unwrap();
        assert_eq!(parsed, request);
    }

    #[test]
    fn request_method_table_is_exhaustive() {
        // Compile-time assurance that every variant maps to a unique method.
        let methods = [
            request_method(&PluginRequest::Capabilities),
            request_method(&PluginRequest::Configure {
                config: Value::Null,
            }),
            request_method(&PluginRequest::Health {
                config: Value::Null,
            }),
            request_method(&PluginRequest::Validate { spec: Value::Null }),
            request_method(&PluginRequest::TestRun {
                spec: Value::Null,
                sample_input: None,
            }),
            request_method(&PluginRequest::Deploy { spec: Value::Null }),
            request_method(&PluginRequest::PreviewUpdate {
                deployment_id: String::new(),
                patch: Value::Null,
            }),
            request_method(&PluginRequest::Update {
                deployment_id: String::new(),
                patch: Value::Null,
            }),
            request_method(&PluginRequest::Get {
                deployment_id: String::new(),
            }),
            request_method(&PluginRequest::List { filters: None }),
            request_method(&PluginRequest::ListRevisions {
                deployment_id: String::new(),
            }),
            request_method(&PluginRequest::PreviewRollback {
                deployment_id: String::new(),
                revision_id: String::new(),
            }),
            request_method(&PluginRequest::Rollback {
                deployment_id: String::new(),
                revision_id: String::new(),
            }),
            request_method(&PluginRequest::Start {
                deployment_id: String::new(),
            }),
            request_method(&PluginRequest::Stop {
                deployment_id: String::new(),
            }),
            request_method(&PluginRequest::Delete {
                deployment_id: String::new(),
            }),
            request_method(&PluginRequest::Shutdown),
        ];
        let mut sorted: Vec<&str> = methods.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), methods.len(), "duplicate method names");
        for method in methods {
            assert!(
                method.starts_with("deployment."),
                "method {method} missing deployment. prefix"
            );
        }
    }
}
