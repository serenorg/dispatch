use super::model_request::{build_wasm_model_requests, load_tool_schema, select_chat_backend};
use super::*;
use dispatch_wasm_abi::ABI as DISPATCH_WASM_COMPONENT_ABI;
use wasmtime::{
    Store,
    component::{HasSelf, Linker, ResourceTable},
};
use wasmtime_wasi::p2;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

pub(super) struct WasmHostState {
    host: WasmHost,
    wasi_ctx: WasiCtx,
    resource_table: ResourceTable,
}

pub(super) struct WasmHost {
    parcel: LoadedParcel,
    session: CourierSession,
    chat_backend_override: Option<Arc<dyn ChatModelBackend>>,
    run_deadline: Option<Instant>,
}

#[derive(Debug, Clone)]
struct CachedValue<T> {
    value: T,
    last_used: u64,
}

#[derive(Debug, Clone)]
pub(super) struct BoundedLruCache<T> {
    max_entries: usize,
    tick: u64,
    entries: BTreeMap<String, CachedValue<T>>,
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
        if let Some(request) = build_local_tool_approval_request(&tool, invocation.input.as_deref())
            && !check_tool_approval(&request).map_err(|error| error.to_string())?
        {
            return Err(CourierError::ApprovalDenied { tool: request.tool }.to_string());
        }
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

impl<T: Clone> BoundedLruCache<T> {
    pub(super) fn new(max_entries: usize) -> Self {
        Self {
            max_entries,
            tick: 0,
            entries: BTreeMap::new(),
        }
    }

    pub(super) fn get(&mut self, key: &str) -> Option<T> {
        let entry = self.entries.get(key).cloned()?;
        self.tick = self.tick.saturating_add(1);
        if let Some(current) = self.entries.get_mut(key) {
            current.last_used = self.tick;
        }
        Some(entry.value)
    }

    pub(super) fn insert(&mut self, key: String, value: T) {
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
    pub(super) fn keys(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }
}

pub(super) fn wasm_component_cache_limit() -> usize {
    std::env::var("DISPATCH_WASM_COMPONENT_CACHE_SIZE")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(16)
}

pub(super) fn validate_wasm_component_metadata(parcel: &LoadedParcel) -> Result<(), CourierError> {
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

pub(super) fn resolve_wasm_component_path(parcel: &LoadedParcel) -> Result<PathBuf, CourierError> {
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

pub(super) fn load_wasm_component(
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

pub(super) fn instantiate_wasm_guest(
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
    p2::add_to_linker_sync(&mut linker).map_err(|source| {
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

pub(super) fn wasm_guest_session(
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

pub(super) fn wasm_operation(
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

pub(super) fn wasm_events_to_courier_events(
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

pub(super) fn apply_wasm_turn_to_session(
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
