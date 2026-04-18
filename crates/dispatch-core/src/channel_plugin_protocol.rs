pub use dispatch_channel_protocol::{
    AttachmentSource, CHANNEL_EVENT_NOTIFICATION_METHOD, CHANNEL_PLUGIN_PROTOCOL_VERSION,
    ChannelCapabilities, ChannelEventNotification, ChannelPolicy, ConfiguredChannel,
    DeliveryReceipt, HealthReport, InboundActor, InboundAttachment, InboundConversationRef,
    InboundEventEnvelope, InboundMessage, IngressCallbackReply, IngressMode, IngressPayload,
    IngressState, OutboundAttachment, OutboundMessageEnvelope, PluginErrorPayload, PluginMessage,
    PluginNotificationEnvelope, PluginRequest, PluginRequestEnvelope, PluginRequestId,
    PluginResponse, RuntimeStateSnapshot, StatusAcceptance, StatusFrame, StatusKind,
    ThreadingModel, notification_to_jsonrpc, parse_jsonrpc_message, parse_jsonrpc_request,
    parse_jsonrpc_response, parse_tagged_channel_reply, plugin_error, request_method,
    request_to_jsonrpc, response_to_jsonrpc,
};
use serde_json::Value;

pub type ChannelPluginRequest = PluginRequest<Value, Value>;
pub type ChannelPluginRequestEnvelope = PluginRequestEnvelope<ChannelPluginRequest>;
pub type ChannelPluginResponse = PluginResponse;
