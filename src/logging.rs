use std::{
    fmt,
    fs::OpenOptions,
    future::Future,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::task::JoinHandle;

tokio::task_local! {
    /// Optional server log file path carried through explicitly scoped tasks.
    static LOG_FILE: Option<Arc<PathBuf>>;
}

/// Severity level attached to a server log message.
pub enum Level {
    /// Informational message about normal server activity.
    Info,
    /// Error message about failed server activity.
    Error,
}

impl fmt::Display for Level {
    /// Formats the severity level as a stable uppercase label.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Info => write!(f, "INFO"),
            Self::Error => write!(f, "ERROR"),
        }
    }
}

/// Writes one formatted server log line to stdout or the scoped log file.
pub fn log(level: Level, message: String) {
    let line = format!("{}: {}\n", level, message);

    if let Some(log_file) = current_log_file() {
        if let Err(err) = write_log_file_line(log_file.as_ref(), &line) {
            let mut stderr = io::stderr().lock();
            let _ = writeln!(
                stderr,
                "ERROR: failed to write log file {}: {err}",
                log_file.display()
            );
            let _ = stderr.write_all(line.as_bytes());
        }
    } else {
        let _ = write_stdout_line(&line);
    }
}

/// Ensures the configured log file can be created before server startup completes.
pub fn prepare_log_file(path: &Path) -> io::Result<()> {
    create_parent_directories(path)?;
    OpenOptions::new().create(true).append(true).open(path)?;
    Ok(())
}

/// Runs a future with the provided log file stored in the current Tokio task local.
pub async fn scope_log_file<F>(log_file: Option<Arc<PathBuf>>, future: F) -> F::Output
where
    F: Future,
{
    LOG_FILE.scope(log_file, future).await
}

/// Returns the current Tokio task-local log file path, if one is configured.
pub fn current_log_file() -> Option<Arc<PathBuf>> {
    LOG_FILE.try_with(Clone::clone).ok().flatten()
}

/// Spawns a task that inherits the current Tokio task-local log file path.
pub fn spawn_with_current_log_file<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let log_file = current_log_file();
    tokio::spawn(async move { scope_log_file(log_file, future).await })
}

/// Appends one formatted log line to the configured log file path.
fn write_log_file_line(path: &Path, line: &str) -> io::Result<()> {
    create_parent_directories(path)?;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())
}

/// Creates all parent directories for a file path when the path has a parent directory.
fn create_parent_directories(path: &Path) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }

    Ok(())
}

/// Writes one formatted log line to stdout.
fn write_stdout_line(line: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(line.as_bytes())
}

/// Logs a formatted server message at the provided severity level.
#[macro_export]
macro_rules! log {
    ($level:expr, $($arg:tt)*) => {
        $crate::logging::log($level, format!($($arg)*))
    };
}
