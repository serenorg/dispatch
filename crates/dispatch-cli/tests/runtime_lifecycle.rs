use dispatch_core::{
    BuildOptions, CourierPluginExec, CourierPluginManifest, PluginTransport, build_agentfile,
    install_courier_plugin,
};
use dispatch_process::run_command_with_file_capture;
#[cfg(unix)]
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};
use tempfile::tempdir;

fn dispatch_bin() -> PathBuf {
    if let Ok(current_exe) = std::env::current_exe()
        && let Some(bin_dir) = current_exe.parent()
        && bin_dir.file_name().is_some_and(|name| name == "deps")
        && let Some(target_dir) = bin_dir.parent()
    {
        let sibling = target_dir.join(format!("dispatch{}", std::env::consts::EXE_SUFFIX));
        if sibling.is_file() {
            return sibling;
        }
    }

    let cargo_bin = PathBuf::from(env!("CARGO_BIN_EXE_dispatch"));
    if let Some(bin_dir) = cargo_bin.parent()
        && bin_dir.file_name().is_some_and(|name| name == "deps")
        && let Some(target_dir) = bin_dir.parent()
    {
        let sibling = target_dir.join(cargo_bin.file_name().unwrap_or_default());
        if sibling.is_file() {
            return sibling;
        }
    }
    cargo_bin
}

fn write_heartbeat_parcel(
    dir: &Path,
    extra_agentfile_lines: &[&str],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut agentfile = String::from(
        "FROM dispatch/native:latest\n\nNAME runtime-lifecycle\nVERSION 0.1.0\n\nUSER USER.md\nHEARTBEAT EVERY 60s FILE HEARTBEAT.md\n",
    );
    for line in extra_agentfile_lines {
        agentfile.push_str(line);
        agentfile.push('\n');
    }
    agentfile.push_str("ENTRYPOINT heartbeat\n");
    fs::write(dir.join("Agentfile"), agentfile)?;
    fs::write(
        dir.join("USER.md"),
        "You are a local runtime lifecycle test parcel.\n",
    )?;
    fs::write(
        dir.join("HEARTBEAT.md"),
        "When you receive a heartbeat payload, acknowledge it briefly.\n",
    )?;
    Ok(())
}

fn run_dispatch(
    cwd: &Path,
    envs: &[(&str, &str)],
    args: &[&str],
) -> Result<std::process::Output, Box<dyn std::error::Error>> {
    let dispatch_bin = dispatch_bin();
    let capture_dir = tempdir()?;
    let stdout_path = capture_dir.path().join("stdout.txt");
    let stderr_path = capture_dir.path().join("stderr.txt");
    let mut command = Command::new(&dispatch_bin);
    command.current_dir(cwd).args(args);
    for (name, value) in envs {
        command.env(name, value);
    }
    run_command_with_file_capture(
        &mut command,
        &stdout_path,
        &stderr_path,
        Duration::from_secs(15),
        Duration::from_millis(50),
    )
    .map_err(Into::into)
}

fn output_text(output: &std::process::Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn require_success(
    output: std::process::Output,
    context: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let (stdout, stderr) = output_text(&output);
    if !output.status.success() {
        return Err(format!("{context} failed\nstdout:\n{stdout}\nstderr:\n{stderr}").into());
    }
    Ok(stdout)
}

fn extract_value(prefix: &str, output: &str) -> Result<String, Box<dyn std::error::Error>> {
    output
        .lines()
        .find_map(|line| line.strip_prefix(prefix).map(str::to_string))
        .ok_or_else(|| format!("missing `{prefix}` in output:\n{output}").into())
}

fn read_run_record(record_path: &Path) -> Result<Value, Box<dyn std::error::Error>> {
    Ok(serde_json::from_slice(&fs::read(record_path)?)?)
}

fn wait_for_run_record(
    record_path: &Path,
    predicate: impl Fn(&Value) -> bool,
) -> Result<Value, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let record = read_run_record(record_path)?;
        if predicate(&record) {
            return Ok(record);
        }
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for record {}", record_path.display()).into());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn http_request(addr: &str, request: &str) -> Result<String, Box<dyn std::error::Error>> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(request.as_bytes())?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}

fn http_request_with_retry(
    addr: &str,
    request: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match http_request(addr, request) {
            Ok(response) => return Ok(response),
            Err(error) if Instant::now() < deadline => {
                let retryable = error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io_error| {
                        matches!(
                            io_error.kind(),
                            std::io::ErrorKind::ConnectionRefused
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::ConnectionReset
                        )
                    });
                if !retryable {
                    return Err(error);
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error),
        }
    }
}

fn reserve_loopback_addr() -> Result<String, Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?.to_string();
    drop(listener);
    Ok(addr)
}

fn write_channel_test_plugin(dir: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let plugin_dir = dir.join("channel-plugin");
    fs::create_dir_all(&plugin_dir)?;

    let script_path = plugin_dir.join("channel-test.sh");
    fs::write(
        &script_path,
        r#"#!/bin/sh
read line
case "$line" in
    *'"kind":"start_ingress"'*)
        printf '%s\n' '{"kind":"ingress_started","state":{"mode":"webhook","status":"registered","endpoint":"https://example.test/hook","metadata":{}}}'
        ;;
    *'"kind":"stop_ingress"'*)
        printf '%s\n' '{"kind":"ingress_stopped","state":{"mode":"webhook","status":"stopped","endpoint":"https://example.test/hook","metadata":{}}}'
        ;;
    *'"kind":"ingress_event"'*)
        printf '%s\n' '{"kind":"ingress_events_received","events":[{"event_id":"evt-1","platform":"webhook","event_type":"message","received_at":"2026-04-12T00:00:00Z","conversation":{"id":"conv-1","kind":"private"},"actor":{"id":"user-1","is_bot":false,"metadata":{}},"message":{"id":"msg-1","content":"hello","content_type":"text/plain","attachments":[],"metadata":{}},"metadata":{}}],"callback_reply":{"status":202,"content_type":"text/plain; charset=utf-8","body":"accepted\n"}}'
        ;;
    *)
        printf '%s\n' '{"kind":"error","error":{"code":"unexpected_request","message":"unhandled request"}}'
        exit 1
        ;;
esac
"#,
    )?;
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&script_path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions)?;
    }

    let manifest_path = plugin_dir.join("channel-plugin.json");
    fs::write(
        &manifest_path,
        format!(
            r#"{{
    "kind": "channel",
    "name": "channel-test",
    "version": "0.1",
    "protocol": "jsonl",
    "protocol_version": 1,
    "description": "Test channel plugin",
    "entrypoint": {{
        "command": "{}",
        "args": []
    }},
    "capabilities": {{
        "channel": {{
            "platform": "webhook",
            "allowed_paths": ["/hook"],
            "delivery": {{
                "attachment_sources": ["data_base64"]
            }}
        }}
    }}
}}"#,
            script_path.display()
        ),
    )?;

    Ok(manifest_path)
}

fn write_lifecycle_channel_test_plugin(
    dir: &Path,
    log_path: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let plugin_dir = dir.join("lifecycle-channel-plugin");
    fs::create_dir_all(&plugin_dir)?;

    let script_path = plugin_dir.join("channel-test.sh");
    fs::write(
        &script_path,
        format!(
            r#"#!/bin/sh
read line
printf '%s\n' "$line" >> "{}"
case "$line" in
    *'"kind":"start_ingress"'*)
        printf '%s\n' '{{"kind":"ingress_started","state":{{"mode":"webhook","status":"registered","endpoint":"https://example.test/hook","metadata":{{"phase":"start"}}}}}}'
        ;;
    *'"kind":"stop_ingress"'*)
        printf '%s\n' '{{"kind":"ingress_stopped","state":{{"mode":"webhook","status":"stopped","endpoint":"https://example.test/hook","metadata":{{"phase":"stop"}}}}}}'
        ;;
    *'"kind":"ingress_event"'*)
        printf '%s\n' '{{"kind":"ingress_events_received","events":[],"callback_reply":{{"status":200,"content_type":"text/plain; charset=utf-8","body":"ok\n"}}}}'
        ;;
    *)
        printf '%s\n' '{{"kind":"error","error":{{"code":"unexpected_request","message":"unexpected request kind"}}}}'
        ;;
esac
"#,
            log_path.display()
        ),
    )?;
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&script_path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions)?;
    }

    let manifest_path = plugin_dir.join("channel-plugin.json");
    fs::write(
        &manifest_path,
        format!(
            r#"{{
    "kind": "channel",
    "name": "channel-lifecycle-test",
    "version": "0.1",
    "protocol": "jsonl",
    "protocol_version": 1,
    "description": "Lifecycle test channel plugin",
    "entrypoint": {{
        "command": "{}",
        "args": []
    }},
    "capabilities": {{
        "channel": {{
            "platform": "webhook",
            "allowed_paths": ["/hook"]
        }}
    }}
}}"#,
            script_path.display()
        ),
    )?;

    Ok(manifest_path)
}

fn write_polling_channel_test_plugin(
    dir: &Path,
    log_path: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let plugin_dir = dir.join("polling-channel-plugin");
    fs::create_dir_all(&plugin_dir)?;

    let script_path = plugin_dir.join("channel-test.sh");
    fs::write(
        &script_path,
        format!(
            r#"#!/bin/sh
read line
printf '%s\n' "$line" >> "{}"
case "$line" in
    *'"kind":"start_ingress"'*)
        printf '%s\n' '{{"kind":"ingress_started","state":{{"mode":"polling","status":"running","metadata":{{"cursor":"0"}}}}}}'
        ;;
    *'"kind":"poll_ingress"'*)
        printf '%s\n' '{{"kind":"ingress_events_received","events":[{{"event_id":"poll-evt-1","platform":"telegram","event_type":"message.received","received_at":"2026-04-12T00:00:00Z","conversation":{{"id":"chat-1","kind":"private"}},"actor":{{"id":"user-1","is_bot":false,"metadata":{{}}}},"message":{{"id":"msg-1","content":"hello from poll","content_type":"text/plain","attachments":[],"metadata":{{}}}},"metadata":{{"transport":"polling"}}}}],"state":{{"mode":"polling","status":"running","metadata":{{"cursor":"1"}}}},"poll_after_ms":25}}'
        ;;
    *'"kind":"stop_ingress"'*)
        printf '%s\n' '{{"kind":"ingress_stopped","state":{{"mode":"polling","status":"stopped","metadata":{{"cursor":"1"}}}}}}'
        ;;
    *)
        printf '%s\n' '{{"kind":"error","error":{{"code":"unexpected_request","message":"unexpected request kind"}}}}'
        ;;
esac
"#,
            log_path.display()
        ),
    )?;
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&script_path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions)?;
    }

    let manifest_path = plugin_dir.join("channel-plugin.json");
    fs::write(
        &manifest_path,
        format!(
            r#"{{
    "kind": "channel",
    "name": "channel-polling-test",
    "version": "0.1",
    "protocol": "jsonl",
    "protocol_version": 1,
    "description": "Polling test channel plugin",
    "entrypoint": {{
        "command": "{}",
        "args": []
    }},
    "capabilities": {{
        "channel": {{
            "platform": "telegram",
            "ingress_modes": ["polling"]
        }}
    }}
}}"#,
            script_path.display()
        ),
    )?;

    Ok(manifest_path)
}

fn write_logging_channel_test_plugin(
    dir: &Path,
    log_path: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let plugin_dir = dir.join("logging-channel-plugin");
    fs::create_dir_all(&plugin_dir)?;

    let script_path = plugin_dir.join("channel-test.sh");
    fs::write(
        &script_path,
        format!(
            r#"#!/bin/sh
read line
case "$line" in
    *'"kind":"start_ingress"'*)
        printf '%s\n' '{{"kind":"ingress_started","state":{{"mode":"webhook","status":"registered","endpoint":"https://example.test/hook","metadata":{{}}}}}}'
        ;;
    *'"kind":"stop_ingress"'*)
        printf '%s\n' '{{"kind":"ingress_stopped","state":{{"mode":"webhook","status":"stopped","endpoint":"https://example.test/hook","metadata":{{}}}}}}'
        ;;
    *'"kind":"ingress_event"'*)
        printf '%s\n' "$line" > "{}"
        printf '%s\n' '{{"kind":"ingress_events_received","events":[],"callback_reply":{{"status":200,"content_type":"text/plain; charset=utf-8","body":"ok\n"}}}}'
        ;;
    *)
        printf '%s\n' '{{"kind":"error","error":{{"code":"unexpected_request","message":"unexpected request kind"}}}}'
        exit 1
        ;;
esac
"#,
            log_path.display()
        ),
    )?;
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&script_path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions)?;
    }

    let manifest_path = plugin_dir.join("channel-plugin.json");
    fs::write(
        &manifest_path,
        format!(
            r#"{{
    "kind": "channel",
    "name": "channel-query-test",
    "version": "0.1",
    "protocol": "jsonl",
    "protocol_version": 1,
    "description": "Query decoding test channel plugin",
    "entrypoint": {{
        "command": "{}",
        "args": []
    }},
    "capabilities": {{
        "channel": {{
            "platform": "webhook",
            "allowed_paths": ["/hook"]
        }}
    }}
}}"#,
            script_path.display()
        ),
    )?;

    Ok(manifest_path)
}

fn write_trusted_channel_test_plugin(
    dir: &Path,
    secret_name: &str,
    log_path: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let plugin_dir = dir.join("trusted-channel-plugin");
    fs::create_dir_all(&plugin_dir)?;

    let script_path = plugin_dir.join("channel-test.sh");
    fs::write(
        &script_path,
        format!(
            r#"#!/bin/sh
read line
case "$line" in
    *'"kind":"start_ingress"'*)
        printf '%s\n' '{{"kind":"ingress_started","state":{{"mode":"webhook","status":"registered","endpoint":"https://example.test/hook","metadata":{{}}}}}}'
        ;;
    *'"kind":"stop_ingress"'*)
        printf '%s\n' '{{"kind":"ingress_stopped","state":{{"mode":"webhook","status":"stopped","endpoint":"https://example.test/hook","metadata":{{}}}}}}'
        ;;
    *'"kind":"ingress_event"'*)
        echo invoked >> "{}"
        printf '%s\n' '{{"kind":"ingress_events_received","events":[],"callback_reply":{{"status":200,"content_type":"text/plain; charset=utf-8","body":"ok\n"}}}}'
        ;;
    *)
        printf '%s\n' '{{"kind":"error","error":{{"code":"unexpected_request","message":"unexpected request kind"}}}}'
        exit 1
        ;;
esac
"#,
            log_path.display()
        ),
    )?;
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&script_path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions)?;
    }

    let manifest_path = plugin_dir.join("channel-plugin.json");
    fs::write(
        &manifest_path,
        format!(
            r#"{{
    "kind": "channel",
    "name": "channel-trusted-test",
    "version": "0.1",
    "protocol": "jsonl",
    "protocol_version": 1,
    "description": "Trusted test channel plugin",
    "entrypoint": {{
        "command": "{}",
        "args": []
    }},
    "capabilities": {{
        "channel": {{
            "platform": "webhook",
            "ingress": {{
                "endpoints": [
                    {{
                        "path": "/hook",
                        "methods": ["POST"],
                        "host_managed": true
                    }}
                ],
                "trust": {{
                    "mode": "shared_secret_header",
                    "header_name": "X-Dispatch-Secret",
                    "secret_name": "{}",
                    "host_managed": true
                }}
            }}
        }}
    }}
}}"#,
            script_path.display(),
            secret_name
        ),
    )?;

    Ok(manifest_path)
}

fn build_chat_test_parcel(root: &Path) -> Result<(PathBuf, String), Box<dyn std::error::Error>> {
    let context_dir = root.join("reply-parcel");
    fs::create_dir_all(&context_dir)?;
    fs::write(
        context_dir.join("Agentfile"),
        "FROM dispatch/native:latest\n\
NAME reply-fixture\n\
VERSION 0.1.0\n\
SKILL SKILL.md\n\
ENTRYPOINT chat\n",
    )?;
    fs::write(context_dir.join("SKILL.md"), "Always reply briefly.\n")?;

    let built = build_agentfile(
        &context_dir.join("Agentfile"),
        &BuildOptions {
            output_root: context_dir.join(".dispatch/parcels"),
        },
    )?;
    Ok((built.parcel_dir, built.digest))
}

fn install_test_courier_plugin(
    root: &Path,
    registry_path: &Path,
    name: &str,
    parcel_digest: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    install_test_courier_plugin_with_reply(root, registry_path, name, parcel_digest, "plugin reply")
}

fn write_test_courier_plugin_manifest(
    root: &Path,
    name: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let plugin_dir = root.join(format!("{name}-courier-plugin"));
    fs::create_dir_all(&plugin_dir)?;

    let script_path = plugin_dir.join("courier-test.sh");
    fs::write(
        &script_path,
        concat!(
            "#!/bin/sh\n",
            "set -eu\n",
            "while IFS= read -r line; do\n",
            "if printf '%s' \"$line\" | grep -q '\"kind\":\"capabilities\"'; then\n",
            "  printf '%s\\n' '{\"kind\":\"capabilities\",\"capabilities\":{\"courier_id\":\"demo-jsonl\",\"kind\":\"custom\",\"supports_chat\":true,\"supports_job\":false,\"supports_heartbeat\":false,\"supports_local_tools\":false,\"supports_mounts\":[]}}'\n",
            "else\n",
            "  printf '%s\\n' '{\"kind\":\"error\",\"error\":{\"code\":\"unexpected_request\",\"message\":\"unhandled request\"}}'\n",
            "  exit 1\n",
            "fi\n",
            "done\n"
        ),
    )?;
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&script_path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions)?;
    }

    let manifest_path = plugin_dir.join("courier-plugin.json");
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&CourierPluginManifest {
            name: name.to_string(),
            version: "0.1.0".to_string(),
            protocol_version: 1,
            transport: PluginTransport::Jsonl,
            description: Some("Demo courier plugin for runtime tests".to_string()),
            exec: CourierPluginExec {
                command: script_path.display().to_string(),
                args: Vec::new(),
            },
            installed_sha256: None,
        })?,
    )?;

    Ok(manifest_path)
}

fn install_test_courier_plugin_with_event(
    root: &Path,
    registry_path: &Path,
    name: &str,
    parcel_digest: &str,
    event: Value,
    history_content: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let script_path = root.join("courier-test.sh");
    let event_json = shell_single_quote_literal(
        &serde_json::json!({
            "kind": "event",
            "event": event,
        })
        .to_string(),
    );
    let done_json = shell_single_quote_literal(
        &serde_json::json!({
            "kind": "done",
            "session": {
                "id": "demo-jsonl-session",
                "parcel_digest": parcel_digest,
                "entrypoint": "chat",
                "turn_count": 1,
                "elapsed_ms": 0,
                "history": [{
                    "role": "assistant",
                    "content": history_content
                }],
                "resolved_mounts": [],
                "backend_state": "done"
            }
        })
        .to_string(),
    );
    fs::write(
        &script_path,
        format!(
            concat!(
                "#!/bin/sh\n",
                "set -eu\n",
                "while IFS= read -r line; do\n",
                "if printf '%s' \"$line\" | grep -q '\"kind\":\"capabilities\"'; then\n",
                "  printf '%s\\n' '{{\"kind\":\"capabilities\",\"capabilities\":{{\"courier_id\":\"demo-jsonl\",\"kind\":\"custom\",\"supports_chat\":true,\"supports_job\":false,\"supports_heartbeat\":false,\"supports_local_tools\":false,\"supports_mounts\":[]}}}}'\n",
                "elif printf '%s' \"$line\" | grep -q '\"kind\":\"validate_parcel\"'; then\n",
                "  printf '%s\\n' '{{\"kind\":\"ok\"}}'\n",
                "elif printf '%s' \"$line\" | grep -q '\"kind\":\"inspect\"'; then\n",
                "  printf '%s\\n' '{{\"kind\":\"inspection\",\"inspection\":{{\"courier_id\":\"demo-jsonl\",\"kind\":\"custom\",\"entrypoint\":\"chat\",\"required_secrets\":[],\"mounts\":[],\"local_tools\":[]}}}}'\n",
                "elif printf '%s' \"$line\" | grep -q '\"kind\":\"open_session\"'; then\n",
                "  printf '%s\\n' '{{\"kind\":\"session\",\"session\":{{\"id\":\"demo-jsonl-session\",\"parcel_digest\":\"{parcel_digest}\",\"entrypoint\":\"chat\",\"turn_count\":0,\"elapsed_ms\":0,\"history\":[],\"resolved_mounts\":[],\"backend_state\":\"open\"}}}}'\n",
                "elif printf '%s' \"$line\" | grep -q '\"kind\":\"run\"'; then\n",
                "  printf '%s\\n' '{event_json}'\n",
                "  printf '%s\\n' '{done_json}'\n",
                "else\n",
                "  printf '%s\\n' '{{\"kind\":\"error\",\"error\":{{\"code\":\"unexpected_request\",\"message\":\"unhandled request\"}}}}'\n",
                "  exit 1\n",
                "fi\n",
                "done\n"
            ),
            parcel_digest = parcel_digest,
            event_json = event_json,
            done_json = done_json,
        ),
    )?;
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&script_path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions)?;
    }

    let manifest_path = root.join("courier-plugin.json");
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&CourierPluginManifest {
            name: name.to_string(),
            version: "0.1.0".to_string(),
            protocol_version: 1,
            transport: PluginTransport::Jsonl,
            description: Some("Demo courier plugin for listener tests".to_string()),
            exec: CourierPluginExec {
                command: script_path.display().to_string(),
                args: Vec::new(),
            },
            installed_sha256: None,
        })?,
    )?;

    install_courier_plugin(&manifest_path, Some(registry_path))?;
    Ok(())
}

fn install_test_courier_plugin_with_reply(
    root: &Path,
    registry_path: &Path,
    name: &str,
    parcel_digest: &str,
    assistant_reply: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    install_test_courier_plugin_with_event(
        root,
        registry_path,
        name,
        parcel_digest,
        serde_json::json!({
            "kind": "message",
            "role": "assistant",
            "content": assistant_reply
        }),
        assistant_reply,
    )
}

fn shell_single_quote_literal(value: &str) -> String {
    value.replace('\'', "'\"'\"'")
}

fn write_reply_channel_test_plugin(
    dir: &Path,
    log_path: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let plugin_dir = dir.join("reply-channel-plugin");
    fs::create_dir_all(&plugin_dir)?;

    let script_path = plugin_dir.join("channel-test.sh");
    fs::write(
        &script_path,
        format!(
            concat!(
                "#!/bin/sh\n",
                "set -eu\n",
                "read line\n",
                "if printf '%s' \"$line\" | grep -q '\"kind\":\"start_ingress\"'; then\n",
                "  printf '%s\\n' '{{\"kind\":\"ingress_started\",\"state\":{{\"mode\":\"webhook\",\"status\":\"registered\",\"endpoint\":\"https://example.test/hook\",\"metadata\":{{}}}}}}'\n",
                "elif printf '%s' \"$line\" | grep -q '\"kind\":\"stop_ingress\"'; then\n",
                "  printf '%s\\n' '{{\"kind\":\"ingress_stopped\",\"state\":{{\"mode\":\"webhook\",\"status\":\"stopped\",\"endpoint\":\"https://example.test/hook\",\"metadata\":{{}}}}}}'\n",
                "elif printf '%s' \"$line\" | grep -q '\"kind\":\"ingress_event\"'; then\n",
                "  printf '%s\\n' '{{\"kind\":\"ingress_events_received\",\"events\":[{{\"event_id\":\"evt-1\",\"platform\":\"webhook\",\"event_type\":\"message\",\"received_at\":\"2026-04-12T00:00:00Z\",\"conversation\":{{\"id\":\"conv-1\",\"kind\":\"private\",\"thread_id\":\"thread-1\"}},\"actor\":{{\"id\":\"user-1\",\"is_bot\":false,\"metadata\":{{}}}},\"message\":{{\"id\":\"msg-1\",\"content\":\"hello\",\"content_type\":\"text/plain\",\"attachments\":[],\"metadata\":{{}}}},\"metadata\":{{}}}}],\"callback_reply\":{{\"status\":200,\"content_type\":\"text/plain; charset=utf-8\",\"body\":\"ok\\n\"}}}}'\n",
                "elif printf '%s' \"$line\" | grep -q '\"kind\":\"deliver\"'; then\n",
                "  printf '%s\\n' \"$line\" >> \"{}\"\n",
                "  printf '%s\\n' '{{\"kind\":\"delivered\",\"delivery\":{{\"message_id\":\"delivery-1\",\"conversation_id\":\"conv-1\",\"metadata\":{{\"delivered_by\":\"test\"}}}}}}'\n",
                "else\n",
                "  printf '%s\\n' '{{\"kind\":\"error\",\"error\":{{\"code\":\"unexpected_request\",\"message\":\"unhandled request\"}}}}'\n",
                "  exit 1\n",
                "fi\n"
            ),
            log_path.display()
        ),
    )?;
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&script_path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions)?;
    }

    let manifest_path = plugin_dir.join("channel-plugin.json");
    fs::write(
        &manifest_path,
        format!(
            r#"{{
    "kind": "channel",
    "name": "channel-reply-test",
    "version": "0.1",
    "protocol": "jsonl",
    "protocol_version": 1,
    "description": "Reply test channel plugin",
    "entrypoint": {{
        "command": "{}",
        "args": []
    }},
    "capabilities": {{
        "channel": {{
            "platform": "webhook",
            "allowed_paths": ["/hook"]
        }}
    }}
}}"#,
            script_path.display()
        ),
    )?;

    Ok(manifest_path)
}

#[cfg(unix)]
#[test]
fn detached_service_persists_terminal_status_after_sigterm()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    write_heartbeat_parcel(dir.path(), &[])?;

    let serve_output = require_success(
        run_dispatch(
            dir.path(),
            &[],
            &[
                "serve",
                ".",
                "--courier",
                "native",
                "--interval-ms",
                "60000",
                "--detach",
            ],
        )?,
        "dispatch serve --detach",
    )?;
    let record_path = PathBuf::from(extract_value("Record: ", &serve_output)?);

    let running = wait_for_run_record(&record_path, |record| record["status"] == "running")?;
    let pid = running["pid"].as_i64().ok_or("missing service pid")?;
    kill(Pid::from_raw(pid as i32), Signal::SIGTERM)?;

    let exited = wait_for_run_record(&record_path, |record| {
        record["status"] == "exited" && record["stopped_at_ms"].is_u64()
    })?;
    assert_eq!(exited["status"], "exited");
    assert_eq!(exited["exit_code"], 0);

    Ok(())
}

#[test]
fn detached_service_lifecycle_commands_work_end_to_end() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    write_heartbeat_parcel(
        dir.path(),
        &[
            "SECRET DISPATCH_WEBHOOK_SECRET",
            "LISTEN \"127.0.0.1:0\"",
            "LISTEN_PATH \"/hook\"",
            "LISTEN_METHOD POST",
            "LISTEN_SECRET DISPATCH_WEBHOOK_SECRET",
            "LISTEN_MAX_BODY_BYTES 1024",
            "LISTEN_MAX_HEADER_BYTES 1024",
        ],
    )?;

    let serve_output = require_success(
        run_dispatch(
            dir.path(),
            &[("DISPATCH_WEBHOOK_SECRET", "topsecret")],
            &[
                "serve",
                ".",
                "--courier",
                "native",
                "--interval-ms",
                "60000",
                "--detach",
            ],
        )?,
        "dispatch serve --detach",
    )?;
    let run_id = extract_value("Started service ", &serve_output)?;
    let record_path = PathBuf::from(extract_value("Record: ", &serve_output)?);

    let record = wait_for_run_record(&record_path, |record| {
        record["status"] == "running"
            && record["operation"]["listeners"][0]["bound_addr"].is_string()
    })?;
    let bound_addr = record["operation"]["listeners"][0]["bound_addr"]
        .as_str()
        .ok_or("missing bound listener address")?;

    let wrong_path = http_request_with_retry(
        bound_addr,
        "GET /wrong HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )?;
    assert!(wrong_path.starts_with("HTTP/1.1 404 Not Found"));

    let unauthorized = http_request_with_retry(
        bound_addr,
        "POST /hook HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )?;
    assert!(unauthorized.starts_with("HTTP/1.1 401 Unauthorized"));

    let authorized = http_request_with_retry(
        bound_addr,
        "POST /hook HTTP/1.1\r\nHost: localhost\r\nx-dispatch-secret: topsecret\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
    )?;
    assert!(authorized.starts_with("HTTP/1.1 202 Accepted"));

    let ps_output = require_success(
        run_dispatch(dir.path(), &[], &["ps", ".", "--json"])?,
        "dispatch ps --json",
    )?;
    let runs: Value = serde_json::from_str(&ps_output)?;
    assert!(
        runs.as_array()
            .ok_or("expected run inventory array")?
            .iter()
            .any(|run| run["run_id"] == run_id)
    );

    let logs_output = require_success(
        run_dispatch(dir.path(), &[], &["logs", &run_id, "."])?,
        "dispatch logs",
    )?;
    assert!(logs_output.contains("[redacted]"));
    assert!(!logs_output.contains("x-dispatch-secret\":\"topsecret"));

    let inspect_output = require_success(
        run_dispatch(dir.path(), &[], &["inspect-run", &run_id, ".", "--json"])?,
        "dispatch inspect-run --json",
    )?;
    let inspected: Value = serde_json::from_str(&inspect_output)?;
    assert_eq!(inspected["status"], "running");

    require_success(
        run_dispatch(
            dir.path(),
            &[],
            &["stop", &run_id, ".", "--grace-period-ms", "1"],
        )?,
        "dispatch stop",
    )?;
    let stopped = wait_for_run_record(&record_path, |record| record["status"] == "stopped")?;
    assert_eq!(stopped["status"], "stopped");
    let stopped_wait = require_success(
        run_dispatch(dir.path(), &[], &["wait", &run_id, "."])?,
        "dispatch wait after stop",
    )?;
    assert_eq!(stopped_wait.trim(), "1");

    let restart_output = require_success(
        run_dispatch(dir.path(), &[], &["restart", &run_id, ".", "--force"])?,
        "dispatch restart",
    )?;
    assert!(restart_output.contains(&format!("Restarted run {run_id}")));
    let restarted = wait_for_run_record(&record_path, |record| {
        record["status"] == "running"
            && record["operation"]["listeners"][0]["bound_addr"].is_string()
    })?;
    assert_eq!(restarted["status"], "running");
    let restarted_bound_addr = restarted["operation"]["listeners"][0]["bound_addr"]
        .as_str()
        .ok_or("missing restarted bound listener address")?;
    let restarted_authorized = http_request_with_retry(
        restarted_bound_addr,
        "POST /hook HTTP/1.1\r\nHost: localhost\r\nx-dispatch-secret: topsecret\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
    )?;
    assert!(restarted_authorized.starts_with("HTTP/1.1 202 Accepted"));
    let restarted_after_request = wait_for_run_record(&record_path, |record| {
        record["status"] == "running"
            && record["operation"]["listeners"][0]["requests_handled"]
                .as_u64()
                .unwrap_or_default()
                >= 1
    })?;
    assert_eq!(
        restarted_after_request["operation"]["listeners"][0]["requests_handled"],
        1
    );

    require_success(
        run_dispatch(dir.path(), &[], &["rm", &run_id, ".", "--force"])?,
        "dispatch rm --force",
    )?;
    assert!(!record_path.exists());
    assert!(!PathBuf::from(extract_value("Log: ", &serve_output)?).exists());
    assert!(
        !PathBuf::from(
            stopped["session_file"]
                .as_str()
                .ok_or("missing session file")?
        )
        .exists()
    );

    Ok(())
}

#[test]
fn channel_listen_handles_http_request_end_to_end() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    let registry_path = dir.path().join("channels.json");
    let manifest_path = write_channel_test_plugin(dir.path())?;

    require_success(
        run_dispatch(
            dir.path(),
            &[],
            &[
                "channel",
                "install",
                manifest_path
                    .to_str()
                    .ok_or("manifest path is not valid UTF-8")?,
                "--registry",
                registry_path
                    .to_str()
                    .ok_or("registry path is not valid UTF-8")?,
            ],
        )?,
        "dispatch channel install",
    )?;

    let listen_addr = reserve_loopback_addr()?;
    let dispatch_bin = dispatch_bin();
    let child = Command::new(&dispatch_bin)
        .current_dir(dir.path())
        .args([
            "channel",
            "listen",
            "channel-test",
            "--listen",
            &listen_addr,
            "--once",
            "--json",
            "--registry",
            registry_path
                .to_str()
                .ok_or("registry path is not valid UTF-8")?,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let response = http_request_with_retry(
        &listen_addr,
        "POST /hook?conversation_id=abc HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
    )?;
    assert!(response.starts_with("HTTP/1.1 202 Accepted"));
    assert!(response.ends_with("accepted\n"));

    let output = child.wait_with_output()?;
    let stdout = require_success(output, "dispatch channel listen --once")?;
    let json_line = stdout
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with('{'))
        .ok_or_else(|| format!("missing JSON payload in output:\n{stdout}"))?;
    let event_output: Value = serde_json::from_str(json_line)?;
    assert_eq!(event_output["plugin"], "channel-test");
    assert_eq!(event_output["events"][0]["event_id"], "evt-1");
    assert_eq!(event_output["parcel_runs"], Value::Array(Vec::new()));

    Ok(())
}

#[test]
fn dispatch_up_uses_project_local_channel_registry() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    let manifest_path = write_channel_test_plugin(dir.path())?;
    let listen_addr = reserve_loopback_addr()?;
    fs::write(
        dir.path().join("dispatch.toml"),
        format!(
            r#"
[[extensions]]
manifest = "{}"

[[channels]]
plugin = "channel-test"
mode = "listen"
listen = "{}"
once = true
"#,
            manifest_path.display(),
            listen_addr,
        ),
    )?;

    let dispatch_bin = dispatch_bin();
    let child = Command::new(&dispatch_bin)
        .current_dir(dir.path())
        .args(["up", "dispatch.toml"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let response = http_request_with_retry(
        &listen_addr,
        "POST /hook HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
    )?;
    assert!(response.starts_with("HTTP/1.1 202 Accepted"));

    let stdout = require_success(child.wait_with_output()?, "dispatch up")?;
    assert!(stdout.contains("Installed channel plugin `channel-test`"));

    let registry_path = dir.path().join(".dispatch/registries/channels.json");
    assert!(registry_path.exists());
    let registry: Value = serde_json::from_slice(&fs::read(registry_path)?)?;
    assert_eq!(registry["plugins"][0]["name"], "channel-test");

    Ok(())
}

#[test]
fn dispatch_up_loads_channel_toml_config_file() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    let request_log = dir.path().join("request.json");
    let manifest_path = write_logging_channel_test_plugin(dir.path(), &request_log)?;
    let listen_addr = reserve_loopback_addr()?;
    fs::write(
        dir.path().join("channel-config.toml"),
        r#"
bot_token_env = "TELEGRAM_BOT_TOKEN"

[delivery]
mode = "reply"
"#,
    )?;
    fs::write(
        dir.path().join("dispatch.toml"),
        format!(
            r#"
[[extensions]]
manifest = "{}"

[[channels]]
plugin = "channel-query-test"
mode = "listen"
listen = "{}"
once = true
config_file = "./channel-config.toml"
"#,
            manifest_path.display(),
            listen_addr,
        ),
    )?;

    let dispatch_bin = dispatch_bin();
    let child = Command::new(&dispatch_bin)
        .current_dir(dir.path())
        .args(["up", "dispatch.toml"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let response = http_request_with_retry(
        &listen_addr,
        "POST /hook HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )?;
    assert!(response.contains("ok\n"), "unexpected response: {response}");

    let _ = require_success(child.wait_with_output()?, "dispatch up")?;

    let logged_request: Value = serde_json::from_slice(&fs::read(&request_log)?)?;
    assert_eq!(
        logged_request["request"]["config"]["bot_token_env"],
        "TELEGRAM_BOT_TOKEN"
    );
    assert_eq!(
        logged_request["request"]["config"]["delivery"]["mode"],
        "reply"
    );

    Ok(())
}

#[test]
fn dispatch_up_dry_run_does_not_install_or_start_channels() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = tempdir()?;
    let manifest_path = write_channel_test_plugin(dir.path())?;
    fs::write(
        dir.path().join("dispatch.toml"),
        format!(
            r#"
[[extensions]]
manifest = "{}"

[[channels]]
plugin = "channel-test"
mode = "poll"
once = true
"#,
            manifest_path.display(),
        ),
    )?;

    let stdout = require_success(
        run_dispatch(dir.path(), &[], &["up", "dispatch.toml", "--dry-run"])?,
        "dispatch up --dry-run",
    )?;

    assert!(stdout.contains("Dry Run: yes"));
    assert!(stdout.contains("channel-test"));
    assert!(
        stdout.contains("Courier Status: `native` resolves"),
        "expected dry-run to report built-in courier resolution; got:\n{stdout}"
    );
    assert!(
        !dir.path()
            .join(".dispatch/registries/channels.json")
            .exists()
    );

    Ok(())
}

#[test]
fn dispatch_up_dry_run_flags_unresolvable_courier() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    let manifest_path = write_channel_test_plugin(dir.path())?;
    fs::write(
        dir.path().join("dispatch.toml"),
        format!(
            r#"
courier = "ghost-cloud"

[[extensions]]
manifest = "{}"

[[channels]]
plugin = "channel-test"
mode = "poll"
once = true
"#,
            manifest_path.display(),
        ),
    )?;

    let stdout = require_success(
        run_dispatch(dir.path(), &[], &["up", "dispatch.toml", "--dry-run"])?,
        "dispatch up --dry-run",
    )?;

    assert!(
        stdout.contains("Courier Status: `ghost-cloud` does not resolve"),
        "expected dry-run to flag unresolvable courier; got:\n{stdout}"
    );

    Ok(())
}

#[test]
fn dispatch_up_dry_run_only_claims_matching_courier_will_install()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    let channel_manifest = write_channel_test_plugin(dir.path())?;
    let courier_manifest = write_test_courier_plugin_manifest(dir.path(), "different-cloud")?;
    fs::write(
        dir.path().join("dispatch.toml"),
        format!(
            r#"
courier = "ghost-cloud"

[[extensions]]
manifest = "{}"

[[extensions]]
manifest = "{}"

[[channels]]
plugin = "channel-test"
mode = "poll"
once = true
"#,
            channel_manifest.display(),
            courier_manifest.display(),
        ),
    )?;

    let stdout = require_success(
        run_dispatch(dir.path(), &[], &["up", "dispatch.toml", "--dry-run"])?,
        "dispatch up --dry-run",
    )?;

    assert!(
        stdout.contains("Courier Status: `ghost-cloud` does not resolve"),
        "expected dry-run to keep reporting the missing configured courier; got:\n{stdout}"
    );
    assert!(
        !stdout.contains("will be installed via [[extensions]]"),
        "expected dry-run not to claim an unrelated courier extension satisfies the configured courier; got:\n{stdout}"
    );

    Ok(())
}

#[test]
fn dispatch_up_dry_run_reports_matching_courier_will_install()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    let channel_manifest = write_channel_test_plugin(dir.path())?;
    let courier_manifest = write_test_courier_plugin_manifest(dir.path(), "ghost-cloud")?;
    fs::write(
        dir.path().join("dispatch.toml"),
        format!(
            r#"
courier = "ghost-cloud"

[[extensions]]
manifest = "{}"

[[extensions]]
manifest = "{}"

[[channels]]
plugin = "channel-test"
mode = "poll"
once = true
"#,
            channel_manifest.display(),
            courier_manifest.display(),
        ),
    )?;

    let stdout = require_success(
        run_dispatch(dir.path(), &[], &["up", "dispatch.toml", "--dry-run"])?,
        "dispatch up --dry-run",
    )?;

    assert!(
        stdout.contains("Courier Status: `ghost-cloud` will be installed via [[extensions]]"),
        "expected dry-run to report that the configured courier will be installed; got:\n{stdout}"
    );

    Ok(())
}

#[test]
fn channel_listen_decodes_query_params_before_plugin_call() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = tempdir()?;
    let registry_path = dir.path().join("channels.json");
    let request_log = dir.path().join("request.json");
    let manifest_path = write_logging_channel_test_plugin(dir.path(), &request_log)?;

    require_success(
        run_dispatch(
            dir.path(),
            &[],
            &[
                "channel",
                "install",
                manifest_path
                    .to_str()
                    .ok_or("manifest path is not valid UTF-8")?,
                "--registry",
                registry_path
                    .to_str()
                    .ok_or("registry path is not valid UTF-8")?,
            ],
        )?,
        "dispatch channel install",
    )?;

    let listen_addr = reserve_loopback_addr()?;
    let dispatch_bin = dispatch_bin();
    let child = Command::new(&dispatch_bin)
        .current_dir(dir.path())
        .args([
            "channel",
            "listen",
            "channel-query-test",
            "--listen",
            &listen_addr,
            "--once",
            "--registry",
            registry_path
                .to_str()
                .ok_or("registry path is not valid UTF-8")?,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let response = http_request_with_retry(
        &listen_addr,
        "POST /hook?subject=hello%20world&name=dispatch%2Bbot&flag HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )?;
    assert!(response.contains("ok\n"), "unexpected response: {response}");

    let output = child.wait_with_output()?;
    let _ = require_success(output, "dispatch channel listen query decode --once")?;

    let logged_request: Value = serde_json::from_slice(&fs::read(&request_log)?)?;
    let payload = &logged_request["request"]["payload"];
    assert_eq!(
        payload["raw_query"],
        "subject=hello%20world&name=dispatch%2Bbot&flag"
    );
    assert_eq!(payload["query"]["subject"], "hello world");
    assert_eq!(payload["query"]["name"], "dispatch+bot");
    assert_eq!(payload["query"]["flag"], "");

    Ok(())
}

#[test]
fn channel_listen_calls_start_and_stop_ingress() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    let registry_path = dir.path().join("channels.json");
    let lifecycle_log = dir.path().join("lifecycle.log");
    let manifest_path = write_lifecycle_channel_test_plugin(dir.path(), &lifecycle_log)?;

    require_success(
        run_dispatch(
            dir.path(),
            &[],
            &[
                "channel",
                "install",
                manifest_path
                    .to_str()
                    .ok_or("manifest path is not valid UTF-8")?,
                "--registry",
                registry_path
                    .to_str()
                    .ok_or("registry path is not valid UTF-8")?,
            ],
        )?,
        "dispatch channel install",
    )?;

    let listen_addr = reserve_loopback_addr()?;
    let dispatch_bin = dispatch_bin();
    let child = Command::new(&dispatch_bin)
        .current_dir(dir.path())
        .args([
            "channel",
            "listen",
            "channel-lifecycle-test",
            "--listen",
            &listen_addr,
            "--once",
            "--registry",
            registry_path
                .to_str()
                .ok_or("registry path is not valid UTF-8")?,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let response = http_request_with_retry(
        &listen_addr,
        "POST /hook HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )?;
    assert!(response.starts_with("HTTP/1.1 200 OK"));

    let output = child.wait_with_output()?;
    let _ = require_success(output, "dispatch channel listen lifecycle --once")?;

    let logged_requests = fs::read_to_string(&lifecycle_log)?;
    let request_lines = logged_requests.lines().collect::<Vec<_>>();
    assert_eq!(
        request_lines.len(),
        3,
        "unexpected lifecycle log:\n{logged_requests}"
    );

    let start_request: Value = serde_json::from_str(request_lines[0])?;
    let ingress_request: Value = serde_json::from_str(request_lines[1])?;
    let stop_request: Value = serde_json::from_str(request_lines[2])?;

    assert_eq!(start_request["request"]["kind"], "start_ingress");
    assert_eq!(ingress_request["request"]["kind"], "ingress_event");
    assert_eq!(stop_request["request"]["kind"], "stop_ingress");
    assert_eq!(stop_request["request"]["state"]["status"], "registered");
    assert_eq!(
        stop_request["request"]["state"]["endpoint"],
        "https://example.test/hook"
    );

    Ok(())
}

#[test]
fn channel_poll_calls_start_poll_and_stop_ingress() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    let registry_path = dir.path().join("channels.json");
    let poll_log = dir.path().join("poll.log");
    let manifest_path = write_polling_channel_test_plugin(dir.path(), &poll_log)?;

    require_success(
        run_dispatch(
            dir.path(),
            &[],
            &[
                "channel",
                "install",
                manifest_path
                    .to_str()
                    .ok_or("manifest path is not valid UTF-8")?,
                "--registry",
                registry_path
                    .to_str()
                    .ok_or("registry path is not valid UTF-8")?,
            ],
        )?,
        "dispatch channel install",
    )?;

    let stdout = require_success(
        run_dispatch(
            dir.path(),
            &[],
            &[
                "channel",
                "poll",
                "channel-polling-test",
                "--once",
                "--json",
                "--registry",
                registry_path
                    .to_str()
                    .ok_or("registry path is not valid UTF-8")?,
            ],
        )?,
        "dispatch channel poll --once",
    )?;

    let json_line = stdout
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with('{'))
        .ok_or_else(|| format!("missing JSON payload in output:\n{stdout}"))?;
    let poll_output: Value = serde_json::from_str(json_line)?;
    assert_eq!(poll_output["plugin"], "channel-polling-test");
    assert_eq!(poll_output["events"][0]["event_id"], "poll-evt-1");
    assert_eq!(poll_output["state"]["metadata"]["cursor"], "1");
    assert_eq!(poll_output["poll_after_ms"], 25);

    let logged_requests = fs::read_to_string(&poll_log)?;
    let request_lines = logged_requests.lines().collect::<Vec<_>>();
    assert_eq!(
        request_lines.len(),
        3,
        "unexpected poll log:\n{logged_requests}"
    );

    let start_request: Value = serde_json::from_str(request_lines[0])?;
    let poll_request: Value = serde_json::from_str(request_lines[1])?;
    let stop_request: Value = serde_json::from_str(request_lines[2])?;

    assert_eq!(start_request["request"]["kind"], "start_ingress");
    assert_eq!(poll_request["request"]["kind"], "poll_ingress");
    assert_eq!(stop_request["request"]["kind"], "stop_ingress");
    assert_eq!(poll_request["request"]["state"]["metadata"]["cursor"], "0");
    assert_eq!(stop_request["request"]["state"]["metadata"]["cursor"], "1");

    Ok(())
}

#[test]
fn channel_listen_rejects_polling_plugins_and_stops_ingress()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    let registry_path = dir.path().join("channels.json");
    let poll_log = dir.path().join("poll.log");
    let manifest_path = write_polling_channel_test_plugin(dir.path(), &poll_log)?;

    require_success(
        run_dispatch(
            dir.path(),
            &[],
            &[
                "channel",
                "install",
                manifest_path
                    .to_str()
                    .ok_or("manifest path is not valid UTF-8")?,
                "--registry",
                registry_path
                    .to_str()
                    .ok_or("registry path is not valid UTF-8")?,
            ],
        )?,
        "dispatch channel install",
    )?;

    let listen_addr = reserve_loopback_addr()?;
    let output = run_dispatch(
        dir.path(),
        &[],
        &[
            "channel",
            "listen",
            "channel-polling-test",
            "--listen",
            &listen_addr,
            "--once",
            "--registry",
            registry_path
                .to_str()
                .ok_or("registry path is not valid UTF-8")?,
        ],
    )?;

    let (stdout, stderr) = output_text(&output);
    assert!(
        !output.status.success(),
        "dispatch channel listen unexpectedly succeeded\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("poll") && stderr.contains("instead of listen"),
        "stderr did not mention polling guidance\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let logged_requests = fs::read_to_string(&poll_log)?;
    let request_lines = logged_requests.lines().collect::<Vec<_>>();
    assert_eq!(
        request_lines.len(),
        2,
        "unexpected poll log:\n{logged_requests}"
    );

    let start_request: Value = serde_json::from_str(request_lines[0])?;
    let stop_request: Value = serde_json::from_str(request_lines[1])?;
    assert_eq!(start_request["request"]["kind"], "start_ingress");
    assert_eq!(stop_request["request"]["kind"], "stop_ingress");

    Ok(())
}

#[test]
fn channel_inspect_json_reports_plugin_and_fixed_timeout() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = tempdir()?;
    let registry_path = dir.path().join("channels.json");
    let manifest_path = write_channel_test_plugin(dir.path())?;

    require_success(
        run_dispatch(
            dir.path(),
            &[],
            &[
                "channel",
                "install",
                manifest_path
                    .to_str()
                    .ok_or("manifest path is not valid UTF-8")?,
                "--registry",
                registry_path
                    .to_str()
                    .ok_or("registry path is not valid UTF-8")?,
            ],
        )?,
        "dispatch channel install",
    )?;

    let stdout = require_success(
        run_dispatch(
            dir.path(),
            &[],
            &[
                "channel",
                "inspect",
                "channel-test",
                "--json",
                "--registry",
                registry_path
                    .to_str()
                    .ok_or("registry path is not valid UTF-8")?,
            ],
        )?,
        "dispatch channel inspect --json",
    )?;

    let inspected: Value = serde_json::from_str(&stdout)?;
    assert_eq!(inspected["plugin"]["name"], "channel-test");
    assert_eq!(inspected["plugin"]["platform"], "webhook");
    assert_eq!(inspected["call_timeout_ms"], 30_000);
    assert_eq!(inspected["call_timeout_display"], "30s");

    Ok(())
}

#[test]
fn channel_inspect_text_reports_fixed_timeout() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    let registry_path = dir.path().join("channels.json");
    let manifest_path = write_channel_test_plugin(dir.path())?;

    require_success(
        run_dispatch(
            dir.path(),
            &[],
            &[
                "channel",
                "install",
                manifest_path
                    .to_str()
                    .ok_or("manifest path is not valid UTF-8")?,
                "--registry",
                registry_path
                    .to_str()
                    .ok_or("registry path is not valid UTF-8")?,
            ],
        )?,
        "dispatch channel install",
    )?;

    let stdout = require_success(
        run_dispatch(
            dir.path(),
            &[],
            &[
                "channel",
                "inspect",
                "channel-test",
                "--registry",
                registry_path
                    .to_str()
                    .ok_or("registry path is not valid UTF-8")?,
            ],
        )?,
        "dispatch channel inspect",
    )?;

    assert!(stdout.contains("Name: channel-test"));
    assert!(stdout.contains("Platform: webhook"));
    assert!(stdout.contains("Call Timeout: 30s"));

    Ok(())
}

#[test]
fn channel_listen_rejects_bad_host_managed_secret_before_plugin_invocation()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    let registry_path = dir.path().join("channels.json");
    let invocation_log = dir.path().join("plugin-invocations.log");
    let manifest_path = write_trusted_channel_test_plugin(
        dir.path(),
        "DISPATCH_TEST_CHANNEL_SECRET",
        &invocation_log,
    )?;

    require_success(
        run_dispatch(
            dir.path(),
            &[],
            &[
                "channel",
                "install",
                manifest_path
                    .to_str()
                    .ok_or("manifest path is not valid UTF-8")?,
                "--registry",
                registry_path
                    .to_str()
                    .ok_or("registry path is not valid UTF-8")?,
            ],
        )?,
        "dispatch channel install",
    )?;

    let listen_addr = reserve_loopback_addr()?;
    let dispatch_bin = dispatch_bin();
    let child = Command::new(&dispatch_bin)
        .current_dir(dir.path())
        .env("DISPATCH_TEST_CHANNEL_SECRET", "expected-secret")
        .args([
            "channel",
            "listen",
            "channel-trusted-test",
            "--listen",
            &listen_addr,
            "--once",
            "--registry",
            registry_path
                .to_str()
                .ok_or("registry path is not valid UTF-8")?,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let response = http_request_with_retry(
        &listen_addr,
        "POST /hook HTTP/1.1\r\nHost: localhost\r\nX-Dispatch-Secret: wrong-secret\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
    )?;
    assert!(response.starts_with("HTTP/1.1 403 Forbidden"));
    assert!(response.contains("did not match"));

    let output = child.wait_with_output()?;
    let _ = require_success(output, "dispatch channel listen --once")?;
    assert!(
        !invocation_log.exists(),
        "plugin should not have been invoked"
    );

    Ok(())
}

#[test]
fn channel_listen_delivers_replies_through_plugin() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    let channel_registry_path = dir.path().join("channels.json");
    let courier_registry_path = dir.path().join("couriers.json");
    let delivery_log = dir.path().join("deliver.log");
    let channel_manifest_path = write_reply_channel_test_plugin(dir.path(), &delivery_log)?;
    let (parcel_dir, parcel_digest) = build_chat_test_parcel(dir.path())?;
    install_test_courier_plugin(
        dir.path(),
        &courier_registry_path,
        "listener-test-courier",
        &parcel_digest,
    )?;

    require_success(
        run_dispatch(
            dir.path(),
            &[],
            &[
                "channel",
                "install",
                channel_manifest_path
                    .to_str()
                    .ok_or("channel manifest path is not valid UTF-8")?,
                "--registry",
                channel_registry_path
                    .to_str()
                    .ok_or("channel registry path is not valid UTF-8")?,
            ],
        )?,
        "dispatch channel install",
    )?;

    let listen_addr = reserve_loopback_addr()?;
    let dispatch_bin = dispatch_bin();
    let child = Command::new(&dispatch_bin)
        .current_dir(dir.path())
        .args([
            "channel",
            "listen",
            "channel-reply-test",
            "--listen",
            &listen_addr,
            "--once",
            "--json",
            "--parcel",
            parcel_dir
                .to_str()
                .ok_or("parcel path is not valid UTF-8")?,
            "--courier",
            "listener-test-courier",
            "--courier-registry",
            courier_registry_path
                .to_str()
                .ok_or("courier registry path is not valid UTF-8")?,
            "--deliver-replies",
            "--registry",
            channel_registry_path
                .to_str()
                .ok_or("channel registry path is not valid UTF-8")?,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let response = http_request_with_retry(
        &listen_addr,
        "POST /hook HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
    )?;
    assert!(response.starts_with("HTTP/1.1 200 OK"));

    let output = child.wait_with_output()?;
    let stdout = require_success(output, "dispatch channel listen --deliver-replies")?;
    let json_line = stdout
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with('{'))
        .ok_or_else(|| format!("missing JSON payload in output:\n{stdout}"))?;
    let event_output: Value = serde_json::from_str(json_line)?;
    assert_eq!(event_output["plugin"], "channel-reply-test");
    assert_eq!(event_output["parcel_runs"][0]["event_id"], "evt-1");
    assert_eq!(
        event_output["parcel_runs"][0]["delivery"]["message_id"],
        "delivery-1"
    );

    let logged_request = fs::read_to_string(&delivery_log)?;
    let deliver_envelope: Value = serde_json::from_str(logged_request.trim())?;
    assert_eq!(deliver_envelope["request"]["kind"], "deliver");
    assert_eq!(
        deliver_envelope["request"]["message"]["content"],
        "plugin reply"
    );
    assert_eq!(
        deliver_envelope["request"]["message"]["metadata"]["conversation_id"],
        "conv-1"
    );
    assert_eq!(
        deliver_envelope["request"]["message"]["metadata"]["thread_id"],
        "thread-1"
    );
    assert_eq!(
        deliver_envelope["request"]["message"]["metadata"]["reply_to_message_id"],
        "msg-1"
    );

    Ok(())
}

#[test]
fn channel_listen_rejects_deliver_replies_without_parcel() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = tempdir()?;
    let registry_path = dir.path().join("channels.json");
    let manifest_path = write_channel_test_plugin(dir.path())?;

    require_success(
        run_dispatch(
            dir.path(),
            &[],
            &[
                "channel",
                "install",
                manifest_path
                    .to_str()
                    .ok_or("manifest path is not valid UTF-8")?,
                "--registry",
                registry_path
                    .to_str()
                    .ok_or("registry path is not valid UTF-8")?,
            ],
        )?,
        "dispatch channel install",
    )?;

    let output = run_dispatch(
        dir.path(),
        &[],
        &[
            "channel",
            "listen",
            "channel-test",
            "--listen",
            "127.0.0.1:8787",
            "--deliver-replies",
            "--registry",
            registry_path
                .to_str()
                .ok_or("registry path is not valid UTF-8")?,
        ],
    )?;
    let (stdout, stderr) = output_text(&output);
    assert!(
        !output.status.success(),
        "dispatch channel listen unexpectedly succeeded\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stderr.contains("reply delivery requires a parcel"));
    assert!(stderr.contains("--parcel"));

    Ok(())
}

#[test]
fn channel_listen_delivers_structured_channel_reply_envelope()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    let channel_registry_path = dir.path().join("channels.json");
    let courier_registry_path = dir.path().join("couriers.json");
    let delivery_log = dir.path().join("deliver.log");
    let channel_manifest_path = write_reply_channel_test_plugin(dir.path(), &delivery_log)?;
    let (parcel_dir, parcel_digest) = build_chat_test_parcel(dir.path())?;
    let assistant_reply = serde_json::json!({
        "kind": "channel_reply",
        "content": "plugin reply with attachment",
        "content_type": "text/plain",
        "attachments": [{
            "name": "report.txt",
            "mime_type": "text/plain",
            "data_base64": "aGVsbG8="
        }],
        "metadata": {
            "custom": "value"
        }
    })
    .to_string();
    install_test_courier_plugin_with_reply(
        dir.path(),
        &courier_registry_path,
        "listener-test-courier",
        &parcel_digest,
        &assistant_reply,
    )?;

    require_success(
        run_dispatch(
            dir.path(),
            &[],
            &[
                "channel",
                "install",
                channel_manifest_path
                    .to_str()
                    .ok_or("channel manifest path is not valid UTF-8")?,
                "--registry",
                channel_registry_path
                    .to_str()
                    .ok_or("channel registry path is not valid UTF-8")?,
            ],
        )?,
        "dispatch channel install",
    )?;

    let listen_addr = reserve_loopback_addr()?;
    let dispatch_bin = dispatch_bin();
    let child = Command::new(&dispatch_bin)
        .current_dir(dir.path())
        .args([
            "channel",
            "listen",
            "channel-reply-test",
            "--listen",
            &listen_addr,
            "--once",
            "--json",
            "--parcel",
            parcel_dir
                .to_str()
                .ok_or("parcel path is not valid UTF-8")?,
            "--courier",
            "listener-test-courier",
            "--courier-registry",
            courier_registry_path
                .to_str()
                .ok_or("courier registry path is not valid UTF-8")?,
            "--deliver-replies",
            "--registry",
            channel_registry_path
                .to_str()
                .ok_or("channel registry path is not valid UTF-8")?,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let response = http_request_with_retry(
        &listen_addr,
        "POST /hook HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
    )?;
    assert!(response.starts_with("HTTP/1.1 200 OK"));

    let output = child.wait_with_output()?;
    let stdout = require_success(output, "dispatch channel listen --deliver-replies")?;
    let json_line = stdout
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with('{'))
        .ok_or_else(|| format!("missing JSON payload in output:\n{stdout}"))?;
    let event_output: Value = serde_json::from_str(json_line)?;
    assert_eq!(event_output["plugin"], "channel-reply-test");
    assert_eq!(
        event_output["parcel_runs"][0]["delivery"]["message_id"],
        "delivery-1"
    );

    let logged_request = fs::read_to_string(&delivery_log)?;
    let deliver_envelope: Value = serde_json::from_str(logged_request.trim())?;
    assert_eq!(deliver_envelope["request"]["kind"], "deliver");
    assert_eq!(
        deliver_envelope["request"]["message"]["content"],
        "plugin reply with attachment"
    );
    assert_eq!(
        deliver_envelope["request"]["message"]["attachments"][0]["name"],
        "report.txt"
    );
    assert_eq!(
        deliver_envelope["request"]["message"]["attachments"][0]["data_base64"],
        "aGVsbG8="
    );
    assert_eq!(
        deliver_envelope["request"]["message"]["metadata"]["custom"],
        "value"
    );
    assert_eq!(
        deliver_envelope["request"]["message"]["metadata"]["conversation_id"],
        "conv-1"
    );
    assert_eq!(
        deliver_envelope["request"]["message"]["metadata"]["thread_id"],
        "thread-1"
    );
    assert_eq!(
        deliver_envelope["request"]["message"]["metadata"]["reply_to_message_id"],
        "msg-1"
    );

    Ok(())
}

#[test]
fn channel_listen_delivers_first_class_channel_reply_event()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    let channel_registry_path = dir.path().join("channels.json");
    let courier_registry_path = dir.path().join("couriers.json");
    let delivery_log = dir.path().join("deliver.log");
    let channel_manifest_path = write_reply_channel_test_plugin(dir.path(), &delivery_log)?;
    let (parcel_dir, parcel_digest) = build_chat_test_parcel(dir.path())?;
    install_test_courier_plugin_with_event(
        dir.path(),
        &courier_registry_path,
        "listener-test-courier",
        &parcel_digest,
        serde_json::json!({
            "kind": "channel_reply",
            "message": {
                "content": "plugin reply with attachment",
                "content_type": "text/plain",
                "attachments": [{
                    "name": "report.txt",
                    "mime_type": "text/plain",
                    "data_base64": "aGVsbG8="
                }],
                "metadata": {
                    "custom": "value"
                }
            }
        }),
        "plugin reply with attachment",
    )?;

    require_success(
        run_dispatch(
            dir.path(),
            &[],
            &[
                "channel",
                "install",
                channel_manifest_path
                    .to_str()
                    .ok_or("channel manifest path is not valid UTF-8")?,
                "--registry",
                channel_registry_path
                    .to_str()
                    .ok_or("channel registry path is not valid UTF-8")?,
            ],
        )?,
        "dispatch channel install",
    )?;

    let listen_addr = reserve_loopback_addr()?;
    let dispatch_bin = dispatch_bin();
    let child = Command::new(&dispatch_bin)
        .current_dir(dir.path())
        .args([
            "channel",
            "listen",
            "channel-reply-test",
            "--listen",
            &listen_addr,
            "--once",
            "--json",
            "--parcel",
            parcel_dir
                .to_str()
                .ok_or("parcel path is not valid UTF-8")?,
            "--courier",
            "listener-test-courier",
            "--courier-registry",
            courier_registry_path
                .to_str()
                .ok_or("courier registry path is not valid UTF-8")?,
            "--deliver-replies",
            "--registry",
            channel_registry_path
                .to_str()
                .ok_or("channel registry path is not valid UTF-8")?,
        ])
        .spawn()?;

    let request = concat!(
        "POST /hook HTTP/1.1\r\n",
        "Host: example.test\r\n",
        "Content-Type: application/json\r\n",
        "Content-Length: 17\r\n",
        "\r\n",
        "{\"hello\":\"world\"}"
    );
    let response = http_request_with_retry(&listen_addr, request)?;
    assert!(response.starts_with("HTTP/1.1 200 OK"));

    let status = child.wait_with_output()?;
    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        return Err(format!("dispatch channel listen failed:\n{stderr}").into());
    }

    let deliver_envelope = wait_for_run_record(&delivery_log, |record| {
        record["request"]["kind"] == "deliver"
    })?;
    assert_eq!(
        deliver_envelope["request"]["message"]["attachments"][0]["name"],
        "report.txt"
    );
    assert_eq!(
        deliver_envelope["request"]["message"]["metadata"]["custom"],
        "value"
    );
    assert_eq!(
        deliver_envelope["request"]["message"]["metadata"]["reply_to_message_id"],
        "msg-1"
    );

    Ok(())
}

#[test]
fn detached_heartbeat_wait_returns_exit_code() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    write_heartbeat_parcel(dir.path(), &[])?;

    let run_output = require_success(
        run_dispatch(dir.path(), &[], &["run", ".", "--heartbeat", "--detach"])?,
        "dispatch run --heartbeat --detach",
    )?;
    let run_id = extract_value("Started run ", &run_output)?;
    let record_path = PathBuf::from(extract_value("Record: ", &run_output)?);

    let wait_output = require_success(
        run_dispatch(dir.path(), &[], &["wait", &run_id, "."])?,
        "dispatch wait",
    )?;
    assert_eq!(wait_output.trim(), "0");

    let record = wait_for_run_record(&record_path, |record| record["status"] == "exited")?;
    assert_eq!(record["exit_code"], 0);

    require_success(
        run_dispatch(dir.path(), &[], &["rm", &run_id, ".", "--force"])?,
        "dispatch rm after wait",
    )?;
    Ok(())
}

#[test]
fn wait_timeout_returns_error_for_active_service() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    write_heartbeat_parcel(dir.path(), &["LISTEN \"127.0.0.1:0\""])?;

    let serve_output = require_success(
        run_dispatch(
            dir.path(),
            &[],
            &[
                "serve",
                ".",
                "--courier",
                "native",
                "--interval-ms",
                "60000",
                "--detach",
            ],
        )?,
        "dispatch serve --detach",
    )?;
    let run_id = extract_value("Started service ", &serve_output)?;
    let record_path = PathBuf::from(extract_value("Record: ", &serve_output)?);

    let _running = wait_for_run_record(&record_path, |record| {
        record["status"] == "running" && record["pid"].is_u64()
    })?;

    let wait_output = run_dispatch(
        dir.path(),
        &[],
        &["wait", &run_id, ".", "--timeout-ms", "100"],
    )?;
    let (stdout, stderr) = output_text(&wait_output);
    assert!(
        !wait_output.status.success(),
        "wait unexpectedly succeeded\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("timed out waiting for run"),
        "unexpected wait timeout stderr:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    require_success(
        run_dispatch(dir.path(), &[], &["rm", &run_id, ".", "--force"])?,
        "dispatch rm after timed wait",
    )?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn inspect_run_reconciles_dead_service_helpers() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    write_heartbeat_parcel(dir.path(), &["LISTEN \"127.0.0.1:0\""])?;

    let serve_output = require_success(
        run_dispatch(
            dir.path(),
            &[],
            &[
                "serve",
                ".",
                "--courier",
                "native",
                "--interval-ms",
                "60000",
                "--detach",
            ],
        )?,
        "dispatch serve --detach",
    )?;
    let run_id = extract_value("Started service ", &serve_output)?;
    let record_path = PathBuf::from(extract_value("Record: ", &serve_output)?);

    let record = wait_for_run_record(&record_path, |record| record["status"] == "running")?;
    let pid = record["pid"].as_u64().ok_or("missing pid")? as i32;

    kill(Pid::from_raw(pid), Signal::SIGKILL)?;

    let inspect_output = require_success(
        run_dispatch(dir.path(), &[], &["inspect-run", &run_id, ".", "--json"])?,
        "dispatch inspect-run after kill",
    )?;
    let inspected: Value = serde_json::from_str(&inspect_output)?;
    assert_eq!(inspected["status"], "stopped");

    require_success(
        run_dispatch(dir.path(), &[], &["rm", &run_id, ".", "--force"])?,
        "dispatch rm after kill",
    )?;
    assert!(!record_path.exists());

    Ok(())
}
