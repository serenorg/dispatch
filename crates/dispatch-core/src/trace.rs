use crate::CourierEvent;
use serde::{Deserialize, Serialize};

pub const DISPATCH_TRACE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DispatchTraceArtifact {
    pub version: u32,
    pub kind: String,
    pub parcel_digest: String,
    pub courier: String,
    pub dataset: Option<String>,
    pub case_name: String,
    pub packaged_path: String,
    pub entrypoint: String,
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    pub passed: bool,
    pub failures: Vec<String>,
    pub error: Option<String>,
    pub steps: Vec<DispatchTraceStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DispatchTraceStep {
    SessionOpen {
        session_id: Option<String>,
        status: String,
        error: Option<String>,
    },
    Operation {
        operation: String,
        input: Option<String>,
    },
    Event {
        event: CourierEvent,
    },
}
