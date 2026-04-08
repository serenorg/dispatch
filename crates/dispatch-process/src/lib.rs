use std::{
    fs,
    io::{self, BufRead},
    path::Path,
    process::{Child, Command, Output, Stdio},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[cfg(not(any(unix, windows)))]
compile_error!("dispatch-process currently supports only Unix and Windows targets");

#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokenPipePolicy {
    Error,
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminationTarget {
    Pid(u32),
    ProcessGroup(u32),
}

#[derive(Debug)]
pub enum RecvLineError {
    Timeout,
    Disconnected,
    Read(String),
    ChildWait(io::Error),
}

pub type LineReadResult = Result<(usize, String), String>;

pub fn configure_detached_command(command: &mut Command) {
    #[cfg(windows)]
    {
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

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
}

pub fn write_child_stdin(
    child: &mut Child,
    input: Option<&[u8]>,
    broken_pipe_policy: BrokenPipePolicy,
) -> io::Result<()> {
    if let Some(input) = input
        && let Some(stdin) = child.stdin.as_mut()
    {
        use std::io::Write as _;
        if let Err(error) = stdin.write_all(input)
            && !(broken_pipe_policy == BrokenPipePolicy::Ignore
                && error.kind() == io::ErrorKind::BrokenPipe)
        {
            return Err(error);
        }
    }
    drop(child.stdin.take());
    Ok(())
}

pub fn wait_for_child_timeout(
    child: &mut Child,
    timeout: Option<Duration>,
    poll_interval: Duration,
) -> io::Result<Option<std::process::ExitStatus>> {
    let deadline = timeout.and_then(|value| Instant::now().checked_add(value));
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(None);
        }
        thread::sleep(poll_interval);
    }
}

pub fn kill_child_and_wait(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
pub fn pid_is_running(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as i32, 0) };
    if rc == 0 {
        return true;
    }
    matches!(
        io::Error::last_os_error().raw_os_error(),
        Some(code) if code == libc::EPERM
    )
}

#[cfg(windows)]
pub fn pid_is_running(pid: u32) -> bool {
    let output = match Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
    {
        Ok(output) => output,
        Err(_) => return false,
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.trim_start().starts_with('"')
}

#[cfg(unix)]
pub fn terminate_target(target: TerminationTarget, force: bool) -> io::Result<()> {
    let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
    let process = match target {
        TerminationTarget::Pid(pid) => pid as i32,
        TerminationTarget::ProcessGroup(process_group_id) => -(process_group_id as i32),
    };
    let rc = unsafe { libc::kill(process, signal) };
    if rc == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error)
}

#[cfg(windows)]
pub fn terminate_target(target: TerminationTarget, force: bool) -> io::Result<()> {
    let pid = match target {
        TerminationTarget::Pid(pid) | TerminationTarget::ProcessGroup(pid) => pid,
    };
    let mut command = Command::new("taskkill");
    command.arg("/PID").arg(pid.to_string());
    if force {
        command.arg("/F");
    }
    let status = command.status()?;
    if status.success() || !force {
        Ok(())
    } else {
        Err(io::Error::other(format!("failed to stop pid {pid}")))
    }
}

pub fn run_command_with_file_capture(
    command: &mut Command,
    stdout_path: &Path,
    stderr_path: &Path,
    timeout: Duration,
    poll_interval: Duration,
) -> io::Result<Output> {
    let stdout_file = fs::File::create(stdout_path)?;
    let stderr_file = fs::File::create(stderr_path)?;
    command.stdout(Stdio::from(stdout_file));
    command.stderr(Stdio::from(stderr_file));

    let mut child = command.spawn()?;
    let Some(status) = wait_for_child_timeout(&mut child, Some(timeout), poll_interval)? else {
        kill_child_and_wait(&mut child);
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("command timed out after {}ms", timeout.as_millis()),
        ));
    };

    Ok(Output {
        status,
        stdout: fs::read(stdout_path)?,
        stderr: fs::read(stderr_path)?,
    })
}

pub fn spawn_line_reader<R>(mut reader: R) -> (Receiver<LineReadResult>, JoinHandle<()>)
where
    R: BufRead + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        loop {
            let mut line = String::new();
            let result = reader
                .read_line(&mut line)
                .map(|bytes| (bytes, line))
                .map_err(|error| error.to_string());
            let done = matches!(result, Ok((0, _)));
            if sender.send(result).is_err() {
                break;
            }
            if done {
                break;
            }
        }
    });
    (receiver, handle)
}

pub fn recv_line_with_child_exit(
    receiver: &Receiver<LineReadResult>,
    child: &mut Child,
    timeout: Option<Duration>,
    poll_interval: Duration,
) -> Result<Option<(usize, String)>, RecvLineError> {
    let deadline = timeout.and_then(|value| Instant::now().checked_add(value));
    loop {
        let wait_for = deadline
            .map(|deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(poll_interval)
            })
            .unwrap_or(poll_interval);

        let recv_result = match receiver.recv_timeout(wait_for) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if child
                    .try_wait()
                    .map_err(RecvLineError::ChildWait)?
                    .is_some()
                {
                    return Ok(None);
                }
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    return Err(RecvLineError::Timeout);
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(RecvLineError::Disconnected),
        };

        return recv_result.map(Some).map_err(RecvLineError::Read);
    }
}
