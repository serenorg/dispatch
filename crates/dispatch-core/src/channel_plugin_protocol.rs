pub use dispatch_channel_protocol::{
    AttachmentSource, CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelCapabilities, ChannelPolicy,
    ConfiguredChannel, DeliveryReceipt, HealthReport, InboundActor, InboundAttachment,
    InboundConversationRef, InboundEventEnvelope, InboundMessage, IngressCallbackReply,
    IngressMode, IngressPayload, IngressState, OutboundAttachment, OutboundMessageEnvelope,
    PluginErrorPayload, PluginRequest, PluginRequestEnvelope, PluginResponse, RuntimeStateSnapshot,
    StatusAcceptance, StatusFrame, StatusKind, ThreadingModel, parse_tagged_channel_reply,
    plugin_error,
};
use serde_json::Value;

pub type ChannelPluginRequest = PluginRequest<Value, Value>;
pub type ChannelPluginRequestEnvelope = PluginRequestEnvelope<ChannelPluginRequest>;
pub type ChannelPluginResponse = PluginResponse;
