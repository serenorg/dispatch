use crate::{
    manifest::{
        A2aAuthScheme, A2aEndpointMode, BuiltinToolConfig, InstructionKind, ModelReference,
        MountConfig, MountKind, ParcelManifest, ToolConfig,
    },
    plugin_protocol::{PluginRequest, PluginRequestEnvelope, PluginResponse},
    plugins::CourierPluginManifest,
};
use dispatch_wasm_abi::ABI as DISPATCH_WASM_COMPONENT_ABI;
use jsonschema::Validator;
use rusqlite::{Connection, params};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    collections::BTreeMap,
    fs,
    future::Future,
    io::{BufReader, Write as _},
    path::{Path, PathBuf},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio},
    sync::OnceLock,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use wasmtime::{
    Config, Engine, Store,
    component::{Component, HasSelf, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

#[path = "courier_a2a.rs"]
mod a2a;
mod model_backends;

use self::{a2a::execute_a2a_tool_with_env, model_backends::*};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);
static PARCEL_SCHEMA_VALIDATOR: OnceLock<Validator> = OnceLock::new();

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
    #[error("required secret `{name}` is not present in the environment")]
    MissingSecret { name: String },
    #[error("parcel manifest `{path}` does not conform to the Dispatch parcel schema: {message}")]
    InvalidParcelSchema { path: String, message: String },
    #[error("courier `{courier}` does not support mount `{kind:?}` with driver `{driver}`")]
    UnsupportedMount {
        courier: String,
        kind: MountKind,
        driver: String,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocalToolTransport {
    Local,
    A2a,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum LocalToolTarget {
    Local {
        packaged_path: String,
        command: String,
        args: Vec<String>,
    },
    A2a {
        endpoint_url: String,
        endpoint_mode: Option<A2aEndpointMode>,
        auth_secret_name: Option<String>,
        auth_scheme: Option<A2aAuthScheme>,
        auth_header_name: Option<String>,
        expected_agent_name: Option<String>,
        expected_card_sha256: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalToolSpec {
    pub alias: String,
    pub description: Option<String>,
    pub input_schema_packaged_path: Option<String>,
    pub input_schema_sha256: Option<String>,
    #[serde(flatten)]
    pub target: LocalToolTarget,
}

impl LocalToolSpec {
    pub fn transport(&self) -> LocalToolTransport {
        match self.target {
            LocalToolTarget::Local { .. } => LocalToolTransport::Local,
            LocalToolTarget::A2a { .. } => LocalToolTransport::A2a,
        }
    }

    pub fn packaged_path(&self) -> Option<&str> {
        match &self.target {
            LocalToolTarget::Local { packaged_path, .. } => Some(packaged_path.as_str()),
            LocalToolTarget::A2a { .. } => None,
        }
    }

    pub fn command(&self) -> &str {
        match &self.target {
            LocalToolTarget::Local { command, .. } => command.as_str(),
            LocalToolTarget::A2a { .. } => "dispatch-a2a",
        }
    }

    pub fn args(&self) -> &[String] {
        match &self.target {
            LocalToolTarget::Local { args, .. } => args.as_slice(),
            LocalToolTarget::A2a { .. } => &[],
        }
    }

    pub fn endpoint_url(&self) -> Option<&str> {
        match &self.target {
            LocalToolTarget::A2a { endpoint_url, .. } => Some(endpoint_url.as_str()),
            LocalToolTarget::Local { .. } => None,
        }
    }

    pub fn endpoint_mode(&self) -> Option<A2aEndpointMode> {
        match &self.target {
            LocalToolTarget::A2a { endpoint_mode, .. } => *endpoint_mode,
            LocalToolTarget::Local { .. } => None,
        }
    }

    pub fn auth_secret_name(&self) -> Option<&str> {
        match &self.target {
            LocalToolTarget::A2a {
                auth_secret_name, ..
            } => auth_secret_name.as_deref(),
            LocalToolTarget::Local { .. } => None,
        }
    }

    pub fn auth_scheme(&self) -> Option<A2aAuthScheme> {
        match &self.target {
            LocalToolTarget::A2a { auth_scheme, .. } => *auth_scheme,
            LocalToolTarget::Local { .. } => None,
        }
    }

    pub fn expected_agent_name(&self) -> Option<&str> {
        match &self.target {
            LocalToolTarget::A2a {
                expected_agent_name,
                ..
            } => expected_agent_name.as_deref(),
            LocalToolTarget::Local { .. } => None,
        }
    }

    pub fn expected_card_sha256(&self) -> Option<&str> {
        match &self.target {
            LocalToolTarget::A2a {
                expected_card_sha256,
                ..
            } => expected_card_sha256.as_deref(),
            LocalToolTarget::Local { .. } => None,
        }
    }

    pub fn auth_header_name(&self) -> Option<&str> {
        match &self.target {
            LocalToolTarget::A2a {
                auth_header_name, ..
            } => auth_header_name.as_deref(),
            LocalToolTarget::Local { .. } => None,
        }
    }

    pub fn matches_name(&self, tool_name: &str) -> bool {
        self.alias == tool_name || self.packaged_path().is_some_and(|path| path == tool_name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuiltinToolSpec {
    pub capability: String,
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolRunResult {
    pub tool: String,
    pub command: String,
    pub args: Vec<String>,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CourierKind {
    Native,
    Docker,
    Wasm,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MountRequest {
    pub parcel_digest: String,
    pub spec: MountConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedMount {
    pub kind: MountKind,
    pub driver: String,
    pub target_path: String,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolInvocation {
    pub name: String,
    pub input: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
/// Courier operations mix side-effecting turns (`chat`, `job`, `heartbeat`,
/// `invoke_tool`) with read-style queries (`resolve_prompt`,
/// `list_local_tools`). Callers should treat the latter as parcel inspection
/// helpers even though they share the same request envelope.
pub enum CourierOperation {
    ResolvePrompt,
    ListLocalTools,
    InvokeTool { invocation: ToolInvocation },
    Chat { input: String },
    Job { payload: String },
    Heartbeat { payload: Option<String> },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CourierRequest {
    pub session: CourierSession,
    pub operation: CourierOperation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CourierCapabilities {
    pub courier_id: String,
    pub kind: CourierKind,
    pub supports_chat: bool,
    pub supports_job: bool,
    pub supports_heartbeat: bool,
    pub supports_local_tools: bool,
    pub supports_mounts: Vec<MountKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CourierInspection {
    pub courier_id: String,
    pub kind: CourierKind,
    pub entrypoint: Option<String>,
    pub required_secrets: Vec<String>,
    pub mounts: Vec<MountConfig>,
    pub local_tools: Vec<LocalToolSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CourierSession {
    pub id: String,
    pub parcel_digest: String,
    pub entrypoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub turn_count: u64,
    #[serde(default)]
    pub elapsed_ms: u64,
    pub history: Vec<ConversationMessage>,
    #[serde(default)]
    pub resolved_mounts: Vec<ResolvedMount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_state: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CourierEvent {
    PromptResolved {
        text: String,
    },
    LocalToolsListed {
        tools: Vec<LocalToolSpec>,
    },
    BackendFallback {
        backend: String,
        error: String,
    },
    ToolCallStarted {
        invocation: ToolInvocation,
        command: String,
        args: Vec<String>,
    },
    ToolCallFinished {
        result: ToolRunResult,
    },
    Message {
        role: String,
        content: String,
    },
    TextDelta {
        content: String,
    },
    Done,
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
}

struct WasmHostState {
    host: WasmHost,
    wasi_ctx: WasiCtx,
    resource_table: ResourceTable,
}

struct WasmHost {
    parcel: LoadedParcel,
    session: CourierSession,
    chat_backend_override: Option<Arc<dyn ChatModelBackend>>,
    run_deadline: Option<Instant>,
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
    pub llm_timeout_ms: Option<u64>,
    pub context_token_limit: Option<u32>,
    pub tool_call_limit: Option<u32>,
    pub tool_output_limit: Option<usize>,
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
    stdout: BufReader<ChildStdout>,
    stderr: ChildStderr,
}

#[derive(Debug, Clone)]
struct CachedValue<T> {
    value: T,
    last_used: u64,
}

#[derive(Debug, Clone)]
struct BoundedLruCache<T> {
    max_entries: usize,
    tick: u64,
    entries: BTreeMap<String, CachedValue<T>>,
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

impl Default for WasmCourier {
    fn default() -> Self {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).expect("failed to initialize wasmtime engine");
        Self {
            engine,
            chat_backend_override: None,
            component_cache: Arc::new(Mutex::new(BoundedLruCache::new(
                wasm_component_cache_limit(),
            ))),
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

impl wasm_bindings::dispatch::courier::host::Host for WasmHost {
    fn model_complete(
        &mut self,
        request: wasm_bindings::dispatch::courier::host::ModelRequest,
    ) -> Result<wasm_bindings::dispatch::courier::host::ModelResponse, String> {
        let messages = request
            .messages
            .into_iter()
            .map(|message| ConversationMessage {
                role: message.role,
                content: message.content,
            })
            .collect::<Vec<_>>();
        let tools = request
            .tools
            .into_iter()
            .map(|tool| {
                let format = match tool.kind {
                    wasm_bindings::dispatch::courier::host::ModelToolKind::Custom => {
                        Ok::<ModelToolFormat, String>(ModelToolFormat::Text)
                    }
                    wasm_bindings::dispatch::courier::host::ModelToolKind::Function => {
                        match tool.input_schema_json {
                            Some(schema_json) => {
                                let schema: serde_json::Value = serde_json::from_str(&schema_json)
                                    .map_err(|error| {
                                        format!("invalid model tool schema JSON: {error}")
                                    })?;
                                Ok::<ModelToolFormat, String>(ModelToolFormat::JsonSchema {
                                    schema,
                                })
                            }
                            None => Ok::<ModelToolFormat, String>(ModelToolFormat::Text),
                        }
                    }
                }?;
                Ok(ModelToolDefinition {
                    name: tool.name,
                    description: tool.description,
                    format,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let tool_outputs = request
            .tool_outputs
            .into_iter()
            .map(|output| ModelToolOutput {
                call_id: output.call_id,
                name: String::new(),
                output: output.output,
                kind: match output.kind {
                    wasm_bindings::dispatch::courier::host::ModelToolKind::Custom => {
                        ModelToolKind::Custom
                    }
                    wasm_bindings::dispatch::courier::host::ModelToolKind::Function => {
                        ModelToolKind::Function
                    }
                },
            })
            .collect::<Vec<_>>();
        let requests = build_wasm_model_requests(
            &self.parcel,
            WasmModelRequestInput {
                requested_model: request.model,
                instructions: request.instructions,
                messages,
                tools,
                tool_outputs,
                previous_response_id: request.previous_response_id,
                run_deadline: self.run_deadline,
            },
        )
        .map_err(|error| error.to_string())?;
        let mut last_error = None;
        for model_request in requests {
            let backend = select_chat_backend(self.chat_backend_override.as_ref(), &model_request);
            match backend.generate(&model_request) {
                Ok(ModelGeneration::Reply(reply)) => {
                    return Ok(wasm_bindings::dispatch::courier::host::ModelResponse {
                        backend: reply.backend,
                        text: reply.text,
                        response_id: reply.response_id,
                        tool_calls: reply
                            .tool_calls
                            .into_iter()
                            .map(
                                |call| wasm_bindings::dispatch::courier::host::ModelToolCall {
                                    call_id: call.call_id,
                                    name: call.name,
                                    input: call.input,
                                    kind: match call.kind {
                                        ModelToolKind::Custom => {
                                            wasm_bindings::dispatch::courier::host::ModelToolKind::Custom
                                        }
                                        ModelToolKind::Function => {
                                            wasm_bindings::dispatch::courier::host::ModelToolKind::Function
                                        }
                                    },
                                },
                            )
                            .collect(),
                    });
                }
                Ok(ModelGeneration::NotConfigured { backend, reason }) => {
                    last_error = Some(format!("{backend} backend not configured: {reason}"));
                }
                Err(error) => {
                    last_error = Some(error.to_string());
                }
            }
        }
        let message =
            last_error.unwrap_or_else(|| "no model configured for wasm guest request".to_string());
        Err(message)
    }

    fn invoke_tool(
        &mut self,
        invocation: wasm_bindings::dispatch::courier::host::ToolInvocation,
    ) -> Result<wasm_bindings::dispatch::courier::host::ToolResult, String> {
        let tool = resolve_local_tool(&self.parcel, &invocation.name)
            .map_err(|error| error.to_string())?;
        let result = execute_local_tool_with_env(
            &self.parcel,
            &tool,
            invocation.input.as_deref(),
            self.run_deadline,
            process_env_lookup,
        )
        .map_err(|error| error.to_string())?;
        Ok(wasm_bindings::dispatch::courier::host::ToolResult {
            tool: result.tool,
            command: result.command,
            args: result.args,
            exit_code: result.exit_code,
            stdout: result.stdout,
            stderr: result.stderr,
        })
    }

    fn memory_get(
        &mut self,
        namespace: String,
        key: String,
    ) -> Result<Option<wasm_bindings::dispatch::courier::host::MemoryEntry>, String> {
        memory_get(&self.session, &namespace, &key)
            .map(|entry| {
                entry.map(
                    |entry| wasm_bindings::dispatch::courier::host::MemoryEntry {
                        namespace: entry.namespace,
                        key: entry.key,
                        value: entry.value,
                        updated_at: entry.updated_at,
                    },
                )
            })
            .map_err(|error| error.to_string())
    }

    fn memory_put(
        &mut self,
        namespace: String,
        key: String,
        value: String,
    ) -> Result<bool, String> {
        memory_put(&self.session, &namespace, &key, &value).map_err(|error| error.to_string())
    }

    fn memory_delete(&mut self, namespace: String, key: String) -> Result<bool, String> {
        memory_delete(&self.session, &namespace, &key).map_err(|error| error.to_string())
    }

    fn memory_list(
        &mut self,
        namespace: String,
        prefix: Option<String>,
    ) -> Result<Vec<wasm_bindings::dispatch::courier::host::MemoryEntry>, String> {
        memory_list(&self.session, &namespace, prefix.as_deref())
            .map(|entries| {
                entries
                    .into_iter()
                    .map(
                        |entry| wasm_bindings::dispatch::courier::host::MemoryEntry {
                            namespace: entry.namespace,
                            key: entry.key,
                            value: entry.value,
                            updated_at: entry.updated_at,
                        },
                    )
                    .collect()
            })
            .map_err(|error| error.to_string())
    }
}

impl WasiView for WasmHostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi_ctx,
            table: &mut self.resource_table,
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

impl<T: Clone> BoundedLruCache<T> {
    fn new(max_entries: usize) -> Self {
        Self {
            max_entries,
            tick: 0,
            entries: BTreeMap::new(),
        }
    }

    fn get(&mut self, key: &str) -> Option<T> {
        let entry = self.entries.get(key).cloned()?;
        self.tick = self.tick.saturating_add(1);
        if let Some(current) = self.entries.get_mut(key) {
            current.last_used = self.tick;
        }
        Some(entry.value)
    }

    fn insert(&mut self, key: String, value: T) {
        if self.max_entries == 0 {
            return;
        }
        self.tick = self.tick.saturating_add(1);
        self.entries.insert(
            key,
            CachedValue {
                value,
                last_used: self.tick,
            },
        );
        while self.entries.len() > self.max_entries {
            let Some(evicted_key) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.entries.remove(&evicted_key);
        }
    }

    #[cfg(test)]
    fn keys(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }
}

fn wasm_component_cache_limit() -> usize {
    std::env::var("DISPATCH_WASM_COMPONENT_CACHE_SIZE")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(16)
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

pub fn load_parcel(path: &Path) -> Result<LoadedParcel, CourierError> {
    let manifest_path = resolve_manifest_path(path)?
        .canonicalize()
        .map_err(|source| CourierError::ReadFile {
            path: path.display().to_string(),
            source,
        })?;
    let parcel_dir = manifest_path.parent().map(PathBuf::from).ok_or_else(|| {
        CourierError::MissingParcelPath {
            path: manifest_path.display().to_string(),
        }
    })?;
    let source = fs::read_to_string(&manifest_path).map_err(|source| CourierError::ReadFile {
        path: manifest_path.display().to_string(),
        source,
    })?;
    let manifest_json = serde_json::from_str::<serde_json::Value>(&source).map_err(|source| {
        CourierError::ParseParcelManifest {
            path: manifest_path.display().to_string(),
            source,
        }
    })?;
    validate_parcel_schema(&manifest_path, &manifest_json)?;
    let config = serde_json::from_value::<ParcelManifest>(manifest_json).map_err(|source| {
        CourierError::ParseParcelManifest {
            path: manifest_path.display().to_string(),
            source,
        }
    })?;
    if config.schema != crate::manifest::PARCEL_SCHEMA_URL {
        return Err(CourierError::UnsupportedParcelSchema {
            path: manifest_path.display().to_string(),
            found: config.schema.clone(),
            expected: crate::manifest::PARCEL_SCHEMA_URL.to_string(),
        });
    }
    if config.format_version != crate::manifest::PARCEL_FORMAT_VERSION {
        return Err(CourierError::UnsupportedParcelFormatVersion {
            path: manifest_path.display().to_string(),
            found: config.format_version,
            supported: crate::manifest::PARCEL_FORMAT_VERSION,
        });
    }

    Ok(LoadedParcel {
        parcel_dir,
        manifest_path,
        config,
    })
}

pub fn resolve_prompt_text(parcel: &LoadedParcel) -> Result<String, CourierError> {
    let mut sections = Vec::new();

    for instruction in &parcel.config.instructions {
        if !matches!(
            instruction.kind,
            InstructionKind::Identity
                | InstructionKind::Soul
                | InstructionKind::Skill
                | InstructionKind::Agents
                | InstructionKind::User
                | InstructionKind::Tools
                | InstructionKind::Memory
                | InstructionKind::Heartbeat
        ) {
            continue;
        }
        let path = parcel
            .parcel_dir
            .join("context")
            .join(&instruction.packaged_path);
        let body = fs::read_to_string(&path).map_err(|source| CourierError::ReadFile {
            path: path.display().to_string(),
            source,
        })?;
        sections.push(format!(
            "# {}\n\n{}",
            instruction_heading(instruction.kind),
            body.trim_end()
        ));
    }

    for prompt in &parcel.config.inline_prompts {
        sections.push(format!("# PROMPT\n\n{}", prompt.trim_end()));
    }

    Ok(sections.join("\n\n"))
}

pub fn list_local_tools(parcel: &LoadedParcel) -> Vec<LocalToolSpec> {
    parcel
        .config
        .tools
        .iter()
        .filter_map(|tool| match tool {
            ToolConfig::Local(local) => Some(LocalToolSpec {
                alias: local.alias.clone(),
                description: local.description.clone(),
                input_schema_packaged_path: local
                    .input_schema
                    .as_ref()
                    .map(|schema| schema.packaged_path.clone()),
                input_schema_sha256: local
                    .input_schema
                    .as_ref()
                    .map(|schema| schema.sha256.clone()),
                target: LocalToolTarget::Local {
                    packaged_path: local.packaged_path.clone(),
                    command: local.runner.command.clone(),
                    args: local.runner.args.clone(),
                },
            }),
            ToolConfig::A2a(a2a) => Some(LocalToolSpec {
                alias: a2a.alias.clone(),
                description: a2a.description.clone(),
                input_schema_packaged_path: a2a
                    .input_schema
                    .as_ref()
                    .map(|schema| schema.packaged_path.clone()),
                input_schema_sha256: a2a
                    .input_schema
                    .as_ref()
                    .map(|schema| schema.sha256.clone()),
                target: LocalToolTarget::A2a {
                    endpoint_url: a2a.url.clone(),
                    endpoint_mode: a2a.endpoint_mode,
                    auth_secret_name: a2a.auth.as_ref().map(|auth| auth.secret_name.clone()),
                    auth_scheme: a2a.auth.as_ref().map(|auth| auth.scheme),
                    auth_header_name: a2a.auth.as_ref().and_then(|auth| auth.header_name.clone()),
                    expected_agent_name: a2a.expected_agent_name.clone(),
                    expected_card_sha256: a2a.expected_card_sha256.clone(),
                },
            }),
            _ => None,
        })
        .collect()
}

pub fn list_native_builtin_tools(parcel: &LoadedParcel) -> Vec<BuiltinToolSpec> {
    parcel
        .config
        .tools
        .iter()
        .filter_map(|tool| match tool {
            ToolConfig::Builtin(builtin) => builtin_memory_tool_spec(builtin),
            _ => None,
        })
        .collect()
}

fn builtin_memory_tool_spec(tool: &BuiltinToolConfig) -> Option<BuiltinToolSpec> {
    let input_schema = builtin_memory_tool_schema(&tool.capability)?;
    Some(BuiltinToolSpec {
        capability: tool.capability.clone(),
        description: tool.description.clone(),
        input_schema,
    })
}

fn builtin_memory_tool_schema(capability: &str) -> Option<serde_json::Value> {
    match capability {
        "memory_get" => Some(serde_json::json!({
            "type": "object",
            "properties": {
                "namespace": { "type": "string" },
                "key": { "type": "string" }
            },
            "required": ["key"],
            "additionalProperties": false
        })),
        "memory_put" => Some(serde_json::json!({
            "type": "object",
            "properties": {
                "namespace": { "type": "string" },
                "key": { "type": "string" },
                "value": { "type": "string" }
            },
            "required": ["key", "value"],
            "additionalProperties": false
        })),
        "memory_delete" => Some(serde_json::json!({
            "type": "object",
            "properties": {
                "namespace": { "type": "string" },
                "key": { "type": "string" }
            },
            "required": ["key"],
            "additionalProperties": false
        })),
        "memory_list" => Some(serde_json::json!({
            "type": "object",
            "properties": {
                "namespace": { "type": "string" },
                "prefix": { "type": "string" }
            },
            "additionalProperties": false
        })),
        _ => None,
    }
}

fn builtin_memory_tool_description(tool: &BuiltinToolSpec) -> String {
    tool.description
        .clone()
        .unwrap_or_else(|| match tool.capability.as_str() {
            "memory_get" => {
                "Read a value from the configured Dispatch memory mount by key.".to_string()
            }
            "memory_put" => {
                "Store or update a value in the configured Dispatch memory mount.".to_string()
            }
            "memory_delete" => {
                "Delete a value from the configured Dispatch memory mount by key.".to_string()
            }
            "memory_list" => {
                "List stored values from the configured Dispatch memory mount by prefix."
                    .to_string()
            }
            _ => format!("Dispatch builtin capability `{}`.", tool.capability),
        })
}

pub fn run_local_tool(
    parcel: &LoadedParcel,
    tool_name: &str,
    input: Option<&str>,
) -> Result<ToolRunResult, CourierError> {
    run_local_tool_with_env(parcel, tool_name, input, process_env_lookup)
}

fn run_local_tool_with_env<F>(
    parcel: &LoadedParcel,
    tool_name: &str,
    input: Option<&str>,
    env_lookup: F,
) -> Result<ToolRunResult, CourierError>
where
    F: FnMut(&str) -> Option<String> + Copy,
{
    let tool = resolve_local_tool_with_env(parcel, tool_name, env_lookup)?;

    execute_local_tool_with_env(parcel, &tool, input, None, env_lookup)
}

fn resolve_local_tool(
    parcel: &LoadedParcel,
    tool_name: &str,
) -> Result<LocalToolSpec, CourierError> {
    resolve_local_tool_with_env(parcel, tool_name, process_env_lookup)
}

fn resolve_local_tool_with_env<F>(
    parcel: &LoadedParcel,
    tool_name: &str,
    env_lookup: F,
) -> Result<LocalToolSpec, CourierError>
where
    F: FnMut(&str) -> Option<String>,
{
    validate_required_secrets_with(parcel, env_lookup)?;
    list_local_tools(parcel)
        .into_iter()
        .find(|tool| tool.matches_name(tool_name))
        .ok_or_else(|| CourierError::UnknownLocalTool {
            tool: tool_name.to_string(),
        })
}

fn validate_required_secrets(parcel: &LoadedParcel) -> Result<(), CourierError> {
    validate_required_secrets_with(parcel, process_env_lookup)
}

fn validate_required_secrets_with<F>(
    parcel: &LoadedParcel,
    mut env_lookup: F,
) -> Result<(), CourierError>
where
    F: FnMut(&str) -> Option<String>,
{
    for secret in &parcel.config.secrets {
        if secret.required && env_lookup(&secret.name).is_none() {
            return Err(CourierError::MissingSecret {
                name: secret.name.clone(),
            });
        }
    }

    Ok(())
}

fn process_env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn ensure_mounts_supported(
    courier_name: &str,
    mounts: &[MountConfig],
    supported_mounts: &[MountKind],
) -> Result<(), CourierError> {
    for mount in mounts {
        if !supported_mounts.contains(&mount.kind) {
            return Err(CourierError::UnsupportedMount {
                courier: courier_name.to_string(),
                kind: mount.kind,
                driver: mount.driver.clone(),
            });
        }
    }
    Ok(())
}

fn resolve_builtin_mounts(
    parcel: &LoadedParcel,
    courier_name: &str,
    session_id: &str,
) -> Result<Vec<ResolvedMount>, CourierError> {
    let mut mounts = Vec::with_capacity(parcel.config.mounts.len());
    let parcel_state_root = resolve_parcel_state_root(parcel);
    let session_state_root = parcel_state_root.join("sessions").join(session_id);

    for mount in &parcel.config.mounts {
        let resolved = match (mount.kind, mount.driver.as_str()) {
            (MountKind::Session, "memory") => ResolvedMount {
                kind: mount.kind,
                driver: mount.driver.clone(),
                target_path: format!("dispatch://session/{session_id}"),
                metadata: BTreeMap::from([("storage".to_string(), "memory".to_string())]),
            },
            (MountKind::Session, "sqlite") => {
                let path = session_state_root.join("session.sqlite");
                ensure_parent_dir(&path)?;
                touch_file(&path)?;
                ResolvedMount {
                    kind: mount.kind,
                    driver: mount.driver.clone(),
                    target_path: path.display().to_string(),
                    metadata: BTreeMap::new(),
                }
            }
            (MountKind::Memory, "none") => ResolvedMount {
                kind: mount.kind,
                driver: mount.driver.clone(),
                target_path: "dispatch://memory/none".to_string(),
                metadata: BTreeMap::new(),
            },
            (MountKind::Memory, "sqlite") => {
                let path = parcel_state_root.join("memory.sqlite");
                ensure_parent_dir(&path)?;
                touch_file(&path)?;
                ResolvedMount {
                    kind: mount.kind,
                    driver: mount.driver.clone(),
                    target_path: path.display().to_string(),
                    metadata: BTreeMap::new(),
                }
            }
            (MountKind::Artifacts, "local") => {
                let path = parcel_state_root.join("artifacts");
                fs::create_dir_all(&path).map_err(|source| CourierError::CreateDir {
                    path: path.display().to_string(),
                    source,
                })?;
                ResolvedMount {
                    kind: mount.kind,
                    driver: mount.driver.clone(),
                    target_path: path.display().to_string(),
                    metadata: BTreeMap::new(),
                }
            }
            _ => {
                return Err(CourierError::UnsupportedMount {
                    courier: courier_name.to_string(),
                    kind: mount.kind,
                    driver: mount.driver.clone(),
                });
            }
        };
        mounts.push(resolved);
    }

    Ok(mounts)
}

fn resolve_parcel_state_root(parcel: &LoadedParcel) -> PathBuf {
    if let Some(root) = std::env::var_os("DISPATCH_STATE_ROOT") {
        return PathBuf::from(root).join(&parcel.config.digest);
    }

    let parcel_dir = parcel.parcel_dir.as_path();
    if let Some(parent) = parcel_dir.parent()
        && parent.file_name().is_some_and(|name| name == "parcels")
        && let Some(dispatch_root) = parent.parent()
    {
        return dispatch_root.join("state").join(&parcel.config.digest);
    }

    parcel_dir
        .parent()
        .unwrap_or(parcel_dir)
        .join(".dispatch-state")
        .join(&parcel.config.digest)
}

fn validate_parcel_schema(
    manifest_path: &Path,
    manifest_json: &serde_json::Value,
) -> Result<(), CourierError> {
    let validator = parcel_schema_validator();
    let mut errors = validator.iter_errors(manifest_json);
    if let Some(first) = errors.next() {
        let mut messages = vec![format_schema_error(&first)];
        for error in errors.take(7) {
            messages.push(format_schema_error(&error));
        }
        return Err(CourierError::InvalidParcelSchema {
            path: manifest_path.display().to_string(),
            message: messages.join("; "),
        });
    }
    Ok(())
}

fn parcel_schema_validator() -> &'static Validator {
    PARCEL_SCHEMA_VALIDATOR.get_or_init(|| {
        let schema = serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../schemas/parcel.v1.json"
        ))
        .expect("embedded parcel schema must be valid JSON");
        jsonschema::validator_for(&schema).expect("embedded parcel schema must compile")
    })
}

fn format_schema_error(error: &jsonschema::ValidationError<'_>) -> String {
    let path = error.instance_path().to_string();
    if path.is_empty() {
        error.to_string()
    } else {
        format!("{path}: {error}")
    }
}

fn ensure_parent_dir(path: &Path) -> Result<(), CourierError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| CourierError::CreateDir {
            path: parent.display().to_string(),
            source,
        })?;
    }
    Ok(())
}

fn touch_file(path: &Path) -> Result<(), CourierError> {
    if path.exists() {
        return Ok(());
    }
    fs::write(path, []).map_err(|source| CourierError::WriteFile {
        path: path.display().to_string(),
        source,
    })
}

fn persist_session_mounts(session: &CourierSession) -> Result<(), CourierError> {
    for mount in &session.resolved_mounts {
        if mount.kind == MountKind::Session && mount.driver == "sqlite" {
            persist_session_sqlite(Path::new(&mount.target_path), session)?;
        }
    }
    Ok(())
}

fn persist_session_sqlite(path: &Path, session: &CourierSession) -> Result<(), CourierError> {
    let connection = Connection::open(path).map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "open",
        source,
    })?;
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS dispatch_sessions (
                session_id TEXT PRIMARY KEY,
                parcel_digest TEXT NOT NULL,
                entrypoint TEXT,
                turn_count INTEGER NOT NULL,
                payload_json TEXT NOT NULL
            );",
        )
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "create_session_table",
            source,
        })?;
    let payload = serde_json::to_string(session)
        .map_err(|error| CourierError::SerializeSession(error.to_string()))?;
    connection
        .execute(
            concat!(
                "INSERT INTO dispatch_sessions ",
                "(session_id, parcel_digest, entrypoint, turn_count, payload_json) ",
                "VALUES (?1, ?2, ?3, ?4, ?5) ",
                "ON CONFLICT(session_id) DO UPDATE SET ",
                "parcel_digest = excluded.parcel_digest, ",
                "entrypoint = excluded.entrypoint, ",
                "turn_count = excluded.turn_count, ",
                "payload_json = excluded.payload_json"
            ),
            params![
                session.id,
                session.parcel_digest,
                session.entrypoint,
                session.turn_count as i64,
                payload,
            ],
        )
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "upsert_session",
            source,
        })?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct MemoryEntry {
    namespace: String,
    key: String,
    value: String,
    updated_at: u64,
}

fn memory_mount_path(session: &CourierSession) -> Option<&Path> {
    session
        .resolved_mounts
        .iter()
        .find(|mount| mount.kind == MountKind::Memory && mount.driver == "sqlite")
        .map(|mount| Path::new(&mount.target_path))
}

fn require_memory_mount_path(session: &CourierSession) -> Result<&Path, CourierError> {
    memory_mount_path(session).ok_or_else(|| CourierError::MissingMemoryMount {
        parcel_digest: session.parcel_digest.clone(),
    })
}

fn ensure_memory_sqlite(connection: &Connection, path: &Path) -> Result<(), CourierError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS dispatch_memory (
                parcel_digest TEXT NOT NULL,
                namespace TEXT NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY(parcel_digest, namespace, key)
            );",
        )
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "create_memory_table",
            source,
        })
}

fn current_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn memory_get(
    session: &CourierSession,
    namespace: &str,
    key: &str,
) -> Result<Option<MemoryEntry>, CourierError> {
    let path = require_memory_mount_path(session)?;
    let connection = Connection::open(path).map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "open_memory_get",
        source,
    })?;
    ensure_memory_sqlite(&connection, path)?;
    connection
        .query_row(
            concat!(
                "SELECT namespace, key, value, updated_at ",
                "FROM dispatch_memory ",
                "WHERE parcel_digest = ?1 AND namespace = ?2 AND key = ?3"
            ),
            params![session.parcel_digest, namespace, key],
            |row| {
                Ok(MemoryEntry {
                    namespace: row.get(0)?,
                    key: row.get(1)?,
                    value: row.get(2)?,
                    updated_at: row.get::<_, i64>(3)? as u64,
                })
            },
        )
        .map(Some)
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            source => Err(CourierError::SqliteMount {
                path: path.display().to_string(),
                operation: "query_memory_get",
                source,
            }),
        })
}

fn memory_put(
    session: &CourierSession,
    namespace: &str,
    key: &str,
    value: &str,
) -> Result<bool, CourierError> {
    let path = require_memory_mount_path(session)?;
    let mut connection = Connection::open(path).map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "open_memory_put",
        source,
    })?;
    ensure_memory_sqlite(&connection, path)?;
    let tx = connection
        .transaction()
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "begin_memory_put",
            source,
        })?;
    let existed = tx
        .query_row(
            concat!(
                "SELECT EXISTS(",
                "SELECT 1 FROM dispatch_memory ",
                "WHERE parcel_digest = ?1 AND namespace = ?2 AND key = ?3",
                ")"
            ),
            params![session.parcel_digest, namespace, key],
            |row| row.get::<_, i64>(0),
        )
        .map(|value| value != 0)
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "query_memory_put_exists",
            source,
        })?;
    tx.execute(
        concat!(
            "INSERT INTO dispatch_memory ",
            "(parcel_digest, namespace, key, value, updated_at) ",
            "VALUES (?1, ?2, ?3, ?4, ?5) ",
            "ON CONFLICT(parcel_digest, namespace, key) DO UPDATE SET ",
            "value = excluded.value, ",
            "updated_at = excluded.updated_at"
        ),
        params![
            session.parcel_digest,
            namespace,
            key,
            value,
            current_unix_timestamp() as i64,
        ],
    )
    .map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "upsert_memory_put",
        source,
    })?;
    tx.commit().map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "commit_memory_put",
        source,
    })?;
    Ok(existed)
}

fn memory_delete(
    session: &CourierSession,
    namespace: &str,
    key: &str,
) -> Result<bool, CourierError> {
    let path = require_memory_mount_path(session)?;
    let connection = Connection::open(path).map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "open_memory_delete",
        source,
    })?;
    ensure_memory_sqlite(&connection, path)?;
    let deleted = connection
        .execute(
            concat!(
                "DELETE FROM dispatch_memory ",
                "WHERE parcel_digest = ?1 AND namespace = ?2 AND key = ?3"
            ),
            params![session.parcel_digest, namespace, key],
        )
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "delete_memory_entry",
            source,
        })?;
    Ok(deleted > 0)
}

fn memory_list(
    session: &CourierSession,
    namespace: &str,
    prefix: Option<&str>,
) -> Result<Vec<MemoryEntry>, CourierError> {
    let path = require_memory_mount_path(session)?;
    let connection = Connection::open(path).map_err(|source| CourierError::SqliteMount {
        path: path.display().to_string(),
        operation: "open_memory_list",
        source,
    })?;
    ensure_memory_sqlite(&connection, path)?;
    let prefix_like = escape_sql_like_prefix(prefix.unwrap_or_default());
    let mut statement = connection
        .prepare(concat!(
            "SELECT namespace, key, value, updated_at ",
            "FROM dispatch_memory ",
            "WHERE parcel_digest = ?1 AND namespace = ?2 AND key LIKE ?3 ESCAPE '\\' ",
            "ORDER BY key ASC"
        ))
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "prepare_memory_list",
            source,
        })?;
    let rows = statement
        .query_map(
            params![session.parcel_digest, namespace, prefix_like],
            |row| {
                Ok(MemoryEntry {
                    namespace: row.get(0)?,
                    key: row.get(1)?,
                    value: row.get(2)?,
                    updated_at: row.get::<_, i64>(3)? as u64,
                })
            },
        )
        .map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "query_memory_list",
            source,
        })?;
    let mut entries = Vec::new();
    for entry in rows {
        entries.push(entry.map_err(|source| CourierError::SqliteMount {
            path: path.display().to_string(),
            operation: "read_memory_list",
            source,
        })?);
    }
    Ok(entries)
}

fn escape_sql_like_prefix(prefix: &str) -> String {
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

fn parse_memory_ref(token: &str) -> (&str, &str) {
    match token.split_once(':') {
        Some((namespace, key)) if !namespace.is_empty() && !key.is_empty() => (namespace, key),
        _ => ("default", token),
    }
}

fn default_memory_namespace() -> String {
    "default".to_string()
}

#[derive(Debug, Deserialize)]
struct BuiltinMemoryGetInput {
    #[serde(default = "default_memory_namespace")]
    namespace: String,
    key: String,
}

#[derive(Debug, Deserialize)]
struct BuiltinMemoryPutInput {
    #[serde(default = "default_memory_namespace")]
    namespace: String,
    key: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct BuiltinMemoryListInput {
    #[serde(default = "default_memory_namespace")]
    namespace: String,
    prefix: Option<String>,
}

fn parse_builtin_tool_input<T>(tool: &str, input: &str) -> Result<T, CourierError>
where
    T: DeserializeOwned,
{
    serde_json::from_str::<T>(input).map_err(|error| CourierError::InvalidBuiltinToolInput {
        tool: tool.to_string(),
        message: error.to_string(),
    })
}

fn execute_builtin_tool(
    session: &CourierSession,
    capability: &str,
    input: &str,
) -> Result<ToolRunResult, CourierError> {
    let stdout = match capability {
        "memory_get" => {
            let input: BuiltinMemoryGetInput = parse_builtin_tool_input(capability, input)?;
            match memory_get(session, &input.namespace, &input.key)? {
                Some(entry) => format!("{}:{} = {}", entry.namespace, entry.key, entry.value),
                None => format!("No memory entry for {}:{}", input.namespace, input.key),
            }
        }
        "memory_put" => {
            let input: BuiltinMemoryPutInput = parse_builtin_tool_input(capability, input)?;
            let replaced = memory_put(session, &input.namespace, &input.key, &input.value)?;
            if replaced {
                format!("Updated memory {}:{}", input.namespace, input.key)
            } else {
                format!("Stored memory {}:{}", input.namespace, input.key)
            }
        }
        "memory_delete" => {
            let input: BuiltinMemoryGetInput = parse_builtin_tool_input(capability, input)?;
            let deleted = memory_delete(session, &input.namespace, &input.key)?;
            if deleted {
                format!("Deleted memory {}:{}", input.namespace, input.key)
            } else {
                format!("No memory entry for {}:{}", input.namespace, input.key)
            }
        }
        "memory_list" => {
            let input: BuiltinMemoryListInput = parse_builtin_tool_input(capability, input)?;
            let entries = memory_list(session, &input.namespace, input.prefix.as_deref())?;
            if entries.is_empty() {
                format!("No memory entries in namespace `{}`.", input.namespace)
            } else {
                entries
                    .into_iter()
                    .map(|entry| format!("{}:{} = {}", entry.namespace, entry.key, entry.value))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        _ => {
            return Err(CourierError::InvalidBuiltinToolInput {
                tool: capability.to_string(),
                message: "unsupported builtin capability for native tool execution".to_string(),
            });
        }
    };

    Ok(ToolRunResult {
        tool: capability.to_string(),
        command: "dispatch-builtin".to_string(),
        args: vec![capability.to_string()],
        exit_code: 0,
        stdout,
        stderr: String::new(),
    })
}

fn handle_native_memory_command(
    session: &CourierSession,
    command: &str,
) -> Result<String, CourierError> {
    let Some(rest) = command.strip_prefix("/memory") else {
        return Ok("Usage: /memory <put|get|delete|list> ...".to_string());
    };
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        return Ok("Usage: /memory <put|get|delete|list> ...".to_string());
    }
    if memory_mount_path(session).is_none() {
        return Ok("No sqlite memory mount is configured for this parcel.".to_string());
    }

    let mut parts = trimmed.splitn(3, ' ');
    let verb = parts.next().unwrap_or_default();
    match verb {
        "put" => {
            let key_ref = parts.next().unwrap_or_default().trim();
            let value = parts.next().unwrap_or_default().trim();
            if key_ref.is_empty() || value.is_empty() {
                return Ok("Usage: /memory put <key|namespace:key> <value>".to_string());
            }
            let (namespace, key) = parse_memory_ref(key_ref);
            let replaced = memory_put(session, namespace, key, value)?;
            Ok(if replaced {
                format!("Updated memory {}:{}", namespace, key)
            } else {
                format!("Stored memory {}:{}", namespace, key)
            })
        }
        "get" => {
            let key_ref = parts.next().unwrap_or_default().trim();
            if key_ref.is_empty() {
                return Ok("Usage: /memory get <key|namespace:key>".to_string());
            }
            let (namespace, key) = parse_memory_ref(key_ref);
            match memory_get(session, namespace, key)? {
                Some(entry) => Ok(format!(
                    "{}:{} = {}",
                    entry.namespace, entry.key, entry.value
                )),
                None => Ok(format!("No memory entry for {}:{}", namespace, key)),
            }
        }
        "delete" => {
            let key_ref = parts.next().unwrap_or_default().trim();
            if key_ref.is_empty() {
                return Ok("Usage: /memory delete <key|namespace:key>".to_string());
            }
            let (namespace, key) = parse_memory_ref(key_ref);
            let deleted = memory_delete(session, namespace, key)?;
            Ok(if deleted {
                format!("Deleted memory {}:{}", namespace, key)
            } else {
                format!("No memory entry for {}:{}", namespace, key)
            })
        }
        "list" => {
            let key_ref = parts.next().unwrap_or_default().trim();
            let (namespace, prefix) = if key_ref.is_empty() {
                ("default", None)
            } else {
                let (namespace, key) = parse_memory_ref(key_ref);
                (namespace, Some(key))
            };
            let entries = memory_list(session, namespace, prefix)?;
            if entries.is_empty() {
                return Ok(format!("No memory entries in namespace `{namespace}`."));
            }
            Ok(entries
                .into_iter()
                .map(|entry| format!("{}:{} = {}", entry.namespace, entry.key, entry.value))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        _ => Ok("Usage: /memory <put|get|delete|list> ...".to_string()),
    }
}

fn forwarded_tool_env(parcel: &LoadedParcel, input: Option<&str>) -> Vec<(String, String)> {
    forwarded_tool_env_with(parcel, input, process_env_lookup)
}

fn forwarded_tool_env_with<F>(
    parcel: &LoadedParcel,
    input: Option<&str>,
    mut env_lookup: F,
) -> Vec<(String, String)>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut env = Vec::new();
    for var in ["PATH", "HOME", "TMPDIR", "TEMP", "TMP"] {
        if let Some(value) = env_lookup(var) {
            env.push((var.to_string(), value));
        }
    }
    for entry in &parcel.config.env {
        env.push((entry.name.clone(), entry.value.clone()));
    }
    for secret in &parcel.config.secrets {
        if let Some(value) = env_lookup(&secret.name) {
            env.push((secret.name.clone(), value));
        }
    }
    if let Some(input) = input {
        env.push(("TOOL_INPUT".to_string(), input.to_string()));
    }
    env
}

fn configured_timeout_duration(
    timeouts: &[crate::manifest::TimeoutSpec],
    scope: &str,
) -> Result<Option<Duration>, CourierError> {
    let Some(timeout) = timeouts
        .iter()
        .rev()
        .find(|timeout| timeout.scope.eq_ignore_ascii_case(scope))
    else {
        return Ok(None);
    };
    parse_timeout_duration(&timeout.duration)
        .map(Some)
        .ok_or_else(|| CourierError::InvalidTimeoutSpec {
            scope: timeout.scope.clone(),
            duration: timeout.duration.clone(),
        })
}

fn parse_timeout_duration(raw: &str) -> Option<Duration> {
    let trimmed = raw.trim();
    let (value, unit) = if let Some(value) = trimmed.strip_suffix("ms") {
        (value, "ms")
    } else if let Some(value) = trimmed.strip_suffix('s') {
        (value, "s")
    } else if let Some(value) = trimmed.strip_suffix('m') {
        (value, "m")
    } else if let Some(value) = trimmed.strip_suffix('h') {
        (value, "h")
    } else {
        return None;
    };
    let amount = value.trim().parse::<u64>().ok()?;
    match unit {
        "ms" => Some(Duration::from_millis(amount)),
        "s" => Some(Duration::from_secs(amount)),
        "m" => Some(Duration::from_secs(amount.saturating_mul(60))),
        "h" => Some(Duration::from_secs(amount.saturating_mul(60 * 60))),
        _ => None,
    }
}

fn wait_for_tool_output(
    mut child: Child,
    tool: &str,
    timeout_spec: Option<(&str, Duration)>,
) -> Result<std::process::Output, CourierError> {
    if let Some((timeout_label, timeout)) = timeout_spec {
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(CourierError::ToolTimedOut {
                        tool: tool.to_string(),
                        timeout: timeout_label.to_string(),
                    });
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(source) => {
                    return Err(CourierError::WaitTool {
                        tool: tool.to_string(),
                        source,
                    });
                }
            }
        }
    }
    child
        .wait_with_output()
        .map_err(|source| CourierError::WaitTool {
            tool: tool.to_string(),
            source,
        })
}

fn ensure_run_timeout_budget(
    session: &CourierSession,
    timeouts: &[crate::manifest::TimeoutSpec],
) -> Result<(), CourierError> {
    let Some((timeout_duration, timeout_literal)) =
        configured_timeout_duration_with_literal(timeouts, "RUN")?
    else {
        return Ok(());
    };
    let limit_ms = u64::try_from(timeout_duration.as_millis()).unwrap_or(u64::MAX);
    if session.elapsed_ms >= limit_ms {
        return Err(CourierError::RunTimedOut {
            session_id: session.id.clone(),
            timeout: timeout_literal,
        });
    }
    Ok(())
}

fn configured_timeout_duration_with_literal(
    timeouts: &[crate::manifest::TimeoutSpec],
    scope: &str,
) -> Result<Option<(Duration, String)>, CourierError> {
    let Some(timeout_spec) = timeouts
        .iter()
        .rev()
        .find(|timeout| timeout.scope.eq_ignore_ascii_case(scope))
    else {
        return Ok(None);
    };
    let Some(timeout) = parse_timeout_duration(&timeout_spec.duration) else {
        return Err(CourierError::InvalidTimeoutSpec {
            scope: timeout_spec.scope.clone(),
            duration: timeout_spec.duration.clone(),
        });
    };
    Ok(Some((timeout, timeout_spec.duration.clone())))
}

fn remaining_run_budget_duration(
    session: &CourierSession,
    timeouts: &[crate::manifest::TimeoutSpec],
) -> Result<Option<Duration>, CourierError> {
    let Some((run_timeout, _)) = configured_timeout_duration_with_literal(timeouts, "RUN")? else {
        return Ok(None);
    };
    let limit_ms = u64::try_from(run_timeout.as_millis()).unwrap_or(u64::MAX);
    let remaining_ms = limit_ms.saturating_sub(session.elapsed_ms);
    Ok(Some(Duration::from_millis(remaining_ms)))
}

fn run_timeout_deadline(
    session: &CourierSession,
    timeouts: &[crate::manifest::TimeoutSpec],
) -> Result<Option<Instant>, CourierError> {
    Ok(remaining_run_budget_duration(session, timeouts)?.map(|duration| Instant::now() + duration))
}

fn remaining_deadline_duration(deadline: Option<Instant>) -> Option<Duration> {
    deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()))
}

fn effective_timeout_spec(
    timeouts: &[crate::manifest::TimeoutSpec],
    scope: &str,
    run_deadline: Option<Instant>,
) -> Result<Option<(&'static str, Duration)>, CourierError> {
    let configured = configured_timeout_duration_with_literal(timeouts, scope)?;
    let remaining_run = remaining_deadline_duration(run_deadline);
    Ok(match (configured, remaining_run) {
        (Some((configured_duration, _)), Some(remaining_run_duration)) => {
            if remaining_run_duration < configured_duration {
                Some(("RUN", remaining_run_duration))
            } else {
                Some((scope_to_timeout_label(scope), configured_duration))
            }
        }
        (Some((configured_duration, _)), None) => {
            Some((scope_to_timeout_label(scope), configured_duration))
        }
        (None, Some(remaining_run_duration)) => Some(("RUN", remaining_run_duration)),
        (None, None) => None,
    })
}

fn scope_to_timeout_label(scope: &str) -> &'static str {
    match scope {
        "TOOL" => "TOOL",
        "LLM" => "LLM",
        "RUN" => "RUN",
        _ => "TIMEOUT",
    }
}

fn operation_counts_toward_run_budget(operation: &CourierOperation) -> bool {
    match operation {
        CourierOperation::InvokeTool { .. }
        | CourierOperation::Chat { .. }
        | CourierOperation::Job { .. }
        | CourierOperation::Heartbeat { .. } => true,
        CourierOperation::ResolvePrompt | CourierOperation::ListLocalTools => false,
    }
}

fn apply_session_run_elapsed(session: &mut CourierSession, started_at: Instant) {
    let elapsed_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    session.elapsed_ms = session.elapsed_ms.saturating_add(elapsed_ms);
}

// Execute a tool whose spec has already been resolved. Callers are responsible
// for validating required secrets before calling this function.
fn execute_local_tool(
    parcel: &LoadedParcel,
    tool: &LocalToolSpec,
    input: Option<&str>,
) -> Result<ToolRunResult, CourierError> {
    execute_local_tool_with_env(parcel, tool, input, None, process_env_lookup)
}

fn execute_local_tool_with_env<F>(
    parcel: &LoadedParcel,
    tool: &LocalToolSpec,
    input: Option<&str>,
    run_deadline: Option<Instant>,
    env_lookup: F,
) -> Result<ToolRunResult, CourierError>
where
    F: FnMut(&str) -> Option<String>,
{
    if matches!(tool.target, LocalToolTarget::A2a { .. }) {
        let timeout_spec = effective_timeout_spec(&parcel.config.timeouts, "TOOL", run_deadline)?;
        return execute_a2a_tool_with_env(tool, input, env_lookup, timeout_spec);
    }

    let packaged_path = tool.packaged_path().expect("local tool path");
    let tool_path = parcel.parcel_dir.join("context").join(packaged_path);
    if !tool_path.exists() {
        return Err(CourierError::MissingToolFile {
            tool: tool.alias.clone(),
            path: tool_path.display().to_string(),
        });
    }

    let mut command = Command::new(tool.command());
    command.args(tool.args());
    if tool.command() == packaged_path {
        command.current_dir(parcel.parcel_dir.join("context"));
    } else {
        command.arg(&tool_path);
        command.current_dir(parcel.parcel_dir.join("context"));
    }
    command.stdin(std::process::Stdio::piped());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    // Clear the inherited environment so undeclared variables from the parent
    // process (API keys, personal config, etc.) do not leak into tool
    // subprocesses. Only declared ENV vars, the image's required secrets, and
    // the minimal system variables needed to locate interpreters are forwarded.
    command.env_clear();
    for (name, value) in forwarded_tool_env_with(parcel, input, env_lookup) {
        command.env(name, value);
    }

    let mut child = command.spawn().map_err(|source| CourierError::SpawnTool {
        tool: tool.alias.clone(),
        source,
    })?;

    if let Some(input) = input
        && let Some(stdin) = child.stdin.as_mut()
    {
        use std::io::Write as _;
        stdin
            .write_all(input.as_bytes())
            .map_err(|source| CourierError::WriteToolInput {
                tool: tool.alias.clone(),
                source,
            })?;
    }

    let output = wait_for_tool_output(
        child,
        &tool.alias,
        effective_timeout_spec(&parcel.config.timeouts, "TOOL", run_deadline)?,
    )?;

    Ok(ToolRunResult {
        tool: tool.alias.clone(),
        command: tool.command().to_string(),
        args: tool.args().to_vec(),
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

fn execute_local_tool_in_docker(
    parcel: &LoadedParcel,
    tool: &LocalToolSpec,
    input: Option<&str>,
    courier: &DockerCourier,
    run_deadline: Option<Instant>,
) -> Result<ToolRunResult, CourierError> {
    if matches!(tool.target, LocalToolTarget::A2a { .. }) {
        let timeout_spec = effective_timeout_spec(&parcel.config.timeouts, "TOOL", run_deadline)?;
        return execute_a2a_tool_with_env(tool, input, process_env_lookup, timeout_spec);
    }

    let packaged_path = tool.packaged_path().expect("local tool path");
    let tool_path = parcel.parcel_dir.join("context").join(packaged_path);
    if !tool_path.exists() {
        return Err(CourierError::MissingToolFile {
            tool: tool.alias.clone(),
            path: tool_path.display().to_string(),
        });
    }

    let parcel_root =
        parcel
            .parcel_dir
            .canonicalize()
            .map_err(|source| CourierError::ReadFile {
                path: parcel.parcel_dir.display().to_string(),
                source,
            })?;
    let mount_arg = format!("{}:/workspace:ro", parcel_root.display());
    let mut command = Command::new(&courier.docker_bin);
    command
        .arg("run")
        .arg("--rm")
        .arg("-i")
        .arg("--workdir")
        .arg("/workspace/context")
        .arg("-v")
        .arg(mount_arg);
    for (name, value) in forwarded_tool_env(parcel, input) {
        command.arg("-e").arg(format!("{name}={value}"));
    }
    command.arg(&courier.helper_image);
    command.arg(tool.command());
    command.args(tool.args());
    if tool.command() != packaged_path {
        command.arg(packaged_path);
    }
    command.stdin(std::process::Stdio::piped());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    let mut child = command.spawn().map_err(|source| CourierError::SpawnTool {
        tool: tool.alias.clone(),
        source,
    })?;

    if let Some(input) = input
        && let Some(stdin) = child.stdin.as_mut()
    {
        use std::io::Write as _;
        stdin
            .write_all(input.as_bytes())
            .map_err(|source| CourierError::WriteToolInput {
                tool: tool.alias.clone(),
                source,
            })?;
    }

    let output = wait_for_tool_output(
        child,
        &tool.alias,
        effective_timeout_spec(&parcel.config.timeouts, "TOOL", run_deadline)?,
    )?;

    Ok(ToolRunResult {
        tool: tool.alias.clone(),
        command: tool.command().to_string(),
        args: tool.args().to_vec(),
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

fn execute_host_local_tool(
    parcel: &LoadedParcel,
    tool: &LocalToolSpec,
    input: Option<&str>,
    runner: HostToolRunner<'_>,
    run_deadline: Option<Instant>,
) -> Result<ToolRunResult, CourierError> {
    match runner {
        HostToolRunner::Native if run_deadline.is_none() => execute_local_tool(parcel, tool, input),
        HostToolRunner::Native => {
            execute_local_tool_with_env(parcel, tool, input, run_deadline, process_env_lookup)
        }
        HostToolRunner::Docker(courier) => {
            execute_local_tool_in_docker(parcel, tool, input, courier, run_deadline)
        }
    }
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
        image: &LoadedParcel,
    ) -> impl Future<Output = Result<(), CourierError>> + Send {
        let result = validate_native_parcel(image);
        async move { result }
    }

    fn inspect(
        &self,
        image: &LoadedParcel,
    ) -> impl Future<Output = Result<CourierInspection, CourierError>> + Send {
        let inspection = CourierInspection {
            courier_id: self.id().to_string(),
            kind: self.kind(),
            entrypoint: image.config.entrypoint.clone(),
            required_secrets: image
                .config
                .secrets
                .iter()
                .map(|secret| secret.name.clone())
                .collect(),
            mounts: image.config.mounts.clone(),
            local_tools: list_local_tools(image),
        };
        async move { Ok(inspection) }
    }

    fn open_session(
        &self,
        image: &LoadedParcel,
    ) -> impl Future<Output = Result<CourierSession, CourierError>> + Send {
        let validation = validate_native_parcel(image);
        let parcel_digest = image.config.digest.clone();
        let entrypoint = image.config.entrypoint.clone();
        async move {
            validation?;
            validate_required_secrets(image)?;
            let sequence = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
            let session_id = format!("native-{parcel_digest}-{sequence}");
            let session = CourierSession {
                resolved_mounts: resolve_builtin_mounts(image, "native", &session_id)?,
                id: session_id,
                parcel_digest,
                entrypoint,
                label: image.config.name.clone(),
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
        image: &LoadedParcel,
        request: CourierRequest,
    ) -> impl Future<Output = Result<CourierResponse, CourierError>> + Send {
        let courier_id = self.id().to_string();
        let operation = request.operation;
        let mut session = request.session;
        let chat_backend_override = self.chat_backend_override.clone();

        async move {
            validate_native_parcel(image)?;
            ensure_session_matches_parcel(image, &session)?;
            ensure_operation_matches_entrypoint(&session, &operation)?;
            let consumes_run_budget = operation_counts_toward_run_budget(&operation);
            if consumes_run_budget {
                ensure_run_timeout_budget(&session, &image.config.timeouts)?;
            }
            let run_deadline = if consumes_run_budget {
                run_timeout_deadline(&session, &image.config.timeouts)?
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
                            text: resolve_prompt_text(image)?,
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
                            tools: list_local_tools(image),
                        },
                        CourierEvent::Done,
                    ],
                }),
                CourierOperation::InvokeTool { invocation } => {
                    let tool = resolve_local_tool(image, &invocation.name)?;
                    let result = execute_host_local_tool(
                        image,
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
                        image,
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
                    session.history.push(ConversationMessage {
                        role: "assistant".to_string(),
                        content: chat_turn.reply.clone(),
                    });
                    if !chat_turn.streamed_reply {
                        chat_turn.events.push(CourierEvent::Message {
                            role: "assistant".to_string(),
                            content: chat_turn.reply.clone(),
                        });
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
                    image,
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
                    image,
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
        image: &LoadedParcel,
    ) -> impl Future<Output = Result<(), CourierError>> + Send {
        let reference = image.config.courier.reference().to_string();
        async move { validate_courier_reference("docker", CourierKind::Docker, &reference) }
    }

    fn inspect(
        &self,
        image: &LoadedParcel,
    ) -> impl Future<Output = Result<CourierInspection, CourierError>> + Send {
        let reference = image.config.courier.reference().to_string();
        let inspection = CourierInspection {
            courier_id: self.id().to_string(),
            kind: self.kind(),
            entrypoint: image.config.entrypoint.clone(),
            required_secrets: image
                .config
                .secrets
                .iter()
                .map(|secret| secret.name.clone())
                .collect(),
            mounts: image.config.mounts.clone(),
            local_tools: list_local_tools(image),
        };
        async move {
            validate_courier_reference("docker", CourierKind::Docker, &reference)?;
            Ok(inspection)
        }
    }

    fn open_session(
        &self,
        image: &LoadedParcel,
    ) -> impl Future<Output = Result<CourierSession, CourierError>> + Send {
        let reference = image.config.courier.reference().to_string();
        let parcel_digest = image.config.digest.clone();
        let entrypoint = image.config.entrypoint.clone();
        async move {
            validate_courier_reference("docker", CourierKind::Docker, &reference)?;
            validate_required_secrets(image)?;
            let sequence = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
            let session_id = format!("docker-{parcel_digest}-{sequence}");
            let session = CourierSession {
                resolved_mounts: resolve_builtin_mounts(image, "docker", &session_id)?,
                id: session_id,
                parcel_digest,
                entrypoint,
                label: image.config.name.clone(),
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
        image: &LoadedParcel,
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
                image.config.courier.reference(),
            )?;
            ensure_session_matches_parcel(image, &session)?;
            ensure_operation_matches_entrypoint(&session, &operation)?;
            let consumes_run_budget = operation_counts_toward_run_budget(&operation);
            if consumes_run_budget {
                ensure_run_timeout_budget(&session, &image.config.timeouts)?;
            }
            let run_deadline = if consumes_run_budget {
                run_timeout_deadline(&session, &image.config.timeouts)?
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
                            text: resolve_prompt_text(image)?,
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
                            tools: list_local_tools(image),
                        },
                        CourierEvent::Done,
                    ],
                }),
                CourierOperation::InvokeTool { invocation } => {
                    let tool = resolve_local_tool(image, &invocation.name)?;
                    let result = execute_local_tool_in_docker(
                        image,
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
                        image,
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
                    session.history.push(ConversationMessage {
                        role: "assistant".to_string(),
                        content: chat_turn.reply.clone(),
                    });
                    if !chat_turn.streamed_reply {
                        chat_turn.events.push(CourierEvent::Message {
                            role: "assistant".to_string(),
                            content: chat_turn.reply.clone(),
                        });
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
                    image,
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
                    image,
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
        image: &LoadedParcel,
    ) -> impl Future<Output = Result<(), CourierError>> + Send {
        let courier = self.clone();
        let parcel_dir = canonical_parcel_dir(image);
        async move {
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
        image: &LoadedParcel,
    ) -> impl Future<Output = Result<CourierInspection, CourierError>> + Send {
        let courier = self.clone();
        let parcel_dir = canonical_parcel_dir(image);
        async move {
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
        image: &LoadedParcel,
    ) -> impl Future<Output = Result<CourierSession, CourierError>> + Send {
        let courier = self.clone();
        let parcel_dir = canonical_parcel_dir(image);
        async move {
            validate_required_secrets(image)?;
            let capabilities = courier.capabilities().await?;
            ensure_mounts_supported(
                &courier.manifest.name,
                image.config.mounts.as_slice(),
                &capabilities.supports_mounts,
            )?;
            let parcel_dir = parcel_dir?;
            let mut process = courier.spawn_persistent_plugin()?;
            process.write_request(
                courier.manifest.protocol_version,
                &courier.manifest.name,
                PluginRequest::OpenSession { parcel_dir },
            )?;
            match process.read_response(&courier.manifest.name)? {
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
        image: &LoadedParcel,
        request: CourierRequest,
    ) -> impl Future<Output = Result<CourierResponse, CourierError>> + Send {
        let courier = self.clone();
        let parcel_dir = canonical_parcel_dir(image);
        let operation = request.operation;
        let session = request.session;
        async move {
            ensure_session_matches_parcel(image, &session)?;
            let consumes_run_budget = operation_counts_toward_run_budget(&operation);
            if consumes_run_budget {
                ensure_run_timeout_budget(&session, &image.config.timeouts)?;
            }
            let started_at = Instant::now();
            let parcel_dir = parcel_dir?;
            let session = courier.ensure_persistent_process(&parcel_dir, session)?;
            let (session, events) = courier.run_persistent_plugin(
                session.id.clone(),
                PluginRequest::Run {
                    parcel_dir,
                    session,
                    operation,
                },
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
        write_plugin_request(
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
        let response = read_plugin_response(&mut reader, &self.manifest.name)?;
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
            stdout: BufReader::new(stdout),
            stderr,
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
        process.write_request(self.manifest.protocol_version, &self.manifest.name, request)?;
        let mut events = Vec::new();
        match read_plugin_run_completion(&mut process.stdout, &self.manifest.name, &mut events) {
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
        process.write_request(
            self.manifest.protocol_version,
            &self.manifest.name,
            PluginRequest::ResumeSession {
                parcel_dir: parcel_dir.to_string(),
                session,
            },
        )?;
        match process.read_response(&self.manifest.name) {
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

fn write_plugin_request(
    child: &mut Child,
    courier_name: &str,
    protocol_version: u32,
    request: PluginRequest,
    close_stdin: bool,
) -> Result<(), CourierError> {
    let stdin = child
        .stdin
        .as_mut()
        .ok_or_else(|| CourierError::PluginProtocol {
            courier: courier_name.to_string(),
            message: "plugin stdin was not captured".to_string(),
        })?;
    write_plugin_request_to(stdin, courier_name, protocol_version, request)?;
    if close_stdin {
        let _ = child.stdin.take();
    }
    Ok(())
}

fn write_plugin_request_to<W: std::io::Write>(
    mut writer: W,
    courier_name: &str,
    protocol_version: u32,
    request: PluginRequest,
) -> Result<(), CourierError> {
    serde_json::to_writer(
        &mut writer,
        &PluginRequestEnvelope {
            protocol_version,
            request,
        },
    )
    .map_err(|source| CourierError::PluginProtocol {
        courier: courier_name.to_string(),
        message: format!("failed to serialize plugin request: {source}"),
    })?;
    writer
        .write_all(b"\n")
        .map_err(|source| CourierError::WritePluginRequest {
            courier: courier_name.to_string(),
            source,
        })?;
    writer
        .flush()
        .map_err(|source| CourierError::WritePluginRequest {
            courier: courier_name.to_string(),
            source,
        })?;
    Ok(())
}

impl PersistentPluginProcess {
    fn write_request(
        &mut self,
        protocol_version: u32,
        courier_name: &str,
        request: PluginRequest,
    ) -> Result<(), CourierError> {
        write_plugin_request_to(&mut self.stdin, courier_name, protocol_version, request)
    }

    fn read_response(&mut self, courier_name: &str) -> Result<PluginResponse, CourierError> {
        read_plugin_response(&mut self.stdout, courier_name)
    }
}

fn read_plugin_response<R: std::io::BufRead>(
    reader: &mut R,
    courier_name: &str,
) -> Result<PluginResponse, CourierError> {
    let mut line = String::new();
    let bytes = reader
        .read_line(&mut line)
        .map_err(|source| CourierError::ReadPluginResponse {
            courier: courier_name.to_string(),
            source,
        })?;
    if bytes == 0 {
        return Err(CourierError::PluginProtocol {
            courier: courier_name.to_string(),
            message: "plugin produced no response".to_string(),
        });
    }
    serde_json::from_str(line.trim_end()).map_err(|source| CourierError::PluginProtocol {
        courier: courier_name.to_string(),
        message: format!("invalid plugin JSON: {source}"),
    })
}

fn read_plugin_run_completion<R: std::io::BufRead>(
    reader: &mut R,
    courier_name: &str,
    events: &mut Vec<CourierEvent>,
) -> Result<CourierSession, CourierError> {
    loop {
        match read_plugin_response(reader, courier_name)? {
            PluginResponse::Event { event } => events.push(event),
            PluginResponse::Done { session } => return Ok(session),
            PluginResponse::Error { error } => {
                return Err(CourierError::PluginProtocol {
                    courier: courier_name.to_string(),
                    message: format!("{}: {}", error.code, error.message),
                });
            }
            other => {
                return Err(CourierError::PluginProtocol {
                    courier: courier_name.to_string(),
                    message: format!(
                        "unexpected plugin response for `run`: {}",
                        describe_plugin_response(&other)
                    ),
                });
            }
        }
    }
}

fn shutdown_persistent_plugin_process(
    process: &mut PersistentPluginProcess,
    courier_name: &str,
    protocol_version: u32,
) -> Result<(), CourierError> {
    let _ = process.write_request(protocol_version, courier_name, PluginRequest::Shutdown);
    let _ = process.read_response(courier_name);
    let _ = process.stdin.flush();
    if process.child.try_wait().ok().flatten().is_none() {
        let _ = process.child.kill();
    }
    let mut stderr = String::new();
    use std::io::Read as _;
    let _ = process.stderr.read_to_string(&mut stderr);
    process
        .child
        .wait()
        .map_err(|source| CourierError::WaitPlugin {
            courier: courier_name.to_string(),
            source,
        })?;
    Ok(())
}

fn wait_for_plugin_exit(mut child: Child, courier_name: &str) -> Result<(), CourierError> {
    let mut stderr = String::new();
    if let Some(mut stderr_pipe) = child.stderr.take() {
        use std::io::Read as _;
        stderr_pipe.read_to_string(&mut stderr).map_err(|source| {
            CourierError::ReadPluginResponse {
                courier: courier_name.to_string(),
                source,
            }
        })?;
    }
    let status = child.wait().map_err(|source| CourierError::WaitPlugin {
        courier: courier_name.to_string(),
        source,
    })?;
    if status.success() {
        return Ok(());
    }

    Err(CourierError::PluginExit {
        courier: courier_name.to_string(),
        status: status.code().unwrap_or(-1),
        stderr: stderr.trim().to_string(),
    })
}

fn canonical_parcel_dir(parcel: &LoadedParcel) -> Result<String, CourierError> {
    parcel
        .parcel_dir
        .canonicalize()
        .map(|path| path.display().to_string())
        .map_err(|source| CourierError::ReadFile {
            path: parcel.parcel_dir.display().to_string(),
            source,
        })
}

fn describe_plugin_response(response: &PluginResponse) -> &'static str {
    match response {
        PluginResponse::Capabilities { .. } => "capabilities",
        PluginResponse::Inspection { .. } => "inspection",
        PluginResponse::Session { .. } => "session",
        PluginResponse::Ok => "ok",
        PluginResponse::Event { .. } => "event",
        PluginResponse::Done { .. } => "done",
        PluginResponse::Error { .. } => "error",
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
            ensure_session_matches_parcel(&parcel, &session)?;
            validate_wasm_component_metadata(&parcel)?;
            let consumes_run_budget = operation_counts_toward_run_budget(&operation);
            if consumes_run_budget {
                ensure_run_timeout_budget(&session, &parcel.config.timeouts)?;
            }
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
                CourierOperation::InvokeTool { .. } => Err(CourierError::UnsupportedOperation {
                    courier: "wasm".to_string(),
                    operation: "tool".to_string(),
                }),
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
        image: &LoadedParcel,
    ) -> impl Future<Output = Result<(), CourierError>> + Send {
        let courier_id = self.courier_id;
        let kind = self.kind;
        let reference = image.config.courier.reference().to_string();
        async move { validate_courier_reference(courier_id, kind, &reference) }
    }

    fn inspect(
        &self,
        image: &LoadedParcel,
    ) -> impl Future<Output = Result<CourierInspection, CourierError>> + Send {
        let courier_id = self.courier_id;
        let kind = self.kind;
        let reference = image.config.courier.reference().to_string();
        let inspection = CourierInspection {
            courier_id: courier_id.to_string(),
            kind,
            entrypoint: image.config.entrypoint.clone(),
            required_secrets: image
                .config
                .secrets
                .iter()
                .map(|secret| secret.name.clone())
                .collect(),
            mounts: image.config.mounts.clone(),
            local_tools: list_local_tools(image),
        };
        async move {
            validate_courier_reference(courier_id, kind, &reference)?;
            Ok(inspection)
        }
    }

    fn open_session(
        &self,
        image: &LoadedParcel,
    ) -> impl Future<Output = Result<CourierSession, CourierError>> + Send {
        let courier_id = self.courier_id;
        let kind = self.kind;
        let reference = image.config.courier.reference().to_string();
        let parcel_digest = image.config.digest.clone();
        let entrypoint = image.config.entrypoint.clone();
        async move {
            validate_courier_reference(courier_id, kind, &reference)?;
            validate_required_secrets(image)?;
            ensure_mounts_supported(courier_id, image.config.mounts.as_slice(), &[])?;
            let sequence = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
            let session = CourierSession {
                id: format!("{courier_id}-{parcel_digest}-{sequence}"),
                parcel_digest,
                entrypoint,
                label: image.config.name.clone(),
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
        image: &LoadedParcel,
        request: CourierRequest,
    ) -> impl Future<Output = Result<CourierResponse, CourierError>> + Send {
        let courier_id = self.courier_id.to_string();
        let kind = self.kind;
        let reference = image.config.courier.reference().to_string();
        let operation = request.operation;
        let mut session = request.session;

        async move {
            validate_courier_reference(&courier_id, kind, &reference)?;
            ensure_session_matches_parcel(image, &session)?;
            let consumes_run_budget = operation_counts_toward_run_budget(&operation);
            if consumes_run_budget {
                ensure_run_timeout_budget(&session, &image.config.timeouts)?;
            }
            session.turn_count += 1;
            let started_at = Instant::now();

            let mut response = match operation {
                CourierOperation::ResolvePrompt => Ok(CourierResponse {
                    courier_id,
                    session,
                    events: vec![
                        CourierEvent::PromptResolved {
                            text: resolve_prompt_text(image)?,
                        },
                        CourierEvent::Done,
                    ],
                }),
                CourierOperation::ListLocalTools => Ok(CourierResponse {
                    courier_id,
                    session,
                    events: vec![
                        CourierEvent::LocalToolsListed {
                            tools: list_local_tools(image),
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
    image: &LoadedParcel,
    mut session: CourierSession,
    mode: NativeTurnMode,
    input: String,
    context: HostTurnContext<'_>,
) -> Result<CourierResponse, CourierError> {
    session.history.push(ConversationMessage {
        role: "user".to_string(),
        content: input.clone(),
    });
    let mut turn = execute_host_turn(image, &session, &input, mode, context)?;
    session.history.push(ConversationMessage {
        role: "assistant".to_string(),
        content: turn.reply.clone(),
    });
    if !turn.streamed_reply {
        turn.events.push(CourierEvent::Message {
            role: "assistant".to_string(),
            content: turn.reply.clone(),
        });
    }
    turn.events.push(CourierEvent::Done);

    persist_session_mounts(&session)?;
    Ok(CourierResponse {
        courier_id: context.host_label.to_ascii_lowercase(),
        session,
        events: turn.events,
    })
}

fn validate_native_parcel(image: &LoadedParcel) -> Result<(), CourierError> {
    validate_courier_reference(
        "native",
        CourierKind::Native,
        image.config.courier.reference(),
    )
}

fn validate_wasm_component_metadata(parcel: &LoadedParcel) -> Result<(), CourierError> {
    let component =
        parcel
            .config
            .courier
            .component()
            .ok_or_else(|| CourierError::MissingCourierComponent {
                courier: "wasm".to_string(),
                parcel_digest: parcel.config.digest.clone(),
            })?;

    if component.abi != DISPATCH_WASM_COMPONENT_ABI {
        return Err(CourierError::WasmGuest {
            courier: "wasm".to_string(),
            message: format!(
                "unsupported WASM ABI `{}`; expected `{}`",
                component.abi, DISPATCH_WASM_COMPONENT_ABI
            ),
        });
    }

    Ok(())
}

fn resolve_wasm_component_path(parcel: &LoadedParcel) -> Result<PathBuf, CourierError> {
    let component =
        parcel
            .config
            .courier
            .component()
            .ok_or_else(|| CourierError::MissingCourierComponent {
                courier: "wasm".to_string(),
                parcel_digest: parcel.config.digest.clone(),
            })?;
    let path = parcel
        .parcel_dir
        .join("context")
        .join(&component.packaged_path);
    if !path.exists() {
        return Err(CourierError::MissingToolFile {
            tool: "component".to_string(),
            path: path.display().to_string(),
        });
    }
    Ok(path)
}

fn load_wasm_component(
    engine: &Engine,
    component_cache: &Arc<Mutex<BoundedLruCache<Component>>>,
    parcel: &LoadedParcel,
    path: &Path,
) -> Result<Component, CourierError> {
    let component_config =
        parcel
            .config
            .courier
            .component()
            .ok_or_else(|| CourierError::MissingCourierComponent {
                courier: "wasm".to_string(),
                parcel_digest: parcel.config.digest.clone(),
            })?;
    if let Some(component) = component_cache
        .lock()
        .expect("wasm component cache lock poisoned")
        .get(&component_config.sha256)
    {
        return Ok(component);
    }

    let component = Component::from_file(engine, path).map_err(|source| {
        CourierError::CompileWasmComponent {
            courier: "wasm".to_string(),
            path: path.display().to_string(),
            source,
        }
    })?;
    component_cache
        .lock()
        .expect("wasm component cache lock poisoned")
        .insert(component_config.sha256.clone(), component.clone());
    Ok(component)
}

fn instantiate_wasm_guest(
    engine: &Engine,
    component_cache: &Arc<Mutex<BoundedLruCache<Component>>>,
    parcel: &LoadedParcel,
    session: &CourierSession,
    chat_backend_override: Option<Arc<dyn ChatModelBackend>>,
) -> Result<
    (
        Store<WasmHostState>,
        wasm_bindings::CourierGuest,
        wasm_bindings::exports::dispatch::courier::guest::ParcelContext,
    ),
    CourierError,
> {
    let component_path = resolve_wasm_component_path(parcel)?;
    let component = load_wasm_component(engine, component_cache, parcel, &component_path)?;
    let prompt = resolve_prompt_text(parcel)?;
    let local_tools = list_local_tools(parcel);

    let mut linker = Linker::new(engine);
    wasm_bindings::CourierGuest::add_to_linker::<WasmHostState, HasSelf<WasmHost>>(
        &mut linker,
        |state: &mut WasmHostState| &mut state.host,
    )
    .map_err(|source| CourierError::InstantiateWasmComponent {
        courier: "wasm".to_string(),
        path: component_path.display().to_string(),
        source,
    })?;
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|source| {
        CourierError::InstantiateWasmComponent {
            courier: "wasm".to_string(),
            path: component_path.display().to_string(),
            source,
        }
    })?;

    let parcel_context = wasm_parcel_context(parcel, &prompt, &local_tools);
    let mut store = Store::new(
        engine,
        WasmHostState {
            host: WasmHost {
                parcel: parcel.clone(),
                session: session.clone(),
                chat_backend_override,
                run_deadline: run_timeout_deadline(session, &parcel.config.timeouts)?,
            },
            wasi_ctx: WasiCtx::builder().build(),
            resource_table: ResourceTable::new(),
        },
    );
    let guest = wasm_bindings::CourierGuest::instantiate(&mut store, &component, &linker).map_err(
        |source| CourierError::InstantiateWasmComponent {
            courier: "wasm".to_string(),
            path: component_path.display().to_string(),
            source,
        },
    )?;

    Ok((store, guest, parcel_context))
}

fn wasm_parcel_context(
    parcel: &LoadedParcel,
    prompt: &str,
    local_tools: &[LocalToolSpec],
) -> wasm_bindings::exports::dispatch::courier::guest::ParcelContext {
    wasm_bindings::exports::dispatch::courier::guest::ParcelContext {
        parcel_digest: parcel.config.digest.clone(),
        entrypoint: parcel.config.entrypoint.clone(),
        prompt: prompt.to_string(),
        local_tools: local_tools
            .iter()
            .map(|tool| wasm_bindings::dispatch::courier::host::LocalTool {
                alias: tool.alias.clone(),
                description: tool.description.clone(),
                input_schema_json: match (
                    tool.input_schema_packaged_path.as_deref(),
                    tool.input_schema_sha256.as_deref(),
                ) {
                    (Some(packaged_path), expected_sha256) => {
                        load_tool_schema(parcel, &tool.alias, packaged_path, expected_sha256)
                            .ok()
                            .and_then(|schema| serde_json::to_string(&schema).ok())
                    }
                    (None, _) => None,
                },
            })
            .collect(),
        primary_model: parcel
            .config
            .models
            .primary
            .as_ref()
            .map(|model| model.id.clone()),
    }
}

fn wasm_guest_session(
    session: &CourierSession,
) -> wasm_bindings::exports::dispatch::courier::guest::GuestSession {
    wasm_bindings::exports::dispatch::courier::guest::GuestSession {
        turn_count: session.turn_count,
        history: session
            .history
            .iter()
            .map(
                |message| wasm_bindings::dispatch::courier::host::ConversationMessage {
                    role: message.role.clone(),
                    content: message.content.clone(),
                },
            )
            .collect(),
        backend_state: session.backend_state.clone(),
    }
}

fn wasm_operation(
    operation: &CourierOperation,
) -> Option<wasm_bindings::exports::dispatch::courier::guest::Operation> {
    match operation {
        CourierOperation::Chat { input } => {
            Some(wasm_bindings::exports::dispatch::courier::guest::Operation::Chat(input.clone()))
        }
        CourierOperation::Job { payload } => {
            Some(wasm_bindings::exports::dispatch::courier::guest::Operation::Job(payload.clone()))
        }
        CourierOperation::Heartbeat { payload } => Some(
            wasm_bindings::exports::dispatch::courier::guest::Operation::Heartbeat(payload.clone()),
        ),
        CourierOperation::ResolvePrompt
        | CourierOperation::ListLocalTools
        | CourierOperation::InvokeTool { .. } => None,
    }
}

fn wasm_events_to_courier_events(
    events: Vec<wasm_bindings::exports::dispatch::courier::guest::GuestEvent>,
) -> Vec<CourierEvent> {
    let mut out = Vec::with_capacity(events.len() + 1);
    for event in events {
        match event {
            wasm_bindings::exports::dispatch::courier::guest::GuestEvent::Message(message) => {
                out.push(CourierEvent::Message {
                    role: message.role,
                    content: message.content,
                });
            }
            wasm_bindings::exports::dispatch::courier::guest::GuestEvent::TextDelta(content) => {
                out.push(CourierEvent::TextDelta { content });
            }
            wasm_bindings::exports::dispatch::courier::guest::GuestEvent::BackendFallback(
                fallback,
            ) => out.push(CourierEvent::BackendFallback {
                backend: fallback.backend,
                error: fallback.error,
            }),
        }
    }
    out.push(CourierEvent::Done);
    out
}

fn apply_wasm_turn_to_session(
    session: &mut CourierSession,
    operation: &CourierOperation,
    result: &wasm_bindings::exports::dispatch::courier::guest::TurnResult,
) {
    session.turn_count += 1;
    session.backend_state = result.backend_state.clone();

    if let CourierOperation::Chat { input } = operation {
        session.history.push(ConversationMessage {
            role: "user".to_string(),
            content: input.clone(),
        });
    }

    for event in &result.events {
        if let wasm_bindings::exports::dispatch::courier::guest::GuestEvent::Message(message) =
            event
        {
            session.history.push(ConversationMessage {
                role: message.role.clone(),
                content: message.content.clone(),
            });
        }
    }
}

fn validate_courier_reference(
    courier_name: &str,
    kind: CourierKind,
    reference: &str,
) -> Result<(), CourierError> {
    if courier_reference_matches(kind, reference) {
        return Ok(());
    }

    Err(CourierError::IncompatibleCourier {
        courier: courier_name.to_string(),
        parcel_courier: reference.to_string(),
        supported: supported_courier_references(kind).join(", "),
    })
}

fn courier_reference_matches(kind: CourierKind, reference: &str) -> bool {
    match kind {
        CourierKind::Native => {
            reference == "native"
                || reference == "dispatch/native"
                || reference.starts_with("dispatch/native:")
                || reference.starts_with("dispatch/native@")
        }
        CourierKind::Docker => {
            reference == "docker"
                || reference == "dispatch/docker"
                || reference.starts_with("dispatch/docker:")
                || reference.starts_with("dispatch/docker@")
        }
        CourierKind::Wasm => {
            reference == "wasm"
                || reference == "dispatch/wasm"
                || reference.starts_with("dispatch/wasm:")
                || reference.starts_with("dispatch/wasm@")
        }
        CourierKind::Custom => {
            reference == "custom"
                || reference == "dispatch/custom"
                || reference.starts_with("dispatch/custom:")
                || reference.starts_with("dispatch/custom@")
        }
    }
}

fn supported_courier_references(kind: CourierKind) -> &'static [&'static str] {
    match kind {
        CourierKind::Native => &["dispatch/native", "dispatch/native:<tag>", "native"],
        CourierKind::Docker => &["dispatch/docker", "dispatch/docker:<tag>", "docker"],
        CourierKind::Wasm => &["dispatch/wasm", "dispatch/wasm:<tag>", "wasm"],
        CourierKind::Custom => &["dispatch/custom", "dispatch/custom:<tag>", "custom"],
    }
}

fn ensure_session_matches_parcel(
    image: &LoadedParcel,
    session: &CourierSession,
) -> Result<(), CourierError> {
    if session.parcel_digest != image.config.digest {
        return Err(CourierError::SessionParcelMismatch {
            session_parcel_digest: session.parcel_digest.clone(),
            parcel_digest: image.config.digest.clone(),
        });
    }

    Ok(())
}

fn ensure_operation_matches_entrypoint(
    session: &CourierSession,
    operation: &CourierOperation,
) -> Result<(), CourierError> {
    let Some(entrypoint) = session.entrypoint.as_deref() else {
        return Ok(());
    };

    let Some(operation_name) = operation_entrypoint_name(operation) else {
        return Ok(());
    };

    if entrypoint == operation_name {
        return Ok(());
    }

    Err(CourierError::EntrypointMismatch {
        entrypoint: entrypoint.to_string(),
        operation: operation_name.to_string(),
    })
}

fn operation_entrypoint_name(operation: &CourierOperation) -> Option<&'static str> {
    match operation {
        CourierOperation::Chat { .. } => Some("chat"),
        CourierOperation::Job { .. } => Some("job"),
        CourierOperation::Heartbeat { .. } => Some("heartbeat"),
        CourierOperation::ResolvePrompt
        | CourierOperation::ListLocalTools
        | CourierOperation::InvokeTool { .. } => None,
    }
}

fn resolve_manifest_path(path: &Path) -> Result<PathBuf, CourierError> {
    if !path.exists() {
        return Err(CourierError::MissingParcelPath {
            path: path.display().to_string(),
        });
    }

    if path.is_dir() {
        Ok(path.join("manifest.json"))
    } else {
        Ok(path.to_path_buf())
    }
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

fn execute_host_turn(
    image: &LoadedParcel,
    session: &CourierSession,
    input: &str,
    mode: NativeTurnMode,
    context: HostTurnContext<'_>,
) -> Result<ChatTurnResult, CourierError> {
    let trimmed = input.trim();
    let local_tools = list_local_tools(image);
    let builtin_tools = list_native_builtin_tools(image);
    let mut events = Vec::new();

    if matches!(mode, NativeTurnMode::Chat) && trimmed.eq_ignore_ascii_case("/prompt") {
        return Ok(ChatTurnResult {
            reply: resolve_prompt_text(image)?,
            events: Vec::new(),
            streamed_reply: false,
        });
    }

    if matches!(mode, NativeTurnMode::Chat) && trimmed.eq_ignore_ascii_case("/tools") {
        if local_tools.is_empty() {
            return Ok(ChatTurnResult {
                reply: "No local tools are declared for this image.".to_string(),
                events: Vec::new(),
                streamed_reply: false,
            });
        }

        let names = local_tools
            .iter()
            .map(|tool| tool.alias.clone())
            .collect::<Vec<_>>()
            .join(", ");
        return Ok(ChatTurnResult {
            reply: format!("Declared local tools: {names}"),
            events: Vec::new(),
            streamed_reply: false,
        });
    }

    if matches!(mode, NativeTurnMode::Chat) && trimmed.eq_ignore_ascii_case("/help") {
        return Ok(ChatTurnResult {
            reply: format!(
                "{} chat is a reference backend. Available commands: /prompt, /tools, /memory, /help.",
                context.host_label
            ),
            events: Vec::new(),
            streamed_reply: false,
        });
    }

    if matches!(mode, NativeTurnMode::Chat) && trimmed.starts_with("/memory") {
        return Ok(ChatTurnResult {
            reply: handle_native_memory_command(session, trimmed)?,
            events: Vec::new(),
            streamed_reply: false,
        });
    }

    let requests =
        build_model_requests(image, &session.history, &local_tools, context.run_deadline)?;
    if let Some(mut request) = requests.first().cloned() {
        let mut remaining_requests = requests.into_iter().skip(1).collect::<Vec<_>>();
        // Validate required secrets once before any tool execution.
        for secret in &image.config.secrets {
            if secret.required && std::env::var(&secret.name).is_err() {
                return Err(CourierError::MissingSecret {
                    name: secret.name.clone(),
                });
            }
        }
        const MAX_TOOL_ROUNDS: u32 = 8;
        let mut rounds = 0u32;
        let mut executed_tool_calls = 0u32;
        let mut backend = select_chat_backend(context.chat_backend_override, &request);
        let mut candidate_locked = false;
        let mut streamed_reply = false;
        loop {
            if rounds >= MAX_TOOL_ROUNDS {
                events.push(CourierEvent::BackendFallback {
                    backend: backend.id().to_string(),
                    error: format!(
                        "tool call loop reached {} rounds without a final reply; falling back to local reference reply",
                        MAX_TOOL_ROUNDS
                    ),
                });
                break;
            }
            rounds += 1;

            request.llm_timeout_ms =
                effective_llm_timeout_ms(&image.config.timeouts, context.run_deadline)?;
            let mut streamed_deltas = Vec::new();
            let reply = match backend.generate_with_events(&request, &mut |event| match event {
                ModelStreamEvent::TextDelta { content } => streamed_deltas.push(content),
            }) {
                Ok(ModelGeneration::Reply(reply)) => reply,
                Ok(ModelGeneration::NotConfigured {
                    backend: not_configured_backend,
                    reason,
                }) => {
                    if !candidate_locked
                        && let Some(next_request) = remaining_requests.first().cloned()
                    {
                        events.push(CourierEvent::BackendFallback {
                            backend: not_configured_backend,
                            error: format!(
                                "{reason}; trying fallback model `{}`",
                                next_request.model
                            ),
                        });
                        request = next_request;
                        backend = select_chat_backend(context.chat_backend_override, &request);
                        remaining_requests.remove(0);
                        continue;
                    }
                    events.push(CourierEvent::BackendFallback {
                        backend: not_configured_backend,
                        error: reason,
                    });
                    break;
                }
                Err(error) => {
                    if !candidate_locked
                        && let Some(next_request) = remaining_requests.first().cloned()
                    {
                        events.push(CourierEvent::BackendFallback {
                            backend: backend.id().to_string(),
                            error: format!(
                                "{error}; trying fallback model `{}`",
                                next_request.model
                            ),
                        });
                        request = next_request;
                        backend = select_chat_backend(context.chat_backend_override, &request);
                        remaining_requests.remove(0);
                        continue;
                    }
                    events.push(CourierEvent::BackendFallback {
                        backend: backend.id().to_string(),
                        error: error.to_string(),
                    });
                    break;
                }
            };

            if reply.tool_calls.is_empty() && !streamed_deltas.is_empty() {
                streamed_reply = true;
                events.extend(
                    streamed_deltas
                        .into_iter()
                        .map(|content| CourierEvent::TextDelta { content }),
                );
            }

            if !reply.tool_calls.is_empty() {
                candidate_locked = true;
                if let Some(limit) = request.tool_call_limit {
                    let attempted =
                        executed_tool_calls.saturating_add(reply.tool_calls.len() as u32);
                    if attempted > limit {
                        return Err(CourierError::ToolCallLimitExceeded { limit, attempted });
                    }
                }
                let reply_tool_calls = reply.tool_calls.clone();
                let mut tool_outputs = Vec::with_capacity(reply.tool_calls.len());
                for tool_call in reply.tool_calls {
                    let invocation = ToolInvocation {
                        name: tool_call.name.clone(),
                        input: Some(tool_call.input.clone()),
                    };
                    let tool_result = if let Some(tool) =
                        local_tools.iter().find(|t| t.matches_name(&tool_call.name))
                    {
                        let normalized_input =
                            normalize_local_tool_input(tool, tool_call.input.as_str())?;
                        events.push(CourierEvent::ToolCallStarted {
                            invocation,
                            command: tool.command().to_string(),
                            args: tool.args().to_vec(),
                        });
                        execute_host_local_tool(
                            image,
                            tool,
                            Some(normalized_input.as_ref()),
                            context.tool_runner,
                            context.run_deadline,
                        )?
                    } else if let Some(tool) = builtin_tools
                        .iter()
                        .find(|tool| tool.capability == tool_call.name)
                    {
                        events.push(CourierEvent::ToolCallStarted {
                            invocation,
                            command: "dispatch-builtin".to_string(),
                            args: vec![tool.capability.clone()],
                        });
                        execute_builtin_tool(session, &tool.capability, &tool_call.input)?
                    } else {
                        return Err(CourierError::UnknownLocalTool {
                            tool: tool_call.name.clone(),
                        });
                    };
                    let combined_output = if tool_result.exit_code == 0 {
                        if tool_result.stderr.trim().is_empty() {
                            tool_result.stdout.clone()
                        } else if tool_result.stdout.trim().is_empty() {
                            tool_result.stderr.clone()
                        } else {
                            format!(
                                "stdout:\n{}\n\nstderr:\n{}",
                                tool_result.stdout, tool_result.stderr
                            )
                        }
                    } else {
                        format!(
                            "tool_failed exit_code={}\nstdout:\n{}\n\nstderr:\n{}",
                            tool_result.exit_code, tool_result.stdout, tool_result.stderr
                        )
                    };
                    let combined_output =
                        truncate_tool_output(combined_output, request.tool_output_limit);
                    events.push(CourierEvent::ToolCallFinished {
                        result: tool_result,
                    });
                    tool_outputs.push(ModelToolOutput {
                        call_id: tool_call.call_id,
                        name: tool_call.name,
                        output: combined_output,
                        kind: tool_call.kind,
                    });
                    executed_tool_calls = executed_tool_calls.saturating_add(1);
                }

                if backend.supports_previous_response_id() {
                    request.messages.clear();
                    request.pending_tool_calls.clear();
                    request.tool_outputs = tool_outputs;
                    request.previous_response_id = reply.response_id;
                } else {
                    request.pending_tool_calls = reply_tool_calls;
                    request.tool_outputs = tool_outputs;
                    request.previous_response_id = None;
                }
                continue;
            }

            if let Some(text) = reply.text {
                return Ok(ChatTurnResult {
                    reply: text,
                    events,
                    streamed_reply,
                });
            }
            break;
        }
    }

    let prompt_sections = image
        .config
        .instructions
        .iter()
        .filter(|instruction| {
            matches!(
                instruction.kind,
                InstructionKind::Soul
                    | InstructionKind::Identity
                    | InstructionKind::Skill
                    | InstructionKind::Agents
                    | InstructionKind::User
                    | InstructionKind::Tools
                    | InstructionKind::Memory
                    | InstructionKind::Heartbeat
            )
        })
        .count()
        + usize::from(!image.config.inline_prompts.is_empty());
    let tool_count = local_tools.len() + builtin_tools.len();
    let prior_messages = session.history.len().saturating_sub(1);

    Ok(ChatTurnResult {
        reply: format!(
            "{} {} reference reply for turn {}. Loaded {} prompt section(s) and {} tool(s). Prior messages in session: {}. Input: {}",
            context.host_label,
            native_turn_mode_name(mode),
            session.turn_count,
            prompt_sections,
            tool_count,
            prior_messages,
            input
        ),
        events,
        streamed_reply: false,
    })
}

fn native_turn_mode_name(mode: NativeTurnMode) -> &'static str {
    match mode {
        NativeTurnMode::Chat => "chat",
        NativeTurnMode::Job => "job",
        NativeTurnMode::Heartbeat => "heartbeat",
    }
}

fn format_job_payload(payload: &str) -> String {
    format!("Job payload:\n{payload}")
}

fn format_heartbeat_payload(payload: Option<&str>) -> String {
    match payload {
        Some(payload) if !payload.trim().is_empty() => format!("Heartbeat payload:\n{payload}"),
        _ => "Heartbeat tick".to_string(),
    }
}

#[cfg(test)]
fn build_model_request(
    image: &LoadedParcel,
    messages: &[ConversationMessage],
    local_tools: &[LocalToolSpec],
) -> Result<Option<ModelRequest>, CourierError> {
    Ok(build_model_requests(image, messages, local_tools, None)?
        .into_iter()
        .next())
}

fn build_model_requests(
    image: &LoadedParcel,
    messages: &[ConversationMessage],
    local_tools: &[LocalToolSpec],
    run_deadline: Option<Instant>,
) -> Result<Vec<ModelRequest>, CourierError> {
    let model_refs = configured_model_references(&image.config.models);
    if model_refs.is_empty() {
        return Ok(Vec::new());
    }
    let builtin_tools = list_native_builtin_tools(image);
    let mut tools = local_tools
        .iter()
        .map(|tool| build_model_tool_definition(image, tool))
        .collect::<Result<Vec<_>, _>>()?;
    tools.extend(
        builtin_tools
            .iter()
            .map(build_builtin_model_tool_definition),
    );
    let instructions = resolve_prompt_text(image)?;
    let llm_timeout_ms = effective_llm_timeout_ms(&image.config.timeouts, run_deadline)?;

    Ok(model_refs
        .into_iter()
        .map(|model| ModelRequest {
            model: model.id,
            provider: model.provider,
            llm_timeout_ms,
            context_token_limit: configured_context_token_limit(&image.config.limits),
            tool_call_limit: configured_tool_call_limit(&image.config.limits),
            tool_output_limit: configured_tool_output_limit(&image.config.limits),
            instructions: instructions.clone(),
            messages: messages.to_vec(),
            tools: tools.clone(),
            pending_tool_calls: Vec::new(),
            tool_outputs: Vec::new(),
            previous_response_id: None,
        })
        .collect())
}

fn configured_model_references(policy: &crate::manifest::ModelPolicy) -> Vec<ModelReference> {
    let mut models = Vec::new();
    if let Some(primary) = &policy.primary {
        models.push(primary.clone());
        models.extend(policy.fallbacks.iter().cloned());
        return models;
    }
    let Some(model) = configured_model_id(None) else {
        return models;
    };
    models.push(ModelReference {
        id: model,
        provider: std::env::var("LLM_BACKEND").ok(),
    });
    models
}

fn build_wasm_model_requests(
    parcel: &LoadedParcel,
    input: WasmModelRequestInput,
) -> Result<Vec<ModelRequest>, CourierError> {
    let configured = configured_model_references(&parcel.config.models);
    let model_refs = match input.requested_model {
        Some(model) => {
            if let Some(index) = configured
                .iter()
                .position(|candidate| candidate.id == model)
            {
                configured[index..].to_vec()
            } else {
                vec![ModelReference {
                    id: model,
                    provider: None,
                }]
            }
        }
        None => configured,
    };

    if model_refs.is_empty() {
        return Err(CourierError::ModelBackendRequest(
            "no model configured for wasm guest request".to_string(),
        ));
    }

    let llm_timeout_ms = effective_llm_timeout_ms(&parcel.config.timeouts, input.run_deadline)?;

    Ok(model_refs
        .into_iter()
        .map(|model| ModelRequest {
            model: model.id,
            provider: model.provider,
            llm_timeout_ms,
            context_token_limit: configured_context_token_limit(&parcel.config.limits),
            tool_call_limit: configured_tool_call_limit(&parcel.config.limits),
            tool_output_limit: configured_tool_output_limit(&parcel.config.limits),
            instructions: input.instructions.clone(),
            messages: input.messages.clone(),
            tools: input.tools.clone(),
            pending_tool_calls: Vec::new(),
            tool_outputs: input.tool_outputs.clone(),
            previous_response_id: input.previous_response_id.clone(),
        })
        .collect())
}

fn configured_context_token_limit(limits: &[crate::manifest::LimitSpec]) -> Option<u32> {
    configured_limit_u32(limits, "CONTEXT_TOKENS")
}

fn configured_llm_timeout_ms(
    timeouts: &[crate::manifest::TimeoutSpec],
) -> Result<Option<u64>, CourierError> {
    configured_timeout_duration(timeouts, "LLM")
        .map(|timeout| timeout.map(|duration| duration.as_millis() as u64))
}

fn effective_llm_timeout_ms(
    timeouts: &[crate::manifest::TimeoutSpec],
    run_deadline: Option<Instant>,
) -> Result<Option<u64>, CourierError> {
    let configured_ms = configured_llm_timeout_ms(timeouts)?;
    let run_remaining_ms = remaining_deadline_duration(run_deadline)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));
    Ok(match (configured_ms, run_remaining_ms) {
        (Some(configured), Some(run_remaining)) => Some(configured.min(run_remaining)),
        (Some(configured), None) => Some(configured),
        (None, Some(run_remaining)) => Some(run_remaining),
        (None, None) => None,
    })
}

fn configured_tool_call_limit(limits: &[crate::manifest::LimitSpec]) -> Option<u32> {
    configured_limit_u32(limits, "TOOL_CALLS")
}

fn configured_tool_output_limit(limits: &[crate::manifest::LimitSpec]) -> Option<usize> {
    configured_limit_u32(limits, "TOOL_OUTPUT").map(|value| value as usize)
}

fn configured_limit_u32(limits: &[crate::manifest::LimitSpec], scope: &str) -> Option<u32> {
    limits
        .iter()
        .rev()
        .find(|limit| limit.scope.eq_ignore_ascii_case(scope))
        .and_then(|limit| limit.value.parse::<u32>().ok())
        .filter(|value| *value > 0)
}

fn truncate_tool_output(output: String, limit: Option<usize>) -> String {
    const TRUNCATION_NOTE: &str = "\n\n[dispatch truncated tool output]";
    let Some(limit) = limit else {
        return output;
    };
    if output.len() <= limit {
        return output;
    }
    if limit <= TRUNCATION_NOTE.len() {
        return TRUNCATION_NOTE[..limit].to_string();
    }
    let keep = limit - TRUNCATION_NOTE.len();
    let mut truncated = String::with_capacity(limit);
    let mut used = 0usize;
    for ch in output.chars() {
        let ch_len = ch.len_utf8();
        if used + ch_len > keep {
            break;
        }
        truncated.push(ch);
        used += ch_len;
    }
    truncated.push_str(TRUNCATION_NOTE);
    truncated
}

fn select_chat_backend(
    chat_backend_override: Option<&Arc<dyn ChatModelBackend>>,
    request: &ModelRequest,
) -> Arc<dyn ChatModelBackend> {
    match chat_backend_override {
        Some(backend) => backend.clone(),
        None => default_chat_backend_for_provider(request.provider.as_deref()),
    }
}

fn build_model_tool_definition(
    image: &LoadedParcel,
    tool: &LocalToolSpec,
) -> Result<ModelToolDefinition, CourierError> {
    let description = tool.description.clone().unwrap_or_else(|| match &tool.target {
        LocalToolTarget::Local { packaged_path, .. } => format!(
            "Local Dispatch tool `{}` packaged at `{}`. Provide free-form text or JSON input appropriate for the tool.",
            tool.alias, packaged_path
        ),
        LocalToolTarget::A2a { .. } => format!(
            "Dispatch A2A tool `{}` delegates to the configured remote agent endpoint. Provide free-form text or JSON input appropriate for the remote agent.",
            tool.alias
        ),
    });
    let format = match (
        tool.input_schema_packaged_path.as_deref(),
        tool.input_schema_sha256.as_deref(),
    ) {
        (Some(source), expected_sha256) => ModelToolFormat::JsonSchema {
            schema: load_tool_schema(image, &tool.alias, source, expected_sha256)?,
        },
        (None, _) => ModelToolFormat::Text,
    };

    Ok(ModelToolDefinition {
        name: tool.alias.clone(),
        description,
        format,
    })
}

fn build_builtin_model_tool_definition(tool: &BuiltinToolSpec) -> ModelToolDefinition {
    ModelToolDefinition {
        name: tool.capability.clone(),
        description: builtin_memory_tool_description(tool),
        format: ModelToolFormat::JsonSchema {
            schema: tool.input_schema.clone(),
        },
    }
}

fn normalize_local_tool_input<'a>(
    tool: &LocalToolSpec,
    input: &'a str,
) -> Result<Cow<'a, str>, CourierError> {
    if tool.input_schema_packaged_path.is_some() {
        return Ok(Cow::Borrowed(input));
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(input) else {
        return Ok(Cow::Borrowed(input));
    };
    let Some(object) = value.as_object() else {
        return Ok(Cow::Borrowed(input));
    };
    if object.len() != 1 {
        return Ok(Cow::Borrowed(input));
    }
    match object.get("input").and_then(serde_json::Value::as_str) {
        Some(value) => Ok(Cow::Owned(value.to_string())),
        None => Ok(Cow::Borrowed(input)),
    }
}

fn load_tool_schema(
    image: &LoadedParcel,
    tool: &str,
    packaged_path: &str,
    expected_sha256: Option<&str>,
) -> Result<serde_json::Value, CourierError> {
    let path = image.parcel_dir.join("context").join(packaged_path);
    let body = fs::read(&path).map_err(|source_error| CourierError::ReadFile {
        path: path.display().to_string(),
        source: source_error,
    })?;
    if let Some(expected_sha256) = expected_sha256 {
        let actual_sha256 = encode_hex(Sha256::digest(&body));
        if actual_sha256 != expected_sha256 {
            return Err(CourierError::ToolSchemaDigestMismatch {
                tool: tool.to_string(),
                path: path.display().to_string(),
                expected_sha256: expected_sha256.to_string(),
                actual_sha256,
            });
        }
    }
    let schema: serde_json::Value =
        serde_json::from_slice(&body).map_err(|source_error| CourierError::ParseToolSchema {
            tool: tool.to_string(),
            path: path.display().to_string(),
            source: source_error,
        })?;
    if !schema.is_object() {
        return Err(CourierError::ToolSchemaShape {
            tool: tool.to_string(),
            path: path.display().to_string(),
        });
    }

    Ok(schema)
}

fn configured_model_id(primary: Option<&ModelReference>) -> Option<String> {
    configured_model_id_with(primary, process_env_lookup)
}

fn configured_model_id_with<F>(
    primary: Option<&ModelReference>,
    mut env_lookup: F,
) -> Option<String>
where
    F: FnMut(&str) -> Option<String>,
{
    primary
        .map(|model| model.id.clone())
        .or_else(|| env_lookup("LLM_MODEL"))
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
mod tests {
    use super::*;
    use crate::{BuildOptions, build_agentfile};
    use rusqlite::Connection;
    use serde_json::Value;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};
    use std::{
        fs,
        io::{BufRead, BufReader, Read, Write},
        net::{TcpListener, TcpStream},
        sync::atomic::AtomicU64,
        sync::mpsc,
        thread,
        time::Duration,
    };
    use tempfile::tempdir;

    struct TestImage {
        _dir: tempfile::TempDir,
        image: LoadedParcel,
    }

    struct TestA2aServer {
        base_url: String,
        shutdown: mpsc::Sender<()>,
        handle: Option<thread::JoinHandle<()>>,
    }

    #[derive(Clone)]
    struct TestA2aServerOptions {
        agent_name: Option<String>,
        expected_auth: Option<String>,
        publish_card: bool,
        task_state: String,
        task_status_message: String,
        task_get_state: Option<String>,
        task_get_status_message: Option<String>,
        cancel_count: Option<Arc<AtomicU64>>,
        rpc_error: Option<(i64, String)>,
        card_url: Option<String>,
        response_delay: Duration,
    }

    impl Default for TestA2aServerOptions {
        fn default() -> Self {
            Self {
                agent_name: Some("demo-a2a".to_string()),
                expected_auth: None,
                publish_card: true,
                task_state: "completed".to_string(),
                task_status_message: "ok".to_string(),
                task_get_state: None,
                task_get_status_message: None,
                cancel_count: None,
                rpc_error: None,
                card_url: None,
                response_delay: Duration::from_millis(0),
            }
        }
    }

    impl Drop for TestA2aServer {
        fn drop(&mut self) {
            let _ = self.shutdown.send(());
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn start_test_a2a_server() -> TestA2aServer {
        start_test_a2a_server_with_options(TestA2aServerOptions::default())
    }

    fn start_test_a2a_server_with_options(options: TestA2aServerOptions) -> TestA2aServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
        let server_base_url = base_url.clone();
        let options = options.clone();
        let handle = thread::spawn(move || {
            loop {
                if shutdown_rx.try_recv().is_ok() {
                    break;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        handle_test_a2a_connection(stream, &server_base_url, &options)
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("failed to accept A2A connection: {error}"),
                }
            }
        });
        TestA2aServer {
            base_url,
            shutdown: shutdown_tx,
            handle: Some(handle),
        }
    }

    fn handle_test_a2a_connection(
        stream: TcpStream,
        base_url: &str,
        options: &TestA2aServerOptions,
    ) {
        stream.set_nonblocking(false).unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap() == 0 {
            return;
        }
        let request_line = request_line.trim_end();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default();
        let target = parts.next().unwrap_or_default();
        let mut content_length = 0usize;
        let mut authorization = None;
        let mut headers = Vec::new();
        loop {
            let mut header_line = String::new();
            reader.read_line(&mut header_line).unwrap();
            let header_line = header_line.trim_end();
            if header_line.is_empty() {
                break;
            }
            headers.push(header_line.to_string());
            if let Some((name, value)) = header_line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse().unwrap();
            } else if let Some((name, value)) = header_line.split_once(':')
                && name.eq_ignore_ascii_case("authorization")
            {
                authorization = Some(value.trim().to_string());
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).unwrap();

        if options.expected_auth.as_deref().is_some_and(|expected| {
            if expected.contains(':') {
                !headers
                    .iter()
                    .any(|header| header.eq_ignore_ascii_case(expected))
            } else {
                authorization.as_deref() != Some(expected)
            }
        }) {
            write_test_http_response(&mut writer, 401, "text/plain", b"unauthorized");
            return;
        }

        match (method, target) {
            ("GET", "/.well-known/agent.json") if options.publish_card => write_test_http_response(
                &mut writer,
                200,
                "application/json",
                serde_json::to_vec(&serde_json::json!({
                    "name": options.agent_name,
                    "url": options.card_url.clone().unwrap_or_else(|| format!("{base_url}/a2a"))
                }))
                .unwrap()
                .as_slice(),
            ),
            ("POST", path) if path.ends_with("/a2a") => {
                if !options.response_delay.is_zero() {
                    thread::sleep(options.response_delay);
                }
                if let Some((code, message)) = &options.rpc_error {
                    let output = serde_json::json!({
                        "jsonrpc":"2.0",
                        "id":"1",
                        "error":{"code": code, "message": message}
                    });
                    write_test_http_response(
                        &mut writer,
                        200,
                        "application/json",
                        serde_json::to_vec(&output).unwrap().as_slice(),
                    );
                    return;
                }
                let mut payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let method = payload
                    .get("method")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let output = if method == "tasks/get" {
                    let state = options
                        .task_get_state
                        .as_deref()
                        .unwrap_or(options.task_state.as_str());
                    let message = options
                        .task_get_status_message
                        .as_deref()
                        .unwrap_or(options.task_status_message.as_str());
                    serde_json::json!({
                        "jsonrpc":"2.0",
                        "id":"1",
                        "result":{
                            "id":"task-1",
                            "status":{"state": state, "message": message},
                            "artifacts":[{"parts":[{"kind":"text","text":"echo:hello"}]}]
                        }
                    })
                } else if method == "tasks/cancel" {
                    if let Some(counter) = &options.cancel_count {
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                    serde_json::json!({
                        "jsonrpc":"2.0",
                        "id":"1",
                        "result":{
                            "id":"task-1",
                            "status":{"state":"canceled","message":"canceled"},
                            "artifacts":[]
                        }
                    })
                } else {
                    let part = payload
                        .pointer_mut("/params/message/parts/0")
                        .expect("expected request part");
                    if let Some(text) = part.get("text").and_then(serde_json::Value::as_str) {
                        serde_json::json!({
                            "jsonrpc":"2.0",
                            "id":"1",
                            "result":{
                                "id":"task-1",
                                "status":{"state": options.task_state, "message": options.task_status_message},
                                "artifacts":[{"parts":[{"kind":"text","text":format!("echo:{text}")}]}]
                            }
                        })
                    } else {
                        serde_json::json!({
                            "jsonrpc":"2.0",
                            "id":"1",
                            "result":{
                                "id":"task-1",
                                "status":{"state": options.task_state, "message": options.task_status_message},
                                "artifacts":[{"parts":[{"kind":"data","data":part.get("data").cloned().unwrap_or(serde_json::Value::Null)}]}]
                            }
                        })
                    }
                };
                write_test_http_response(
                    &mut writer,
                    200,
                    "application/json",
                    serde_json::to_vec(&output).unwrap().as_slice(),
                );
            }
            _ => write_test_http_response(&mut writer, 404, "text/plain", b"not found"),
        }
    }

    fn write_test_http_response(
        writer: &mut TcpStream,
        status: u16,
        content_type: &str,
        body: &[u8],
    ) {
        let reason = match status {
            200 => "OK",
            404 => "Not Found",
            _ => "OK",
        };
        let headers = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = writer.write_all(headers.as_bytes());
        let _ = writer.write_all(body);
        let _ = writer.flush();
    }

    fn build_test_image(agentfile: &str, files: &[(&str, &str)]) -> TestImage {
        let dir = tempdir().unwrap();
        let output_root = dir.path().join(".dispatch/parcels");
        build_test_image_in_dir(dir, agentfile, files, &[], output_root)
    }

    fn build_test_image_with_output_root(
        agentfile: &str,
        files: &[(&str, &str)],
        output_root: &Path,
    ) -> TestImage {
        let dir = tempdir().unwrap();
        build_test_image_in_dir(dir, agentfile, files, &[], output_root.to_path_buf())
    }

    fn build_test_image_with_binary_files(
        agentfile: &str,
        files: &[(&str, &str)],
        binary_files: &[(&str, &[u8])],
    ) -> TestImage {
        let dir = tempdir().unwrap();
        let output_root = dir.path().join(".dispatch/parcels");
        build_test_image_in_dir(dir, agentfile, files, binary_files, output_root)
    }

    fn build_test_image_in_dir(
        dir: tempfile::TempDir,
        agentfile: &str,
        files: &[(&str, &str)],
        binary_files: &[(&str, &[u8])],
        output_root: PathBuf,
    ) -> TestImage {
        fs::write(dir.path().join("Agentfile"), agentfile).unwrap();
        for (relative, body) in files {
            let path = dir.path().join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, body).unwrap();
        }
        for (relative, body) in binary_files {
            let path = dir.path().join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, body).unwrap();
        }

        let built =
            build_agentfile(&dir.path().join("Agentfile"), &BuildOptions { output_root }).unwrap();

        TestImage {
            image: load_parcel(&built.parcel_dir).unwrap(),
            _dir: dir,
        }
    }

    #[cfg(unix)]
    fn build_test_plugin_courier(
        dir: &tempfile::TempDir,
        digest: &str,
        error_mode: bool,
    ) -> (JsonlCourierPlugin, std::path::PathBuf) {
        let plugin_path = dir.path().join("plugin.sh");
        let script = if error_mode {
            "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{\"kind\":\"error\",\"error\":{\"code\":\"bad_request\",\"message\":\"plugin rejected request\"}}'\n"
                .to_string()
        } else {
            format!(
                "#!/bin/sh\nwhile IFS= read -r request; do\ncase \"$request\" in\n*'\"kind\":\"capabilities\"'*)\nprintf '%s\\n' '{{\"kind\":\"capabilities\",\"capabilities\":{{\"courier_id\":\"demo-plugin\",\"kind\":\"custom\",\"supports_chat\":true,\"supports_job\":false,\"supports_heartbeat\":false,\"supports_local_tools\":false,\"supports_mounts\":[]}}}}'\n;;\n*'\"kind\":\"validate_parcel\"'*)\nprintf '%s\\n' '{{\"kind\":\"ok\"}}'\n;;\n*'\"kind\":\"inspect\"'*)\nprintf '%s\\n' '{{\"kind\":\"inspection\",\"inspection\":{{\"courier_id\":\"demo-plugin\",\"kind\":\"custom\",\"entrypoint\":\"chat\",\"required_secrets\":[],\"mounts\":[],\"local_tools\":[]}}}}'\n;;\n*'\"kind\":\"open_session\"'*)\nprintf '%s\\n' '{{\"kind\":\"session\",\"session\":{{\"id\":\"plugin-session\",\"parcel_digest\":\"{digest}\",\"entrypoint\":\"chat\",\"turn_count\":0,\"history\":[],\"backend_state\":\"open\"}}}}'\n;;\n*'\"kind\":\"resume_session\"'*)\nprintf '%s\\n' '{{\"kind\":\"session\",\"session\":{{\"id\":\"plugin-session\",\"parcel_digest\":\"{digest}\",\"entrypoint\":\"chat\",\"turn_count\":1,\"history\":[{{\"role\":\"user\",\"content\":\"hello plugin\"}},{{\"role\":\"assistant\",\"content\":\"from plugin\"}}],\"backend_state\":\"warm|resumed\"}}}}'\n;;\n*'\"kind\":\"shutdown\"'*)\nprintf '%s\\n' '{{\"kind\":\"ok\"}}'\nexit 0\n;;\n*'\"kind\":\"run\"'*)\nprintf '%s\\n' '{{\"kind\":\"event\",\"event\":{{\"kind\":\"message\",\"role\":\"assistant\",\"content\":\"from plugin\"}}}}'\nprintf '%s\\n' '{{\"kind\":\"done\",\"session\":{{\"id\":\"plugin-session\",\"parcel_digest\":\"{digest}\",\"entrypoint\":\"chat\",\"turn_count\":1,\"history\":[{{\"role\":\"user\",\"content\":\"hello plugin\"}},{{\"role\":\"assistant\",\"content\":\"from plugin\"}}],\"backend_state\":\"turns:1\"}}}}'\n;;\n*)\nprintf '%s\\n' '{{\"kind\":\"error\",\"error\":{{\"code\":\"bad_request\",\"message\":\"unexpected request\"}}}}'\n;;\nesac\ndone\n"
            )
        };
        fs::write(&plugin_path, &script).unwrap();
        fs::set_permissions(&plugin_path, fs::Permissions::from_mode(0o755)).unwrap();

        (
            JsonlCourierPlugin::new(CourierPluginManifest {
                name: "demo-plugin".to_string(),
                version: "0.1.0".to_string(),
                protocol_version: 1,
                transport: crate::plugins::PluginTransport::Jsonl,
                description: Some("Demo plugin".to_string()),
                exec: crate::plugins::CourierPluginExec {
                    command: plugin_path.display().to_string(),
                    args: Vec::new(),
                },
                installed_sha256: Some(encode_hex(Sha256::digest(script.as_bytes()))),
            }),
            plugin_path,
        )
    }

    fn build_test_counting_plugin_courier(
        dir: &tempfile::TempDir,
        digest: &str,
    ) -> (JsonlCourierPlugin, std::path::PathBuf, std::path::PathBuf) {
        let plugin_path = dir.path().join("counting-plugin.sh");
        let starts_path = dir.path().join("plugin-starts.log");
        let script = format!(
            "#!/bin/sh\nprintf 'started\\n' >> '{}'\nwhile IFS= read -r request; do\ncase \"$request\" in\n*'\"kind\":\"capabilities\"'*)\nprintf '%s\\n' '{{\"kind\":\"capabilities\",\"capabilities\":{{\"courier_id\":\"demo-counting-plugin\",\"kind\":\"custom\",\"supports_chat\":true,\"supports_job\":false,\"supports_heartbeat\":false,\"supports_local_tools\":false,\"supports_mounts\":[]}}}}'\n;;\n*'\"kind\":\"open_session\"'*)\nprintf '%s\\n' '{{\"kind\":\"session\",\"session\":{{\"id\":\"plugin-session-counting\",\"parcel_digest\":\"{digest}\",\"entrypoint\":\"chat\",\"turn_count\":0,\"history\":[],\"backend_state\":\"open\"}}}}'\n;;\n*'\"kind\":\"resume_session\"'*)\nprintf '%s\\n' '{{\"kind\":\"session\",\"session\":{{\"id\":\"plugin-session-counting\",\"parcel_digest\":\"{digest}\",\"entrypoint\":\"chat\",\"turn_count\":1,\"history\":[{{\"role\":\"user\",\"content\":\"first\"}},{{\"role\":\"assistant\",\"content\":\"from plugin turn 1\"}}],\"backend_state\":\"turns:1|resumed\"}}}}'\n;;\n*'\"kind\":\"shutdown\"'*)\nprintf '%s\\n' '{{\"kind\":\"ok\"}}'\nexit 0\n;;\n*'\"kind\":\"run\"'*)\ncase \"$request\" in\n*'\"turn_count\":1'*)\nprintf '%s\\n' '{{\"kind\":\"event\",\"event\":{{\"kind\":\"message\",\"role\":\"assistant\",\"content\":\"from plugin turn 2\"}}}}'\nprintf '%s\\n' '{{\"kind\":\"done\",\"session\":{{\"id\":\"plugin-session-counting\",\"parcel_digest\":\"{digest}\",\"entrypoint\":\"chat\",\"turn_count\":2,\"history\":[{{\"role\":\"user\",\"content\":\"first\"}},{{\"role\":\"assistant\",\"content\":\"from plugin turn 1\"}},{{\"role\":\"user\",\"content\":\"second\"}},{{\"role\":\"assistant\",\"content\":\"from plugin turn 2\"}}],\"backend_state\":\"turns:2\"}}}}'\n;;\n*)\nprintf '%s\\n' '{{\"kind\":\"event\",\"event\":{{\"kind\":\"message\",\"role\":\"assistant\",\"content\":\"from plugin turn 1\"}}}}'\nprintf '%s\\n' '{{\"kind\":\"done\",\"session\":{{\"id\":\"plugin-session-counting\",\"parcel_digest\":\"{digest}\",\"entrypoint\":\"chat\",\"turn_count\":1,\"history\":[{{\"role\":\"user\",\"content\":\"first\"}},{{\"role\":\"assistant\",\"content\":\"from plugin turn 1\"}}],\"backend_state\":\"turns:1\"}}}}'\n;;\nesac\n;;\n*)\nprintf '%s\\n' '{{\"kind\":\"error\",\"error\":{{\"code\":\"bad_request\",\"message\":\"unexpected request\"}}}}'\n;;\nesac\ndone\n",
            starts_path.display()
        );
        fs::write(&plugin_path, &script).unwrap();
        fs::set_permissions(&plugin_path, fs::Permissions::from_mode(0o755)).unwrap();

        (
            JsonlCourierPlugin::new(CourierPluginManifest {
                name: "demo-counting-plugin".to_string(),
                version: "0.1.0".to_string(),
                protocol_version: 1,
                transport: crate::plugins::PluginTransport::Jsonl,
                description: Some("Demo counting courier plugin".to_string()),
                exec: crate::plugins::CourierPluginExec {
                    command: plugin_path.display().to_string(),
                    args: Vec::new(),
                },
                installed_sha256: Some(encode_hex(Sha256::digest(script.as_bytes()))),
            }),
            plugin_path,
            starts_path,
        )
    }

    fn build_test_shutdown_plugin_courier(
        dir: &tempfile::TempDir,
        digest: &str,
    ) -> (JsonlCourierPlugin, std::path::PathBuf) {
        let plugin_path = dir.path().join("shutdown-plugin.sh");
        let shutdowns_path = dir.path().join("plugin-shutdowns.log");
        let script = format!(
            "#!/bin/sh\nwhile IFS= read -r request; do\ncase \"$request\" in\n*'\"kind\":\"open_session\"'*)\nprintf '%s\\n' '{{\"kind\":\"session\",\"session\":{{\"id\":\"plugin-session-shutdown\",\"parcel_digest\":\"{digest}\",\"entrypoint\":\"chat\",\"turn_count\":0,\"history\":[],\"backend_state\":\"open\"}}}}'\n;;\n*'\"kind\":\"run\"'*)\nprintf '%s\\n' '{{\"kind\":\"event\",\"event\":{{\"kind\":\"message\",\"role\":\"assistant\",\"content\":\"from shutdown plugin\"}}}}'\nprintf '%s\\n' '{{\"kind\":\"done\",\"session\":{{\"id\":\"plugin-session-shutdown\",\"parcel_digest\":\"{digest}\",\"entrypoint\":\"chat\",\"turn_count\":1,\"history\":[{{\"role\":\"user\",\"content\":\"hello\"}},{{\"role\":\"assistant\",\"content\":\"from shutdown plugin\"}}],\"backend_state\":\"turns:1\"}}}}'\n;;\n*'\"kind\":\"shutdown\"'*)\nprintf 'shutdown\\n' >> '{}'\nprintf '%s\\n' '{{\"kind\":\"ok\"}}'\nexit 0\n;;\n*'\"kind\":\"capabilities\"'*)\nprintf '%s\\n' '{{\"kind\":\"capabilities\",\"capabilities\":{{\"courier_id\":\"demo-shutdown-plugin\",\"kind\":\"custom\",\"supports_chat\":true,\"supports_job\":false,\"supports_heartbeat\":false,\"supports_local_tools\":false,\"supports_mounts\":[]}}}}'\n;;\n*'\"kind\":\"validate_parcel\"'*)\nprintf '%s\\n' '{{\"kind\":\"ok\"}}'\n;;\n*'\"kind\":\"inspect\"'*)\nprintf '%s\\n' '{{\"kind\":\"inspection\",\"inspection\":{{\"courier_id\":\"demo-shutdown-plugin\",\"kind\":\"custom\",\"entrypoint\":\"chat\",\"required_secrets\":[],\"mounts\":[],\"local_tools\":[]}}}}'\n;;\n*)\nprintf '%s\\n' '{{\"kind\":\"error\",\"error\":{{\"code\":\"bad_request\",\"message\":\"unexpected request\"}}}}'\n;;\nesac\ndone\n",
            shutdowns_path.display()
        );
        fs::write(&plugin_path, &script).unwrap();
        fs::set_permissions(&plugin_path, fs::Permissions::from_mode(0o755)).unwrap();

        (
            JsonlCourierPlugin::new(CourierPluginManifest {
                name: "demo-shutdown-plugin".to_string(),
                version: "0.1.0".to_string(),
                protocol_version: 1,
                transport: crate::plugins::PluginTransport::Jsonl,
                description: Some("Demo shutdown courier plugin".to_string()),
                exec: crate::plugins::CourierPluginExec {
                    command: plugin_path.display().to_string(),
                    args: Vec::new(),
                },
                installed_sha256: Some(encode_hex(Sha256::digest(script.as_bytes()))),
            }),
            shutdowns_path,
        )
    }

    fn mount_path<'a>(session: &'a CourierSession, kind: MountKind, driver: &str) -> &'a str {
        session
            .resolved_mounts
            .iter()
            .find(|mount| mount.kind == kind && mount.driver == driver)
            .map(|mount| mount.target_path.as_str())
            .expect("expected resolved mount")
    }

    #[derive(Default)]
    struct FakeChatBackend {
        replies: Mutex<Vec<Result<ModelGeneration, String>>>,
        streams: Mutex<Vec<Vec<String>>>,
        calls: Mutex<Vec<ModelRequest>>,
        supports_previous_response_id: bool,
    }

    impl FakeChatBackend {
        fn with_reply(reply: impl Into<String>) -> Self {
            Self {
                replies: Mutex::new(vec![Ok(ModelGeneration::Reply(ModelReply {
                    text: Some(reply.into()),
                    backend: "fake".to_string(),
                    response_id: None,
                    tool_calls: Vec::new(),
                }))]),
                streams: Mutex::new(vec![Vec::new()]),
                calls: Mutex::new(Vec::new()),
                supports_previous_response_id: true,
            }
        }

        fn with_streaming_reply(reply: impl Into<String>, deltas: Vec<&str>) -> Self {
            Self {
                replies: Mutex::new(vec![Ok(ModelGeneration::Reply(ModelReply {
                    text: Some(reply.into()),
                    backend: "fake".to_string(),
                    response_id: None,
                    tool_calls: Vec::new(),
                }))]),
                streams: Mutex::new(vec![deltas.into_iter().map(ToString::to_string).collect()]),
                calls: Mutex::new(Vec::new()),
                supports_previous_response_id: true,
            }
        }

        fn with_replies(replies: Vec<Option<ModelReply>>) -> Self {
            let reply_count = replies.len();
            Self {
                replies: Mutex::new(
                    replies
                        .into_iter()
                        .map(|reply| match reply {
                            Some(reply) => Ok(ModelGeneration::Reply(reply)),
                            None => Ok(ModelGeneration::NotConfigured {
                                backend: "fake".to_string(),
                                reason: "not configured".to_string(),
                            }),
                        })
                        .collect(),
                ),
                streams: Mutex::new(vec![Vec::new(); reply_count]),
                calls: Mutex::new(Vec::new()),
                supports_previous_response_id: true,
            }
        }

        fn with_replies_without_previous_response_id(replies: Vec<Option<ModelReply>>) -> Self {
            let reply_count = replies.len();
            Self {
                replies: Mutex::new(
                    replies
                        .into_iter()
                        .map(|reply| match reply {
                            Some(reply) => Ok(ModelGeneration::Reply(reply)),
                            None => Ok(ModelGeneration::NotConfigured {
                                backend: "fake".to_string(),
                                reason: "not configured".to_string(),
                            }),
                        })
                        .collect(),
                ),
                streams: Mutex::new(vec![Vec::new(); reply_count]),
                calls: Mutex::new(Vec::new()),
                supports_previous_response_id: false,
            }
        }

        fn with_error(error: impl Into<String>) -> Self {
            Self {
                replies: Mutex::new(vec![Err(error.into())]),
                streams: Mutex::new(vec![Vec::new()]),
                calls: Mutex::new(Vec::new()),
                supports_previous_response_id: true,
            }
        }
    }

    impl ChatModelBackend for FakeChatBackend {
        fn id(&self) -> &str {
            "fake"
        }

        fn supports_previous_response_id(&self) -> bool {
            self.supports_previous_response_id
        }

        fn generate(&self, request: &ModelRequest) -> Result<ModelGeneration, CourierError> {
            self.calls.lock().unwrap().push(request.clone());
            let mut replies = self.replies.lock().unwrap();
            if replies.is_empty() {
                return Ok(ModelGeneration::NotConfigured {
                    backend: "fake".to_string(),
                    reason: "not configured".to_string(),
                });
            }
            replies.remove(0).map_err(CourierError::ModelBackendRequest)
        }

        fn generate_with_events(
            &self,
            request: &ModelRequest,
            on_event: &mut dyn FnMut(ModelStreamEvent),
        ) -> Result<ModelGeneration, CourierError> {
            self.calls.lock().unwrap().push(request.clone());
            let mut replies = self.replies.lock().unwrap();
            let mut streams = self.streams.lock().unwrap();
            if replies.is_empty() {
                return Ok(ModelGeneration::NotConfigured {
                    backend: "fake".to_string(),
                    reason: "not configured".to_string(),
                });
            }
            let stream = streams.remove(0);
            for content in stream {
                on_event(ModelStreamEvent::TextDelta { content });
            }
            replies.remove(0).map_err(CourierError::ModelBackendRequest)
        }
    }

    #[test]
    fn resolve_prompt_omits_eval_files() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
SOUL SOUL.md
SKILL SKILL.md
MEMORY POLICY MEMORY.md
EVAL evals/smoke.eval
ENTRYPOINT chat
",
            &[
                ("SOUL.md", "Soul body"),
                ("SKILL.md", "Skill body"),
                ("MEMORY.md", "Memory body"),
                ("evals/smoke.eval", "assert output contains ok"),
            ],
        );

        let prompt = resolve_prompt_text(&test_image.image).unwrap();
        assert!(prompt.contains("# SOUL"));
        assert!(prompt.contains("# SKILL"));
        assert!(prompt.contains("# MEMORY"));
        assert!(!prompt.contains("smoke.eval"));
        assert!(!prompt.contains("# EVAL"));
    }

    #[test]
    fn resolve_prompt_includes_extended_workspace_files() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
IDENTITY IDENTITY.md
SOUL SOUL.md
AGENTS AGENTS.md
USER USER.md
TOOLS TOOLS.md
MEMORY POLICY MEMORY.md
ENTRYPOINT chat
",
            &[
                ("IDENTITY.md", "Name: Demo"),
                ("SOUL.md", "Soul body"),
                ("AGENTS.md", "Workflow body"),
                ("USER.md", "User body"),
                ("TOOLS.md", "Tool body"),
                ("MEMORY.md", "Memory body"),
            ],
        );

        let prompt = resolve_prompt_text(&test_image.image).unwrap();
        assert!(prompt.contains("# IDENTITY"));
        assert!(prompt.contains("Name: Demo"));
        assert!(prompt.contains("# AGENTS"));
        assert!(prompt.contains("Workflow body"));
        assert!(prompt.contains("# USER"));
        assert!(prompt.contains("# TOOLS"));
    }

    #[test]
    #[cfg(unix)]
    fn jsonl_plugin_courier_supports_capabilities_inspect_and_run() {
        let test_image = build_test_image(
            "\
FROM dispatch/custom:latest
ENTRYPOINT chat
",
            &[],
        );
        let (courier, _) =
            build_test_plugin_courier(&test_image._dir, &test_image.image.config.digest, false);

        let capabilities = futures::executor::block_on(courier.capabilities()).unwrap();
        assert_eq!(capabilities.courier_id, "demo-plugin");
        assert_eq!(capabilities.kind, CourierKind::Custom);
        assert!(capabilities.supports_chat);

        futures::executor::block_on(courier.validate_parcel(&test_image.image)).unwrap();
        let inspection = futures::executor::block_on(courier.inspect(&test_image.image)).unwrap();
        assert_eq!(inspection.courier_id, "demo-plugin");
        assert_eq!(inspection.entrypoint.as_deref(), Some("chat"));

        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
        assert_eq!(session.id, "plugin-session");
        assert_eq!(session.parcel_digest, test_image.image.config.digest);

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "hello plugin".to_string(),
                },
            },
        ))
        .unwrap();

        assert_eq!(response.courier_id, "demo-plugin");
        assert_eq!(response.session.turn_count, 1);
        assert!(matches!(
            response.events.first(),
            Some(CourierEvent::Message { role, content })
                if role == "assistant" && content == "from plugin"
        ));
    }

    #[test]
    #[cfg(unix)]
    fn jsonl_plugin_courier_surfaces_structured_errors() {
        let test_image = build_test_image(
            "\
FROM dispatch/custom:latest
ENTRYPOINT chat
",
            &[],
        );
        let (courier, _) =
            build_test_plugin_courier(&test_image._dir, &test_image.image.config.digest, true);

        let error = futures::executor::block_on(courier.inspect(&test_image.image)).unwrap_err();
        assert!(matches!(
            error,
            CourierError::PluginProtocol { courier, message }
                if courier == "demo-plugin" && message.contains("bad_request") && message.contains("plugin rejected request")
        ));
    }

    #[test]
    #[cfg(unix)]
    fn jsonl_plugin_reuses_persistent_process_across_turns() {
        let test_image = build_test_image(
            "\
FROM dispatch/custom:latest
ENTRYPOINT chat
",
            &[],
        );
        let (courier, _plugin_path, starts_path) =
            build_test_counting_plugin_courier(&test_image._dir, &test_image.image.config.digest);

        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
        let starts_after_open = fs::read_to_string(&starts_path).unwrap();
        assert_eq!(starts_after_open.lines().count(), 2);

        let first = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "first".to_string(),
                },
            },
        ))
        .unwrap();
        let starts_after_first = fs::read_to_string(&starts_path).unwrap();
        assert_eq!(starts_after_first.lines().count(), 2);
        assert_eq!(first.session.turn_count, 1);
        assert!(matches!(
            first.events.first(),
            Some(CourierEvent::Message { role, content })
                if role == "assistant" && content == "from plugin turn 1"
        ));

        let second = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session: first.session,
                operation: CourierOperation::Chat {
                    input: "second".to_string(),
                },
            },
        ))
        .unwrap();
        let starts_after_second = fs::read_to_string(&starts_path).unwrap();
        assert_eq!(starts_after_second.lines().count(), 2);
        assert_eq!(second.session.turn_count, 2);
        assert!(matches!(
            second.events.first(),
            Some(CourierEvent::Message { role, content })
                if role == "assistant" && content == "from plugin turn 2"
        ));
    }

    #[test]
    #[cfg(unix)]
    fn jsonl_plugin_resumes_persistent_session_after_new_host_process() {
        let test_image = build_test_image(
            "\
FROM dispatch/custom:latest
ENTRYPOINT chat
",
            &[],
        );
        let (courier, _plugin_path, starts_path) =
            build_test_counting_plugin_courier(&test_image._dir, &test_image.image.config.digest);

        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
        let first = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "first".to_string(),
                },
            },
        ))
        .unwrap();
        assert_eq!(first.session.turn_count, 1);

        let manifest = courier.manifest.clone();
        drop(courier);

        let starts_after_restart = fs::read_to_string(&starts_path).unwrap();
        assert_eq!(starts_after_restart.lines().count(), 2);

        let resumed_courier = JsonlCourierPlugin::new(manifest);
        let second = futures::executor::block_on(resumed_courier.run(
            &test_image.image,
            CourierRequest {
                session: first.session,
                operation: CourierOperation::Chat {
                    input: "second".to_string(),
                },
            },
        ))
        .unwrap();

        let starts_after_resume = fs::read_to_string(&starts_path).unwrap();
        assert_eq!(starts_after_resume.lines().count(), 3);
        assert_eq!(second.session.turn_count, 2);
        assert!(matches!(
            second.events.first(),
            Some(CourierEvent::Message { role, content })
                if role == "assistant" && content == "from plugin turn 2"
        ));
    }

    #[test]
    #[cfg(unix)]
    fn jsonl_plugin_sends_shutdown_to_persistent_process_on_drop() {
        let test_image = build_test_image(
            "\
FROM dispatch/custom:latest
ENTRYPOINT chat
",
            &[],
        );
        let dir = tempdir().unwrap();
        let (courier, shutdowns_path) =
            build_test_shutdown_plugin_courier(&dir, &test_image.image.config.digest);

        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
        let _ = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "hello".to_string(),
                },
            },
        ))
        .unwrap();

        drop(courier);

        let shutdowns = fs::read_to_string(shutdowns_path).unwrap();
        assert!(shutdowns.contains("shutdown"));
    }

    #[test]
    #[cfg(unix)]
    fn jsonl_plugin_courier_detects_executable_drift() {
        let test_image = build_test_image(
            "\
FROM dispatch/custom:latest
ENTRYPOINT chat
",
            &[],
        );
        let (courier, plugin_path) =
            build_test_plugin_courier(&test_image._dir, &test_image.image.config.digest, false);
        fs::write(&plugin_path, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&plugin_path, fs::Permissions::from_mode(0o755)).unwrap();

        let error = futures::executor::block_on(courier.capabilities()).unwrap_err();
        assert!(matches!(
            error,
            CourierError::PluginExecutableChanged { courier, .. } if courier == "demo-plugin"
        ));
    }

    #[test]
    fn list_local_tools_uses_typed_manifest() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
TOOL LOCAL tools/demo.py AS demo USING python3 -u
ENTRYPOINT job
",
            &[("tools/demo.py", "print('ok')")],
        );

        let tools = list_local_tools(&test_image.image);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].alias, "demo");
        assert_eq!(tools[0].command(), "python3");
        assert_eq!(tools[0].args(), ["-u".to_string()]);
        assert_eq!(tools[0].transport(), LocalToolTransport::Local);
    }

    #[test]
    fn list_local_tools_includes_a2a_tools() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
SECRET A2A_TOKEN
TOOL A2A broker URL https://broker.example.com DISCOVERY direct AUTH bearer A2A_TOKEN EXPECT_AGENT_NAME remote-broker EXPECT_CARD_SHA256 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa SCHEMA schemas/input.json DESCRIPTION \"Delegate to broker\"
ENTRYPOINT job
",
            &[(
                "schemas/input.json",
                "{\n  \"type\": \"object\",\n  \"properties\": {\n    \"query\": { \"type\": \"string\" }\n  },\n  \"required\": [\"query\"]\n}\n",
            )],
        );

        let tools = list_local_tools(&test_image.image);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].alias, "broker");
        assert_eq!(tools[0].transport(), LocalToolTransport::A2a);
        assert_eq!(tools[0].endpoint_url(), Some("https://broker.example.com"));
        assert_eq!(tools[0].endpoint_mode(), Some(A2aEndpointMode::Direct));
        assert_eq!(tools[0].auth_secret_name(), Some("A2A_TOKEN"));
        assert_eq!(tools[0].auth_scheme(), Some(A2aAuthScheme::Bearer));
        assert_eq!(tools[0].auth_header_name(), None);
        assert_eq!(tools[0].expected_agent_name(), Some("remote-broker"));
        assert_eq!(
            tools[0].expected_card_sha256(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(tools[0].command(), "dispatch-a2a");
    }

    #[test]
    fn native_courier_executes_a2a_tools_via_host_transport() {
        let server = start_test_a2a_server();
        let test_image = build_test_image(
            &format!(
                "\
FROM dispatch/native:latest
TOOL A2A broker URL {} DESCRIPTION \"Delegate to broker\"
ENTRYPOINT job
",
                server.base_url
            ),
            &[],
        );

        let result = run_local_tool(&test_image.image, "broker", Some("hello remote")).unwrap();
        assert_eq!(result.tool, "broker");
        assert_eq!(result.command, "dispatch-a2a");
        assert!(result.stdout.contains("echo:hello remote"));
    }

    #[test]
    fn native_courier_executes_a2a_tools_with_json_payloads() {
        let server = start_test_a2a_server();
        let test_image = build_test_image(
            &format!(
                "\
FROM dispatch/native:latest
TOOL A2A broker URL {} SCHEMA schemas/input.json
ENTRYPOINT job
",
                server.base_url
            ),
            &[(
                "schemas/input.json",
                "{\n  \"type\": \"object\",\n  \"properties\": {\n    \"query\": { \"type\": \"string\" }\n  },\n  \"required\": [\"query\"]\n}\n",
            )],
        );

        let result =
            run_local_tool(&test_image.image, "broker", Some("{\"query\":\"weather\"}")).unwrap();
        let output: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
        assert_eq!(
            output.pointer("/query").and_then(serde_json::Value::as_str),
            Some("weather")
        );
    }

    #[test]
    fn native_courier_executes_a2a_tools_with_bearer_auth() {
        let server = start_test_a2a_server_with_options(TestA2aServerOptions {
            expected_auth: Some("Bearer topsecret".to_string()),
            ..Default::default()
        });
        let test_image = build_test_image(
            &format!(
                "\
FROM dispatch/native:latest
SECRET A2A_TOKEN
TOOL A2A broker URL {} AUTH bearer A2A_TOKEN
ENTRYPOINT job
",
                server.base_url
            ),
            &[],
        );

        let result = run_local_tool_with_env(&test_image.image, "broker", Some("hello"), |name| {
            (name == "A2A_TOKEN").then(|| "topsecret".to_string())
        })
        .unwrap();
        assert!(result.stdout.contains("echo:hello"));
    }

    #[test]
    fn native_courier_executes_a2a_tools_with_header_auth() {
        let server = start_test_a2a_server_with_options(TestA2aServerOptions {
            expected_auth: Some("X-Api-Key: topsecret".to_string()),
            ..Default::default()
        });
        let test_image = build_test_image(
            &format!(
                "\
FROM dispatch/native:latest
SECRET API_KEY
TOOL A2A broker URL {} AUTH header X-Api-Key API_KEY
ENTRYPOINT job
",
                server.base_url
            ),
            &[],
        );

        let result = run_local_tool_with_env(&test_image.image, "broker", Some("hello"), |name| {
            (name == "API_KEY").then(|| "topsecret".to_string())
        })
        .unwrap();
        assert!(result.stdout.contains("echo:hello"));
    }

    #[test]
    fn native_courier_rejects_a2a_call_when_auth_secret_is_missing() {
        let server = start_test_a2a_server_with_options(TestA2aServerOptions {
            expected_auth: Some("Bearer topsecret".to_string()),
            ..Default::default()
        });
        let tool = LocalToolSpec {
            alias: "broker".to_string(),
            description: None,
            input_schema_packaged_path: None,
            input_schema_sha256: None,
            target: LocalToolTarget::A2a {
                endpoint_url: server.base_url.clone(),
                endpoint_mode: None,
                auth_secret_name: Some("A2A_TOKEN".to_string()),
                auth_scheme: Some(A2aAuthScheme::Bearer),
                auth_header_name: None,
                expected_agent_name: None,
                expected_card_sha256: None,
            },
        };

        let error = execute_a2a_tool_with_env(&tool, Some("hello"), |_| None, None).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("configured A2A auth secret `A2A_TOKEN` is not available")
        );
    }

    #[test]
    fn native_courier_rejects_a2a_agent_name_mismatch() {
        let server = start_test_a2a_server_with_options(TestA2aServerOptions {
            agent_name: Some("actual-agent".to_string()),
            ..Default::default()
        });
        let test_image = build_test_image(
            &format!(
                "\
FROM dispatch/native:latest
TOOL A2A broker URL {} EXPECT_AGENT_NAME expected-agent
ENTRYPOINT job
",
                server.base_url
            ),
            &[],
        );

        let error = run_local_tool(&test_image.image, "broker", Some("hello")).unwrap_err();
        assert!(
            error.to_string().contains(
                "agent card name mismatch: expected `expected-agent`, got `actual-agent`"
            )
        );
    }

    #[test]
    fn native_courier_rejects_a2a_agent_name_requirement_when_card_has_no_name() {
        let server = start_test_a2a_server_with_options(TestA2aServerOptions {
            agent_name: None,
            ..Default::default()
        });
        let test_image = build_test_image(
            &format!(
                "\
FROM dispatch/native:latest
TOOL A2A broker URL {} EXPECT_AGENT_NAME expected-agent
ENTRYPOINT job
",
                server.base_url
            ),
            &[],
        );

        let error = run_local_tool(&test_image.image, "broker", Some("hello")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("agent card did not include `name`, but `expected-agent` was required")
        );
    }

    #[test]
    fn native_courier_rejects_a2a_card_digest_mismatch() {
        let server = start_test_a2a_server();
        let test_image = build_test_image(
            &format!(
                "\
FROM dispatch/native:latest
TOOL A2A broker URL {} EXPECT_CARD_SHA256 ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff
ENTRYPOINT job
",
                server.base_url
            ),
            &[],
        );

        let error = run_local_tool(&test_image.image, "broker", Some("hello")).unwrap_err();
        assert!(error
            .to_string()
            .contains("agent card digest mismatch: expected `ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff`"));
    }

    #[test]
    fn native_courier_accepts_matching_a2a_card_digest() {
        let server = start_test_a2a_server_with_options(TestA2aServerOptions {
            agent_name: Some("demo-a2a".to_string()),
            ..Default::default()
        });
        let expected_card_sha256 = encode_hex(Sha256::digest(
            serde_json::to_vec(&serde_json::json!({
                "name": "demo-a2a",
                "url": format!("{}/a2a", server.base_url)
            }))
            .unwrap(),
        ));
        let test_image = build_test_image(
            &format!(
                "\
FROM dispatch/native:latest
TOOL A2A broker URL {} EXPECT_CARD_SHA256 {}
ENTRYPOINT job
",
                server.base_url, expected_card_sha256
            ),
            &[],
        );

        let result = run_local_tool(&test_image.image, "broker", Some("hello")).unwrap();
        assert!(result.stdout.contains("echo:hello"));
    }

    #[test]
    fn native_courier_rejects_a2a_card_origin_pivot() {
        let server = start_test_a2a_server_with_options(TestA2aServerOptions {
            card_url: Some("https://evil.example.com/a2a".to_string()),
            ..Default::default()
        });
        let test_image = build_test_image(
            &format!(
                "\
FROM dispatch/native:latest
TOOL A2A broker URL {}
ENTRYPOINT job
",
                server.base_url
            ),
            &[],
        );

        let error = run_local_tool(&test_image.image, "broker", Some("hello")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("discovered agent card URL must stay on the declared origin")
        );
    }

    #[test]
    fn native_courier_enforces_tool_timeout_for_a2a_tools() {
        let server = start_test_a2a_server_with_options(TestA2aServerOptions {
            response_delay: Duration::from_millis(200),
            ..Default::default()
        });
        let test_image = build_test_image(
            &format!(
                "\
FROM dispatch/native:latest
TIMEOUT TOOL 50ms
TOOL A2A broker URL {}
ENTRYPOINT job
",
                server.base_url
            ),
            &[],
        );

        let error = run_local_tool(&test_image.image, "broker", Some("hello")).unwrap_err();
        assert!(matches!(
            error,
            CourierError::ToolTimedOut { ref tool, ref timeout }
                if tool == "broker" && timeout == "TOOL"
        ));
    }

    #[test]
    fn native_courier_enforces_tool_timeout_for_local_tools() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
TIMEOUT TOOL 50ms
TOOL LOCAL tools/slow.py AS slow USING python3 -u
ENTRYPOINT job
",
            &[(
                "tools/slow.py",
                "import time\n\
time.sleep(0.2)\n\
print('done')\n",
            )],
        );

        let error = run_local_tool(&test_image.image, "slow", None).unwrap_err();
        assert!(matches!(
            error,
            CourierError::ToolTimedOut { ref tool, ref timeout } if tool == "slow" && timeout == "TOOL"
        ));
    }

    #[test]
    fn native_courier_caps_tool_timeout_by_remaining_run_budget() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
TIMEOUT RUN 100ms
TOOL LOCAL tools/slow.py AS slow USING python3 -u
ENTRYPOINT job
",
            &[(
                "tools/slow.py",
                "import time\n\
time.sleep(0.2)\n\
print('done')\n",
            )],
        );
        let courier = NativeCourier::default();
        let mut session =
            futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
        session.elapsed_ms = 60;

        let error = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::InvokeTool {
                    invocation: ToolInvocation {
                        name: "slow".to_string(),
                        input: None,
                    },
                },
            },
        ))
        .unwrap_err();

        assert!(matches!(
            error,
            CourierError::ToolTimedOut { ref tool, ref timeout }
                if tool == "slow" && timeout == "RUN"
        ));
    }

    #[test]
    fn native_courier_requires_card_discovery_when_configured() {
        let server = start_test_a2a_server_with_options(TestA2aServerOptions {
            publish_card: false,
            ..Default::default()
        });
        let test_image = build_test_image(
            &format!(
                "\
FROM dispatch/native:latest
TOOL A2A broker URL {} DISCOVERY card
ENTRYPOINT job
",
                server.base_url
            ),
            &[],
        );

        let error = run_local_tool(&test_image.image, "broker", Some("hello")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("agent card discovery failed for required `DISCOVERY card` mode")
        );
    }

    #[test]
    fn native_courier_polls_non_completed_a2a_tasks_until_completion() {
        let server = start_test_a2a_server_with_options(TestA2aServerOptions {
            task_state: "working".to_string(),
            task_status_message: "queued for async execution".to_string(),
            task_get_state: Some("completed".to_string()),
            task_get_status_message: Some("done".to_string()),
            ..Default::default()
        });
        let test_image = build_test_image(
            &format!(
                "\
FROM dispatch/native:latest
TOOL A2A broker URL {}
ENTRYPOINT job
",
                server.base_url
            ),
            &[],
        );

        let result = run_local_tool(&test_image.image, "broker", Some("hello")).unwrap();
        assert!(result.stdout.contains("echo:hello"));
    }

    #[test]
    fn native_courier_times_out_polling_non_completed_a2a_tasks() {
        let cancel_count = Arc::new(AtomicU64::new(0));
        let server = start_test_a2a_server_with_options(TestA2aServerOptions {
            task_state: "working".to_string(),
            task_status_message: "queued for async execution".to_string(),
            task_get_state: Some("working".to_string()),
            task_get_status_message: Some("still running".to_string()),
            cancel_count: Some(cancel_count.clone()),
            ..Default::default()
        });
        let test_image = build_test_image(
            &format!(
                "\
FROM dispatch/native:latest
TIMEOUT TOOL 75ms
TOOL A2A broker URL {}
ENTRYPOINT job
",
                server.base_url
            ),
            &[],
        );

        let error = run_local_tool(&test_image.image, "broker", Some("hello")).unwrap_err();
        assert!(matches!(
            error,
            CourierError::ToolTimedOut { ref tool, ref timeout }
                if tool == "broker" && timeout == "TOOL"
        ));
        assert_eq!(cancel_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn native_courier_surfaces_a2a_json_rpc_errors() {
        let server = start_test_a2a_server_with_options(TestA2aServerOptions {
            rpc_error: Some((-32001, "remote agent unavailable".to_string())),
            ..Default::default()
        });
        let test_image = build_test_image(
            &format!(
                "\
FROM dispatch/native:latest
TOOL A2A broker URL {}
ENTRYPOINT job
",
                server.base_url
            ),
            &[],
        );

        let error = run_local_tool(&test_image.image, "broker", Some("hello")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("JSON-RPC error -32001: remote agent unavailable")
        );
    }

    #[test]
    fn native_courier_rejects_a2a_url_outside_operator_allowlist() {
        let server = start_test_a2a_server();
        let test_image = build_test_image(
            &format!(
                "\
FROM dispatch/native:latest
TOOL A2A broker URL {}
ENTRYPOINT job
",
                server.base_url
            ),
            &[],
        );

        let error = run_local_tool_with_env(&test_image.image, "broker", Some("hello"), |name| {
            (name == "DISPATCH_A2A_ALLOWED_ORIGINS")
                .then(|| "https://agents.example.com,broker.internal".to_string())
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("is not allowed by DISPATCH_A2A_ALLOWED_ORIGINS")
        );
    }

    #[test]
    fn native_courier_allows_a2a_url_with_matching_operator_allowlist_origin() {
        let server = start_test_a2a_server();
        let parsed = url::Url::parse(&server.base_url).unwrap();
        let origin = a2a::a2a_origin(&parsed).unwrap();
        let test_image = build_test_image(
            &format!(
                "\
FROM dispatch/native:latest
TOOL A2A broker URL {}
ENTRYPOINT job
",
                server.base_url
            ),
            &[],
        );

        let result = run_local_tool_with_env(&test_image.image, "broker", Some("hello"), |name| {
            (name == "DISPATCH_A2A_ALLOWED_ORIGINS").then(|| origin.clone())
        })
        .unwrap();
        assert!(result.stdout.contains("echo:hello"));
    }

    #[test]
    fn native_courier_open_session_sets_identity_and_zero_turns() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
ENTRYPOINT chat
",
            &[],
        );
        let courier = NativeCourier::default();
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        assert!(
            session
                .id
                .starts_with(&format!("native-{}", test_image.image.config.digest))
        );
        assert_eq!(session.parcel_digest, test_image.image.config.digest);
        assert_eq!(session.entrypoint.as_deref(), Some("chat"));
        assert_eq!(session.turn_count, 0);
        assert!(session.history.is_empty());
    }

    #[test]
    fn native_courier_validate_parcel_rejects_foreign_courier_reference() {
        let test_image = build_test_image(
            "\
FROM example/remote-worker:latest
ENTRYPOINT chat
",
            &[],
        );
        let courier = NativeCourier::default();

        let error =
            futures::executor::block_on(courier.validate_parcel(&test_image.image)).unwrap_err();

        assert!(matches!(
            error,
            CourierError::IncompatibleCourier { courier, parcel_courier, .. }
                if courier == "native" && parcel_courier == "example/remote-worker:latest"
        ));
    }

    #[test]
    fn docker_courier_accepts_docker_image_reference() {
        let test_image = build_test_image(
            "\
FROM dispatch/docker:latest
ENTRYPOINT job
",
            &[],
        );
        let courier = DockerCourier::default();

        futures::executor::block_on(courier.validate_parcel(&test_image.image)).unwrap();
        let inspection = futures::executor::block_on(courier.inspect(&test_image.image)).unwrap();
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        assert_eq!(inspection.courier_id, "docker");
        assert_eq!(inspection.kind, CourierKind::Docker);
        assert_eq!(session.entrypoint.as_deref(), Some("job"));
        assert!(session.id.starts_with("docker-"));
    }

    #[test]
    fn docker_courier_rejects_native_image_reference() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
ENTRYPOINT chat
",
            &[],
        );
        let courier = DockerCourier::default();

        let error =
            futures::executor::block_on(courier.validate_parcel(&test_image.image)).unwrap_err();

        assert!(matches!(
            error,
            CourierError::IncompatibleCourier { courier, parcel_courier, .. }
                if courier == "docker" && parcel_courier == "dispatch/native:latest"
        ));
    }

    #[test]
    fn wasm_courier_accepts_component_backed_wasm_parcel() {
        let test_image = build_test_image(
            "\
FROM dispatch/wasm:latest
COMPONENT components/assistant.wat
SOUL SOUL.md
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT chat
",
            &[
                ("SOUL.md", "Soul body"),
                ("components/assistant.wat", "(component)"),
                ("tools/demo.sh", "printf ok"),
            ],
        );
        let courier = WasmCourier::default();

        futures::executor::block_on(courier.validate_parcel(&test_image.image)).unwrap();
        let inspection = futures::executor::block_on(courier.inspect(&test_image.image)).unwrap();
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        assert_eq!(inspection.courier_id, "wasm");
        assert_eq!(inspection.kind, CourierKind::Wasm);
        assert_eq!(inspection.local_tools.len(), 1);
        assert!(session.id.starts_with("wasm-"));
        assert_eq!(session.parcel_digest, test_image.image.config.digest);
        assert_eq!(session.backend_state, None);

        let prompt = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session: session.clone(),
                operation: CourierOperation::ResolvePrompt,
            },
        ))
        .unwrap();
        assert!(matches!(
            prompt.events.first(),
            Some(CourierEvent::PromptResolved { text }) if text.contains("Soul body")
        ));

        let tools = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::ListLocalTools,
            },
        ))
        .unwrap();
        assert!(matches!(
            tools.events.first(),
            Some(CourierEvent::LocalToolsListed { tools }) if tools.len() == 1 && tools[0].alias == "demo"
        ));
    }

    #[test]
    fn wasm_courier_executes_reference_guest_chat_with_model_and_tool_imports() {
        static REFERENCE_GUEST: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/dispatch-wasm-guest-reference.wasm"
        ));

        let test_image = build_test_image_with_binary_files(
            "\
FROM dispatch/wasm:latest
COMPONENT components/reference.wasm
SOUL SOUL.md
MODEL gpt-5-mini
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT chat
",
            &[
                ("SOUL.md", "Soul body"),
                ("tools/demo.sh", "printf 'tool-output'"),
            ],
            &[("components/reference.wasm", REFERENCE_GUEST)],
        );
        let backend = Arc::new(FakeChatBackend::with_reply("backend reply"));
        let courier = WasmCourier::with_chat_backend(backend.clone());
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let model_response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "model".to_string(),
                },
            },
        ))
        .unwrap();

        assert_eq!(model_response.session.turn_count, 1);
        let expected_model_state = format!("opened:{}:1", test_image.image.config.digest);
        assert_eq!(
            model_response.session.backend_state.as_deref(),
            Some(expected_model_state.as_str())
        );
        assert!(matches!(
            model_response.events.first(),
            Some(CourierEvent::Message { role, content })
                if role == "assistant" && content == "backend reply"
        ));
        assert_eq!(model_response.session.history.len(), 2);
        assert_eq!(model_response.session.history[0].content, "model");
        assert_eq!(model_response.session.history[1].content, "backend reply");

        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].model, "gpt-5-mini");
        assert!(calls[0].instructions.contains("Soul body"));
        assert_eq!(calls[0].messages.len(), 1);
        assert_eq!(calls[0].messages[0].content, "model");
        drop(calls);

        let tool_response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session: model_response.session,
                operation: CourierOperation::Chat {
                    input: "tool demo".to_string(),
                },
            },
        ))
        .unwrap();

        assert_eq!(tool_response.session.turn_count, 2);
        let expected_tool_state = format!("opened:{}:2", test_image.image.config.digest);
        assert_eq!(
            tool_response.session.backend_state.as_deref(),
            Some(expected_tool_state.as_str())
        );
        assert!(matches!(
            tool_response.events.first(),
            Some(CourierEvent::Message { role, content })
                if role == "assistant" && content.contains("tool demo ok: tool-output")
        ));
        assert_eq!(tool_response.session.history.len(), 4);
        assert_eq!(tool_response.session.history[2].content, "tool demo");
    }

    #[test]
    fn wasm_courier_host_model_complete_uses_fallback_models() {
        static REFERENCE_GUEST: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/dispatch-wasm-guest-reference.wasm"
        ));

        let test_image = build_test_image_with_binary_files(
            "\
FROM dispatch/wasm:latest
COMPONENT components/reference.wasm
SOUL SOUL.md
MODEL primary-model
FALLBACK fallback-model
ENTRYPOINT chat
",
            &[("SOUL.md", "Soul body")],
            &[("components/reference.wasm", REFERENCE_GUEST)],
        );
        let backend = Arc::new(FakeChatBackend::with_replies(vec![
            None,
            Some(ModelReply {
                text: Some("fallback wasm reply".to_string()),
                backend: "fake".to_string(),
                response_id: None,
                tool_calls: Vec::new(),
            }),
        ]));
        let courier = WasmCourier::with_chat_backend(backend.clone());
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "model".to_string(),
                },
            },
        ))
        .unwrap();

        assert!(matches!(
            response.events.first(),
            Some(CourierEvent::Message { role, content })
                if role == "assistant" && content == "fallback wasm reply"
        ));

        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].model, "primary-model");
        assert_eq!(calls[1].model, "fallback-model");
    }

    #[test]
    fn wasm_courier_executes_reference_guest_job_and_heartbeat() {
        static REFERENCE_GUEST: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/dispatch-wasm-guest-reference.wasm"
        ));

        let job_image = build_test_image_with_binary_files(
            "\
FROM dispatch/wasm:latest
COMPONENT components/reference.wasm
ENTRYPOINT job
",
            &[],
            &[("components/reference.wasm", REFERENCE_GUEST)],
        );
        let heartbeat_image = build_test_image_with_binary_files(
            "\
FROM dispatch/wasm:latest
COMPONENT components/reference.wasm
ENTRYPOINT heartbeat
",
            &[],
            &[("components/reference.wasm", REFERENCE_GUEST)],
        );
        let courier = WasmCourier::default();

        let job_session =
            futures::executor::block_on(courier.open_session(&job_image.image)).unwrap();
        let job_response = futures::executor::block_on(courier.run(
            &job_image.image,
            CourierRequest {
                session: job_session,
                operation: CourierOperation::Job {
                    payload: "{\"task\":\"ping\"}".to_string(),
                },
            },
        ))
        .unwrap();
        assert!(matches!(
            job_response.events.first(),
            Some(CourierEvent::Message { role, content })
                if role == "assistant" && content == "job accepted: {\"task\":\"ping\"}"
        ));
        assert_eq!(job_response.session.turn_count, 1);
        let expected_job_state = format!("opened:{}:1", job_image.image.config.digest);
        assert_eq!(
            job_response.session.backend_state.as_deref(),
            Some(expected_job_state.as_str())
        );

        let heartbeat_session =
            futures::executor::block_on(courier.open_session(&heartbeat_image.image)).unwrap();
        let heartbeat_response = futures::executor::block_on(courier.run(
            &heartbeat_image.image,
            CourierRequest {
                session: heartbeat_session,
                operation: CourierOperation::Heartbeat {
                    payload: Some("tick".to_string()),
                },
            },
        ))
        .unwrap();
        assert!(matches!(
            heartbeat_response.events.first(),
            Some(CourierEvent::TextDelta { content }) if content == "heartbeat:tick"
        ));
        assert_eq!(heartbeat_response.session.turn_count, 1);
        let expected_heartbeat_state = format!("opened:{}:1", heartbeat_image.image.config.digest);
        assert_eq!(
            heartbeat_response.session.backend_state.as_deref(),
            Some(expected_heartbeat_state.as_str())
        );
    }

    #[test]
    fn wasm_courier_reference_guest_memory_persists_across_sessions() {
        static REFERENCE_GUEST: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/dispatch-wasm-guest-reference.wasm"
        ));

        let test_image = build_test_image_with_binary_files(
            "\
FROM dispatch/wasm:latest
COMPONENT components/reference.wasm
MOUNT SESSION sqlite
MOUNT MEMORY sqlite
ENTRYPOINT chat
",
            &[],
            &[("components/reference.wasm", REFERENCE_GUEST)],
        );
        let courier = WasmCourier::default();

        let first_session =
            futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
        let first_response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session: first_session,
                operation: CourierOperation::Chat {
                    input: "remember profile:name Christian".to_string(),
                },
            },
        ))
        .unwrap();
        assert!(matches!(
            first_response.events.first(),
            Some(CourierEvent::Message { content, .. }) if content == "remembered profile:name"
        ));

        let second_session =
            futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
        let second_response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session: second_session,
                operation: CourierOperation::Chat {
                    input: "recall profile:name".to_string(),
                },
            },
        ))
        .unwrap();
        assert!(matches!(
            second_response.events.first(),
            Some(CourierEvent::Message { content, .. }) if content == "profile:name = Christian"
        ));
    }

    #[test]
    fn docker_courier_can_resolve_prompt_and_list_tools() {
        let test_image = build_test_image(
            "\
FROM dispatch/docker:latest
SOUL SOUL.md
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT chat
",
            &[("SOUL.md", "Soul body"), ("tools/demo.sh", "printf ok")],
        );
        let courier = DockerCourier::default();
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let prompt = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session: session.clone(),
                operation: CourierOperation::ResolvePrompt,
            },
        ))
        .unwrap();
        let tools = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::ListLocalTools,
            },
        ))
        .unwrap();

        assert!(matches!(
            prompt.events.first(),
            Some(CourierEvent::PromptResolved { text }) if text.contains("Soul body")
        ));
        assert!(matches!(
            tools.events.first(),
            Some(CourierEvent::LocalToolsListed { tools }) if tools.len() == 1 && tools[0].alias == "demo"
        ));
    }

    #[test]
    fn docker_courier_chat_executes_reference_reply_and_records_history() {
        let test_image = build_test_image(
            "\
FROM dispatch/docker:latest
ENTRYPOINT chat
",
            &[],
        );
        let courier = DockerCourier::default();
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "hello".to_string(),
                },
            },
        ))
        .unwrap();

        assert!(matches!(
            response.events.first(),
            Some(CourierEvent::Message { content, .. })
                if content.contains("Docker chat reference reply")
        ));
        assert_eq!(response.session.turn_count, 1);
        assert_eq!(response.session.history.len(), 2);
    }

    #[test]
    #[cfg(unix)]
    fn docker_courier_invokes_local_tools_via_docker_cli() {
        let dir = tempdir().unwrap();
        let docker_bin = dir.path().join("docker");
        fs::write(
            &docker_bin,
            "\
#!/bin/sh
index=1
for arg in \"$@\"; do
printf 'arg%d=%s\\n' \"$index\" \"$arg\"
index=$((index + 1))
done
cat >/dev/null
",
        )
        .unwrap();
        fs::set_permissions(&docker_bin, fs::Permissions::from_mode(0o755)).unwrap();

        let test_image = build_test_image(
            "\
FROM dispatch/docker:latest
TOOL LOCAL tools/demo.sh AS demo
ENV CAST_VISIBLE_ENV=visible
ENTRYPOINT job
",
            &[("tools/demo.sh", "printf ok")],
        );
        let courier = DockerCourier::new(&docker_bin, "python:3.13-alpine");
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::InvokeTool {
                    invocation: ToolInvocation {
                        name: "demo".to_string(),
                        input: Some("{\"ping\":true}".to_string()),
                    },
                },
            },
        ))
        .unwrap();

        assert!(matches!(
            response.events.first(),
            Some(CourierEvent::ToolCallStarted { command, .. }) if command == "sh"
        ));
        let CourierEvent::ToolCallFinished { result } = &response.events[1] else {
            panic!("expected tool call finished event");
        };
        assert_eq!(result.tool, "demo");
        assert_eq!(result.command, "sh");
        assert!(result.stdout.contains("arg1=run"));
        assert!(result.stdout.contains("arg2=--rm"));
        assert!(result.stdout.contains("arg3=-i"));
        assert!(result.stdout.contains("arg4=--workdir"));
        assert!(result.stdout.contains("arg5=/workspace/context"));
        assert!(result.stdout.contains("CAST_VISIBLE_ENV=visible"));
        assert!(result.stdout.contains("TOOL_INPUT={\"ping\":true}"));
        assert!(result.stdout.contains("python:3.13-alpine"));
        assert!(result.stdout.contains("tools/demo.sh"));
        assert!(matches!(response.events.last(), Some(CourierEvent::Done)));
    }

    #[test]
    #[cfg(unix)]
    fn docker_courier_enforces_tool_timeout_for_local_tools() {
        let dir = tempdir().unwrap();
        let docker_bin = dir.path().join("docker");
        fs::write(&docker_bin, "#!/bin/sh\nsleep 0.2\ncat >/dev/null\n").unwrap();
        fs::set_permissions(&docker_bin, fs::Permissions::from_mode(0o755)).unwrap();

        let test_image = build_test_image(
            "\
FROM dispatch/docker:latest
TIMEOUT TOOL 50ms
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT job
",
            &[("tools/demo.sh", "printf ok")],
        );
        let courier = DockerCourier::new(&docker_bin, "python:3.13-alpine");
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let error = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::InvokeTool {
                    invocation: ToolInvocation {
                        name: "demo".to_string(),
                        input: None,
                    },
                },
            },
        ))
        .unwrap_err();

        assert!(matches!(
            error,
            CourierError::ToolTimedOut { ref tool, ref timeout }
                if tool == "demo" && timeout == "TOOL"
        ));
    }

    #[test]
    #[cfg(unix)]
    fn docker_courier_chat_executes_model_tool_calls_via_docker_cli() {
        let dir = tempdir().unwrap();
        let docker_bin = dir.path().join("docker");
        fs::write(
            &docker_bin,
            "#!/bin/sh\nprintf 'docker-tool-output'\ncat >/dev/null\n",
        )
        .unwrap();
        fs::set_permissions(&docker_bin, fs::Permissions::from_mode(0o755)).unwrap();

        let test_image = build_test_image(
            "\
FROM dispatch/docker:latest
MODEL gpt-5-mini
TOOL LOCAL tools/demo.sh AS demo SCHEMA schemas/demo.json
ENTRYPOINT chat
",
            &[
                ("tools/demo.sh", "printf ok"),
                (
                    "schemas/demo.json",
                    "{\n  \"type\": \"object\",\n  \"properties\": {\n    \"query\": { \"type\": \"string\" }\n  },\n  \"required\": [\"query\"]\n}",
                ),
            ],
        );
        let backend = Arc::new(FakeChatBackend::with_replies(vec![
            Some(ModelReply {
                text: None,
                backend: "fake".to_string(),
                response_id: Some("resp_1".to_string()),
                tool_calls: vec![ModelToolCall {
                    call_id: "call_1".to_string(),
                    name: "demo".to_string(),
                    input: "{\"query\":\"ping\"}".to_string(),
                    kind: ModelToolKind::Function,
                }],
            }),
            Some(ModelReply {
                text: Some("docker final answer".to_string()),
                backend: "fake".to_string(),
                response_id: Some("resp_2".to_string()),
                tool_calls: Vec::new(),
            }),
        ]));
        let courier = DockerCourier::new(&docker_bin, "python:3.13-alpine")
            .with_chat_backend(backend.clone());
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "use the function tool".to_string(),
                },
            },
        ))
        .unwrap();

        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].tool_outputs.len(), 1);
        assert!(
            calls[1].tool_outputs[0]
                .output
                .contains("docker-tool-output")
        );
        drop(calls);

        assert!(matches!(
            response.events.get(1),
            Some(CourierEvent::ToolCallFinished { result })
                if result.tool == "demo" && result.stdout.contains("docker-tool-output")
        ));
        assert!(matches!(
            response.events.iter().rev().nth(1),
            Some(CourierEvent::Message { content, .. }) if content == "docker final answer"
        ));
    }

    #[test]
    fn native_courier_prompt_run_emits_events_and_increments_turns() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
SOUL SOUL.md
ENTRYPOINT chat
",
            &[("SOUL.md", "Soul body")],
        );
        let courier = NativeCourier::default();
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::ResolvePrompt,
            },
        ))
        .unwrap();

        assert_eq!(response.session.turn_count, 1);
        assert!(matches!(
            response.events.first(),
            Some(CourierEvent::PromptResolved { text }) if text.contains("Soul body")
        ));
        assert!(matches!(response.events.last(), Some(CourierEvent::Done)));
    }

    #[test]
    fn native_courier_chat_rejects_mismatched_entrypoint() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
ENTRYPOINT job
",
            &[],
        );
        let courier = NativeCourier::default();
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let error = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "hello".to_string(),
                },
            },
        ))
        .unwrap_err();

        assert!(matches!(
            error,
            CourierError::EntrypointMismatch { entrypoint, operation }
                if entrypoint == "job" && operation == "chat"
        ));
    }

    #[test]
    fn native_courier_run_rejects_session_for_different_parcel() {
        let first_image = build_test_image(
            "\
FROM dispatch/native:latest
ENTRYPOINT chat
",
            &[],
        );
        let second_image = build_test_image(
            "\
FROM dispatch/native:latest
SOUL SOUL.md
ENTRYPOINT chat
",
            &[("SOUL.md", "different")],
        );
        let courier = NativeCourier::default();
        let session =
            futures::executor::block_on(courier.open_session(&first_image.image)).unwrap();

        let error = futures::executor::block_on(courier.run(
            &second_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::ResolvePrompt,
            },
        ))
        .unwrap_err();

        assert!(matches!(
            error,
            CourierError::SessionParcelMismatch { session_parcel_digest, parcel_digest }
                if session_parcel_digest == first_image.image.config.digest
                    && parcel_digest == second_image.image.config.digest
        ));
    }

    #[test]
    fn native_courier_tool_run_emits_started_and_finished_events() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT job
",
            &[("tools/demo.sh", "printf '{\"ok\":true}'")],
        );
        let courier = NativeCourier::default();
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::InvokeTool {
                    invocation: ToolInvocation {
                        name: "demo".to_string(),
                        input: Some("{\"ping\":true}".to_string()),
                    },
                },
            },
        ))
        .unwrap();

        assert_eq!(response.session.turn_count, 1);
        assert!(matches!(
            response.events.first(),
            Some(CourierEvent::ToolCallStarted { command, .. }) if command == "sh"
        ));
        assert!(matches!(
            response.events.get(1),
            Some(CourierEvent::ToolCallFinished { result }) if result.exit_code == 0 && result.stdout.contains("{\"ok\":true}")
        ));
        assert!(matches!(response.events.last(), Some(CourierEvent::Done)));
    }

    #[test]
    fn native_courier_chat_emits_assistant_message_and_records_history() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
SOUL SOUL.md
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT chat
",
            &[("SOUL.md", "Soul body"), ("tools/demo.sh", "printf ok")],
        );
        let courier = NativeCourier::default();
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "hello courier".to_string(),
                },
            },
        ))
        .unwrap();

        assert_eq!(response.session.turn_count, 1);
        assert_eq!(response.session.history.len(), 2);
        assert_eq!(response.session.history[0].role, "user");
        assert_eq!(response.session.history[0].content, "hello courier");
        assert_eq!(response.session.history[1].role, "assistant");
        assert!(response.session.history[1].content.contains("turn 1"));
        assert!(response.session.history[1].content.contains("1 tool"));
        assert!(matches!(
            response.events.first(),
            Some(CourierEvent::Message { role, content })
                if role == "assistant" && content.contains("hello courier")
        ));
    }

    #[test]
    fn builtin_mounts_scope_session_state_per_session_and_memory_per_parcel() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
MOUNT SESSION sqlite
MOUNT MEMORY sqlite
MOUNT ARTIFACTS local
ENTRYPOINT chat
",
            &[],
        );
        let courier = NativeCourier::default();
        let first_session =
            futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
        let second_session =
            futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let first_session_db = mount_path(&first_session, MountKind::Session, "sqlite");
        let second_session_db = mount_path(&second_session, MountKind::Session, "sqlite");
        let first_memory_db = mount_path(&first_session, MountKind::Memory, "sqlite");
        let second_memory_db = mount_path(&second_session, MountKind::Memory, "sqlite");
        let first_artifacts = mount_path(&first_session, MountKind::Artifacts, "local");
        let second_artifacts = mount_path(&second_session, MountKind::Artifacts, "local");

        assert_ne!(first_session_db, second_session_db);
        assert!(first_session_db.contains("/sessions/"));
        assert!(second_session_db.contains("/sessions/"));
        assert_eq!(first_memory_db, second_memory_db);
        assert!(first_memory_db.ends_with("memory.sqlite"));
        assert_eq!(first_artifacts, second_artifacts);
        assert!(first_artifacts.ends_with("artifacts"));
    }

    #[test]
    fn builtin_mounts_use_explicit_state_root_for_custom_output_layouts() {
        let root = tempdir().unwrap();
        let output_root = root.path().join("pulled");
        let test_image = build_test_image_with_output_root(
            "\
FROM dispatch/native:latest
MOUNT SESSION sqlite
MOUNT MEMORY sqlite
MOUNT ARTIFACTS local
ENTRYPOINT chat
",
            &[],
            &output_root,
        );
        let courier = NativeCourier::default();
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let session_db = mount_path(&session, MountKind::Session, "sqlite");
        let memory_db = mount_path(&session, MountKind::Memory, "sqlite");
        let artifacts_dir = mount_path(&session, MountKind::Artifacts, "local");

        let expected_root = output_root
            .canonicalize()
            .unwrap()
            .join(".dispatch-state")
            .join(&test_image.image.config.digest);
        assert!(session_db.starts_with(expected_root.join("sessions").to_string_lossy().as_ref()));
        assert_eq!(
            memory_db,
            expected_root.join("memory.sqlite").to_string_lossy()
        );
        assert_eq!(
            artifacts_dir,
            expected_root.join("artifacts").to_string_lossy()
        );
    }

    #[test]
    fn native_courier_memory_sqlite_persists_across_sessions() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
MOUNT SESSION sqlite
MOUNT MEMORY sqlite
ENTRYPOINT chat
",
            &[],
        );
        let courier = NativeCourier::default();
        let first_session =
            futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let first_response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session: first_session,
                operation: CourierOperation::Chat {
                    input: "/memory put profile:name Christian".to_string(),
                },
            },
        ))
        .unwrap();
        assert!(matches!(
            first_response.events.first(),
            Some(CourierEvent::Message { content, .. }) if content == "Stored memory profile:name"
        ));

        let second_session =
            futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
        let second_response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session: second_session,
                operation: CourierOperation::Chat {
                    input: "/memory get profile:name".to_string(),
                },
            },
        ))
        .unwrap();

        assert!(matches!(
            second_response.events.first(),
            Some(CourierEvent::Message { content, .. }) if content == "profile:name = Christian"
        ));
    }

    #[test]
    fn native_courier_memory_put_reports_updates_after_first_write() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
MOUNT SESSION sqlite
MOUNT MEMORY sqlite
ENTRYPOINT chat
",
            &[],
        );
        let courier = NativeCourier::default();
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let first = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "/memory put profile:name Christian".to_string(),
                },
            },
        ))
        .unwrap();
        assert!(matches!(
            first.events.first(),
            Some(CourierEvent::Message { content, .. }) if content == "Stored memory profile:name"
        ));

        let second = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session: first.session,
                operation: CourierOperation::Chat {
                    input: "/memory put profile:name Chris".to_string(),
                },
            },
        ))
        .unwrap();
        assert!(matches!(
            second.events.first(),
            Some(CourierEvent::Message { content, .. }) if content == "Updated memory profile:name"
        ));

        let third = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session: second.session,
                operation: CourierOperation::Chat {
                    input: "/memory get profile:name".to_string(),
                },
            },
        ))
        .unwrap();
        assert!(matches!(
            third.events.first(),
            Some(CourierEvent::Message { content, .. }) if content == "profile:name = Chris"
        ));
    }

    #[test]
    fn native_memory_list_treats_underscore_as_literal_prefix_character() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
MOUNT SESSION sqlite
MOUNT MEMORY sqlite
ENTRYPOINT chat
",
            &[],
        );
        let courier = NativeCourier::default();
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let session = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "/memory put default:user_1 first".to_string(),
                },
            },
        ))
        .unwrap()
        .session;
        let session = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "/memory put default:userA second".to_string(),
                },
            },
        ))
        .unwrap()
        .session;
        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "/memory list default:user_".to_string(),
                },
            },
        ))
        .unwrap();

        let CourierEvent::Message { content, .. } = response.events.first().unwrap() else {
            panic!("expected message event");
        };
        assert!(content.contains("default:user_1 = first"));
        assert!(!content.contains("default:userA = second"));
    }

    #[test]
    fn wasm_courier_reference_guest_rejects_memory_ops_without_memory_mount() {
        static REFERENCE_GUEST: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/dispatch-wasm-guest-reference.wasm"
        ));

        let test_image = build_test_image_with_binary_files(
            "\
FROM dispatch/wasm:latest
COMPONENT components/reference.wasm
MOUNT MEMORY none
ENTRYPOINT chat
",
            &[],
            &[("components/reference.wasm", REFERENCE_GUEST)],
        );
        let courier = WasmCourier::default();
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let error = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "remember profile:name Christian".to_string(),
                },
            },
        ))
        .unwrap_err();

        assert!(matches!(
            error,
            CourierError::WasmGuest { message, .. }
                if message.contains("memory put failed")
                    && message.contains("does not declare a usable memory mount")
        ));
    }

    #[test]
    fn build_model_request_uses_primary_model_prompt_and_history() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
SOUL SOUL.md
MODEL gpt-5-mini
ENTRYPOINT chat
",
            &[("SOUL.md", "Soul body")],
        );

        let local_tools = list_local_tools(&test_image.image);
        let request = build_model_request(
            &test_image.image,
            &[ConversationMessage {
                role: "user".to_string(),
                content: "hello".to_string(),
            }],
            &local_tools,
        )
        .unwrap()
        .expect("expected model request");

        assert_eq!(request.model, "gpt-5-mini");
        assert!(request.instructions.contains("Soul body"));
        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].content, "hello");
        assert!(request.tool_outputs.is_empty());
        assert!(request.previous_response_id.is_none());
    }

    #[test]
    fn build_model_request_uses_declared_tool_description() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL LOCAL tools/demo.sh AS demo DESCRIPTION \"Look up a record by id. Input: JSON with an id field.\"
ENTRYPOINT chat
",
            &[("tools/demo.sh", "printf ok")],
        );

        let local_tools = list_local_tools(&test_image.image);
        let request = build_model_request(
            &test_image.image,
            &[ConversationMessage {
                role: "user".to_string(),
                content: "hello".to_string(),
            }],
            &local_tools,
        )
        .unwrap()
        .expect("expected model request");

        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.tools[0].name, "demo");
        assert_eq!(
            request.tools[0].description,
            "Look up a record by id. Input: JSON with an id field."
        );
        assert!(matches!(request.tools[0].format, ModelToolFormat::Text));
    }

    #[test]
    fn build_model_request_loads_declared_tool_input_schema() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL LOCAL tools/demo.sh AS demo SCHEMA schemas/demo.json
ENTRYPOINT chat
",
            &[
                ("tools/demo.sh", "printf ok"),
                (
                    "schemas/demo.json",
                    "{\n  \"type\": \"object\",\n  \"properties\": {\n    \"id\": { \"type\": \"string\" }\n  },\n  \"required\": [\"id\"]\n}",
                ),
            ],
        );

        let local_tools = list_local_tools(&test_image.image);
        let request = build_model_request(
            &test_image.image,
            &[ConversationMessage {
                role: "user".to_string(),
                content: "hello".to_string(),
            }],
            &local_tools,
        )
        .unwrap()
        .expect("expected model request");

        assert_eq!(request.tools.len(), 1);
        match &request.tools[0].format {
            ModelToolFormat::JsonSchema { schema } => {
                assert_eq!(schema["type"], "object");
                assert_eq!(schema["required"][0], "id");
            }
            other => panic!("expected json schema tool format, got {other:?}"),
        }
    }

    #[test]
    fn list_native_builtin_tools_only_exposes_supported_memory_capabilities() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL BUILTIN memory_put
TOOL BUILTIN memory_get DESCRIPTION \"Read remembered state.\"
TOOL BUILTIN web_search
ENTRYPOINT chat
",
            &[],
        );

        let tools = list_native_builtin_tools(&test_image.image);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].capability, "memory_put");
        assert_eq!(tools[1].capability, "memory_get");
        assert_eq!(
            tools[1].description.as_deref(),
            Some("Read remembered state.")
        );
    }

    #[test]
    fn build_model_request_includes_supported_builtin_memory_tools() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL BUILTIN memory_put
TOOL BUILTIN memory_get
ENTRYPOINT chat
",
            &[],
        );

        let request = build_model_request(
            &test_image.image,
            &[ConversationMessage {
                role: "user".to_string(),
                content: "remember this".to_string(),
            }],
            &[],
        )
        .unwrap()
        .expect("expected model request");

        assert_eq!(request.tools.len(), 2);
        assert_eq!(request.tools[0].name, "memory_put");
        assert!(matches!(
            request.tools[0].format,
            ModelToolFormat::JsonSchema { .. }
        ));
        assert_eq!(request.tools[1].name, "memory_get");
    }

    #[test]
    fn build_model_request_rejects_tampered_packaged_tool_schema() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL LOCAL tools/demo.sh AS demo SCHEMA schemas/demo.json
ENTRYPOINT chat
",
            &[
                ("tools/demo.sh", "printf ok"),
                (
                    "schemas/demo.json",
                    "{\n  \"type\": \"object\",\n  \"properties\": {\n    \"id\": { \"type\": \"string\" }\n  }\n}",
                ),
            ],
        );
        fs::write(
            test_image
                .image
                .parcel_dir
                .join("context/schemas/demo.json"),
            "{ \"type\": \"array\" }",
        )
        .unwrap();

        let local_tools = list_local_tools(&test_image.image);
        let error = build_model_request(&test_image.image, &[], &local_tools).unwrap_err();
        assert!(matches!(
            error,
            CourierError::ToolSchemaDigestMismatch { tool, .. } if tool == "demo"
        ));
    }

    #[test]
    fn openai_tool_definition_uses_function_shape_for_schema_tools() {
        let value = openai_tool_definition(&ModelToolDefinition {
            name: "demo".to_string(),
            description: "Search by id".to_string(),
            format: ModelToolFormat::JsonSchema {
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" }
                    }
                }),
            },
        });

        assert_eq!(value["type"], "function");
        assert_eq!(value["name"], "demo");
        assert_eq!(value["parameters"]["type"], "object");
    }

    #[test]
    fn default_chat_backend_selects_openai_compatible_from_env() {
        let backend = default_chat_backend_for_provider_with(None, |name| match name {
            "LLM_BACKEND" => Some("openai_compatible".to_string()),
            _ => None,
        });

        assert_eq!(backend.id(), "openai_compatible_chat_completions");
        assert!(!backend.supports_previous_response_id());
    }

    #[test]
    fn default_chat_backend_selects_anthropic_from_env() {
        let backend = default_chat_backend_for_provider_with(None, |name| match name {
            "LLM_BACKEND" => Some("anthropic".to_string()),
            _ => None,
        });

        assert_eq!(backend.id(), "anthropic_messages");
        assert!(!backend.supports_previous_response_id());
    }

    #[test]
    fn default_chat_backend_selects_gemini_from_env() {
        let backend = default_chat_backend_for_provider_with(None, |name| match name {
            "LLM_BACKEND" => Some("gemini".to_string()),
            _ => None,
        });

        assert_eq!(backend.id(), "google_gemini_generate_content");
        assert!(!backend.supports_previous_response_id());
    }

    #[test]
    fn default_chat_backend_prefers_model_provider_over_env() {
        let backend =
            default_chat_backend_for_provider_with(Some("anthropic"), |name| match name {
                "LLM_BACKEND" => Some("openai".to_string()),
                _ => None,
            });

        assert_eq!(backend.id(), "anthropic_messages");
    }

    #[test]
    fn extract_openai_chat_completions_output_parses_tool_calls() {
        let body = serde_json::json!({
            "choices": [
                {
                    "message": {
                        "role": "assistant",
                        "tool_calls": [
                            {
                                "id": "call_fn",
                                "type": "function",
                                "function": {
                                    "name": "lookup",
                                    "arguments": "{\"id\":\"123\"}"
                                }
                            },
                            {
                                "id": "call_custom",
                                "type": "custom",
                                "custom": {
                                    "name": "shell",
                                    "input": "echo hi"
                                }
                            }
                        ]
                    }
                }
            ]
        });

        let reply = match extract_openai_chat_completions_output(&body).unwrap() {
            ModelGeneration::Reply(reply) => reply,
            ModelGeneration::NotConfigured { backend, reason } => {
                panic!("expected model reply, got unconfigured backend {backend}: {reason}");
            }
        };
        assert_eq!(reply.backend, "openai_compatible_chat_completions");
        assert!(reply.text.is_none());
        assert_eq!(reply.tool_calls.len(), 2);
        assert_eq!(reply.tool_calls[0].kind, ModelToolKind::Function);
        assert_eq!(reply.tool_calls[0].name, "lookup");
        assert_eq!(reply.tool_calls[1].kind, ModelToolKind::Custom);
        assert_eq!(reply.tool_calls[1].input, "echo hi");
    }

    #[test]
    fn extract_anthropic_output_parses_tool_use_blocks() {
        let body = serde_json::json!({
            "id": "msg_123",
            "content": [
                { "type": "text", "text": "Let me check." },
                {
                    "type": "tool_use",
                    "id": "toolu_123",
                    "name": "lookup",
                    "input": { "id": "123" }
                }
            ]
        });

        let reply = match extract_anthropic_output(&body).unwrap() {
            ModelGeneration::Reply(reply) => reply,
            ModelGeneration::NotConfigured { backend, reason } => {
                panic!("expected anthropic reply, got unconfigured backend {backend}: {reason}");
            }
        };
        assert_eq!(reply.backend, "anthropic_messages");
        assert_eq!(reply.response_id.as_deref(), Some("msg_123"));
        assert_eq!(reply.text.as_deref(), Some("Let me check."));
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].name, "lookup");
        assert_eq!(reply.tool_calls[0].kind, ModelToolKind::Function);
        assert_eq!(reply.tool_calls[0].input, "{\"id\":\"123\"}");
    }

    #[test]
    fn extract_gemini_output_parses_function_calls() {
        let body = serde_json::json!({
            "candidates": [
                {
                    "content": {
                        "parts": [
                            { "text": "Checking..." },
                            {
                                "functionCall": {
                                    "name": "lookup",
                                    "args": { "id": "123" }
                                }
                            }
                        ]
                    }
                }
            ]
        });

        let reply = match extract_gemini_output(&body).unwrap() {
            ModelGeneration::Reply(reply) => reply,
            ModelGeneration::NotConfigured { backend, reason } => {
                panic!("expected gemini reply, got unconfigured backend {backend}: {reason}");
            }
        };
        assert_eq!(reply.backend, "google_gemini_generate_content");
        assert_eq!(reply.text.as_deref(), Some("Checking..."));
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].name, "lookup");
        assert_eq!(reply.tool_calls[0].kind, ModelToolKind::Function);
        assert_eq!(reply.tool_calls[0].input, "{\"id\":\"123\"}");
    }

    #[test]
    fn extract_openai_output_parses_function_calls() {
        let body = serde_json::json!({
            "output": [
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "demo",
                    "arguments": "{\"id\":\"123\"}"
                }
            ]
        });

        let (text, tool_calls) = extract_openai_output(&body).unwrap();

        assert!(text.is_none());
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "demo");
        assert_eq!(tool_calls[0].kind, ModelToolKind::Function);
        assert_eq!(tool_calls[0].input, "{\"id\":\"123\"}");
    }

    #[test]
    fn configured_model_id_uses_env_when_primary_missing() {
        let model = configured_model_id_with(None, |name| match name {
            "LLM_MODEL" => Some("claude-sonnet-4".to_string()),
            _ => None,
        });

        assert_eq!(model.as_deref(), Some("claude-sonnet-4"));
    }

    #[test]
    fn configured_context_token_limit_uses_last_valid_context_limit() {
        let limits = vec![
            crate::manifest::LimitSpec {
                scope: "ITERATIONS".to_string(),
                value: "10".to_string(),
                qualifiers: Vec::new(),
            },
            crate::manifest::LimitSpec {
                scope: "CONTEXT_TOKENS".to_string(),
                value: "16000".to_string(),
                qualifiers: Vec::new(),
            },
            crate::manifest::LimitSpec {
                scope: "CONTEXT_TOKENS".to_string(),
                value: "32000".to_string(),
                qualifiers: Vec::new(),
            },
        ];

        assert_eq!(configured_context_token_limit(&limits), Some(32000));
    }

    #[test]
    fn configured_llm_timeout_ms_uses_last_matching_timeout() {
        let timeouts = vec![
            crate::manifest::TimeoutSpec {
                scope: "LLM".to_string(),
                duration: "15s".to_string(),
                qualifiers: Vec::new(),
            },
            crate::manifest::TimeoutSpec {
                scope: "TOOL".to_string(),
                duration: "50ms".to_string(),
                qualifiers: Vec::new(),
            },
            crate::manifest::TimeoutSpec {
                scope: "LLM".to_string(),
                duration: "1200ms".to_string(),
                qualifiers: Vec::new(),
            },
        ];

        assert_eq!(configured_llm_timeout_ms(&timeouts).unwrap(), Some(1200));
    }

    #[test]
    fn configured_tool_limits_use_last_valid_values() {
        let limits = vec![
            crate::manifest::LimitSpec {
                scope: "TOOL_CALLS".to_string(),
                value: "2".to_string(),
                qualifiers: Vec::new(),
            },
            crate::manifest::LimitSpec {
                scope: "TOOL_OUTPUT".to_string(),
                value: "0".to_string(),
                qualifiers: Vec::new(),
            },
            crate::manifest::LimitSpec {
                scope: "TOOL_CALLS".to_string(),
                value: "5".to_string(),
                qualifiers: Vec::new(),
            },
            crate::manifest::LimitSpec {
                scope: "TOOL_OUTPUT".to_string(),
                value: "1024".to_string(),
                qualifiers: Vec::new(),
            },
        ];

        assert_eq!(configured_tool_call_limit(&limits), Some(5));
        assert_eq!(configured_tool_output_limit(&limits), Some(1024));
    }

    #[test]
    fn truncate_tool_output_preserves_utf8_boundaries() {
        let output = "hello π world and a much longer tool output payload".to_string();
        let truncated = truncate_tool_output(output, Some(40));
        assert!(truncated.is_char_boundary(truncated.len()));
        assert!(truncated.contains("[dispatch truncated tool output]"));
    }

    #[test]
    fn courier_error_retryability_is_classified() {
        assert!(CourierError::ModelBackendRequest("network".to_string()).is_retryable());
        assert!(
            !CourierError::ToolCallLimitExceeded {
                limit: 2,
                attempted: 3
            }
            .is_retryable()
        );
        assert!(
            !CourierError::ToolTimedOut {
                tool: "slow".to_string(),
                timeout: "TOOL".to_string()
            }
            .is_retryable()
        );
        assert!(
            !CourierError::RunTimedOut {
                session_id: "session-1".to_string(),
                timeout: "RUN".to_string()
            }
            .is_retryable()
        );
        assert!(
            !CourierError::MissingSecret {
                name: "OPENAI_API_KEY".to_string()
            }
            .is_retryable()
        );
    }

    #[test]
    fn anthropic_max_tokens_uses_context_token_limit_when_present() {
        let request = ModelRequest {
            model: "claude-sonnet-4".to_string(),
            provider: Some("anthropic".to_string()),
            llm_timeout_ms: None,
            context_token_limit: Some(16000),
            tool_call_limit: None,
            tool_output_limit: None,
            instructions: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            pending_tool_calls: Vec::new(),
            tool_outputs: Vec::new(),
            previous_response_id: None,
        };

        assert_eq!(anthropic_max_tokens(&request), 16000);
    }

    #[test]
    fn normalize_local_tool_input_extracts_function_style_text_payload() {
        let tool = LocalToolSpec {
            alias: "demo".to_string(),
            description: None,
            input_schema_packaged_path: None,
            input_schema_sha256: None,
            target: LocalToolTarget::Local {
                packaged_path: "tools/demo.sh".to_string(),
                command: "bash".to_string(),
                args: Vec::new(),
            },
        };

        let normalized = normalize_local_tool_input(&tool, "{\"input\":\"echo hi\"}").unwrap();
        assert_eq!(normalized.as_ref(), "echo hi");
    }

    #[test]
    fn openai_chat_completions_messages_include_structured_tool_followup() {
        let request = ModelRequest {
            model: "gpt-5-mini".to_string(),
            provider: Some("openai_compatible".to_string()),
            llm_timeout_ms: None,
            context_token_limit: None,
            tool_call_limit: None,
            tool_output_limit: None,
            instructions: "Be helpful.".to_string(),
            messages: vec![ConversationMessage {
                role: "user".to_string(),
                content: "hello".to_string(),
            }],
            tools: Vec::new(),
            pending_tool_calls: vec![ModelToolCall {
                call_id: "call_1".to_string(),
                name: "lookup".to_string(),
                input: "{\"id\":\"123\"}".to_string(),
                kind: ModelToolKind::Function,
            }],
            tool_outputs: vec![ModelToolOutput {
                call_id: "call_1".to_string(),
                name: "lookup".to_string(),
                output: "found".to_string(),
                kind: ModelToolKind::Function,
            }],
            previous_response_id: None,
        };

        let messages = openai_chat_completions_messages(&request);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["tool_calls"][0]["function"]["name"], "lookup");
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call_1");
        assert_eq!(messages[3]["content"], "found");
    }

    #[test]
    fn anthropic_messages_include_tool_use_and_tool_result_blocks() {
        let request = ModelRequest {
            model: "claude-sonnet-4".to_string(),
            provider: Some("anthropic".to_string()),
            llm_timeout_ms: None,
            context_token_limit: None,
            tool_call_limit: None,
            tool_output_limit: None,
            instructions: String::new(),
            messages: vec![ConversationMessage {
                role: "user".to_string(),
                content: "hello".to_string(),
            }],
            tools: Vec::new(),
            pending_tool_calls: vec![ModelToolCall {
                call_id: "toolu_1".to_string(),
                name: "lookup".to_string(),
                input: "{\"id\":\"123\"}".to_string(),
                kind: ModelToolKind::Function,
            }],
            tool_outputs: vec![ModelToolOutput {
                call_id: "toolu_1".to_string(),
                name: "lookup".to_string(),
                output: "found".to_string(),
                kind: ModelToolKind::Function,
            }],
            previous_response_id: None,
        };

        let messages = anthropic_messages(&request);
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[1]["content"][0]["name"], "lookup");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_1");
    }

    #[test]
    fn gemini_messages_include_function_call_and_response_parts() {
        let request = ModelRequest {
            model: "gemini-2.5-pro".to_string(),
            provider: Some("gemini".to_string()),
            llm_timeout_ms: None,
            context_token_limit: None,
            tool_call_limit: None,
            tool_output_limit: None,
            instructions: String::new(),
            messages: vec![ConversationMessage {
                role: "user".to_string(),
                content: "hello".to_string(),
            }],
            tools: Vec::new(),
            pending_tool_calls: vec![ModelToolCall {
                call_id: "call_1".to_string(),
                name: "lookup".to_string(),
                input: "{\"id\":\"123\"}".to_string(),
                kind: ModelToolKind::Function,
            }],
            tool_outputs: vec![ModelToolOutput {
                call_id: "call_1".to_string(),
                name: "lookup".to_string(),
                output: "found".to_string(),
                kind: ModelToolKind::Function,
            }],
            previous_response_id: None,
        };

        let messages = gemini_messages(&request);
        assert_eq!(messages[1]["role"], "model");
        assert_eq!(messages[1]["parts"][0]["functionCall"]["name"], "lookup");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(
            messages[2]["parts"][0]["functionResponse"]["name"],
            "lookup"
        );
    }

    #[test]
    fn native_courier_chat_uses_backend_when_model_is_declared() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
MODEL gpt-5-mini
ENTRYPOINT chat
",
            &[],
        );
        let backend = Arc::new(FakeChatBackend::with_reply("backend reply"));
        let courier = NativeCourier::with_chat_backend(backend.clone());
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "hello backend".to_string(),
                },
            },
        ))
        .unwrap();

        assert!(matches!(
            response.events.first(),
            Some(CourierEvent::Message { content, .. }) if content == "backend reply"
        ));

        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].model, "gpt-5-mini");
        assert_eq!(calls[0].messages[0].content, "hello backend");
    }

    #[test]
    fn native_courier_caps_llm_timeout_by_remaining_run_budget() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TIMEOUT RUN 100ms
TIMEOUT LLM 5s
ENTRYPOINT chat
",
            &[],
        );
        let backend = Arc::new(FakeChatBackend::with_reply("backend reply"));
        let courier = NativeCourier::with_chat_backend(backend.clone());
        let mut session =
            futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
        session.elapsed_ms = 60;

        let _response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "hello backend".to_string(),
                },
            },
        ))
        .unwrap();

        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let timeout_ms = calls[0].llm_timeout_ms.expect("expected llm timeout");
        assert!((1..=40).contains(&timeout_ms));
    }

    #[test]
    fn native_courier_chat_streams_text_delta_without_duplicate_message() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
MODEL gpt-5-mini
ENTRYPOINT chat
",
            &[],
        );
        let backend = Arc::new(FakeChatBackend::with_streaming_reply(
            "streamed reply",
            vec!["streamed ", "reply"],
        ));
        let courier = NativeCourier::with_chat_backend(backend);
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "stream please".to_string(),
                },
            },
        ))
        .unwrap();

        assert_eq!(
            response
                .events
                .iter()
                .filter_map(|event| match event {
                    CourierEvent::TextDelta { content } => Some(content.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["streamed ", "reply"]
        );
        assert!(matches!(response.events.last(), Some(CourierEvent::Done)));
        assert!(!response.events.iter().any(|event| matches!(
            event,
            CourierEvent::Message { role, content }
                if role == "assistant" && content == "streamed reply"
        )));
        assert_eq!(
            response.session.history.last(),
            Some(&ConversationMessage {
                role: "assistant".to_string(),
                content: "streamed reply".to_string(),
            })
        );
    }

    #[test]
    fn native_courier_chat_executes_tool_calls_then_continues_model_turn() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT chat
",
            &[("tools/demo.sh", "printf 'tool-output'")],
        );
        let backend = Arc::new(FakeChatBackend::with_replies(vec![
            Some(ModelReply {
                text: None,
                backend: "fake".to_string(),
                response_id: Some("resp_1".to_string()),
                tool_calls: vec![ModelToolCall {
                    call_id: "call_1".to_string(),
                    name: "demo".to_string(),
                    input: "{\"query\":\"ping\"}".to_string(),
                    kind: ModelToolKind::Custom,
                }],
            }),
            Some(ModelReply {
                text: Some("final answer".to_string()),
                backend: "fake".to_string(),
                response_id: Some("resp_2".to_string()),
                tool_calls: Vec::new(),
            }),
        ]));
        let courier = NativeCourier::with_chat_backend(backend.clone());
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "use the tool".to_string(),
                },
            },
        ))
        .unwrap();

        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].tools.len(), 1);
        assert_eq!(calls[0].tools[0].name, "demo");
        assert_eq!(calls[1].previous_response_id.as_deref(), Some("resp_1"));
        assert_eq!(calls[1].messages.len(), 0);
        assert_eq!(calls[1].tool_outputs.len(), 1);
        assert_eq!(calls[1].tool_outputs[0].call_id, "call_1");
        assert_eq!(calls[1].tool_outputs[0].kind, ModelToolKind::Custom);
        assert!(calls[1].tool_outputs[0].output.contains("tool-output"));
        assert!(matches!(
            response.events.first(),
            Some(CourierEvent::ToolCallStarted { invocation, .. })
                if invocation.name == "demo"
                    && invocation.input.as_deref() == Some("{\"query\":\"ping\"}")
        ));
        assert!(matches!(
            response.events.get(1),
            Some(CourierEvent::ToolCallFinished { result })
                if result.tool == "demo" && result.stdout.contains("tool-output")
        ));
        assert!(matches!(
            response.events.get(2),
            Some(CourierEvent::Message { content, .. }) if content == "final answer"
        ));
        assert!(matches!(response.events.last(), Some(CourierEvent::Done)));
    }

    #[test]
    fn native_courier_chat_reconstructs_followup_without_response_threading() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT chat
",
            &[("tools/demo.sh", "printf 'tool-output'")],
        );
        let backend = Arc::new(FakeChatBackend::with_replies_without_previous_response_id(
            vec![
                Some(ModelReply {
                    text: None,
                    backend: "fake".to_string(),
                    response_id: Some("resp_1".to_string()),
                    tool_calls: vec![ModelToolCall {
                        call_id: "call_1".to_string(),
                        name: "demo".to_string(),
                        input: "{\"query\":\"ping\"}".to_string(),
                        kind: ModelToolKind::Custom,
                    }],
                }),
                Some(ModelReply {
                    text: Some("final answer".to_string()),
                    backend: "fake".to_string(),
                    response_id: Some("resp_2".to_string()),
                    tool_calls: Vec::new(),
                }),
            ],
        ));
        let courier = NativeCourier::with_chat_backend(backend.clone());
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "use the tool".to_string(),
                },
            },
        ))
        .unwrap();

        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[1].previous_response_id.is_none());
        assert_eq!(calls[1].tool_outputs.len(), 1);
        assert_eq!(calls[1].pending_tool_calls.len(), 1);
        assert_eq!(calls[1].messages.len(), 1);
        assert_eq!(calls[1].messages[0].role, "user");
        assert_eq!(calls[1].messages[0].content, "use the tool");
        assert_eq!(calls[1].pending_tool_calls[0].call_id, "call_1");
        assert_eq!(calls[1].pending_tool_calls[0].name, "demo");
        assert!(calls[1].tool_outputs[0].output.contains("tool-output"));
        drop(calls);

        assert!(matches!(
            response.events.get(2),
            Some(CourierEvent::Message { content, .. }) if content == "final answer"
        ));
        assert!(matches!(response.events.last(), Some(CourierEvent::Done)));
    }

    #[test]
    fn native_courier_chat_falls_back_when_backend_is_unavailable() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
MODEL gpt-5-mini
ENTRYPOINT chat
",
            &[],
        );
        let backend = Arc::new(FakeChatBackend::default());
        let courier = NativeCourier::with_chat_backend(backend.clone());
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "fallback please".to_string(),
                },
            },
        ))
        .unwrap();

        assert!(matches!(
            response.events.first(),
            Some(CourierEvent::BackendFallback { backend, error })
                if backend == "fake" && error.contains("not configured")
        ));
        assert!(matches!(
            response.events.get(1),
            Some(CourierEvent::Message { content, .. })
                if content.contains("Native chat reference reply")
        ));
        assert_eq!(backend.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn native_courier_chat_emits_backend_fallback_event_on_backend_error() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
MODEL gpt-5-mini
ENTRYPOINT chat
",
            &[],
        );
        let backend = Arc::new(FakeChatBackend::with_error("http status: 401"));
        let courier = NativeCourier::with_chat_backend(backend.clone());
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "fallback on error".to_string(),
                },
            },
        ))
        .unwrap();

        assert!(matches!(
            response.events.first(),
            Some(CourierEvent::BackendFallback { backend, error })
                if backend == "fake" && error.contains("401")
        ));
        assert!(matches!(
            response.events.get(1),
            Some(CourierEvent::Message { content, .. })
                if content.contains("Native chat reference reply")
        ));
        assert_eq!(backend.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn native_courier_chat_uses_fallback_model_after_primary_backend_error() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
MODEL primary-model
FALLBACK fallback-model
ENTRYPOINT chat
",
            &[],
        );
        let backend = Arc::new(FakeChatBackend {
            replies: Mutex::new(vec![
                Err("temporary backend failure".to_string()),
                Ok(ModelGeneration::Reply(ModelReply {
                    text: Some("fallback answer".to_string()),
                    backend: "fake".to_string(),
                    response_id: None,
                    tool_calls: Vec::new(),
                })),
            ]),
            streams: Mutex::new(vec![Vec::new(), Vec::new()]),
            calls: Mutex::new(Vec::new()),
            supports_previous_response_id: false,
        });
        let courier = NativeCourier::with_chat_backend(backend.clone());
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "fallback please".to_string(),
                },
            },
        ))
        .unwrap();

        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].model, "primary-model");
        assert_eq!(calls[1].model, "fallback-model");
        drop(calls);
        assert!(matches!(
            response.events.first(),
            Some(CourierEvent::BackendFallback { backend, error })
                if backend == "fake"
                    && error.contains("temporary backend failure")
                    && error.contains("fallback model `fallback-model`")
        ));
        assert!(matches!(
            response.events.get(1),
            Some(CourierEvent::Message { content, .. }) if content == "fallback answer"
        ));
    }

    #[test]
    fn native_courier_chat_emits_backend_fallback_when_tool_loop_is_exhausted() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT chat
",
            &[("tools/demo.sh", "printf 'tool-output'")],
        );
        let backend = Arc::new(FakeChatBackend::with_replies(
            (0..8)
                .map(|index| {
                    Some(ModelReply {
                        text: None,
                        backend: "fake".to_string(),
                        response_id: Some(format!("resp_{index}")),
                        tool_calls: vec![ModelToolCall {
                            call_id: format!("call_{index}"),
                            name: "demo".to_string(),
                            input: "{\"query\":\"ping\"}".to_string(),
                            kind: ModelToolKind::Custom,
                        }],
                    })
                })
                .collect(),
        ));
        let courier = NativeCourier::with_chat_backend(backend.clone());
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "loop forever".to_string(),
                },
            },
        ))
        .unwrap();

        assert!(response.events.iter().any(|event| {
            matches!(
                event,
                CourierEvent::BackendFallback { backend, error }
                    if backend == "fake" && error.contains("tool call loop reached 8 rounds")
            )
        }));
        assert!(matches!(
            response.events.iter().rev().nth(1),
            Some(CourierEvent::Message { content, .. })
                if content.contains("Native chat reference reply")
        ));
        assert_eq!(backend.calls.lock().unwrap().len(), 8);
    }

    #[test]
    fn native_courier_chat_executes_schema_tool_calls_as_function_outputs() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL LOCAL tools/demo.sh AS demo SCHEMA schemas/demo.json
ENTRYPOINT chat
",
            &[
                ("tools/demo.sh", "printf 'tool-output'"),
                (
                    "schemas/demo.json",
                    "{\n  \"type\": \"object\",\n  \"properties\": {\n    \"query\": { \"type\": \"string\" }\n  },\n  \"required\": [\"query\"]\n}",
                ),
            ],
        );
        let backend = Arc::new(FakeChatBackend::with_replies(vec![
            Some(ModelReply {
                text: None,
                backend: "fake".to_string(),
                response_id: Some("resp_1".to_string()),
                tool_calls: vec![ModelToolCall {
                    call_id: "call_1".to_string(),
                    name: "demo".to_string(),
                    input: "{\"query\":\"ping\"}".to_string(),
                    kind: ModelToolKind::Function,
                }],
            }),
            Some(ModelReply {
                text: Some("final answer".to_string()),
                backend: "fake".to_string(),
                response_id: Some("resp_2".to_string()),
                tool_calls: Vec::new(),
            }),
        ]));
        let courier = NativeCourier::with_chat_backend(backend.clone());
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "use the function tool".to_string(),
                },
            },
        ))
        .unwrap();

        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(matches!(
            calls[0].tools[0].format,
            ModelToolFormat::JsonSchema { .. }
        ));
        assert_eq!(calls[1].tool_outputs.len(), 1);
        assert_eq!(calls[1].tool_outputs[0].kind, ModelToolKind::Function);
        assert!(calls[1].tool_outputs[0].output.contains("tool-output"));
        assert!(matches!(response.events.last(), Some(CourierEvent::Done)));
    }

    #[test]
    fn native_courier_chat_executes_builtin_memory_tools() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL BUILTIN memory_put
TOOL BUILTIN memory_get
MOUNT MEMORY sqlite
ENTRYPOINT chat
",
            &[],
        );
        let backend = Arc::new(FakeChatBackend::with_replies(vec![
            Some(ModelReply {
                text: None,
                backend: "fake".to_string(),
                response_id: Some("resp_1".to_string()),
                tool_calls: vec![ModelToolCall {
                    call_id: "call_1".to_string(),
                    name: "memory_put".to_string(),
                    input: "{\"namespace\":\"profile\",\"key\":\"name\",\"value\":\"Christian\"}"
                        .to_string(),
                    kind: ModelToolKind::Function,
                }],
            }),
            Some(ModelReply {
                text: None,
                backend: "fake".to_string(),
                response_id: Some("resp_2".to_string()),
                tool_calls: vec![ModelToolCall {
                    call_id: "call_2".to_string(),
                    name: "memory_get".to_string(),
                    input: "{\"namespace\":\"profile\",\"key\":\"name\"}".to_string(),
                    kind: ModelToolKind::Function,
                }],
            }),
            Some(ModelReply {
                text: Some("memory complete".to_string()),
                backend: "fake".to_string(),
                response_id: Some("resp_3".to_string()),
                tool_calls: Vec::new(),
            }),
        ]));
        let courier = NativeCourier::with_chat_backend(backend.clone());
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "remember my name".to_string(),
                },
            },
        ))
        .unwrap();

        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].tools.len(), 2);
        assert_eq!(calls[0].tools[0].name, "memory_put");
        assert_eq!(calls[0].tools[1].name, "memory_get");
        assert!(matches!(
            calls[0].tools[0].format,
            ModelToolFormat::JsonSchema { .. }
        ));
        assert_eq!(calls[1].previous_response_id.as_deref(), Some("resp_1"));
        assert!(calls[1].messages.is_empty());
        assert_eq!(calls[1].tool_outputs.len(), 1);
        assert_eq!(calls[1].tool_outputs[0].name, "memory_put");
        assert!(
            calls[1].tool_outputs[0]
                .output
                .contains("Stored memory profile:name")
        );
        assert_eq!(calls[2].previous_response_id.as_deref(), Some("resp_2"));
        assert_eq!(calls[2].tool_outputs.len(), 1);
        assert_eq!(calls[2].tool_outputs[0].name, "memory_get");
        assert!(
            calls[2].tool_outputs[0]
                .output
                .contains("profile:name = Christian")
        );
        drop(calls);

        assert!(matches!(
            response.events.first(),
            Some(CourierEvent::ToolCallStarted { invocation, command, args })
                if invocation.name == "memory_put"
                    && command == "dispatch-builtin"
                    && args == &vec!["memory_put".to_string()]
        ));
        assert!(matches!(
            response.events.get(1),
            Some(CourierEvent::ToolCallFinished { result })
                if result.tool == "memory_put"
                    && result.command == "dispatch-builtin"
                    && result.stdout.contains("Stored memory profile:name")
        ));
        assert!(matches!(
            response.events.get(2),
            Some(CourierEvent::ToolCallStarted { invocation, command, args })
                if invocation.name == "memory_get"
                    && command == "dispatch-builtin"
                    && args == &vec!["memory_get".to_string()]
        ));
        assert!(matches!(
            response.events.get(3),
            Some(CourierEvent::ToolCallFinished { result })
                if result.tool == "memory_get"
                    && result.stdout.contains("profile:name = Christian")
        ));
        assert!(matches!(
            response.events.get(4),
            Some(CourierEvent::Message { content, .. }) if content == "memory complete"
        ));
        assert!(matches!(response.events.last(), Some(CourierEvent::Done)));
    }

    #[test]
    fn native_courier_chat_preserves_history_across_turns() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
ENTRYPOINT chat
",
            &[],
        );
        let courier = NativeCourier::default();
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let first = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "first".to_string(),
                },
            },
        ))
        .unwrap();

        let second = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session: first.session,
                operation: CourierOperation::Chat {
                    input: "second".to_string(),
                },
            },
        ))
        .unwrap();

        assert_eq!(second.session.turn_count, 2);
        assert_eq!(second.session.history.len(), 4);
        assert_eq!(second.session.history[2].content, "second");
        assert!(
            second.session.history[3]
                .content
                .contains("Prior messages in session: 2")
        );
    }

    #[test]
    fn native_courier_chat_supports_prompt_command() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
SOUL SOUL.md
ENTRYPOINT chat
",
            &[("SOUL.md", "Soul body")],
        );
        let courier = NativeCourier::default();
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "/prompt".to_string(),
                },
            },
        ))
        .unwrap();

        assert!(matches!(
            response.events.first(),
            Some(CourierEvent::Message { content, .. }) if content.contains("# SOUL")
        ));
    }

    #[test]
    fn native_courier_job_emits_assistant_message_and_records_history() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
ENTRYPOINT job
",
            &[],
        );
        let courier = NativeCourier::default();
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Job {
                    payload: "{\"task\":\"summarize\"}".to_string(),
                },
            },
        ))
        .unwrap();

        assert_eq!(response.session.turn_count, 1);
        assert_eq!(response.session.history.len(), 2);
        assert_eq!(
            response.session.history[0].content,
            "Job payload:\n{\"task\":\"summarize\"}"
        );
        assert!(matches!(
            response.events.first(),
            Some(CourierEvent::Message { content, .. })
                if content.contains("Native job reference reply")
        ));
        assert!(matches!(response.events.last(), Some(CourierEvent::Done)));
    }

    #[test]
    fn native_courier_heartbeat_emits_assistant_message_and_records_history() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
ENTRYPOINT heartbeat
",
            &[],
        );
        let courier = NativeCourier::default();
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Heartbeat { payload: None },
            },
        ))
        .unwrap();

        assert_eq!(response.session.turn_count, 1);
        assert_eq!(response.session.history.len(), 2);
        assert_eq!(response.session.history[0].content, "Heartbeat tick");
        assert!(matches!(
            response.events.first(),
            Some(CourierEvent::Message { content, .. })
                if content.contains("Native heartbeat reference reply")
        ));
        assert!(matches!(response.events.last(), Some(CourierEvent::Done)));
    }

    #[test]
    fn native_courier_inspect_reports_mounts_secrets_and_local_tools() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
TOOL LOCAL tools/demo.sh AS demo
MOUNT SESSION sqlite
SECRET CAST_SAMPLE_SECRET
ENTRYPOINT job
",
            &[("tools/demo.sh", "printf ok")],
        );
        let courier = NativeCourier::default();

        let inspection = futures::executor::block_on(courier.inspect(&test_image.image)).unwrap();

        assert_eq!(inspection.entrypoint.as_deref(), Some("job"));
        assert_eq!(inspection.required_secrets, vec!["CAST_SAMPLE_SECRET"]);
        assert_eq!(inspection.mounts.len(), 1);
        assert_eq!(inspection.mounts[0].driver, "sqlite");
        assert_eq!(inspection.local_tools.len(), 1);
        assert_eq!(inspection.local_tools[0].alias, "demo");
    }

    #[test]
    fn load_parcel_rejects_manifests_that_fail_schema_validation() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
SOUL SOUL.md
ENTRYPOINT chat
",
            &[("SOUL.md", "You are schema-checked.")],
        );

        let manifest_path = test_image.image.parcel_dir.join("manifest.json");
        let mut manifest =
            serde_json::from_slice::<Value>(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["tools"] = Value::String("not-an-array".to_string());
        fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let error = load_parcel(&test_image.image.parcel_dir).unwrap_err();
        assert!(matches!(error, CourierError::InvalidParcelSchema { .. }));
    }

    #[test]
    fn native_courier_persists_session_sqlite_mounts() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
MOUNT SESSION sqlite
ENTRYPOINT chat
",
            &[],
        );
        let courier = NativeCourier::default();
        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        let sqlite_mount = session
            .resolved_mounts
            .iter()
            .find(|mount| mount.kind == MountKind::Session && mount.driver == "sqlite")
            .expect("expected sqlite session mount");
        let connection = Connection::open(&sqlite_mount.target_path).unwrap();
        let (turn_count, payload_json): (i64, String) = connection
            .query_row(
                "SELECT turn_count, payload_json FROM dispatch_sessions WHERE session_id = ?1",
                [&session.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(turn_count, 0);
        let persisted: CourierSession = serde_json::from_str(&payload_json).unwrap();
        assert_eq!(persisted.id, session.id);

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "hello".to_string(),
                },
            },
        ))
        .unwrap();
        let updated_turn_count: i64 = connection
            .query_row(
                "SELECT turn_count FROM dispatch_sessions WHERE session_id = ?1",
                [&response.session.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(updated_turn_count, 1);
    }

    #[test]
    fn open_session_sets_label_and_zero_elapsed_budget() {
        let test_image = build_test_image(
            "\
NAME demo
FROM dispatch/native:latest
ENTRYPOINT chat
",
            &[],
        );
        let courier = NativeCourier::default();

        let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

        assert_eq!(session.label.as_deref(), Some("demo"));
        assert_eq!(session.elapsed_ms, 0);
    }

    #[test]
    fn native_courier_rejects_runs_that_exceed_timeout_budget() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
TIMEOUT RUN 100ms
ENTRYPOINT chat
",
            &[],
        );
        let courier = NativeCourier::default();
        let mut session =
            futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
        session.elapsed_ms = 100;

        let error = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::Chat {
                    input: "hello".to_string(),
                },
            },
        ))
        .unwrap_err();

        assert!(matches!(
            error,
            CourierError::RunTimedOut { ref timeout, .. } if timeout == "100ms"
        ));
    }

    #[test]
    fn native_courier_inspection_helpers_do_not_consume_run_budget() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
TIMEOUT RUN 100ms
ENTRYPOINT chat
",
            &[],
        );
        let courier = NativeCourier::default();
        let mut session =
            futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
        session.elapsed_ms = 100;

        let response = futures::executor::block_on(courier.run(
            &test_image.image,
            CourierRequest {
                session,
                operation: CourierOperation::ListLocalTools,
            },
        ))
        .unwrap();

        assert_eq!(response.session.elapsed_ms, 100);
        assert!(matches!(
            response.events.first(),
            Some(CourierEvent::LocalToolsListed { .. })
        ));
    }

    #[test]
    fn run_local_tool_requires_declared_secrets() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
TOOL LOCAL tools/demo.sh AS demo
SECRET CAST_TEST_SECRET_DOES_NOT_EXIST
ENTRYPOINT job
",
            &[("tools/demo.sh", "printf ok")],
        );

        let error = run_local_tool(&test_image.image, "demo", None).unwrap_err();
        assert!(matches!(
            error,
            CourierError::MissingSecret { name } if name == "CAST_TEST_SECRET_DOES_NOT_EXIST"
        ));
    }

    #[test]
    fn open_session_prefers_secret_validation_to_late_tool_failure() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
TOOL LOCAL tools/demo.sh AS demo
SECRET CAST_TEST_SECRET_DOES_NOT_EXIST
ENTRYPOINT chat
",
            &[("tools/demo.sh", "printf ok")],
        );
        let courier = NativeCourier::default();

        let error =
            futures::executor::block_on(courier.open_session(&test_image.image)).unwrap_err();
        assert!(matches!(
            error,
            CourierError::MissingSecret { name } if name == "CAST_TEST_SECRET_DOES_NOT_EXIST"
        ));
    }

    #[test]
    fn run_local_tool_only_forwards_declared_environment() {
        let test_image = build_test_image(
            "\
FROM dispatch/native:latest
TOOL LOCAL tools/env.sh AS envcheck
ENV CAST_VISIBLE_ENV=visible
SECRET CAST_VISIBLE_SECRET
ENTRYPOINT job
",
            &[(
                "tools/env.sh",
                "printf '%s\\n' \"visible_env=${CAST_VISIBLE_ENV:-}\" \"visible_secret=${CAST_VISIBLE_SECRET:-}\" \"hidden_env=${CAST_HIDDEN_ENV:-}\"",
            )],
        );

        let host_env = BTreeMap::from([
            (
                "CAST_VISIBLE_SECRET".to_string(),
                "secret-value".to_string(),
            ),
            ("CAST_HIDDEN_ENV".to_string(), "hidden-value".to_string()),
        ]);

        let result = run_local_tool_with_env(&test_image.image, "envcheck", None, |name| {
            host_env.get(name).cloned()
        })
        .unwrap();

        assert!(result.stdout.contains("visible_env=visible"));
        assert!(result.stdout.contains("visible_secret=secret-value"));
        assert!(result.stdout.contains("hidden_env="));
        assert!(!result.stdout.contains("hidden_env=hidden-value"));
    }

    #[test]
    fn bounded_lru_cache_evicts_least_recently_used_entries() {
        let mut cache = BoundedLruCache::new(2);
        cache.insert("a".to_string(), "one".to_string());
        cache.insert("b".to_string(), "two".to_string());

        assert_eq!(cache.get("a").as_deref(), Some("one"));

        cache.insert("c".to_string(), "three".to_string());

        assert_eq!(cache.get("a").as_deref(), Some("one"));
        assert_eq!(cache.get("b"), None);
        assert_eq!(cache.get("c").as_deref(), Some("three"));
        assert_eq!(cache.keys(), vec!["a".to_string(), "c".to_string()]);
    }
}
