use super::*;
use crate::{BuildOptions, build_agentfile};
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

mod a2a;
mod docker;
mod limits;
mod model_backends;
mod model_request;
mod mounts;
mod native_chat;
mod native_runtime;
mod parcel;
mod plugin;
mod state;
mod wasm;

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

#[cfg(unix)]
fn write_executable_script(path: &std::path::Path, content: &str) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

struct CodexBackendTestGuard {
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl Drop for CodexBackendTestGuard {
    fn drop(&mut self) {
        clear_test_codex_binary_override();
    }
}

fn lock_codex_backend_test() -> CodexBackendTestGuard {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    CodexBackendTestGuard {
        _guard: LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .expect("codex backend test lock poisoned"),
    }
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
                Ok((stream, _)) => handle_test_a2a_connection(stream, &server_base_url, &options),
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

fn handle_test_a2a_connection(stream: TcpStream, base_url: &str, options: &TestA2aServerOptions) {
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

fn write_test_http_response(writer: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
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

fn build_test_slow_plugin_courier(dir: &tempfile::TempDir, digest: &str) -> JsonlCourierPlugin {
    let plugin_path = dir.path().join("slow-plugin.sh");
    let script = format!(
        "#!/bin/sh\nwhile IFS= read -r request; do\ncase \"$request\" in\n*'\"kind\":\"capabilities\"'*)\nprintf '%s\\n' '{{\"kind\":\"capabilities\",\"capabilities\":{{\"courier_id\":\"demo-slow-plugin\",\"kind\":\"custom\",\"supports_chat\":true,\"supports_job\":false,\"supports_heartbeat\":false,\"supports_local_tools\":false,\"supports_mounts\":[]}}}}'\n;;\n*'\"kind\":\"open_session\"'*)\nprintf '%s\\n' '{{\"kind\":\"session\",\"session\":{{\"id\":\"plugin-session-slow\",\"parcel_digest\":\"{digest}\",\"entrypoint\":\"chat\",\"turn_count\":0,\"history\":[],\"backend_state\":\"open\"}}}}'\n;;\n*'\"kind\":\"resume_session\"'*)\nprintf '%s\\n' '{{\"kind\":\"session\",\"session\":{{\"id\":\"plugin-session-slow\",\"parcel_digest\":\"{digest}\",\"entrypoint\":\"chat\",\"turn_count\":0,\"history\":[],\"backend_state\":\"open|resumed\"}}}}'\n;;\n*'\"kind\":\"shutdown\"'*)\nprintf '%s\\n' '{{\"kind\":\"ok\"}}'\nexit 0\n;;\n*'\"kind\":\"run\"'*)\nsleep 0.2\nprintf '%s\\n' '{{\"kind\":\"done\",\"session\":{{\"id\":\"plugin-session-slow\",\"parcel_digest\":\"{digest}\",\"entrypoint\":\"chat\",\"turn_count\":1,\"history\":[{{\"role\":\"user\",\"content\":\"hello\"}},{{\"role\":\"assistant\",\"content\":\"slow reply\"}}],\"backend_state\":\"turns:1\"}}}}'\n;;\n*)\nprintf '%s\\n' '{{\"kind\":\"error\",\"error\":{{\"code\":\"bad_request\",\"message\":\"unexpected request\"}}}}'\n;;\nesac\ndone\n"
    );
    fs::write(&plugin_path, &script).unwrap();
    fs::set_permissions(&plugin_path, fs::Permissions::from_mode(0o755)).unwrap();

    JsonlCourierPlugin::new(CourierPluginManifest {
        name: "demo-slow-plugin".to_string(),
        version: "0.1.0".to_string(),
        protocol_version: 1,
        transport: crate::plugins::PluginTransport::Jsonl,
        description: Some("Slow plugin".to_string()),
        exec: crate::plugins::CourierPluginExec {
            command: plugin_path.display().to_string(),
            args: Vec::new(),
        },
        installed_sha256: Some(encode_hex(Sha256::digest(script.as_bytes()))),
    })
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
