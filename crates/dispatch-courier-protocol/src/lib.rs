use dispatch_channel_protocol::OutboundMessageEnvelope;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const COURIER_PLUGIN_PROTOCOL_VERSION: u32 = 1;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_request_round_trips_json() {
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

        let json = serde_json::to_string(&request).unwrap();
        let parsed: PluginRequestEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, request);
    }

    #[test]
    fn response_round_trips_json() {
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

        let json = serde_json::to_string(&response).unwrap();
        let parsed: PluginResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, response);
    }

    #[test]
    fn run_event_response_round_trips_channel_reply() {
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

        let json = serde_json::to_string(&response).unwrap();
        let parsed: PluginResponse = serde_json::from_str(&json).unwrap();
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
