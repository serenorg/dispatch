use anyhow::{Context, Result, bail};
use chrono::{DateTime, TimeZone, Utc};
use cron::Schedule;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, BufReader, Read as _, Seek as _, SeekFrom, Write as _},
    net::{TcpListener, TcpStream},
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
        listeners: Vec<RunListener>,
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
pub(crate) struct RunListener {
    pub listen_addr: String,
    pub bound_addr: Option<String>,
    pub requests_handled: u64,
    pub last_request_at_ms: Option<u64>,
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
    for listen in &args.listens {
        validate_listen_addr(listen)?;
    }
    let parcel = crate::run::load_or_build_parcel_for_run(args.path.clone())?;
    validate_service_parcel(&parcel)?;
    for schedule in &parcel.config.schedules {
        validate_schedule_expr(schedule)?;
    }
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
        let schedule_summary = schedule_summary(&run.operation);
        let listener_summary = listener_summary(&run.operation);
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            run.run_id,
            format_status(&run.status),
            run.operation.label(),
            name,
            version,
            schedule_summary,
            listener_summary,
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
    println!("Schedules: {}", schedule_summary(&record.operation));
    println!("Listeners: {}", listener_summary(&record.operation));
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
            listeners,
        } => execute_service_loop(
            &record_path,
            &record,
            payload,
            interval_ms,
            schedules,
            listeners,
        ),
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
    record_path: &Path,
    record: &RunRecord,
    payload: Option<String>,
    interval_ms: u64,
    mut schedules: Vec<RunSchedule>,
    mut listeners: Vec<RunListener>,
) -> Result<()> {
    let mut bound_listeners = bind_service_listeners(record_path, &mut listeners)?;
    let poll_interval_ms = service_poll_interval_ms(interval_ms, &schedules, &listeners);
    let mut next_heartbeat_at_ms = schedules.is_empty().then(now_ms);
    if record.detached {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        loop {
            let fired = execute_due_service_work(
                record,
                payload.as_deref(),
                interval_ms,
                &mut next_heartbeat_at_ms,
                &mut schedules,
                &mut output,
            )?;
            let handled = execute_service_ingress(
                record_path,
                record,
                &mut listeners,
                &mut bound_listeners,
                &mut output,
            )?;
            if fired || handled {
                persist_service_state(record_path, &schedules, &listeners)?;
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
            let fired = execute_due_service_work(
                record,
                payload.as_deref(),
                interval_ms,
                &mut next_heartbeat_at_ms,
                &mut schedules,
                &mut tee,
            )?;
            let handled = execute_service_ingress(
                record_path,
                record,
                &mut listeners,
                &mut bound_listeners,
                &mut tee,
            )?;
            if fired || handled {
                persist_service_state(record_path, &schedules, &listeners)?;
            }
            thread::sleep(Duration::from_millis(poll_interval_ms));
        }
    }
}

fn execute_due_service_work(
    record: &RunRecord,
    payload: Option<&str>,
    interval_ms: u64,
    next_heartbeat_at_ms: &mut Option<u64>,
    schedules: &mut [RunSchedule],
    output: &mut impl io::Write,
) -> Result<bool> {
    let now = now_ms();
    if schedules.is_empty() {
        let Some(next_fire_at_ms) = next_heartbeat_at_ms.as_mut() else {
            return Ok(false);
        };
        if now < *next_fire_at_ms {
            return Ok(false);
        }
        execute_service_heartbeat(record, payload, output)?;
        *next_fire_at_ms = now.saturating_add(interval_ms);
        return Ok(true);
    }

    let mut fired = false;
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
    let schedules = merged_schedule_exprs(&parcel.config.schedules, &args.schedules)
        .into_iter()
        .map(|expr| build_run_schedule(&expr))
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
            listeners: args
                .listens
                .iter()
                .map(|listen_addr| RunListener {
                    listen_addr: listen_addr.clone(),
                    bound_addr: None,
                    requests_handled: 0,
                    last_request_at_ms: None,
                })
                .collect(),
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

fn merged_schedule_exprs(parcel_schedules: &[String], cli_schedules: &[String]) -> Vec<String> {
    let mut merged = Vec::new();
    for expr in parcel_schedules.iter().chain(cli_schedules.iter()) {
        if !merged.iter().any(|existing| existing == expr) {
            merged.push(expr.clone());
        }
    }
    merged
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

fn persist_service_state(
    record_path: &Path,
    schedules: &[RunSchedule],
    listeners: &[RunListener],
) -> Result<()> {
    let mut updated = load_run_record(record_path)?;
    if let RunOperation::Service {
        schedules: current_schedules,
        listeners: current_listeners,
        ..
    } = &mut updated.operation
    {
        if !schedules.is_empty() {
            *current_schedules = schedules.to_vec();
        }
        if !listeners.is_empty() {
            *current_listeners = listeners.to_vec();
        }
        persist_run_record(record_path, &updated)?;
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

fn validate_service_parcel(parcel: &dispatch_core::LoadedParcel) -> Result<()> {
    if parcel.config.entrypoint.as_deref() == Some("heartbeat") {
        Ok(())
    } else {
        bail!(
            "`dispatch serve` requires parcels with `ENTRYPOINT heartbeat`, found {}",
            parcel.config.entrypoint.as_deref().unwrap_or("<unset>")
        )
    }
}

fn schedule_summary(operation: &RunOperation) -> String {
    match operation {
        RunOperation::Service { schedules, .. } if schedules.is_empty() => "none".to_string(),
        RunOperation::Service { schedules, .. } => {
            let mut parts = schedules
                .iter()
                .map(|schedule| {
                    format!(
                        "{}=>{}",
                        schedule.schedule_expr,
                        format_timestamp_ms(schedule.next_fire_at_ms)
                    )
                })
                .collect::<Vec<_>>();
            if parts.len() > 2 {
                parts.truncate(2);
                parts.push("...".to_string());
            }
            parts.join(", ")
        }
        _ => "-".to_string(),
    }
}

fn listener_summary(operation: &RunOperation) -> String {
    match operation {
        RunOperation::Service { listeners, .. } if listeners.is_empty() => "none".to_string(),
        RunOperation::Service { listeners, .. } => {
            let mut parts = listeners
                .iter()
                .map(|listener| {
                    let addr = listener
                        .bound_addr
                        .as_deref()
                        .unwrap_or(listener.listen_addr.as_str());
                    format!("{addr} ({} req)", listener.requests_handled)
                })
                .collect::<Vec<_>>();
            if parts.len() > 2 {
                parts.truncate(2);
                parts.push("...".to_string());
            }
            parts.join(", ")
        }
        _ => "-".to_string(),
    }
}

fn service_poll_interval_ms(
    interval_ms: u64,
    schedules: &[RunSchedule],
    listeners: &[RunListener],
) -> u64 {
    let mut poll_interval_ms = interval_ms;
    if !schedules.is_empty() {
        poll_interval_ms = poll_interval_ms.min(1_000);
    }
    if !listeners.is_empty() {
        poll_interval_ms = poll_interval_ms.min(100);
    }
    poll_interval_ms
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

fn validate_listen_addr(addr: &str) -> Result<()> {
    let _ = addr
        .parse::<std::net::SocketAddr>()
        .with_context(|| format!("failed to parse listen address `{addr}`"))?;
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

fn format_timestamp_ms(ms: u64) -> String {
    timestamp_from_ms(ms)
        .map(|timestamp| timestamp.to_rfc3339())
        .unwrap_or_else(|_| ms.to_string())
}

struct BoundServiceListener {
    index: usize,
    listener: TcpListener,
}

#[derive(Debug, Serialize)]
struct ServiceIngressEnvelope {
    kind: &'static str,
    received_at_ms: u64,
    listener: String,
    remote_addr: String,
    method: String,
    target: String,
    path: String,
    query: Option<String>,
    headers: BTreeMap<String, String>,
    body: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedHttpRequest {
    method: String,
    target: String,
    path: String,
    query: Option<String>,
    headers: BTreeMap<String, String>,
    body: Option<String>,
}

fn bind_service_listeners(
    record_path: &Path,
    listeners: &mut [RunListener],
) -> Result<Vec<BoundServiceListener>> {
    let mut bound = Vec::with_capacity(listeners.len());
    for (index, listener_state) in listeners.iter_mut().enumerate() {
        let listener = TcpListener::bind(&listener_state.listen_addr)
            .with_context(|| format!("failed to bind {}", listener_state.listen_addr))?;
        listener
            .set_nonblocking(true)
            .with_context(|| format!("failed to configure {}", listener_state.listen_addr))?;
        listener_state.bound_addr = Some(
            listener
                .local_addr()
                .with_context(|| format!("failed to inspect {}", listener_state.listen_addr))?
                .to_string(),
        );
        bound.push(BoundServiceListener { index, listener });
    }
    if !listeners.is_empty() {
        persist_service_state(record_path, &[], listeners)?;
    }
    Ok(bound)
}

fn execute_service_ingress(
    record_path: &Path,
    record: &RunRecord,
    listeners: &mut [RunListener],
    bound_listeners: &mut [BoundServiceListener],
    output: &mut impl io::Write,
) -> Result<bool> {
    let mut handled = false;
    for bound in bound_listeners.iter_mut() {
        loop {
            match bound.listener.accept() {
                Ok((stream, remote_addr)) => {
                    handled = true;
                    let now = now_ms();
                    let listener_state = &mut listeners[bound.index];
                    listener_state.requests_handled += 1;
                    listener_state.last_request_at_ms = Some(now);
                    handle_service_connection(
                        record,
                        listener_state,
                        stream,
                        remote_addr.to_string(),
                        output,
                    )?;
                    persist_service_state(record_path, &[], listeners)?;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    writeln!(
                        output,
                        "[dispatch serve] ingress accept failed on {}: {error}",
                        listeners[bound.index]
                            .bound_addr
                            .as_deref()
                            .unwrap_or(listeners[bound.index].listen_addr.as_str())
                    )?;
                    break;
                }
            }
        }
    }
    Ok(handled)
}

fn handle_service_connection(
    record: &RunRecord,
    listener: &RunListener,
    stream: TcpStream,
    remote_addr: String,
    output: &mut impl io::Write,
) -> Result<()> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .context("failed to configure ingress read timeout")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .context("failed to configure ingress write timeout")?;
    let mut writer = stream
        .try_clone()
        .context("failed to clone ingress connection")?;
    let mut reader = BufReader::new(stream);
    match parse_http_request(&mut reader) {
        Ok(request) => {
            writeln!(
                output,
                "[dispatch serve] ingress {} {} from {} via {}",
                request.method,
                request.target,
                remote_addr,
                listener
                    .bound_addr
                    .as_deref()
                    .unwrap_or(listener.listen_addr.as_str())
            )?;
            let payload =
                serde_json::to_string(&request_payload_envelope(listener, &remote_addr, &request))?;
            match execute_service_heartbeat(record, Some(payload.as_str()), output) {
                Ok(()) => write_http_response(
                    &mut writer,
                    202,
                    "Accepted",
                    &format!("accepted {}\n", record.run_id),
                ),
                Err(error) => write_http_response(
                    &mut writer,
                    500,
                    "Internal Server Error",
                    &format!("dispatch serve failed: {error}\n"),
                ),
            }
        }
        Err(error) => write_http_response(
            &mut writer,
            400,
            "Bad Request",
            &format!("invalid request: {error}\n"),
        ),
    }
}

fn request_payload_envelope(
    listener: &RunListener,
    remote_addr: &str,
    request: &ParsedHttpRequest,
) -> ServiceIngressEnvelope {
    ServiceIngressEnvelope {
        kind: "http_request",
        received_at_ms: now_ms(),
        listener: listener
            .bound_addr
            .clone()
            .unwrap_or_else(|| listener.listen_addr.clone()),
        remote_addr: remote_addr.to_string(),
        method: request.method.clone(),
        target: request.target.clone(),
        path: request.path.clone(),
        query: request.query.clone(),
        headers: request.headers.clone(),
        body: request.body.clone(),
    }
}

fn parse_http_request(reader: &mut impl io::BufRead) -> Result<ParsedHttpRequest> {
    let mut request_line = String::new();
    let read = reader
        .read_line(&mut request_line)
        .context("failed to read request line")?;
    if read == 0 {
        bail!("empty request");
    }
    let request_line = request_line.trim_end();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let version = parts.next().unwrap_or_default();
    if method.is_empty() || target.is_empty() || !version.starts_with("HTTP/") {
        bail!("malformed request line `{request_line}`");
    }

    let mut headers = BTreeMap::new();
    let mut content_length = 0usize;
    loop {
        let mut header_line = String::new();
        reader
            .read_line(&mut header_line)
            .context("failed to read request header")?;
        let header_line = header_line.trim_end();
        if header_line.is_empty() {
            break;
        }
        let Some((name, value)) = header_line.split_once(':') else {
            bail!("malformed header `{header_line}`");
        };
        let header_name = name.trim().to_ascii_lowercase();
        let header_value = value.trim().to_string();
        if header_name == "content-length" {
            content_length = header_value
                .parse::<usize>()
                .with_context(|| format!("invalid content-length `{header_value}`"))?;
        }
        if header_name == "transfer-encoding" {
            bail!("transfer-encoding is not supported");
        }
        headers.insert(header_name, header_value);
    }

    if content_length > 256 * 1024 {
        bail!("request body exceeds 262144 bytes");
    }
    let mut body = vec![0u8; content_length];
    reader
        .read_exact(&mut body)
        .context("failed to read request body")?;
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), Some(query.to_string())),
        None => (target.clone(), None),
    };

    Ok(ParsedHttpRequest {
        method,
        target,
        path,
        query,
        headers,
        body: if body.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(&body).into_owned())
        },
    })
}

fn write_http_response(
    writer: &mut impl io::Write,
    status_code: u16,
    status_text: &str,
    body: &str,
) -> Result<()> {
    write!(
        writer,
        "HTTP/1.1 {status_code} {status_text}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .context("failed to write ingress response")
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
    use std::collections::BTreeMap;
    use std::io::Cursor;
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
        assert_eq!(
            super::service_poll_interval_ms(30_000, &schedules, &[]),
            1_000
        );
        assert_eq!(super::service_poll_interval_ms(500, &schedules, &[]), 500);
        assert_eq!(super::service_poll_interval_ms(30_000, &[], &[]), 30_000);
    }

    #[test]
    fn service_poll_interval_uses_fast_polls_for_listeners() {
        let listeners = vec![super::RunListener {
            listen_addr: "127.0.0.1:0".to_string(),
            bound_addr: None,
            requests_handled: 0,
            last_request_at_ms: None,
        }];
        assert_eq!(
            super::service_poll_interval_ms(30_000, &[], &listeners),
            100
        );
    }

    #[test]
    fn merged_schedule_exprs_deduplicates_and_preserves_order() {
        let merged = super::merged_schedule_exprs(
            &["*/5 * * * * * *".to_string(), "0 */2 * * * * *".to_string()],
            &["0 */2 * * * * *".to_string(), "*/1 * * * * * *".to_string()],
        );
        assert_eq!(
            merged,
            vec![
                "*/5 * * * * * *".to_string(),
                "0 */2 * * * * *".to_string(),
                "*/1 * * * * * *".to_string()
            ]
        );
    }

    #[test]
    fn schedule_summary_reports_next_fire_time() {
        let summary = super::schedule_summary(&RunOperation::Service {
            payload: None,
            interval_ms: 30_000,
            schedules: vec![super::RunSchedule {
                schedule_expr: "*/5 * * * * * *".to_string(),
                next_fire_at_ms: 1_700_000_000_000,
                last_fired_at_ms: None,
            }],
            listeners: Vec::new(),
        });
        assert!(summary.contains("*/5 * * * * * *=>"));
        assert!(summary.contains("2023-11-14T22:13:20+00:00"));
    }

    #[test]
    fn listener_summary_reports_bound_address_and_count() {
        let summary = super::listener_summary(&RunOperation::Service {
            payload: None,
            interval_ms: 30_000,
            schedules: Vec::new(),
            listeners: vec![super::RunListener {
                listen_addr: "127.0.0.1:0".to_string(),
                bound_addr: Some("127.0.0.1:48123".to_string()),
                requests_handled: 2,
                last_request_at_ms: Some(1_700_000_000_000),
            }],
        });
        assert_eq!(summary, "127.0.0.1:48123 (2 req)");
    }

    #[test]
    fn parse_http_request_extracts_target_headers_and_body() {
        let request = b"POST /hook?a=1 HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 8\r\n\r\n{\"ok\":1}";
        let parsed = super::parse_http_request(&mut Cursor::new(&request[..])).unwrap();
        let mut headers = BTreeMap::new();
        headers.insert("content-length".to_string(), "8".to_string());
        headers.insert("content-type".to_string(), "application/json".to_string());
        headers.insert("host".to_string(), "localhost".to_string());
        assert_eq!(
            parsed,
            super::ParsedHttpRequest {
                method: "POST".to_string(),
                target: "/hook?a=1".to_string(),
                path: "/hook".to_string(),
                query: Some("a=1".to_string()),
                headers,
                body: Some("{\"ok\":1}".to_string()),
            }
        );
    }
}
