use crate::{
    channel_plugin_protocol::{OutboundMessageEnvelope, parse_tagged_channel_reply},
    manifest::{InstructionKind, ModelReference, NetworkRule, ParcelManifest, ToolConfig},
    plugin_protocol::{PluginRequest, PluginResponse},
    plugins::CourierPluginManifest,
};
pub use dispatch_courier_protocol::{
    A2aAuthConfig, A2aAuthScheme, A2aEndpointMode, ConversationMessage, CourierCapabilities,
    CourierEvent, CourierInspection, CourierKind, CourierOperation, CourierSession, LocalToolSpec,
    LocalToolTarget, LocalToolTransport, MountConfig, MountKind, ResolvedMount, ToolApprovalPolicy,
    ToolApprovalTargetKind, ToolInvocation, ToolRiskLevel, ToolRunResult,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    cell::RefCell,
    collections::BTreeMap,
    fs,
    future::Future,
    io::BufReader,
    path::{Path, PathBuf},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio},
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use wasmtime::{Config, Engine, component::Component};

mod a2a;
mod builtin_tools;
mod checkpoint_store;
mod host_turn;
mod memory_store;
mod model_backends;
mod model_request;
mod mounts;
mod parcel;
mod plugin_process;
mod session_store;
mod tool_exec;
mod validation;
mod wasm_support;

#[cfg(test)]
use self::a2a::execute_a2a_tool_with_env;
use self::builtin_tools::{execute_builtin_tool, handle_native_memory_command};
use self::host_turn::{execute_host_turn, format_heartbeat_payload, format_job_payload};
use self::memory_store::{memory_delete, memory_get, memory_list, memory_put};
#[cfg(test)]
use self::model_backends::{ClaudeCliBackend, clear_test_env_override, set_test_env_override};
#[cfg(all(test, unix))]
use self::model_backends::{
    CodexAppServerBackend, clear_test_claude_binary_override, clear_test_codex_binary_override,
    clear_test_plugin_binary_override,
};
#[cfg(test)]
use self::model_request::{
    build_model_request, configured_context_token_limit, configured_model_id_with,
    configured_tool_call_limit, configured_tool_output_limit,
};
use self::model_request::{
    build_model_requests, configured_tool_round_limit, normalize_local_tool_input,
    select_chat_backend, truncate_tool_output,
};
use self::mounts::{ensure_mounts_supported, resolve_builtin_mounts};
#[cfg(test)]
use self::parcel::run_local_tool_with_env;
pub use self::parcel::{
    collect_skill_allowed_tools, list_local_tools, list_native_builtin_tools, load_parcel,
    resolve_prompt_text, run_local_tool,
};
use self::plugin_process::{
    canonical_parcel_dir, describe_plugin_response, read_expected_plugin_response,
    read_plugin_run_completion, shutdown_persistent_plugin_process, spawn_plugin_response_reader,
    wait_for_plugin_exit, write_plugin_request,
};
use self::session_store::{ensure_session_sqlite, persist_session_mounts};
#[cfg(test)]
use self::tool_exec::configured_llm_timeout_ms;
use self::tool_exec::{
    apply_session_run_elapsed, build_builtin_tool_approval_request,
    build_local_tool_approval_request, check_tool_approval, denied_tool_run_result,
    effective_llm_timeout_ms, ensure_run_timeout_budget, execute_host_local_tool,
    execute_local_tool_in_docker, execute_local_tool_with_env, operation_counts_toward_run_budget,
    remaining_run_budget_with_literal, run_timeout_deadline,
};
use self::validation::{
    ensure_operation_matches_entrypoint, ensure_session_matches_parcel, resolve_manifest_path,
    validate_courier_reference,
};
use self::wasm_support::{
    BoundedLruCache, apply_wasm_turn_to_session, instantiate_wasm_guest, load_wasm_component,
    resolve_wasm_component_path, validate_wasm_component_metadata, wasm_component_cache_limit,
    wasm_events_to_courier_events, wasm_guest_session, wasm_operation,
};
use self::{
    model_backends::*,
    parcel::{builtin_memory_tool_description, resolve_local_tool, validate_required_secrets},
};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);
thread_local! {
    static A2A_OPERATOR_POLICY_OVERRIDES: RefCell<Option<A2aOperatorPolicyOverrides>> = const {
        RefCell::new(None)
    };
    static TOOL_APPROVAL_HANDLER: RefCell<Option<Rc<ToolApprovalHandler>>> = const {
        RefCell::new(None)
    };
}

type ToolApprovalHandler =
    dyn Fn(&ToolApprovalRequest) -> Result<ToolApprovalDecision, String> + 'static;

pub(crate) fn a2a_origin_for_trust(url: &url::Url) -> Option<String> {
    a2a::a2a_origin(url)
}

mod wasm_bindings {
    wasmtime::component::bindgen!({
        path: "../dispatch-wasm-abi/wit",
        world: "courier-guest",
    });
}

#[derive(Debug, Error)]
pub enum CourierError {
    #[error("failed to read `{path}`: {source}")]
    ReadFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse parcel manifest `{path}`: {source}")]
    ParseParcelManifest {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("parcel path `{path}` does not exist")]
    MissingParcelPath { path: String },
    #[error("tool `{tool}` is not a declared local tool in this parcel")]
    UnknownLocalTool { tool: String },
    #[error("tool `{tool}` points to missing packaged file `{path}`")]
    MissingToolFile { tool: String, path: String },
    #[error("A2A tool `{tool}` is missing a configured endpoint URL")]
    MissingA2aToolUrl { tool: String },
    #[error("A2A request for tool `{tool}` failed: {message}")]
    A2aToolRequest { tool: String, message: String },
    #[error("builtin tool `{tool}` received invalid input: {message}")]
    InvalidBuiltinToolInput { tool: String, message: String },
    #[error("tool `{tool}` schema `{path}` is invalid: {source}")]
    ParseToolSchema {
        tool: String,
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("tool `{tool}` schema `{path}` must have a JSON object root")]
    ToolSchemaShape { tool: String, path: String },
    #[error(
        "tool `{tool}` schema `{path}` hash mismatch: expected `{expected_sha256}`, got `{actual_sha256}`"
    )]
    ToolSchemaDigestMismatch {
        tool: String,
        path: String,
        expected_sha256: String,
        actual_sha256: String,
    },
    #[error(
        "parcel manifest `{path}` declares unsupported schema `{found}`; expected `{expected}`"
    )]
    UnsupportedParcelSchema {
        path: String,
        found: String,
        expected: String,
    },
    #[error(
        "parcel manifest `{path}` uses unsupported format_version `{found}`; supported version is `{supported}`"
    )]
    UnsupportedParcelFormatVersion {
        path: String,
        found: u32,
        supported: u32,
    },
    #[error("required secret `{name}` is not present in the environment or local secret store")]
    MissingSecret { name: String },
    #[error("failed to resolve secret `{name}`: {message}")]
    SecretLookup { name: String, message: String },
    #[error("tool `{tool}` requires APPROVAL confirm")]
    ApprovalRequired { tool: String },
    #[error("tool `{tool}` was denied by the approval handler")]
    ApprovalDenied { tool: String },
    #[error("tool `{tool}` approval failed: {message}")]
    ApprovalFailed { tool: String, message: String },
    #[error("parcel manifest `{path}` does not conform to the Dispatch parcel schema: {message}")]
    InvalidParcelSchema { path: String, message: String },
    #[error("courier `{courier}` does not support mount `{kind:?}` with driver `{driver}`")]
    UnsupportedMount {
        courier: String,
        kind: MountKind,
        driver: String,
    },
    #[error(
        "courier `{courier}` does not enforce NETWORK {action} {target}; NETWORK rules are parsed but not yet enforced by this courier"
    )]
    UnsupportedNetworkPolicy {
        courier: String,
        action: String,
        target: String,
    },
    #[error("failed to start tool `{tool}`: {source}")]
    SpawnTool {
        tool: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write tool input for `{tool}`: {source}")]
    WriteToolInput {
        tool: String,
        #[source]
        source: std::io::Error,
    },
    #[error("tool `{tool}` exceeded TIMEOUT TOOL `{timeout}`")]
    ToolTimedOut { tool: String, timeout: String },
    #[error("session `{session_id}` exceeded TIMEOUT RUN `{timeout}`")]
    RunTimedOut { session_id: String, timeout: String },
    #[error("failed to wait for tool `{tool}`: {source}")]
    WaitTool {
        tool: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to wait for courier plugin `{courier}`: {source}")]
    WaitPlugin {
        courier: String,
        #[source]
        source: std::io::Error,
    },
    #[error("courier `{courier}` cannot execute tool `{tool}` with command `{command}`")]
    UnsupportedToolRunner {
        courier: String,
        tool: String,
        command: String,
    },
    #[error("failed to start courier plugin `{courier}`: {source}")]
    SpawnPlugin {
        courier: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write courier plugin request for `{courier}`: {source}")]
    WritePluginRequest {
        courier: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read courier plugin response for `{courier}`: {source}")]
    ReadPluginResponse {
        courier: String,
        #[source]
        source: std::io::Error,
    },
    #[error("courier plugin `{courier}` returned invalid protocol data: {message}")]
    PluginProtocol { courier: String, message: String },
    #[error("courier plugin `{courier}` exited with status {status}: {stderr}")]
    PluginExit {
        courier: String,
        status: i32,
        stderr: String,
    },
    #[error(
        "courier plugin `{courier}` executable hash changed: expected `{expected_sha256}`, got `{actual_sha256}`"
    )]
    PluginExecutableChanged {
        courier: String,
        expected_sha256: String,
        actual_sha256: String,
    },
    #[error("operation `{operation}` is not supported by courier `{courier}`")]
    UnsupportedOperation { courier: String, operation: String },
    #[error(
        "courier `{courier}` cannot execute parcel target `{parcel_courier}`; supported references: {supported}"
    )]
    IncompatibleCourier {
        courier: String,
        parcel_courier: String,
        supported: String,
    },
    #[error("parcel `{parcel_digest}` does not declare a WASM component for courier `{courier}`")]
    MissingCourierComponent {
        courier: String,
        parcel_digest: String,
    },
    #[error("failed to compile WASM component `{path}` for courier `{courier}`: {source}")]
    CompileWasmComponent {
        courier: String,
        path: String,
        #[source]
        source: wasmtime::Error,
    },
    #[error("failed to initialize WASM engine for courier `{courier}`: {source}")]
    InitWasmEngine {
        courier: String,
        #[source]
        source: wasmtime::Error,
    },
    #[error("failed to instantiate WASM component `{path}` for courier `{courier}`: {source}")]
    InstantiateWasmComponent {
        courier: String,
        path: String,
        #[source]
        source: wasmtime::Error,
    },
    #[error("WASM guest for courier `{courier}` rejected the operation: {message}")]
    WasmGuest { courier: String, message: String },
    #[error("operation `{operation}` does not match parcel entrypoint `{entrypoint}`")]
    EntrypointMismatch {
        entrypoint: String,
        operation: String,
    },
    #[error(
        "session parcel digest `{session_parcel_digest}` does not match loaded parcel digest `{parcel_digest}`"
    )]
    SessionParcelMismatch {
        session_parcel_digest: String,
        parcel_digest: String,
    },
    #[error("model backend request failed: {0}")]
    ModelBackendRequest(String),
    #[error("model backend returned an unexpected response: {0}")]
    ModelBackendResponse(String),
    #[error("model requested {attempted} tool calls, exceeding the configured limit of {limit}")]
    ToolCallLimitExceeded { limit: u32, attempted: u32 },
    #[error(
        "parcel `{parcel_digest}` does not declare a usable memory mount for courier memory operations"
    )]
    MissingMemoryMount { parcel_digest: String },
    #[error(
        "parcel `{parcel_digest}` does not declare a usable session sqlite mount for checkpoint operations"
    )]
    MissingSessionMount { parcel_digest: String },
    #[error("failed to create directory `{path}`: {source}")]
    CreateDir {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write `{path}`: {source}")]
    WriteFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize courier session for sqlite persistence: {0}")]
    SerializeSession(String),
    #[error("failed to access sqlite mount `{path}` during `{operation}`: {source}")]
    SqliteMount {
        path: String,
        operation: &'static str,
        #[source]
        source: rusqlite::Error,
    },
    #[error("invalid timeout `{duration}` for scope `{scope}`")]
    InvalidTimeoutSpec { scope: String, duration: String },
}

impl CourierError {
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::ModelBackendRequest(_)
                | Self::ReadPluginResponse { .. }
                | Self::WritePluginRequest { .. }
                | Self::WaitPlugin { .. }
                | Self::SpawnPlugin { .. }
                | Self::SpawnTool { .. }
                | Self::WriteToolInput { .. }
                | Self::WaitTool { .. }
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LoadedParcel {
    pub parcel_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub config: ParcelManifest,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct A2aOperatorPolicyOverrides {
    pub allowed_origins: Option<String>,
    pub trust_policy: Option<String>,
}

pub fn with_a2a_operator_policy_overrides<T>(
    overrides: A2aOperatorPolicyOverrides,
    f: impl FnOnce() -> T,
) -> T {
    if overrides.allowed_origins.is_none() && overrides.trust_policy.is_none() {
        return f();
    }
    A2A_OPERATOR_POLICY_OVERRIDES.with(|slot| {
        let previous = slot.replace(Some(overrides));
        let result = f();
        slot.replace(previous);
        result
    })
}

pub fn with_tool_approval_handler<T>(
    handler: impl Fn(&ToolApprovalRequest) -> Result<ToolApprovalDecision, String> + 'static,
    f: impl FnOnce() -> T,
) -> T {
    TOOL_APPROVAL_HANDLER.with(|slot| {
        let previous = slot.replace(Some(Rc::new(handler)));
        let result = f();
        slot.replace(previous);
        result
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuiltinToolSpec {
    pub capability: String,
    pub approval: Option<ToolApprovalPolicy>,
    pub risk: Option<ToolRiskLevel>,
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalDecision {
    Approve,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolApprovalRequest {
    pub tool: String,
    pub kind: ToolApprovalTargetKind,
    pub command: String,
    pub args: Vec<String>,
    pub approval: ToolApprovalPolicy,
    pub risk: Option<ToolRiskLevel>,
    pub description: Option<String>,
    pub skill_source: Option<String>,
    pub input: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MountRequest {
    pub parcel_digest: String,
    pub spec: MountConfig,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CourierRequest {
    pub session: CourierSession,
    pub operation: CourierOperation,
}

#[derive(Debug, Clone, Serialize)]
pub struct CourierResponse {
    pub courier_id: String,
    pub session: CourierSession,
    pub events: Vec<CourierEvent>,
}

struct ChatTurnResult {
    reply: String,
    events: Vec<CourierEvent>,
    streamed_reply: bool,
    backend_state: Option<String>,
}

fn normalize_assistant_reply(reply_text: &str) -> (String, Option<OutboundMessageEnvelope>) {
    match parse_tagged_channel_reply(reply_text) {
        Some(message) => (message.content.clone(), Some(message)),
        None => (reply_text.to_string(), None),
    }
}

#[derive(Debug, Clone, Copy)]
enum NativeTurnMode {
    Chat,
    Job,
    Heartbeat,
}

#[derive(Clone, Copy)]
struct HostTurnContext<'a> {
    chat_backend_override: Option<&'a Arc<dyn ChatModelBackend>>,
    tool_runner: HostToolRunner<'a>,
    host_label: &'static str,
    run_deadline: Option<Instant>,
}

#[derive(Clone, Copy)]
enum HostToolRunner<'a> {
    Native,
    Docker(&'a DockerCourier),
}

struct WasmModelRequestInput {
    requested_model: Option<String>,
    instructions: String,
    messages: Vec<ConversationMessage>,
    tools: Vec<ModelToolDefinition>,
    tool_outputs: Vec<ModelToolOutput>,
    previous_response_id: Option<String>,
    run_deadline: Option<Instant>,
}

pub trait MountProvider: Send + Sync {
    fn resolve_mount(
        &self,
        request: &MountRequest,
    ) -> impl Future<Output = Result<ResolvedMount, CourierError>> + Send;
}

pub trait CourierBackend: Send + Sync {
    fn id(&self) -> &str;
    fn kind(&self) -> CourierKind;

    fn capabilities(
        &self,
    ) -> impl Future<Output = Result<CourierCapabilities, CourierError>> + Send;
    fn validate_parcel(
        &self,
        parcel: &LoadedParcel,
    ) -> impl Future<Output = Result<(), CourierError>> + Send;
    fn inspect(
        &self,
        parcel: &LoadedParcel,
    ) -> impl Future<Output = Result<CourierInspection, CourierError>> + Send;
    fn open_session(
        &self,
        parcel: &LoadedParcel,
    ) -> impl Future<Output = Result<CourierSession, CourierError>> + Send;
    fn run(
        &self,
        parcel: &LoadedParcel,
        request: CourierRequest,
    ) -> impl Future<Output = Result<CourierResponse, CourierError>> + Send;
}

pub trait ChatModelBackend: Send + Sync {
    fn id(&self) -> &str;
    fn supports_previous_response_id(&self) -> bool {
        false
    }
    fn generate(&self, request: &ModelRequest) -> Result<ModelGeneration, CourierError>;
    fn generate_with_events(
        &self,
        request: &ModelRequest,
        _on_event: &mut dyn FnMut(ModelStreamEvent),
    ) -> Result<ModelGeneration, CourierError> {
        self.generate(request)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModelRequest {
    pub model: String,
    pub provider: Option<String>,
    pub model_options: BTreeMap<String, String>,
    pub llm_timeout_ms: Option<u64>,
    pub context_token_limit: Option<u32>,
    pub tool_call_limit: Option<u32>,
    pub tool_output_limit: Option<usize>,
    pub working_directory: Option<String>,
    pub instructions: String,
    pub messages: Vec<ConversationMessage>,
    pub tools: Vec<ModelToolDefinition>,
    pub pending_tool_calls: Vec<ModelToolCall>,
    pub tool_outputs: Vec<ModelToolOutput>,
    pub previous_response_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModelReply {
    pub text: Option<String>,
    pub backend: String,
    pub response_id: Option<String>,
    pub tool_calls: Vec<ModelToolCall>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelGeneration {
    Reply(ModelReply),
    NotConfigured { backend: String, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStreamEvent {
    TextDelta { content: String },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModelToolDefinition {
    pub name: String,
    pub description: String,
    pub format: ModelToolFormat,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelToolFormat {
    Text,
    JsonSchema { schema: serde_json::Value },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModelToolCall {
    pub call_id: String,
    pub name: String,
    pub input: String,
    pub kind: ModelToolKind,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModelToolOutput {
    pub call_id: String,
    pub name: String,
    pub output: String,
    pub kind: ModelToolKind,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelToolKind {
    Custom,
    Function,
}

#[derive(Default)]
pub struct NativeCourier {
    chat_backend_override: Option<Arc<dyn ChatModelBackend>>,
}

#[derive(Debug, Clone)]
pub struct JsonlCourierPlugin {
    manifest: CourierPluginManifest,
    state: Arc<JsonlCourierPluginState>,
}

struct JsonlCourierPluginState {
    courier_name: String,
    protocol_version: u32,
    sessions: Mutex<BTreeMap<String, PersistentPluginProcess>>,
}

struct PersistentPluginProcess {
    child: Child,
    stdin: ChildStdin,
    stderr: ChildStderr,
    responses: mpsc::Receiver<Result<plugin_process::ParsedPluginResponse, CourierError>>,
}

#[derive(Clone)]
pub struct DockerCourier {
    docker_bin: PathBuf,
    helper_image: String,
    chat_backend_override: Option<Arc<dyn ChatModelBackend>>,
}

#[derive(Clone)]
pub struct WasmCourier {
    engine: Engine,
    chat_backend_override: Option<Arc<dyn ChatModelBackend>>,
    component_cache: Arc<Mutex<BoundedLruCache<Component>>>,
}

impl WasmCourier {
    pub fn new() -> Result<Self, CourierError> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).map_err(|source| CourierError::InitWasmEngine {
            courier: "wasm".to_string(),
            source,
        })?;
        Ok(Self {
            engine,
            chat_backend_override: None,
            component_cache: Arc::new(Mutex::new(BoundedLruCache::new(
                wasm_component_cache_limit(),
            ))),
        })
    }
}

#[derive(Debug, Clone)]
pub struct StubCourier {
    courier_id: &'static str,
    kind: CourierKind,
}

impl Default for DockerCourier {
    fn default() -> Self {
        Self {
            docker_bin: std::env::var_os("DISPATCH_DOCKER_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("docker")),
            helper_image: std::env::var("DISPATCH_DOCKER_IMAGE")
                .unwrap_or_else(|_| "python:3.13-alpine".to_string()),
            chat_backend_override: None,
        }
    }
}

impl JsonlCourierPlugin {
    pub fn new(manifest: CourierPluginManifest) -> Self {
        Self {
            state: Arc::new(JsonlCourierPluginState {
                courier_name: manifest.name.clone(),
                protocol_version: manifest.protocol_version,
                sessions: Mutex::new(BTreeMap::new()),
            }),
            manifest,
        }
    }
}

impl std::fmt::Debug for JsonlCourierPluginState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let session_count = self
            .sessions
            .lock()
            .map(|sessions| sessions.len())
            .unwrap_or(0);
        f.debug_struct("JsonlCourierPluginState")
            .field("session_count", &session_count)
            .finish()
    }
}

impl std::fmt::Debug for PersistentPluginProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersistentPluginProcess")
            .finish_non_exhaustive()
    }
}

impl Drop for JsonlCourierPluginState {
    fn drop(&mut self) {
        if let Ok(mut sessions) = self.sessions.lock() {
            for (_, mut process) in std::mem::take(&mut *sessions) {
                let _ = shutdown_persistent_plugin_process(
                    &mut process,
                    &self.courier_name,
                    self.protocol_version,
                );
            }
        }
    }
}

impl NativeCourier {
    pub fn with_chat_backend(chat_backend: Arc<dyn ChatModelBackend>) -> Self {
        Self {
            chat_backend_override: Some(chat_backend),
        }
    }
}

impl WasmCourier {
    pub fn with_chat_backend(chat_backend: Arc<dyn ChatModelBackend>) -> Self {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).expect("failed to initialize wasmtime engine");
        Self {
            engine,
            chat_backend_override: Some(chat_backend),
            component_cache: Arc::new(Mutex::new(BoundedLruCache::new(
                wasm_component_cache_limit(),
            ))),
        }
    }
}

impl DockerCourier {
    pub fn new(docker_bin: impl Into<PathBuf>, helper_image: impl Into<String>) -> Self {
        Self {
            docker_bin: docker_bin.into(),
            helper_image: helper_image.into(),
            chat_backend_override: None,
        }
    }

    pub fn with_chat_backend(mut self, chat_backend: Arc<dyn ChatModelBackend>) -> Self {
        self.chat_backend_override = Some(chat_backend);
        self
    }
}

impl StubCourier {
    pub fn wasm() -> Self {
        Self {
            courier_id: "wasm",
            kind: CourierKind::Wasm,
        }
    }
}

fn process_env_lookup(name: &str) -> Option<String> {
    if let Some(override_value) = a2a_operator_policy_override_value(name) {
        return Some(override_value);
    }
    std::env::var(name).ok()
}

fn a2a_operator_policy_override_value(name: &str) -> Option<String> {
    match name {
        "DISPATCH_A2A_ALLOWED_ORIGINS" => A2A_OPERATOR_POLICY_OVERRIDES.with(|slot| {
            slot.borrow()
                .as_ref()
                .and_then(|overrides| overrides.allowed_origins.clone())
        }),
        "DISPATCH_A2A_TRUST_POLICY" => A2A_OPERATOR_POLICY_OVERRIDES.with(|slot| {
            slot.borrow()
                .as_ref()
                .and_then(|overrides| overrides.trust_policy.clone())
        }),
        _ => None,
    }
}

pub(super) fn current_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(super) fn escape_sql_like_prefix(prefix: &str) -> String {
    let mut escaped = String::with_capacity(prefix.len() + 1);
    for ch in prefix.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped.push('%');
    escaped
}

impl CourierBackend for NativeCourier {
    fn id(&self) -> &str {
        "native"
    }

    fn kind(&self) -> CourierKind {
        CourierKind::Native
    }

    async fn capabilities(&self) -> Result<CourierCapabilities, CourierError> {
        Ok(CourierCapabilities {
            courier_id: self.id().to_string(),
            kind: self.kind(),
            supports_chat: true,
            supports_job: true,
            supports_heartbeat: true,
            supports_local_tools: true,
            supports_mounts: vec![MountKind::Session, MountKind::Memory, MountKind::Artifacts],
        })
    }

    fn validate_parcel(
        &self,
        parcel: &LoadedParcel,
    ) -> impl Future<Output = Result<(), CourierError>> + Send {
        let result = validate_native_parcel(parcel);
        async move { result }
    }

    fn inspect(
        &self,
        parcel: &LoadedParcel,
    ) -> impl Future<Output = Result<CourierInspection, CourierError>> + Send {
        let inspection = CourierInspection {
            courier_id: self.id().to_string(),
            kind: self.kind(),
            entrypoint: parcel.config.entrypoint.clone(),
            required_secrets: parcel
                .config
                .secrets
                .iter()
                .map(|secret| secret.name.clone())
                .collect(),
            mounts: parcel.config.mounts.clone(),
            local_tools: list_local_tools(parcel),
            extensions: None,
        };
        async move { Ok(inspection) }
    }

    fn open_session(
        &self,
        parcel: &LoadedParcel,
    ) -> impl Future<Output = Result<CourierSession, CourierError>> + Send {
        let validation = validate_native_parcel(parcel);
        let parcel_digest = parcel.config.digest.clone();
        let entrypoint = parcel.config.entrypoint.clone();
        async move {
            validation?;
            validate_required_secrets(parcel)?;
            let sequence = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
            let session_id = format!("native-{parcel_digest}-{sequence}");
            let session = CourierSession {
                resolved_mounts: resolve_builtin_mounts(parcel, "native", &session_id)?,
                id: session_id,
                parcel_digest,
                entrypoint,
                label: parcel.config.name.clone(),
                turn_count: 0,
                elapsed_ms: 0,
                history: Vec::new(),
                backend_state: None,
            };
            persist_session_mounts(&session)?;
            Ok(session)
        }
    }

    fn run(
        &self,
        parcel: &LoadedParcel,
        request: CourierRequest,
    ) -> impl Future<Output = Result<CourierResponse, CourierError>> + Send {
        let courier_id = self.id().to_string();
        let operation = request.operation;
        let mut session = request.session;
        let chat_backend_override = self.chat_backend_override.clone();

        async move {
            validate_native_parcel(parcel)?;
            ensure_session_matches_parcel(parcel, &session)?;
            ensure_operation_matches_entrypoint(&session, &operation)?;
            let consumes_run_budget = operation_counts_toward_run_budget(&operation);
            if consumes_run_budget {
                ensure_run_timeout_budget(&session, &parcel.config.timeouts)?;
            }
            let run_deadline = if consumes_run_budget {
                run_timeout_deadline(&session, &parcel.config.timeouts)?
            } else {
                None
            };
            session.turn_count += 1;
            let started_at = Instant::now();

            let mut response = match operation {
                CourierOperation::ResolvePrompt => Ok(CourierResponse {
                    courier_id,
                    session: {
                        persist_session_mounts(&session)?;
                        session
                    },
                    events: vec![
                        CourierEvent::PromptResolved {
                            text: resolve_prompt_text(parcel)?,
                        },
                        CourierEvent::Done,
                    ],
                }),
                CourierOperation::ListLocalTools => Ok(CourierResponse {
                    courier_id,
                    session: {
                        persist_session_mounts(&session)?;
                        session
                    },
                    events: vec![
                        CourierEvent::LocalToolsListed {
                            tools: list_local_tools(parcel),
                        },
                        CourierEvent::Done,
                    ],
                }),
                CourierOperation::InvokeTool { invocation } => {
                    let tool = resolve_local_tool(parcel, &invocation.name)?;
                    if let Some(request) =
                        build_local_tool_approval_request(&tool, invocation.input.as_deref())
                        && !check_tool_approval(&request)?
                    {
                        return Err(CourierError::ApprovalDenied { tool: request.tool });
                    }
                    let result = execute_host_local_tool(
                        parcel,
                        &tool,
                        invocation.input.as_deref(),
                        HostToolRunner::Native,
                        run_deadline,
                    )?;

                    Ok(CourierResponse {
                        courier_id,
                        session: {
                            persist_session_mounts(&session)?;
                            session
                        },
                        events: vec![
                            CourierEvent::ToolCallStarted {
                                invocation,
                                command: result.command.clone(),
                                args: result.args.clone(),
                            },
                            CourierEvent::ToolCallFinished { result },
                            CourierEvent::Done,
                        ],
                    })
                }
                CourierOperation::Chat { input } => {
                    session.history.push(ConversationMessage {
                        role: "user".to_string(),
                        content: input.clone(),
                    });
                    let mut chat_turn = execute_host_turn(
                        parcel,
                        &session,
                        &input,
                        NativeTurnMode::Chat,
                        HostTurnContext {
                            chat_backend_override: chat_backend_override.as_ref(),
                            tool_runner: HostToolRunner::Native,
                            host_label: "Native",
                            run_deadline,
                        },
                    )?;
                    let (assistant_history_content, channel_reply) =
                        normalize_assistant_reply(&chat_turn.reply);
                    session.history.push(ConversationMessage {
                        role: "assistant".to_string(),
                        content: assistant_history_content,
                    });
                    session.backend_state = chat_turn.backend_state.clone();
                    if !chat_turn.streamed_reply {
                        if let Some(message) = channel_reply {
                            chat_turn
                                .events
                                .push(CourierEvent::ChannelReply { message });
                        } else {
                            chat_turn.events.push(CourierEvent::Message {
                                role: "assistant".to_string(),
                                content: chat_turn.reply.clone(),
                            });
                        }
                    }
                    chat_turn.events.push(CourierEvent::Done);

                    persist_session_mounts(&session)?;
                    Ok(CourierResponse {
                        courier_id,
                        session,
                        events: chat_turn.events,
                    })
                }
                CourierOperation::Job { payload } => run_host_task_operation(
                    parcel,
                    session,
                    NativeTurnMode::Job,
                    format_job_payload(&payload),
                    HostTurnContext {
                        chat_backend_override: chat_backend_override.as_ref(),
                        tool_runner: HostToolRunner::Native,
                        host_label: "Native",
                        run_deadline,
                    },
                ),
                CourierOperation::Heartbeat { payload } => run_host_task_operation(
                    parcel,
                    session,
                    NativeTurnMode::Heartbeat,
                    format_heartbeat_payload(payload.as_deref()),
                    HostTurnContext {
                        chat_backend_override: chat_backend_override.as_ref(),
                        tool_runner: HostToolRunner::Native,
                        host_label: "Native",
                        run_deadline,
                    },
                ),
            }?;

            if consumes_run_budget {
                apply_session_run_elapsed(&mut response.session, started_at);
            }
            persist_session_mounts(&response.session)?;
            Ok(response)
        }
    }
}

impl CourierBackend for DockerCourier {
    fn id(&self) -> &str {
        "docker"
    }

    fn kind(&self) -> CourierKind {
        CourierKind::Docker
    }

    async fn capabilities(&self) -> Result<CourierCapabilities, CourierError> {
        Ok(CourierCapabilities {
            courier_id: self.id().to_string(),
            kind: self.kind(),
            supports_chat: true,
            supports_job: true,
            supports_heartbeat: true,
            supports_local_tools: true,
            supports_mounts: vec![MountKind::Session, MountKind::Memory, MountKind::Artifacts],
        })
    }

    fn validate_parcel(
        &self,
        parcel: &LoadedParcel,
    ) -> impl Future<Output = Result<(), CourierError>> + Send {
        let reference = parcel.config.courier.reference().to_string();
        let network = parcel.config.network.clone();
        async move {
            validate_courier_reference("docker", CourierKind::Docker, &reference)?;
            ensure_network_rules_supported("docker", &network)
        }
    }

    fn inspect(
        &self,
        parcel: &LoadedParcel,
    ) -> impl Future<Output = Result<CourierInspection, CourierError>> + Send {
        let reference = parcel.config.courier.reference().to_string();
        let inspection = CourierInspection {
            courier_id: self.id().to_string(),
            kind: self.kind(),
            entrypoint: parcel.config.entrypoint.clone(),
            required_secrets: parcel
                .config
                .secrets
                .iter()
                .map(|secret| secret.name.clone())
                .collect(),
            mounts: parcel.config.mounts.clone(),
            local_tools: list_local_tools(parcel),
            extensions: None,
        };
        async move {
            validate_courier_reference("docker", CourierKind::Docker, &reference)?;
            ensure_network_rules_supported("docker", &parcel.config.network)?;
            Ok(inspection)
        }
    }

    fn open_session(
        &self,
        parcel: &LoadedParcel,
    ) -> impl Future<Output = Result<CourierSession, CourierError>> + Send {
        let reference = parcel.config.courier.reference().to_string();
        let parcel_digest = parcel.config.digest.clone();
        let entrypoint = parcel.config.entrypoint.clone();
        async move {
            validate_courier_reference("docker", CourierKind::Docker, &reference)?;
            ensure_network_rules_supported("docker", &parcel.config.network)?;
            validate_required_secrets(parcel)?;
            let sequence = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
            let session_id = format!("docker-{parcel_digest}-{sequence}");
            let session = CourierSession {
                resolved_mounts: resolve_builtin_mounts(parcel, "docker", &session_id)?,
                id: session_id,
                parcel_digest,
                entrypoint,
                label: parcel.config.name.clone(),
                turn_count: 0,
                elapsed_ms: 0,
                history: Vec::new(),
                backend_state: None,
            };
            persist_session_mounts(&session)?;
            Ok(session)
        }
    }

    fn run(
        &self,
        parcel: &LoadedParcel,
        request: CourierRequest,
    ) -> impl Future<Output = Result<CourierResponse, CourierError>> + Send {
        let operation = request.operation;
        let mut session = request.session;
        let docker_courier = self.clone();
        let chat_backend_override = self.chat_backend_override.clone();

        async move {
            validate_courier_reference(
                "docker",
                CourierKind::Docker,
                parcel.config.courier.reference(),
            )?;
            ensure_network_rules_supported("docker", &parcel.config.network)?;
            ensure_session_matches_parcel(parcel, &session)?;
            ensure_operation_matches_entrypoint(&session, &operation)?;
            let consumes_run_budget = operation_counts_toward_run_budget(&operation);
            if consumes_run_budget {
                ensure_run_timeout_budget(&session, &parcel.config.timeouts)?;
            }
            let run_deadline = if consumes_run_budget {
                run_timeout_deadline(&session, &parcel.config.timeouts)?
            } else {
                None
            };
            session.turn_count += 1;
            let started_at = Instant::now();

            let mut response = match operation {
                CourierOperation::ResolvePrompt => Ok(CourierResponse {
                    courier_id: "docker".to_string(),
                    session: {
                        persist_session_mounts(&session)?;
                        session
                    },
                    events: vec![
                        CourierEvent::PromptResolved {
                            text: resolve_prompt_text(parcel)?,
                        },
                        CourierEvent::Done,
                    ],
                }),
                CourierOperation::ListLocalTools => Ok(CourierResponse {
                    courier_id: "docker".to_string(),
                    session: {
                        persist_session_mounts(&session)?;
                        session
                    },
                    events: vec![
                        CourierEvent::LocalToolsListed {
                            tools: list_local_tools(parcel),
                        },
                        CourierEvent::Done,
                    ],
                }),
                CourierOperation::InvokeTool { invocation } => {
                    let tool = resolve_local_tool(parcel, &invocation.name)?;
                    if let Some(request) =
                        build_local_tool_approval_request(&tool, invocation.input.as_deref())
                        && !check_tool_approval(&request)?
                    {
                        return Err(CourierError::ApprovalDenied { tool: request.tool });
                    }
                    let result = execute_local_tool_in_docker(
                        parcel,
                        &tool,
                        invocation.input.as_deref(),
                        &docker_courier,
                        run_deadline,
                    )?;

                    Ok(CourierResponse {
                        courier_id: "docker".to_string(),
                        session: {
                            persist_session_mounts(&session)?;
                            session
                        },
                        events: vec![
                            CourierEvent::ToolCallStarted {
                                invocation,
                                command: result.command.clone(),
                                args: result.args.clone(),
                            },
                            CourierEvent::ToolCallFinished { result },
                            CourierEvent::Done,
                        ],
                    })
                }
                CourierOperation::Chat { input } => {
                    session.history.push(ConversationMessage {
                        role: "user".to_string(),
                        content: input.clone(),
                    });
                    let mut chat_turn = execute_host_turn(
                        parcel,
                        &session,
                        &input,
                        NativeTurnMode::Chat,
                        HostTurnContext {
                            chat_backend_override: chat_backend_override.as_ref(),
                            tool_runner: HostToolRunner::Docker(&docker_courier),
                            host_label: "Docker",
                            run_deadline,
                        },
                    )?;
                    let (assistant_history_content, channel_reply) =
                        normalize_assistant_reply(&chat_turn.reply);
                    session.history.push(ConversationMessage {
                        role: "assistant".to_string(),
                        content: assistant_history_content,
                    });
                    session.backend_state = chat_turn.backend_state.clone();
                    if !chat_turn.streamed_reply {
                        if let Some(message) = channel_reply {
                            chat_turn
                                .events
                                .push(CourierEvent::ChannelReply { message });
                        } else {
                            chat_turn.events.push(CourierEvent::Message {
                                role: "assistant".to_string(),
                                content: chat_turn.reply.clone(),
                            });
                        }
                    }
                    chat_turn.events.push(CourierEvent::Done);

                    persist_session_mounts(&session)?;
                    Ok(CourierResponse {
                        courier_id: "docker".to_string(),
                        session,
                        events: chat_turn.events,
                    })
                }
                CourierOperation::Job { payload } => run_host_task_operation(
                    parcel,
                    session,
                    NativeTurnMode::Job,
                    format_job_payload(&payload),
                    HostTurnContext {
                        chat_backend_override: chat_backend_override.as_ref(),
                        tool_runner: HostToolRunner::Docker(&docker_courier),
                        host_label: "Docker",
                        run_deadline,
                    },
                ),
                CourierOperation::Heartbeat { payload } => run_host_task_operation(
                    parcel,
                    session,
                    NativeTurnMode::Heartbeat,
                    format_heartbeat_payload(payload.as_deref()),
                    HostTurnContext {
                        chat_backend_override: chat_backend_override.as_ref(),
                        tool_runner: HostToolRunner::Docker(&docker_courier),
                        host_label: "Docker",
                        run_deadline,
                    },
                ),
            }?;

            if consumes_run_budget {
                apply_session_run_elapsed(&mut response.session, started_at);
            }
            persist_session_mounts(&response.session)?;
            Ok(response)
        }
    }
}

impl CourierBackend for JsonlCourierPlugin {
    fn id(&self) -> &str {
        &self.manifest.name
    }

    fn kind(&self) -> CourierKind {
        CourierKind::Custom
    }

    fn capabilities(
        &self,
    ) -> impl Future<Output = Result<CourierCapabilities, CourierError>> + Send {
        let courier = self.clone();
        async move {
            match courier.plugin_request(PluginRequest::Capabilities) {
                Ok(PluginResponse::Capabilities { capabilities }) => Ok(capabilities),
                Ok(PluginResponse::Error { error }) => Err(CourierError::PluginProtocol {
                    courier: courier.manifest.name.clone(),
                    message: format!("{}: {}", error.code, error.message),
                }),
                Ok(other) => Err(courier
                    .unexpected_plugin_response("capabilities", describe_plugin_response(&other))),
                Err(error) => Err(error),
            }
        }
    }

    fn validate_parcel(
        &self,
        parcel: &LoadedParcel,
    ) -> impl Future<Output = Result<(), CourierError>> + Send {
        let courier = self.clone();
        let parcel_dir = canonical_parcel_dir(parcel);
        async move {
            ensure_network_rules_supported(&courier.manifest.name, &parcel.config.network)?;
            let parcel_dir = parcel_dir?;
            match courier.plugin_request(PluginRequest::ValidateParcel { parcel_dir })? {
                PluginResponse::Ok => Ok(()),
                PluginResponse::Error { error } => Err(CourierError::PluginProtocol {
                    courier: courier.manifest.name.clone(),
                    message: format!("{}: {}", error.code, error.message),
                }),
                other => Err(courier.unexpected_plugin_response(
                    "validate_parcel",
                    describe_plugin_response(&other),
                )),
            }
        }
    }

    fn inspect(
        &self,
        parcel: &LoadedParcel,
    ) -> impl Future<Output = Result<CourierInspection, CourierError>> + Send {
        let courier = self.clone();
        let parcel_dir = canonical_parcel_dir(parcel);
        async move {
            ensure_network_rules_supported(&courier.manifest.name, &parcel.config.network)?;
            let parcel_dir = parcel_dir?;
            match courier.plugin_request(PluginRequest::Inspect { parcel_dir })? {
                PluginResponse::Inspection { inspection } => Ok(inspection),
                PluginResponse::Error { error } => Err(CourierError::PluginProtocol {
                    courier: courier.manifest.name.clone(),
                    message: format!("{}: {}", error.code, error.message),
                }),
                other => {
                    Err(courier
                        .unexpected_plugin_response("inspect", describe_plugin_response(&other)))
                }
            }
        }
    }

    fn open_session(
        &self,
        parcel: &LoadedParcel,
    ) -> impl Future<Output = Result<CourierSession, CourierError>> + Send {
        let courier = self.clone();
        let parcel_dir = canonical_parcel_dir(parcel);
        async move {
            ensure_network_rules_supported(&courier.manifest.name, &parcel.config.network)?;
            validate_required_secrets(parcel)?;
            let capabilities = courier.capabilities().await?;
            ensure_mounts_supported(
                &courier.manifest.name,
                parcel.config.mounts.as_slice(),
                &capabilities.supports_mounts,
            )?;
            let parcel_dir = parcel_dir?;
            let mut process = courier.spawn_persistent_plugin()?;
            let request_id = process.write_request(
                courier.manifest.protocol_version,
                &courier.manifest.name,
                PluginRequest::OpenSession { parcel_dir },
            )?;
            match process.read_response(&courier.manifest.name, &request_id)? {
                PluginResponse::Session { session } => {
                    courier.store_persistent_process(session.id.clone(), process)?;
                    Ok(session)
                }
                PluginResponse::Error { error } => Err(CourierError::PluginProtocol {
                    courier: courier.manifest.name.clone(),
                    message: format!("{}: {}", error.code, error.message),
                }),
                other => Err(courier
                    .unexpected_plugin_response("open_session", describe_plugin_response(&other))),
            }
        }
    }

    fn run(
        &self,
        parcel: &LoadedParcel,
        request: CourierRequest,
    ) -> impl Future<Output = Result<CourierResponse, CourierError>> + Send {
        let courier = self.clone();
        let parcel_dir = canonical_parcel_dir(parcel);
        let operation = request.operation;
        let session = request.session;
        async move {
            ensure_network_rules_supported(&courier.manifest.name, &parcel.config.network)?;
            ensure_session_matches_parcel(parcel, &session)?;
            let consumes_run_budget = operation_counts_toward_run_budget(&operation);
            if consumes_run_budget {
                ensure_run_timeout_budget(&session, &parcel.config.timeouts)?;
            }
            let started_at = Instant::now();
            let parcel_dir = parcel_dir?;
            let session = courier.ensure_persistent_process(&parcel_dir, session)?;
            let run_timeout = if consumes_run_budget {
                remaining_run_budget_with_literal(&session, &parcel.config.timeouts)?
            } else {
                None
            };
            let (session, events) = courier.run_persistent_plugin(
                session.id.clone(),
                PluginRequest::Run {
                    parcel_dir,
                    session,
                    operation,
                },
                run_timeout,
            )?;
            let mut response = CourierResponse {
                courier_id: courier.manifest.name.clone(),
                session,
                events,
            };
            if consumes_run_budget {
                apply_session_run_elapsed(&mut response.session, started_at);
            }
            Ok(response)
        }
    }
}

impl JsonlCourierPlugin {
    fn plugin_request(&self, request: PluginRequest) -> Result<PluginResponse, CourierError> {
        let mut child = self.spawn_plugin()?;
        let request_id = write_plugin_request(
            &mut child,
            &self.manifest.name,
            self.manifest.protocol_version,
            request,
            true,
        )?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CourierError::PluginProtocol {
                courier: self.manifest.name.clone(),
                message: "plugin stdout was not captured".to_string(),
            })?;
        let mut reader = BufReader::new(stdout);
        let response =
            read_expected_plugin_response(&mut reader, &self.manifest.name, &request_id)?;
        wait_for_plugin_exit(child, &self.manifest.name)?;
        Ok(response)
    }

    fn spawn_plugin(&self) -> Result<Child, CourierError> {
        if let Some(expected_sha256) = self.manifest.installed_sha256.as_deref() {
            let actual_sha256 = hash_file_sha256(Path::new(&self.manifest.exec.command))?;
            if actual_sha256 != expected_sha256 {
                return Err(CourierError::PluginExecutableChanged {
                    courier: self.manifest.name.clone(),
                    expected_sha256: expected_sha256.to_string(),
                    actual_sha256,
                });
            }
        }

        let mut command = Command::new(&self.manifest.exec.command);
        command
            .args(&self.manifest.exec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.spawn().map_err(|source| CourierError::SpawnPlugin {
            courier: self.manifest.name.clone(),
            source,
        })
    }

    fn spawn_persistent_plugin(&self) -> Result<PersistentPluginProcess, CourierError> {
        let mut child = self.spawn_plugin()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| CourierError::PluginProtocol {
                courier: self.manifest.name.clone(),
                message: "plugin stdin was not captured".to_string(),
            })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CourierError::PluginProtocol {
                courier: self.manifest.name.clone(),
                message: "plugin stdout was not captured".to_string(),
            })?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| CourierError::PluginProtocol {
                courier: self.manifest.name.clone(),
                message: "plugin stderr was not captured".to_string(),
            })?;
        Ok(PersistentPluginProcess {
            child,
            stdin,
            stderr,
            responses: spawn_plugin_response_reader(stdout, self.manifest.name.clone()),
        })
    }

    fn store_persistent_process(
        &self,
        session_id: String,
        process: PersistentPluginProcess,
    ) -> Result<(), CourierError> {
        let mut sessions =
            self.state
                .sessions
                .lock()
                .map_err(|_| CourierError::PluginProtocol {
                    courier: self.manifest.name.clone(),
                    message: "plugin session state is poisoned".to_string(),
                })?;
        if let Some(mut existing) = sessions.remove(&session_id) {
            let _ = shutdown_persistent_plugin_process(
                &mut existing,
                &self.manifest.name,
                self.manifest.protocol_version,
            );
        }
        sessions.insert(session_id, process);
        Ok(())
    }

    fn run_persistent_plugin(
        &self,
        session_id: String,
        request: PluginRequest,
        run_timeout: Option<(String, Duration)>,
    ) -> Result<(CourierSession, Vec<CourierEvent>), CourierError> {
        let mut sessions =
            self.state
                .sessions
                .lock()
                .map_err(|_| CourierError::PluginProtocol {
                    courier: self.manifest.name.clone(),
                    message: "plugin session state is poisoned".to_string(),
                })?;
        let process =
            sessions
                .get_mut(&session_id)
                .ok_or_else(|| CourierError::PluginProtocol {
                    courier: self.manifest.name.clone(),
                    message: format!(
                        "no persistent plugin process exists for session `{session_id}`"
                    ),
                })?;
        let request_id =
            process.write_request(self.manifest.protocol_version, &self.manifest.name, request)?;
        let mut events = Vec::new();
        match read_plugin_run_completion(
            process,
            &self.manifest.name,
            &session_id,
            &request_id,
            run_timeout,
            &mut events,
        ) {
            Ok(session) => Ok((session, events)),
            Err(error) => {
                let mut process = sessions.remove(&session_id).expect("session just existed");
                let _ = shutdown_persistent_plugin_process(
                    &mut process,
                    &self.manifest.name,
                    self.manifest.protocol_version,
                );
                Err(error)
            }
        }
    }

    fn ensure_persistent_process(
        &self,
        parcel_dir: &str,
        session: CourierSession,
    ) -> Result<CourierSession, CourierError> {
        {
            let sessions =
                self.state
                    .sessions
                    .lock()
                    .map_err(|_| CourierError::PluginProtocol {
                        courier: self.manifest.name.clone(),
                        message: "plugin session state is poisoned".to_string(),
                    })?;
            if sessions.contains_key(&session.id) {
                return Ok(session);
            }
        }

        let mut process = self.spawn_persistent_plugin()?;
        let request_id = process.write_request(
            self.manifest.protocol_version,
            &self.manifest.name,
            PluginRequest::ResumeSession {
                parcel_dir: parcel_dir.to_string(),
                session,
            },
        )?;
        match process.read_response(&self.manifest.name, &request_id) {
            Ok(PluginResponse::Session { session }) => {
                self.store_persistent_process(session.id.clone(), process)?;
                Ok(session)
            }
            Ok(PluginResponse::Error { error }) => {
                let _ = shutdown_persistent_plugin_process(
                    &mut process,
                    &self.manifest.name,
                    self.manifest.protocol_version,
                );
                Err(CourierError::PluginProtocol {
                    courier: self.manifest.name.clone(),
                    message: format!("{}: {}", error.code, error.message),
                })
            }
            Ok(other) => {
                let _ = shutdown_persistent_plugin_process(
                    &mut process,
                    &self.manifest.name,
                    self.manifest.protocol_version,
                );
                Err(self
                    .unexpected_plugin_response("resume_session", describe_plugin_response(&other)))
            }
            Err(error) => {
                let _ = shutdown_persistent_plugin_process(
                    &mut process,
                    &self.manifest.name,
                    self.manifest.protocol_version,
                );
                Err(error)
            }
        }
    }

    fn unexpected_plugin_response(&self, request_kind: &str, response_kind: &str) -> CourierError {
        CourierError::PluginProtocol {
            courier: self.manifest.name.clone(),
            message: format!("unexpected plugin response for `{request_kind}`: {response_kind}"),
        }
    }
}

impl CourierBackend for WasmCourier {
    fn id(&self) -> &str {
        "wasm"
    }

    fn kind(&self) -> CourierKind {
        CourierKind::Wasm
    }

    async fn capabilities(&self) -> Result<CourierCapabilities, CourierError> {
        Ok(CourierCapabilities {
            courier_id: "wasm".to_string(),
            kind: CourierKind::Wasm,
            supports_chat: true,
            supports_job: true,
            supports_heartbeat: true,
            supports_local_tools: true,
            supports_mounts: vec![MountKind::Session, MountKind::Memory, MountKind::Artifacts],
        })
    }

    fn validate_parcel(
        &self,
        parcel: &LoadedParcel,
    ) -> impl Future<Output = Result<(), CourierError>> + Send {
        let engine = self.engine.clone();
        let component_cache = self.component_cache.clone();
        let parcel = parcel.clone();
        async move {
            validate_courier_reference(
                "wasm",
                CourierKind::Wasm,
                parcel.config.courier.reference(),
            )?;
            ensure_network_rules_supported("wasm", &parcel.config.network)?;
            let component_path = resolve_wasm_component_path(&parcel)?;
            validate_wasm_component_metadata(&parcel)?;
            let _ = load_wasm_component(&engine, &component_cache, &parcel, &component_path)?;
            Ok(())
        }
    }

    fn inspect(
        &self,
        parcel: &LoadedParcel,
    ) -> impl Future<Output = Result<CourierInspection, CourierError>> + Send {
        let engine = self.engine.clone();
        let component_cache = self.component_cache.clone();
        let parcel = parcel.clone();
        async move {
            validate_courier_reference(
                "wasm",
                CourierKind::Wasm,
                parcel.config.courier.reference(),
            )?;
            ensure_network_rules_supported("wasm", &parcel.config.network)?;
            let component_path = resolve_wasm_component_path(&parcel)?;
            validate_wasm_component_metadata(&parcel)?;
            let _ = load_wasm_component(&engine, &component_cache, &parcel, &component_path)?;
            Ok(CourierInspection {
                courier_id: "wasm".to_string(),
                kind: CourierKind::Wasm,
                entrypoint: parcel.config.entrypoint.clone(),
                required_secrets: parcel
                    .config
                    .secrets
                    .iter()
                    .map(|secret| secret.name.clone())
                    .collect(),
                mounts: parcel.config.mounts.clone(),
                local_tools: list_local_tools(&parcel),
                extensions: None,
            })
        }
    }

    fn open_session(
        &self,
        parcel: &LoadedParcel,
    ) -> impl Future<Output = Result<CourierSession, CourierError>> + Send {
        let engine = self.engine.clone();
        let component_cache = self.component_cache.clone();
        let parcel = parcel.clone();
        async move {
            validate_courier_reference(
                "wasm",
                CourierKind::Wasm,
                parcel.config.courier.reference(),
            )?;
            ensure_network_rules_supported("wasm", &parcel.config.network)?;
            validate_required_secrets(&parcel)?;
            let component_path = resolve_wasm_component_path(&parcel)?;
            validate_wasm_component_metadata(&parcel)?;
            let _ = load_wasm_component(&engine, &component_cache, &parcel, &component_path)?;

            let sequence = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
            let session_id = format!("wasm-{}-{sequence}", parcel.config.digest);
            let session = CourierSession {
                resolved_mounts: resolve_builtin_mounts(&parcel, "wasm", &session_id)?,
                id: session_id,
                parcel_digest: parcel.config.digest.clone(),
                entrypoint: parcel.config.entrypoint.clone(),
                label: parcel.config.name.clone(),
                turn_count: 0,
                elapsed_ms: 0,
                history: Vec::new(),
                backend_state: None,
            };
            persist_session_mounts(&session)?;
            Ok(session)
        }
    }

    fn run(
        &self,
        parcel: &LoadedParcel,
        request: CourierRequest,
    ) -> impl Future<Output = Result<CourierResponse, CourierError>> + Send {
        let engine = self.engine.clone();
        let component_cache = self.component_cache.clone();
        let parcel = parcel.clone();
        let operation = request.operation;
        let mut session = request.session;
        let chat_backend_override = self.chat_backend_override.clone();
        async move {
            validate_courier_reference(
                "wasm",
                CourierKind::Wasm,
                parcel.config.courier.reference(),
            )?;
            ensure_network_rules_supported("wasm", &parcel.config.network)?;
            ensure_session_matches_parcel(&parcel, &session)?;
            validate_wasm_component_metadata(&parcel)?;
            let consumes_run_budget = operation_counts_toward_run_budget(&operation);
            if consumes_run_budget {
                ensure_run_timeout_budget(&session, &parcel.config.timeouts)?;
            }
            let run_deadline = if consumes_run_budget {
                run_timeout_deadline(&session, &parcel.config.timeouts)?
            } else {
                None
            };
            let started_at = Instant::now();

            let mut response = match operation {
                CourierOperation::ResolvePrompt => Ok(CourierResponse {
                    courier_id: "wasm".to_string(),
                    session: {
                        persist_session_mounts(&session)?;
                        session
                    },
                    events: vec![
                        CourierEvent::PromptResolved {
                            text: resolve_prompt_text(&parcel)?,
                        },
                        CourierEvent::Done,
                    ],
                }),
                CourierOperation::ListLocalTools => Ok(CourierResponse {
                    courier_id: "wasm".to_string(),
                    session: {
                        persist_session_mounts(&session)?;
                        session
                    },
                    events: vec![
                        CourierEvent::LocalToolsListed {
                            tools: list_local_tools(&parcel),
                        },
                        CourierEvent::Done,
                    ],
                }),
                CourierOperation::InvokeTool { invocation } => {
                    session.turn_count += 1;
                    let tool = resolve_local_tool(&parcel, &invocation.name)?;
                    if let Some(request) =
                        build_local_tool_approval_request(&tool, invocation.input.as_deref())
                        && !check_tool_approval(&request)?
                    {
                        return Err(CourierError::ApprovalDenied { tool: request.tool });
                    }
                    let result = execute_host_local_tool(
                        &parcel,
                        &tool,
                        invocation.input.as_deref(),
                        HostToolRunner::Native,
                        run_deadline,
                    )?;

                    Ok(CourierResponse {
                        courier_id: "wasm".to_string(),
                        session: {
                            persist_session_mounts(&session)?;
                            session
                        },
                        events: vec![
                            CourierEvent::ToolCallStarted {
                                invocation,
                                command: result.command.clone(),
                                args: result.args.clone(),
                            },
                            CourierEvent::ToolCallFinished { result },
                            CourierEvent::Done,
                        ],
                    })
                }
                CourierOperation::Chat { .. }
                | CourierOperation::Job { .. }
                | CourierOperation::Heartbeat { .. } => {
                    let operation_kind = match &operation {
                        CourierOperation::Chat { .. } => "chat",
                        CourierOperation::Job { .. } => "job",
                        CourierOperation::Heartbeat { .. } => "heartbeat",
                        _ => unreachable!(),
                    };
                    let guest_operation = wasm_operation(&operation).expect("guest operation");
                    let (mut store, guest, parcel_context) = instantiate_wasm_guest(
                        &engine,
                        &component_cache,
                        &parcel,
                        &session,
                        chat_backend_override,
                    )?;
                    let result = guest
                        .dispatch_courier_guest()
                        .call_handle_operation(
                            &mut store,
                            &parcel_context,
                            &wasm_guest_session(&session),
                            &guest_operation,
                        )
                        .map_err(|source| {
                            let component_path = resolve_wasm_component_path(&parcel)
                                .unwrap_or_else(|_| parcel.parcel_dir.join("context"));
                            CourierError::InstantiateWasmComponent {
                                courier: "wasm".to_string(),
                                path: component_path.display().to_string(),
                                source,
                            }
                        })?
                        .map_err(|message| CourierError::WasmGuest {
                            courier: "wasm".to_string(),
                            message: format!(
                                "guest rejected `{operation_kind}` operation: {message}"
                            ),
                        })?;

                    apply_wasm_turn_to_session(&mut session, &operation, &result);
                    persist_session_mounts(&session)?;
                    Ok(CourierResponse {
                        courier_id: "wasm".to_string(),
                        session,
                        events: wasm_events_to_courier_events(result.events),
                    })
                }
            }?;

            if consumes_run_budget {
                apply_session_run_elapsed(&mut response.session, started_at);
            }
            persist_session_mounts(&response.session)?;
            Ok(response)
        }
    }
}

impl CourierBackend for StubCourier {
    fn id(&self) -> &str {
        self.courier_id
    }

    fn kind(&self) -> CourierKind {
        self.kind
    }

    async fn capabilities(&self) -> Result<CourierCapabilities, CourierError> {
        Ok(CourierCapabilities {
            courier_id: self.id().to_string(),
            kind: self.kind(),
            supports_chat: false,
            supports_job: false,
            supports_heartbeat: false,
            supports_local_tools: false,
            supports_mounts: Vec::new(),
        })
    }

    fn validate_parcel(
        &self,
        parcel: &LoadedParcel,
    ) -> impl Future<Output = Result<(), CourierError>> + Send {
        let courier_id = self.courier_id;
        let kind = self.kind;
        let reference = parcel.config.courier.reference().to_string();
        let network = parcel.config.network.clone();
        async move {
            validate_courier_reference(courier_id, kind, &reference)?;
            ensure_network_rules_supported(courier_id, &network)
        }
    }

    fn inspect(
        &self,
        parcel: &LoadedParcel,
    ) -> impl Future<Output = Result<CourierInspection, CourierError>> + Send {
        let courier_id = self.courier_id;
        let kind = self.kind;
        let reference = parcel.config.courier.reference().to_string();
        let inspection = CourierInspection {
            courier_id: courier_id.to_string(),
            kind,
            entrypoint: parcel.config.entrypoint.clone(),
            required_secrets: parcel
                .config
                .secrets
                .iter()
                .map(|secret| secret.name.clone())
                .collect(),
            mounts: parcel.config.mounts.clone(),
            local_tools: list_local_tools(parcel),
            extensions: None,
        };
        async move {
            validate_courier_reference(courier_id, kind, &reference)?;
            ensure_network_rules_supported(courier_id, &parcel.config.network)?;
            Ok(inspection)
        }
    }

    fn open_session(
        &self,
        parcel: &LoadedParcel,
    ) -> impl Future<Output = Result<CourierSession, CourierError>> + Send {
        let courier_id = self.courier_id;
        let kind = self.kind;
        let reference = parcel.config.courier.reference().to_string();
        let parcel_digest = parcel.config.digest.clone();
        let entrypoint = parcel.config.entrypoint.clone();
        async move {
            validate_courier_reference(courier_id, kind, &reference)?;
            ensure_network_rules_supported(courier_id, &parcel.config.network)?;
            validate_required_secrets(parcel)?;
            ensure_mounts_supported(courier_id, parcel.config.mounts.as_slice(), &[])?;
            let sequence = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
            let session = CourierSession {
                id: format!("{courier_id}-{parcel_digest}-{sequence}"),
                parcel_digest,
                entrypoint,
                label: parcel.config.name.clone(),
                turn_count: 0,
                elapsed_ms: 0,
                history: Vec::new(),
                resolved_mounts: Vec::new(),
                backend_state: None,
            };
            Ok(session)
        }
    }

    fn run(
        &self,
        parcel: &LoadedParcel,
        request: CourierRequest,
    ) -> impl Future<Output = Result<CourierResponse, CourierError>> + Send {
        let courier_id = self.courier_id.to_string();
        let kind = self.kind;
        let reference = parcel.config.courier.reference().to_string();
        let operation = request.operation;
        let mut session = request.session;

        async move {
            validate_courier_reference(&courier_id, kind, &reference)?;
            ensure_network_rules_supported(&courier_id, &parcel.config.network)?;
            ensure_session_matches_parcel(parcel, &session)?;
            let consumes_run_budget = operation_counts_toward_run_budget(&operation);
            if consumes_run_budget {
                ensure_run_timeout_budget(&session, &parcel.config.timeouts)?;
            }
            session.turn_count += 1;
            let started_at = Instant::now();

            let mut response = match operation {
                CourierOperation::ResolvePrompt => Ok(CourierResponse {
                    courier_id,
                    session,
                    events: vec![
                        CourierEvent::PromptResolved {
                            text: resolve_prompt_text(parcel)?,
                        },
                        CourierEvent::Done,
                    ],
                }),
                CourierOperation::ListLocalTools => Ok(CourierResponse {
                    courier_id,
                    session,
                    events: vec![
                        CourierEvent::LocalToolsListed {
                            tools: list_local_tools(parcel),
                        },
                        CourierEvent::Done,
                    ],
                }),
                CourierOperation::InvokeTool { .. } => Err(CourierError::UnsupportedOperation {
                    courier: courier_id,
                    operation: "invoke_tool".to_string(),
                }),
                CourierOperation::Chat { .. } => Err(CourierError::UnsupportedOperation {
                    courier: courier_id,
                    operation: "chat".to_string(),
                }),
                CourierOperation::Job { .. } => Err(CourierError::UnsupportedOperation {
                    courier: courier_id,
                    operation: "job".to_string(),
                }),
                CourierOperation::Heartbeat { .. } => Err(CourierError::UnsupportedOperation {
                    courier: courier_id,
                    operation: "heartbeat".to_string(),
                }),
            }?;

            if consumes_run_budget {
                apply_session_run_elapsed(&mut response.session, started_at);
            }
            Ok(response)
        }
    }
}

fn run_host_task_operation(
    parcel: &LoadedParcel,
    mut session: CourierSession,
    mode: NativeTurnMode,
    input: String,
    context: HostTurnContext<'_>,
) -> Result<CourierResponse, CourierError> {
    session.history.push(ConversationMessage {
        role: "user".to_string(),
        content: input.clone(),
    });
    let mut turn = execute_host_turn(parcel, &session, &input, mode, context)?;
    let (assistant_history_content, channel_reply) = normalize_assistant_reply(&turn.reply);
    session.history.push(ConversationMessage {
        role: "assistant".to_string(),
        content: assistant_history_content,
    });
    session.backend_state = turn.backend_state.clone();
    if !turn.streamed_reply {
        if let Some(message) = channel_reply {
            turn.events.push(CourierEvent::ChannelReply { message });
        } else {
            turn.events.push(CourierEvent::Message {
                role: "assistant".to_string(),
                content: turn.reply.clone(),
            });
        }
    }
    turn.events.push(CourierEvent::Done);

    persist_session_mounts(&session)?;
    Ok(CourierResponse {
        courier_id: context.host_label.to_ascii_lowercase(),
        session,
        events: turn.events,
    })
}

fn validate_native_parcel(parcel: &LoadedParcel) -> Result<(), CourierError> {
    validate_courier_reference(
        "native",
        CourierKind::Native,
        parcel.config.courier.reference(),
    )?;
    ensure_network_rules_supported("native", &parcel.config.network)
}

fn ensure_network_rules_supported(
    courier: &str,
    network_rules: &[NetworkRule],
) -> Result<(), CourierError> {
    let Some(rule) = network_rules.first() else {
        return Ok(());
    };
    Err(CourierError::UnsupportedNetworkPolicy {
        courier: courier.to_string(),
        action: rule.action.clone(),
        target: rule.target.clone(),
    })
}

fn instruction_heading(kind: InstructionKind) -> &'static str {
    match kind {
        InstructionKind::Identity => "IDENTITY",
        InstructionKind::Soul => "SOUL",
        InstructionKind::Skill => "SKILL",
        InstructionKind::Agents => "AGENTS",
        InstructionKind::User => "USER",
        InstructionKind::Tools => "TOOLS",
        InstructionKind::Memory => "MEMORY",
        InstructionKind::Heartbeat => "HEARTBEAT",
        InstructionKind::Eval => "EVAL",
    }
}

fn next_generated_tool_call_id() -> String {
    format!("call_{}", SESSION_COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn encode_hex(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn hash_file_sha256(path: &Path) -> Result<String, CourierError> {
    let body = fs::read(path).map_err(|source| CourierError::ReadFile {
        path: path.display().to_string(),
        source,
    })?;
    Ok(encode_hex(Sha256::digest(&body)))
}

#[cfg(test)]
mod tests;
