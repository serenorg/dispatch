use serde::{Deserialize, Serialize};

pub const JSONRPC_VERSION: &str = "2.0";

// Standard JSON-RPC 2.0 pre-defined error codes (per the spec, section 5.1).
// Use `JSONRPC_APPLICATION_ERROR` and lower (down to -32099) for
// implementation-defined server errors, where the `data` field carries the
// specific Dispatch error payload.
pub const JSONRPC_PARSE_ERROR: i64 = -32700;
pub const JSONRPC_INVALID_REQUEST: i64 = -32600;
pub const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
pub const JSONRPC_INVALID_PARAMS: i64 = -32602;
pub const JSONRPC_INTERNAL_ERROR: i64 = -32603;
pub const JSONRPC_APPLICATION_ERROR: i64 = -32000;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    String(String),
    Integer(i64),
}

impl RequestId {
    pub fn integer(value: i64) -> Self {
        Self::Integer(value)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcSuccessResponse {
    pub jsonrpc: String,
    pub id: RequestId,
    pub result: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcErrorResponse {
    pub jsonrpc: String,
    // JSON-RPC 2.0 (section 5.1) requires the `id` member on an error response.
    // It MUST be Null when the request id could not be detected (parse error,
    // invalid request shape, etc.). Keep the field always serialized so
    // `None` renders as `"id": null`, not an omitted field.
    #[serde(default)]
    pub id: Option<RequestId>,
    pub error: JsonRpcErrorObject,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Notification(JsonRpcNotification),
    Response(JsonRpcSuccessResponse),
    Error(JsonRpcErrorResponse),
}

impl JsonRpcRequest {
    pub fn new(
        id: RequestId,
        method: impl Into<String>,
        params: Option<serde_json::Value>,
    ) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            method: method.into(),
            params,
        }
    }
}

impl JsonRpcNotification {
    pub fn new(method: impl Into<String>, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params,
        }
    }
}

impl JsonRpcSuccessResponse {
    pub fn new(id: RequestId, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result,
        }
    }
}

impl JsonRpcErrorResponse {
    pub fn new(
        id: Option<RequestId>,
        code: i64,
        message: impl Into<String>,
        data: Option<serde_json::Value>,
    ) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            error: JsonRpcErrorObject {
                code,
                message: message.into(),
                data,
            },
        }
    }
}

pub fn ensure_jsonrpc_version(version: &str) -> Result<(), String> {
    if version == JSONRPC_VERSION {
        Ok(())
    } else {
        Err(format!(
            "expected jsonrpc version {JSONRPC_VERSION}, got {version}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_json() {
        let request = JsonRpcRequest::new(
            RequestId::integer(1),
            "channel.capabilities",
            Some(serde_json::json!({ "protocol_version": 1, "kind": "capabilities" })),
        );

        let json = serde_json::to_string(&request).unwrap();
        let parsed: JsonRpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, request);
    }

    #[test]
    fn notification_round_trips_json() {
        let notification = JsonRpcNotification::new(
            "courier.event",
            Some(serde_json::json!({ "kind": "event" })),
        );

        let json = serde_json::to_string(&notification).unwrap();
        let parsed: JsonRpcNotification = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, notification);
    }

    #[test]
    fn error_round_trips_json() {
        let response = JsonRpcErrorResponse::new(
            Some(RequestId::integer(9)),
            JSONRPC_APPLICATION_ERROR,
            "failed",
            Some(serde_json::json!({ "dispatch_error": { "code": "bad_request" } })),
        );

        let json = serde_json::to_string(&response).unwrap();
        let parsed: JsonRpcErrorResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, response);
    }

    #[test]
    fn error_response_with_unknown_id_serializes_as_null() {
        let response =
            JsonRpcErrorResponse::new(None, JSONRPC_PARSE_ERROR, "could not parse request", None);

        let json = serde_json::to_string(&response).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        // JSON-RPC 2.0 requires the `id` field on an error response to be
        // present; it is Null when the incoming request id could not be
        // detected.
        assert_eq!(value.get("id"), Some(&serde_json::Value::Null));

        let parsed: JsonRpcErrorResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, response);
    }
}
