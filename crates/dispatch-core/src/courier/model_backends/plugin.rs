use super::*;
#[cfg(test)]
use std::collections::HashMap;
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(crate) struct PluginModelBackend {
    provider: String,
    session_capable: bool,
}

impl PluginModelBackend {
    pub(crate) fn new(provider: impl Into<String>, session_capable: bool) -> Self {
        Self {
            provider: provider.into(),
            session_capable,
        }
    }
}

#[cfg(test)]
static TEST_PLUGIN_BINARY_OVERRIDES: std::sync::OnceLock<
    std::sync::Mutex<HashMap<String, String>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn clear_test_plugin_binary_override(provider: &str) {
    if let Some(slot) = TEST_PLUGIN_BINARY_OVERRIDES.get() {
        slot.lock()
            .expect("plugin binary override lock poisoned")
            .remove(provider);
    }
}

impl ChatModelBackend for PluginModelBackend {
    fn id(&self) -> &str {
        &self.provider
    }

    fn supports_previous_response_id(&self) -> bool {
        self.session_capable
    }

    fn generate(&self, request: &ModelRequest) -> Result<ModelGeneration, CourierError> {
        generate_with_noop_events(self, request)
    }

    fn generate_with_events(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent),
    ) -> Result<ModelGeneration, CourierError> {
        let Some(mut child) = spawn_plugin_process(&self.provider, request)? else {
            return Ok(ModelGeneration::NotConfigured {
                backend: self.provider.clone(),
                reason: format!(
                    "no `dispatch-backend-{}` plugin found; set DISPATCH_BACKEND_{}",
                    self.provider,
                    self.provider.to_ascii_uppercase()
                ),
            });
        };

        collect_plugin_output(
            &mut child,
            &self.provider,
            self.session_capable,
            request,
            on_event,
        )
    }
}

#[derive(Debug)]
struct PluginLaunch {
    program: String,
    args: Vec<String>,
}

fn resolve_plugin_launch(provider: &str) -> Option<PluginLaunch> {
    #[cfg(test)]
    if let Some(slot) = TEST_PLUGIN_BINARY_OVERRIDES.get()
        && let Some(path) = slot
            .lock()
            .expect("plugin binary override lock poisoned")
            .get(provider)
            .cloned()
    {
        return Some(PluginLaunch {
            program: path,
            args: Vec::new(),
        });
    }

    let env_key = format!("DISPATCH_BACKEND_{}", provider.to_ascii_uppercase());
    if let Some(val) = std::env::var(&env_key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        // This env override is intentionally simple: a binary path followed by
        // whitespace-separated args. Paths with embedded spaces should use the
        // bundled lookup or PATH instead of relying on shell-style quoting here.
        let mut parts = val.splitn(2, ' ');
        let program = parts.next()?.to_string();
        let args = parts
            .next()
            .map(|rest| rest.split_whitespace().map(ToString::to_string).collect())
            .unwrap_or_default();
        return Some(PluginLaunch { program, args });
    }

    if let Some(launch) = bundled_plugin_launch(provider) {
        return Some(launch);
    }

    // Fall back to PATH lookup - spawn will return NotFound if not present.
    Some(PluginLaunch {
        program: plugin_binary_name(provider),
        args: Vec::new(),
    })
}

fn plugin_binary_name(provider: &str) -> String {
    let base = format!("dispatch-backend-{provider}");
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base
    }
}

fn bundled_plugin_launch(provider: &str) -> Option<PluginLaunch> {
    let current_exe = std::env::current_exe().ok()?;
    let bin_dir = current_exe.parent()?;
    let exe_name = plugin_binary_name(provider);
    let candidates = [
        bin_dir.join(&exe_name),
        bin_dir.join("libexec").join(&exe_name),
    ];
    candidates
        .iter()
        .find(|path| path.is_file())
        .map(|path| PluginLaunch {
            program: path.display().to_string(),
            args: Vec::new(),
        })
}

fn spawn_plugin_process(
    provider: &str,
    request: &ModelRequest,
) -> Result<Option<Child>, CourierError> {
    let Some(launch) = resolve_plugin_launch(provider) else {
        return Ok(None);
    };

    let serialized = serde_json::to_vec(request).map_err(|error| {
        CourierError::ModelBackendRequest(format!(
            "failed to serialize request for `dispatch-backend-{provider}`: {error}"
        ))
    })?;

    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("DISPATCH_BACKEND_PROTOCOL", "1");

    if let Some(dir) = request.working_directory.as_deref() {
        command.current_dir(dir);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(CourierError::ModelBackendRequest(format!(
                "failed to launch `dispatch-backend-{provider}`: {error}"
            )));
        }
    };

    let Some(mut stdin) = child.stdin.take() else {
        return Err(CourierError::ModelBackendRequest(format!(
            "`dispatch-backend-{provider}` stdin was not captured"
        )));
    };
    stdin.write_all(&serialized).map_err(|error| {
        CourierError::ModelBackendRequest(format!(
            "failed to write request to `dispatch-backend-{provider}`: {error}"
        ))
    })?;
    drop(stdin);

    Ok(Some(child))
}

fn collect_plugin_output(
    child: &mut Child,
    provider: &str,
    session_capable: bool,
    request: &ModelRequest,
    on_event: &mut dyn FnMut(ModelStreamEvent),
) -> Result<ModelGeneration, CourierError> {
    let deadline = plugin_timeout_deadline(request);
    let stderr_capture = spawn_stderr_capture(child, provider)?;
    let stdout = child.stdout.take().ok_or_else(|| {
        CourierError::ModelBackendRequest(format!(
            "`dispatch-backend-{provider}` stdout was not captured"
        ))
    })?;
    let mut stdout = BufReader::new(stdout);

    let mut streamed_text = String::new();
    let mut result_text: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut result_error: Option<String> = None;
    let mut not_configured: Option<String> = None;
    let mut tool_calls: Vec<ModelToolCall> = Vec::new();
    let mut output_error: Option<CourierError> = None;

    while let Some((bytes, line)) = read_line_with_timeout(&mut stdout, child, deadline, provider)?
    {
        if bytes == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => {
                output_error = Some(CourierError::ModelBackendResponse(format!(
                    "failed to parse `dispatch-backend-{provider}` output: {error}"
                )));
                let _ = child.kill();
                break;
            }
        };

        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("text_delta") => {
                if let Some(text) = value.get("content").and_then(serde_json::Value::as_str)
                    && !text.is_empty()
                {
                    streamed_text.push_str(text);
                    on_event(ModelStreamEvent::TextDelta {
                        content: text.to_string(),
                    });
                }
            }
            Some("tool_call") => {
                if let (Some(id), Some(name), Some(input)) = (
                    value.get("id").and_then(serde_json::Value::as_str),
                    value.get("name").and_then(serde_json::Value::as_str),
                    value.get("input").and_then(serde_json::Value::as_str),
                ) {
                    tool_calls.push(ModelToolCall {
                        call_id: id.to_string(),
                        name: name.to_string(),
                        input: input.to_string(),
                        kind: ModelToolKind::Function,
                    });
                }
            }
            Some("session") => {
                session_id = session_id.or_else(|| {
                    value
                        .get("session_id")
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string)
                });
            }
            Some("result") => {
                result_text = value
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .filter(|t| !t.is_empty())
                    .map(ToString::to_string)
                    .or(result_text);
                if let Some(id) = value
                    .get("response_id")
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    session_id = Some(id.to_string());
                }
                if let Some(calls) = value
                    .get("tool_calls")
                    .and_then(serde_json::Value::as_array)
                {
                    for call in calls {
                        if let (Some(id), Some(name), Some(input)) = (
                            call.get("id").and_then(serde_json::Value::as_str),
                            call.get("name").and_then(serde_json::Value::as_str),
                            call.get("input").and_then(serde_json::Value::as_str),
                        ) && !tool_calls.iter().any(|tc| tc.call_id == id)
                        {
                            tool_calls.push(ModelToolCall {
                                call_id: id.to_string(),
                                name: name.to_string(),
                                input: input.to_string(),
                                kind: ModelToolKind::Function,
                            });
                        }
                    }
                }
            }
            Some("not_configured") => {
                not_configured = Some(
                    value
                        .get("reason")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("plugin reported not_configured without a reason")
                        .to_string(),
                );
            }
            Some("error") => {
                result_error = Some(
                    value
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("plugin reported an unknown error")
                        .to_string(),
                );
            }
            _ => {}
        }
    }

    let status = wait_for_exit(child, deadline, provider)?;
    let stderr_text = join_stderr_capture(stderr_capture, provider)?;

    if let Some(error) = output_error {
        return Err(error);
    }

    if let Some(reason) = not_configured {
        return Ok(ModelGeneration::NotConfigured {
            backend: provider.to_string(),
            reason,
        });
    }

    if let Some(message) = result_error {
        return Err(CourierError::ModelBackendRequest(message));
    }

    if !status.success() {
        let detail = stderr_text.trim();
        let suffix = if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        };
        return Err(CourierError::ModelBackendRequest(format!(
            "`dispatch-backend-{provider}` exited with status {status}{suffix}"
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
        backend: provider.to_string(),
        response_id: if session_capable { session_id } else { None },
        tool_calls,
    }))
}

fn plugin_timeout_deadline(request: &ModelRequest) -> Option<Instant> {
    request
        .llm_timeout_ms
        .map(Duration::from_millis)
        .and_then(|timeout| Instant::now().checked_add(timeout))
}

fn spawn_stderr_capture(
    child: &mut Child,
    provider: &str,
) -> Result<JoinHandle<String>, CourierError> {
    let stderr = child.stderr.take().ok_or_else(|| {
        CourierError::ModelBackendRequest(format!(
            "`dispatch-backend-{provider}` stderr was not captured"
        ))
    })?;
    Ok(thread::spawn(move || {
        let mut text = String::new();
        let mut stderr = BufReader::new(stderr);
        let _ = stderr.read_to_string(&mut text);
        text
    }))
}

fn join_stderr_capture(handle: JoinHandle<String>, provider: &str) -> Result<String, CourierError> {
    handle.join().map_err(|_| {
        CourierError::ModelBackendRequest(format!(
            "`dispatch-backend-{provider}` stderr reader panicked"
        ))
    })
}

fn plugin_timeout_error(provider: &str) -> CourierError {
    CourierError::ModelBackendRequest(format!("`dispatch-backend-{provider}` request timed out"))
}

fn remaining_timeout(
    deadline: Option<Instant>,
    provider: &str,
) -> Result<Option<Duration>, CourierError> {
    let Some(deadline) = deadline else {
        return Ok(None);
    };
    deadline
        .checked_duration_since(Instant::now())
        .map(Some)
        .ok_or_else(|| plugin_timeout_error(provider))
}

fn read_line_with_timeout<R: BufRead + Send>(
    reader: &mut R,
    child: &mut Child,
    deadline: Option<Instant>,
    provider: &str,
) -> Result<Option<(usize, String)>, CourierError> {
    let Some(timeout) = remaining_timeout(deadline, provider)? else {
        let mut line = String::new();
        let bytes = reader
            .read_line(&mut line)
            .map_err(|error| CourierError::ModelBackendRequest(error.to_string()))?;
        return Ok(Some((bytes, line)));
    };

    let (sender, receiver) = mpsc::sync_channel(1);
    thread::scope(|scope| {
        scope.spawn(|| {
            let mut line = String::new();
            let result = reader
                .read_line(&mut line)
                .map(|bytes| (bytes, line))
                .map_err(|error| CourierError::ModelBackendRequest(error.to_string()));
            let _ = sender.send(result);
        });

        match receiver.recv_timeout(timeout) {
            Ok(result) => result.map(Some),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = child.kill();
                Err(plugin_timeout_error(provider))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(CourierError::ModelBackendRequest(
                format!("`dispatch-backend-{provider}` output reader disconnected unexpectedly"),
            )),
        }
    })
}

fn wait_for_exit(
    child: &mut Child,
    deadline: Option<Instant>,
    provider: &str,
) -> Result<ExitStatus, CourierError> {
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| CourierError::ModelBackendRequest(error.to_string()))?
        {
            return Ok(status);
        }

        let sleep_for = match remaining_timeout(deadline, provider)? {
            Some(timeout) if timeout.is_zero() => {
                let _ = child.kill();
                return Err(plugin_timeout_error(provider));
            }
            Some(timeout) => timeout.min(Duration::from_millis(25)),
            None => Duration::from_millis(25),
        };
        thread::sleep(sleep_for);
    }
}
