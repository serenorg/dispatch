use super::model_request::load_tool_schema;
use super::{
    Arc, BoundedLruCache, ChatModelBackend, Component, ConversationMessage, CourierError,
    CourierEvent, CourierOperation, CourierSession, Engine, HasSelf, Linker, LoadedParcel,
    LocalToolSpec, Mutex, Path, PathBuf, ResourceTable, Store, WasiCtx, WasmHost, WasmHostState,
    list_local_tools, resolve_prompt_text, run_timeout_deadline, wasm_bindings,
};
use dispatch_wasm_abi::ABI as DISPATCH_WASM_COMPONENT_ABI;
use wasmtime_wasi::p2;

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
