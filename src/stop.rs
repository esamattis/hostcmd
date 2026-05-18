use std::{fs, thread::sleep, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use nix::{
    errno::Errno,
    sys::signal::{Signal, kill},
    unistd::Pid,
};

use crate::{cli::StopArgs, config::expand_home_path};

/// Time allowed for a stopped daemon to exit after TERM.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// Polling delay used while waiting for a stopped daemon to exit.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Process target selected for stop signals.
#[derive(Clone, Copy)]
enum StopTarget {
    /// The daemon process group, rooted at the daemon pid.
    ProcessGroup(Pid),
    /// Only the daemon process id from the pid file.
    Process(Pid),
}

/// Stops a daemon server by sending TERM to its process group or process id.
pub fn stop_daemon(args: StopArgs) -> Result<()> {
    let pid_file = expand_home_path(&args.pid_file)?;
    let pid_text = fs::read_to_string(&pid_file)
        .with_context(|| format!("failed to read pid file {}", pid_file.display()))?;
    let pid = parse_pid(&pid_text)?;

    let target = signal_stop_target(pid, Signal::SIGTERM)?;
    if !wait_for_exit(target)? {
        signal_existing_target(target, Signal::SIGKILL)?;
        wait_for_kill(target)?;
    }

    let _ = fs::remove_file(&pid_file);
    Ok(())
}

/// Sends the first stop signal to a process group, falling back to a single process.
fn signal_stop_target(pid: Pid, signal: Signal) -> Result<StopTarget> {
    let group = process_group_pid(pid);

    match kill(group, signal) {
        Ok(()) => Ok(StopTarget::ProcessGroup(pid)),
        Err(Errno::ESRCH) => {
            kill(pid, signal).with_context(|| format!("failed to signal process {pid}"))?;
            Ok(StopTarget::Process(pid))
        }
        Err(err) => Err(err).with_context(|| format!("failed to signal process group {pid}")),
    }
}

/// Sends a follow-up signal to a previously selected stop target.
fn signal_existing_target(target: StopTarget, signal: Signal) -> Result<()> {
    let result = match target {
        StopTarget::ProcessGroup(pid) => kill(process_group_pid(pid), signal),
        StopTarget::Process(pid) => kill(pid, signal),
    };

    match result {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to signal {target}")),
    }
}

/// Waits until the signaled stop target no longer exists.
fn wait_for_exit(target: StopTarget) -> Result<bool> {
    let attempts = STOP_GRACE.as_millis() / STOP_POLL_INTERVAL.as_millis();

    for _ in 0..attempts {
        if !target_exists(target)? {
            return Ok(true);
        }

        sleep(STOP_POLL_INTERVAL);
    }

    Ok(false)
}

/// Waits until a force-killed stop target no longer exists.
fn wait_for_kill(target: StopTarget) -> Result<()> {
    if wait_for_exit(target)? {
        return Ok(());
    }

    bail!("{target} did not exit after KILL")
}

/// Returns the negative pid used by kill(2) to target a process group.
fn process_group_pid(pid: Pid) -> Pid {
    Pid::from_raw(-pid.as_raw())
}

/// Returns whether a stop target exists according to signal 0.
fn target_exists(target: StopTarget) -> Result<bool> {
    match target {
        StopTarget::ProcessGroup(pid) => process_group_exists(pid),
        StopTarget::Process(pid) => process_exists(pid),
    }
}

/// Returns whether a process group exists according to signal 0.
fn process_group_exists(pid: Pid) -> Result<bool> {
    match kill(process_group_pid(pid), None) {
        Ok(()) => Ok(true),
        Err(Errno::ESRCH) => Ok(false),
        Err(err) => Err(err).with_context(|| format!("failed to check process group {pid}")),
    }
}

/// Returns whether a process exists according to signal 0.
fn process_exists(pid: Pid) -> Result<bool> {
    match kill(pid, None) {
        Ok(()) => Ok(true),
        Err(Errno::ESRCH) => Ok(false),
        Err(err) => Err(err).with_context(|| format!("failed to check process {pid}")),
    }
}

/// Parses a process id from pid file contents.
fn parse_pid(pid_text: &str) -> Result<Pid> {
    let pid = pid_text.trim();

    if pid.is_empty() {
        bail!("pid file is empty");
    }

    let pid = pid
        .parse::<i32>()
        .with_context(|| format!("invalid pid file contents {pid:?}"))?;

    if pid <= 0 {
        return Err(anyhow!("pid file contains invalid pid 0"));
    }

    Ok(Pid::from_raw(pid))
}

impl std::fmt::Display for StopTarget {
    /// Formats a stop target for diagnostic messages.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StopTarget::ProcessGroup(pid) => write!(formatter, "process group {pid}"),
            StopTarget::Process(pid) => write!(formatter, "process {pid}"),
        }
    }
}
