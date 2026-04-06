use anyhow::{Context, Result, bail};
use chrono::{DateTime, TimeZone, Utc};
use cron::Schedule;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::{
    env, fs,
    io::{self, Read as _, Seek as _, SeekFrom, Write as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    str::FromStr,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunStatus {
    Starting,
    Running,
    Exited,
    Failed,
    Stopped,
}

impl RunStatus {
    fn is_active(&self) -> bool {
        matches!(self, Self::Starting | Self::Running)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum RunOperation {
    Job {
        payload: String,
    },
    Heartbeat {
        payload: Option<String>,
    },
    Service {
        payload: Option<String>,
        interval_ms: u64,
        schedules: Vec<RunSchedule>,
    },
}

impl RunOperation {
    fn label(&self) -> &'static str {
        match self {
            Self::Job { .. } => "job",
            Self::Heartbeat { .. } => "heartbeat",
            Self::Service { .. } => "service",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RunSchedule {
    pub schedule_expr: String,
    pub next_fire_at_ms: u64,
    pub last_fired_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RunRecord {
    pub run_id: String,
    pub parcel_digest: String,
    pub parcel_name: Option<String>,
    pub parcel_version: Option<String>,
    pub parcel_path: PathBuf,
    pub courier: String,
    pub registry: Option<PathBuf>,
    pub operation: RunOperation,
    pub status: RunStatus,
    pub pid: Option<u32>,
    pub process_group_id: Option<u32>,
    pub started_at_ms: Option<u64>,
    pub stopped_at_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub session_file: PathBuf,
    pub log_path: PathBuf,
    pub tool_approval: crate::CliToolApprovalMode,
    pub a2a_policy: crate::CliA2aPolicy,
    pub last_error: Option<String>,
    pub detached: bool,
}

#[derive(Debug, Clone)]
struct RunPaths {
    record_path: PathBuf,
    log_path: PathBuf,
    session_path: PathBuf,
}

pub(crate) fn run_detached(args: crate::RunArgs) -> Result<()> {
    let crate::RunArgs { path, exec } = args;
    let operation = run_operation_from_exec(&exec)?;
    let detached = exec.detach;
    let parcel = crate::run::load_or_build_parcel_for_run(path)?;
    let paths = allocate_run_paths(&parcel, None)?;
    let record = build_run_record(&parcel, &paths, &exec, operation, detached);
    persist_run_record(&paths.record_path, &record)?;
    spawn_detached_runner(&paths.record_path, &record.log_path)?;
    println!("Started run {}", record.run_id);
    println!("Status: {}", format_status(&record.status));
    println!("Log: {}", record.log_path.display());
    println!("Record: {}", paths.record_path.display());
    Ok(())
}

pub(crate) fn serve(args: crate::ServeArgs) -> Result<()> {
    if args.interval_ms == 0 {
        bail!("`dispatch serve` requires --interval-ms > 0");
    }
    for schedule in &args.schedules {
        validate_schedule_expr(schedule)?;
    }

    let parcel = crate::run::load_or_build_parcel_for_run(args.path.clone())?;
    let paths = allocate_run_paths(&parcel, None)?;
    let record = build_run_record_for_service(&parcel, &paths, &args);
    persist_run_record(&paths.record_path, &record)?;

    if args.detach {
        spawn_detached_runner(&paths.record_path, &record.log_path)?;
        println!("Started service {}", record.run_id);
        println!("Status: {}", format_status(&record.status));
        println!("Log: {}", record.log_path.display());
        println!("Record: {}", paths.record_path.display());
        return Ok(());
    }

    internal_run_record(paths.record_path)
}

pub(crate) fn ps(args: crate::PsArgs) -> Result<()> {
    let root = crate::resolve_runs_root(&args.path);
    let mut runs = collect_run_records(&root)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&runs)?);
        return Ok(());
    }

    if runs.is_empty() {
        println!("No local runs found under {}", root.display());
        return Ok(());
    }

    runs.sort_by(|left, right| right.started_at_ms.cmp(&left.started_at_ms));
    for run in runs {
        let name = run.parcel_name.as_deref().unwrap_or("<unknown>");
        let version = run.parcel_version.as_deref().unwrap_or("<unspecified>");
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            run.run_id,
            format_status(&run.status),
            run.operation.label(),
            name,
            version,
            run.log_path.display()
        );
    }
    Ok(())
}

pub(crate) fn logs(args: crate::LogsArgs) -> Result<()> {
    let root = crate::resolve_runs_root(&args.path);
    let record_path = resolve_run_prefix(&root, &args.run)?;
    let mut record = load_run_record(&record_path)?;
    refresh_run_record(&record_path, &mut record)?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .open(&record.log_path)
        .with_context(|| format!("failed to read {}", record.log_path.display()))?;
    let mut buffer = String::new();
    file.read_to_string(&mut buffer)
        .with_context(|| format!("failed to read {}", record.log_path.display()))?;
    print!("{buffer}");
    io::stdout().flush()?;

    if !args.follow {
        return Ok(());
    }

    let mut offset = buffer.len() as u64;
    loop {
        let mut refreshed = load_run_record(&record_path)?;
        refresh_run_record(&record_path, &mut refreshed)?;

        file.seek(SeekFrom::Start(offset))
            .with_context(|| format!("failed to read {}", record.log_path.display()))?;
        let mut chunk = String::new();
        file.read_to_string(&mut chunk)
            .with_context(|| format!("failed to read {}", record.log_path.display()))?;
        if !chunk.is_empty() {
            offset += chunk.len() as u64;
            print!("{chunk}");
            io::stdout().flush()?;
        }

        if !refreshed.status.is_active() {
            break;
        }
        thread::sleep(Duration::from_millis(250));
    }

    Ok(())
}

pub(crate) fn stop(args: crate::StopArgs) -> Result<()> {
    let root = crate::resolve_runs_root(&args.path);
    let record_path = resolve_run_prefix(&root, &args.run)?;
    let mut record = load_run_record(&record_path)?;
    refresh_run_record(&record_path, &mut record)?;
    if !record.status.is_active() {
        println!(
            "Run {} is already {}",
            record.run_id,
            format_status(&record.status)
        );
        return Ok(());
    }
    let Some(pid) = record.pid else {
        bail!("run `{}` is missing a process id", record.run_id);
    };
    if let Some(process_group_id) = record.process_group_id {
        terminate_process_group(process_group_id, args.force)?;
    } else {
        terminate_pid(pid, args.force)?;
    }
    record.status = RunStatus::Stopped;
    record.stopped_at_ms = Some(now_ms());
    record.exit_code = None;
    record.last_error = None;
    persist_run_record(&record_path, &record)?;
    println!("Stopped run {}", record.run_id);
    Ok(())
}

pub(crate) fn rm(args: crate::RemoveRunArgs) -> Result<()> {
    let root = crate::resolve_runs_root(&args.path);
    let record_path = resolve_run_prefix(&root, &args.run)?;
    let mut record = load_run_record(&record_path)?;
    refresh_run_record(&record_path, &mut record)?;
    if record.status.is_active() {
        if !args.force {
            bail!(
                "run `{}` is still active; stop it before removing the record or pass --force",
                record.run_id
            );
        }
        let stop_args = crate::StopArgs {
            run: record.run_id.clone(),
            path: args.path.clone(),
            force: true,
        };
        stop(stop_args)?;
        record = load_run_record(&record_path)?;
    }

    if let Err(error) = fs::remove_file(&record_path)
        && (!args.force || error.kind() != io::ErrorKind::NotFound)
    {
        return Err(error).with_context(|| format!("failed to remove {}", record_path.display()));
    }
    if let Err(error) = fs::remove_file(&record.log_path)
        && (!args.force || error.kind() != io::ErrorKind::NotFound)
    {
        return Err(error)
            .with_context(|| format!("failed to remove {}", record.log_path.display()));
    }
    if let Err(error) = fs::remove_file(&record.session_file)
        && (!args.force || error.kind() != io::ErrorKind::NotFound)
    {
        return Err(error)
            .with_context(|| format!("failed to remove {}", record.session_file.display()));
    }
    println!("Removed run {}", record.run_id);
    Ok(())
}

pub(crate) fn inspect_run(args: crate::InspectRunArgs) -> Result<()> {
    let root = crate::resolve_runs_root(&args.path);
    let record_path = resolve_run_prefix(&root, &args.run)?;
    let mut record = load_run_record(&record_path)?;
    refresh_run_record(&record_path, &mut record)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&record)?);
        return Ok(());
    }

    println!("Run: {}", record.run_id);
    println!("Status: {}", format_status(&record.status));
    println!("Operation: {}", record.operation.label());
    println!("Courier: {}", record.courier);
    println!("Parcel: {}", record.parcel_path.display());
    println!("Log: {}", record.log_path.display());
    println!("Session: {}", record.session_file.display());
    if let Some(pid) = record.pid {
        println!("PID: {pid}");
    }
    if let Some(process_group_id) = record.process_group_id {
        println!("Process Group: {process_group_id}");
    }
    if let Some(code) = record.exit_code {
        println!("Exit: {code}");
    }
    if let Some(error) = &record.last_error {
        println!("Error: {error}");
    }
    Ok(())
}

pub(crate) fn internal_command(command: crate::InternalCommand) -> Result<()> {
    match command {
        crate::InternalCommand::RunRecord { record } => internal_run_record(record),
    }
}

fn internal_run_record(record_path: PathBuf) -> Result<()> {
    let mut record = load_run_record(&record_path)?;
    record.pid = Some(std::process::id());
    record.process_group_id = current_process_group_id();
    record.status = RunStatus::Running;
    record.started_at_ms.get_or_insert_with(now_ms);
    persist_run_record(&record_path, &record)?;

    let result = match record.operation.clone() {
        RunOperation::Job { payload } => execute_recorded_run(&record, RecordedMode::Job(payload)),
        RunOperation::Heartbeat { payload } => {
            execute_recorded_run(&record, RecordedMode::Heartbeat(payload))
        }
        RunOperation::Service {
            payload,
            interval_ms,
            schedules,
        } => execute_service_loop(&record, payload, interval_ms, schedules),
    };

    match result {
        Ok(()) => {
            let mut updated = load_run_record(&record_path)?;
            updated.status = RunStatus::Exited;
            updated.stopped_at_ms = Some(now_ms());
            updated.exit_code = Some(0);
            updated.last_error = None;
            persist_run_record(&record_path, &updated)?;
            Ok(())
        }
        Err(error) => {
            let mut updated = load_run_record(&record_path)?;
            updated.status = RunStatus::Failed;
            updated.stopped_at_ms = Some(now_ms());
            updated.exit_code = Some(1);
            updated.last_error = Some(error.to_string());
            persist_run_record(&record_path, &updated)?;
            Err(error)
        }
    }
}

enum RecordedMode {
    Job(String),
    Heartbeat(Option<String>),
}

fn execute_recorded_run(record: &RunRecord, mode: RecordedMode) -> Result<()> {
    let (job, heartbeat) = match mode {
        RecordedMode::Job(payload) => (Some(payload), None),
        RecordedMode::Heartbeat(payload) => (None, Some(payload.unwrap_or_default())),
    };
    crate::run::run(crate::RunArgs {
        path: record.parcel_path.clone(),
        exec: crate::RunExecutionArgs {
            courier: record.courier.clone(),
            registry: record.registry.clone(),
            session_file: Some(record.session_file.clone()),
            chat: None,
            job,
            heartbeat,
            interactive: false,
            print_prompt: false,
            list_tools: false,
            json: false,
            tool: None,
            input: None,
            tool_approval: Some(record.tool_approval),
            a2a_allowed_origins: record.a2a_policy.allowed_origins.clone(),
            a2a_trust_policy: record.a2a_policy.trust_policy.clone(),
            detach: false,
        },
    })
}

fn execute_service_loop(
    record: &RunRecord,
    payload: Option<String>,
    interval_ms: u64,
    mut schedules: Vec<RunSchedule>,
) -> Result<()> {
    let poll_interval_ms = service_poll_interval_ms(interval_ms, &schedules);
    if record.detached {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        loop {
            let fired =
                execute_due_service_work(record, payload.as_deref(), &mut schedules, &mut output)?;
            if fired {
                persist_service_schedules(record, &schedules)?;
            }
            thread::sleep(Duration::from_millis(poll_interval_ms));
        }
    } else {
        let log_file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&record.log_path)
            .with_context(|| format!("failed to open {}", record.log_path.display()))?;
        let stdout = io::stdout();
        let mut tee = TeeWriter::new(stdout.lock(), log_file);
        loop {
            let fired =
                execute_due_service_work(record, payload.as_deref(), &mut schedules, &mut tee)?;
            if fired {
                persist_service_schedules(record, &schedules)?;
            }
            thread::sleep(Duration::from_millis(poll_interval_ms));
        }
    }
}

fn execute_due_service_work(
    record: &RunRecord,
    payload: Option<&str>,
    schedules: &mut [RunSchedule],
    output: &mut impl io::Write,
) -> Result<bool> {
    if schedules.is_empty() {
        execute_service_heartbeat(record, payload, output)?;
        return Ok(true);
    }

    let mut fired = false;
    let now = now_ms();
    for schedule in schedules.iter_mut() {
        if schedule.next_fire_at_ms > now {
            continue;
        }
        execute_service_heartbeat(record, payload, output)?;
        schedule.last_fired_at_ms = Some(now);
        schedule.next_fire_at_ms = next_schedule_fire_ms(&schedule.schedule_expr, now)?;
        fired = true;
    }

    Ok(fired)
}

fn execute_service_heartbeat(
    record: &RunRecord,
    payload: Option<&str>,
    output: &mut impl io::Write,
) -> Result<()> {
    crate::run::run_into(
        crate::RunArgs {
            path: record.parcel_path.clone(),
            exec: crate::RunExecutionArgs {
                courier: record.courier.clone(),
                registry: record.registry.clone(),
                session_file: Some(record.session_file.clone()),
                chat: None,
                job: None,
                heartbeat: Some(payload.unwrap_or_default().to_string()),
                interactive: false,
                print_prompt: false,
                list_tools: false,
                json: false,
                tool: None,
                input: None,
                tool_approval: Some(record.tool_approval),
                a2a_allowed_origins: record.a2a_policy.allowed_origins.clone(),
                a2a_trust_policy: record.a2a_policy.trust_policy.clone(),
                detach: false,
            },
        },
        output,
    )
}

fn run_operation_from_exec(exec: &crate::RunExecutionArgs) -> Result<RunOperation> {
    match (
        &exec.job,
        &exec.heartbeat,
        exec.chat.is_some(),
        exec.interactive,
    ) {
        (Some(payload), None, false, false) => Ok(RunOperation::Job {
            payload: payload.clone(),
        }),
        (None, Some(payload), false, false) => Ok(RunOperation::Heartbeat {
            payload: if payload.is_empty() {
                None
            } else {
                Some(payload.clone())
            },
        }),
        _ => bail!(
            "`dispatch run --detach` currently supports only `--job <payload>` or `--heartbeat [payload]`"
        ),
    }
}

fn build_run_record(
    parcel: &dispatch_core::LoadedParcel,
    paths: &RunPaths,
    exec: &crate::RunExecutionArgs,
    operation: RunOperation,
    detached: bool,
) -> RunRecord {
    RunRecord {
        run_id: paths
            .record_path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("run")
            .to_string(),
        parcel_digest: parcel.config.digest.clone(),
        parcel_name: parcel.config.name.clone(),
        parcel_version: parcel.config.version.clone(),
        parcel_path: parcel.parcel_dir.clone(),
        courier: exec.courier.clone(),
        registry: exec.registry.clone(),
        operation,
        status: RunStatus::Starting,
        pid: None,
        process_group_id: None,
        started_at_ms: None,
        stopped_at_ms: None,
        exit_code: None,
        session_file: exec
            .session_file
            .clone()
            .unwrap_or_else(|| paths.session_path.clone()),
        log_path: paths.log_path.clone(),
        tool_approval: crate::resolve_noninteractive_tool_approval_mode(exec.tool_approval),
        a2a_policy: crate::CliA2aPolicy {
            allowed_origins: exec.a2a_allowed_origins.clone(),
            trust_policy: exec.a2a_trust_policy.clone(),
        },
        last_error: None,
        detached,
    }
}

fn build_run_record_for_service(
    parcel: &dispatch_core::LoadedParcel,
    paths: &RunPaths,
    args: &crate::ServeArgs,
) -> RunRecord {
    let schedules = args
        .schedules
        .iter()
        .map(|expr| build_run_schedule(expr))
        .collect::<Result<Vec<_>>>()
        .expect("schedules are validated before the run record is built");
    RunRecord {
        run_id: paths
            .record_path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("run")
            .to_string(),
        parcel_digest: parcel.config.digest.clone(),
        parcel_name: parcel.config.name.clone(),
        parcel_version: parcel.config.version.clone(),
        parcel_path: parcel.parcel_dir.clone(),
        courier: args.courier.clone(),
        registry: args.registry.clone(),
        operation: RunOperation::Service {
            payload: args.payload.clone(),
            interval_ms: args.interval_ms,
            schedules,
        },
        status: RunStatus::Starting,
        pid: None,
        process_group_id: None,
        started_at_ms: None,
        stopped_at_ms: None,
        exit_code: None,
        session_file: args
            .session_file
            .clone()
            .unwrap_or_else(|| paths.session_path.clone()),
        log_path: paths.log_path.clone(),
        tool_approval: crate::CliToolApprovalMode::Never,
        a2a_policy: crate::CliA2aPolicy {
            allowed_origins: args.a2a_allowed_origins.clone(),
            trust_policy: args.a2a_trust_policy.clone(),
        },
        last_error: None,
        detached: args.detach,
    }
}

fn build_run_schedule(expr: &str) -> Result<RunSchedule> {
    Ok(RunSchedule {
        schedule_expr: expr.to_string(),
        next_fire_at_ms: next_schedule_fire_ms(expr, now_ms())?,
        last_fired_at_ms: None,
    })
}

fn allocate_run_paths(
    parcel: &dispatch_core::LoadedParcel,
    explicit_root: Option<&Path>,
) -> Result<RunPaths> {
    let root = explicit_root
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::resolve_runs_root(&parcel.parcel_dir));
    fs::create_dir_all(&root).with_context(|| format!("failed to create {}", root.display()))?;
    let run_id = format!(
        "{}-{}",
        now_ms(),
        &parcel.config.digest[..std::cmp::min(parcel.config.digest.len(), 12)]
    );
    Ok(RunPaths {
        record_path: root.join(format!("{run_id}.json")),
        log_path: root.join(format!("{run_id}.log")),
        session_path: root.join(format!("{run_id}.session.json")),
    })
}

fn collect_run_records(root: &Path) -> Result<Vec<RunRecord>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut runs = Vec::new();
    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let entry = entry.with_context(|| format!("failed to inspect {}", root.display()))?;
        let path = entry.path();
        if !path.is_file()
            || path.extension().and_then(|ext| ext.to_str()) != Some("json")
            || path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".session.json"))
        {
            continue;
        }
        let mut record = load_run_record(&path)?;
        refresh_run_record(&path, &mut record)?;
        runs.push(record);
    }
    Ok(runs)
}

fn persist_service_schedules(record: &RunRecord, schedules: &[RunSchedule]) -> Result<()> {
    let record_path =
        crate::resolve_runs_root(&record.parcel_path).join(format!("{}.json", record.run_id));
    let mut updated = load_run_record(&record_path)?;
    if let RunOperation::Service {
        schedules: current, ..
    } = &mut updated.operation
    {
        *current = schedules.to_vec();
        persist_run_record(&record_path, &updated)?;
    }
    Ok(())
}

fn resolve_run_prefix(root: &Path, prefix: &str) -> Result<PathBuf> {
    if prefix.contains(std::path::MAIN_SEPARATOR) || prefix.ends_with(".json") {
        let path = PathBuf::from(prefix);
        if path.exists() {
            return Ok(path);
        }
    }
    let mut matches = Vec::new();
    if root.exists() {
        for entry in
            fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))?
        {
            let entry = entry.with_context(|| format!("failed to inspect {}", root.display()))?;
            let path = entry.path();
            if !path.is_file()
                || path.extension().and_then(|ext| ext.to_str()) != Some("json")
                || path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".session.json"))
            {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if stem.starts_with(prefix) {
                matches.push(path);
            }
        }
    }
    match matches.len() {
        0 => bail!("no run matched `{prefix}` under {}", root.display()),
        1 => Ok(matches.remove(0)),
        _ => bail!(
            "run prefix `{prefix}` is ambiguous under {}",
            root.display()
        ),
    }
}

fn refresh_run_record(record_path: &Path, record: &mut RunRecord) -> Result<()> {
    if !record.status.is_active() {
        return Ok(());
    }
    let Some(pid) = record.pid else {
        return Ok(());
    };
    if pid_is_running(pid) {
        return Ok(());
    }
    record.status = RunStatus::Stopped;
    record.stopped_at_ms.get_or_insert_with(now_ms);
    persist_run_record(record_path, record)
}

fn load_run_record(path: &Path) -> Result<RunRecord> {
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&source).with_context(|| format!("failed to parse {}", path.display()))
}

fn persist_run_record(path: &Path, record: &RunRecord) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, serde_json::to_vec_pretty(record)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

fn spawn_detached_runner(record_path: &Path, log_path: &Path) -> Result<()> {
    if let Some(parent) = log_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let stdout = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;
    let stderr = stdout
        .try_clone()
        .with_context(|| format!("failed to open {}", log_path.display()))?;
    let mut command =
        Command::new(env::current_exe().context("failed to resolve dispatch binary")?);
    command
        .arg("internal")
        .arg("run-record")
        .arg(record_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            Ok(())
        });
    }
    command.spawn().with_context(|| {
        format!(
            "failed to launch detached runner for {}",
            record_path.display()
        )
    })?;
    Ok(())
}

fn format_status(status: &RunStatus) -> &'static str {
    match status {
        RunStatus::Starting => "starting",
        RunStatus::Running => "running",
        RunStatus::Exited => "exited",
        RunStatus::Failed => "failed",
        RunStatus::Stopped => "stopped",
    }
}

fn service_poll_interval_ms(interval_ms: u64, schedules: &[RunSchedule]) -> u64 {
    if schedules.is_empty() {
        interval_ms
    } else {
        interval_ms.min(1_000)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn validate_schedule_expr(expr: &str) -> Result<()> {
    let _ = Schedule::from_str(expr)
        .with_context(|| format!("failed to parse cron schedule `{expr}`"))?;
    Ok(())
}

fn next_schedule_fire_ms(expr: &str, after_ms: u64) -> Result<u64> {
    let schedule = Schedule::from_str(expr)
        .with_context(|| format!("failed to parse cron schedule `{expr}`"))?;
    let after = timestamp_from_ms(after_ms)?;
    let next = schedule.after(&after).next().ok_or_else(|| {
        anyhow::anyhow!("cron schedule `{expr}` did not produce a next fire time")
    })?;
    Ok(next.timestamp_millis() as u64)
}

fn timestamp_from_ms(ms: u64) -> Result<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms as i64)
        .single()
        .ok_or_else(|| anyhow::anyhow!("invalid timestamp `{ms}`"))
}

#[cfg(unix)]
fn pid_is_running(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as i32, 0) };
    if rc == 0 {
        return true;
    }
    matches!(
        io::Error::last_os_error().raw_os_error(),
        Some(code) if code == libc::EPERM
    )
}

#[cfg(not(unix))]
fn pid_is_running(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
fn terminate_pid(pid: u32, force: bool) -> Result<()> {
    let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
    let rc = unsafe { libc::kill(pid as i32, signal) };
    if rc == 0 {
        return Ok(());
    }
    Err(io::Error::last_os_error()).with_context(|| format!("failed to stop pid {pid}"))
}

#[cfg(unix)]
fn terminate_process_group(process_group_id: u32, force: bool) -> Result<()> {
    let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
    let rc = unsafe { libc::kill(-(process_group_id as i32), signal) };
    if rc == 0 {
        return Ok(());
    }
    Err(io::Error::last_os_error())
        .with_context(|| format!("failed to stop process group {process_group_id}"))
}

#[cfg(not(unix))]
fn terminate_pid(pid: u32, force: bool) -> Result<()> {
    let mut command = Command::new("taskkill");
    command.arg("/PID").arg(pid.to_string()).arg("/T");
    if force {
        command.arg("/F");
    }
    let status = command
        .status()
        .with_context(|| format!("failed to stop pid {pid}"))?;
    if status.success() {
        Ok(())
    } else {
        bail!("failed to stop pid {pid}")
    }
}

#[cfg(not(unix))]
fn terminate_process_group(process_group_id: u32, force: bool) -> Result<()> {
    terminate_pid(process_group_id, force)
}

#[cfg(unix)]
fn current_process_group_id() -> Option<u32> {
    let pgid = unsafe { libc::getpgid(0) };
    if pgid < 0 { None } else { Some(pgid as u32) }
}

#[cfg(not(unix))]
fn current_process_group_id() -> Option<u32> {
    None
}

struct TeeWriter<A, B> {
    left: A,
    right: B,
}

impl<A, B> TeeWriter<A, B> {
    fn new(left: A, right: B) -> Self {
        Self { left, right }
    }
}

impl<A: io::Write, B: io::Write> io::Write for TeeWriter<A, B> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.left.write_all(buf)?;
        self.right.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.left.flush()?;
        self.right.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::{RunOperation, RunRecord, RunStatus, resolve_run_prefix};
    use crate::CliA2aPolicy;
    use tempfile::tempdir;

    #[test]
    fn resolve_run_prefix_matches_unique_ids() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("123abc.json"), "{}").unwrap();
        let resolved = resolve_run_prefix(dir.path(), "123").unwrap();
        assert_eq!(resolved, dir.path().join("123abc.json"));
    }

    #[test]
    fn resolve_run_prefix_rejects_ambiguous_matches() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("123abc.json"), "{}").unwrap();
        std::fs::write(dir.path().join("123def.json"), "{}").unwrap();
        let error = resolve_run_prefix(dir.path(), "123")
            .unwrap_err()
            .to_string();
        assert!(error.contains("ambiguous"));
    }

    #[test]
    fn run_record_round_trips_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("run.json");
        let record = RunRecord {
            run_id: "run-1".to_string(),
            parcel_digest: "digest".to_string(),
            parcel_name: Some("demo".to_string()),
            parcel_version: Some("1.0.0".to_string()),
            parcel_path: dir.path().join("parcel"),
            courier: "native".to_string(),
            registry: None,
            operation: RunOperation::Job {
                payload: "{}".to_string(),
            },
            status: RunStatus::Starting,
            pid: Some(42),
            process_group_id: Some(42),
            started_at_ms: Some(1),
            stopped_at_ms: None,
            exit_code: None,
            session_file: dir.path().join("run.session.json"),
            log_path: dir.path().join("run.log"),
            tool_approval: crate::CliToolApprovalMode::Never,
            a2a_policy: CliA2aPolicy::default(),
            last_error: None,
            detached: true,
        };
        let payload = serde_json::to_string_pretty(&record).unwrap();
        std::fs::write(&path, payload).unwrap();
        let loaded: RunRecord =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded, record);
    }

    #[test]
    fn build_run_schedule_sets_next_fire_time() {
        let schedule = super::build_run_schedule("*/5 * * * * * *").unwrap();
        assert!(schedule.next_fire_at_ms > 0);
        assert!(schedule.last_fired_at_ms.is_none());
    }

    #[test]
    fn service_poll_interval_uses_one_second_when_scheduled() {
        let schedules = vec![super::RunSchedule {
            schedule_expr: "*/5 * * * * * *".to_string(),
            next_fire_at_ms: 1,
            last_fired_at_ms: None,
        }];
        assert_eq!(super::service_poll_interval_ms(30_000, &schedules), 1_000);
        assert_eq!(super::service_poll_interval_ms(500, &schedules), 500);
        assert_eq!(super::service_poll_interval_ms(30_000, &[]), 30_000);
    }
}
