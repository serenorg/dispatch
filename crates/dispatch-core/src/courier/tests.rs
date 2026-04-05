use super::*;
use crate::{BuildOptions, build_agentfile};
use rusqlite::Connection;
use serde_json::Value;
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
#[cfg(unix)]
fn jsonl_plugin_courier_supports_capabilities_inspect_and_run() {
    let test_image = build_test_image(
        "\
FROM dispatch/custom:latest
ENTRYPOINT chat
",
        &[],
    );
    let (courier, _) =
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
    let (courier, _) =
        build_test_plugin_courier(&test_image._dir, &test_image.image.config.digest, true);

    let error = futures::executor::block_on(courier.inspect(&test_image.image)).unwrap_err();
    assert!(matches!(
        error,
        CourierError::PluginProtocol { courier, message }
            if courier == "demo-plugin" && message.contains("bad_request") && message.contains("plugin rejected request")
    ));
}

#[test]
#[cfg(unix)]
fn jsonl_plugin_reuses_persistent_process_across_turns() {
    let test_image = build_test_image(
        "\
FROM dispatch/custom:latest
ENTRYPOINT chat
",
        &[],
    );
    let (courier, _plugin_path, starts_path) =
        build_test_counting_plugin_courier(&test_image._dir, &test_image.image.config.digest);

    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
    let starts_after_open = fs::read_to_string(&starts_path).unwrap();
    assert_eq!(starts_after_open.lines().count(), 2);

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
    let starts_after_first = fs::read_to_string(&starts_path).unwrap();
    assert_eq!(starts_after_first.lines().count(), 2);
    assert_eq!(first.session.turn_count, 1);
    assert!(matches!(
        first.events.first(),
        Some(CourierEvent::Message { role, content })
            if role == "assistant" && content == "from plugin turn 1"
    ));

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
    let starts_after_second = fs::read_to_string(&starts_path).unwrap();
    assert_eq!(starts_after_second.lines().count(), 2);
    assert_eq!(second.session.turn_count, 2);
    assert!(matches!(
        second.events.first(),
        Some(CourierEvent::Message { role, content })
            if role == "assistant" && content == "from plugin turn 2"
    ));
}

#[test]
#[cfg(unix)]
fn jsonl_plugin_resumes_persistent_session_after_new_host_process() {
    let test_image = build_test_image(
        "\
FROM dispatch/custom:latest
ENTRYPOINT chat
",
        &[],
    );
    let (courier, _plugin_path, starts_path) =
        build_test_counting_plugin_courier(&test_image._dir, &test_image.image.config.digest);

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
    assert_eq!(first.session.turn_count, 1);

    let manifest = courier.manifest.clone();
    drop(courier);

    let starts_after_restart = fs::read_to_string(&starts_path).unwrap();
    assert_eq!(starts_after_restart.lines().count(), 2);

    let resumed_courier = JsonlCourierPlugin::new(manifest);
    let second = futures::executor::block_on(resumed_courier.run(
        &test_image.image,
        CourierRequest {
            session: first.session,
            operation: CourierOperation::Chat {
                input: "second".to_string(),
            },
        },
    ))
    .unwrap();

    let starts_after_resume = fs::read_to_string(&starts_path).unwrap();
    assert_eq!(starts_after_resume.lines().count(), 3);
    assert_eq!(second.session.turn_count, 2);
    assert!(matches!(
        second.events.first(),
        Some(CourierEvent::Message { role, content })
            if role == "assistant" && content == "from plugin turn 2"
    ));
}

#[test]
#[cfg(unix)]
fn jsonl_plugin_sends_shutdown_to_persistent_process_on_drop() {
    let test_image = build_test_image(
        "\
FROM dispatch/custom:latest
ENTRYPOINT chat
",
        &[],
    );
    let dir = tempdir().unwrap();
    let (courier, shutdowns_path) =
        build_test_shutdown_plugin_courier(&dir, &test_image.image.config.digest);

    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
    let _ = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "hello".to_string(),
            },
        },
    ))
    .unwrap();

    drop(courier);

    let shutdowns = fs::read_to_string(shutdowns_path).unwrap();
    assert!(shutdowns.contains("shutdown"));
}

#[test]
#[cfg(unix)]
fn jsonl_plugin_run_timeout_uses_remaining_run_budget() {
    let test_image = build_test_image(
        "\
FROM dispatch/custom:latest
TIMEOUT RUN 50ms
ENTRYPOINT chat
",
        &[],
    );
    let dir = tempdir().unwrap();
    let courier = build_test_slow_plugin_courier(&dir, &test_image.image.config.digest);

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
        CourierError::RunTimedOut { ref timeout, .. } if timeout == "50ms"
    ));
}

#[test]
#[cfg(unix)]
fn jsonl_plugin_courier_detects_executable_drift() {
    let test_image = build_test_image(
        "\
FROM dispatch/custom:latest
ENTRYPOINT chat
",
        &[],
    );
    let (courier, plugin_path) =
        build_test_plugin_courier(&test_image._dir, &test_image.image.config.digest, false);
    fs::write(&plugin_path, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&plugin_path, fs::Permissions::from_mode(0o755)).unwrap();

    let error = futures::executor::block_on(courier.capabilities()).unwrap_err();
    assert!(matches!(
        error,
        CourierError::PluginExecutableChanged { courier, .. } if courier == "demo-plugin"
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
fn native_courier_executes_a2a_tools_via_host_transport() {
    let server = start_test_a2a_server();
    let test_image = build_test_image(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {} DESCRIPTION \"Delegate to broker\"
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let result = run_local_tool(&test_image.image, "broker", Some("hello remote")).unwrap();
    assert_eq!(result.tool, "broker");
    assert_eq!(result.command, "dispatch-a2a");
    assert!(result.stdout.contains("echo:hello remote"));
}

#[test]
fn native_courier_executes_a2a_tools_with_json_payloads() {
    let server = start_test_a2a_server();
    let test_image = build_test_image(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {} SCHEMA schemas/input.json
ENTRYPOINT job
",
            server.base_url
        ),
        &[(
            "schemas/input.json",
            "{\n  \"type\": \"object\",\n  \"properties\": {\n    \"query\": { \"type\": \"string\" }\n  },\n  \"required\": [\"query\"]\n}\n",
        )],
    );

    let result =
        run_local_tool(&test_image.image, "broker", Some("{\"query\":\"weather\"}")).unwrap();
    let output: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(
        output.pointer("/query").and_then(serde_json::Value::as_str),
        Some("weather")
    );
}

#[test]
fn native_courier_rejects_non_loopback_cleartext_a2a_urls() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
TOOL A2A broker URL http://example.com DISCOVERY direct
ENTRYPOINT job
",
        &[],
    );

    let error = run_local_tool(&test_image.image, "broker", Some("hello")).unwrap_err();
    assert!(matches!(
        error,
        CourierError::A2aToolRequest { ref message, .. }
            if message.contains("must use https unless it targets a loopback host")
    ));
}

#[test]
fn native_courier_rejects_a2a_urls_with_embedded_credentials() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
TOOL A2A broker URL http://user:pass@127.0.0.1:7777 DISCOVERY direct
ENTRYPOINT job
",
        &[],
    );

    let error = run_local_tool(&test_image.image, "broker", Some("hello")).unwrap_err();
    assert!(matches!(
        error,
        CourierError::A2aToolRequest { ref message, .. }
            if message.contains("must not embed credentials")
    ));
}

#[test]
fn native_courier_executes_a2a_tools_with_bearer_auth() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        expected_auth: Some("Bearer topsecret".to_string()),
        ..Default::default()
    });
    let test_image = build_test_image(
        &format!(
            "\
FROM dispatch/native:latest
SECRET A2A_TOKEN
TOOL A2A broker URL {} AUTH bearer A2A_TOKEN
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let result = run_local_tool_with_env(&test_image.image, "broker", Some("hello"), |name| {
        (name == "A2A_TOKEN").then(|| "topsecret".to_string())
    })
    .unwrap();
    assert!(result.stdout.contains("echo:hello"));
}

#[test]
fn native_courier_executes_a2a_tools_with_header_auth() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        expected_auth: Some("X-Api-Key: topsecret".to_string()),
        ..Default::default()
    });
    let test_image = build_test_image(
        &format!(
            "\
FROM dispatch/native:latest
SECRET API_KEY
TOOL A2A broker URL {} AUTH header X-Api-Key API_KEY
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let result = run_local_tool_with_env(&test_image.image, "broker", Some("hello"), |name| {
        (name == "API_KEY").then(|| "topsecret".to_string())
    })
    .unwrap();
    assert!(result.stdout.contains("echo:hello"));
}

#[test]
fn native_courier_executes_a2a_tools_with_basic_auth() {
    let encoded = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode("demo-user:topsecret")
    };
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        expected_auth: Some(format!("Basic {encoded}")),
        ..Default::default()
    });
    let test_image = build_test_image(
        &format!(
            "\
FROM dispatch/native:latest
SECRET A2A_USER
SECRET A2A_PASSWORD
TOOL A2A broker URL {} AUTH basic A2A_USER A2A_PASSWORD
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let result =
        run_local_tool_with_env(
            &test_image.image,
            "broker",
            Some("hello"),
            |name| match name {
                "A2A_USER" => Some("demo-user".to_string()),
                "A2A_PASSWORD" => Some("topsecret".to_string()),
                _ => None,
            },
        )
        .unwrap();
    assert!(result.stdout.contains("echo:hello"));
}

#[test]
fn native_courier_rejects_a2a_call_when_auth_secret_is_missing() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        expected_auth: Some("Bearer topsecret".to_string()),
        ..Default::default()
    });
    let tool = LocalToolSpec {
        alias: "broker".to_string(),
        description: None,
        input_schema_packaged_path: None,
        input_schema_sha256: None,
        approval: None,
        risk: None,
        skill_source: None,
        target: LocalToolTarget::A2a {
            endpoint_url: server.base_url.clone(),
            endpoint_mode: None,
            auth: Some(crate::manifest::A2aAuthConfig::Bearer {
                secret_name: "A2A_TOKEN".to_string(),
            }),
            expected_agent_name: None,
            expected_card_sha256: None,
        },
    };

    let error = execute_a2a_tool_with_env(&tool, Some("hello"), |_| None, None).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("configured A2A auth secret `A2A_TOKEN` is not available")
    );
}

#[test]
fn native_courier_rejects_a2a_agent_name_mismatch() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        agent_name: Some("actual-agent".to_string()),
        ..Default::default()
    });
    let test_image = build_test_image(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {} EXPECT_AGENT_NAME expected-agent
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool(&test_image.image, "broker", Some("hello")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("agent card name mismatch: expected `expected-agent`, got `actual-agent`")
    );
}

#[test]
fn native_courier_rejects_a2a_agent_name_requirement_when_card_has_no_name() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        agent_name: None,
        ..Default::default()
    });
    let test_image = build_test_image(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {} EXPECT_AGENT_NAME expected-agent
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool(&test_image.image, "broker", Some("hello")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("agent card did not include `name`, but `expected-agent` was required")
    );
}

#[test]
fn native_courier_rejects_a2a_card_digest_mismatch() {
    let server = start_test_a2a_server();
    let test_image = build_test_image(
            &format!(
                "\
FROM dispatch/native:latest
TOOL A2A broker URL {} EXPECT_CARD_SHA256 ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff
ENTRYPOINT job
",
                server.base_url
            ),
            &[],
        );

    let error = run_local_tool(&test_image.image, "broker", Some("hello")).unwrap_err();
    assert!(error
            .to_string()
            .contains("agent card digest mismatch: expected `ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff`"));
}

#[test]
fn native_courier_accepts_matching_a2a_card_digest() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        agent_name: Some("demo-a2a".to_string()),
        ..Default::default()
    });
    let expected_card_sha256 = encode_hex(Sha256::digest(
        serde_json::to_vec(&serde_json::json!({
            "name": "demo-a2a",
            "url": format!("{}/a2a", server.base_url)
        }))
        .unwrap(),
    ));
    let test_image = build_test_image(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {} EXPECT_CARD_SHA256 {}
ENTRYPOINT job
",
            server.base_url, expected_card_sha256
        ),
        &[],
    );

    let result = run_local_tool(&test_image.image, "broker", Some("hello")).unwrap();
    assert!(result.stdout.contains("echo:hello"));
}

#[test]
fn native_courier_rejects_a2a_card_origin_pivot() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        card_url: Some("https://evil.example.com/a2a".to_string()),
        ..Default::default()
    });
    let test_image = build_test_image(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {}
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool(&test_image.image, "broker", Some("hello")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("discovered agent card URL must stay on the declared origin")
    );
}

#[test]
fn native_courier_enforces_tool_timeout_for_a2a_tools() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        response_delay: Duration::from_millis(200),
        ..Default::default()
    });
    let test_image = build_test_image(
        &format!(
            "\
FROM dispatch/native:latest
TIMEOUT TOOL 50ms
TOOL A2A broker URL {}
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool(&test_image.image, "broker", Some("hello")).unwrap_err();
    assert!(matches!(
        error,
        CourierError::ToolTimedOut { ref tool, ref timeout }
            if tool == "broker" && timeout == "TOOL"
    ));
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
fn native_courier_requires_card_discovery_when_configured() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        publish_card: false,
        ..Default::default()
    });
    let test_image = build_test_image(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {} DISCOVERY card
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool(&test_image.image, "broker", Some("hello")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("agent card discovery failed for required `DISCOVERY card` mode")
    );
}

#[test]
fn native_courier_polls_non_completed_a2a_tasks_until_completion() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        task_state: "working".to_string(),
        task_status_message: "queued for async execution".to_string(),
        task_get_state: Some("completed".to_string()),
        task_get_status_message: Some("done".to_string()),
        ..Default::default()
    });
    let test_image = build_test_image(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {}
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let result = run_local_tool(&test_image.image, "broker", Some("hello")).unwrap();
    assert!(result.stdout.contains("echo:hello"));
}

#[test]
fn native_courier_times_out_polling_non_completed_a2a_tasks() {
    let cancel_count = Arc::new(AtomicU64::new(0));
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        task_state: "working".to_string(),
        task_status_message: "queued for async execution".to_string(),
        task_get_state: Some("working".to_string()),
        task_get_status_message: Some("still running".to_string()),
        cancel_count: Some(cancel_count.clone()),
        ..Default::default()
    });
    let test_image = build_test_image(
        &format!(
            "\
FROM dispatch/native:latest
TIMEOUT TOOL 75ms
TOOL A2A broker URL {}
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool(&test_image.image, "broker", Some("hello")).unwrap_err();
    assert!(matches!(
        error,
        CourierError::ToolTimedOut { ref tool, ref timeout }
            if tool == "broker" && timeout == "TOOL"
    ));
    assert_eq!(cancel_count.load(Ordering::Relaxed), 1);
}

#[test]
fn native_courier_surfaces_a2a_json_rpc_errors() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        rpc_error: Some((-32001, "remote agent unavailable".to_string())),
        ..Default::default()
    });
    let test_image = build_test_image(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {}
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool(&test_image.image, "broker", Some("hello")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("JSON-RPC error -32001: remote agent unavailable")
    );
}

#[test]
fn native_courier_rejects_a2a_url_outside_operator_allowlist() {
    let server = start_test_a2a_server();
    let test_image = build_test_image(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {}
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool_with_env(&test_image.image, "broker", Some("hello"), |name| {
        (name == "DISPATCH_A2A_ALLOWED_ORIGINS")
            .then(|| "https://agents.example.com,broker.internal".to_string())
    })
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("is not allowed by DISPATCH_A2A_ALLOWED_ORIGINS")
    );
}

#[test]
fn native_courier_allows_a2a_url_with_matching_operator_allowlist_origin() {
    let server = start_test_a2a_server();
    let parsed = url::Url::parse(&server.base_url).unwrap();
    let origin = a2a::a2a_origin(&parsed).unwrap();
    let test_image = build_test_image(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {}
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let result = run_local_tool_with_env(&test_image.image, "broker", Some("hello"), |name| {
        (name == "DISPATCH_A2A_ALLOWED_ORIGINS").then(|| origin.clone())
    })
    .unwrap();
    assert!(result.stdout.contains("echo:hello"));
}

#[test]
fn native_courier_rejects_a2a_url_when_operator_allowlist_is_explicitly_empty() {
    let server = start_test_a2a_server();
    let test_image = build_test_image(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {}
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool_with_env(&test_image.image, "broker", Some("hello"), |name| {
        (name == "DISPATCH_A2A_ALLOWED_ORIGINS").then(String::new)
    })
    .unwrap_err();
    assert!(error.to_string().contains("resolved to an empty allowlist"));
}

#[test]
fn native_courier_rejects_a2a_url_outside_operator_trust_policy() {
    let server = start_test_a2a_server();
    let dir = tempdir().unwrap();
    let policy_path = dir.path().join("a2a-trust.toml");
    fs::write(
        &policy_path,
        "[[rules]]\norigin_prefix = \"https://agents.example.com\"\n",
    )
    .unwrap();
    let test_image = build_test_image(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {}
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool_with_env(&test_image.image, "broker", Some("hello"), |name| {
        (name == "DISPATCH_A2A_TRUST_POLICY").then(|| policy_path.display().to_string())
    })
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("is not allowed by DISPATCH_A2A_TRUST_POLICY")
    );
}

#[test]
fn native_courier_enforces_operator_a2a_trust_policy_identity() {
    let server = start_test_a2a_server_with_options(TestA2aServerOptions {
        agent_name: Some("planner-agent".to_string()),
        ..Default::default()
    });
    let dir = tempdir().unwrap();
    let policy_path = dir.path().join("a2a-trust.toml");
    let card_body = serde_json::to_vec(&serde_json::json!({
        "name": "planner-agent",
        "url": format!("{}/a2a", server.base_url),
    }))
    .unwrap();
    let card_sha = encode_hex(Sha256::digest(card_body));
    fs::write(
            &policy_path,
            format!(
                "[[rules]]\nhostname = \"127.0.0.1\"\nexpected_agent_name = \"planner-agent\"\nexpected_card_sha256 = \"{}\"\n",
                card_sha
            ),
        )
        .unwrap();
    let test_image = build_test_image(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {}
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let result = run_local_tool_with_env(&test_image.image, "broker", Some("hello"), |name| {
        (name == "DISPATCH_A2A_TRUST_POLICY").then(|| policy_path.display().to_string())
    })
    .unwrap();
    assert!(result.stdout.contains("echo:hello"));
}

#[test]
fn native_courier_rejects_direct_a2a_with_operator_identity_requirement() {
    let server = start_test_a2a_server();
    let dir = tempdir().unwrap();
    let policy_path = dir.path().join("a2a-trust.toml");
    fs::write(
        &policy_path,
        "[[rules]]\nhostname = \"127.0.0.1\"\nexpected_agent_name = \"planner-agent\"\n",
    )
    .unwrap();
    let test_image = build_test_image(
        &format!(
            "\
FROM dispatch/native:latest
TOOL A2A broker URL {} DISCOVERY direct
ENTRYPOINT job
",
            server.base_url
        ),
        &[],
    );

    let error = run_local_tool_with_env(&test_image.image, "broker", Some("hello"), |name| {
        (name == "DISPATCH_A2A_TRUST_POLICY").then(|| policy_path.display().to_string())
    })
    .unwrap_err();
    assert!(error.to_string().contains("DISCOVERY direct"));
}

#[test]
fn a2a_operator_policy_overrides_supply_allowed_origins_to_process_lookup() {
    let result = with_a2a_operator_policy_overrides(
        A2aOperatorPolicyOverrides {
            allowed_origins: Some("https://planner.example.com".to_string()),
            trust_policy: None,
        },
        || process_env_lookup("DISPATCH_A2A_ALLOWED_ORIGINS"),
    );
    assert_eq!(result.as_deref(), Some("https://planner.example.com"));
    assert!(a2a_operator_policy_override_value("DISPATCH_A2A_ALLOWED_ORIGINS").is_none());
}

#[test]
fn a2a_operator_policy_overrides_supply_trust_policy_to_process_lookup() {
    let result = with_a2a_operator_policy_overrides(
        A2aOperatorPolicyOverrides {
            allowed_origins: None,
            trust_policy: Some("/tmp/dispatch-a2a-policy.toml".to_string()),
        },
        || process_env_lookup("DISPATCH_A2A_TRUST_POLICY"),
    );
    assert_eq!(result.as_deref(), Some("/tmp/dispatch-a2a-policy.toml"));
    assert!(a2a_operator_policy_override_value("DISPATCH_A2A_TRUST_POLICY").is_none());
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
    let courier = WasmCourier::new().unwrap();

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
fn wasm_courier_supports_direct_tool_invocation() {
    let test_image = build_test_image(
        "\
FROM dispatch/wasm:latest
COMPONENT components/assistant.wat
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT chat
",
        &[
            ("components/assistant.wat", "(component)"),
            ("tools/demo.sh", "printf 'direct-tool-ok'"),
        ],
    );
    let courier = WasmCourier::new().unwrap();
    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session,
            operation: CourierOperation::InvokeTool {
                invocation: ToolInvocation {
                    name: "demo".to_string(),
                    input: Some("hello".to_string()),
                },
            },
        },
    ))
    .unwrap();

    assert_eq!(response.session.turn_count, 1);
    assert!(matches!(
        response.events.as_slice(),
        [
            CourierEvent::ToolCallStarted { invocation, .. },
            CourierEvent::ToolCallFinished { result },
            CourierEvent::Done
        ] if invocation.name == "demo"
            && result.tool == "demo"
            && result.exit_code == 0
            && result.stdout.contains("direct-tool-ok")
    ));
}

#[test]
fn wasm_courier_host_model_complete_uses_fallback_models() {
    static REFERENCE_GUEST: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/dispatch-wasm-guest-reference.wasm"
    ));

    let test_image = build_test_image_with_binary_files(
        "\
FROM dispatch/wasm:latest
COMPONENT components/reference.wasm
SOUL SOUL.md
MODEL primary-model
FALLBACK fallback-model
ENTRYPOINT chat
",
        &[("SOUL.md", "Soul body")],
        &[("components/reference.wasm", REFERENCE_GUEST)],
    );
    let backend = Arc::new(FakeChatBackend::with_replies(vec![
        None,
        Some(ModelReply {
            text: Some("fallback wasm reply".to_string()),
            backend: "fake".to_string(),
            response_id: None,
            tool_calls: Vec::new(),
        }),
    ]));
    let courier = WasmCourier::with_chat_backend(backend.clone());
    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "model".to_string(),
            },
        },
    ))
    .unwrap();

    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::Message { role, content })
            if role == "assistant" && content == "fallback wasm reply"
    ));

    let calls = backend.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].model, "primary-model");
    assert_eq!(calls[1].model, "fallback-model");
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
    let courier = WasmCourier::new().unwrap();

    let job_session = futures::executor::block_on(courier.open_session(&job_image.image)).unwrap();
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
fn wasm_courier_reference_guest_memory_persists_across_sessions() {
    static REFERENCE_GUEST: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/dispatch-wasm-guest-reference.wasm"
    ));

    let test_image = build_test_image_with_binary_files(
        "\
FROM dispatch/wasm:latest
COMPONENT components/reference.wasm
MOUNT SESSION sqlite
MOUNT MEMORY sqlite
ENTRYPOINT chat
",
        &[],
        &[("components/reference.wasm", REFERENCE_GUEST)],
    );
    let courier = WasmCourier::new().unwrap();

    let first_session =
        futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
    let first_response = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session: first_session,
            operation: CourierOperation::Chat {
                input: "remember profile:name Christian".to_string(),
            },
        },
    ))
    .unwrap();
    assert!(matches!(
        first_response.events.first(),
        Some(CourierEvent::Message { content, .. }) if content == "remembered profile:name"
    ));

    let second_session =
        futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
    let second_response = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session: second_session,
            operation: CourierOperation::Chat {
                input: "recall profile:name".to_string(),
            },
        },
    ))
    .unwrap();
    assert!(matches!(
        second_response.events.first(),
        Some(CourierEvent::Message { content, .. }) if content == "profile:name = Christian"
    ));
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
fn docker_courier_chat_executes_reference_reply_and_records_history() {
    let test_image = build_test_image(
        "\
FROM dispatch/docker:latest
ENTRYPOINT chat
",
        &[],
    );
    let courier = DockerCourier::default();
    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

    let response = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "hello".to_string(),
            },
        },
    ))
    .unwrap();

    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::Message { content, .. })
            if content.contains("Docker chat reference reply")
    ));
    assert_eq!(response.session.turn_count, 1);
    assert_eq!(response.session.history.len(), 2);
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
#[cfg(unix)]
fn docker_courier_enforces_tool_timeout_for_local_tools() {
    let dir = tempdir().unwrap();
    let docker_bin = dir.path().join("docker");
    fs::write(&docker_bin, "#!/bin/sh\nsleep 0.2\ncat >/dev/null\n").unwrap();
    fs::set_permissions(&docker_bin, fs::Permissions::from_mode(0o755)).unwrap();

    let test_image = build_test_image(
        "\
FROM dispatch/docker:latest
TIMEOUT TOOL 50ms
TOOL LOCAL tools/demo.sh AS demo
ENTRYPOINT job
",
        &[("tools/demo.sh", "printf ok")],
    );
    let courier = DockerCourier::new(&docker_bin, "python:3.13-alpine");
    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

    let error = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session,
            operation: CourierOperation::InvokeTool {
                invocation: ToolInvocation {
                    name: "demo".to_string(),
                    input: None,
                },
            },
        },
    ))
    .unwrap_err();

    assert!(matches!(
        error,
        CourierError::ToolTimedOut { ref tool, ref timeout }
            if tool == "demo" && timeout == "TOOL"
    ));
}

#[test]
#[cfg(unix)]
fn docker_courier_chat_executes_model_tool_calls_via_docker_cli() {
    let dir = tempdir().unwrap();
    let docker_bin = dir.path().join("docker");
    fs::write(
        &docker_bin,
        "#!/bin/sh\nprintf 'docker-tool-output'\ncat >/dev/null\n",
    )
    .unwrap();
    fs::set_permissions(&docker_bin, fs::Permissions::from_mode(0o755)).unwrap();

    let test_image = build_test_image(
        "\
FROM dispatch/docker:latest
MODEL gpt-5-mini
TOOL LOCAL tools/demo.sh AS demo SCHEMA schemas/demo.json
ENTRYPOINT chat
",
        &[
            ("tools/demo.sh", "printf ok"),
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
            text: Some("docker final answer".to_string()),
            backend: "fake".to_string(),
            response_id: Some("resp_2".to_string()),
            tool_calls: Vec::new(),
        }),
    ]));
    let courier =
        DockerCourier::new(&docker_bin, "python:3.13-alpine").with_chat_backend(backend.clone());
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
    assert_eq!(calls[1].tool_outputs.len(), 1);
    assert!(
        calls[1].tool_outputs[0]
            .output
            .contains("docker-tool-output")
    );
    drop(calls);

    assert!(matches!(
        response.events.get(1),
        Some(CourierEvent::ToolCallFinished { result })
            if result.tool == "demo" && result.stdout.contains("docker-tool-output")
    ));
    assert!(matches!(
        response.events.iter().rev().nth(1),
        Some(CourierEvent::Message { content, .. }) if content == "docker final answer"
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
fn native_courier_memory_sqlite_persists_across_sessions() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MOUNT SESSION sqlite
MOUNT MEMORY sqlite
ENTRYPOINT chat
",
        &[],
    );
    let courier = NativeCourier::default();
    let first_session =
        futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

    let first_response = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session: first_session,
            operation: CourierOperation::Chat {
                input: "/memory put profile:name Christian".to_string(),
            },
        },
    ))
    .unwrap();
    assert!(matches!(
        first_response.events.first(),
        Some(CourierEvent::Message { content, .. }) if content == "Stored memory profile:name"
    ));

    let second_session =
        futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();
    let second_response = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session: second_session,
            operation: CourierOperation::Chat {
                input: "/memory get profile:name".to_string(),
            },
        },
    ))
    .unwrap();

    assert!(matches!(
        second_response.events.first(),
        Some(CourierEvent::Message { content, .. }) if content == "profile:name = Christian"
    ));
}

#[test]
fn native_courier_memory_put_reports_updates_after_first_write() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MOUNT SESSION sqlite
MOUNT MEMORY sqlite
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
                input: "/memory put profile:name Christian".to_string(),
            },
        },
    ))
    .unwrap();
    assert!(matches!(
        first.events.first(),
        Some(CourierEvent::Message { content, .. }) if content == "Stored memory profile:name"
    ));

    let second = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session: first.session,
            operation: CourierOperation::Chat {
                input: "/memory put profile:name Chris".to_string(),
            },
        },
    ))
    .unwrap();
    assert!(matches!(
        second.events.first(),
        Some(CourierEvent::Message { content, .. }) if content == "Updated memory profile:name"
    ));

    let third = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session: second.session,
            operation: CourierOperation::Chat {
                input: "/memory get profile:name".to_string(),
            },
        },
    ))
    .unwrap();
    assert!(matches!(
        third.events.first(),
        Some(CourierEvent::Message { content, .. }) if content == "profile:name = Chris"
    ));
}

#[test]
fn native_memory_list_treats_underscore_as_literal_prefix_character() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MOUNT SESSION sqlite
MOUNT MEMORY sqlite
ENTRYPOINT chat
",
        &[],
    );
    let courier = NativeCourier::default();
    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

    let session = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "/memory put default:user_1 first".to_string(),
            },
        },
    ))
    .unwrap()
    .session;
    let session = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "/memory put default:userA second".to_string(),
            },
        },
    ))
    .unwrap()
    .session;
    let response = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "/memory list default:user_".to_string(),
            },
        },
    ))
    .unwrap();

    let CourierEvent::Message { content, .. } = response.events.first().unwrap() else {
        panic!("expected message event");
    };
    assert!(content.contains("default:user_1 = first"));
    assert!(!content.contains("default:userA = second"));
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
fn list_native_builtin_tools_only_exposes_supported_memory_capabilities() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL BUILTIN memory_put
TOOL BUILTIN memory_get DESCRIPTION \"Read remembered state.\"
TOOL BUILTIN memory_list_range
TOOL BUILTIN memory_delete_range
TOOL BUILTIN memory_put_many
TOOL BUILTIN checkpoint_put
TOOL BUILTIN checkpoint_list
TOOL BUILTIN web_search
ENTRYPOINT chat
",
        &[],
    );

    let tools = list_native_builtin_tools(&test_image.image);
    assert_eq!(tools.len(), 7);
    assert_eq!(tools[0].capability, "memory_put");
    assert_eq!(tools[1].capability, "memory_get");
    assert_eq!(tools[2].capability, "memory_list_range");
    assert_eq!(tools[3].capability, "memory_delete_range");
    assert_eq!(tools[4].capability, "memory_put_many");
    assert_eq!(tools[5].capability, "checkpoint_put");
    assert_eq!(tools[6].capability, "checkpoint_list");
    assert_eq!(
        tools[1].description.as_deref(),
        Some("Read remembered state.")
    );
}

#[test]
fn build_model_request_includes_supported_builtin_memory_tools() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL BUILTIN memory_put
TOOL BUILTIN memory_get
TOOL BUILTIN memory_list_range
TOOL BUILTIN memory_delete_range
TOOL BUILTIN memory_put_many
TOOL BUILTIN checkpoint_put
TOOL BUILTIN checkpoint_list
ENTRYPOINT chat
",
        &[],
    );

    let request = build_model_request(
        &test_image.image,
        &[ConversationMessage {
            role: "user".to_string(),
            content: "remember this".to_string(),
        }],
        &[],
    )
    .unwrap()
    .expect("expected model request");

    assert_eq!(request.tools.len(), 7);
    assert_eq!(request.tools[0].name, "memory_put");
    assert!(matches!(
        request.tools[0].format,
        ModelToolFormat::JsonSchema { .. }
    ));
    assert_eq!(request.tools[1].name, "memory_get");
    assert_eq!(request.tools[2].name, "memory_list_range");
    assert_eq!(request.tools[3].name, "memory_delete_range");
    assert_eq!(request.tools[4].name, "memory_put_many");
    assert_eq!(request.tools[5].name, "checkpoint_put");
    assert_eq!(request.tools[6].name, "checkpoint_list");
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
fn default_chat_backend_selects_openai_compatible_from_env() {
    let backend = default_chat_backend_for_provider_with(None, |name| match name {
        "LLM_BACKEND" => Some("openai_compatible".to_string()),
        _ => None,
    });

    assert_eq!(backend.id(), "openai_compatible_chat_completions");
    assert!(!backend.supports_previous_response_id());
}

#[test]
fn default_chat_backend_selects_anthropic_from_env() {
    let backend = default_chat_backend_for_provider_with(None, |name| match name {
        "LLM_BACKEND" => Some("anthropic".to_string()),
        _ => None,
    });

    assert_eq!(backend.id(), "anthropic_messages");
    assert!(!backend.supports_previous_response_id());
}

#[test]
fn default_chat_backend_selects_gemini_from_env() {
    let backend = default_chat_backend_for_provider_with(None, |name| match name {
        "LLM_BACKEND" => Some("gemini".to_string()),
        _ => None,
    });

    assert_eq!(backend.id(), "google_gemini_generate_content");
    assert!(!backend.supports_previous_response_id());
}

#[test]
fn default_chat_backend_prefers_model_provider_over_env() {
    let backend = default_chat_backend_for_provider_with(Some("anthropic"), |name| match name {
        "LLM_BACKEND" => Some("openai".to_string()),
        _ => None,
    });

    assert_eq!(backend.id(), "anthropic_messages");
}

#[test]
fn extract_openai_chat_completions_output_parses_tool_calls() {
    let body = serde_json::json!({
        "choices": [
            {
                "message": {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": "call_fn",
                            "type": "function",
                            "function": {
                                "name": "lookup",
                                "arguments": "{\"id\":\"123\"}"
                            }
                        },
                        {
                            "id": "call_custom",
                            "type": "custom",
                            "custom": {
                                "name": "shell",
                                "input": "echo hi"
                            }
                        }
                    ]
                }
            }
        ]
    });

    let reply = match extract_openai_chat_completions_output(&body).unwrap() {
        ModelGeneration::Reply(reply) => reply,
        ModelGeneration::NotConfigured { backend, reason } => {
            panic!("expected model reply, got unconfigured backend {backend}: {reason}");
        }
    };
    assert_eq!(reply.backend, "openai_compatible_chat_completions");
    assert!(reply.text.is_none());
    assert_eq!(reply.tool_calls.len(), 2);
    assert_eq!(reply.tool_calls[0].kind, ModelToolKind::Function);
    assert_eq!(reply.tool_calls[0].name, "lookup");
    assert_eq!(reply.tool_calls[1].kind, ModelToolKind::Custom);
    assert_eq!(reply.tool_calls[1].input, "echo hi");
}

#[test]
fn extract_anthropic_output_parses_tool_use_blocks() {
    let body = serde_json::json!({
        "id": "msg_123",
        "content": [
            { "type": "text", "text": "Let me check." },
            {
                "type": "tool_use",
                "id": "toolu_123",
                "name": "lookup",
                "input": { "id": "123" }
            }
        ]
    });

    let reply = match extract_anthropic_output(&body).unwrap() {
        ModelGeneration::Reply(reply) => reply,
        ModelGeneration::NotConfigured { backend, reason } => {
            panic!("expected anthropic reply, got unconfigured backend {backend}: {reason}");
        }
    };
    assert_eq!(reply.backend, "anthropic_messages");
    assert_eq!(reply.response_id.as_deref(), Some("msg_123"));
    assert_eq!(reply.text.as_deref(), Some("Let me check."));
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].name, "lookup");
    assert_eq!(reply.tool_calls[0].kind, ModelToolKind::Function);
    assert_eq!(reply.tool_calls[0].input, "{\"id\":\"123\"}");
}

#[test]
fn extract_gemini_output_parses_function_calls() {
    let body = serde_json::json!({
        "candidates": [
            {
                "content": {
                    "parts": [
                        { "text": "Checking..." },
                        {
                            "functionCall": {
                                "name": "lookup",
                                "args": { "id": "123" }
                            }
                        }
                    ]
                }
            }
        ]
    });

    let reply = match extract_gemini_output(&body).unwrap() {
        ModelGeneration::Reply(reply) => reply,
        ModelGeneration::NotConfigured { backend, reason } => {
            panic!("expected gemini reply, got unconfigured backend {backend}: {reason}");
        }
    };
    assert_eq!(reply.backend, "google_gemini_generate_content");
    assert_eq!(reply.text.as_deref(), Some("Checking..."));
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].name, "lookup");
    assert_eq!(reply.tool_calls[0].kind, ModelToolKind::Function);
    assert_eq!(reply.tool_calls[0].input, "{\"id\":\"123\"}");
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
fn configured_model_id_uses_env_when_primary_missing() {
    let model = configured_model_id_with(None, |name| match name {
        "LLM_MODEL" => Some("claude-sonnet-4".to_string()),
        _ => None,
    });

    assert_eq!(model.as_deref(), Some("claude-sonnet-4"));
}

#[test]
fn configured_context_token_limit_uses_last_valid_context_limit() {
    let limits = vec![
        crate::manifest::LimitSpec {
            scope: "ITERATIONS".to_string(),
            value: "10".to_string(),
            qualifiers: Vec::new(),
        },
        crate::manifest::LimitSpec {
            scope: "CONTEXT_TOKENS".to_string(),
            value: "16000".to_string(),
            qualifiers: Vec::new(),
        },
        crate::manifest::LimitSpec {
            scope: "CONTEXT_TOKENS".to_string(),
            value: "32000".to_string(),
            qualifiers: Vec::new(),
        },
    ];

    assert_eq!(configured_context_token_limit(&limits), Some(32000));
}

#[test]
fn configured_llm_timeout_ms_uses_last_matching_timeout() {
    let timeouts = vec![
        crate::manifest::TimeoutSpec {
            scope: "LLM".to_string(),
            duration: "15s".to_string(),
            qualifiers: Vec::new(),
        },
        crate::manifest::TimeoutSpec {
            scope: "TOOL".to_string(),
            duration: "50ms".to_string(),
            qualifiers: Vec::new(),
        },
        crate::manifest::TimeoutSpec {
            scope: "LLM".to_string(),
            duration: "1200ms".to_string(),
            qualifiers: Vec::new(),
        },
    ];

    assert_eq!(configured_llm_timeout_ms(&timeouts).unwrap(), Some(1200));
}

#[test]
fn configured_tool_limits_use_last_valid_values() {
    let limits = vec![
        crate::manifest::LimitSpec {
            scope: "TOOL_CALLS".to_string(),
            value: "2".to_string(),
            qualifiers: Vec::new(),
        },
        crate::manifest::LimitSpec {
            scope: "TOOL_OUTPUT".to_string(),
            value: "0".to_string(),
            qualifiers: Vec::new(),
        },
        crate::manifest::LimitSpec {
            scope: "TOOL_CALLS".to_string(),
            value: "5".to_string(),
            qualifiers: Vec::new(),
        },
        crate::manifest::LimitSpec {
            scope: "TOOL_OUTPUT".to_string(),
            value: "1024".to_string(),
            qualifiers: Vec::new(),
        },
        crate::manifest::LimitSpec {
            scope: "TOOL_ROUNDS".to_string(),
            value: "0".to_string(),
            qualifiers: Vec::new(),
        },
        crate::manifest::LimitSpec {
            scope: "TOOL_ROUNDS".to_string(),
            value: "6".to_string(),
            qualifiers: Vec::new(),
        },
    ];

    assert_eq!(configured_tool_call_limit(&limits), Some(5));
    assert_eq!(configured_tool_output_limit(&limits), Some(1024));
    assert_eq!(configured_tool_round_limit(&limits), Some(6));
}

#[test]
fn truncate_tool_output_preserves_utf8_boundaries() {
    let output = "hello π world and a much longer tool output payload".to_string();
    let truncated = truncate_tool_output(output, Some(40));
    assert!(truncated.is_char_boundary(truncated.len()));
    assert!(truncated.contains("[dispatch truncated tool output]"));
}

#[test]
fn courier_error_retryability_is_classified() {
    assert!(CourierError::ModelBackendRequest("network".to_string()).is_retryable());
    assert!(
        !CourierError::ToolCallLimitExceeded {
            limit: 2,
            attempted: 3
        }
        .is_retryable()
    );
    assert!(
        !CourierError::ToolTimedOut {
            tool: "slow".to_string(),
            timeout: "TOOL".to_string()
        }
        .is_retryable()
    );
    assert!(
        !CourierError::RunTimedOut {
            session_id: "session-1".to_string(),
            timeout: "RUN".to_string()
        }
        .is_retryable()
    );
    assert!(
        !CourierError::MissingSecret {
            name: "OPENAI_API_KEY".to_string()
        }
        .is_retryable()
    );
}

#[test]
fn anthropic_max_tokens_uses_context_token_limit_when_present() {
    let request = ModelRequest {
        model: "claude-sonnet-4".to_string(),
        provider: Some("anthropic".to_string()),
        llm_timeout_ms: None,
        context_token_limit: Some(16000),
        tool_call_limit: None,
        tool_output_limit: None,
        instructions: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        pending_tool_calls: Vec::new(),
        tool_outputs: Vec::new(),
        previous_response_id: None,
    };

    assert_eq!(anthropic_max_tokens(&request), 16000);
}

#[test]
fn normalize_local_tool_input_extracts_function_style_text_payload() {
    let tool = LocalToolSpec {
        alias: "demo".to_string(),
        description: None,
        input_schema_packaged_path: None,
        input_schema_sha256: None,
        approval: None,
        risk: None,
        skill_source: None,
        target: LocalToolTarget::Local {
            packaged_path: "tools/demo.sh".to_string(),
            command: "bash".to_string(),
            args: Vec::new(),
        },
    };

    let normalized = normalize_local_tool_input(&tool, "{\"input\":\"echo hi\"}").unwrap();
    assert_eq!(normalized.as_ref(), "echo hi");
}

#[test]
fn openai_chat_completions_messages_include_structured_tool_followup() {
    let request = ModelRequest {
        model: "gpt-5-mini".to_string(),
        provider: Some("openai_compatible".to_string()),
        llm_timeout_ms: None,
        context_token_limit: None,
        tool_call_limit: None,
        tool_output_limit: None,
        instructions: "Be helpful.".to_string(),
        messages: vec![ConversationMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        }],
        tools: Vec::new(),
        pending_tool_calls: vec![ModelToolCall {
            call_id: "call_1".to_string(),
            name: "lookup".to_string(),
            input: "{\"id\":\"123\"}".to_string(),
            kind: ModelToolKind::Function,
        }],
        tool_outputs: vec![ModelToolOutput {
            call_id: "call_1".to_string(),
            name: "lookup".to_string(),
            output: "found".to_string(),
            kind: ModelToolKind::Function,
        }],
        previous_response_id: None,
    };

    let messages = openai_chat_completions_messages(&request);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["tool_calls"][0]["function"]["name"], "lookup");
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "call_1");
    assert_eq!(messages[3]["content"], "found");
}

#[test]
fn anthropic_messages_include_tool_use_and_tool_result_blocks() {
    let request = ModelRequest {
        model: "claude-sonnet-4".to_string(),
        provider: Some("anthropic".to_string()),
        llm_timeout_ms: None,
        context_token_limit: None,
        tool_call_limit: None,
        tool_output_limit: None,
        instructions: String::new(),
        messages: vec![ConversationMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        }],
        tools: Vec::new(),
        pending_tool_calls: vec![ModelToolCall {
            call_id: "toolu_1".to_string(),
            name: "lookup".to_string(),
            input: "{\"id\":\"123\"}".to_string(),
            kind: ModelToolKind::Function,
        }],
        tool_outputs: vec![ModelToolOutput {
            call_id: "toolu_1".to_string(),
            name: "lookup".to_string(),
            output: "found".to_string(),
            kind: ModelToolKind::Function,
        }],
        previous_response_id: None,
    };

    let messages = anthropic_messages(&request);
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"][0]["type"], "tool_use");
    assert_eq!(messages[1]["content"][0]["name"], "lookup");
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"][0]["type"], "tool_result");
    assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_1");
}

#[test]
fn gemini_messages_include_function_call_and_response_parts() {
    let request = ModelRequest {
        model: "gemini-2.5-pro".to_string(),
        provider: Some("gemini".to_string()),
        llm_timeout_ms: None,
        context_token_limit: None,
        tool_call_limit: None,
        tool_output_limit: None,
        instructions: String::new(),
        messages: vec![ConversationMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        }],
        tools: Vec::new(),
        pending_tool_calls: vec![ModelToolCall {
            call_id: "call_1".to_string(),
            name: "lookup".to_string(),
            input: "{\"id\":\"123\"}".to_string(),
            kind: ModelToolKind::Function,
        }],
        tool_outputs: vec![ModelToolOutput {
            call_id: "call_1".to_string(),
            name: "lookup".to_string(),
            output: "found".to_string(),
            kind: ModelToolKind::Function,
        }],
        previous_response_id: None,
    };

    let messages = gemini_messages(&request);
    assert_eq!(messages[1]["role"], "model");
    assert_eq!(messages[1]["parts"][0]["functionCall"]["name"], "lookup");
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(
        messages[2]["parts"][0]["functionResponse"]["name"],
        "lookup"
    );
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
fn native_courier_chat_executes_builtin_memory_tools() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL BUILTIN memory_put
TOOL BUILTIN memory_get
MOUNT MEMORY sqlite
ENTRYPOINT chat
",
        &[],
    );
    let backend = Arc::new(FakeChatBackend::with_replies(vec![
        Some(ModelReply {
            text: None,
            backend: "fake".to_string(),
            response_id: Some("resp_1".to_string()),
            tool_calls: vec![ModelToolCall {
                call_id: "call_1".to_string(),
                name: "memory_put".to_string(),
                input: "{\"namespace\":\"profile\",\"key\":\"name\",\"value\":\"Christian\"}"
                    .to_string(),
                kind: ModelToolKind::Function,
            }],
        }),
        Some(ModelReply {
            text: None,
            backend: "fake".to_string(),
            response_id: Some("resp_2".to_string()),
            tool_calls: vec![ModelToolCall {
                call_id: "call_2".to_string(),
                name: "memory_get".to_string(),
                input: "{\"namespace\":\"profile\",\"key\":\"name\"}".to_string(),
                kind: ModelToolKind::Function,
            }],
        }),
        Some(ModelReply {
            text: Some("memory complete".to_string()),
            backend: "fake".to_string(),
            response_id: Some("resp_3".to_string()),
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
                input: "remember my name".to_string(),
            },
        },
    ))
    .unwrap();

    let calls = backend.calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].tools.len(), 2);
    assert_eq!(calls[0].tools[0].name, "memory_put");
    assert_eq!(calls[0].tools[1].name, "memory_get");
    assert!(matches!(
        calls[0].tools[0].format,
        ModelToolFormat::JsonSchema { .. }
    ));
    assert_eq!(calls[1].previous_response_id.as_deref(), Some("resp_1"));
    assert!(calls[1].messages.is_empty());
    assert_eq!(calls[1].tool_outputs.len(), 1);
    assert_eq!(calls[1].tool_outputs[0].name, "memory_put");
    assert!(
        calls[1].tool_outputs[0]
            .output
            .contains("Stored memory profile:name")
    );
    assert_eq!(calls[2].previous_response_id.as_deref(), Some("resp_2"));
    assert_eq!(calls[2].tool_outputs.len(), 1);
    assert_eq!(calls[2].tool_outputs[0].name, "memory_get");
    assert!(
        calls[2].tool_outputs[0]
            .output
            .contains("profile:name = Christian")
    );
    drop(calls);

    assert!(matches!(
        response.events.first(),
        Some(CourierEvent::ToolCallStarted { invocation, command, args })
            if invocation.name == "memory_put"
                && command == "dispatch-builtin"
                && args == &vec!["memory_put".to_string()]
    ));
    assert!(matches!(
        response.events.get(1),
        Some(CourierEvent::ToolCallFinished { result })
            if result.tool == "memory_put"
                && result.command == "dispatch-builtin"
                && result.stdout.contains("Stored memory profile:name")
    ));
    assert!(matches!(
        response.events.get(2),
        Some(CourierEvent::ToolCallStarted { invocation, command, args })
            if invocation.name == "memory_get"
                && command == "dispatch-builtin"
                && args == &vec!["memory_get".to_string()]
    ));
    assert!(matches!(
        response.events.get(3),
        Some(CourierEvent::ToolCallFinished { result })
            if result.tool == "memory_get"
                && result.stdout.contains("profile:name = Christian")
    ));
    assert!(matches!(
        response.events.get(4),
        Some(CourierEvent::Message { content, .. }) if content == "memory complete"
    ));
    assert!(matches!(response.events.last(), Some(CourierEvent::Done)));
}

#[test]
fn execute_builtin_memory_range_and_batch_tools() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MOUNT MEMORY sqlite
ENTRYPOINT chat
",
        &[],
    );
    let courier = NativeCourier::default();
    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

    let put_many = execute_builtin_tool(
            &session,
            "memory_put_many",
            r#"{"namespace":"profile","entries":[{"key":"a","value":"one"},{"key":"b","value":"two"},{"key":"c","value":"three"}]}"#,
        )
        .unwrap();
    assert!(
        put_many
            .stdout
            .contains("Stored 3 and updated 0 memory entries in namespace `profile`.")
    );

    let range = execute_builtin_tool(
        &session,
        "memory_list_range",
        r#"{"namespace":"profile","start_key":"b","end_key":"d"}"#,
    )
    .unwrap();
    assert_eq!(range.stdout, "profile:b = two\nprofile:c = three");

    let delete = execute_builtin_tool(
        &session,
        "memory_delete_range",
        r#"{"namespace":"profile","start_key":"b","end_key":"c"}"#,
    )
    .unwrap();
    assert!(
        delete
            .stdout
            .contains("Deleted 1 memory entry from namespace `profile`.")
    );

    let remaining =
        execute_builtin_tool(&session, "memory_list_range", r#"{"namespace":"profile"}"#).unwrap();
    assert_eq!(remaining.stdout, "profile:a = one\nprofile:c = three");
}

#[test]
fn execute_builtin_checkpoint_tools() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MOUNT SESSION sqlite
ENTRYPOINT chat
",
        &[],
    );
    let courier = NativeCourier::default();
    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

    let put = execute_builtin_tool(
        &session,
        "checkpoint_put",
        r#"{"name":"fetch-users","value":"{\"page\":2}"}"#,
    )
    .unwrap();
    assert_eq!(put.stdout, "Stored checkpoint `fetch-users`");

    let get =
        execute_builtin_tool(&session, "checkpoint_get", r#"{"name":"fetch-users"}"#).unwrap();
    assert_eq!(get.stdout, r#"fetch-users = {"page":2}"#);

    let list = execute_builtin_tool(&session, "checkpoint_list", r#"{"prefix":"fetch"}"#).unwrap();
    assert_eq!(list.stdout, r#"fetch-users = {"page":2}"#);

    let delete =
        execute_builtin_tool(&session, "checkpoint_delete", r#"{"name":"fetch-users"}"#).unwrap();
    assert_eq!(delete.stdout, "Deleted checkpoint `fetch-users`");
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
fn load_parcel_rejects_manifests_that_fail_schema_validation() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
SOUL SOUL.md
ENTRYPOINT chat
",
        &[("SOUL.md", "You are schema-checked.")],
    );

    let manifest_path = test_image.image.parcel_dir.join("manifest.json");
    let mut manifest = serde_json::from_slice::<Value>(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["tools"] = Value::String("not-an-array".to_string());
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let error = load_parcel(&test_image.image.parcel_dir).unwrap_err();
    assert!(matches!(error, CourierError::InvalidParcelSchema { .. }));
}

#[test]
fn native_courier_persists_session_sqlite_mounts() {
    let test_image = build_test_image(
        "\
FROM dispatch/native:latest
MOUNT SESSION sqlite
ENTRYPOINT chat
",
        &[],
    );
    let courier = NativeCourier::default();
    let session = futures::executor::block_on(courier.open_session(&test_image.image)).unwrap();

    let sqlite_mount = session
        .resolved_mounts
        .iter()
        .find(|mount| mount.kind == MountKind::Session && mount.driver == "sqlite")
        .expect("expected sqlite session mount");
    let connection = Connection::open(&sqlite_mount.target_path).unwrap();
    let (turn_count, payload_json): (i64, String) = connection
        .query_row(
            "SELECT turn_count, payload_json FROM dispatch_sessions WHERE session_id = ?1",
            [&session.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(turn_count, 0);
    let persisted: CourierSession = serde_json::from_str(&payload_json).unwrap();
    assert_eq!(persisted.id, session.id);

    let response = futures::executor::block_on(courier.run(
        &test_image.image,
        CourierRequest {
            session,
            operation: CourierOperation::Chat {
                input: "hello".to_string(),
            },
        },
    ))
    .unwrap();
    let updated_turn_count: i64 = connection
        .query_row(
            "SELECT turn_count FROM dispatch_sessions WHERE session_id = ?1",
            [&response.session.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(updated_turn_count, 1);
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
