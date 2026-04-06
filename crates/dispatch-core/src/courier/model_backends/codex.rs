use super::*;
use std::{
    fs::File,
    io::{BufReader, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc::Receiver,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[cfg(not(unix))]
use std::process::ChildStdin;

#[cfg(unix)]
use nix::{
    pty::openpty,
    sys::termios::{SetArg, cfmakeraw, tcgetattr, tcsetattr},
    unistd::dup,
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

        let Some(mut process) = CodexProcess::spawn(working_directory, request)? else {
            return Ok(ModelGeneration::NotConfigured {
                backend: self.id().to_string(),
                reason: "missing CODEX_BINARY or `codex` executable".to_string(),
            });
        };
        let deadline = request_timeout_deadline(request);

        let result = (|| {
            process.initialize(deadline)?;
            let reasoning_effort = process.resolve_reasoning_effort(request, deadline)?;
            let thread = process.open_thread(request, deadline)?;
            let prompt = codex_prompt_text(request);
            let turn_id = process.start_turn(
                &thread.thread_id,
                &request.model,
                reasoning_effort.as_deref(),
                &prompt,
                deadline,
            )?;
            let text = process.collect_turn_output(&turn_id, on_event, deadline)?;
            let response_id = if codex_history_persistence_enabled(request) {
                Some(thread.encode())
            } else {
                None
            };

            Ok(ModelGeneration::Reply(ModelReply {
                text: Some(text),
                backend: self.id().to_string(),
                response_id,
                tool_calls: Vec::new(),
            }))
        })();
        process.shutdown(deadline);
        result
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

fn requested_model_reasoning_effort(request: &ModelRequest) -> Option<String> {
    model_option_value(request, "reasoning-effort")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
        .or_else(|| {
            std::env::var("DISPATCH_REASONING_EFFORT")
                .ok()
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty())
        })
}

fn codex_history_persistence_enabled(request: &ModelRequest) -> bool {
    env_flag_override("DISPATCH_PERSIST_THREAD")
        .or_else(|| model_option_bool(request, "persist-thread"))
        .unwrap_or(true)
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
        // The tool note is only needed once, on the first turn, to set Codex's
        // expectations. On subsequent turns the Codex thread already has context
        // and repeating the note adds noise without value.
        let mut parts = Vec::new();
        if let Some(text) = latest_user {
            parts.push(text);
        } else if !request.messages.is_empty() {
            parts.push(render_conversation_transcript(&request.messages));
        } else if !request.instructions.trim().is_empty() {
            parts.push(request.instructions.clone());
        }
        return parts
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
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
    io: CodexIo,
    stdout_receiver: Receiver<LineReadResult>,
    stdout_reader: Option<JoinHandle<()>>,
    next_request_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexThreadState {
    thread_id: String,
    rollout_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexModelReasoningProfile {
    display_name: Option<String>,
    supported_efforts: Vec<String>,
    default_effort: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CodexThreadStateWire {
    thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rollout_path: Option<String>,
}

impl CodexThreadState {
    fn encode(&self) -> String {
        serde_json::to_string(&CodexThreadStateWire {
            thread_id: self.thread_id.clone(),
            rollout_path: self.rollout_path.clone(),
        })
        .unwrap_or_else(|_| self.thread_id.clone())
    }

    fn decode(raw: Option<&str>) -> Option<Self> {
        let raw = raw?.trim();
        if raw.is_empty() {
            return None;
        }
        if let Ok(value) = serde_json::from_str::<CodexThreadStateWire>(raw) {
            return Some(Self {
                thread_id: value.thread_id,
                rollout_path: value.rollout_path,
            });
        }
        Some(Self {
            thread_id: raw.to_string(),
            rollout_path: None,
        })
    }
}

enum CodexIo {
    #[cfg(not(unix))]
    Pipes { stdin: ChildStdin },
    #[cfg(unix)]
    Pty { stdin: File },
}

impl CodexProcess {
    fn spawn(
        working_directory: &str,
        _request: &ModelRequest,
    ) -> Result<Option<Self>, CourierError> {
        let binary = codex_binary_path();
        let mut command = Command::new(&binary);
        command
            .arg("app-server")
            .current_dir(Path::new(working_directory));
        if let Ok(home) = std::env::var("CODEX_HOME") {
            let trimmed = home.trim();
            if !trimmed.is_empty() {
                command.env("CODEX_HOME", trimmed);
            }
        }

        #[cfg(unix)]
        {
            Self::spawn_with_pty(command)
        }

        #[cfg(not(unix))]
        {
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            Self::spawn_with_pipes(command)
        }
    }

    #[cfg(unix)]
    fn spawn_with_pty(mut command: Command) -> Result<Option<Self>, CourierError> {
        let pty = openpty(None, None).map_err(|error| {
            CourierError::ModelBackendRequest(format!(
                "failed to allocate PTY for codex app-server: {error}"
            ))
        })?;
        let mut termios = tcgetattr(&pty.slave).map_err(|error| {
            CourierError::ModelBackendRequest(format!(
                "failed to read PTY settings for codex app-server: {error}"
            ))
        })?;
        cfmakeraw(&mut termios);
        tcsetattr(&pty.slave, SetArg::TCSANOW, &termios).map_err(|error| {
            CourierError::ModelBackendRequest(format!(
                "failed to configure PTY for codex app-server: {error}"
            ))
        })?;

        let stdin = File::from(dup(&pty.master).map_err(|error| {
            CourierError::ModelBackendRequest(format!(
                "failed to clone codex app-server PTY master for input: {error}"
            ))
        })?);
        let stdout = BufReader::new(File::from(dup(&pty.master).map_err(|error| {
            CourierError::ModelBackendRequest(format!(
                "failed to clone codex app-server PTY master for output: {error}"
            ))
        })?));
        let (stdout_receiver, stdout_reader) = spawn_line_reader(stdout);

        command
            .stdin(Stdio::from(File::from(dup(&pty.slave).map_err(
                |error| {
                    CourierError::ModelBackendRequest(format!(
                        "failed to clone codex app-server PTY slave for stdin: {error}"
                    ))
                },
            )?)))
            .stdout(Stdio::from(File::from(dup(&pty.slave).map_err(
                |error| {
                    CourierError::ModelBackendRequest(format!(
                        "failed to clone codex app-server PTY slave for stdout: {error}"
                    ))
                },
            )?)))
            .stderr(Stdio::piped());
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(CourierError::ModelBackendRequest(format!(
                    "failed to start codex app-server: {error}"
                )));
            }
        };

        Ok(Some(Self {
            child,
            io: CodexIo::Pty { stdin },
            stdout_receiver,
            stdout_reader: Some(stdout_reader),
            next_request_id: 1,
        }))
    }

    #[cfg(not(unix))]
    fn spawn_with_pipes(mut command: Command) -> Result<Option<Self>, CourierError> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
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
        let (stdout_receiver, stdout_reader) = spawn_line_reader(BufReader::new(stdout));
        Ok(Some(Self {
            child,
            io: CodexIo::Pipes { stdin },
            stdout_receiver,
            stdout_reader: Some(stdout_reader),
            next_request_id: 1,
        }))
    }

    fn initialize(&mut self, deadline: Option<Instant>) -> Result<(), CourierError> {
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
            deadline,
        )?;
        self.notify("initialized", serde_json::json!({}))?;
        Ok(())
    }

    fn open_thread(
        &mut self,
        request: &ModelRequest,
        deadline: Option<Instant>,
    ) -> Result<CodexThreadState, CourierError> {
        let persistence_enabled = codex_history_persistence_enabled(request);
        if persistence_enabled
            && let Some(state) = CodexThreadState::decode(request.previous_response_id.as_deref())
        {
            let value = self
                .request(
                    "thread/resume",
                    codex_thread_resume_params(&state, request, persistence_enabled),
                    deadline,
                )
                .map_err(|error| {
                    CourierError::ModelBackendRequest(format!(
                        "failed to resume codex thread `{}`: {error}",
                        state.thread_id
                    ))
                })?;
            return extract_thread_state(&value).ok_or_else(|| {
                CourierError::ModelBackendResponse(format!(
                    "codex thread/resume response for `{}` did not include a thread id",
                    state.thread_id
                ))
            });
        }

        let value = self.request(
            "thread/start",
            codex_thread_start_params(request, persistence_enabled),
            deadline,
        )?;
        extract_thread_state(&value).ok_or_else(|| {
            CourierError::ModelBackendResponse(
                "codex thread/start response did not include a thread id".to_string(),
            )
        })
    }

    fn resolve_reasoning_effort(
        &mut self,
        request: &ModelRequest,
        deadline: Option<Instant>,
    ) -> Result<Option<String>, CourierError> {
        let Some(effort) = requested_model_reasoning_effort(request) else {
            return Ok(None);
        };
        let Some(profile) = self.find_model_reasoning_profile(&request.model, deadline)? else {
            return Err(CourierError::ModelBackendRequest(format!(
                "codex model/list did not include model `{}` needed to validate reasoning effort `{}`",
                request.model, effort
            )));
        };
        if profile
            .supported_efforts
            .iter()
            .any(|value| value == &effort)
        {
            return Ok(Some(effort));
        }
        let supported = if profile.supported_efforts.is_empty() {
            "none".to_string()
        } else {
            profile.supported_efforts.join(", ")
        };
        let default_effort = profile.default_effort.as_deref().unwrap_or("none");
        Err(CourierError::ModelBackendRequest(format!(
            "codex model `{}` does not support reasoning effort `{}`; supported values: {}; default: {}",
            profile.display_name.as_deref().unwrap_or(&request.model),
            effort,
            supported,
            default_effort
        )))
    }

    fn find_model_reasoning_profile(
        &mut self,
        requested_model: &str,
        deadline: Option<Instant>,
    ) -> Result<Option<CodexModelReasoningProfile>, CourierError> {
        let mut cursor: Option<String> = None;
        for _ in 0..20 {
            let mut params = serde_json::json!({
                "includeHidden": true,
            });
            if let Some(value) = &cursor {
                params["cursor"] = serde_json::Value::String(value.clone());
            }
            let value = self.request("model/list", params, deadline)?;
            if let Some(found) = value
                .get("data")
                .and_then(serde_json::Value::as_array)
                .and_then(|models| {
                    models
                        .iter()
                        .find(|model| codex_model_matches(model, requested_model))
                        .map(parse_codex_model_reasoning_profile)
                })
                .transpose()?
            {
                return Ok(Some(found));
            }
            cursor = value
                .get("nextCursor")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string);
            if cursor.is_none() {
                return Ok(None);
            }
        }
        Err(CourierError::ModelBackendResponse(
            "codex model/list pagination exceeded 20 pages".to_string(),
        ))
    }

    fn start_turn(
        &mut self,
        thread_id: &str,
        model: &str,
        reasoning_effort: Option<&str>,
        prompt: &str,
        deadline: Option<Instant>,
    ) -> Result<String, CourierError> {
        let mut params = serde_json::json!({
            "threadId": thread_id,
            "input": [{ "type": "text", "text": prompt }],
            "model": model,
        });
        if let Some(value) = reasoning_effort {
            params["effort"] = serde_json::Value::String(value.to_string());
        }
        let value = self.request("turn/start", params, deadline)?;
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
        deadline: Option<Instant>,
    ) -> Result<String, CourierError> {
        let mut reply = String::new();
        loop {
            let value = self.read_message(deadline)?;
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
        deadline: Option<Instant>,
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
            let value = self.read_message(deadline)?;
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
        let result = match method {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                serde_json::json!({ "decision": "decline" })
            }
            "item/permissions/requestApproval" => {
                // Grant zero permissions for the current turn.
                // The protocol accepts {"permissions": {}, "scope": "turn"} as
                // the deny/no-grant response.
                serde_json::json!({
                    "permissions": {},
                    "scope": "turn"
                })
            }
            "execCommandApproval" | "applyPatchApproval" => {
                serde_json::json!({ "decision": "denied" })
            }
            _ => {
                let message = format!("Unsupported Codex app-server request: {method}");
                return self.write_message(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": message
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
        match &mut self.io {
            #[cfg(not(unix))]
            CodexIo::Pipes { stdin, .. } => {
                stdin
                    .write_all(&encoded)
                    .map_err(|error| CourierError::ModelBackendRequest(error.to_string()))?;
                stdin
                    .write_all(b"\n")
                    .map_err(|error| CourierError::ModelBackendRequest(error.to_string()))?;
                stdin
                    .flush()
                    .map_err(|error| CourierError::ModelBackendRequest(error.to_string()))
            }
            #[cfg(unix)]
            CodexIo::Pty { stdin, .. } => {
                stdin
                    .write_all(&encoded)
                    .map_err(|error| CourierError::ModelBackendRequest(error.to_string()))?;
                stdin
                    .write_all(b"\n")
                    .map_err(|error| CourierError::ModelBackendRequest(error.to_string()))?;
                stdin
                    .flush()
                    .map_err(|error| CourierError::ModelBackendRequest(error.to_string()))
            }
        }
    }

    fn read_message(
        &mut self,
        deadline: Option<Instant>,
    ) -> Result<serde_json::Value, CourierError> {
        let bytes_and_line =
            read_line_with_timeout(&self.stdout_receiver, &mut self.child, deadline)?;
        let Some((bytes, line)) = bytes_and_line else {
            return Err(CourierError::ModelBackendRequest(
                read_stderr(&mut self.child)
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "codex app-server closed unexpectedly".to_string()),
            ));
        };
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

    fn shutdown(self, deadline: Option<Instant>) {
        let CodexProcess {
            mut child,
            io,
            stdout_reader,
            ..
        } = self;
        drop(io);

        let timeout = remaining_timeout(deadline)
            .ok()
            .flatten()
            .unwrap_or_else(|| Duration::from_millis(250))
            .min(Duration::from_secs(2));
        let wait_deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < wait_deadline => {
                    thread::sleep(Duration::from_millis(10))
                }
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
        if let Some(stdout_reader) = stdout_reader {
            let _ = join_line_reader(
                stdout_reader,
                CourierError::ModelBackendRequest(
                    "codex app-server stdout reader panicked".to_string(),
                ),
            );
        }
    }
}

fn request_timeout_deadline(request: &ModelRequest) -> Option<Instant> {
    request
        .llm_timeout_ms
        .map(Duration::from_millis)
        .and_then(|timeout| Instant::now().checked_add(timeout))
}

fn remaining_timeout(deadline: Option<Instant>) -> Result<Option<Duration>, CourierError> {
    let Some(deadline) = deadline else {
        return Ok(None);
    };
    deadline
        .checked_duration_since(Instant::now())
        .map(Some)
        .ok_or_else(codex_timeout_error)
}

fn codex_timeout_error() -> CourierError {
    CourierError::ModelBackendRequest("codex app-server request timed out".to_string())
}

fn read_line_with_timeout(
    receiver: &Receiver<LineReadResult>,
    child: &mut Child,
    deadline: Option<Instant>,
) -> Result<Option<(usize, String)>, CourierError> {
    recv_line_with_timeout(
        receiver,
        child,
        remaining_timeout(deadline)?,
        codex_timeout_error(),
        CourierError::ModelBackendRequest(
            "codex app-server reader disconnected unexpectedly".to_string(),
        ),
    )
}

fn read_stderr(child: &mut Child) -> Option<String> {
    // Wait for the process to exit before reading stderr.
    // Without this, read_to_string on a pipe can block indefinitely if the
    // child process has closed stdout but not yet closed stderr.
    let _ = child.wait();
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

fn extract_thread_state(value: &serde_json::Value) -> Option<CodexThreadState> {
    let thread_id = extract_thread_id(value)?;
    let rollout_path = value
        .get("thread")
        .and_then(|thread| thread.get("path"))
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string);
    Some(CodexThreadState {
        thread_id,
        rollout_path,
    })
}

fn codex_model_matches(model: &serde_json::Value, requested_model: &str) -> bool {
    model
        .get("id")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| value == requested_model)
        || model
            .get("model")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| value == requested_model)
}

fn parse_codex_model_reasoning_profile(
    model: &serde_json::Value,
) -> Result<CodexModelReasoningProfile, CourierError> {
    let supported_efforts = model
        .get("supportedReasoningEfforts")
        .and_then(serde_json::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .map(|entry| {
                    entry
                        .get("reasoningEffort")
                        .and_then(serde_json::Value::as_str)
                        .map(|value| value.to_ascii_lowercase())
                        .ok_or_else(|| {
                            CourierError::ModelBackendResponse(
                                "codex model/list reasoning effort entry omitted reasoningEffort"
                                    .to_string(),
                            )
                        })
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    let default_effort = model
        .get("defaultReasoningEffort")
        .and_then(serde_json::Value::as_str)
        .map(|value| value.to_ascii_lowercase());
    let display_name = model
        .get("displayName")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string);
    Ok(CodexModelReasoningProfile {
        display_name,
        supported_efforts,
        default_effort,
    })
}

fn codex_thread_start_params(
    request: &ModelRequest,
    persistence_enabled: bool,
) -> serde_json::Value {
    serde_json::json!({
        "cwd": request.working_directory,
        "approvalPolicy": "on-request",
        "sandbox": "workspace-write",
        "experimentalRawEvents": false,
        "persistExtendedHistory": persistence_enabled,
        "model": request.model,
    })
}

fn codex_thread_resume_params(
    state: &CodexThreadState,
    request: &ModelRequest,
    persistence_enabled: bool,
) -> serde_json::Value {
    // Prefer resuming by rollout path when available - it is more reliable than
    // thread ID alone. ThreadResumeParams precedence: history > path > threadId.
    let mut params = serde_json::json!({
        "threadId": state.thread_id,
        "cwd": request.working_directory,
        "approvalPolicy": "on-request",
        "sandbox": "workspace-write",
        "persistExtendedHistory": persistence_enabled,
        "model": request.model,
    });
    if let Some(path) = &state.rollout_path {
        params["path"] = serde_json::Value::String(path.clone());
    }
    params
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
