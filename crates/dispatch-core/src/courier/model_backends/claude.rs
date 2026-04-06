use super::*;
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(crate) struct ClaudeCliBackend;

#[cfg(test)]
static TEST_CLAUDE_BINARY_OVERRIDE: std::sync::OnceLock<std::sync::Mutex<Option<String>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
impl ClaudeCliBackend {
    pub(crate) fn with_binary_path_for_tests(path: impl Into<String>) -> Self {
        TEST_CLAUDE_BINARY_OVERRIDE
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .expect("claude binary override lock poisoned")
            .replace(path.into());
        Self
    }
}

#[cfg(test)]
pub(crate) fn clear_test_claude_binary_override() {
    if let Some(slot) = TEST_CLAUDE_BINARY_OVERRIDE.get() {
        *slot.lock().expect("claude binary override lock poisoned") = None;
    }
}

impl ChatModelBackend for ClaudeCliBackend {
    fn id(&self) -> &str {
        "claude"
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
        let binary = claude_binary_path();
        let persist = claude_persistence_enabled(request);
        let deadline = claude_timeout_deadline(request);
        let resumed_session_id = if persist {
            request.previous_response_id.as_deref()
        } else {
            None
        };

        let mut cmd = Command::new(&binary);
        // Avoid `--bare` so the user's existing Claude auth continues to work.
        cmd.arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--input-format")
            .arg("stream-json")
            .arg("--include-partial-messages")
            .arg("--permission-mode")
            .arg("dontAsk")
            // Do not load CLAUDE.md or Claude settings files from disk.
            .arg("--setting-sources")
            .arg("")
            .arg("--model")
            .arg(&request.model)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(effort) = claude_reasoning_effort(request) {
            cmd.arg("--effort").arg(&effort);
        }

        // Append system instructions only on fresh sessions; resumed sessions
        // already have the instructions in their stored context.
        if !request.instructions.trim().is_empty() && resumed_session_id.is_none() {
            cmd.arg("--append-system-prompt").arg(&request.instructions);
        }

        if let Some(session_id) = resumed_session_id {
            cmd.arg("--resume").arg(session_id);
        } else {
            // Prevent the CLI from writing a session file we will never resume.
            if !persist {
                cmd.arg("--no-session-persistence");
            }
        }

        if let Some(cwd) = &request.working_directory {
            cmd.current_dir(cwd);
        }

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ModelGeneration::NotConfigured {
                    backend: self.id().to_string(),
                    reason: format!(
                        "no `{binary}` executable found; set CLAUDE_BINARY or ensure `claude` is on PATH"
                    ),
                });
            }
            Err(error) => {
                return Err(CourierError::ModelBackendRequest(format!(
                    "failed to start `{binary}`: {error}"
                )));
            }
        };

        // Drain stderr in a background thread so the stderr pipe never fills and
        // stalls the child while we are busy reading stdout.
        let stderr_capture = claude_spawn_stderr_capture(&mut child)?;

        // Write conversation messages to stdin in a background thread. This prevents
        // a deadlock where a large prompt fills the stdin pipe buffer before the child
        // starts reading, while simultaneously the child's stdout fills because the
        // parent is blocked on the stdin write.
        let stdin_lines = claude_stdin_messages(request, resumed_session_id.unwrap_or_default());
        let stdin_thread = {
            let mut stdin = child.stdin.take().ok_or_else(|| {
                CourierError::ModelBackendRequest("`claude` stdin was not captured".to_string())
            })?;
            thread::spawn(move || {
                for line in &stdin_lines {
                    if stdin.write_all(line.as_bytes()).is_err() {
                        break;
                    }
                    if stdin.write_all(b"\n").is_err() {
                        break;
                    }
                }
                // stdin drops here, sending EOF to the claude process.
            })
        };

        let stdout = child.stdout.take().ok_or_else(|| {
            CourierError::ModelBackendRequest("`claude` stdout was not captured".to_string())
        })?;
        let mut reader = BufReader::new(stdout);

        let mut streamed_text = String::new();
        let mut result_text: Option<String> = None;
        let mut response_session_id: Option<String> = None;
        let mut response_error: Option<CourierError> = None;

        while let Some((bytes, line)) =
            claude_read_line_with_timeout(&mut reader, &mut child, deadline)?
        {
            if bytes == 0 {
                break;
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };

            match value.get("type").and_then(serde_json::Value::as_str) {
                Some("stream_event") => {
                    if let (Some("text_delta"), Some(text)) = (
                        value
                            .pointer("/event/delta/type")
                            .and_then(serde_json::Value::as_str),
                        value
                            .pointer("/event/delta/text")
                            .and_then(serde_json::Value::as_str),
                    ) && !text.is_empty()
                    {
                        streamed_text.push_str(text);
                        on_event(ModelStreamEvent::TextDelta {
                            content: text.to_string(),
                        });
                    }
                }
                Some("result") => {
                    if value.get("is_error").and_then(serde_json::Value::as_bool) == Some(true) {
                        let message = value
                            .get("result")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("claude returned an error")
                            .to_string();
                        response_error = Some(CourierError::ModelBackendResponse(message));
                        break;
                    }
                    result_text = value
                        .get("result")
                        .and_then(serde_json::Value::as_str)
                        .filter(|t| !t.is_empty())
                        .map(ToString::to_string);
                    response_session_id = value
                        .get("session_id")
                        .and_then(serde_json::Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(ToString::to_string);
                }
                _ => {}
            }
        }

        let status = match claude_wait_for_exit(&mut child, deadline) {
            Ok(status) => status,
            Err(error) => {
                let _ = claude_join_stdin_writer(stdin_thread);
                let _ = claude_join_stderr_capture(stderr_capture);
                return Err(error);
            }
        };
        claude_join_stdin_writer(stdin_thread)?;
        let stderr_text = claude_join_stderr_capture(stderr_capture)?;

        if let Some(error) = response_error {
            return Err(error);
        }

        if !status.success() {
            let detail = stderr_text.trim();
            let suffix = if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            };
            return Err(CourierError::ModelBackendRequest(format!(
                "`claude` exited with status {status}{suffix}"
            )));
        }

        Ok(ModelGeneration::Reply(ModelReply {
            text: result_text.or({
                if streamed_text.is_empty() {
                    None
                } else {
                    Some(streamed_text)
                }
            }),
            backend: self.id().to_string(),
            response_id: if persist { response_session_id } else { None },
            tool_calls: Vec::new(),
        }))
    }
}

fn claude_binary_path() -> String {
    #[cfg(test)]
    if let Some(slot) = TEST_CLAUDE_BINARY_OVERRIDE.get()
        && let Some(path) = slot
            .lock()
            .expect("claude binary override lock poisoned")
            .clone()
    {
        return path;
    }

    std::env::var("CLAUDE_BINARY").unwrap_or_else(|_| "claude".to_string())
}

fn claude_persistence_enabled(request: &ModelRequest) -> bool {
    env_flag_override("DISPATCH_PERSIST_THREAD")
        .or_else(|| claude_model_option_bool(request, "persist-thread"))
        .unwrap_or(true)
}

fn claude_reasoning_effort(request: &ModelRequest) -> Option<String> {
    claude_model_option_value(request, "reasoning-effort")
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_ascii_lowercase())
        .or_else(|| {
            std::env::var("DISPATCH_REASONING_EFFORT")
                .ok()
                .map(|v| v.trim().to_ascii_lowercase())
                .filter(|v| !v.is_empty())
        })
}

/// Build the NDJSON lines to write to Claude's stdin.
///
/// For resumed sessions `session_id` is the prior session ID and only the most
/// recent user message is sent. For fresh sessions `session_id` is empty and
/// the full message list is sent in order.
fn claude_stdin_messages(request: &ModelRequest, session_id: &str) -> Vec<String> {
    if !session_id.is_empty() {
        let content = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role.eq_ignore_ascii_case("user"))
            .map(|m| m.content.as_str())
            .unwrap_or("");
        let msg = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": content },
            "parent_tool_use_id": null,
            "session_id": session_id
        });
        return vec![msg.to_string()];
    }

    request
        .messages
        .iter()
        .map(|m| {
            let role = if m.role.eq_ignore_ascii_case("assistant") {
                "assistant"
            } else {
                "user"
            };
            serde_json::json!({
                "type": "user",
                "message": { "role": role, "content": m.content },
                "parent_tool_use_id": null,
                "session_id": ""
            })
            .to_string()
        })
        .collect()
}

fn claude_spawn_stderr_capture(child: &mut Child) -> Result<JoinHandle<String>, CourierError> {
    let stderr = child.stderr.take().ok_or_else(|| {
        CourierError::ModelBackendRequest("`claude` stderr was not captured".to_string())
    })?;
    Ok(thread::spawn(move || {
        let mut text = String::new();
        let mut stderr = BufReader::new(stderr);
        let _ = stderr.read_to_string(&mut text);
        text
    }))
}

fn claude_join_stdin_writer(handle: JoinHandle<()>) -> Result<(), CourierError> {
    handle.join().map_err(|_| {
        CourierError::ModelBackendRequest("`claude` stdin writer panicked".to_string())
    })
}

fn claude_join_stderr_capture(handle: JoinHandle<String>) -> Result<String, CourierError> {
    handle.join().map_err(|_| {
        CourierError::ModelBackendRequest("`claude` stderr reader panicked".to_string())
    })
}

fn env_flag_override(name: &str) -> Option<bool> {
    std::env::var(name)
        .ok()
        .and_then(|value| parse_claude_flag_bool(&value))
}

fn claude_model_option_bool(request: &ModelRequest, key: &str) -> Option<bool> {
    claude_model_option_value(request, key).and_then(parse_claude_flag_bool)
}

fn claude_model_option_value<'a>(request: &'a ModelRequest, key: &str) -> Option<&'a str> {
    request.model_options.get(key).map(String::as_str)
}

fn parse_claude_flag_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "0" | "false" | "no" | "off" => Some(false),
        "1" | "true" | "yes" | "on" => Some(true),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Process management with optional deadline
// ---------------------------------------------------------------------------

fn claude_timeout_deadline(request: &ModelRequest) -> Option<Instant> {
    request
        .llm_timeout_ms
        .map(Duration::from_millis)
        .and_then(|timeout| Instant::now().checked_add(timeout))
}

fn claude_timeout_error() -> CourierError {
    CourierError::ModelBackendRequest("`claude` request timed out".to_string())
}

fn claude_remaining_timeout(deadline: Option<Instant>) -> Result<Option<Duration>, CourierError> {
    let Some(deadline) = deadline else {
        return Ok(None);
    };
    deadline
        .checked_duration_since(Instant::now())
        .map(Some)
        .ok_or_else(claude_timeout_error)
}

fn claude_read_line_with_timeout<R: BufRead + Send>(
    reader: &mut R,
    child: &mut Child,
    deadline: Option<Instant>,
) -> Result<Option<(usize, String)>, CourierError> {
    let Some(timeout) = claude_remaining_timeout(deadline)? else {
        let mut line = String::new();
        let bytes = reader
            .read_line(&mut line)
            .map_err(|e| CourierError::ModelBackendRequest(e.to_string()))?;
        return Ok(Some((bytes, line)));
    };

    let (sender, receiver) = mpsc::sync_channel(1);
    thread::scope(|scope| {
        scope.spawn(|| {
            let mut line = String::new();
            let result = reader
                .read_line(&mut line)
                .map(|bytes| (bytes, line))
                .map_err(|e| CourierError::ModelBackendRequest(e.to_string()));
            let _ = sender.send(result);
        });

        match receiver.recv_timeout(timeout) {
            Ok(result) => result.map(Some),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = child.kill();
                Err(claude_timeout_error())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(CourierError::ModelBackendRequest(
                "`claude` output reader disconnected unexpectedly".to_string(),
            )),
        }
    })
}

fn claude_wait_for_exit(
    child: &mut Child,
    deadline: Option<Instant>,
) -> Result<ExitStatus, CourierError> {
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| CourierError::ModelBackendRequest(e.to_string()))?
        {
            return Ok(status);
        }

        let sleep_for = match claude_remaining_timeout(deadline)? {
            Some(timeout) if timeout.is_zero() => {
                let _ = child.kill();
                return Err(claude_timeout_error());
            }
            Some(timeout) => timeout.min(Duration::from_millis(25)),
            None => Duration::from_millis(25),
        };
        thread::sleep(sleep_for);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_stdin_messages_sends_only_last_user_message_on_resume() {
        let request = ModelRequest {
            model: "claude-sonnet-4-6".to_string(),
            provider: Some("claude".to_string()),
            model_options: Default::default(),
            llm_timeout_ms: None,
            context_token_limit: None,
            tool_call_limit: None,
            tool_output_limit: None,
            working_directory: None,
            instructions: "Be helpful.".to_string(),
            messages: vec![
                ConversationMessage {
                    role: "user".to_string(),
                    content: "first message".to_string(),
                },
                ConversationMessage {
                    role: "assistant".to_string(),
                    content: "first reply".to_string(),
                },
                ConversationMessage {
                    role: "user".to_string(),
                    content: "follow up".to_string(),
                },
            ],
            tools: Vec::new(),
            pending_tool_calls: Vec::new(),
            tool_outputs: Vec::new(),
            previous_response_id: Some("session-abc".to_string()),
        };

        let lines = claude_stdin_messages(&request, "session-abc");
        assert_eq!(lines.len(), 1);
        let msg: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(msg["type"], "user");
        assert_eq!(msg["message"]["role"], "user");
        assert_eq!(msg["message"]["content"], "follow up");
        assert_eq!(msg["session_id"], "session-abc");
        assert_eq!(msg["parent_tool_use_id"], serde_json::Value::Null);
    }

    #[test]
    fn claude_stdin_messages_sends_all_messages_on_fresh_session() {
        let request = ModelRequest {
            model: "claude-sonnet-4-6".to_string(),
            provider: Some("claude".to_string()),
            model_options: Default::default(),
            llm_timeout_ms: None,
            context_token_limit: None,
            tool_call_limit: None,
            tool_output_limit: None,
            working_directory: None,
            instructions: "Be helpful.".to_string(),
            messages: vec![
                ConversationMessage {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                },
                ConversationMessage {
                    role: "assistant".to_string(),
                    content: "hi there".to_string(),
                },
                ConversationMessage {
                    role: "user".to_string(),
                    content: "follow up".to_string(),
                },
            ],
            tools: Vec::new(),
            pending_tool_calls: Vec::new(),
            tool_outputs: Vec::new(),
            previous_response_id: None,
        };

        let lines = claude_stdin_messages(&request, "");
        assert_eq!(lines.len(), 3);

        let first: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(first["message"]["role"], "user");
        assert_eq!(first["message"]["content"], "hello");
        assert_eq!(first["session_id"], "");

        let second: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(second["message"]["role"], "assistant");
        assert_eq!(second["message"]["content"], "hi there");

        let third: serde_json::Value = serde_json::from_str(&lines[2]).unwrap();
        assert_eq!(third["message"]["role"], "user");
        assert_eq!(third["message"]["content"], "follow up");
    }

    #[test]
    fn claude_stdin_messages_returns_empty_for_no_messages_fresh_session() {
        let request = ModelRequest {
            model: "claude-sonnet-4-6".to_string(),
            provider: Some("claude".to_string()),
            model_options: Default::default(),
            llm_timeout_ms: None,
            context_token_limit: None,
            tool_call_limit: None,
            tool_output_limit: None,
            working_directory: None,
            instructions: "Be helpful.".to_string(),
            messages: Vec::new(),
            tools: Vec::new(),
            pending_tool_calls: Vec::new(),
            tool_outputs: Vec::new(),
            previous_response_id: None,
        };

        let lines = claude_stdin_messages(&request, "");
        assert!(lines.is_empty());
    }
}
