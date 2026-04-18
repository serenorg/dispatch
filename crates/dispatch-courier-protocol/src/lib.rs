pub use dispatch_channel_protocol::{OutboundAttachment, OutboundMessageEnvelope};
use dispatch_plugin_rpc::{
    JSONRPC_APPLICATION_ERROR, JsonRpcErrorResponse, JsonRpcMessage, JsonRpcNotification,
    JsonRpcRequest, JsonRpcSuccessResponse, RequestId, ensure_jsonrpc_version,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const COURIER_PLUGIN_PROTOCOL_VERSION: u32 = 1;
pub const COURIER_EVENT_NOTIFICATION_METHOD: &str = "courier.event";

pub use dispatch_plugin_rpc::{JsonRpcErrorObject, RequestId as PluginRequestId};

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
) -> Result<JsonRpcRequest, String> {
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

pub fn parse_jsonrpc_request(line: &str) -> Result<(RequestId, PluginRequestEnvelope), String> {
    let message: JsonRpcMessage = serde_json::from_str(line)
        .map_err(|source| format!("invalid JSON-RPC message: {source}"))?;
    let JsonRpcMessage::Request(request) = message else {
        return Err("expected JSON-RPC request".to_string());
    };
    ensure_jsonrpc_version(&request.jsonrpc)?;
    let params = request
        .params
        .ok_or_else(|| "missing JSON-RPC params".to_string())?;
    let envelope = decode_request_params(&request.method, params)?;
    Ok((request.id, envelope))
}

pub fn response_to_jsonrpc(id: &RequestId, response: &PluginResponse) -> Result<String, String> {
    let message = match response {
        PluginResponse::Event { .. } => JsonRpcMessage::Notification(JsonRpcNotification::new(
            COURIER_EVENT_NOTIFICATION_METHOD,
            Some(
                serde_json::to_value(response)
                    .map_err(|source| format!("failed to serialize courier event: {source}"))?,
            ),
        )),
        PluginResponse::Error { error } => JsonRpcMessage::Error(JsonRpcErrorResponse::new(
            Some(id.clone()),
            JSONRPC_APPLICATION_ERROR,
            error.message.clone(),
            Some(serde_json::json!({ "dispatch_error": error })),
        )),
        other => JsonRpcMessage::Response(JsonRpcSuccessResponse::new(
            id.clone(),
            serde_json::to_value(other)
                .map_err(|source| format!("failed to serialize plugin response: {source}"))?,
        )),
    };
    serde_json::to_string(&message)
        .map_err(|source| format!("failed to serialize JSON-RPC message: {source}"))
}

pub fn parse_jsonrpc_message(line: &str) -> Result<PluginResponse, String> {
    let message: JsonRpcMessage = serde_json::from_str(line)
        .map_err(|source| format!("invalid JSON-RPC message: {source}"))?;
    match message {
        JsonRpcMessage::Response(response) => {
            ensure_jsonrpc_version(&response.jsonrpc)?;
            serde_json::from_value(response.result)
                .map_err(|source| format!("invalid courier result payload: {source}"))
        }
        JsonRpcMessage::Error(error) => {
            ensure_jsonrpc_version(&error.jsonrpc)?;
            Ok(PluginResponse::Error {
                error: decode_dispatch_error(error),
            })
        }
        JsonRpcMessage::Notification(notification) => {
            ensure_jsonrpc_version(&notification.jsonrpc)?;
            if notification.method != COURIER_EVENT_NOTIFICATION_METHOD {
                return Err(format!(
                    "unexpected JSON-RPC notification method `{}`",
                    notification.method
                ));
            }
            serde_json::from_value(
                notification
                    .params
                    .ok_or_else(|| "missing courier event params".to_string())?,
            )
            .map_err(|source| format!("invalid courier notification payload: {source}"))
        }
        JsonRpcMessage::Request(_) => Err("expected JSON-RPC response, got request".to_string()),
    }
}

fn request_params_with_version(
    protocol_version: u32,
    request: &PluginRequest,
) -> Result<Value, String> {
    let mut params = serde_json::to_value(request)
        .map_err(|source| format!("failed to serialize request: {source}"))?;
    let Value::Object(ref mut object) = params else {
        return Err("plugin request did not serialize to an object".to_string());
    };
    object.insert(
        "protocol_version".to_string(),
        Value::from(protocol_version),
    );
    Ok(params)
}

fn decode_request_params(method: &str, params: Value) -> Result<PluginRequestEnvelope, String> {
    let Value::Object(mut object) = params else {
        return Err("JSON-RPC params must be an object".to_string());
    };
    let protocol_version = object
        .remove("protocol_version")
        .ok_or_else(|| "missing protocol_version in JSON-RPC params".to_string())?
        .as_u64()
        .ok_or_else(|| "protocol_version must be an unsigned integer".to_string())?
        as u32;
    let request: PluginRequest = serde_json::from_value(Value::Object(object))
        .map_err(|source| format!("invalid plugin request params: {source}"))?;
    let expected_method = request_method(&request);
    if expected_method != method {
        return Err(format!(
            "JSON-RPC method `{method}` did not match request payload `{expected_method}`"
        ));
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
        code: "jsonrpc_error".to_string(),
        message: error.error.message,
    })
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
        let parsed = parse_jsonrpc_message(&json).unwrap();
        assert_eq!(parsed, response);
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
        let parsed = parse_jsonrpc_message(&json).unwrap();
        assert_eq!(parsed, response);
    }

    #[test]
    fn error_round_trips_jsonrpc() {
        let response = plugin_error("bad_request", "missing parcel_dir");
        let json = response_to_jsonrpc(&RequestId::integer(10), &response).unwrap();
        let parsed = parse_jsonrpc_message(&json).unwrap();
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
}
