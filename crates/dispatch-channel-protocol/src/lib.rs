use dispatch_plugin_rpc::{
    JSONRPC_APPLICATION_ERROR, JSONRPC_INTERNAL_ERROR, JSONRPC_INVALID_PARAMS,
    JSONRPC_INVALID_REQUEST, JSONRPC_METHOD_NOT_FOUND, JSONRPC_PARSE_ERROR, JsonRpcErrorResponse,
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcSuccessResponse, RequestId,
    ensure_jsonrpc_version, standard_error_code_name,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const CHANNEL_PLUGIN_PROTOCOL_VERSION: u32 = 2;
pub const CHANNEL_EVENT_METHOD: &str = "channel.event";
pub const CHANNEL_HEARTBEAT_METHOD: &str = "channel.heartbeat";
pub const MAX_CHANNEL_BATCH_EVENTS: usize = 32;
pub const MAX_CHANNEL_BATCH_BYTES: usize = 2 * 1024 * 1024;
const MAX_CHANNEL_EVENT_ID_BYTES: usize = 512;
pub const MAX_CHANNEL_FRAME_BYTES: usize =
    MAX_CHANNEL_BATCH_BYTES + 2 * MAX_CHANNEL_BATCH_EVENTS * MAX_CHANNEL_EVENT_ID_BYTES + 4 * 1024;

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

/// Host-owned storage location for the plugin's pending ingress journal.
///
/// The host derives this path from its own storage scope. Provider
/// configuration never supplies it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChannelIngressStorage {
    pub journal_path: String,
    /// Current authenticated host head, never provider-selected recovery data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<ChannelIngressRecovery>,
}

impl ChannelIngressStorage {
    pub fn validate(&self) -> Result<(), JsonRpcMessageError> {
        let path = std::path::Path::new(&self.journal_path);
        if self.journal_path.is_empty()
            || self.journal_path.len() > 4096
            || self.journal_path.chars().any(char::is_control)
            || !path.is_absolute()
            || path.file_name().is_none()
            || path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err(JsonRpcMessageError::message(
                "channel ingress journal must have an absolute host-owned path",
            ));
        }
        if let Some(recovery) = &self.recovery {
            recovery.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
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
        storage: ChannelIngressStorage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<IngressState>,
    },
    StartIngress {
        config: C,
        storage: ChannelIngressStorage,
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
        storage: ChannelIngressStorage,
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
    /// Fetch a delivered message by receipt-bound reference.
    ///
    /// The host must authorize the conversation before issuing this request.
    GetMessage {
        config: C,
        reference: MessageRef,
    },
    /// Resolve a message permalink by receipt-bound reference.
    ///
    /// The host must authorize the conversation before issuing this request.
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
    /// The message returned for the requested reference.
    MessageFetched {
        message: FetchedMessage,
    },
    /// No message exists for the requested reference.
    MessageNotFound {
        reference: MessageRef,
    },
    /// The permalink returned for the requested reference.
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
#[serde(deny_unknown_fields)]
pub struct ChannelEventBatch {
    #[serde(default)]
    pub events: Vec<InboundEventEnvelope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<IngressState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_after_ms: Option<u64>,
}

/// One immutable inbound batch sent by a plugin as an ID-bearing request.
///
/// The request ID is `channel:<batch_digest>`. The host answers with a
/// [`ChannelEventAcceptance`] only after it durably stores the complete batch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChannelEventRequest {
    pub protocol_version: u32,
    pub batch_digest: String,
    pub batch: ChannelEventBatch,
    /// Exact current-head receipt required for a newly constructed successor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_acceptance: Option<ChannelEventAcceptance>,
}

impl ChannelEventRequest {
    pub fn new(
        batch: ChannelEventBatch,
        previous_acceptance: Option<ChannelEventAcceptance>,
    ) -> Result<Self, JsonRpcMessageError> {
        let batch_digest = channel_batch_digest(&batch)?;
        let request = Self {
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            batch_digest,
            batch,
            previous_acceptance,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), JsonRpcMessageError> {
        if self.protocol_version != CHANNEL_PLUGIN_PROTOCOL_VERSION
            || self.batch_digest != channel_batch_digest(&self.batch)?
        {
            return Err(JsonRpcMessageError::message(
                "channel batch identity does not match its original envelope",
            ));
        }
        if let Some(previous) = &self.previous_acceptance {
            previous.validate()?;
            // A receipt at the final sequence can have no successor.
            if previous.sequence() == u64::MAX {
                return Err(JsonRpcMessageError::message(
                    "channel acceptance sequence is exhausted",
                ));
            }
        }
        validate_batch_event_ids(&self.event_ids())
    }

    pub fn request_id(&self) -> RequestId {
        channel_batch_request_id(&self.batch_digest)
    }

    pub fn event_ids(&self) -> Vec<String> {
        self.batch
            .events
            .iter()
            .map(|event| event.event_id.clone())
            .collect()
    }

    /// Expected receipt after checking the durable predecessor and committing
    /// the entire original. This calculation does not authorize acceptance.
    pub fn acceptance(&self) -> Result<ChannelEventAcceptance, JsonRpcMessageError> {
        self.validate()?;
        let sequence = match &self.previous_acceptance {
            Some(previous) => previous.sequence().checked_add(1).ok_or_else(|| {
                JsonRpcMessageError::message("channel acceptance sequence is exhausted")
            })?,
            None => 1,
        };
        Ok(ChannelEventAcceptance::Accepted {
            batch_digest: self.batch_digest.clone(),
            event_ids: self.event_ids(),
            sequence,
        })
    }
}

/// Authenticated host head used to restore the producer's accepted original.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChannelIngressRecovery {
    pub request: ChannelEventRequest,
    pub acceptance: ChannelEventAcceptance,
    /// Effective accepted cursor, including state inherited from a prior head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<IngressState>,
}

impl ChannelIngressRecovery {
    pub fn validate(&self) -> Result<(), JsonRpcMessageError> {
        self.request.validate()?;
        self.acceptance.validate()?;
        if !self.acceptance.matches(&self.request) {
            return Err(JsonRpcMessageError::message(
                "channel recovery receipt differs from its original request",
            ));
        }
        if self
            .request
            .batch
            .state
            .as_ref()
            .is_some_and(|state| self.state.as_ref() != Some(state))
        {
            return Err(JsonRpcMessageError::message(
                "channel recovery state differs from its accepted original",
            ));
        }
        Ok(())
    }
}

/// Exact durable-storage receipt for one [`ChannelEventRequest`].
///
/// A receipt confirms storage only. It does not confirm execution or delivery.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChannelEventAcceptance {
    Accepted {
        batch_digest: String,
        event_ids: Vec<String>,
        /// Host-owned occurrence sequence; first 1, then predecessor plus 1.
        sequence: u64,
    },
}

impl ChannelEventAcceptance {
    pub fn matches(&self, request: &ChannelEventRequest) -> bool {
        request.acceptance().is_ok_and(|expected| self == &expected)
    }

    pub fn validate(&self) -> Result<(), JsonRpcMessageError> {
        let Self::Accepted {
            batch_digest,
            event_ids,
            sequence,
        } = self;
        if !is_channel_batch_digest(batch_digest) || *sequence == 0 {
            return Err(JsonRpcMessageError::message(
                "channel acceptance identity is invalid",
            ));
        }
        validate_batch_event_ids(event_ids)
    }

    pub fn batch_digest(&self) -> &str {
        let Self::Accepted { batch_digest, .. } = self;
        batch_digest
    }

    pub fn sequence(&self) -> u64 {
        let Self::Accepted { sequence, .. } = self;
        *sequence
    }

    pub fn request_id(&self) -> RequestId {
        channel_batch_request_id(self.batch_digest())
    }
}

/// Liveness-only notification. It never carries cursor state or acceptance.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChannelHeartbeat {
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// One line read by a plugin from its stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginInput<C, M> {
    Request {
        id: RequestId,
        envelope: Box<PluginRequestEnvelope<PluginRequest<C, M>>>,
    },
    EventAcceptance {
        id: RequestId,
        acceptance: ChannelEventAcceptance,
    },
}

/// One line read by the host from plugin stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginMessage {
    Response {
        id: RequestId,
        response: PluginResponse,
    },
    EventRequest {
        id: RequestId,
        request: ChannelEventRequest,
    },
    Heartbeat(PluginNotificationEnvelope<ChannelHeartbeat>),
}

/// Lowercase 64-character hexadecimal SHA-256 digest.
pub fn is_channel_batch_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn channel_batch_request_id(batch_digest: &str) -> RequestId {
    RequestId::String(format!("channel:{batch_digest}"))
}

fn validate_batch_event_ids(ids: &[String]) -> Result<(), JsonRpcMessageError> {
    let mut distinct = BTreeSet::new();
    if ids.len() > MAX_CHANNEL_BATCH_EVENTS
        || ids.iter().any(|id| {
            id.is_empty()
                || id.trim() != id
                || id.len() > MAX_CHANNEL_EVENT_ID_BYTES
                || id.chars().any(char::is_control)
                || !distinct.insert(id)
        })
    {
        return Err(JsonRpcMessageError::message(
            "channel batch event identities are invalid",
        ));
    }
    Ok(())
}

/// Canonical batch bytes: compact serde_json output of the typed batch with
/// recursively sorted object keys, raw UTF-8, and array order preserved.
pub fn canonical_channel_batch_bytes(
    batch: &ChannelEventBatch,
) -> Result<Vec<u8>, JsonRpcMessageError> {
    if batch.events.len() > MAX_CHANNEL_BATCH_EVENTS {
        return Err(JsonRpcMessageError::message(
            "channel batch exceeds its event bound",
        ));
    }
    let value = serde_json::to_value(batch).map_err(JsonRpcMessageError::invalid_json)?;
    let bytes = serde_json::to_vec(&canonical_channel_value(value))
        .map_err(JsonRpcMessageError::invalid_json)?;
    if bytes.len() > MAX_CHANNEL_BATCH_BYTES {
        return Err(JsonRpcMessageError::message(
            "channel batch exceeds its byte bound",
        ));
    }
    Ok(bytes)
}

/// Canonical event content for retry identity. Only the top-level host
/// observation `received_at` is excluded; the full original frame stays exact.
pub fn canonical_channel_event_content_bytes(
    event: &InboundEventEnvelope,
) -> Result<Vec<u8>, JsonRpcMessageError> {
    let Value::Object(mut fields) =
        serde_json::to_value(event).map_err(JsonRpcMessageError::invalid_json)?
    else {
        return Err(JsonRpcMessageError::message(
            "channel event must be an object",
        ));
    };
    fields.remove("received_at");
    let bytes = serde_json::to_vec(&canonical_channel_value(Value::Object(fields)))
        .map_err(JsonRpcMessageError::invalid_json)?;
    if bytes.len() > MAX_CHANNEL_BATCH_BYTES {
        return Err(JsonRpcMessageError::message(
            "channel event content exceeds its byte bound",
        ));
    }
    Ok(bytes)
}

fn canonical_channel_value(value: Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (key, canonical_channel_value(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        Value::Array(array) => {
            Value::Array(array.into_iter().map(canonical_channel_value).collect())
        }
        value => value,
    }
}

fn channel_batch_digest(batch: &ChannelEventBatch) -> Result<String, JsonRpcMessageError> {
    let digest = Sha256::digest(canonical_channel_batch_bytes(batch)?);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
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
    /// Complete provider coordinates, flattened into the existing receipt fields.
    #[serde(flatten)]
    pub reference: MessageRef,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// Provider-neutral coordinates for one delivered message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageRef {
    /// Provider conversation or chat identifier.
    pub conversation_id: String,
    /// Provider message identifier.
    pub message_id: String,
    /// Provider thread parent identifier, when the provider requires it to
    /// address the message: a Slack thread timestamp, a Telegram topic id, or
    /// an email parent `Message-ID`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// A message returned for a receipt-bound reference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FetchedMessage {
    /// Complete provider coordinates, flattened into provider-neutral wire fields.
    #[serde(flatten)]
    pub reference: MessageRef,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<FetchedMessageAuthor>,
    /// Canonical URL for the fetched message, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permalink: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// Provider author identity, when available.
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
    /// Complete provider coordinates for the resolved permalink.
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

/// Parse one plugin stdin line: a host request or an exact event receipt.
pub fn parse_plugin_input<C: DeserializeOwned, M: DeserializeOwned>(
    line: &str,
) -> Result<PluginInput<C, M>, JsonRpcMessageError> {
    // Host requests keep their existing size and envelope contract; only
    // durable receipts are strict.
    let (message, keys) = parse_channel_frame(line)?;
    match message {
        JsonRpcMessage::Request(request) => {
            ensure_jsonrpc_version(&request.jsonrpc)?;
            let params = request.params.ok_or(JsonRpcMessageError::MissingParams)?;
            let envelope = decode_request_params(&request.method, params)?;
            Ok(PluginInput::Request {
                id: request.id,
                envelope: Box::new(envelope),
            })
        }
        JsonRpcMessage::Response(response) => {
            ensure_exact_frame_keys(&keys, &["jsonrpc", "id", "result"])?;
            ensure_jsonrpc_version(&response.jsonrpc)?;
            let acceptance: ChannelEventAcceptance = serde_json::from_value(response.result)
                .map_err(JsonRpcMessageError::invalid_json)?;
            acceptance.validate()?;
            if response.id != acceptance.request_id() {
                return Err(JsonRpcMessageError::message(
                    "channel acceptance response identity differs",
                ));
            }
            Ok(PluginInput::EventAcceptance {
                id: response.id,
                acceptance,
            })
        }
        JsonRpcMessage::Error(_) => Err(JsonRpcMessageError::message(
            "host refused channel durable acceptance",
        )),
        JsonRpcMessage::Notification(_) => Err(JsonRpcMessageError::ExpectedRequest),
    }
}

pub fn event_request_to_jsonrpc(
    request: &ChannelEventRequest,
) -> Result<String, JsonRpcMessageError> {
    request.validate()?;
    let params = serde_json::to_value(request).map_err(JsonRpcMessageError::invalid_json)?;
    let message = JsonRpcMessage::Request(JsonRpcRequest::new(
        request.request_id(),
        CHANNEL_EVENT_METHOD,
        Some(params),
    ));
    serde_json::to_string(&message).map_err(JsonRpcMessageError::invalid_json)
}

pub fn event_acceptance_to_jsonrpc(
    id: &RequestId,
    acceptance: &ChannelEventAcceptance,
) -> Result<String, JsonRpcMessageError> {
    acceptance.validate()?;
    if id != &acceptance.request_id() {
        return Err(JsonRpcMessageError::message(
            "channel acceptance response identity differs",
        ));
    }
    let result = serde_json::to_value(acceptance).map_err(JsonRpcMessageError::invalid_json)?;
    let message = JsonRpcMessage::Response(JsonRpcSuccessResponse::new(id.clone(), result));
    serde_json::to_string(&message).map_err(JsonRpcMessageError::invalid_json)
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
        PluginMessage::Heartbeat(_) => Err(JsonRpcMessageError::UnexpectedNotification),
        PluginMessage::EventRequest { .. } => Err(JsonRpcMessageError::UnexpectedRequest),
    }
}

pub fn heartbeat_to_jsonrpc(
    envelope: &PluginNotificationEnvelope<ChannelHeartbeat>,
) -> Result<String, JsonRpcMessageError> {
    let params = notification_params_with_version(envelope)?;
    let message = JsonRpcMessage::Notification(JsonRpcNotification::new(
        CHANNEL_HEARTBEAT_METHOD,
        Some(params),
    ));
    serde_json::to_string(&message).map_err(|source| {
        JsonRpcMessageError::message(format!(
            "failed to serialize JSON-RPC notification: {source}"
        ))
    })
}

pub fn parse_jsonrpc_message(line: &str) -> Result<PluginMessage, JsonRpcMessageError> {
    let (message, keys) = parse_channel_frame(line)?;
    match message {
        JsonRpcMessage::Response(response) => {
            ensure_jsonrpc_version(&response.jsonrpc)?;
            // Ordinary result extensions stay accepted, but batch data must
            // arrive through a `channel.event` request and its receipt.
            if response.result.get("kind").and_then(Value::as_str)
                == Some("ingress_events_received")
                && response.result.get("events").is_some()
            {
                return Err(JsonRpcMessageError::message(
                    "ingress control response contains inline batch events",
                ));
            }
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
            Ok(PluginMessage::Heartbeat(decode_notification_params(
                &notification.method,
                notification
                    .params
                    .ok_or(JsonRpcMessageError::MissingParams)?,
            )?))
        }
        JsonRpcMessage::Request(request) => {
            ensure_exact_frame_keys(&keys, &["jsonrpc", "id", "method", "params"])?;
            ensure_jsonrpc_version(&request.jsonrpc)?;
            // The complete line is already allocated here; this bound limits
            // what a host accepts as one durable batch frame.
            if line.len() > MAX_CHANNEL_FRAME_BYTES {
                return Err(JsonRpcMessageError::message(
                    "channel batch frame exceeds its byte bound",
                ));
            }
            if request.method != CHANNEL_EVENT_METHOD {
                return Err(JsonRpcMessageError::UnexpectedRequest);
            }
            let params = request.params.ok_or(JsonRpcMessageError::MissingParams)?;
            let event: ChannelEventRequest = serde_json::from_value(params.clone())
                .map_err(JsonRpcMessageError::invalid_json)?;
            // The original request preserves its typed shape, including the
            // predecessor, so retries cannot change absent fields into nulls.
            let typed_request =
                serde_json::to_value(&event).map_err(JsonRpcMessageError::invalid_json)?;
            if canonical_channel_value(params) != canonical_channel_value(typed_request) {
                return Err(JsonRpcMessageError::message(
                    "channel batch wire shape differs from its exact typed envelope",
                ));
            }
            event.validate()?;
            if request.id != event.request_id() {
                return Err(JsonRpcMessageError::message(
                    "channel batch request identity differs",
                ));
            }
            Ok(PluginMessage::EventRequest {
                id: request.id,
                request: event,
            })
        }
    }
}

fn parse_channel_frame(line: &str) -> Result<(JsonRpcMessage, Vec<String>), JsonRpcMessageError> {
    let value: Value = serde_json::from_str(line).map_err(JsonRpcMessageError::invalid_json)?;
    let keys = value
        .as_object()
        .ok_or_else(|| JsonRpcMessageError::invalid_json("channel frame must be a JSON object"))?
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let message: JsonRpcMessage =
        serde_json::from_value(value).map_err(JsonRpcMessageError::invalid_json)?;
    Ok((message, keys))
}

/// Durable batch requests and receipts carry no envelope extensions.
fn ensure_exact_frame_keys(keys: &[String], allowed: &[&str]) -> Result<(), JsonRpcMessageError> {
    if keys.iter().any(|key| !allowed.contains(&key.as_str())) {
        return Err(JsonRpcMessageError::message(
            "channel durable frame contains incompatible or unknown fields",
        ));
    }
    Ok(())
}

fn validate_ingress_storage<C, M>(
    request: &PluginRequest<C, M>,
) -> Result<(), JsonRpcMessageError> {
    match request {
        PluginRequest::StartIngress { storage, .. }
        | PluginRequest::PollIngress { storage, .. }
        | PluginRequest::IngressEvent { storage, .. } => storage.validate(),
        _ => Ok(()),
    }
}

fn request_params_with_version<C: Serialize, M: Serialize>(
    protocol_version: u32,
    request: &PluginRequest<C, M>,
) -> Result<Value, JsonRpcMessageError> {
    validate_ingress_storage(request)?;
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
    envelope: &PluginNotificationEnvelope<ChannelHeartbeat>,
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
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(JsonRpcMessageError::InvalidProtocolVersion)?;
    let request: PluginRequest<C, M> =
        serde_json::from_value(Value::Object(object)).map_err(|source| {
            JsonRpcMessageError::message(format!("invalid plugin request params: {source}"))
        })?;
    validate_ingress_storage(&request)?;
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
) -> Result<PluginNotificationEnvelope<ChannelHeartbeat>, JsonRpcMessageError> {
    if method != CHANNEL_HEARTBEAT_METHOD {
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
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(JsonRpcMessageError::InvalidProtocolVersion)?;
    if protocol_version != CHANNEL_PLUGIN_PROTOCOL_VERSION {
        return Err(JsonRpcMessageError::InvalidProtocolVersion);
    }
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

    fn empty_cursor_request(cursor: &str) -> ChannelEventRequest {
        ChannelEventRequest::new(
            ChannelEventBatch {
                events: vec![],
                state: Some(IngressState {
                    mode: IngressMode::Polling,
                    status: "running".into(),
                    endpoint: None,
                    metadata: BTreeMap::from([("cursor".into(), cursor.into())]),
                }),
                poll_after_ms: Some(100),
            },
            None,
        )
        .unwrap()
    }

    #[test]
    fn batch_digest_is_lowercase_sha256_of_canonical_bytes() {
        let empty = ChannelEventBatch {
            events: vec![],
            state: None,
            poll_after_ms: None,
        };
        assert_eq!(
            canonical_channel_batch_bytes(&empty).unwrap(),
            br#"{"events":[]}"#
        );
        assert_eq!(
            ChannelEventRequest::new(empty, None).unwrap().batch_digest,
            "24de1c4a19c43ad41b013f13dcd858c17b0daa7f33a53f19913e5b11366d1c2e"
        );
        let paced = ChannelEventRequest::new(
            ChannelEventBatch {
                events: vec![],
                state: None,
                poll_after_ms: Some(100),
            },
            None,
        )
        .unwrap();
        assert_eq!(
            paced.batch_digest,
            "371e1d003b2839e4c815bbba85786f31954405ae611b6948424e9f771c7086fd"
        );
        assert_eq!(
            paced.request_id(),
            RequestId::String(format!("channel:{}", paced.batch_digest))
        );
    }

    #[test]
    fn event_content_excludes_only_the_top_level_host_observation() {
        let value = serde_json::json!({
            "event_id":"event", "platform":"test", "event_type":"message.received",
            "received_at":"2026-10-14T00:00:00Z",
            "conversation":{"id":"conversation", "kind":"private", "thread_id":"thread",
                "parent_message_id":"parent", "workspace_id":"workspace", "parent_conversation_id":"root"},
            "actor":{"id":"author", "display_name":"name", "username":"user", "is_bot":false,
                "metadata":{"received_at":"actor time"}},
            "message":{"id":"message", "content":"hello", "content_type":"text/plain",
                "reply_to_message_id":"reply", "attachments":[{"id":"file", "kind":"image",
                    "url":"https://example.invalid/file", "mime_type":"image/png", "size_bytes":1,
                    "name":"image", "storage_key":"storage", "extracted_text":"text",
                    "extras":{"received_at":"attachment time"}}],
                "metadata":{"received_at":"message time"}},
            "account_id":"account", "activation":{"reason":"direct_message",
                "agent_account_id":"agent", "referenced_message_author_id":"agent"},
            "metadata":{"received_at":"metadata time"}
        });
        let original: InboundEventEnvelope = serde_json::from_value(value.clone()).unwrap();
        let expected = canonical_channel_event_content_bytes(&original).unwrap();
        let mut observed = original.clone();
        observed.received_at = "2026-10-15T00:00:00Z".into();
        assert_eq!(
            canonical_channel_event_content_bytes(&observed).unwrap(),
            expected
        );
        let batch = |event| ChannelEventBatch {
            events: vec![event],
            state: None,
            poll_after_ms: None,
        };
        assert_ne!(
            channel_batch_digest(&batch(original)).unwrap(),
            channel_batch_digest(&batch(observed)).unwrap()
        );
        for pointer in [
            "/event_id",
            "/platform",
            "/event_type",
            "/conversation/id",
            "/conversation/kind",
            "/conversation/thread_id",
            "/conversation/parent_message_id",
            "/conversation/workspace_id",
            "/conversation/parent_conversation_id",
            "/actor/id",
            "/actor/display_name",
            "/actor/username",
            "/actor/is_bot",
            "/actor/metadata/received_at",
            "/message/id",
            "/message/content",
            "/message/content_type",
            "/message/reply_to_message_id",
            "/message/attachments/0/id",
            "/message/attachments/0/kind",
            "/message/attachments/0/url",
            "/message/attachments/0/mime_type",
            "/message/attachments/0/size_bytes",
            "/message/attachments/0/name",
            "/message/attachments/0/storage_key",
            "/message/attachments/0/extracted_text",
            "/message/attachments/0/extras/received_at",
            "/message/metadata/received_at",
            "/account_id",
            "/activation/reason",
            "/activation/agent_account_id",
            "/activation/referenced_message_author_id",
            "/metadata/received_at",
        ] {
            let mut changed = value.clone();
            let field = changed.pointer_mut(pointer).unwrap();
            let replacement = if field.is_boolean() {
                Value::Bool(true)
            } else if field.is_number() {
                Value::from(2)
            } else {
                Value::String("changed".into())
            };
            *field = replacement;
            let event = serde_json::from_value(changed).unwrap();
            assert_ne!(
                canonical_channel_event_content_bytes(&event).unwrap(),
                expected,
                "{pointer} must remain in content identity"
            );
        }
    }

    #[test]
    fn empty_progress_batches_require_exact_receipts_and_heartbeat_has_no_state() {
        let original = empty_cursor_request("before");
        let next = empty_cursor_request("after");
        assert!(original.acceptance().unwrap().matches(&original));
        assert!(!original.acceptance().unwrap().matches(&next));
        let wire = event_request_to_jsonrpc(&original).unwrap();
        assert!(matches!(
            parse_jsonrpc_message(&wire).unwrap(),
            PluginMessage::EventRequest { .. }
        ));
        let heartbeat = heartbeat_to_jsonrpc(&PluginNotificationEnvelope {
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            notification: ChannelHeartbeat {
                metadata: BTreeMap::from([("status".into(), "alive".into())]),
            },
        })
        .unwrap();
        assert!(matches!(
            parse_jsonrpc_message(&heartbeat).unwrap(),
            PluginMessage::Heartbeat(_)
        ));
        let mut malformed: Value = serde_json::from_str(&heartbeat).unwrap();
        malformed["params"]["state"] = serde_json::to_value(&original.batch.state).unwrap();
        assert!(parse_jsonrpc_message(&malformed.to_string()).is_err());
        malformed["params"].as_object_mut().unwrap().remove("state");
        malformed["params"]["protocol_version"] = Value::from(1u64 + (1u64 << 32));
        assert!(parse_jsonrpc_message(&malformed.to_string()).is_err());
    }

    #[test]
    fn predecessor_is_part_of_the_original_frame_but_not_the_content_digest() {
        let first = empty_cursor_request("1");
        let bare = empty_cursor_request("2");
        let next = ChannelEventRequest::new(bare.batch.clone(), Some(first.acceptance().unwrap()))
            .unwrap();
        assert_eq!(next.batch_digest, bare.batch_digest);
        assert_ne!(next, bare);
        let wire = event_request_to_jsonrpc(&next).unwrap();
        assert!(
            matches!(parse_jsonrpc_message(&wire).unwrap(), PluginMessage::EventRequest { request, .. } if request == next)
        );
        let recovery = ChannelIngressRecovery {
            request: next.clone(),
            acceptance: next.acceptance().unwrap(),
            state: next.batch.state.clone(),
        };
        recovery.validate().unwrap();
        let mut wrong = recovery.clone();
        wrong.acceptance = first.acceptance().unwrap();
        assert!(wrong.validate().is_err());
        let mut wrong_state = recovery.clone();
        wrong_state.state = None;
        assert!(wrong_state.validate().is_err());
        let recurring =
            ChannelEventRequest::new(next.batch.clone(), Some(next.acceptance().unwrap())).unwrap();
        assert_eq!(recurring.batch_digest, next.batch_digest);
        assert_eq!(recurring.acceptance().unwrap().sequence(), 3);
        // Occurrences of identical content share a request ID, so only the
        // full sequence-bearing receipt identifies which one was accepted.
        assert_eq!(recurring.request_id(), next.request_id());
        assert_ne!(recurring.acceptance().unwrap(), next.acceptance().unwrap());
        assert!(!next.acceptance().unwrap().matches(&recurring));
        assert!(!recurring.acceptance().unwrap().matches(&next));
        let mut wrong_version = first;
        wrong_version.protocol_version = 1;
        assert!(wrong_version.validate().is_err());
        let mut null_predecessor: Value =
            serde_json::from_str(&event_request_to_jsonrpc(&bare).unwrap()).unwrap();
        null_predecessor["params"]["previous_acceptance"] = Value::Null;
        assert!(parse_jsonrpc_message(&null_predecessor.to_string()).is_err());
    }

    #[test]
    fn acceptance_sequences_are_positive_exact_and_fail_closed_at_overflow() {
        let first = empty_cursor_request("first");
        let receipt = first.acceptance().unwrap();
        assert_eq!(receipt.sequence(), 1);
        let next =
            ChannelEventRequest::new(empty_cursor_request("next").batch, Some(receipt.clone()))
                .unwrap();
        let next_receipt = next.acceptance().unwrap();
        assert_eq!(next_receipt.sequence(), 2);
        let mut wire: Value = serde_json::from_str(
            &event_acceptance_to_jsonrpc(&first.request_id(), &receipt).unwrap(),
        )
        .unwrap();
        for sequence in [Value::from(0), Value::from(-1), Value::from(1.5)] {
            wire["result"]["sequence"] = sequence;
            assert!(parse_plugin_input::<Value, Value>(&wire.to_string()).is_err());
        }
        wire["result"].as_object_mut().unwrap().remove("sequence");
        assert!(parse_plugin_input::<Value, Value>(&wire.to_string()).is_err());
        let mut wrong = next_receipt.clone();
        let ChannelEventAcceptance::Accepted { sequence, .. } = &mut wrong;
        *sequence = 3;
        assert!(!wrong.matches(&next));
        let mut last = receipt;
        let ChannelEventAcceptance::Accepted { sequence, .. } = &mut last;
        *sequence = u64::MAX - 1;
        let final_successor =
            ChannelEventRequest::new(next.batch.clone(), Some(last.clone())).unwrap();
        assert_eq!(final_successor.acceptance().unwrap().sequence(), u64::MAX);
        let ChannelEventAcceptance::Accepted { sequence, .. } = &mut last;
        *sequence = u64::MAX;
        assert!(ChannelEventRequest::new(next.batch.clone(), Some(last.clone())).is_err());
        // A decoded frame naming the final receipt is refused before any host work.
        let mut overflow = final_successor;
        overflow.previous_acceptance = Some(last);
        assert!(overflow.validate().is_err());
        assert!(overflow.acceptance().is_err());
    }

    #[test]
    fn strict_frames_reject_legacy_inline_events_and_ambiguous_receipts() {
        let request = empty_cursor_request("42");
        let receipt =
            event_acceptance_to_jsonrpc(&request.request_id(), &request.acceptance().unwrap())
                .unwrap();
        let mut mixed: Value = serde_json::from_str(&receipt).unwrap();
        mixed["method"] = Value::String("channel.health".into());
        assert!(parse_plugin_input::<Value, Value>(&mixed.to_string()).is_err());
        let mut wrong_id: Value = serde_json::from_str(&receipt).unwrap();
        wrong_id["id"] = Value::String("another-request".into());
        assert!(parse_plugin_input::<Value, Value>(&wrong_id.to_string()).is_err());
        let mut unknown: Value = serde_json::from_str(&receipt).unwrap();
        unknown["result"]["extra"] = Value::Bool(true);
        assert!(parse_plugin_input::<Value, Value>(&unknown.to_string()).is_err());
        let mut partial: Value = serde_json::from_str(&receipt).unwrap();
        partial["result"]["event_ids"] = serde_json::json!(["not-in-batch"]);
        let PluginInput::EventAcceptance { acceptance, .. } =
            parse_plugin_input::<Value, Value>(&partial.to_string()).unwrap()
        else {
            panic!("expected a receipt");
        };
        assert!(!acceptance.matches(&request));
        let control = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"kind": "ingress_events_received", "events": []}
        });
        assert!(parse_jsonrpc_response(&control.to_string()).is_err());
        let legacy = serde_json::json!({
            "jsonrpc": "2.0",
            "method": CHANNEL_EVENT_METHOD,
            "params": {"protocol_version": 1, "events": []}
        });
        assert!(matches!(
            parse_jsonrpc_message(&legacy.to_string()),
            Err(JsonRpcMessageError::UnexpectedNotificationMethod(_))
        ));
        let mut request_wire: Value =
            serde_json::from_str(&event_request_to_jsonrpc(&request).unwrap()).unwrap();
        request_wire["params"]["batch"]["state"]["unknown"] = Value::Bool(true);
        assert!(parse_jsonrpc_message(&request_wire.to_string()).is_err());
        let mut null_default: Value =
            serde_json::from_str(&event_request_to_jsonrpc(&request).unwrap()).unwrap();
        null_default["params"]["batch"]["state"]["endpoint"] = Value::Null;
        assert!(parse_jsonrpc_message(&null_default.to_string()).is_err());
        let mut wrong_request_id: Value =
            serde_json::from_str(&event_request_to_jsonrpc(&request).unwrap()).unwrap();
        wrong_request_id["id"] = Value::from(1);
        assert!(parse_jsonrpc_message(&wrong_request_id.to_string()).is_err());
    }

    #[test]
    fn ingress_storage_is_required_checked_and_canonical_digest_preserves_arrays() {
        let absent = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "channel.poll_ingress",
            "params": {"protocol_version": 1, "kind": "poll_ingress", "config": {}}
        });
        assert!(parse_plugin_input::<Value, Value>(&absent.to_string()).is_err());
        for path in [
            "relative.sqlite",
            "/tmp/../other.sqlite",
            "/",
            "/tmp/bad\nname",
        ] {
            let storage = ChannelIngressStorage {
                recovery: None,
                journal_path: path.into(),
            };
            assert!(storage.validate().is_err());
        }
        let good = ChannelIngressStorage {
            recovery: None,
            journal_path: "/tmp/owned/incoming.sqlite".into(),
        };
        good.validate().unwrap();
        let request = JsonRequest::PollIngress {
            config: Value::Null,
            storage: good,
            state: None,
        };
        let frame = request_to_jsonrpc(
            RequestId::Integer(9),
            &PluginRequestEnvelope {
                protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
                request,
            },
        )
        .unwrap();
        assert!(matches!(
            parse_plugin_input::<Value, Value>(&serde_json::to_string(&frame).unwrap()).unwrap(),
            PluginInput::Request { .. }
        ));
        let first = serde_json::json!({"z": [{"b": 2, "a": 1}, 3], "a": 0});
        let reordered = serde_json::json!({"a": 0, "z": [{"a": 1, "b": 2}, 3]});
        assert_eq!(
            canonical_channel_value(first.clone()),
            canonical_channel_value(reordered)
        );
        assert_ne!(
            canonical_channel_value(first),
            canonical_channel_value(serde_json::json!({"a": 0, "z": [3, {"a": 1, "b": 2}]}))
        );
        let mut too_large = empty_cursor_request("42").batch;
        too_large
            .state
            .as_mut()
            .unwrap()
            .metadata
            .insert("huge".into(), "x".repeat(MAX_CHANNEL_BATCH_BYTES));
        assert!(ChannelEventRequest::new(too_large, None).is_err());
    }

    #[test]
    fn batch_frame_bound_does_not_change_ordinary_requests_or_responses() {
        let deliver = JsonRequest::Deliver {
            config: Value::Null,
            message: serde_json::json!({"content": "x".repeat(MAX_CHANNEL_FRAME_BYTES)}),
        };
        let frame = serde_json::to_string(
            &request_to_jsonrpc(
                RequestId::Integer(3),
                &PluginRequestEnvelope {
                    protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
                    request: deliver,
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert!(frame.len() > MAX_CHANNEL_FRAME_BYTES);
        assert!(matches!(
            parse_plugin_input::<Value, Value>(&frame).unwrap(),
            PluginInput::Request { .. }
        ));
        let oversized_output = format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":{{"kind":"ok","pad":"{}"}}}}"#,
            "x".repeat(MAX_CHANNEL_FRAME_BYTES)
        );
        assert!(matches!(
            parse_jsonrpc_message(&oversized_output).unwrap(),
            PluginMessage::Response { .. }
        ));
        let request = empty_cursor_request("42");
        let mut oversized_batch: Value =
            serde_json::from_str(&event_request_to_jsonrpc(&request).unwrap()).unwrap();
        oversized_batch["params"]["batch"]["state"]["metadata"]["padding"] =
            Value::String("x".repeat(MAX_CHANNEL_FRAME_BYTES));
        assert!(parse_jsonrpc_message(&oversized_batch.to_string()).is_err());
    }

    #[test]
    fn ordinary_health_response_extensions_remain_accepted() {
        let wire = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"kind": "health", "health": {"ok": true, "status": "ready"}, "extension": "provider metadata"}
        });
        assert!(matches!(
            parse_jsonrpc_message(&wire.to_string()).unwrap(),
            PluginMessage::Response {
                response: PluginResponse::Health { .. },
                ..
            }
        ));

        // Ordinary JSON-RPC envelopes keep accepting top-level extensions.
        let mut envelope = wire.clone();
        envelope["trace"] = Value::String("provider-trace".into());
        assert!(matches!(
            parse_jsonrpc_message(&envelope.to_string()).unwrap(),
            PluginMessage::Response {
                response: PluginResponse::Health { .. },
                ..
            }
        ));
        let control = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "trace": "provider-trace",
            "result": {"kind": "ingress_events_received", "poll_after_ms": 5, "extension": true}
        });
        assert!(matches!(
            parse_jsonrpc_message(&control.to_string()).unwrap(),
            PluginMessage::Response {
                response: PluginResponse::IngressEventsReceived {
                    poll_after_ms: Some(5),
                    ..
                },
                ..
            }
        ));
        let mut host_request = serde_json::to_value(
            request_to_jsonrpc(
                RequestId::Integer(4),
                &PluginRequestEnvelope {
                    protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
                    request: JsonRequest::Health {
                        config: Value::Null,
                    },
                },
            )
            .unwrap(),
        )
        .unwrap();
        host_request["trace"] = Value::String("host-trace".into());
        assert!(matches!(
            parse_plugin_input::<Value, Value>(&host_request.to_string()).unwrap(),
            PluginInput::Request { .. }
        ));

        // Durable batch requests and receipts stay exact.
        let request = empty_cursor_request("7");
        let mut event: Value =
            serde_json::from_str(&event_request_to_jsonrpc(&request).unwrap()).unwrap();
        event["trace"] = Value::String("provider-trace".into());
        assert!(parse_jsonrpc_message(&event.to_string()).is_err());
        let mut receipt: Value = serde_json::from_str(
            &event_acceptance_to_jsonrpc(&request.request_id(), &request.acceptance().unwrap())
                .unwrap(),
        )
        .unwrap();
        receipt["trace"] = Value::String("host-trace".into());
        assert!(parse_plugin_input::<Value, Value>(&receipt.to_string()).is_err());
    }

    #[test]
    fn maximal_batch_and_escaped_predecessor_identities_fit_the_frame_bound() {
        let mut batch = empty_cursor_request("cursor").batch;
        batch
            .state
            .as_mut()
            .unwrap()
            .metadata
            .insert("padding".into(), String::new());
        let overhead = canonical_channel_batch_bytes(&batch).unwrap().len();
        batch.state.as_mut().unwrap().metadata.insert(
            "padding".into(),
            "x".repeat(MAX_CHANNEL_BATCH_BYTES - overhead),
        );
        assert_eq!(
            canonical_channel_batch_bytes(&batch).unwrap().len(),
            MAX_CHANNEL_BATCH_BYTES
        );
        let previous = ChannelEventAcceptance::Accepted {
            batch_digest: "1".repeat(64),
            event_ids: (0..MAX_CHANNEL_BATCH_EVENTS)
                .map(|index| format!("{index:02}{}", "\\".repeat(510)))
                .collect(),
            // The widest sequence that still admits a successor.
            sequence: u64::MAX - 1,
        };
        let request = ChannelEventRequest::new(batch, Some(previous)).unwrap();
        let wire = event_request_to_jsonrpc(&request).unwrap();
        assert!(wire.len() <= MAX_CHANNEL_FRAME_BYTES);
        assert!(
            matches!(parse_jsonrpc_message(&wire).unwrap(), PluginMessage::EventRequest { request: decoded, .. } if decoded == request)
        );
        let recovery = ChannelIngressRecovery {
            state: request.batch.state.clone(),
            acceptance: request.acceptance().unwrap(),
            request,
        };
        let start = PluginRequestEnvelope {
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            request: JsonRequest::StartIngress {
                config: Value::Null,
                storage: ChannelIngressStorage {
                    journal_path: "/tmp/owned/producer.sqlite".into(),
                    recovery: Some(recovery),
                },
                state: None,
            },
        };
        let host_wire =
            serde_json::to_string(&request_to_jsonrpc(RequestId::Integer(1), &start).unwrap())
                .unwrap();
        assert!(host_wire.len() > MAX_CHANNEL_FRAME_BYTES);
        assert!(matches!(
            parse_plugin_input::<Value, Value>(&host_wire).unwrap(),
            PluginInput::Request { .. }
        ));
    }
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
        let heartbeat = heartbeat_to_jsonrpc(&PluginNotificationEnvelope {
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            notification: ChannelHeartbeat::default(),
        })
        .unwrap();

        let error = parse_jsonrpc_response(&heartbeat).expect_err("expected notification to fail");
        assert!(
            error
                .to_string()
                .contains("expected JSON-RPC response, got notification")
        );

        let legacy_event = JsonRpcNotification::new(
            CHANNEL_EVENT_METHOD,
            Some(serde_json::json!({
                "protocol_version": CHANNEL_PLUGIN_PROTOCOL_VERSION,
                "events": [],
                "poll_after_ms": 25
            })),
        );
        let json = serde_json::to_string(&legacy_event).unwrap();
        let error = parse_jsonrpc_response(&json).expect_err("expected legacy event to fail");
        assert!(matches!(
            error,
            JsonRpcMessageError::UnexpectedNotificationMethod(_)
        ));
    }

    #[test]
    fn ingress_request_round_trips_with_raw_query() {
        let request = JsonEnvelope {
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            request: PluginRequest::IngressEvent {
                config: serde_json::json!({ "channel": "twilio_sms" }),
                storage: ChannelIngressStorage {
                    recovery: None,
                    journal_path: "/tmp/dispatch-ingress.sqlite".into(),
                },
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
                "storage": { "journal_path": "/tmp/dispatch-ingress.sqlite" },
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
                storage: ChannelIngressStorage {
                    recovery: None,
                    journal_path: "/tmp/dispatch-ingress.sqlite".into(),
                },
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
                storage: ChannelIngressStorage {
                    recovery: None,
                    journal_path: "/tmp/dispatch-ingress.sqlite".into(),
                },
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
    fn event_request_and_exact_durable_acceptance_round_trip_in_both_directions() {
        let batch = ChannelEventBatch {
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
        };
        let request = ChannelEventRequest::new(batch, None).unwrap();
        let json = event_request_to_jsonrpc(&request).unwrap();
        let parsed = parse_jsonrpc_message(&json).unwrap();
        assert_eq!(
            parsed,
            PluginMessage::EventRequest {
                id: request.request_id(),
                request: request.clone(),
            }
        );
        let acceptance = request.acceptance().unwrap();
        let wire = event_acceptance_to_jsonrpc(&request.request_id(), &acceptance).unwrap();
        assert_eq!(
            parse_plugin_input::<Value, Value>(&wire).unwrap(),
            PluginInput::EventAcceptance {
                id: request.request_id(),
                acceptance: acceptance.clone(),
            }
        );
        assert!(acceptance.matches(&request));

        let mut changed = request.clone();
        changed.batch.events[0].message.content = "different original content".into();
        assert!(changed.validate().is_err());
        assert!(!acceptance.matches(&changed));
        let changed = ChannelEventRequest::new(changed.batch, None).unwrap();
        assert_ne!(changed.request_id(), request.request_id());
        assert!(!acceptance.matches(&changed));

        let mut duplicate = request.batch.clone();
        duplicate.events.push(duplicate.events[0].clone());
        assert!(ChannelEventRequest::new(duplicate, None).is_err());

        let mut full = request.batch.clone();
        full.events = (0..MAX_CHANNEL_BATCH_EVENTS)
            .map(|index| {
                let mut event = request.batch.events[0].clone();
                event.event_id = format!("event-{index}");
                event
            })
            .collect();
        ChannelEventRequest::new(full.clone(), None).unwrap();
        let mut extra = request.batch.events[0].clone();
        extra.event_id = "extra".into();
        full.events.push(extra);
        assert!(ChannelEventRequest::new(full, None).is_err());
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
                    thread_id: Some("1787796500.000100".to_string()),
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
        assert_eq!(value["result"]["message"]["thread_id"], "1787796500.000100");
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
        assert!(value["result"]["permalink"].get("reference").is_none());
        let (id, parsed) = parse_jsonrpc_response(&json).unwrap();
        assert_eq!(id, RequestId::integer(25));
        assert_eq!(parsed, response);
    }

    #[test]
    fn delivery_receipt_round_trips_flattened_message_ref() {
        let receipt = DeliveryReceipt {
            reference: MessageRef {
                conversation_id: "C0BJSQDLURY".to_string(),
                message_id: "1787796588.437149".to_string(),
                thread_id: Some("1787796500.000100".to_string()),
            },
            metadata: BTreeMap::from([("platform".to_string(), "slack".to_string())]),
        };

        let value = serde_json::to_value(&receipt).unwrap();
        assert_eq!(value["conversation_id"], "C0BJSQDLURY");
        assert_eq!(value["message_id"], "1787796588.437149");
        assert_eq!(value["thread_id"], "1787796500.000100");
        assert!(value.get("reference").is_none());

        let parsed: DeliveryReceipt = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, receipt);
    }
}
