use dispatch_plugin_rpc::{
    JSONRPC_APPLICATION_ERROR, JSONRPC_INTERNAL_ERROR, JSONRPC_INVALID_PARAMS,
    JSONRPC_INVALID_REQUEST, JSONRPC_METHOD_NOT_FOUND, JSONRPC_PARSE_ERROR, JsonRpcErrorResponse,
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcSuccessResponse, RequestId,
    ensure_jsonrpc_version, standard_error_code_name,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const CHANNEL_PLUGIN_PROTOCOL_VERSION: u32 = 1;
pub const CHANNEL_EVENT_NOTIFICATION_METHOD: &str = "channel.event";

pub use dispatch_plugin_rpc::{
    JsonRpcErrorObject, JsonRpcMessageError, RequestId as PluginRequestId,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AttachmentSource {
    DataBase64,
    Url,
    StorageKey,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct TaggedChannelReplyEnvelope {
    kind: String,
    #[serde(flatten)]
    reply: OutboundMessageEnvelope,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StatusKind {
    Processing,
    Completed,
    Cancelled,
    OperationStarted,
    OperationFinished,
    ApprovalNeeded,
    Info,
    Delivering,
    AuthRequired,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginRequestEnvelope<R> {
    pub protocol_version: u32,
    pub request: R,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginNotificationEnvelope<N> {
    pub protocol_version: u32,
    #[serde(flatten)]
    pub notification: N,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginRequest<C, M> {
    Capabilities,
    Configure {
        config: C,
    },
    Health {
        config: C,
    },
    PollIngress {
        config: C,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<IngressState>,
    },
    StartIngress {
        config: C,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<IngressState>,
    },
    StopIngress {
        config: C,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<IngressState>,
    },
    IngressEvent {
        config: C,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<IngressState>,
        payload: IngressPayload,
    },
    Deliver {
        config: C,
        message: M,
    },
    Push {
        config: C,
        message: M,
    },
    /// Fetch one already-delivered message by receipt-bound reference.
    ///
    /// The host authorizes `reference.conversation_id` against channel scope
    /// before issuing this request; the provider must fetch only the named
    /// message from the named conversation and reject a mismatched response.
    GetMessage {
        config: C,
        reference: MessageRef,
    },
    /// Resolve the canonical permalink for one message by receipt-bound
    /// reference. Authorized identically to [`PluginRequest::GetMessage`].
    GetPermalink {
        config: C,
        reference: MessageRef,
    },
    Status {
        config: C,
        update: StatusFrame,
    },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginResponse {
    Capabilities {
        capabilities: ChannelCapabilities,
    },
    Configured {
        configuration: Box<ConfiguredChannel>,
    },
    Health {
        health: HealthReport,
    },
    IngressStarted {
        state: IngressState,
    },
    IngressStopped {
        state: IngressState,
    },
    IngressEventsReceived {
        events: Vec<InboundEventEnvelope>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        callback_reply: Option<IngressCallbackReply>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<IngressState>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        poll_after_ms: Option<u64>,
    },
    Delivered {
        delivery: DeliveryReceipt,
    },
    Pushed {
        delivery: DeliveryReceipt,
    },
    /// The exact requested message, fetched back from the provider.
    MessageFetched {
        message: FetchedMessage,
    },
    /// The requested message could not be found (missing or deleted).
    ///
    /// A stable, typed negative result echoing the requested reference, so a
    /// caller never mistakes a nearby message for the one it asked for.
    MessageNotFound {
        reference: MessageRef,
    },
    /// The canonical permalink for the requested message.
    PermalinkResolved {
        permalink: MessagePermalink,
    },
    StatusAccepted {
        status: StatusAcceptance,
    },
    Ok,
    Error {
        error: PluginErrorPayload,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelEventNotification {
    #[serde(default)]
    pub events: Vec<InboundEventEnvelope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<IngressState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_after_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginMessage {
    Response {
        id: RequestId,
        response: PluginResponse,
    },
    Notification(PluginNotificationEnvelope<ChannelEventNotification>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginErrorPayload {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ThreadingModel {
    ChatOrTopic,
    ChannelOrThread,
    ChatOrThread,
    PhoneNumber,
    CallerDefined,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum IngressMode {
    Webhook,
    EventsWebhook,
    InteractionWebhook,
    Polling,
    Websocket,
    #[serde(other)]
    Unknown,
}

impl IngressMode {
    pub fn wire_name(&self) -> &'static str {
        match self {
            Self::Webhook => "webhook",
            Self::EventsWebhook => "events_webhook",
            Self::InteractionWebhook => "interaction_webhook",
            Self::Polling => "polling",
            Self::Websocket => "websocket",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_wire_name(value: &str) -> Option<Self> {
        match value {
            "webhook" => Some(Self::Webhook),
            "events_webhook" => Some(Self::EventsWebhook),
            "interaction_webhook" => Some(Self::InteractionWebhook),
            "polling" => Some(Self::Polling),
            "websocket" => Some(Self::Websocket),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelCapabilities {
    pub plugin_id: String,
    pub platform: String,
    pub ingress_modes: Vec<IngressMode>,
    pub outbound_message_types: Vec<String>,
    pub threading_model: ThreadingModel,
    pub attachment_support: bool,
    pub reply_verification_support: bool,
    pub account_scoped_config: bool,
    #[serde(default)]
    pub accepts_push: bool,
    #[serde(default)]
    pub accepts_status_frames: bool,
    #[serde(default)]
    pub attachment_sources: Vec<AttachmentSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attachment_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfiguredChannel {
    pub metadata: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<ChannelPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeStateSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthReport {
    pub ok: bool,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IngressState {
    pub mode: IngressMode,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IngressPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_id: Option<String>,
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub query: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_query: Option<String>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub trust_verified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub received_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IngressCallbackReply {
    pub status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default)]
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeliveryReceipt {
    pub message_id: String,
    pub conversation_id: String,
    /// Stable, typed handle to the delivered message for receipt-bound read-back.
    ///
    /// Carries the exact provider coordinates (`message_id`, `conversation_id`,
    /// optional thread and workspace) so a governed `GetMessage`/`GetPermalink`
    /// can name the artifact without parsing provider-specific `metadata`
    /// strings. Absent when the provider cannot describe the delivered message
    /// as a re-fetchable reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_ref: Option<MessageRef>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// A provider-neutral handle to one delivered message.
///
/// Every read-back operation is bound to a reference: the conversation is
/// always authorized against channel scope before the provider is queried, so
/// a message identifier can never widen access beyond its conversation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageRef {
    /// Conversation (channel/chat) the message was delivered to.
    pub conversation_id: String,
    /// Provider message identifier (for Slack, the message `ts`).
    pub message_id: String,
    /// Thread parent identifier when the provider requires it to resolve a
    /// threaded reply. Absent for top-level conversation messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Provider workspace, team, or guild that owns the conversation.
    ///
    /// Scoped one level above `conversation_id`; carried so a read-back can
    /// reject a provider response that resolves to a different workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

/// One message fetched back through a receipt-bound read operation.
///
/// The provider returns exactly the requested message, normalized to these
/// typed fields. A missing or deleted message is reported as
/// [`PluginResponse::MessageNotFound`], never as a different nearby message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FetchedMessage {
    /// The exact provider coordinates resolved by the read-back.
    ///
    /// Flattened to keep the wire fields provider-neutral while ensuring Rust
    /// consumers compare the complete reference rather than selecting a subset
    /// of its conversation, message, thread, and workspace coordinates.
    #[serde(flatten)]
    pub reference: MessageRef,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<FetchedMessageAuthor>,
    /// Canonical artifact URL for the exact fetched message, when the provider
    /// resolves one during the fetch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permalink: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// The author identity of a fetched message, when the provider exposes it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FetchedMessageAuthor {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default)]
    pub is_bot: bool,
}

/// A permalink resolved for one exact message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessagePermalink {
    /// The exact provider coordinates for the resolved permalink.
    #[serde(flatten)]
    pub reference: MessageRef,
    pub url: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusAcceptance {
    #[serde(default)]
    pub accepted: bool,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusFrame {
    pub kind: StatusKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InboundEventEnvelope {
    pub event_id: String,
    pub platform: String,
    pub event_type: String,
    pub received_at: String,
    pub conversation: InboundConversationRef,
    pub actor: InboundActor,
    pub message: InboundMessage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Activation evidence a host uses to re-authorize the event on its own.
    ///
    /// Absent when the plugin reports no activation evidence, so a host that
    /// requires evidence rejects the event instead of inferring a reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation: Option<InboundActivation>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InboundConversationRef {
    pub id: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_message_id: Option<String>,
    /// Provider workspace, server, or guild that owns the conversation.
    ///
    /// Scoped one level above `id`: a workspace contains many conversations, so
    /// the two identifiers are not interchangeable when checking policy scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Conversation this thread or sub-conversation descends from.
    ///
    /// Set when `id` names a child thread, so a host can resolve the thread back
    /// to the parent conversation that policy was granted against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_conversation_id: Option<String>,
}

/// Evidence for why a plugin treated an inbound event as addressed to the agent.
///
/// Every field is a provider-supplied value relayed by the plugin, so a host
/// treats it as untrusted input and re-checks it against its own policy rather
/// than accepting the plugin's activation decision on its own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InboundActivation {
    /// Why the plugin considers this event addressed to the agent.
    pub reason: String,
    /// Provider account identity the plugin authenticated as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_account_id: Option<String>,
    /// Author of the message this event replies to, when the activation is a reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referenced_message_author_id: Option<String>,
}

impl InboundActivation {
    /// The agent was named directly in the message.
    pub const REASON_DIRECT_MENTION: &'static str = "direct_mention";
    /// The message replies to one the agent authored.
    pub const REASON_REPLY_TO_AGENT: &'static str = "reply_to_agent";
    /// The event came from a command the provider routed to the agent.
    pub const REASON_SLASH_COMMAND: &'static str = "slash_command";
    /// The event arrived in a one-to-one conversation with the agent.
    pub const REASON_DIRECT_MESSAGE: &'static str = "direct_message";
    /// The binding activates on every message in the conversation.
    pub const REASON_ALL_MESSAGES: &'static str = "all_messages";
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InboundActor {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default)]
    pub is_bot: bool,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InboundMessage {
    pub id: String,
    pub content: String,
    pub content_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<String>,
    #[serde(default)]
    pub attachments: Vec<InboundAttachment>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InboundAttachment {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extracted_text: Option<String>,
    #[serde(default)]
    pub extras: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutboundMessageEnvelope {
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default)]
    pub attachments: Vec<OutboundAttachment>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

pub fn parse_tagged_channel_reply(reply_text: &str) -> Option<OutboundMessageEnvelope> {
    let tagged = serde_json::from_str::<TaggedChannelReplyEnvelope>(reply_text).ok()?;
    if tagged.kind == "channel_reply" {
        Some(tagged.reply)
    } else {
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutboundAttachment {
    pub name: String,
    pub mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_base64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_key: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
    #[serde(default)]
    pub allowed_sender_ids: Vec<String>,
    /// Conversations this binding may act on.
    ///
    /// Conversation-scoped only. A workspace, server, or guild identifier is one
    /// level wider and belongs in `allowed_workspace_ids`; placing one here would
    /// match no conversation, or the wrong one if the provider reuses the value.
    #[serde(default)]
    pub allowed_conversation_ids: Vec<String>,
    /// Workspaces, servers, or guilds this binding may act within.
    ///
    /// Scoped one level above `allowed_conversation_ids` and checked separately,
    /// so widening the workspace scope does not widen the conversation scope.
    #[serde(default)]
    pub allowed_workspace_ids: Vec<String>,
    /// Destinations this binding may publish to.
    ///
    /// Empty means outbound is no wider than `allowed_conversation_ids`, so an
    /// unset value never grants a destination the inbound scope excludes.
    #[serde(default)]
    pub allowed_outbound_conversation_ids: Vec<String>,
    /// Activation mode in force for this binding.
    ///
    /// Recorded so a host evaluates inbound activation evidence against the mode
    /// the binding was granted, not the mode the plugin reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation: Option<String>,
    /// How child threads of an allowed conversation are treated.
    ///
    /// Stated explicitly because a thread is a distinct conversation on most
    /// providers, so parent scope does not imply child scope on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_policy: Option<String>,
    /// Explicit child-thread conversations when `thread_policy` is allowlist.
    #[serde(default)]
    pub allowed_thread_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dm_policy: Option<String>,
    /// Explicit direct-message senders when `dm_policy` is allowlist.
    #[serde(default)]
    pub allowed_dm_sender_ids: Vec<String>,
    /// Which runtime path owns the visible reply for one inbound event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_delivery: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_signature_validation: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_group_messages: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attachment_bytes: Option<u64>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeStateSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message_id: Option<String>,
    #[serde(default)]
    pub cursors: BTreeMap<String, String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

pub fn plugin_error(code: &str, message: impl Into<String>) -> PluginResponse {
    PluginResponse::Error {
        error: PluginErrorPayload {
            code: code.to_string(),
            message: message.into(),
        },
    }
}

pub fn request_method<C, M>(request: &PluginRequest<C, M>) -> &'static str {
    match request {
        PluginRequest::Capabilities => "channel.capabilities",
        PluginRequest::Configure { .. } => "channel.configure",
        PluginRequest::Health { .. } => "channel.health",
        PluginRequest::PollIngress { .. } => "channel.poll_ingress",
        PluginRequest::StartIngress { .. } => "channel.start_ingress",
        PluginRequest::StopIngress { .. } => "channel.stop_ingress",
        PluginRequest::IngressEvent { .. } => "channel.ingress_event",
        PluginRequest::Deliver { .. } => "channel.deliver",
        PluginRequest::Push { .. } => "channel.push",
        PluginRequest::GetMessage { .. } => "channel.get_message",
        PluginRequest::GetPermalink { .. } => "channel.get_permalink",
        PluginRequest::Status { .. } => "channel.status",
        PluginRequest::Shutdown => "channel.shutdown",
    }
}

pub fn request_to_jsonrpc<C: Serialize, M: Serialize>(
    id: RequestId,
    envelope: &PluginRequestEnvelope<PluginRequest<C, M>>,
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

pub fn parse_jsonrpc_request<C: DeserializeOwned, M: DeserializeOwned>(
    line: &str,
) -> Result<(RequestId, PluginRequestEnvelope<PluginRequest<C, M>>), JsonRpcMessageError> {
    let message: JsonRpcMessage =
        serde_json::from_str(line).map_err(JsonRpcMessageError::invalid_json)?;
    let JsonRpcMessage::Request(request) = message else {
        return Err(JsonRpcMessageError::ExpectedRequest);
    };
    ensure_jsonrpc_version(&request.jsonrpc)?;
    let params = request.params.ok_or(JsonRpcMessageError::MissingParams)?;
    let envelope = decode_request_params::<C, M>(&request.method, params)?;
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
                    "failed to serialize plugin response: {source}"
                ))
            })?,
        )),
    };
    serde_json::to_string(&message).map_err(|source| {
        JsonRpcMessageError::message(format!("failed to serialize JSON-RPC response: {source}"))
    })
}

pub fn parse_jsonrpc_response(
    line: &str,
) -> Result<(RequestId, PluginResponse), JsonRpcMessageError> {
    match parse_jsonrpc_message(line)? {
        PluginMessage::Response { id, response } => Ok((id, response)),
        PluginMessage::Notification(_) => Err(JsonRpcMessageError::UnexpectedNotification),
    }
}

pub fn notification_to_jsonrpc(
    envelope: &PluginNotificationEnvelope<ChannelEventNotification>,
) -> Result<String, JsonRpcMessageError> {
    let params = notification_params_with_version(envelope)?;
    let message = JsonRpcMessage::Notification(JsonRpcNotification::new(
        CHANNEL_EVENT_NOTIFICATION_METHOD,
        Some(params),
    ));
    serde_json::to_string(&message).map_err(|source| {
        JsonRpcMessageError::message(format!(
            "failed to serialize JSON-RPC notification: {source}"
        ))
    })
}

pub fn parse_jsonrpc_message(line: &str) -> Result<PluginMessage, JsonRpcMessageError> {
    let message: JsonRpcMessage =
        serde_json::from_str(line).map_err(JsonRpcMessageError::invalid_json)?;
    match message {
        JsonRpcMessage::Response(response) => {
            ensure_jsonrpc_version(&response.jsonrpc)?;
            let id = response.id;
            let response = serde_json::from_value(response.result).map_err(|source| {
                JsonRpcMessageError::message(format!("invalid plugin result payload: {source}"))
            })?;
            Ok(PluginMessage::Response { id, response })
        }
        JsonRpcMessage::Error(error) => {
            ensure_jsonrpc_version(&error.jsonrpc)?;
            let id = error
                .id
                .clone()
                .ok_or(JsonRpcMessageError::MissingResponseId)?;
            Ok(PluginMessage::Response {
                id,
                response: PluginResponse::Error {
                    error: decode_dispatch_error(error),
                },
            })
        }
        JsonRpcMessage::Notification(notification) => {
            ensure_jsonrpc_version(&notification.jsonrpc)?;
            Ok(PluginMessage::Notification(decode_notification_params(
                &notification.method,
                notification
                    .params
                    .ok_or(JsonRpcMessageError::MissingParams)?,
            )?))
        }
        JsonRpcMessage::Request(_) => Err(JsonRpcMessageError::UnexpectedRequest),
    }
}

fn request_params_with_version<C: Serialize, M: Serialize>(
    protocol_version: u32,
    request: &PluginRequest<C, M>,
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

fn notification_params_with_version(
    envelope: &PluginNotificationEnvelope<ChannelEventNotification>,
) -> Result<Value, JsonRpcMessageError> {
    let mut params = serde_json::to_value(&envelope.notification).map_err(|source| {
        JsonRpcMessageError::message(format!("failed to serialize notification: {source}"))
    })?;
    let Value::Object(ref mut object) = params else {
        return Err(JsonRpcMessageError::message(
            "channel plugin notification did not serialize to an object",
        ));
    };
    object.insert(
        "protocol_version".to_string(),
        Value::from(envelope.protocol_version),
    );
    Ok(params)
}

fn decode_request_params<C: DeserializeOwned, M: DeserializeOwned>(
    method: &str,
    params: Value,
) -> Result<PluginRequestEnvelope<PluginRequest<C, M>>, JsonRpcMessageError> {
    let Value::Object(mut object) = params else {
        return Err(JsonRpcMessageError::ParamsMustBeObject);
    };
    let protocol_version = object
        .remove("protocol_version")
        .ok_or(JsonRpcMessageError::MissingProtocolVersion)?
        .as_u64()
        .ok_or(JsonRpcMessageError::InvalidProtocolVersion)? as u32;
    let request: PluginRequest<C, M> =
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

fn decode_notification_params(
    method: &str,
    params: Value,
) -> Result<PluginNotificationEnvelope<ChannelEventNotification>, JsonRpcMessageError> {
    if method != CHANNEL_EVENT_NOTIFICATION_METHOD {
        return Err(JsonRpcMessageError::UnexpectedNotificationMethod(
            method.to_string(),
        ));
    }

    let Value::Object(mut object) = params else {
        return Err(JsonRpcMessageError::ParamsMustBeObject);
    };
    let protocol_version = object
        .remove("protocol_version")
        .ok_or(JsonRpcMessageError::MissingProtocolVersion)?
        .as_u64()
        .ok_or(JsonRpcMessageError::InvalidProtocolVersion)? as u32;
    let notification = serde_json::from_value(Value::Object(object)).map_err(|source| {
        JsonRpcMessageError::message(format!("invalid channel notification payload: {source}"))
    })?;
    Ok(PluginNotificationEnvelope {
        protocol_version,
        notification,
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

    type JsonRequest = PluginRequest<serde_json::Value, serde_json::Value>;
    type JsonEnvelope = PluginRequestEnvelope<JsonRequest>;

    #[test]
    fn enum_wire_names_use_snake_case() {
        assert_eq!(
            serde_json::to_string(&StatusKind::OperationStarted).unwrap(),
            "\"operation_started\""
        );
        assert_eq!(
            serde_json::to_string(&ThreadingModel::ChannelOrThread).unwrap(),
            "\"channel_or_thread\""
        );
        assert_eq!(
            serde_json::to_string(&IngressMode::InteractionWebhook).unwrap(),
            "\"interaction_webhook\""
        );
        assert_eq!(
            serde_json::to_string(&IngressMode::Websocket).unwrap(),
            "\"websocket\""
        );
        assert_eq!(IngressMode::Websocket.wire_name(), "websocket");
        assert_eq!(
            IngressMode::InteractionWebhook.wire_name(),
            "interaction_webhook"
        );
        for mode in [
            IngressMode::Webhook,
            IngressMode::EventsWebhook,
            IngressMode::InteractionWebhook,
            IngressMode::Polling,
            IngressMode::Websocket,
            IngressMode::Unknown,
        ] {
            assert_eq!(IngressMode::from_wire_name(mode.wire_name()), Some(mode));
        }
        assert_eq!(IngressMode::from_wire_name("future_mode"), None);
    }

    #[test]
    fn unknown_enum_values_fall_back() {
        let status_kind: StatusKind = serde_json::from_str("\"future_status_kind\"").unwrap();
        assert_eq!(status_kind, StatusKind::Unknown);

        let threading_model: ThreadingModel =
            serde_json::from_str("\"future_threading_model\"").unwrap();
        assert_eq!(threading_model, ThreadingModel::Unknown);

        let ingress_mode: IngressMode = serde_json::from_str("\"future_ingress_mode\"").unwrap();
        assert_eq!(ingress_mode, IngressMode::Unknown);
    }

    #[test]
    fn request_round_trips_jsonrpc() {
        let request = JsonEnvelope {
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            request: PluginRequest::Status {
                config: serde_json::json!({ "bot_token_env": "TOKEN" }),
                update: StatusFrame {
                    kind: StatusKind::Processing,
                    message: "working".to_string(),
                    conversation_id: Some("chat-1".to_string()),
                    thread_id: None,
                    metadata: BTreeMap::new(),
                },
            },
        };

        let rpc = request_to_jsonrpc(RequestId::integer(7), &request).unwrap();
        let json = serde_json::to_string(&rpc).unwrap();
        let (id, parsed) =
            parse_jsonrpc_request::<serde_json::Value, serde_json::Value>(&json).unwrap();
        assert_eq!(id, RequestId::integer(7));
        assert_eq!(parsed, request);
    }

    #[test]
    fn response_round_trips_jsonrpc() {
        let response = PluginResponse::Capabilities {
            capabilities: ChannelCapabilities {
                plugin_id: "telegram".to_string(),
                platform: "telegram".to_string(),
                ingress_modes: vec![IngressMode::Webhook],
                outbound_message_types: vec!["text".to_string()],
                threading_model: ThreadingModel::ChatOrTopic,
                attachment_support: false,
                reply_verification_support: true,
                account_scoped_config: true,
                accepts_push: true,
                accepts_status_frames: true,
                attachment_sources: vec![AttachmentSource::DataBase64],
                max_attachment_bytes: None,
            },
        };

        let json = response_to_jsonrpc(&RequestId::integer(9), &response).unwrap();
        let (id, parsed) = parse_jsonrpc_response(&json).unwrap();
        assert_eq!(id, RequestId::integer(9));
        assert_eq!(parsed, response);
    }

    #[test]
    fn error_response_round_trips_jsonrpc() {
        let response = plugin_error("bad_request", "missing webhook token");

        let json = response_to_jsonrpc(&RequestId::integer(11), &response).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["error"]["code"], JSONRPC_INVALID_PARAMS);

        let (id, parsed) = parse_jsonrpc_response(&json).unwrap();
        assert_eq!(id, RequestId::integer(11));
        assert_eq!(parsed, response);
    }

    #[test]
    fn request_rejects_method_payload_mismatch() {
        let json = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "channel.configure",
            "params": {
                "protocol_version": CHANNEL_PLUGIN_PROTOCOL_VERSION,
                "kind": "capabilities"
            }
        });

        let error =
            parse_jsonrpc_request::<serde_json::Value, serde_json::Value>(&json.to_string())
                .expect_err("expected method mismatch to fail");
        assert!(error.to_string().contains("did not match request payload"));
    }

    #[test]
    fn response_rejects_notification() {
        let notification = JsonRpcNotification::new(
            CHANNEL_EVENT_NOTIFICATION_METHOD,
            Some(serde_json::json!({
                "protocol_version": CHANNEL_PLUGIN_PROTOCOL_VERSION,
                "events": [{
                    "event_id": "evt-1",
                    "platform": "telegram",
                    "event_type": "message.received",
                    "received_at": "2026-04-12T00:00:00Z",
                    "conversation": {
                        "id": "chat-1",
                        "kind": "private"
                    },
                    "actor": {
                        "id": "user-1",
                        "is_bot": false,
                        "metadata": {}
                    },
                    "message": {
                        "id": "msg-1",
                        "content": "hello",
                        "content_type": "text/plain",
                        "attachments": [],
                        "metadata": {}
                    },
                    "metadata": {}
                }],
                "poll_after_ms": 25
            })),
        );
        let json = serde_json::to_string(&notification).unwrap();

        let error = parse_jsonrpc_response(&json).expect_err("expected notification to fail");
        assert!(
            error
                .to_string()
                .contains("expected JSON-RPC response, got notification")
        );
    }

    #[test]
    fn ingress_request_round_trips_with_raw_query() {
        let request = JsonEnvelope {
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            request: PluginRequest::IngressEvent {
                config: serde_json::json!({ "channel": "twilio_sms" }),
                state: Some(IngressState {
                    mode: IngressMode::Webhook,
                    status: "running".to_string(),
                    endpoint: Some("/twilio/sms".to_string()),
                    metadata: BTreeMap::from([("cursor".to_string(), "41".to_string())]),
                }),
                payload: IngressPayload {
                    endpoint_id: Some("channel-twilio-sms:/twilio/sms".to_string()),
                    method: "POST".to_string(),
                    path: "/twilio/sms".to_string(),
                    headers: BTreeMap::from([(
                        "X-Twilio-Signature".to_string(),
                        "signature".to_string(),
                    )]),
                    query: BTreeMap::from([("foo".to_string(), "bar".to_string())]),
                    raw_query: Some("foo=bar&baz=qux".to_string()),
                    body: "Body=hello".to_string(),
                    trust_verified: false,
                    received_at: Some("2026-04-12T00:00:00Z".to_string()),
                },
            },
        };

        let json = serde_json::to_string(&request).unwrap();
        let parsed: JsonEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, request);
    }

    #[test]
    fn ingress_request_defaults_missing_raw_query_to_none() {
        let json = serde_json::json!({
            "protocol_version": CHANNEL_PLUGIN_PROTOCOL_VERSION,
            "request": {
                "kind": "ingress_event",
                "config": { "channel": "webhook" },
                "payload": {
                    "method": "POST",
                    "path": "/hook",
                    "headers": {},
                    "query": {},
                    "body": "",
                    "trust_verified": true
                }
            }
        });

        let parsed: JsonEnvelope = serde_json::from_value(json).unwrap();
        let PluginRequest::IngressEvent { state, payload, .. } = parsed.request else {
            panic!("expected ingress_event request");
        };
        assert_eq!(state, None);
        assert_eq!(payload.raw_query, None);
    }

    #[test]
    fn start_ingress_request_round_trips_json() {
        let request = JsonEnvelope {
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            request: PluginRequest::StartIngress {
                config: serde_json::json!({ "channel": "telegram" }),
                state: Some(IngressState {
                    mode: IngressMode::Polling,
                    status: "running".to_string(),
                    endpoint: None,
                    metadata: BTreeMap::from([("cursor".to_string(), "41".to_string())]),
                }),
            },
        };

        let json = serde_json::to_string(&request).unwrap();
        let parsed: JsonEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, request);
    }

    #[test]
    fn poll_ingress_request_round_trips_json() {
        let request = JsonEnvelope {
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            request: PluginRequest::PollIngress {
                config: serde_json::json!({ "channel": "slack" }),
                state: Some(IngressState {
                    mode: IngressMode::Polling,
                    status: "running".to_string(),
                    endpoint: None,
                    metadata: BTreeMap::from([("cursor".to_string(), "42".to_string())]),
                }),
            },
        };

        let rpc = request_to_jsonrpc(RequestId::integer(13), &request).unwrap();
        let json = serde_json::to_string(&rpc).unwrap();
        let (id, parsed) =
            parse_jsonrpc_request::<serde_json::Value, serde_json::Value>(&json).unwrap();
        assert_eq!(id, RequestId::integer(13));
        assert_eq!(parsed, request);
    }

    #[test]
    fn polling_ingress_response_round_trips_json() {
        let response = PluginResponse::IngressEventsReceived {
            events: Vec::new(),
            callback_reply: None,
            state: Some(IngressState {
                mode: IngressMode::Polling,
                status: "running".to_string(),
                endpoint: None,
                metadata: BTreeMap::from([("next_update_id".to_string(), "42".to_string())]),
            }),
            poll_after_ms: Some(250),
        };

        let json = serde_json::to_string(&response).unwrap();
        let parsed: PluginResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, response);
    }

    #[test]
    fn event_notification_round_trips_jsonrpc() {
        let notification = PluginNotificationEnvelope {
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            notification: ChannelEventNotification {
                events: vec![InboundEventEnvelope {
                    event_id: "evt-1".to_string(),
                    platform: "signal".to_string(),
                    event_type: "message.received".to_string(),
                    received_at: "2026-04-12T00:00:00Z".to_string(),
                    conversation: InboundConversationRef {
                        id: "chat-1".to_string(),
                        kind: "private".to_string(),
                        thread_id: None,
                        parent_message_id: None,
                        workspace_id: None,
                        parent_conversation_id: None,
                    },
                    actor: InboundActor {
                        id: "user-1".to_string(),
                        display_name: Some("User".to_string()),
                        username: None,
                        is_bot: false,
                        metadata: BTreeMap::new(),
                    },
                    message: InboundMessage {
                        id: "msg-1".to_string(),
                        content: "hello".to_string(),
                        content_type: "text/plain".to_string(),
                        reply_to_message_id: None,
                        attachments: Vec::new(),
                        metadata: BTreeMap::new(),
                    },
                    account_id: None,
                    activation: None,
                    metadata: BTreeMap::new(),
                }],
                state: Some(IngressState {
                    mode: IngressMode::Polling,
                    status: "running".to_string(),
                    endpoint: None,
                    metadata: BTreeMap::from([("cursor".to_string(), "42".to_string())]),
                }),
                poll_after_ms: Some(250),
            },
        };

        let json = notification_to_jsonrpc(&notification).unwrap();
        let parsed = parse_jsonrpc_message(&json).unwrap();
        assert_eq!(parsed, PluginMessage::Notification(notification));
    }

    #[test]
    fn outbound_message_envelope_round_trips_json() {
        let envelope = OutboundMessageEnvelope {
            content: "reply text".to_string(),
            content_type: Some("text/plain".to_string()),
            attachments: vec![OutboundAttachment {
                name: "notes.txt".to_string(),
                mime_type: "text/plain".to_string(),
                data_base64: None,
                url: Some("https://example.com/notes.txt".to_string()),
                storage_key: None,
            }],
            metadata: BTreeMap::from([
                ("conversation_id".to_string(), "chat-123".to_string()),
                ("thread_id".to_string(), "7".to_string()),
            ]),
        };

        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: OutboundMessageEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, envelope);
    }

    #[test]
    fn inbound_attachment_omits_missing_url() {
        let attachment = InboundAttachment {
            id: Some("telegram-file-id".to_string()),
            kind: "image".to_string(),
            url: None,
            mime_type: Some("image/jpeg".to_string()),
            size_bytes: Some(2048),
            name: None,
            storage_key: Some("telegram:file:telegram-file-id".to_string()),
            extracted_text: None,
            extras: BTreeMap::from([("file_unique_id".to_string(), "unique-1".to_string())]),
        };

        let value = serde_json::to_value(&attachment).unwrap();
        assert!(value.get("url").is_none());

        let parsed: InboundAttachment = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, attachment);
    }

    #[test]
    fn attachment_source_round_trips_wire_name() {
        let value = serde_json::to_string(&AttachmentSource::DataBase64).unwrap();
        assert_eq!(value, "\"data_base64\"");

        let parsed: AttachmentSource = serde_json::from_str("\"storage_key\"").unwrap();
        assert_eq!(parsed, AttachmentSource::StorageKey);

        let unknown: AttachmentSource = serde_json::from_str("\"signed_url\"").unwrap();
        assert_eq!(unknown, AttachmentSource::Unknown);
    }

    #[test]
    fn plugin_error_builds_error_response() {
        let response = plugin_error("bad_request", "missing webhook token");
        assert_eq!(
            response,
            PluginResponse::Error {
                error: PluginErrorPayload {
                    code: "bad_request".to_string(),
                    message: "missing webhook token".to_string(),
                },
            }
        );
    }

    #[test]
    fn standard_jsonrpc_error_without_dispatch_payload_uses_named_code() {
        let json = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 13,
            "error": {
                "code": JSONRPC_METHOD_NOT_FOUND,
                "message": "unknown method"
            }
        });

        let (id, parsed) = parse_jsonrpc_response(&json.to_string()).unwrap();
        assert_eq!(id, RequestId::integer(13));
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

    #[test]
    fn inbound_event_without_provenance_fields_still_deserializes() {
        let json = serde_json::json!({
            "event_id": "evt-1",
            "platform": "telegram",
            "event_type": "message.received",
            "received_at": "2026-04-12T00:00:00Z",
            "conversation": {
                "id": "chat-1",
                "kind": "private",
                "thread_id": "7"
            },
            "actor": {
                "id": "user-1",
                "is_bot": false,
                "metadata": {}
            },
            "message": {
                "id": "msg-1",
                "content": "hello",
                "content_type": "text/plain",
                "attachments": [],
                "metadata": {}
            },
            "metadata": {}
        });

        let parsed: InboundEventEnvelope = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.activation, None);
        assert_eq!(parsed.conversation.workspace_id, None);
        assert_eq!(parsed.conversation.parent_conversation_id, None);
        assert_eq!(parsed.conversation.thread_id, Some("7".to_string()));
    }

    #[test]
    fn inbound_event_with_activation_round_trips_json() {
        let envelope = InboundEventEnvelope {
            event_id: "evt-2".to_string(),
            platform: "slack".to_string(),
            event_type: "message.received".to_string(),
            received_at: "2026-04-12T00:00:00Z".to_string(),
            conversation: InboundConversationRef {
                id: "thread-9".to_string(),
                kind: "channel".to_string(),
                thread_id: Some("thread-9".to_string()),
                parent_message_id: Some("msg-0".to_string()),
                workspace_id: Some("workspace-1".to_string()),
                parent_conversation_id: Some("channel-1".to_string()),
            },
            actor: InboundActor {
                id: "user-1".to_string(),
                display_name: Some("User".to_string()),
                username: Some("user".to_string()),
                is_bot: false,
                metadata: BTreeMap::new(),
            },
            message: InboundMessage {
                id: "msg-1".to_string(),
                content: "hello".to_string(),
                content_type: "text/plain".to_string(),
                reply_to_message_id: Some("msg-0".to_string()),
                attachments: Vec::new(),
                metadata: BTreeMap::new(),
            },
            account_id: Some("account-1".to_string()),
            activation: Some(InboundActivation {
                reason: InboundActivation::REASON_REPLY_TO_AGENT.to_string(),
                agent_account_id: Some("agent-1".to_string()),
                referenced_message_author_id: Some("agent-1".to_string()),
            }),
            metadata: BTreeMap::new(),
        };

        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: InboundEventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, envelope);

        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["activation"]["reason"], "reply_to_agent");
        assert_eq!(value["conversation"]["workspace_id"], "workspace-1");
        assert_eq!(value["conversation"]["parent_conversation_id"], "channel-1");
    }

    #[test]
    fn inbound_activation_omits_missing_optional_fields() {
        let activation = InboundActivation {
            reason: InboundActivation::REASON_DIRECT_MENTION.to_string(),
            agent_account_id: None,
            referenced_message_author_id: None,
        };

        let value = serde_json::to_value(&activation).unwrap();
        assert!(value.get("agent_account_id").is_none());
        assert!(value.get("referenced_message_author_id").is_none());

        let parsed: InboundActivation = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, activation);
    }

    #[test]
    fn channel_policy_without_scope_fields_still_deserializes() {
        let json = serde_json::json!({
            "owner_id": "owner-1",
            "allowed_sender_ids": ["user-1"],
            "allowed_conversation_ids": ["chat-1"],
            "dm_policy": "owner_only",
            "require_signature_validation": true,
            "allow_group_messages": false,
            "metadata": {}
        });

        let parsed: ChannelPolicy = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.allowed_conversation_ids, vec!["chat-1".to_string()]);
        assert!(parsed.allowed_workspace_ids.is_empty());
        assert!(parsed.allowed_outbound_conversation_ids.is_empty());
        assert_eq!(parsed.activation, None);
        assert_eq!(parsed.thread_policy, None);
        assert!(parsed.allowed_thread_ids.is_empty());
        assert!(parsed.allowed_dm_sender_ids.is_empty());
        assert_eq!(parsed.reply_delivery, None);
    }

    #[test]
    fn channel_policy_with_scope_fields_round_trips_json() {
        let policy = ChannelPolicy {
            owner_id: Some("owner-1".to_string()),
            allowed_sender_ids: vec!["user-1".to_string()],
            allowed_conversation_ids: vec!["channel-1".to_string()],
            allowed_workspace_ids: vec!["workspace-1".to_string()],
            allowed_outbound_conversation_ids: vec!["channel-1".to_string()],
            activation: Some(InboundActivation::REASON_DIRECT_MENTION.to_string()),
            thread_policy: Some("inherit_parent".to_string()),
            allowed_thread_ids: vec!["thread-1".to_string()],
            dm_policy: Some("owner_only".to_string()),
            allowed_dm_sender_ids: vec!["dm-user-1".to_string()],
            reply_delivery: Some("runtime_owned".to_string()),
            require_signature_validation: Some(true),
            allow_group_messages: Some(false),
            max_attachment_bytes: None,
            metadata: BTreeMap::new(),
        };

        let json = serde_json::to_string(&policy).unwrap();
        let parsed: ChannelPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, policy);
    }

    #[test]
    fn get_message_request_round_trips_jsonrpc() {
        let request = JsonEnvelope {
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            request: PluginRequest::GetMessage {
                config: serde_json::json!({ "bot_token_env": "TOKEN" }),
                reference: MessageRef {
                    conversation_id: "C0BJSQDLURY".to_string(),
                    message_id: "1787796588.437149".to_string(),
                    thread_id: Some("1787796500.000100".to_string()),
                    workspace_id: Some("T0AAA".to_string()),
                },
            },
        };

        let rpc = request_to_jsonrpc(RequestId::integer(21), &request).unwrap();
        let value: Value = serde_json::to_value(&rpc).unwrap();
        assert_eq!(value["method"], "channel.get_message");
        let json = serde_json::to_string(&rpc).unwrap();
        let (id, parsed) =
            parse_jsonrpc_request::<serde_json::Value, serde_json::Value>(&json).unwrap();
        assert_eq!(id, RequestId::integer(21));
        assert_eq!(parsed, request);
    }

    #[test]
    fn get_permalink_request_round_trips_jsonrpc() {
        let request = JsonEnvelope {
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            request: PluginRequest::GetPermalink {
                config: serde_json::json!({ "bot_token_env": "TOKEN" }),
                reference: MessageRef {
                    conversation_id: "C0BJSQDLURY".to_string(),
                    message_id: "1787796588.437149".to_string(),
                    thread_id: None,
                    workspace_id: None,
                },
            },
        };

        let rpc = request_to_jsonrpc(RequestId::integer(22), &request).unwrap();
        let value: Value = serde_json::to_value(&rpc).unwrap();
        assert_eq!(value["method"], "channel.get_permalink");
        let json = serde_json::to_string(&rpc).unwrap();
        let (id, parsed) =
            parse_jsonrpc_request::<serde_json::Value, serde_json::Value>(&json).unwrap();
        assert_eq!(id, RequestId::integer(22));
        assert_eq!(parsed, request);
    }

    #[test]
    fn message_fetched_response_round_trips_jsonrpc() {
        let response = PluginResponse::MessageFetched {
            message: FetchedMessage {
                reference: MessageRef {
                    conversation_id: "C0BJSQDLURY".to_string(),
                    message_id: "1787796588.437149".to_string(),
                    thread_id: None,
                    workspace_id: Some("T0AAA".to_string()),
                },
                content: "hello".to_string(),
                content_type: Some("text/plain".to_string()),
                author: Some(FetchedMessageAuthor {
                    id: "U0BOT".to_string(),
                    display_name: Some("Seren".to_string()),
                    username: None,
                    is_bot: true,
                }),
                permalink: Some(
                    "https://example.slack.com/archives/C0BJSQDLURY/p1787796588437149".to_string(),
                ),
                metadata: BTreeMap::new(),
            },
        };

        let json = response_to_jsonrpc(&RequestId::integer(23), &response).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["result"]["message"]["conversation_id"], "C0BJSQDLURY");
        assert_eq!(value["result"]["message"]["workspace_id"], "T0AAA");
        assert!(value["result"]["message"].get("reference").is_none());
        let (id, parsed) = parse_jsonrpc_response(&json).unwrap();
        assert_eq!(id, RequestId::integer(23));
        assert_eq!(parsed, response);
    }

    #[test]
    fn message_not_found_response_round_trips_jsonrpc() {
        let response = PluginResponse::MessageNotFound {
            reference: MessageRef {
                conversation_id: "C0BJSQDLURY".to_string(),
                message_id: "1787796588.437149".to_string(),
                thread_id: None,
                workspace_id: None,
            },
        };

        let json = response_to_jsonrpc(&RequestId::integer(24), &response).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["result"]["kind"], "message_not_found");
        let (id, parsed) = parse_jsonrpc_response(&json).unwrap();
        assert_eq!(id, RequestId::integer(24));
        assert_eq!(parsed, response);
    }

    #[test]
    fn permalink_resolved_response_round_trips_jsonrpc() {
        let response = PluginResponse::PermalinkResolved {
            permalink: MessagePermalink {
                reference: MessageRef {
                    conversation_id: "C0BJSQDLURY".to_string(),
                    message_id: "1787796588.437149".to_string(),
                    thread_id: Some("1787796500.000100".to_string()),
                    workspace_id: Some("T0AAA".to_string()),
                },
                url: "https://example.slack.com/archives/C0BJSQDLURY/p1787796588437149".to_string(),
            },
        };

        let json = response_to_jsonrpc(&RequestId::integer(25), &response).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value["result"]["permalink"]["thread_id"],
            "1787796500.000100"
        );
        assert_eq!(value["result"]["permalink"]["workspace_id"], "T0AAA");
        let (id, parsed) = parse_jsonrpc_response(&json).unwrap();
        assert_eq!(id, RequestId::integer(25));
        assert_eq!(parsed, response);
    }

    #[test]
    fn delivery_receipt_round_trips_typed_message_ref() {
        let receipt = DeliveryReceipt {
            message_id: "1787796588.437149".to_string(),
            conversation_id: "C0BJSQDLURY".to_string(),
            message_ref: Some(MessageRef {
                conversation_id: "C0BJSQDLURY".to_string(),
                message_id: "1787796588.437149".to_string(),
                thread_id: Some("1787796500.000100".to_string()),
                workspace_id: Some("T0AAA".to_string()),
            }),
            metadata: BTreeMap::from([("platform".to_string(), "slack".to_string())]),
        };

        let json = serde_json::to_string(&receipt).unwrap();
        let parsed: DeliveryReceipt = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, receipt);
    }

    #[test]
    fn delivery_receipt_omits_absent_message_ref() {
        let receipt = DeliveryReceipt {
            message_id: "webhook-1".to_string(),
            conversation_id: "incoming_webhook".to_string(),
            message_ref: None,
            metadata: BTreeMap::new(),
        };

        let value = serde_json::to_value(&receipt).unwrap();
        assert!(value.get("message_ref").is_none());

        let parsed: DeliveryReceipt = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, receipt);
    }

    #[test]
    fn legacy_delivery_receipt_without_message_ref_still_deserializes() {
        let json = serde_json::json!({
            "message_id": "1787796588.437149",
            "conversation_id": "C0BJSQDLURY",
            "metadata": {}
        });

        let parsed: DeliveryReceipt = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.message_ref, None);
    }
}
