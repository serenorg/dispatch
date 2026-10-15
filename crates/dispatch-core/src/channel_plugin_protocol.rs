pub use dispatch_channel_protocol::{
    AttachmentSource, CHANNEL_EVENT_METHOD, CHANNEL_HEARTBEAT_METHOD,
    CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelCapabilities, ChannelEventAcceptance,
    ChannelEventBatch, ChannelEventRequest, ChannelHeartbeat, ChannelIngressRecovery,
    ChannelIngressStorage, ChannelPolicy, ConfiguredChannel, DeliveryReceipt, HealthReport,
    InboundActor, InboundAttachment, InboundConversationRef, InboundEventEnvelope, InboundMessage,
    IngressCallbackReply, IngressMode, IngressPayload, IngressState, MAX_CHANNEL_BATCH_BYTES,
    MAX_CHANNEL_BATCH_EVENTS, MAX_CHANNEL_FRAME_BYTES, OutboundAttachment, OutboundMessageEnvelope,
    PluginErrorPayload, PluginInput, PluginMessage, PluginNotificationEnvelope, PluginRequest,
    PluginRequestEnvelope, PluginRequestId, PluginResponse, RuntimeStateSnapshot, StatusAcceptance,
    StatusFrame, StatusKind, ThreadingModel, canonical_channel_batch_bytes,
    canonical_channel_event_content_bytes, event_acceptance_to_jsonrpc, event_request_to_jsonrpc,
    heartbeat_to_jsonrpc, is_channel_batch_digest, parse_jsonrpc_message, parse_jsonrpc_request,
    parse_jsonrpc_response, parse_plugin_input, parse_tagged_channel_reply, plugin_error,
    request_method, request_to_jsonrpc, response_to_jsonrpc,
};
use serde_json::Value;

pub type ChannelPluginRequest = PluginRequest<Value, Value>;
pub type ChannelPluginRequestEnvelope = PluginRequestEnvelope<ChannelPluginRequest>;
pub type ChannelPluginResponse = PluginResponse;
