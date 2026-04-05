use super::*;
use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

#[derive(Default)]
pub(crate) struct CodexAppServerBackend;

impl ChatModelBackend for CodexAppServerBackend {
    fn id(&self) -> &str {
        CODEX_BACKEND_ID
    }

    fn supports_previous_response_id(&self) -> bool {
        true
    }

    fn generate(&self, request: &ModelRequest) -> Result<ModelGeneration, CourierError> {
        generate_with_noop_events(self, request)
    }

    fn generate_with_events(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent),
    ) -> Result<ModelGeneration, CourierError> {
        let Some(working_directory) = request.working_directory.as_deref() else {
            return Ok(ModelGeneration::NotConfigured {
                backend: self.id().to_string(),
                reason: "missing working directory for codex app-server request".to_string(),
            });
        };

        let Some(mut process) = CodexProcess::spawn(working_directory)? else {
            return Ok(ModelGeneration::NotConfigured {
                backend: self.id().to_string(),
                reason: "missing CODEX_BINARY or `codex` executable".to_string(),
            });
        };

        process.initialize()?;
        let thread_id = process.open_thread(request)?;
        let prompt = codex_prompt_text(request);
        let turn_id = process.start_turn(&thread_id, &request.model, &prompt)?;
        let text = process.collect_turn_output(&turn_id, on_event)?;

        Ok(ModelGeneration::Reply(ModelReply {
            text: Some(text),
            backend: self.id().to_string(),
            response_id: Some(thread_id),
            tool_calls: Vec::new(),
        }))
    }
}

#[cfg(test)]
impl CodexAppServerBackend {
    pub(crate) fn with_binary_path_for_tests(path: impl Into<String>) -> Self {
        TEST_CODEX_BINARY_OVERRIDE
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .expect("codex binary override lock poisoned")
            .replace(path.into());
        Self
    }
}

#[cfg(test)]
static TEST_CODEX_BINARY_OVERRIDE: std::sync::OnceLock<std::sync::Mutex<Option<String>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn clear_test_codex_binary_override() {
    if let Some(slot) = TEST_CODEX_BINARY_OVERRIDE.get() {
        *slot.lock().expect("codex binary override lock poisoned") = None;
    }
}

fn codex_binary_path() -> String {
    #[cfg(test)]
    if let Some(slot) = TEST_CODEX_BINARY_OVERRIDE.get()
        && let Some(path) = slot
            .lock()
            .expect("codex binary override lock poisoned")
            .clone()
    {
        return path;
    }

    std::env::var("CODEX_BINARY").unwrap_or_else(|_| "codex".to_string())
}

fn codex_reasoning_effort() -> String {
    std::env::var("CODEX_REASONING_EFFORT")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "medium".to_string())
}

fn codex_prompt_text(request: &ModelRequest) -> String {
    let latest_user = request
        .messages
        .iter()
        .rev()
        .find(|message| message.role.eq_ignore_ascii_case("user"))
        .map(|message| message.content.clone());

    let tool_note = if request.tools.is_empty() {
        None
    } else {
        Some(
            "Dispatch note: declared parcel tools are not bridged into the codex app-server backend in this runtime. Do not assume tool access."
                .to_string(),
        )
    };

    if request.previous_response_id.is_some() {
        let mut parts = Vec::new();
        if let Some(note) = tool_note {
            parts.push(note);
        }
        if let Some(text) = latest_user {
            parts.push(text);
        } else if !request.messages.is_empty() {
            parts.push(render_conversation_transcript(&request.messages));
        } else if !request.instructions.trim().is_empty() {
            parts.push(request.instructions.clone());
        }
        return parts.join("\n\n");
    }

    let mut sections = Vec::new();
    if !request.instructions.trim().is_empty() {
        sections.push(format!("System instructions:\n{}", request.instructions));
    }
    if let Some(note) = tool_note {
        sections.push(note);
    }
    if !request.messages.is_empty() {
        sections.push(format!(
            "Conversation so far:\n{}",
            render_conversation_transcript(&request.messages)
        ));
    }
    sections.join("\n\n")
}

fn render_conversation_transcript(messages: &[ConversationMessage]) -> String {
    messages
        .iter()
        .map(|message| {
            let role = if message.role.eq_ignore_ascii_case("assistant") {
                "Assistant"
            } else if message.role.eq_ignore_ascii_case("user") {
                "User"
            } else {
                "Message"
            };
            format!("{role}: {}", message.content)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

struct CodexProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_request_id: i64,
}

impl CodexProcess {
    fn spawn(working_directory: &str) -> Result<Option<Self>, CourierError> {
        let binary = codex_binary_path();
        let mut command = Command::new(&binary);
        command
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(Path::new(working_directory));
        if let Ok(home) = std::env::var("CODEX_HOME") {
            let trimmed = home.trim();
            if !trimmed.is_empty() {
                command.env("CODEX_HOME", trimmed);
            }
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(CourierError::ModelBackendRequest(format!(
                    "failed to start codex app-server: {error}"
                )));
            }
        };
        let stdin = child.stdin.take().ok_or_else(|| {
            CourierError::ModelBackendRequest("codex app-server missing stdin".to_string())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            CourierError::ModelBackendRequest("codex app-server missing stdout".to_string())
        })?;
        Ok(Some(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_request_id: 1,
        }))
    }

    fn initialize(&mut self) -> Result<(), CourierError> {
        self.request(
            "initialize",
            serde_json::json!({
                "clientInfo": {
                    "name": "dispatch",
                    "title": "Dispatch",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": {
                    "experimentalApi": true
                }
            }),
        )?;
        self.notify("initialized", serde_json::json!({}))?;
        Ok(())
    }

    fn open_thread(&mut self, request: &ModelRequest) -> Result<String, CourierError> {
        if let Some(thread_id) = request.previous_response_id.as_deref()
            && let Ok(value) = self.request(
                "thread/resume",
                serde_json::json!({
                    "threadId": thread_id,
                    "cwd": request.working_directory,
                    "approvalPolicy": "on-request",
                    "sandbox": "workspace-write",
                    "experimentalRawEvents": false,
                    "model": request.model,
                }),
            )
            && let Some(thread_id) = extract_thread_id(&value)
        {
            return Ok(thread_id);
        }

        let value = self.request(
            "thread/start",
            serde_json::json!({
                "cwd": request.working_directory,
                "approvalPolicy": "on-request",
                "sandbox": "workspace-write",
                "experimentalRawEvents": false,
                "model": request.model,
            }),
        )?;
        extract_thread_id(&value).ok_or_else(|| {
            CourierError::ModelBackendResponse(
                "codex thread/start response did not include a thread id".to_string(),
            )
        })
    }

    fn start_turn(
        &mut self,
        thread_id: &str,
        model: &str,
        prompt: &str,
    ) -> Result<String, CourierError> {
        let value = self.request(
            "turn/start",
            serde_json::json!({
                "threadId": thread_id,
                "input": [{ "type": "text", "text": prompt }],
                "model": model,
                "effort": codex_reasoning_effort(),
            }),
        )?;
        value
            .get("turn")
            .and_then(|turn| turn.get("id"))
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| {
                CourierError::ModelBackendResponse(
                    "codex turn/start response did not include a turn id".to_string(),
                )
            })
    }

    fn collect_turn_output(
        &mut self,
        turn_id: &str,
        on_event: &mut dyn FnMut(ModelStreamEvent),
    ) -> Result<String, CourierError> {
        let mut reply = String::new();
        loop {
            let value = self.read_message()?;
            if is_response(&value) {
                continue;
            }
            if is_server_request(&value) {
                self.respond_to_server_request(&value)?;
                continue;
            }

            let Some(method) = value.get("method").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let params = value
                .get("params")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            match method {
                "item/agentMessage/delta" => {
                    if let Some(delta) = params.get("delta").and_then(serde_json::Value::as_str) {
                        reply.push_str(delta);
                        on_event(ModelStreamEvent::TextDelta {
                            content: delta.to_string(),
                        });
                    }
                }
                "turn/completed" => {
                    let turn = params.get("turn").unwrap_or(&params);
                    let completed_turn_id = turn
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    if !completed_turn_id.is_empty() && completed_turn_id != turn_id {
                        continue;
                    }
                    let status = turn
                        .get("status")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("completed");
                    if status == "failed" {
                        let message = turn
                            .get("error")
                            .and_then(|value| value.get("message"))
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("codex app-server turn failed");
                        return Err(CourierError::ModelBackendRequest(message.to_string()));
                    }
                    return Ok(reply);
                }
                "error" => {
                    if let Some(message) = params
                        .get("error")
                        .and_then(|value| value.get("message"))
                        .and_then(serde_json::Value::as_str)
                    {
                        return Err(CourierError::ModelBackendRequest(message.to_string()));
                    }
                }
                _ => {}
            }
        }
    }

    fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, CourierError> {
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        self.write_message(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        }))?;

        loop {
            let value = self.read_message()?;
            if is_response_for(&value, request_id) {
                if let Some(message) = value
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(serde_json::Value::as_str)
                {
                    return Err(CourierError::ModelBackendRequest(message.to_string()));
                }
                return Ok(value
                    .get("result")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})));
            }
            if is_server_request(&value) {
                self.respond_to_server_request(&value)?;
            }
        }
    }

    fn notify(&mut self, method: &str, params: serde_json::Value) -> Result<(), CourierError> {
        self.write_message(serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
    }

    fn respond_to_server_request(&mut self, value: &serde_json::Value) -> Result<(), CourierError> {
        let id = value.get("id").cloned().ok_or_else(|| {
            CourierError::ModelBackendResponse("codex request missing id".to_string())
        })?;
        let method = value
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let params = value
            .get("params")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));

        let result = match method {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                serde_json::json!({ "decision": "decline" })
            }
            "item/permissions/requestApproval" => {
                let permissions = params
                    .get("permissions")
                    .and_then(serde_json::Value::as_object)
                    .map(|_| serde_json::json!({}))
                    .unwrap_or_else(|| serde_json::json!({}));
                serde_json::json!({
                    "permissions": permissions,
                    "scope": "turn"
                })
            }
            "execCommandApproval" | "applyPatchApproval" => {
                serde_json::json!({ "decision": "denied" })
            }
            _ => {
                return self.write_message(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": format!("Unsupported Codex app-server request: {method}")
                    }
                }));
            }
        };

        self.write_message(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
    }

    fn write_message(&mut self, value: serde_json::Value) -> Result<(), CourierError> {
        let encoded = serde_json::to_vec(&value)
            .map_err(|error| CourierError::ModelBackendResponse(error.to_string()))?;
        self.stdin
            .write_all(&encoded)
            .map_err(|error| CourierError::ModelBackendRequest(error.to_string()))?;
        self.stdin
            .write_all(b"\n")
            .map_err(|error| CourierError::ModelBackendRequest(error.to_string()))?;
        self.stdin
            .flush()
            .map_err(|error| CourierError::ModelBackendRequest(error.to_string()))
    }

    fn read_message(&mut self) -> Result<serde_json::Value, CourierError> {
        let mut line = String::new();
        let bytes = self
            .stdout
            .read_line(&mut line)
            .map_err(|error| CourierError::ModelBackendRequest(error.to_string()))?;
        if bytes == 0 {
            return Err(CourierError::ModelBackendRequest(
                read_stderr(&mut self.child)
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "codex app-server closed unexpectedly".to_string()),
            ));
        }
        serde_json::from_str(line.trim_end())
            .map_err(|error| CourierError::ModelBackendResponse(error.to_string()))
    }
}

fn read_stderr(child: &mut Child) -> Option<String> {
    let mut stderr = String::new();
    let stderr_pipe = child.stderr.as_mut()?;
    let _ = std::io::Read::read_to_string(stderr_pipe, &mut stderr);
    Some(stderr.trim().to_string())
}

fn extract_thread_id(value: &serde_json::Value) -> Option<String> {
    value
        .get("thread")
        .and_then(|thread| thread.get("id"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| value.get("threadId").and_then(serde_json::Value::as_str))
        .map(ToString::to_string)
}

fn is_server_request(value: &serde_json::Value) -> bool {
    value
        .get("method")
        .and_then(serde_json::Value::as_str)
        .is_some()
        && value.get("id").is_some()
}

fn is_response(value: &serde_json::Value) -> bool {
    value.get("id").is_some() && value.get("method").is_none()
}

fn is_response_for(value: &serde_json::Value, request_id: i64) -> bool {
    value
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .is_some_and(|id| id == request_id)
        && value.get("method").is_none()
}
