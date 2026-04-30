pub use dispatch_channel_protocol::{OutboundAttachment, OutboundMessageEnvelope};
use dispatch_plugin_rpc::{
    JSONRPC_APPLICATION_ERROR, JSONRPC_INTERNAL_ERROR, JSONRPC_INVALID_PARAMS,
    JSONRPC_INVALID_REQUEST, JSONRPC_METHOD_NOT_FOUND, JSONRPC_PARSE_ERROR, JsonRpcErrorResponse,
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcSuccessResponse, RequestId,
    ensure_jsonrpc_version, standard_error_code_name,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const COURIER_PLUGIN_PROTOCOL_VERSION: u32 = 1;
pub const COURIER_EVENT_NOTIFICATION_METHOD: &str = "courier.event";

pub use dispatch_plugin_rpc::{
    JsonRpcErrorObject, JsonRpcMessageError, RequestId as PluginRequestId,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginRequestEnvelope {
    pub protocol_version: u32,
    pub request: PluginRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginRequest {
    Capabilities,
    ValidateParcel {
        parcel_dir: String,
    },
    Inspect {
        parcel_dir: String,
    },
    OpenSession {
        parcel_dir: String,
    },
    ResumeSession {
        parcel_dir: String,
        session: CourierSession,
    },
    Shutdown,
    Run {
        parcel_dir: String,
        session: CourierSession,
        operation: CourierOperation,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginResponse {
    Capabilities { capabilities: CourierCapabilities },
    Inspection { inspection: CourierInspection },
    Session { session: CourierSession },
    Ok,
    Event { event: CourierEvent },
    Done { session: CourierSession },
    Error { error: PluginErrorPayload },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginErrorPayload {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CourierKind {
    Native,
    Docker,
    Wasm,
    Custom,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MountKind {
    Session,
    Memory,
    Artifacts,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MountConfig {
    pub kind: MountKind,
    pub driver: String,
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalPolicy {
    Never,
    Always,
    Confirm,
    Audit,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolRiskLevel {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum A2aEndpointMode {
    Auto,
    Card,
    Direct,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum A2aAuthScheme {
    Bearer,
    Header,
    Basic,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "scheme", rename_all = "snake_case")]
pub enum A2aAuthConfig {
    Bearer {
        secret_name: String,
    },
    Header {
        header_name: String,
        secret_name: String,
    },
    Basic {
        username_secret_name: String,
        password_secret_name: String,
    },
}

impl A2aAuthConfig {
    pub fn scheme(&self) -> A2aAuthScheme {
        match self {
            Self::Bearer { .. } => A2aAuthScheme::Bearer,
            Self::Header { .. } => A2aAuthScheme::Header,
            Self::Basic { .. } => A2aAuthScheme::Basic,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocalToolTransport {
    Local,
    A2a,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum LocalToolTarget {
    Local {
        packaged_path: String,
        command: String,
        args: Vec<String>,
    },
    A2a {
        endpoint_url: String,
        endpoint_mode: Option<A2aEndpointMode>,
        auth: Option<A2aAuthConfig>,
        expected_agent_name: Option<String>,
        expected_card_sha256: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalTargetKind {
    Local,
    Builtin,
    A2a,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalToolSpec {
    pub alias: String,
    pub approval: Option<ToolApprovalPolicy>,
    pub risk: Option<ToolRiskLevel>,
    pub description: Option<String>,
    pub input_schema_packaged_path: Option<String>,
    pub input_schema_sha256: Option<String>,
    pub skill_source: Option<String>,
    #[serde(flatten)]
    pub target: LocalToolTarget,
}

impl LocalToolSpec {
    pub fn transport(&self) -> LocalToolTransport {
        match self.target {
            LocalToolTarget::Local { .. } => LocalToolTransport::Local,
            LocalToolTarget::A2a { .. } => LocalToolTransport::A2a,
        }
    }

    pub fn packaged_path(&self) -> Option<&str> {
        match &self.target {
            LocalToolTarget::Local { packaged_path, .. } => Some(packaged_path.as_str()),
            LocalToolTarget::A2a { .. } => None,
        }
    }

    pub fn command(&self) -> &str {
        match &self.target {
            LocalToolTarget::Local { command, .. } => command.as_str(),
            LocalToolTarget::A2a { .. } => "dispatch-a2a",
        }
    }

    pub fn args(&self) -> &[String] {
        match &self.target {
            LocalToolTarget::Local { args, .. } => args.as_slice(),
            LocalToolTarget::A2a { .. } => &[],
        }
    }

    pub fn endpoint_url(&self) -> Option<&str> {
        match &self.target {
            LocalToolTarget::A2a { endpoint_url, .. } => Some(endpoint_url.as_str()),
            LocalToolTarget::Local { .. } => None,
        }
    }

    pub fn endpoint_mode(&self) -> Option<A2aEndpointMode> {
        match &self.target {
            LocalToolTarget::A2a { endpoint_mode, .. } => *endpoint_mode,
            LocalToolTarget::Local { .. } => None,
        }
    }

    pub fn auth(&self) -> Option<&A2aAuthConfig> {
        match &self.target {
            LocalToolTarget::A2a { auth, .. } => auth.as_ref(),
            LocalToolTarget::Local { .. } => None,
        }
    }

    pub fn auth_scheme(&self) -> Option<A2aAuthScheme> {
        self.auth().map(A2aAuthConfig::scheme)
    }

    pub fn auth_username_secret_name(&self) -> Option<&str> {
        match self.auth() {
            Some(A2aAuthConfig::Basic {
                username_secret_name,
                ..
            }) => Some(username_secret_name.as_str()),
            _ => None,
        }
    }

    pub fn auth_password_secret_name(&self) -> Option<&str> {
        match self.auth() {
            Some(A2aAuthConfig::Basic {
                password_secret_name,
                ..
            }) => Some(password_secret_name.as_str()),
            _ => None,
        }
    }

    pub fn expected_agent_name(&self) -> Option<&str> {
        match &self.target {
            LocalToolTarget::A2a {
                expected_agent_name,
                ..
            } => expected_agent_name.as_deref(),
            LocalToolTarget::Local { .. } => None,
        }
    }

    pub fn expected_card_sha256(&self) -> Option<&str> {
        match &self.target {
            LocalToolTarget::A2a {
                expected_card_sha256,
                ..
            } => expected_card_sha256.as_deref(),
            LocalToolTarget::Local { .. } => None,
        }
    }

    pub fn auth_header_name(&self) -> Option<&str> {
        match self.auth() {
            Some(A2aAuthConfig::Header { header_name, .. }) => Some(header_name.as_str()),
            _ => None,
        }
    }

    pub fn matches_name(&self, tool_name: &str) -> bool {
        self.alias == tool_name || self.packaged_path().is_some_and(|path| path == tool_name)
    }

    pub fn approval_kind(&self) -> ToolApprovalTargetKind {
        match self.target {
            LocalToolTarget::Local { .. } => ToolApprovalTargetKind::Local,
            LocalToolTarget::A2a { .. } => ToolApprovalTargetKind::A2a,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolRunResult {
    pub tool: String,
    pub command: String,
    pub args: Vec<String>,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedMount {
    pub kind: MountKind,
    pub driver: String,
    pub target_path: String,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolInvocation {
    pub name: String,
    pub input: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CourierOperation {
    ResolvePrompt,
    ListLocalTools,
    InvokeTool { invocation: ToolInvocation },
    Chat { input: String },
    Job { payload: String },
    Heartbeat { payload: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CourierCapabilities {
    pub courier_id: String,
    pub kind: CourierKind,
    pub supports_chat: bool,
    pub supports_job: bool,
    pub supports_heartbeat: bool,
    pub supports_local_tools: bool,
    pub supports_mounts: Vec<MountKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CourierInspection {
    pub courier_id: String,
    pub kind: CourierKind,
    pub entrypoint: Option<String>,
    pub required_secrets: Vec<String>,
    pub mounts: Vec<MountConfig>,
    pub local_tools: Vec<LocalToolSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CourierSession {
    pub id: String,
    pub parcel_digest: String,
    pub entrypoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub turn_count: u64,
    #[serde(default)]
    pub elapsed_ms: u64,
    pub history: Vec<ConversationMessage>,
    #[serde(default)]
    pub resolved_mounts: Vec<ResolvedMount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_state: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CourierEvent {
    PromptResolved {
        text: String,
    },
    LocalToolsListed {
        tools: Vec<LocalToolSpec>,
    },
    BackendFallback {
        backend: String,
        error: String,
    },
    ToolCallStarted {
        invocation: ToolInvocation,
        command: String,
        args: Vec<String>,
    },
    ToolCallFinished {
        result: ToolRunResult,
    },
    Message {
        role: String,
        content: String,
    },
    ChannelReply {
        message: OutboundMessageEnvelope,
    },
    TextDelta {
        content: String,
    },
    Done,
}

pub fn plugin_error(code: &str, message: impl Into<String>) -> PluginResponse {
    PluginResponse::Error {
        error: PluginErrorPayload {
            code: code.to_string(),
            message: message.into(),
        },
    }
}

pub fn request_method(request: &PluginRequest) -> &'static str {
    match request {
        PluginRequest::Capabilities => "courier.capabilities",
        PluginRequest::ValidateParcel { .. } => "courier.validate_parcel",
        PluginRequest::Inspect { .. } => "courier.inspect",
        PluginRequest::OpenSession { .. } => "courier.open_session",
        PluginRequest::ResumeSession { .. } => "courier.resume_session",
        PluginRequest::Shutdown => "courier.shutdown",
        PluginRequest::Run { .. } => "courier.run",
    }
}

pub fn request_to_jsonrpc(
    id: RequestId,
    envelope: &PluginRequestEnvelope,
) -> Result<JsonRpcRequest, JsonRpcMessageError> {
    let mut params = request_params_with_version(envelope.protocol_version, &envelope.request)?;
    if !matches!(params, Value::Object(_)) {
        let mut object = Map::new();
        object.insert(
            "protocol_version".to_string(),
            Value::from(envelope.protocol_version),
        );
        object.insert("payload".to_string(), params);
        params = Value::Object(object);
    }

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
        PluginResponse::Event { event } => JsonRpcMessage::Notification(JsonRpcNotification::new(
            COURIER_EVENT_NOTIFICATION_METHOD,
            Some(serde_json::to_value(event).map_err(|source| {
                JsonRpcMessageError::message(format!("failed to serialize courier event: {source}"))
            })?),
        )),
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
                    "failed to serialize plugin response: {source}"
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
                JsonRpcMessageError::message(format!("invalid courier result payload: {source}"))
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
        JsonRpcMessage::Notification(notification) => {
            ensure_jsonrpc_version(&notification.jsonrpc)?;
            if notification.method != COURIER_EVENT_NOTIFICATION_METHOD {
                return Err(JsonRpcMessageError::UnexpectedNotificationMethod(
                    notification.method,
                ));
            }
            let event = serde_json::from_value(
                notification
                    .params
                    .ok_or(JsonRpcMessageError::message("missing courier event params"))?,
            )
            .map_err(|source| {
                JsonRpcMessageError::message(format!(
                    "invalid courier notification payload: {source}"
                ))
            })?;
            Ok((None, PluginResponse::Event { event }))
        }
        JsonRpcMessage::Request(_) => Err(JsonRpcMessageError::message(
            "expected JSON-RPC response, got request",
        )),
    }
}

fn request_params_with_version(
    protocol_version: u32,
    request: &PluginRequest,
) -> Result<Value, JsonRpcMessageError> {
    let mut params = serde_json::to_value(request).map_err(|source| {
        JsonRpcMessageError::message(format!("failed to serialize request: {source}"))
    })?;
    let Value::Object(ref mut object) = params else {
        return Err(JsonRpcMessageError::message(
            "plugin request did not serialize to an object",
        ));
    };
    object.insert(
        "protocol_version".to_string(),
        Value::from(protocol_version),
    );
    Ok(params)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_request_round_trips_jsonrpc() {
        let request = PluginRequestEnvelope {
            protocol_version: COURIER_PLUGIN_PROTOCOL_VERSION,
            request: PluginRequest::Run {
                parcel_dir: "/tmp/demo".to_string(),
                session: CourierSession {
                    id: "session-1".to_string(),
                    parcel_digest: "digest".to_string(),
                    entrypoint: Some("chat".to_string()),
                    label: None,
                    turn_count: 1,
                    elapsed_ms: 0,
                    history: vec![ConversationMessage {
                        role: "user".to_string(),
                        content: "hello".to_string(),
                    }],
                    resolved_mounts: Vec::new(),
                    backend_state: None,
                },
                operation: CourierOperation::Chat {
                    input: "hello".to_string(),
                },
            },
        };

        let rpc = request_to_jsonrpc(RequestId::integer(5), &request).unwrap();
        let json = serde_json::to_string(&rpc).unwrap();
        let (id, parsed) = parse_jsonrpc_request(&json).unwrap();
        assert_eq!(id, RequestId::integer(5));
        assert_eq!(parsed, request);
    }

    #[test]
    fn response_round_trips_jsonrpc() {
        let response = PluginResponse::Capabilities {
            capabilities: CourierCapabilities {
                courier_id: "demo".to_string(),
                kind: CourierKind::Custom,
                supports_chat: true,
                supports_job: false,
                supports_heartbeat: false,
                supports_local_tools: false,
                supports_mounts: Vec::new(),
            },
        };

        let json = response_to_jsonrpc(&RequestId::integer(8), &response).unwrap();
        let (id, parsed) = parse_jsonrpc_message(&json).unwrap();
        assert_eq!(id, Some(RequestId::integer(8)));
        assert_eq!(parsed, response);
    }

    #[test]
    fn request_rejects_invalid_jsonrpc_version() {
        let json = serde_json::json!({
            "jsonrpc": "1.0",
            "id": 6,
            "method": "courier.capabilities",
            "params": {
                "protocol_version": COURIER_PLUGIN_PROTOCOL_VERSION,
                "kind": "capabilities"
            }
        });

        let error = parse_jsonrpc_request(&json.to_string())
            .expect_err("expected invalid JSON-RPC version to fail");
        assert!(error.to_string().contains("expected jsonrpc version 2.0"));
    }

    #[test]
    fn run_event_response_round_trips_notification() {
        let response = PluginResponse::Event {
            event: CourierEvent::ChannelReply {
                message: OutboundMessageEnvelope {
                    content: "reply text".to_string(),
                    content_type: Some("text/plain".to_string()),
                    attachments: Vec::new(),
                    metadata: Default::default(),
                },
            },
        };

        let json = response_to_jsonrpc(&RequestId::integer(9), &response).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["method"], COURIER_EVENT_NOTIFICATION_METHOD);
        assert_eq!(value["params"]["kind"], "channel_reply");
        assert!(value["params"].get("event").is_none());

        let (id, parsed) = parse_jsonrpc_message(&json).unwrap();
        assert_eq!(id, None);
        assert_eq!(parsed, response);
    }

    #[test]
    fn error_round_trips_jsonrpc() {
        let response = plugin_error("bad_request", "missing parcel_dir");
        let json = response_to_jsonrpc(&RequestId::integer(10), &response).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["error"]["code"], JSONRPC_INVALID_PARAMS);

        let (id, parsed) = parse_jsonrpc_message(&json).unwrap();
        assert_eq!(id, Some(RequestId::integer(10)));
        assert_eq!(parsed, response);
    }

    #[test]
    fn plugin_error_builds_error_response() {
        let response = plugin_error("bad_request", "missing parcel_dir");
        assert_eq!(
            response,
            PluginResponse::Error {
                error: PluginErrorPayload {
                    code: "bad_request".to_string(),
                    message: "missing parcel_dir".to_string(),
                },
            }
        );
    }

    #[test]
    fn standard_jsonrpc_error_without_dispatch_payload_uses_named_code() {
        let json = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 12,
            "error": {
                "code": JSONRPC_METHOD_NOT_FOUND,
                "message": "unknown method"
            }
        });

        let (id, parsed) = parse_jsonrpc_message(&json.to_string()).unwrap();
        assert_eq!(id, Some(RequestId::integer(12)));
        assert_eq!(
            parsed,
            PluginResponse::Error {
                error: PluginErrorPayload {
                    code: "method_not_found".to_string(),
                    message: "unknown method".to_string(),
                },
            }
        );
    }
}
