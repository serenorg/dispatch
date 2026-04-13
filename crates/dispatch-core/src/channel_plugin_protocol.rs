use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const CHANNEL_PLUGIN_PROTOCOL_VERSION: u32 = 1;

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

/// Well-known status frame kinds for the Dispatch channel protocol.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StatusKind {
    /// Agent is working on a response.
    Processing,
    /// Agent finished and the response has been (or will be) delivered.
    Completed,
    /// Agent processing was cancelled or timed out.
    Cancelled,
    /// A host-visible operation started (extension invocation, courier handoff).
    OperationStarted,
    /// A host-visible operation finished.
    OperationFinished,
    /// An action requires operator/user approval before proceeding.
    ApprovalNeeded,
    /// Informational status text to relay to the conversation.
    Info,
    /// Channel is actively delivering a message to the platform.
    Delivering,
    /// An extension requires the end-user to authenticate.
    AuthRequired,
    /// A newer peer sent a status kind this build does not recognize yet.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelPluginRequestEnvelope {
    pub protocol_version: u32,
    pub request: ChannelPluginRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChannelPluginRequest {
    Capabilities,
    Configure {
        config: Value,
    },
    Health {
        config: Value,
    },
    StartIngress {
        config: Value,
    },
    StopIngress {
        config: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<IngressState>,
    },
    PollIngress {
        config: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<IngressState>,
    },
    IngressEvent {
        config: Value,
        payload: IngressPayload,
    },
    Deliver {
        config: Value,
        message: Value,
    },
    Push {
        config: Value,
        message: Value,
    },
    Status {
        config: Value,
        update: StatusFrame,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChannelPluginResponse {
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
    StatusAccepted {
        status: StatusAcceptance,
    },
    Error {
        error: PluginErrorPayload,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginErrorPayload {
    pub code: String,
    pub message: String,
}

/// How a channel organizes conversations and replies.
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

/// How a channel receives inbound events from the platform.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum IngressMode {
    Webhook,
    EventsWebhook,
    InteractionWebhook,
    Polling,
    #[serde(other)]
    Unknown,
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
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
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

pub(crate) fn parse_tagged_channel_reply(reply_text: &str) -> Option<OutboundMessageEnvelope> {
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
    #[serde(default)]
    pub allowed_conversation_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dm_policy: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn request_round_trips_json() {
        let request = ChannelPluginRequestEnvelope {
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            request: ChannelPluginRequest::Status {
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

        let json = serde_json::to_string(&request).unwrap();
        let parsed: ChannelPluginRequestEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, request);
    }

    #[test]
    fn response_round_trips_json() {
        let response = ChannelPluginResponse::Capabilities {
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

        let json = serde_json::to_string(&response).unwrap();
        let parsed: ChannelPluginResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, response);
    }

    #[test]
    fn ingress_request_round_trips_with_raw_query() {
        let request = ChannelPluginRequestEnvelope {
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            request: ChannelPluginRequest::IngressEvent {
                config: serde_json::json!({ "channel": "twilio_sms" }),
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
        let parsed: ChannelPluginRequestEnvelope = serde_json::from_str(&json).unwrap();
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

        let parsed: ChannelPluginRequestEnvelope = serde_json::from_value(json).unwrap();
        let ChannelPluginRequest::IngressEvent { payload, .. } = parsed.request else {
            panic!("expected ingress_event request");
        };
        assert_eq!(payload.raw_query, None);
    }

    #[test]
    fn poll_ingress_request_round_trips_json() {
        let request = ChannelPluginRequestEnvelope {
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            request: ChannelPluginRequest::PollIngress {
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
        let parsed: ChannelPluginRequestEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, request);
    }

    #[test]
    fn polling_ingress_response_round_trips_json() {
        let response = ChannelPluginResponse::IngressEventsReceived {
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
        let parsed: ChannelPluginResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, response);
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
        let value = serde_json::to_string(&AttachmentSource::DataBase64).expect("serialize enum");
        assert_eq!(value, "\"data_base64\"");

        let parsed: AttachmentSource =
            serde_json::from_str("\"storage_key\"").expect("deserialize enum");
        assert_eq!(parsed, AttachmentSource::StorageKey);

        let unknown: AttachmentSource =
            serde_json::from_str("\"signed_url\"").expect("deserialize enum");
        assert_eq!(unknown, AttachmentSource::Unknown);
    }
}
