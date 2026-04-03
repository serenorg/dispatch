use crate::{
    CourierCapabilities, CourierEvent, CourierInspection, CourierOperation, CourierSession,
};
use serde::{Deserialize, Serialize};

pub const COURIER_PLUGIN_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginRequestEnvelope {
    pub protocol_version: u32,
    pub request: PluginRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginRequest {
    Capabilities,
    ValidateParcel {
        parcel_dir: String,
    },
    Inspect {
        parcel_dir: String,
    },
    OpenSession {
        parcel_dir: String,
    },
    Run {
        parcel_dir: String,
        session: CourierSession,
        operation: CourierOperation,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginResponse {
    Result {
        #[serde(default)]
        capabilities: Option<CourierCapabilities>,
        #[serde(default)]
        inspection: Option<CourierInspection>,
        #[serde(default)]
        session: Option<CourierSession>,
    },
    Event {
        event: CourierEvent,
    },
    Done {
        session: CourierSession,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConversationMessage, CourierKind};

    #[test]
    fn run_request_round_trips_json() {
        let request = PluginRequestEnvelope {
            protocol_version: COURIER_PLUGIN_PROTOCOL_VERSION,
            request: PluginRequest::Run {
                parcel_dir: "/tmp/demo".to_string(),
                session: CourierSession {
                    id: "session-1".to_string(),
                    parcel_digest: "digest".to_string(),
                    entrypoint: Some("chat".to_string()),
                    turn_count: 1,
                    history: vec![ConversationMessage {
                        role: "user".to_string(),
                        content: "hello".to_string(),
                    }],
                    backend_state: None,
                },
                operation: CourierOperation::Chat {
                    input: "hello".to_string(),
                },
            },
        };

        let json = serde_json::to_string(&request).unwrap();
        let parsed: PluginRequestEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, request);
    }

    #[test]
    fn response_round_trips_json() {
        let response = PluginResponse::Result {
            capabilities: Some(CourierCapabilities {
                courier_id: "demo".to_string(),
                kind: CourierKind::Custom,
                supports_chat: true,
                supports_job: false,
                supports_heartbeat: false,
                supports_local_tools: false,
                supports_mounts: Vec::new(),
            }),
            inspection: None,
            session: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        let parsed: PluginResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, response);
    }
}
