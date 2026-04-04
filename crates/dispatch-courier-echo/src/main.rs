use dispatch_core::{
    COURIER_PLUGIN_PROTOCOL_VERSION, ConversationMessage, CourierCapabilities, CourierEvent,
    CourierInspection, CourierKind, CourierOperation, CourierSession, LoadedParcel,
    PluginErrorPayload, PluginRequest, PluginRequestEnvelope, PluginResponse, list_local_tools,
    load_parcel, resolve_prompt_text,
};
use std::{
    io::{self, BufRead as _, Write as _},
    path::Path,
};

fn main() -> std::process::ExitCode {
    match run_stdio() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            let _ = emit_response(&PluginResponse::Error {
                error: PluginErrorPayload {
                    code: "courier_error".to_string(),
                    message: error,
                },
            });
            std::process::ExitCode::from(1)
        }
    }
}

fn run_stdio() -> Result<(), String> {
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = line.map_err(|error| format!("failed to read request: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let envelope: PluginRequestEnvelope = serde_json::from_str(&line)
            .map_err(|error| format!("invalid request JSON: {error}"))?;
        if envelope.protocol_version != COURIER_PLUGIN_PROTOCOL_VERSION {
            return Err(format!(
                "unsupported protocol version {}",
                envelope.protocol_version
            ));
        }

        let should_shutdown = matches!(envelope.request, PluginRequest::Shutdown);
        for response in handle_request(envelope.request)? {
            emit_response(&response)
                .map_err(|error| format!("failed to write response: {error}"))?;
        }
        if should_shutdown {
            break;
        }
    }
    Ok(())
}

fn emit_response(response: &PluginResponse) -> io::Result<()> {
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    serde_json::to_writer(&mut lock, response)?;
    lock.write_all(b"\n")?;
    lock.flush()
}

fn handle_request(request: PluginRequest) -> Result<Vec<PluginResponse>, String> {
    match request {
        PluginRequest::Capabilities => Ok(vec![PluginResponse::Capabilities {
            capabilities: CourierCapabilities {
                courier_id: "echo".to_string(),
                kind: CourierKind::Custom,
                supports_chat: true,
                supports_job: true,
                supports_heartbeat: true,
                supports_local_tools: false,
                supports_mounts: Vec::new(),
            },
        }]),
        PluginRequest::ValidateParcel { parcel_dir } => {
            let _ = load_parcel(Path::new(&parcel_dir))
                .map_err(|error| format!("failed to load parcel: {error}"))?;
            Ok(vec![PluginResponse::Ok])
        }
        PluginRequest::Inspect { parcel_dir } => {
            let parcel = load_parcel(Path::new(&parcel_dir))
                .map_err(|error| format!("failed to load parcel: {error}"))?;
            let local_tools = list_local_tools(&parcel);
            Ok(vec![PluginResponse::Inspection {
                inspection: CourierInspection {
                    courier_id: "echo".to_string(),
                    kind: CourierKind::Custom,
                    entrypoint: parcel.config.entrypoint.clone(),
                    required_secrets: parcel
                        .config
                        .secrets
                        .iter()
                        .map(|secret| secret.name.clone())
                        .collect(),
                    mounts: parcel.config.mounts.clone(),
                    local_tools,
                },
            }])
        }
        PluginRequest::OpenSession { parcel_dir } => {
            let parcel = load_parcel(Path::new(&parcel_dir))
                .map_err(|error| format!("failed to load parcel: {error}"))?;
            Ok(vec![PluginResponse::Session {
                session: CourierSession {
                    id: format!("echo-{}", parcel.config.digest),
                    parcel_digest: parcel.config.digest,
                    entrypoint: parcel.config.entrypoint,
                    label: parcel.config.name,
                    turn_count: 0,
                    elapsed_ms: 0,
                    history: Vec::new(),
                    resolved_mounts: Vec::new(),
                    backend_state: Some("open".to_string()),
                },
            }])
        }
        PluginRequest::ResumeSession {
            parcel_dir,
            mut session,
        } => {
            let parcel = load_parcel(Path::new(&parcel_dir))
                .map_err(|error| format!("failed to load parcel: {error}"))?;
            if session.parcel_digest != parcel.config.digest {
                return Err(format!(
                    "session digest {} does not match parcel {}",
                    session.parcel_digest, parcel.config.digest
                ));
            }
            session.backend_state = Some(
                session
                    .backend_state
                    .as_deref()
                    .map(|value| format!("{value}|resumed"))
                    .unwrap_or_else(|| "resumed".to_string()),
            );
            Ok(vec![PluginResponse::Session { session }])
        }
        PluginRequest::Shutdown => Ok(vec![PluginResponse::Ok]),
        PluginRequest::Run {
            parcel_dir,
            session,
            operation,
        } => {
            let parcel = load_parcel(Path::new(&parcel_dir))
                .map_err(|error| format!("failed to load parcel: {error}"))?;
            handle_run(&parcel, session, operation)
        }
    }
}

fn handle_run(
    parcel: &LoadedParcel,
    session: CourierSession,
    operation: CourierOperation,
) -> Result<Vec<PluginResponse>, String> {
    if session.parcel_digest != parcel.config.digest {
        return Err(format!(
            "session digest {} does not match parcel {}",
            session.parcel_digest, parcel.config.digest
        ));
    }

    match operation {
        CourierOperation::ResolvePrompt => Ok(vec![
            PluginResponse::Event {
                event: CourierEvent::PromptResolved {
                    text: resolve_prompt_text(parcel)
                        .map_err(|error| format!("failed to resolve prompt: {error}"))?,
                },
            },
            PluginResponse::Done {
                session: next_turn(session),
            },
        ]),
        CourierOperation::ListLocalTools => Ok(vec![
            PluginResponse::Event {
                event: CourierEvent::LocalToolsListed {
                    tools: list_local_tools(parcel),
                },
            },
            PluginResponse::Done {
                session: next_turn(session),
            },
        ]),
        CourierOperation::Chat { input } => {
            let reply = format!("echo: {input}");
            Ok(message_turn(session, input, reply))
        }
        CourierOperation::Job { payload } => {
            let reply = format!("job: {payload}");
            Ok(message_turn(session, payload, reply))
        }
        CourierOperation::Heartbeat { payload } => {
            let payload = payload.unwrap_or_else(|| "tick".to_string());
            let reply = format!("heartbeat: {payload}");
            Ok(message_turn(session, payload, reply))
        }
        CourierOperation::InvokeTool { invocation } => Ok(vec![PluginResponse::Error {
            error: PluginErrorPayload {
                code: "unsupported_operation".to_string(),
                message: format!(
                    "tool invocation is not supported by echo courier: {}",
                    invocation.name
                ),
            },
        }]),
    }
}

fn message_turn(mut session: CourierSession, input: String, reply: String) -> Vec<PluginResponse> {
    session.history.push(ConversationMessage {
        role: "user".to_string(),
        content: input,
    });
    session.history.push(ConversationMessage {
        role: "assistant".to_string(),
        content: reply.clone(),
    });
    session.turn_count += 1;
    session.backend_state = Some(format!("turns:{}", session.turn_count));

    vec![
        PluginResponse::Event {
            event: CourierEvent::Message {
                role: "assistant".to_string(),
                content: reply,
            },
        },
        PluginResponse::Done { session },
    ]
}

fn next_turn(mut session: CourierSession) -> CourierSession {
    session.turn_count += 1;
    session
}

#[cfg(test)]
mod tests {
    use super::*;
    use dispatch_core::{BuildOptions, build_agentfile};
    use tempfile::tempdir;

    #[test]
    fn capabilities_request_reports_custom_courier() {
        let responses = handle_request(PluginRequest::Capabilities).unwrap();
        assert_eq!(responses.len(), 1);
        let PluginResponse::Capabilities { capabilities } = &responses[0] else {
            panic!("expected capabilities result");
        };
        assert_eq!(capabilities.courier_id, "echo");
        assert!(capabilities.supports_chat);
    }

    #[test]
    fn chat_run_emits_message_and_updates_history() {
        let dir = tempdir().unwrap();
        let parcel = build_test_parcel(dir.path());
        let session = CourierSession {
            id: format!("echo-{}", parcel.config.digest),
            parcel_digest: parcel.config.digest.clone(),
            entrypoint: Some("chat".to_string()),
            label: None,
            turn_count: 0,
            elapsed_ms: 0,
            history: Vec::new(),
            resolved_mounts: Vec::new(),
            backend_state: None,
        };

        let responses = handle_run(
            &parcel,
            session,
            CourierOperation::Chat {
                input: "hello".to_string(),
            },
        )
        .unwrap();

        assert!(matches!(responses[0], PluginResponse::Event { .. }));
        let PluginResponse::Done { session } = &responses[1] else {
            panic!("expected done response");
        };
        assert_eq!(session.turn_count, 1);
        assert_eq!(session.history.len(), 2);
        assert_eq!(session.history[1].content, "echo: hello");
        assert_eq!(session.backend_state.as_deref(), Some("turns:1"));
    }

    #[test]
    fn resume_session_preserves_and_updates_backend_state() {
        let dir = tempdir().unwrap();
        let parcel = build_test_parcel(dir.path());
        let responses = handle_request(PluginRequest::ResumeSession {
            parcel_dir: parcel.parcel_dir.display().to_string(),
            session: CourierSession {
                id: format!("echo-{}", parcel.config.digest),
                parcel_digest: parcel.config.digest.clone(),
                entrypoint: Some("chat".to_string()),
                label: None,
                turn_count: 1,
                elapsed_ms: 0,
                history: Vec::new(),
                resolved_mounts: Vec::new(),
                backend_state: Some("warm".to_string()),
            },
        })
        .unwrap();

        let PluginResponse::Session { session } = &responses[0] else {
            panic!("expected session response");
        };
        assert_eq!(session.backend_state.as_deref(), Some("warm|resumed"));
    }

    fn build_test_parcel(root: &Path) -> LoadedParcel {
        let context_dir = root.join("parcel");
        std::fs::create_dir_all(&context_dir).unwrap();
        std::fs::write(
            context_dir.join("Agentfile"),
            "FROM dispatch/native:latest\n\
NAME echo-plugin-test\n\
VERSION 0.1.0\n\
SKILL SKILL.md\n\
ENTRYPOINT chat\n",
        )
        .unwrap();
        std::fs::write(context_dir.join("SKILL.md"), "You are a test agent.\n").unwrap();

        let built = build_agentfile(
            &context_dir.join("Agentfile"),
            &BuildOptions {
                output_root: context_dir.join(".dispatch/parcels"),
            },
        )
        .unwrap();
        load_parcel(&built.parcel_dir).unwrap()
    }
}
