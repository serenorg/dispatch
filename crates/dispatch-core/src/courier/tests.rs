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
mod config;
mod docker;
mod model_backends;
mod model_request;
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
fn resolve_prompt_strips_agent_skill_frontmatter_for_skill_directories() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
SKILL file-analyst
ENTRYPOINT chat
",
        &[
            (
                "file-analyst/SKILL.md",
                "---\nname: file-analyst\ndescription: Analyze files\nmetadata:\n  dispatch-manifest: dispatch.toml\n---\nUse the file tools before answering.\n",
            ),
            (
                "file-analyst/dispatch.toml",
                "[[tools]]\nname = \"read_file\"\nscript = \"scripts/read_file.sh\"\n",
            ),
            (
                "file-analyst/scripts/read_file.sh",
                "#!/bin/sh\ncat \"$1\"\n",
            ),
        ],
    );

    let prompt = resolve_prompt_text(&test_image.image).unwrap();
    assert!(prompt.contains("# SKILL"));
    assert!(prompt.contains("Use the file tools before answering."));
    assert!(!prompt.contains("dispatch-manifest"));
    assert!(!prompt.contains("name: file-analyst"));
    assert!(!prompt.contains("description: Analyze files"));
}

#[test]
fn resolve_prompt_keeps_file_based_skill_frontmatter_unchanged() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
SKILL SKILL.md
ENTRYPOINT chat
",
        &[(
            "SKILL.md",
            "---\nname: file-analyst\ndescription: Analyze files\n---\nUse the file tools before answering.\n",
        )],
    );

    let prompt = resolve_prompt_text(&test_image.image).unwrap();
    assert!(prompt.contains("name: file-analyst"));
    assert!(prompt.contains("description: Analyze files"));
    assert!(prompt.contains("Use the file tools before answering."));
}

#[test]
fn collect_skill_allowed_tools_returns_skill_annotations() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
SKILL file-analyst
ENTRYPOINT chat
",
        &[
            (
                "file-analyst/SKILL.md",
                "---\nname: file-analyst\ndescription: Analyze files\nallowed-tools:\n  - Bash\n  - Read\n---\nUse the file tools before answering.\n",
            ),
            (
                "file-analyst/dispatch.toml",
                "[[tools]]\nname = \"read_file\"\nscript = \"scripts/read_file.sh\"\n",
            ),
            (
                "file-analyst/scripts/read_file.sh",
                "#!/bin/sh\ncat \"$1\"\n",
            ),
        ],
    );

    let allowed = collect_skill_allowed_tools(&test_image.image);
    assert_eq!(
        allowed.get("file-analyst"),
        Some(&vec!["Bash".to_string(), "Read".to_string()])
    );
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
TOOL A2A broker URL https://broker.example.com DISCOVERY card AUTH bearer A2A_TOKEN EXPECT_AGENT_NAME remote-broker EXPECT_CARD_SHA256 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa SCHEMA schemas/input.json DESCRIPTION \"Delegate to broker\"
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
    assert_eq!(tools[0].endpoint_mode(), Some(A2aEndpointMode::Card));
    assert_eq!(tools[0].auth_scheme(), Some(A2aAuthScheme::Bearer));
    assert_eq!(tools[0].auth_scheme(), Some(A2aAuthScheme::Bearer));
    assert_eq!(tools[0].auth_username_secret_name(), None);
    assert_eq!(tools[0].auth_password_secret_name(), None);
    assert_eq!(tools[0].auth_header_name(), None);
    assert_eq!(tools[0].expected_agent_name(), Some("remote-broker"));
    assert_eq!(
        tools[0].expected_card_sha256(),
        Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    );
    assert_eq!(tools[0].command(), "dispatch-a2a");
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
    let mut session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
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
    let session = futures::executor::block_on(courier.open_session(&first_image.image)).unwrap();

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
    let courier = WasmCourier::new().unwrap();
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
    let mut session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
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
fn run_local_tool_requires_approval_handler_for_confirm_policy() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
TOOL LOCAL tools/demo.sh AS demo APPROVAL confirm
ENTRYPOINT chat
",
        &[("tools/demo.sh", "printf ok")],
    );

    let error = run_local_tool(&test_image.image, "demo", Some("hello")).unwrap_err();
    assert!(matches!(error, CourierError::ApprovalRequired { ref tool } if tool == "demo"));
}

#[test]
fn native_courier_respects_configured_tool_round_limit() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MODEL fake-model PROVIDER fake
TOOL LOCAL tools/demo.sh AS demo
LIMIT TOOL_ROUNDS 3
ENTRYPOINT chat
",
        &[("tools/demo.sh", "printf ok")],
    );

    let backend = Arc::new(FakeChatBackend::with_replies(
        (0..6)
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
                if backend == "fake" && error.contains("tool call loop reached 3 rounds")
        )
    }));
    assert_eq!(backend.calls.lock().unwrap().len(), 3);
}

#[test]
fn run_local_tool_can_be_denied_by_approval_handler() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
TOOL LOCAL tools/demo.sh AS demo APPROVAL confirm
ENTRYPOINT chat
",
        &[("tools/demo.sh", "printf ok")],
    );

    let error = with_tool_approval_handler(
        |_| Ok(ToolApprovalDecision::Deny),
        || run_local_tool(&test_image.image, "demo", Some("hello")),
    )
    .unwrap_err();
    assert!(matches!(error, CourierError::ApprovalDenied { ref tool } if tool == "demo"));
}

#[test]
fn native_courier_chat_reports_denied_tool_calls() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL LOCAL tools/demo.sh AS demo APPROVAL confirm
ENTRYPOINT chat
",
        &[("tools/demo.sh", "printf 'tool-output'")],
    );
    let backend = Arc::new(FakeChatBackend::with_replies(vec![
        Some(ModelReply {
            text: None,
            backend: "fake".to_string(),
            response_id: None,
            tool_calls: vec![ModelToolCall {
                call_id: "call_1".to_string(),
                name: "demo".to_string(),
                input: "{\"query\":\"ping\"}".to_string(),
                kind: ModelToolKind::Custom,
            }],
        }),
        Some(ModelReply {
            text: Some("final reply".to_string()),
            backend: "fake".to_string(),
            response_id: None,
            tool_calls: Vec::new(),
        }),
    ]));
    let courier = NativeCourier::with_chat_backend(backend);
    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

    let response = with_tool_approval_handler(
        |_| Ok(ToolApprovalDecision::Deny),
        || {
            futures::executor::block_on(courier.run(
                &test_image.image,
                CourierRequest {
                    session,
                    operation: CourierOperation::Chat {
                        input: "try the tool".to_string(),
                    },
                },
            ))
        },
    )
    .unwrap();

    assert!(matches!(
        response.events.get(1),
        Some(CourierEvent::ToolCallFinished { result })
            if result.tool == "demo"
                && result.exit_code == 126
                && result.stderr.contains("denied by APPROVAL confirm")
    ));
    assert!(matches!(
        response.events.iter().rev().nth(1),
        Some(CourierEvent::Message { content, .. }) if content == "final reply"
    ));
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
    let mut session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
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
    let mut session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
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

    let error = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap_err();
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
