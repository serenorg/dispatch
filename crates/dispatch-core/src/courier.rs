use crate::{
    manifest::{InstructionKind, MountConfig, MountKind, ParcelManifest, ToolConfig},
    plugin_protocol::{
        COURIER_PLUGIN_PROTOCOL_VERSION, PluginRequest, PluginRequestEnvelope, PluginResponse,
    },
    plugins::CourierPluginManifest,
};
use dispatch_wasm_abi::ABI as DISPATCH_WASM_COMPONENT_ABI;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    future::Future,
    io::{BufRead as _, BufReader, Write as _},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex},
};
use thiserror::Error;
use wasmtime::{
    Config, Engine, Store,
    component::{Component, HasSelf, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

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
}

#[derive(Debug, Clone, Serialize)]
pub struct LoadedParcel {
    pub parcel_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub config: ParcelManifest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalToolSpec {
    pub alias: String,
    pub packaged_path: String,
    pub command: String,
    pub args: Vec<String>,
    pub description: Option<String>,
    pub input_schema_packaged_path: Option<String>,
    pub input_schema_sha256: Option<String>,
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

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MountRequest {
    pub parcel_digest: String,
    pub spec: MountConfig,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
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
    pub turn_count: u64,
    pub history: Vec<ConversationMessage>,
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
}

struct WasmHostState {
    host: WasmHost,
    wasi_ctx: WasiCtx,
    resource_table: ResourceTable,
}

struct WasmHost {
    parcel: LoadedParcel,
    chat_backend: Arc<dyn ChatModelBackend>,
}

#[derive(Debug, Clone, Copy)]
enum NativeTurnMode {
    Chat,
    Job,
    Heartbeat,
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
    fn generate(&self, request: &ModelRequest) -> Result<Option<ModelReply>, CourierError>;
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModelRequest {
    pub model: String,
    pub instructions: String,
    pub messages: Vec<ConversationMessage>,
    pub tools: Vec<ModelToolDefinition>,
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
    pub output: String,
    pub kind: ModelToolKind,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelToolKind {
    Custom,
    Function,
}

pub struct NativeCourier {
    chat_backend: Arc<dyn ChatModelBackend>,
}

#[derive(Debug, Clone)]
pub struct JsonlCourierPlugin {
    manifest: CourierPluginManifest,
}

#[derive(Debug, Clone)]
pub struct DockerCourier {
    docker_bin: PathBuf,
    helper_image: String,
}

#[derive(Clone)]
pub struct WasmCourier {
    engine: Engine,
    chat_backend: Arc<dyn ChatModelBackend>,
    component_cache: Arc<Mutex<BTreeMap<String, Component>>>,
}

#[derive(Debug, Clone)]
pub struct StubCourier {
    courier_id: &'static str,
    kind: CourierKind,
}

impl Default for NativeCourier {
    fn default() -> Self {
        Self {
            chat_backend: Arc::new(OpenAiResponsesBackend),
        }
    }
}

impl Default for DockerCourier {
    fn default() -> Self {
        Self {
            docker_bin: std::env::var_os("DISPATCH_DOCKER_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("docker")),
            helper_image: std::env::var("DISPATCH_DOCKER_IMAGE")
                .unwrap_or_else(|_| "python:3.13-alpine".to_string()),
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
            chat_backend: Arc::new(OpenAiResponsesBackend),
            component_cache: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }
}

impl JsonlCourierPlugin {
    pub fn new(manifest: CourierPluginManifest) -> Self {
        Self { manifest }
    }
}

impl wasm_bindings::dispatch::courier::host::Host for WasmHost {
    fn model_complete(
        &mut self,
        request: wasm_bindings::dispatch::courier::host::ModelRequest,
    ) -> Result<wasm_bindings::dispatch::courier::host::ModelResponse, String> {
        let model = request
            .model
            .or_else(|| {
                self.parcel
                    .config
                    .models
                    .primary
                    .as_ref()
                    .map(|m| m.id.clone())
            })
            .ok_or_else(|| "no model configured for wasm guest request".to_string())?;
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
        let reply = self
            .chat_backend
            .generate(&ModelRequest {
                model,
                instructions: request.instructions,
                messages,
                tools,
                tool_outputs,
                previous_response_id: request.previous_response_id,
            })
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "model backend returned no reply".to_string())?;
        Ok(wasm_bindings::dispatch::courier::host::ModelResponse {
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
        })
    }

    fn invoke_tool(
        &mut self,
        invocation: wasm_bindings::dispatch::courier::host::ToolInvocation,
    ) -> Result<wasm_bindings::dispatch::courier::host::ToolResult, String> {
        let result = run_local_tool(&self.parcel, &invocation.name, invocation.input.as_deref())
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
        Self { chat_backend }
    }
}

impl WasmCourier {
    pub fn with_chat_backend(chat_backend: Arc<dyn ChatModelBackend>) -> Self {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).expect("failed to initialize wasmtime engine");
        Self {
            engine,
            chat_backend,
            component_cache: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }
}

impl DockerCourier {
    pub fn new(docker_bin: impl Into<PathBuf>, helper_image: impl Into<String>) -> Self {
        Self {
            docker_bin: docker_bin.into(),
            helper_image: helper_image.into(),
        }
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

struct OpenAiResponsesBackend;

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
    let config = serde_json::from_str::<ParcelManifest>(&source).map_err(|source| {
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
                packaged_path: local.packaged_path.clone(),
                command: local.runner.command.clone(),
                args: local.runner.args.clone(),
                description: local.description.clone(),
                input_schema_packaged_path: local
                    .input_schema
                    .as_ref()
                    .map(|schema| schema.packaged_path.clone()),
                input_schema_sha256: local
                    .input_schema
                    .as_ref()
                    .map(|schema| schema.sha256.clone()),
            }),
            _ => None,
        })
        .collect()
}

pub fn run_local_tool(
    parcel: &LoadedParcel,
    tool_name: &str,
    input: Option<&str>,
) -> Result<ToolRunResult, CourierError> {
    let tool = resolve_local_tool(parcel, tool_name)?;

    execute_local_tool(parcel, &tool, input)
}

fn resolve_local_tool(
    parcel: &LoadedParcel,
    tool_name: &str,
) -> Result<LocalToolSpec, CourierError> {
    validate_required_secrets(parcel)?;
    list_local_tools(parcel)
        .into_iter()
        .find(|tool| tool.alias == tool_name || tool.packaged_path == tool_name)
        .ok_or_else(|| CourierError::UnknownLocalTool {
            tool: tool_name.to_string(),
        })
}

fn validate_required_secrets(parcel: &LoadedParcel) -> Result<(), CourierError> {
    for secret in &parcel.config.secrets {
        if secret.required && std::env::var(&secret.name).is_err() {
            return Err(CourierError::MissingSecret {
                name: secret.name.clone(),
            });
        }
    }

    Ok(())
}

fn forwarded_tool_env(parcel: &LoadedParcel, input: Option<&str>) -> Vec<(String, String)> {
    let mut env = Vec::new();
    for var in ["PATH", "HOME", "TMPDIR", "TEMP", "TMP"] {
        if let Ok(value) = std::env::var(var) {
            env.push((var.to_string(), value));
        }
    }
    for entry in &parcel.config.env {
        env.push((entry.name.clone(), entry.value.clone()));
    }
    for secret in &parcel.config.secrets {
        if let Ok(value) = std::env::var(&secret.name) {
            env.push((secret.name.clone(), value));
        }
    }
    if let Some(input) = input {
        env.push(("TOOL_INPUT".to_string(), input.to_string()));
    }
    env
}

// Execute a tool whose spec has already been resolved. Callers are responsible
// for validating required secrets before calling this function.
fn execute_local_tool(
    parcel: &LoadedParcel,
    tool: &LocalToolSpec,
    input: Option<&str>,
) -> Result<ToolRunResult, CourierError> {
    let tool_path = parcel.parcel_dir.join("context").join(&tool.packaged_path);
    if !tool_path.exists() {
        return Err(CourierError::MissingToolFile {
            tool: tool.alias.clone(),
            path: tool_path.display().to_string(),
        });
    }

    let mut command = Command::new(&tool.command);
    command.args(&tool.args);
    if tool.command == tool.packaged_path {
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
    for (name, value) in forwarded_tool_env(parcel, input) {
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

    let output = child
        .wait_with_output()
        .map_err(|source| CourierError::WaitTool {
            tool: tool.alias.clone(),
            source,
        })?;

    Ok(ToolRunResult {
        tool: tool.alias.clone(),
        command: tool.command.clone(),
        args: tool.args.clone(),
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
) -> Result<ToolRunResult, CourierError> {
    let tool_path = parcel.parcel_dir.join("context").join(&tool.packaged_path);
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
    command.arg(&tool.command);
    command.args(&tool.args);
    if tool.command != tool.packaged_path {
        command.arg(&tool.packaged_path);
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

    let output = child
        .wait_with_output()
        .map_err(|source| CourierError::WaitTool {
            tool: tool.alias.clone(),
            source,
        })?;

    Ok(ToolRunResult {
        tool: tool.alias.clone(),
        command: tool.command.clone(),
        args: tool.args.clone(),
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
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
            Ok(CourierSession {
                id: format!("native-{parcel_digest}-{sequence}"),
                parcel_digest,
                entrypoint,
                turn_count: 0,
                history: Vec::new(),
                backend_state: None,
            })
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

        async move {
            validate_native_parcel(image)?;
            ensure_session_matches_parcel(image, &session)?;
            ensure_operation_matches_entrypoint(&session, &operation)?;
            session.turn_count += 1;

            match operation {
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
                CourierOperation::InvokeTool { invocation } => {
                    let result =
                        run_local_tool(image, &invocation.name, invocation.input.as_deref())?;

                    Ok(CourierResponse {
                        courier_id,
                        session,
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
                    let mut chat_turn = execute_native_turn(
                        image,
                        &session,
                        &input,
                        self.chat_backend.as_ref(),
                        NativeTurnMode::Chat,
                    )?;
                    session.history.push(ConversationMessage {
                        role: "assistant".to_string(),
                        content: chat_turn.reply.clone(),
                    });
                    chat_turn.events.push(CourierEvent::Message {
                        role: "assistant".to_string(),
                        content: chat_turn.reply,
                    });
                    chat_turn.events.push(CourierEvent::Done);

                    Ok(CourierResponse {
                        courier_id,
                        session,
                        events: chat_turn.events,
                    })
                }
                CourierOperation::Job { payload } => run_native_task_operation(
                    image,
                    session,
                    courier_id,
                    self.chat_backend.as_ref(),
                    NativeTurnMode::Job,
                    format_job_payload(&payload),
                ),
                CourierOperation::Heartbeat { payload } => run_native_task_operation(
                    image,
                    session,
                    courier_id,
                    self.chat_backend.as_ref(),
                    NativeTurnMode::Heartbeat,
                    format_heartbeat_payload(payload.as_deref()),
                ),
            }
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
            supports_chat: false,
            supports_job: false,
            supports_heartbeat: false,
            supports_local_tools: true,
            supports_mounts: Vec::new(),
        })
    }

    fn validate_parcel(
        &self,
        image: &LoadedParcel,
    ) -> impl Future<Output = Result<(), CourierError>> + Send {
        let reference = image.config.courier.reference.clone();
        async move { validate_courier_reference("docker", CourierKind::Docker, &reference) }
    }

    fn inspect(
        &self,
        image: &LoadedParcel,
    ) -> impl Future<Output = Result<CourierInspection, CourierError>> + Send {
        let reference = image.config.courier.reference.clone();
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
        let reference = image.config.courier.reference.clone();
        let parcel_digest = image.config.digest.clone();
        let entrypoint = image.config.entrypoint.clone();
        async move {
            validate_courier_reference("docker", CourierKind::Docker, &reference)?;
            validate_required_secrets(image)?;
            let sequence = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
            Ok(CourierSession {
                id: format!("docker-{parcel_digest}-{sequence}"),
                parcel_digest,
                entrypoint,
                turn_count: 0,
                history: Vec::new(),
                backend_state: None,
            })
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

        async move {
            validate_courier_reference(
                "docker",
                CourierKind::Docker,
                &image.config.courier.reference,
            )?;
            ensure_session_matches_parcel(image, &session)?;
            session.turn_count += 1;

            match operation {
                CourierOperation::ResolvePrompt => Ok(CourierResponse {
                    courier_id: "docker".to_string(),
                    session,
                    events: vec![
                        CourierEvent::PromptResolved {
                            text: resolve_prompt_text(image)?,
                        },
                        CourierEvent::Done,
                    ],
                }),
                CourierOperation::ListLocalTools => Ok(CourierResponse {
                    courier_id: "docker".to_string(),
                    session,
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
                    )?;

                    Ok(CourierResponse {
                        courier_id: "docker".to_string(),
                        session,
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
                CourierOperation::Chat { .. } => Err(CourierError::UnsupportedOperation {
                    courier: "docker".to_string(),
                    operation: "chat".to_string(),
                }),
                CourierOperation::Job { .. } => Err(CourierError::UnsupportedOperation {
                    courier: "docker".to_string(),
                    operation: "job".to_string(),
                }),
                CourierOperation::Heartbeat { .. } => Err(CourierError::UnsupportedOperation {
                    courier: "docker".to_string(),
                    operation: "heartbeat".to_string(),
                }),
            }
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
                Ok(PluginResponse::Result {
                    capabilities: Some(capabilities),
                    ..
                }) => Ok(capabilities),
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
                PluginResponse::Result { .. } => Ok(()),
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
                PluginResponse::Result {
                    inspection: Some(inspection),
                    ..
                } => Ok(inspection),
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
            let parcel_dir = parcel_dir?;
            match courier.plugin_request(PluginRequest::OpenSession { parcel_dir })? {
                PluginResponse::Result {
                    session: Some(session),
                    ..
                } => Ok(session),
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
        async move {
            ensure_session_matches_parcel(image, &request.session)?;
            let parcel_dir = parcel_dir?;
            let mut child = courier.spawn_plugin()?;
            write_plugin_request(
                &mut child,
                &courier.manifest.name,
                PluginRequest::Run {
                    parcel_dir,
                    session: request.session,
                    operation: request.operation,
                },
            )?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| CourierError::PluginProtocol {
                    courier: courier.manifest.name.clone(),
                    message: "plugin stdout was not captured".to_string(),
                })?;
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let mut events = Vec::new();
            let final_session = loop {
                line.clear();
                let bytes = reader.read_line(&mut line).map_err(|source| {
                    CourierError::ReadPluginResponse {
                        courier: courier.manifest.name.clone(),
                        source,
                    }
                })?;
                if bytes == 0 {
                    return Err(CourierError::PluginProtocol {
                        courier: courier.manifest.name.clone(),
                        message: "plugin closed stdout before emitting `done`".to_string(),
                    });
                }
                let response: PluginResponse =
                    serde_json::from_str(line.trim_end()).map_err(|source| {
                        CourierError::PluginProtocol {
                            courier: courier.manifest.name.clone(),
                            message: format!("invalid plugin JSON: {source}"),
                        }
                    })?;
                match response {
                    PluginResponse::Event { event } => events.push(event),
                    PluginResponse::Done { session } => break session,
                    PluginResponse::Error { error } => {
                        return Err(CourierError::PluginProtocol {
                            courier: courier.manifest.name.clone(),
                            message: format!("{}: {}", error.code, error.message),
                        });
                    }
                    other => {
                        return Err(courier
                            .unexpected_plugin_response("run", describe_plugin_response(&other)));
                    }
                }
            };
            wait_for_plugin_exit(child, &courier.manifest.name)?;

            Ok(CourierResponse {
                courier_id: courier.manifest.name.clone(),
                session: final_session,
                events,
            })
        }
    }
}

impl JsonlCourierPlugin {
    fn plugin_request(&self, request: PluginRequest) -> Result<PluginResponse, CourierError> {
        let mut child = self.spawn_plugin()?;
        write_plugin_request(&mut child, &self.manifest.name, request)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CourierError::PluginProtocol {
                courier: self.manifest.name.clone(),
                message: "plugin stdout was not captured".to_string(),
            })?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let bytes =
            reader
                .read_line(&mut line)
                .map_err(|source| CourierError::ReadPluginResponse {
                    courier: self.manifest.name.clone(),
                    source,
                })?;
        if bytes == 0 {
            return Err(CourierError::PluginProtocol {
                courier: self.manifest.name.clone(),
                message: "plugin produced no response".to_string(),
            });
        }
        let response: PluginResponse = serde_json::from_str(line.trim_end()).map_err(|source| {
            CourierError::PluginProtocol {
                courier: self.manifest.name.clone(),
                message: format!("invalid plugin JSON: {source}"),
            }
        })?;
        wait_for_plugin_exit(child, &self.manifest.name)?;
        Ok(response)
    }

    fn spawn_plugin(&self) -> Result<Child, CourierError> {
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
    request: PluginRequest,
) -> Result<(), CourierError> {
    let stdin = child
        .stdin
        .as_mut()
        .ok_or_else(|| CourierError::PluginProtocol {
            courier: courier_name.to_string(),
            message: "plugin stdin was not captured".to_string(),
        })?;
    serde_json::to_writer(
        &mut *stdin,
        &PluginRequestEnvelope {
            protocol_version: COURIER_PLUGIN_PROTOCOL_VERSION,
            request,
        },
    )
    .map_err(|source| CourierError::PluginProtocol {
        courier: courier_name.to_string(),
        message: format!("failed to serialize plugin request: {source}"),
    })?;
    stdin
        .write_all(b"\n")
        .map_err(|source| CourierError::WritePluginRequest {
            courier: courier_name.to_string(),
            source,
        })?;
    stdin
        .flush()
        .map_err(|source| CourierError::WritePluginRequest {
            courier: courier_name.to_string(),
            source,
        })?;
    let _ = child.stdin.take();
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
        PluginResponse::Result { .. } => "result",
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
            supports_mounts: Vec::new(),
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
                &parcel.config.courier.reference,
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
                &parcel.config.courier.reference,
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
                &parcel.config.courier.reference,
            )?;
            validate_required_secrets(&parcel)?;
            let component_path = resolve_wasm_component_path(&parcel)?;
            validate_wasm_component_metadata(&parcel)?;
            let _ = load_wasm_component(&engine, &component_cache, &parcel, &component_path)?;

            let sequence = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
            Ok(CourierSession {
                id: format!("wasm-{}-{sequence}", parcel.config.digest),
                parcel_digest: parcel.config.digest.clone(),
                entrypoint: parcel.config.entrypoint.clone(),
                turn_count: 0,
                history: Vec::new(),
                backend_state: None,
            })
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
        let chat_backend = self.chat_backend.clone();
        async move {
            validate_courier_reference(
                "wasm",
                CourierKind::Wasm,
                &parcel.config.courier.reference,
            )?;
            ensure_session_matches_parcel(&parcel, &session)?;
            validate_wasm_component_metadata(&parcel)?;

            match operation {
                CourierOperation::ResolvePrompt => Ok(CourierResponse {
                    courier_id: "wasm".to_string(),
                    session,
                    events: vec![
                        CourierEvent::PromptResolved {
                            text: resolve_prompt_text(&parcel)?,
                        },
                        CourierEvent::Done,
                    ],
                }),
                CourierOperation::ListLocalTools => Ok(CourierResponse {
                    courier_id: "wasm".to_string(),
                    session,
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
                    let (mut store, guest, parcel_context) =
                        instantiate_wasm_guest(&engine, &component_cache, &parcel, chat_backend)?;
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
                    Ok(CourierResponse {
                        courier_id: "wasm".to_string(),
                        session,
                        events: wasm_events_to_courier_events(result.events),
                    })
                }
            }
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
        let reference = image.config.courier.reference.clone();
        async move { validate_courier_reference(courier_id, kind, &reference) }
    }

    fn inspect(
        &self,
        image: &LoadedParcel,
    ) -> impl Future<Output = Result<CourierInspection, CourierError>> + Send {
        let courier_id = self.courier_id;
        let kind = self.kind;
        let reference = image.config.courier.reference.clone();
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
        let reference = image.config.courier.reference.clone();
        let parcel_digest = image.config.digest.clone();
        let entrypoint = image.config.entrypoint.clone();
        async move {
            validate_courier_reference(courier_id, kind, &reference)?;
            validate_required_secrets(image)?;
            let sequence = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
            Ok(CourierSession {
                id: format!("{courier_id}-{parcel_digest}-{sequence}"),
                parcel_digest,
                entrypoint,
                turn_count: 0,
                history: Vec::new(),
                backend_state: None,
            })
        }
    }

    fn run(
        &self,
        image: &LoadedParcel,
        request: CourierRequest,
    ) -> impl Future<Output = Result<CourierResponse, CourierError>> + Send {
        let courier_id = self.courier_id.to_string();
        let kind = self.kind;
        let reference = image.config.courier.reference.clone();
        let operation = request.operation;
        let mut session = request.session;

        async move {
            validate_courier_reference(&courier_id, kind, &reference)?;
            ensure_session_matches_parcel(image, &session)?;
            session.turn_count += 1;

            match operation {
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
            }
        }
    }
}

fn run_native_task_operation(
    image: &LoadedParcel,
    mut session: CourierSession,
    courier_id: String,
    chat_backend: &dyn ChatModelBackend,
    mode: NativeTurnMode,
    input: String,
) -> Result<CourierResponse, CourierError> {
    session.history.push(ConversationMessage {
        role: "user".to_string(),
        content: input.clone(),
    });
    let mut turn = execute_native_turn(image, &session, &input, chat_backend, mode)?;
    session.history.push(ConversationMessage {
        role: "assistant".to_string(),
        content: turn.reply.clone(),
    });
    turn.events.push(CourierEvent::Message {
        role: "assistant".to_string(),
        content: turn.reply,
    });
    turn.events.push(CourierEvent::Done);

    Ok(CourierResponse {
        courier_id,
        session,
        events: turn.events,
    })
}

fn validate_native_parcel(image: &LoadedParcel) -> Result<(), CourierError> {
    validate_courier_reference(
        "native",
        CourierKind::Native,
        &image.config.courier.reference,
    )
}

fn validate_wasm_component_metadata(parcel: &LoadedParcel) -> Result<(), CourierError> {
    let component = parcel.config.courier.component.as_ref().ok_or_else(|| {
        CourierError::MissingCourierComponent {
            courier: "wasm".to_string(),
            parcel_digest: parcel.config.digest.clone(),
        }
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
    let component = parcel.config.courier.component.as_ref().ok_or_else(|| {
        CourierError::MissingCourierComponent {
            courier: "wasm".to_string(),
            parcel_digest: parcel.config.digest.clone(),
        }
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
    component_cache: &Arc<Mutex<BTreeMap<String, Component>>>,
    parcel: &LoadedParcel,
    path: &Path,
) -> Result<Component, CourierError> {
    let component_config = parcel.config.courier.component.as_ref().ok_or_else(|| {
        CourierError::MissingCourierComponent {
            courier: "wasm".to_string(),
            parcel_digest: parcel.config.digest.clone(),
        }
    })?;
    if let Some(component) = component_cache
        .lock()
        .expect("wasm component cache lock poisoned")
        .get(&component_config.sha256)
        .cloned()
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
    component_cache: &Arc<Mutex<BTreeMap<String, Component>>>,
    parcel: &LoadedParcel,
    chat_backend: Arc<dyn ChatModelBackend>,
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
                chat_backend,
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

fn execute_native_turn(
    image: &LoadedParcel,
    session: &CourierSession,
    input: &str,
    chat_backend: &dyn ChatModelBackend,
    mode: NativeTurnMode,
) -> Result<ChatTurnResult, CourierError> {
    let trimmed = input.trim();
    let local_tools = list_local_tools(image);
    let mut events = Vec::new();

    if matches!(mode, NativeTurnMode::Chat) && trimmed.eq_ignore_ascii_case("/prompt") {
        return Ok(ChatTurnResult {
            reply: resolve_prompt_text(image)?,
            events: Vec::new(),
        });
    }

    if matches!(mode, NativeTurnMode::Chat) && trimmed.eq_ignore_ascii_case("/tools") {
        if local_tools.is_empty() {
            return Ok(ChatTurnResult {
                reply: "No local tools are declared for this image.".to_string(),
                events: Vec::new(),
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
        });
    }

    if matches!(mode, NativeTurnMode::Chat) && trimmed.eq_ignore_ascii_case("/help") {
        return Ok(ChatTurnResult {
            reply:
                "Native chat is a reference backend. Available commands: /prompt, /tools, /help."
                    .to_string(),
            events: Vec::new(),
        });
    }

    if let Some(mut request) = build_model_request(image, &session.history, &local_tools)? {
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
        loop {
            if rounds >= MAX_TOOL_ROUNDS {
                events.push(CourierEvent::BackendFallback {
                    backend: chat_backend.id().to_string(),
                    error: format!(
                        "tool call loop reached {} rounds without a final reply; falling back to local reference reply",
                        MAX_TOOL_ROUNDS
                    ),
                });
                break;
            }
            rounds += 1;

            let reply = match chat_backend.generate(&request) {
                Ok(Some(reply)) => reply,
                Ok(None) => break,
                Err(error) => {
                    events.push(CourierEvent::BackendFallback {
                        backend: chat_backend.id().to_string(),
                        error: error.to_string(),
                    });
                    break;
                }
            };

            if !reply.tool_calls.is_empty() {
                let mut tool_outputs = Vec::with_capacity(reply.tool_calls.len());
                for tool_call in reply.tool_calls {
                    let tool = local_tools
                        .iter()
                        .find(|t| t.alias == tool_call.name || t.packaged_path == tool_call.name)
                        .ok_or_else(|| CourierError::UnknownLocalTool {
                            tool: tool_call.name.clone(),
                        })?;
                    events.push(CourierEvent::ToolCallStarted {
                        invocation: ToolInvocation {
                            name: tool_call.name.clone(),
                            input: Some(tool_call.input.clone()),
                        },
                        command: tool.command.clone(),
                        args: tool.args.clone(),
                    });
                    let tool_result = execute_local_tool(image, tool, Some(&tool_call.input))?;
                    events.push(CourierEvent::ToolCallFinished {
                        result: tool_result.clone(),
                    });
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
                    tool_outputs.push(ModelToolOutput {
                        call_id: tool_call.call_id,
                        output: combined_output,
                        kind: tool_call.kind,
                    });
                }

                request.messages.clear();
                request.tool_outputs = tool_outputs;
                request.previous_response_id = reply.response_id;
                continue;
            }

            if let Some(text) = reply.text {
                return Ok(ChatTurnResult {
                    reply: text,
                    events,
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
    let tool_count = local_tools.len();
    let prior_messages = session.history.len().saturating_sub(1);

    Ok(ChatTurnResult {
        reply: format!(
            "Native {} reference reply for turn {}. Loaded {} prompt section(s) and {} local tool(s). Prior messages in session: {}. Input: {}",
            native_turn_mode_name(mode),
            session.turn_count,
            prompt_sections,
            tool_count,
            prior_messages,
            input
        ),
        events,
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

fn build_model_request(
    image: &LoadedParcel,
    messages: &[ConversationMessage],
    local_tools: &[LocalToolSpec],
) -> Result<Option<ModelRequest>, CourierError> {
    let Some(primary) = &image.config.models.primary else {
        return Ok(None);
    };

    Ok(Some(ModelRequest {
        model: primary.id.clone(),
        instructions: resolve_prompt_text(image)?,
        messages: messages.to_vec(),
        tools: local_tools
            .iter()
            .cloned()
            .map(|tool| build_model_tool_definition(image, tool))
            .collect::<Result<Vec<_>, _>>()?,
        tool_outputs: Vec::new(),
        previous_response_id: None,
    }))
}

fn build_model_tool_definition(
    image: &LoadedParcel,
    tool: LocalToolSpec,
) -> Result<ModelToolDefinition, CourierError> {
    let description = tool.description.unwrap_or_else(|| {
        format!(
            "Local Dispatch tool `{}` packaged at `{}`. Provide free-form text or JSON input appropriate for the tool.",
            tool.alias, tool.packaged_path
        )
    });
    let format = match (tool.input_schema_packaged_path, tool.input_schema_sha256) {
        (Some(source), expected_sha256) => ModelToolFormat::JsonSchema {
            schema: load_tool_schema(image, &tool.alias, &source, expected_sha256.as_deref())?,
        },
        (None, _) => ModelToolFormat::Text,
    };

    Ok(ModelToolDefinition {
        name: tool.alias,
        description,
        format,
    })
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

impl ChatModelBackend for OpenAiResponsesBackend {
    fn id(&self) -> &str {
        "openai_responses"
    }

    fn generate(&self, request: &ModelRequest) -> Result<Option<ModelReply>, CourierError> {
        let api_key = match std::env::var("OPENAI_API_KEY") {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };

        let base_url = std::env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com".to_string());
        let url = format!("{}/v1/responses", base_url.trim_end_matches('/'));
        let payload = serde_json::json!({
            "model": request.model,
            "instructions": request.instructions,
            "input": if request.previous_response_id.is_some() {
                request
                    .tool_outputs
                    .iter()
                    .map(openai_tool_output_item)
                    .collect::<Vec<_>>()
            } else {
                request
                    .messages
                    .iter()
                    .map(openai_input_message)
                    .collect::<Vec<_>>()
            },
            "previous_response_id": request.previous_response_id,
            "parallel_tool_calls": false,
            "tools": request
                .tools
                .iter()
                .map(openai_tool_definition)
                .collect::<Vec<_>>(),
        });

        let mut response = ureq::post(&url)
            .header("authorization", &format!("Bearer {api_key}"))
            .header("content-type", "application/json")
            .send_json(payload)
            .map_err(|error| CourierError::ModelBackendRequest(error.to_string()))?;

        let body: serde_json::Value = response
            .body_mut()
            .read_json()
            .map_err(|error| CourierError::ModelBackendResponse(error.to_string()))?;
        let (text, tool_calls) = extract_openai_output(&body)?;

        Ok(Some(ModelReply {
            text,
            backend: self.id().to_string(),
            response_id: body
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string),
            tool_calls,
        }))
    }
}

fn openai_input_message(message: &ConversationMessage) -> serde_json::Value {
    serde_json::json!({
        "role": message.role,
        "content": [
            {
                "type": "input_text",
                "text": message.content,
            }
        ],
    })
}

fn openai_tool_definition(tool: &ModelToolDefinition) -> serde_json::Value {
    match &tool.format {
        ModelToolFormat::Text => serde_json::json!({
            "type": "custom",
            "name": tool.name,
            "description": tool.description,
            "format": { "type": "text" },
        }),
        ModelToolFormat::JsonSchema { schema } => serde_json::json!({
            "type": "function",
            "name": tool.name,
            "description": tool.description,
            "parameters": schema,
        }),
    }
}

fn openai_tool_output_item(output: &ModelToolOutput) -> serde_json::Value {
    match output.kind {
        ModelToolKind::Custom => serde_json::json!({
            "type": "custom_tool_call_output",
            "call_id": output.call_id,
            "output": output.output,
        }),
        ModelToolKind::Function => serde_json::json!({
            "type": "function_call_output",
            "call_id": output.call_id,
            "output": output.output,
        }),
    }
}

fn extract_openai_output(
    body: &serde_json::Value,
) -> Result<(Option<String>, Vec<ModelToolCall>), CourierError> {
    let outputs = body
        .get("output")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| CourierError::ModelBackendResponse("missing `output` array".to_string()))?;

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for output in outputs {
        match output.get("type").and_then(serde_json::Value::as_str) {
            Some("custom_tool_call") => {
                tool_calls.push(parse_openai_tool_call(output, ModelToolKind::Custom)?);
                continue;
            }
            Some("function_call") => {
                tool_calls.push(parse_openai_tool_call(output, ModelToolKind::Function)?);
                continue;
            }
            _ => {}
        }

        let Some(content) = output.get("content").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for item in content {
            if item.get("type").and_then(serde_json::Value::as_str) == Some("output_text")
                && let Some(value) = item.get("text").and_then(serde_json::Value::as_str)
            {
                text.push_str(value);
            }
        }
    }

    if !tool_calls.is_empty() {
        return Ok((None, tool_calls));
    }

    if text.is_empty() {
        return Err(CourierError::ModelBackendResponse(
            "response did not contain `output_text` content".to_string(),
        ));
    }

    Ok((Some(text), tool_calls))
}

fn parse_openai_tool_call(
    output: &serde_json::Value,
    kind: ModelToolKind,
) -> Result<ModelToolCall, CourierError> {
    let output_type = output
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("tool_call");
    let call_id = output
        .get("call_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            CourierError::ModelBackendResponse(format!("{output_type} missing `call_id`"))
        })?;
    let name = output
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            CourierError::ModelBackendResponse(format!("{output_type} missing `name`"))
        })?;
    let input_field = match kind {
        ModelToolKind::Custom => "input",
        ModelToolKind::Function => "arguments",
    };
    let input = output
        .get(input_field)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            CourierError::ModelBackendResponse(format!("{output_type} missing `{input_field}`"))
        })?;

    Ok(ModelToolCall {
        call_id: call_id.to_string(),
        name: name.to_string(),
        input: input.to_string(),
        kind,
    })
}

fn encode_hex(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BuildOptions, build_agentfile};
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    struct TestImage {
        _dir: tempfile::TempDir,
        image: LoadedParcel,
    }

    fn build_test_image(agentfile: &str, files: &[(&str, &str)]) -> TestImage {
        build_test_image_with_binary_files(agentfile, files, &[])
    }

    fn build_test_image_with_binary_files(
        agentfile: &str,
        files: &[(&str, &str)],
        binary_files: &[(&str, &[u8])],
    ) -> TestImage {
        let dir = tempdir().unwrap();
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

        let built = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap();

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
    ) -> JsonlCourierPlugin {
        let plugin_path = dir.path().join("plugin.sh");
        let script = if error_mode {
            "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{\"kind\":\"error\",\"error\":{\"code\":\"bad_request\",\"message\":\"plugin rejected request\"}}'\n"
                .to_string()
        } else {
            format!(
                "#!/bin/sh\nrequest=$(cat)\ncase \"$request\" in\n*'\"kind\":\"capabilities\"'*)\nprintf '%s\\n' '{{\"kind\":\"result\",\"capabilities\":{{\"courier_id\":\"demo-plugin\",\"kind\":\"custom\",\"supports_chat\":true,\"supports_job\":false,\"supports_heartbeat\":false,\"supports_local_tools\":false,\"supports_mounts\":[]}}}}'\n;;\n*'\"kind\":\"validate_parcel\"'*)\nprintf '%s\\n' '{{\"kind\":\"result\"}}'\n;;\n*'\"kind\":\"inspect\"'*)\nprintf '%s\\n' '{{\"kind\":\"result\",\"inspection\":{{\"courier_id\":\"demo-plugin\",\"kind\":\"custom\",\"entrypoint\":\"chat\",\"required_secrets\":[],\"mounts\":[],\"local_tools\":[]}}}}'\n;;\n*'\"kind\":\"open_session\"'*)\nprintf '%s\\n' '{{\"kind\":\"result\",\"session\":{{\"id\":\"plugin-session\",\"parcel_digest\":\"{digest}\",\"entrypoint\":\"chat\",\"turn_count\":0,\"history\":[]}}}}'\n;;\n*'\"kind\":\"run\"'*)\nprintf '%s\\n' '{{\"kind\":\"event\",\"event\":{{\"kind\":\"message\",\"role\":\"assistant\",\"content\":\"from plugin\"}}}}'\nprintf '%s\\n' '{{\"kind\":\"done\",\"session\":{{\"id\":\"plugin-session\",\"parcel_digest\":\"{digest}\",\"entrypoint\":\"chat\",\"turn_count\":1,\"history\":[{{\"role\":\"user\",\"content\":\"hello plugin\"}},{{\"role\":\"assistant\",\"content\":\"from plugin\"}}]}}}}'\n;;\n*)\nprintf '%s\\n' '{{\"kind\":\"error\",\"error\":{{\"code\":\"bad_request\",\"message\":\"unexpected request\"}}}}'\n;;\nesac\n"
            )
        };
        fs::write(&plugin_path, script).unwrap();
        fs::set_permissions(&plugin_path, fs::Permissions::from_mode(0o755)).unwrap();

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
        })
    }

    #[derive(Default)]
    struct FakeChatBackend {
        replies: Mutex<Vec<Result<Option<ModelReply>, String>>>,
        calls: Mutex<Vec<ModelRequest>>,
    }

    impl FakeChatBackend {
        fn with_reply(reply: impl Into<String>) -> Self {
            Self {
                replies: Mutex::new(vec![Ok(Some(ModelReply {
                    text: Some(reply.into()),
                    backend: "fake".to_string(),
                    response_id: None,
                    tool_calls: Vec::new(),
                }))]),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn with_replies(replies: Vec<Option<ModelReply>>) -> Self {
            Self {
                replies: Mutex::new(replies.into_iter().map(Ok).collect()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn with_error(error: impl Into<String>) -> Self {
            Self {
                replies: Mutex::new(vec![Err(error.into())]),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl ChatModelBackend for FakeChatBackend {
        fn id(&self) -> &str {
            "fake"
        }

        fn generate(&self, request: &ModelRequest) -> Result<Option<ModelReply>, CourierError> {
            self.calls.lock().unwrap().push(request.clone());
            let mut replies = self.replies.lock().unwrap();
            if replies.is_empty() {
                return Ok(None);
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
        let courier =
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
        let courier =
            build_test_plugin_courier(&test_image._dir, &test_image.image.config.digest, true);

        let error = futures::executor::block_on(courier.inspect(&test_image.image)).unwrap_err();
        assert!(matches!(
            error,
            CourierError::PluginProtocol { courier, message }
                if courier == "demo-plugin" && message.contains("bad_request") && message.contains("plugin rejected request")
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
        assert_eq!(tools[0].command, "python3");
        assert_eq!(tools[0].args, vec!["-u"]);
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
    fn docker_courier_rejects_chat_execution() {
        let test_image = build_test_image(
            "\
FROM dispatch/docker:latest
ENTRYPOINT chat
",
            &[],
        );
        let courier = DockerCourier::default();
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
            CourierError::UnsupportedOperation { courier, operation }
                if courier == "docker" && operation == "chat"
        ));
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
        assert!(response.session.history[1].content.contains("1 local tool"));
        assert!(matches!(
            response.events.first(),
            Some(CourierEvent::Message { role, content })
                if role == "assistant" && content.contains("hello courier")
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
            Some(CourierEvent::Message { content, .. }) if content.contains("Native chat reference reply")
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

        unsafe {
            std::env::set_var("CAST_VISIBLE_SECRET", "secret-value");
            std::env::set_var("CAST_HIDDEN_ENV", "hidden-value");
        }

        let result = run_local_tool(&test_image.image, "envcheck", None).unwrap();

        assert!(result.stdout.contains("visible_env=visible"));
        assert!(result.stdout.contains("visible_secret=secret-value"));
        assert!(result.stdout.contains("hidden_env="));
        assert!(!result.stdout.contains("hidden_env=hidden-value"));
    }
}
