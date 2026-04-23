//! Database plugin wire protocol for Dispatch (v0.4.0 draft).
//!
//! This crate keeps substantive operation payloads (`execute`, `describe`,
//! session shapes) as `serde_json::Value` while the protocol iterates.
//! Envelopes, capabilities, configuration, health, and errors are fully
//! typed.
//!
//! See `docs/database-plugin-protocol.md` in the Dispatch repository for
//! the normative specification.

pub use dispatch_plugin_rpc::{
    JSONRPC_APPLICATION_ERROR, JSONRPC_INTERNAL_ERROR, JSONRPC_INVALID_PARAMS,
    JSONRPC_INVALID_REQUEST, JSONRPC_METHOD_NOT_FOUND, JSONRPC_PARSE_ERROR, JsonRpcErrorObject,
    JsonRpcErrorResponse, JsonRpcMessage, JsonRpcMessageError, JsonRpcNotification, JsonRpcRequest,
    JsonRpcSuccessResponse, RequestId, ensure_jsonrpc_version, standard_error_code_name,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const DATABASE_PLUGIN_PROTOCOL_VERSION: u32 = 1;
pub const DATABASE_EVENT_NOTIFICATION_METHOD: &str = "database.event";

pub const METHOD_CAPABILITIES: &str = "database.capabilities";
pub const METHOD_CONFIGURE: &str = "database.configure";
pub const METHOD_HEALTH: &str = "database.health";
pub const METHOD_DESCRIBE: &str = "database.describe";
pub const METHOD_OPEN_SESSION: &str = "database.open_session";
pub const METHOD_CLOSE_SESSION: &str = "database.close_session";
pub const METHOD_EXECUTE: &str = "database.execute";
pub const METHOD_SHUTDOWN: &str = "database.shutdown";

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
    /// Schema or collection introspection. Engine-specific payload.
    Describe {
        params: Value,
    },
    /// Open a logical connection / transaction. Returns a `session_opened`.
    OpenSession {
        params: Value,
    },
    CloseSession {
        session_id: String,
    },
    /// Execute one typed operation. See the database protocol doc for
    /// per-engine operation shapes.
    Execute {
        params: Value,
    },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginResponse {
    Capabilities {
        capabilities: DatabaseCapabilities,
    },
    Configured {
        configuration: DatabaseConfiguration,
    },
    Health {
        health: DatabaseHealth,
    },
    Event {
        event: Value,
    },
    Schema {
        schema: Option<Value>,
    },
    SessionOpened {
        session: DatabaseSession,
    },
    Result {
        result: Value,
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
pub struct DatabaseCapabilities {
    pub database_id: String,
    pub protocol_version: u32,
    /// Free-form engine identifier: `postgres`, `mongodb`, `neon`, `supabase`,
    /// `mysql`, `sqlite`, etc. Agents route operations to database plugins by
    /// matching on this string.
    pub engine: String,
    #[serde(default)]
    pub operations: Vec<String>,
    #[serde(default)]
    pub supports_transactions: bool,
    #[serde(default)]
    pub supports_streaming_rows: bool,
    #[serde(default)]
    pub supports_schema_introspection: bool,
    #[serde(default)]
    pub auth_modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DatabaseConfiguration {
    pub database_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DatabaseHealth {
    pub reachable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabaseSession {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_ms: Option<u64>,
}

/// Reserved structured error codes. See the protocol doc for semantics.
pub mod error_codes {
    pub const INVALID_STATEMENT: &str = "invalid_statement";
    pub const UNSUPPORTED_OPERATION: &str = "unsupported_operation";
    pub const PERMISSION_DENIED: &str = "permission_denied";
    pub const NOT_FOUND: &str = "not_found";
    pub const CONFLICT: &str = "conflict";
    pub const TIMEOUT: &str = "timeout";
    pub const RESULT_TOO_LARGE: &str = "result_too_large";
    pub const UPSTREAM_ERROR: &str = "upstream_error";
    pub const UNIMPLEMENTED: &str = "unimplemented";
    pub const AUTHENTICATION_FAILED: &str = "authentication_failed";
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
        PluginRequest::Describe { .. } => METHOD_DESCRIBE,
        PluginRequest::OpenSession { .. } => METHOD_OPEN_SESSION,
        PluginRequest::CloseSession { .. } => METHOD_CLOSE_SESSION,
        PluginRequest::Execute { .. } => METHOD_EXECUTE,
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
            DATABASE_EVENT_NOTIFICATION_METHOD,
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
                    "failed to serialize database response: {source}"
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
                JsonRpcMessageError::message(format!("invalid database result payload: {source}"))
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
            if notification.method != DATABASE_EVENT_NOTIFICATION_METHOD {
                return Err(JsonRpcMessageError::UnexpectedNotificationMethod(
                    notification.method,
                ));
            }
            Ok((
                None,
                PluginResponse::Event {
                    event: notification.params.ok_or(JsonRpcMessageError::message(
                        "missing database event params",
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
        JsonRpcMessageError::message(format!("failed to serialize database request: {source}"))
    })?;
    let Value::Object(ref mut object) = params else {
        return Err(JsonRpcMessageError::message(
            "database request did not serialize to an object",
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
            JsonRpcMessageError::message(format!("invalid database request params: {source}"))
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
        let caps = DatabaseCapabilities {
            database_id: "seren-db".to_string(),
            protocol_version: DATABASE_PLUGIN_PROTOCOL_VERSION,
            engine: "postgres".to_string(),
            operations: vec![
                "query".to_string(),
                "execute".to_string(),
                "describe".to_string(),
            ],
            supports_transactions: true,
            supports_streaming_rows: false,
            supports_schema_introspection: true,
            auth_modes: vec!["bearer".to_string()],
            extensions: None,
        };
        let value = serde_json::to_value(&caps).unwrap();
        assert_eq!(value["engine"], "postgres");
        let parsed: DatabaseCapabilities = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, caps);
    }

    #[test]
    fn request_to_jsonrpc_flattens_params() {
        let envelope = PluginRequestEnvelope {
            protocol_version: DATABASE_PLUGIN_PROTOCOL_VERSION,
            request: PluginRequest::OpenSession { params: json!({}) },
        };
        let request = request_to_jsonrpc(RequestId::integer(1), &envelope).unwrap();
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["method"], METHOD_OPEN_SESSION);
        assert_eq!(value["params"]["kind"], "open_session");
        assert_eq!(
            value["params"]["protocol_version"],
            json!(DATABASE_PLUGIN_PROTOCOL_VERSION)
        );
        assert!(value["params"].get("request").is_none());
    }

    #[test]
    fn session_opened_response_round_trips() {
        let response = PluginResponse::SessionOpened {
            session: DatabaseSession {
                id: "sess_1".to_string(),
                expires_in_ms: Some(60_000),
            },
        };
        let value = serde_json::to_value(&response).unwrap();
        let parsed: PluginResponse = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, response);
    }

    #[test]
    fn parse_jsonrpc_request_round_trips_flattened_params() {
        let envelope = PluginRequestEnvelope {
            protocol_version: DATABASE_PLUGIN_PROTOCOL_VERSION,
            request: PluginRequest::Execute {
                params: json!({ "operation": { "kind": "sql_query" } }),
            },
        };
        let request = request_to_jsonrpc(RequestId::integer(9), &envelope).unwrap();
        let line = serde_json::to_string(&request).unwrap();
        let (id, parsed) = parse_jsonrpc_request(&line).unwrap();
        assert_eq!(id, RequestId::integer(9));
        assert_eq!(parsed, envelope);
    }

    #[test]
    fn database_events_round_trip_through_jsonrpc_helpers() {
        let line = response_to_jsonrpc(
            &RequestId::integer(7),
            &PluginResponse::Event {
                event: json!({ "kind": "row_batch", "columns": [{ "name": "id" }], "rows": [[1]] }),
            },
        )
        .unwrap();
        let (id, response) = parse_jsonrpc_message(&line).unwrap();
        assert_eq!(id, None);
        assert_eq!(
            response,
            PluginResponse::Event {
                event: json!({ "kind": "row_batch", "columns": [{ "name": "id" }], "rows": [[1]] }),
            }
        );
    }
}
