//! Provider plugin wire protocol for Dispatch (v0.4.0 draft).
//!
//! This crate intentionally keeps the substantive inference payloads
//! (`complete`, `stream`, `cancel`) as `serde_json::Value` while the protocol
//! is iterating. Envelopes, capabilities, configuration, health, and errors
//! are fully typed so plugins can rely on stable framing without locking in
//! the in-flux message shapes.
//!
//! See `docs/provider-plugin-protocol.md` in the Dispatch repository for the
//! normative specification.

pub use dispatch_plugin_rpc::{
    JSONRPC_APPLICATION_ERROR, JSONRPC_INTERNAL_ERROR, JSONRPC_INVALID_PARAMS,
    JSONRPC_INVALID_REQUEST, JSONRPC_METHOD_NOT_FOUND, JSONRPC_PARSE_ERROR, JsonRpcErrorObject,
    JsonRpcErrorResponse, JsonRpcMessage, JsonRpcMessageError, JsonRpcNotification, JsonRpcRequest,
    JsonRpcSuccessResponse, RequestId, ensure_jsonrpc_version, standard_error_code_name,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const PROVIDER_PLUGIN_PROTOCOL_VERSION: u32 = 1;
pub const PROVIDER_EVENT_NOTIFICATION_METHOD: &str = "provider.event";

pub const METHOD_CAPABILITIES: &str = "provider.capabilities";
pub const METHOD_CONFIGURE: &str = "provider.configure";
pub const METHOD_HEALTH: &str = "provider.health";
pub const METHOD_COMPLETE: &str = "provider.complete";
pub const METHOD_STREAM: &str = "provider.stream";
pub const METHOD_CANCEL: &str = "provider.cancel";
pub const METHOD_SHUTDOWN: &str = "provider.shutdown";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginRequestEnvelope {
    pub protocol_version: u32,
    pub request: PluginRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginRequest {
    Capabilities,
    Configure {
        config: Value,
    },
    Health {
        config: Value,
    },
    /// Non-streaming completion. Payload shape is held in `params` while the
    /// v0.4.0 spec stabilizes; see `docs/provider-plugin-protocol.md`.
    Complete {
        params: Value,
    },
    /// Streaming completion. Same deferral as `Complete`.
    Stream {
        params: Value,
    },
    /// Best-effort cancellation of an in-flight stream by request id.
    Cancel {
        id: RequestId,
    },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginResponse {
    Capabilities {
        capabilities: ProviderCapabilities,
    },
    Configured {
        configuration: ProviderConfiguration,
    },
    Health {
        health: ProviderHealth,
    },
    Event {
        event: Value,
    },
    /// Terminal completion result. Typed as `Value` while the `complete` /
    /// `stream` payload shapes iterate.
    Completion {
        response: Value,
    },
    Ok,
    Error {
        error: PluginErrorPayload,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginErrorPayload {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderCapabilities {
    pub provider_id: String,
    pub protocol_version: u32,
    #[serde(default)]
    pub models: Vec<ProviderModel>,
    #[serde(default)]
    pub supports_streaming: bool,
    #[serde(default)]
    pub supports_tool_use: bool,
    #[serde(default)]
    pub supports_system_prompt: bool,
    #[serde(default)]
    pub supports_vision: bool,
    #[serde(default)]
    pub supports_prompt_caching: bool,
    /// Space for provider-specific capability extensions that have not yet
    /// been promoted to first-class fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modalities: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderConfiguration {
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderHealth {
    pub reachable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Value>,
}

/// Reserved structured error codes. See the protocol doc for semantics.
pub mod error_codes {
    pub const UNSUPPORTED_MODEL: &str = "unsupported_model";
    pub const UNSUPPORTED_MODALITY: &str = "unsupported_modality";
    pub const CONTEXT_LENGTH_EXCEEDED: &str = "context_length_exceeded";
    pub const RATE_LIMITED: &str = "rate_limited";
    pub const AUTHENTICATION_FAILED: &str = "authentication_failed";
    pub const UPSTREAM_ERROR: &str = "upstream_error";
    pub const UNIMPLEMENTED: &str = "unimplemented";
}

pub fn plugin_error(code: &str, message: impl Into<String>) -> PluginResponse {
    PluginResponse::Error {
        error: PluginErrorPayload {
            code: code.to_string(),
            message: message.into(),
            details: None,
        },
    }
}

pub fn request_method(request: &PluginRequest) -> &'static str {
    match request {
        PluginRequest::Capabilities => METHOD_CAPABILITIES,
        PluginRequest::Configure { .. } => METHOD_CONFIGURE,
        PluginRequest::Health { .. } => METHOD_HEALTH,
        PluginRequest::Complete { .. } => METHOD_COMPLETE,
        PluginRequest::Stream { .. } => METHOD_STREAM,
        PluginRequest::Cancel { .. } => METHOD_CANCEL,
        PluginRequest::Shutdown => METHOD_SHUTDOWN,
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
            PROVIDER_EVENT_NOTIFICATION_METHOD,
            Some(event.clone()),
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
                    "failed to serialize provider response: {source}"
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
                JsonRpcMessageError::message(format!("invalid provider result payload: {source}"))
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
            if notification.method != PROVIDER_EVENT_NOTIFICATION_METHOD {
                return Err(JsonRpcMessageError::UnexpectedNotificationMethod(
                    notification.method,
                ));
            }
            Ok((
                None,
                PluginResponse::Event {
                    event: notification.params.ok_or(JsonRpcMessageError::message(
                        "missing provider event params",
                    ))?,
                },
            ))
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
        JsonRpcMessageError::message(format!("failed to serialize provider request: {source}"))
    })?;
    let Value::Object(ref mut object) = params else {
        return Err(JsonRpcMessageError::message(
            "provider request did not serialize to an object",
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
            JsonRpcMessageError::message(format!("invalid provider request params: {source}"))
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
        details: None,
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
    use serde_json::json;

    #[test]
    fn capabilities_round_trip() {
        let caps = ProviderCapabilities {
            provider_id: "seren-models".to_string(),
            protocol_version: PROVIDER_PLUGIN_PROTOCOL_VERSION,
            models: vec![ProviderModel {
                id: "*".to_string(),
                display_name: Some("Seren Models (wildcard)".to_string()),
                context_window: None,
                max_output_tokens: None,
                modalities: None,
            }],
            supports_streaming: false,
            supports_tool_use: false,
            supports_system_prompt: true,
            supports_vision: false,
            supports_prompt_caching: false,
            extensions: None,
        };
        let json = serde_json::to_value(&caps).unwrap();
        let parsed: ProviderCapabilities = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, caps);
    }

    #[test]
    fn request_to_jsonrpc_flattens_params() {
        let envelope = PluginRequestEnvelope {
            protocol_version: PROVIDER_PLUGIN_PROTOCOL_VERSION,
            request: PluginRequest::Configure { config: json!({}) },
        };
        let request = request_to_jsonrpc(RequestId::integer(1), &envelope).unwrap();
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["method"], METHOD_CONFIGURE);
        assert_eq!(value["params"]["kind"], "configure");
        assert_eq!(
            value["params"]["protocol_version"],
            json!(PROVIDER_PLUGIN_PROTOCOL_VERSION)
        );
        assert!(value["params"].get("request").is_none());
    }

    #[test]
    fn cancel_request_carries_request_id() {
        let request = PluginRequest::Cancel {
            id: RequestId::Integer(7),
        };
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["kind"], "cancel");
        assert_eq!(value["id"], json!(7));
    }

    #[test]
    fn parse_jsonrpc_request_round_trips_flattened_params() {
        let envelope = PluginRequestEnvelope {
            protocol_version: PROVIDER_PLUGIN_PROTOCOL_VERSION,
            request: PluginRequest::Health {
                config: json!({ "api_key": "seren_123" }),
            },
        };
        let request = request_to_jsonrpc(RequestId::integer(9), &envelope).unwrap();
        let line = serde_json::to_string(&request).unwrap();
        let (id, parsed) = parse_jsonrpc_request(&line).unwrap();
        assert_eq!(id, RequestId::integer(9));
        assert_eq!(parsed, envelope);
    }

    #[test]
    fn provider_events_round_trip_through_jsonrpc_helpers() {
        let line = response_to_jsonrpc(
            &RequestId::integer(7),
            &PluginResponse::Event {
                event: json!({ "kind": "content_delta", "index": 0, "delta": { "kind": "text", "text": "Hel" } }),
            },
        )
        .unwrap();
        let (id, response) = parse_jsonrpc_message(&line).unwrap();
        assert_eq!(id, None);
        assert_eq!(
            response,
            PluginResponse::Event {
                event: json!({ "kind": "content_delta", "index": 0, "delta": { "kind": "text", "text": "Hel" } }),
            }
        );
    }
}
