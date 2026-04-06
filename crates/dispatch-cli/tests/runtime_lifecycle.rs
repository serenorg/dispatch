use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};
use tempfile::tempdir;

fn dispatch_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dispatch")
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
    let mut command = Command::new(dispatch_bin());
    command.current_dir(cwd).args(args);
    for (name, value) in envs {
        command.env(name, value);
    }
    Ok(command.output()?)
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

    let wrong_path = http_request(
        bound_addr,
        "GET /wrong HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )?;
    assert!(wrong_path.starts_with("HTTP/1.1 404 Not Found"));

    let unauthorized = http_request(
        bound_addr,
        "POST /hook HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )?;
    assert!(unauthorized.starts_with("HTTP/1.1 401 Unauthorized"));

    let authorized = http_request(
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
        run_dispatch(dir.path(), &[], &["stop", &run_id, "."])?,
        "dispatch stop",
    )?;
    let stopped = wait_for_run_record(&record_path, |record| record["status"] == "stopped")?;
    assert_eq!(stopped["status"], "stopped");

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

    let rc = unsafe { libc::kill(pid, libc::SIGKILL) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }

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
