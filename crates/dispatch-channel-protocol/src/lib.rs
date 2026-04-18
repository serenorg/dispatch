use dispatch_plugin_rpc::{
    JSONRPC_APPLICATION_ERROR, JsonRpcErrorResponse, JsonRpcMessage, JsonRpcRequest,
    JsonRpcSuccessResponse, RequestId, ensure_jsonrpc_version,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const CHANNEL_PLUGIN_PROTOCOL_VERSION: u32 = 1;

pub use dispatch_plugin_rpc::{
    JsonRpcErrorObject, JsonRpcNotification, RequestId as PluginRequestId,
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
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginRequest<C, M> {
    Capabilities,
    Configure {
        config: C,
    },
    Health {
        config: C,
    },
    StartIngress {
        config: C,
    },
    StopIngress {
        config: C,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<IngressState>,
    },
    PollIngress {
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
    Status {
        config: C,
        update: StatusFrame,
    },
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
        PluginRequest::StartIngress { .. } => "channel.start_ingress",
        PluginRequest::StopIngress { .. } => "channel.stop_ingress",
        PluginRequest::PollIngress { .. } => "channel.poll_ingress",
        PluginRequest::IngressEvent { .. } => "channel.ingress_event",
        PluginRequest::Deliver { .. } => "channel.deliver",
        PluginRequest::Push { .. } => "channel.push",
        PluginRequest::Status { .. } => "channel.status",
    }
}

pub fn request_to_jsonrpc<C: Serialize, M: Serialize>(
    id: RequestId,
    envelope: &PluginRequestEnvelope<PluginRequest<C, M>>,
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

pub fn parse_jsonrpc_request<C: DeserializeOwned, M: DeserializeOwned>(
    line: &str,
) -> Result<(RequestId, PluginRequestEnvelope<PluginRequest<C, M>>), String> {
    let message: JsonRpcMessage = serde_json::from_str(line)
        .map_err(|source| format!("invalid JSON-RPC message: {source}"))?;
    let JsonRpcMessage::Request(request) = message else {
        return Err("expected JSON-RPC request".to_string());
    };
    ensure_jsonrpc_version(&request.jsonrpc)?;
    let params = request
        .params
        .ok_or_else(|| "missing JSON-RPC params".to_string())?;
    let envelope = decode_request_params::<C, M>(&request.method, params)?;
    Ok((request.id, envelope))
}

pub fn response_to_jsonrpc(id: &RequestId, response: &PluginResponse) -> Result<String, String> {
    let message = match response {
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
        .map_err(|source| format!("failed to serialize JSON-RPC response: {source}"))
}

pub fn parse_jsonrpc_response(line: &str) -> Result<PluginResponse, String> {
    let message: JsonRpcMessage = serde_json::from_str(line)
        .map_err(|source| format!("invalid JSON-RPC message: {source}"))?;
    match message {
        JsonRpcMessage::Response(response) => {
            ensure_jsonrpc_version(&response.jsonrpc)?;
            serde_json::from_value(response.result)
                .map_err(|source| format!("invalid plugin result payload: {source}"))
        }
        JsonRpcMessage::Error(error) => {
            ensure_jsonrpc_version(&error.jsonrpc)?;
            Ok(PluginResponse::Error {
                error: decode_dispatch_error(error),
            })
        }
        JsonRpcMessage::Request(_) => Err("expected JSON-RPC response, got request".to_string()),
        JsonRpcMessage::Notification(_) => {
            Err("expected JSON-RPC response, got notification".to_string())
        }
    }
}

fn request_params_with_version<C: Serialize, M: Serialize>(
    protocol_version: u32,
    request: &PluginRequest<C, M>,
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

fn decode_request_params<C: DeserializeOwned, M: DeserializeOwned>(
    method: &str,
    params: Value,
) -> Result<PluginRequestEnvelope<PluginRequest<C, M>>, String> {
    let Value::Object(mut object) = params else {
        return Err("JSON-RPC params must be an object".to_string());
    };
    let protocol_version = object
        .remove("protocol_version")
        .ok_or_else(|| "missing protocol_version in JSON-RPC params".to_string())?
        .as_u64()
        .ok_or_else(|| "protocol_version must be an unsigned integer".to_string())?
        as u32;
    let request: PluginRequest<C, M> = serde_json::from_value(Value::Object(object))
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
        let parsed = parse_jsonrpc_response(&json).unwrap();
        assert_eq!(parsed, response);
    }

    #[test]
    fn error_response_round_trips_jsonrpc() {
        let response = plugin_error("bad_request", "missing webhook token");

        let json = response_to_jsonrpc(&RequestId::integer(11), &response).unwrap();
        let parsed = parse_jsonrpc_response(&json).unwrap();
        assert_eq!(parsed, response);
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
    fn poll_ingress_request_round_trips_json() {
        let request = JsonEnvelope {
            protocol_version: CHANNEL_PLUGIN_PROTOCOL_VERSION,
            request: PluginRequest::PollIngress {
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
}
