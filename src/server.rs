use std::{
    env, fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Command as StdCommand, ExitStatus, Stdio},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use anyhow::{Context, Error, Result, anyhow};
use axum::{
    Router,
    extract::{State, WebSocketUpgrade, ws::Message},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::IntoResponse,
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, Command as TokioCommand},
    sync::{mpsc, oneshot},
};
use tokio::{task::JoinHandle, time::sleep};

use crate::{
    cli::ServerArgs,
    config::{
        ALLOW_ENV_VAR, CLIENT_CWD_ENV_VAR, CLIENT_EXEC_ENV_VALUE, CLIENT_EXEC_ENV_VAR,
        CLIENT_HOSTNAME_ENV_VAR, CLIENT_USERNAME_ENV_VAR, DAEMON_READY_FILE_ENV_VAR,
        DEFAULT_LOG_FILE, HOST_ENV_VAR, LOG_FILE_ENV_VAR, PID_FILE_ENV_VAR, PORT_ENV_VAR,
        SECRET_ENV_VAR, SSH_FORWARD_ENV_VAR, SSH_FORWARD_PORT_ENV_VAR, TEST_READY_FILE_ENV_VAR,
        expand_home_path,
    },
    log, logging,
    protocol::{
        BYTE_FRAME_HEADER_SIZE, CHUNK_SIZE, ClientFrame, ServerControlFrame,
        put_stderr_frame_header, put_stdout_frame_header,
    },
};

/// Number of queued actor messages allowed before producers apply backpressure.
const CHANNEL_CAPACITY: usize = 8;

/// Time allowed for a cancelled command to exit after a termination request.
const TERMINATION_GRACE: Duration = Duration::from_secs(2);

/// Authorization header prefix expected during websocket upgrade authentication.
const AUTH_PREFIX: &str = "Bearer ";

/// Polling delay used while the daemon parent waits for child startup readiness.
const DAEMON_READY_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Status written by the daemon child after successful startup.
const DAEMON_READY_OK: &str = "OK\n";

/// Prefix written by the daemon child before a startup error message.
const DAEMON_READY_ERR_PREFIX: &str = "ERR ";

/// Shared HTTP application state passed to websocket handlers.
#[derive(Clone)]
struct AppState {
    /// Secret expected in the websocket upgrade authorization header.
    secret: Arc<str>,
    /// Command names allowed for execution when allow-list mode is active.
    allowed_commands: Arc<[String]>,
    /// Log file path propagated into websocket upgrade tasks.
    log_file: Option<Arc<std::path::PathBuf>>,
    /// Sender for the command execution actor.
    actor: mpsc::Sender<ServerActorMessage>,
}

/// Messages accepted by the server command actor.
enum ServerActorMessage {
    /// Request to execute one command and stream its events.
    Exec {
        /// Program and arguments to execute.
        command: Vec<String>,
        /// Hostname of the client machine that requested the command.
        client_hostname: String,
        /// Username of the client user that requested the command.
        client_username: String,
        /// Working directory of the client process that requested the command.
        client_cwd: String,
        /// Output event channel used to stream command results.
        events: mpsc::Sender<ExecEvent>,
        /// Channel carrying stdin chunks received from the websocket client.
        stdin: mpsc::Receiver<Vec<u8>>,
        /// Cancellation signal received when the client aborts the command.
        cancel: oneshot::Receiver<()>,
        /// Completion channel used to report setup or execution errors.
        done: oneshot::Sender<Result<()>>,
    },
}

/// Events produced by a running command.
enum ExecEvent {
    /// A pre-encoded stdout or stderr websocket payload.
    Output(Vec<u8>),
    /// Final process exit code.
    Exit(i32),
}

/// Output stream selector for process pipe readers.
enum StreamKind {
    /// Read chunks should be marked as stdout.
    Stdout,
    /// Read chunks should be marked as stderr.
    Stderr,
}

/// Starts the Axum websocket server and command actor.
pub async fn run_server(args: ServerArgs) -> Result<()> {
    // `server --daemon` starts as a short-lived launcher. It re-execs the same
    // binary with a readiness-file env var; the respawned child sees that env
    // var and skips `start_daemon`, so only the launcher returns to the shell.
    if args.daemon && env::var_os(DAEMON_READY_FILE_ENV_VAR).is_none() {
        return start_daemon(args).await;
    }

    let ready_file = server_ready_file_from_env();
    let result = run_server_foreground(args, ready_file.clone()).await;

    if let (Some(path), Err(err)) = (&ready_file, &result) {
        let _ = write_daemon_ready_error(path, err);
    }

    result
}

/// Returns the server readiness file path from daemon or test-only environment variables.
fn server_ready_file_from_env() -> Option<PathBuf> {
    env::var_os(DAEMON_READY_FILE_ENV_VAR)
        .or_else(|| env::var_os(TEST_READY_FILE_ENV_VAR))
        .map(PathBuf::from)
}

/// Runs the server in the current process after resolving file defaults.
async fn run_server_foreground(args: ServerArgs, ready_file: Option<PathBuf>) -> Result<()> {
    let pid_file = effective_pid_file(&args)?;
    let log_file = effective_log_file(&args)?.map(Arc::new);

    if let Some(path) = log_file.as_deref() {
        logging::prepare_log_file(path.as_path())
            .with_context(|| format!("failed to prepare log file {}", path.display()))?;
    }

    logging::scope_log_file(log_file.clone(), async move {
        let startup_args = format_server_args(&args);
        let addr: SocketAddr = format!("{}:{}", args.connection.host, args.connection.port)
            .parse()
            .context("invalid host or port")?;
        let host = args.connection.host.clone();
        let port = args.connection.port;
        let secret = args.connection.secret.clone();
        let ssh_forward = args.ssh_forward.clone();
        let ssh_forward_port = args.ssh_forward_port;
        let (actor, actor_rx) = mpsc::channel(CHANNEL_CAPACITY);

        logging::spawn_with_current_log_file(server_actor(actor_rx));

        let app = Router::new()
            .route("/ws", get(ws_handler))
            .with_state(AppState {
                secret: Arc::from(args.connection.secret),
                allowed_commands: Arc::from(args.allow),
                log_file,
                actor,
            });

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("failed to bind {addr}"))?;
        let _pid_file = PidFile::write(&pid_file)?;

        log!(logging::Level::Info, "server args {startup_args}");
        log!(logging::Level::Info, "server listening on {addr}");
        log!(
            logging::Level::Info,
            "Try with: hostcmd exec --secret {} --port {port} --host {host} -- uname -a",
            secret
        );

        let server = axum::serve(listener, app).into_future();

        if let Some(hostname) = ssh_forward {
            let mut ssh_forward = spawn_ssh_forward(hostname, ssh_forward_port, &host, port)?;
            if let Some(path) = ready_file.as_deref() {
                write_daemon_ready_ok(path)?;
            }

            tokio::select! {
                result = server => {
                    ssh_forward.abort();
                    result.context("server failed")
                }
                result = &mut ssh_forward => result
                    .context("ssh forward monitor task panicked")?
                    .context("ssh forward exited"),
            }
        } else {
            if let Some(path) = ready_file.as_deref() {
                write_daemon_ready_ok(path)?;
            }

            server.await.context("server failed")
        }
    })
    .await
}

/// Starts a detached daemon child and waits until it reports startup success or failure.
async fn start_daemon(args: ServerArgs) -> Result<()> {
    let pid_file = effective_pid_file(&args)?;
    let log_file = effective_daemon_log_file(&args)?;
    create_parent_directories(&pid_file)
        .with_context(|| format!("failed to create pid file directory {}", pid_file.display()))?;
    create_parent_directories(&log_file)
        .with_context(|| format!("failed to create log file directory {}", log_file.display()))?;

    let ready_file = daemon_ready_file_path(&pid_file)?;
    let current_exe = env::current_exe().context("failed to locate current executable")?;
    let mut command = StdCommand::new(current_exe);
    // Detach is implemented as a self-reexec. The launcher starts a second copy
    // with stdio disconnected from the terminal and passes the resolved runtime
    // configuration through environment variables instead of CLI defaults.
    command
        .arg("server")
        .arg("--daemon")
        .env(SECRET_ENV_VAR, &args.connection.secret)
        .env(PORT_ENV_VAR, args.connection.port.to_string())
        .env(HOST_ENV_VAR, &args.connection.host)
        .env(LOG_FILE_ENV_VAR, &log_file)
        .env(PID_FILE_ENV_VAR, &pid_file)
        .env(DAEMON_READY_FILE_ENV_VAR, &ready_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    if let Some(ssh_forward) = args.ssh_forward.as_deref() {
        command.env(SSH_FORWARD_ENV_VAR, ssh_forward);
    } else {
        command.env_remove(SSH_FORWARD_ENV_VAR);
    }

    if let Some(ssh_forward_port) = args.ssh_forward_port {
        command.env(SSH_FORWARD_PORT_ENV_VAR, ssh_forward_port.to_string());
    } else {
        command.env_remove(SSH_FORWARD_PORT_ENV_VAR);
    }

    if args.allow.is_empty() {
        command.env_remove(ALLOW_ENV_VAR);
    } else {
        command.env(ALLOW_ENV_VAR, args.allow.join(","));
    }

    #[cfg(unix)]
    // Put the child in its own process group so terminal signals aimed at the
    // launcher's foreground job do not keep following the daemon after spawn.
    command.process_group(0);

    let mut child = command.spawn().context("failed to start daemon child")?;
    // The original process stays around only long enough to observe whether the
    // detached child bound its socket and wrote readiness status.
    wait_for_daemon_start(&mut child, &ready_file).await
}

/// Waits for the daemon child to write readiness status or exit early.
async fn wait_for_daemon_start(child: &mut std::process::Child, ready_file: &Path) -> Result<()> {
    loop {
        if let Some(status) = child.try_wait().context("failed to poll daemon child")? {
            let message = read_daemon_ready_file(ready_file).unwrap_or_default();
            let _ = fs::remove_file(ready_file);

            if let Some(err) = parse_daemon_ready_error(&message) {
                return Err(anyhow!(err.to_string()));
            }

            return Err(anyhow!(
                "daemon child exited before startup completed: {status}"
            ));
        }

        match read_daemon_ready_file(ready_file) {
            Ok(message) if message == DAEMON_READY_OK => {
                let _ = fs::remove_file(ready_file);
                return Ok(());
            }
            Ok(message) => {
                if let Some(err) = parse_daemon_ready_error(&message) {
                    let _ = fs::remove_file(ready_file);
                    return Err(anyhow!(err.to_string()));
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "failed to read daemon readiness file {}",
                        ready_file.display()
                    )
                });
            }
        }

        sleep(DAEMON_READY_POLL_INTERVAL).await;
    }
}

/// Returns the effective server pid file path, including the default state location.
fn effective_pid_file(args: &ServerArgs) -> Result<PathBuf> {
    expand_home_path(&args.pid_file)
}

/// Returns the effective server log file path when one should be used.
fn effective_log_file(args: &ServerArgs) -> Result<Option<PathBuf>> {
    if args.daemon {
        return Ok(Some(effective_daemon_log_file(args)?));
    }

    Ok(args.log_file.clone())
}

/// Returns the daemon log file path, using the default when no log file was configured.
fn effective_daemon_log_file(args: &ServerArgs) -> Result<PathBuf> {
    match args.log_file.as_deref() {
        Some(path) => expand_home_path(path),
        None => expand_home_path(Path::new(DEFAULT_LOG_FILE)),
    }
}

/// Returns a temporary readiness file path beside the pid file.
fn daemon_ready_file_path(pid_file: &Path) -> Result<PathBuf> {
    let parent = pid_file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let mut path = parent.map(Path::to_path_buf).unwrap_or_else(env::temp_dir);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_nanos();
    path.push(format!("server.ready.{}.{}", std::process::id(), now));
    Ok(path)
}

/// Writes successful daemon readiness status for the parent process.
fn write_daemon_ready_ok(path: &Path) -> Result<()> {
    fs::write(path, DAEMON_READY_OK)
        .with_context(|| format!("failed to write daemon readiness file {}", path.display()))
}

/// Writes daemon startup error status for the parent process.
fn write_daemon_ready_error(path: &Path, err: &Error) -> Result<()> {
    fs::write(path, format!("{DAEMON_READY_ERR_PREFIX}{err:#}\n"))
        .with_context(|| format!("failed to write daemon readiness file {}", path.display()))
}

/// Reads the daemon readiness file as a UTF-8 string.
fn read_daemon_ready_file(path: &Path) -> std::io::Result<String> {
    fs::read_to_string(path)
}

/// Extracts a daemon startup error from readiness file contents.
fn parse_daemon_ready_error(message: &str) -> Option<&str> {
    message
        .strip_prefix(DAEMON_READY_ERR_PREFIX)
        .map(str::trim_end)
}

/// Creates all parent directories for a file path when the path has a parent directory.
fn create_parent_directories(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }

    Ok(())
}

/// Guard that writes the server pid file during startup.
struct PidFile {
    /// Path to the pid file currently owned by this server process.
    path: PathBuf,
}

impl PidFile {
    /// Writes the current process id to the pid file and returns its cleanup guard.
    fn write(path: &Path) -> Result<Self> {
        create_parent_directories(path)
            .with_context(|| format!("failed to create pid file directory {}", path.display()))?;
        fs::write(path, format!("{}\n", std::process::id()))
            .with_context(|| format!("failed to write pid file {}", path.display()))?;

        Ok(Self {
            path: path.to_path_buf(),
        })
    }
}

impl Drop for PidFile {
    /// Removes the pid file when the server future exits normally.
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Formats effective server startup arguments for logging.
fn format_server_args(args: &ServerArgs) -> String {
    let log_file = args
        .log_file
        .as_deref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "-".to_string());
    let pid_file = args.pid_file.display().to_string();
    let ssh_forward = args.ssh_forward.as_deref().unwrap_or("-");
    let ssh_forward_port = args
        .ssh_forward_port
        .map(|port| port.to_string())
        .unwrap_or_else(|| "-".to_string());
    let allow = if args.allow.is_empty() {
        "*".to_string()
    } else {
        args.allow.join(",")
    };

    format!(
        "host={} port={} secret={} log_file={} pid_file={} daemon={} ssh_forward={} ssh_forward_port={} allow={}",
        args.connection.host,
        args.connection.port,
        args.connection.secret,
        log_file,
        pid_file,
        args.daemon,
        ssh_forward,
        ssh_forward_port,
        allow
    )
}

/// Starts an SSH reverse-forward process and returns a task that resolves when it exits.
fn spawn_ssh_forward(
    hostname: String,
    ssh_port: Option<u16>,
    host: &str,
    port: u16,
) -> Result<JoinHandle<Result<()>>> {
    let remote = format!("{port}:{host}:{port}");
    let ssh_port_flag = ssh_port
        .map(|port| format!(" -p {port}"))
        .unwrap_or_default();
    let command_line =
        format!("ssh{ssh_port_flag} {hostname} -R {remote} -N -o ExitOnForwardFailure=yes");
    log!(logging::Level::Info, "starting ssh forward: {command_line}");

    let mut command = TokioCommand::new("ssh");
    if let Some(ssh_port) = ssh_port {
        command.arg("-p").arg(ssh_port.to_string());
    }

    command
        .arg(&hostname)
        .args([
            "-v",
            "-R",
            remote.as_str(),
            "-N",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            "-o",
            "ExitOnForwardFailure=yes",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let child = command
        .spawn()
        .with_context(|| format!("failed to start ssh forward: {command_line}"))?;

    Ok(logging::spawn_with_current_log_file(async move {
        let output = child
            .wait_with_output()
            .await
            .with_context(|| format!("failed to wait for ssh forward: {command_line}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        Err(anyhow!(
            "ssh forward process exited: command={command_line:?}, status={}, code={:?}, stdout={stdout:?}, stderr={stderr:?}",
            output.status,
            output.status.code()
        ))
    }))
}

/// Checks upgrade authentication and creates a websocket connection for valid clients.
async fn ws_handler(
    headers: HeaderMap,
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let Some(authorization) = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };

    let Some(secret) = authorization.strip_prefix(AUTH_PREFIX) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };

    if secret != state.secret.as_ref() {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let log_file = state.log_file.clone();

    ws.on_upgrade(move |socket| logging::scope_log_file(log_file, handle_ws(socket, state)))
        .into_response()
}

/// Handles one websocket connection from request frame through output streaming.
async fn handle_ws(socket: axum::extract::ws::WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();

    let (command, client_hostname, client_username, client_cwd) = match receiver.next().await {
        Some(Ok(Message::Binary(bytes))) => match ClientFrame::decode(&bytes) {
            Ok(ClientFrame::Exec {
                command,
                client_hostname,
                client_username,
                client_cwd,
            }) => (command, client_hostname, client_username, client_cwd),
            Ok(ClientFrame::Cancel) => {
                let _ = sender
                    .send(Message::Binary(
                        ServerControlFrame::Error("expected exec frame before cancel".to_string())
                            .encode()
                            .into(),
                    ))
                    .await;
                let _ = sender.close().await;
                return;
            }
            Ok(ClientFrame::Stdin(_)) | Ok(ClientFrame::StdinEof) => {
                let _ = sender
                    .send(Message::Binary(
                        ServerControlFrame::Error("expected exec frame before stdin".to_string())
                            .encode()
                            .into(),
                    ))
                    .await;
                let _ = sender.close().await;
                return;
            }
            Err(err) => {
                let _ = sender
                    .send(Message::Binary(
                        ServerControlFrame::Error(err.to_string()).encode().into(),
                    ))
                    .await;
                let _ = sender.close().await;
                return;
            }
        },
        Some(Ok(_)) => {
            let _ = sender
                .send(Message::Binary(
                    ServerControlFrame::Error("expected binary exec frame".to_string())
                        .encode()
                        .into(),
                ))
                .await;
            let _ = sender.close().await;
            return;
        }
        _ => return,
    };

    if !is_command_allowed(&state.allowed_commands, &command) {
        log!(
            logging::Level::Error,
            "rejecting disallowed command: {:?}",
            command
        );
        let _ = sender
            .send(Message::Binary(
                ServerControlFrame::Error("command is not allowed".to_string())
                    .encode()
                    .into(),
            ))
            .await;
        let _ = sender.close().await;
        return;
    }

    let command_for_cancel = command.clone();

    let (events_tx, mut events_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let (stdin_tx, stdin_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let mut stdin_tx = Some(stdin_tx);
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let mut cancel_tx = Some(cancel_tx);
    let (done_tx, done_rx) = oneshot::channel();
    let mut read_client_messages = true;

    if state
        .actor
        .send(ServerActorMessage::Exec {
            command,
            client_hostname,
            client_username,
            client_cwd,
            events: events_tx,
            stdin: stdin_rx,
            cancel: cancel_rx,
            done: done_tx,
        })
        .await
        .is_err()
    {
        let _ = sender
            .send(Message::Binary(
                ServerControlFrame::Error("server actor stopped".to_string())
                    .encode()
                    .into(),
            ))
            .await;
        let _ = sender.close().await;
        return;
    }

    loop {
        tokio::select! {
            event = events_rx.recv() => {
                let Some(event) = event else {
                    break;
                };
                let frame = match event {
                    ExecEvent::Output(bytes) => bytes,
                    ExecEvent::Exit(code) => ServerControlFrame::Exit(code).encode(),
                };

                if sender
                    .send(Message::Binary(frame.into()))
                    .await
                    .is_err()
                {
                    log!(
                        logging::Level::Info,
                        "cancelling command because websocket output send failed: {:?}",
                        command_for_cancel
                    );
                    if let Some(cancel_tx) = cancel_tx.take() {
                        let _ = cancel_tx.send(());
                    }
                    return;
                }
            }
            message = receiver.next(), if read_client_messages => {
                match message {
                    Some(Ok(Message::Binary(bytes))) => {
                        match ClientFrame::decode(&bytes) {
                            Ok(ClientFrame::Cancel) => {
                                log!(
                                    logging::Level::Info,
                                    "received client cancel request for command: {:?}",
                                    command_for_cancel
                                );
                                if let Some(cancel_tx) = cancel_tx.take() {
                                    let _ = cancel_tx.send(());
                                }
                                read_client_messages = false;
                            }
                            Ok(ClientFrame::Stdin(bytes)) => {
                                if let Some(sender) = &stdin_tx
                                    && sender.send(bytes).await.is_err()
                                {
                                    stdin_tx = None;
                                }
                            }
                            Ok(ClientFrame::StdinEof) => {
                                stdin_tx = None;
                            }
                            Ok(ClientFrame::Exec { .. }) => {
                                log!(
                                    logging::Level::Error,
                                    "ignoring duplicate exec frame for command: {:?}",
                                    command_for_cancel
                                );
                            }
                            Err(err) => {
                                log!(
                                    logging::Level::Error,
                                    "cancelling command because client frame decode failed: {:?}: {err}",
                                    command_for_cancel
                                );
                                if let Some(cancel_tx) = cancel_tx.take() {
                                    let _ = cancel_tx.send(());
                                }
                                read_client_messages = false;
                            }
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        log!(
                            logging::Level::Info,
                            "cancelling command because websocket closed: {:?}: {:?}",
                            command_for_cancel,
                            frame
                        );
                        if let Some(cancel_tx) = cancel_tx.take() {
                            let _ = cancel_tx.send(());
                        }
                        read_client_messages = false;
                    }
                    None => {
                        log!(
                            logging::Level::Info,
                            "cancelling command because websocket stream ended: {:?}",
                            command_for_cancel
                        );
                        if let Some(cancel_tx) = cancel_tx.take() {
                            let _ = cancel_tx.send(());
                        }
                        read_client_messages = false;
                    }
                    Some(Err(err)) => {
                        log!(
                            logging::Level::Error,
                            "cancelling command because websocket read failed: {:?}: {err}",
                            command_for_cancel
                        );
                        if let Some(cancel_tx) = cancel_tx.take() {
                            let _ = cancel_tx.send(());
                        }
                        read_client_messages = false;
                    }
                    Some(Ok(_)) => {}
                }
            }
        }
    }

    match done_rx.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            let _ = sender
                .send(Message::Binary(
                    ServerControlFrame::Error(err.to_string()).encode().into(),
                ))
                .await;
        }
        Err(_) => {
            let _ = sender
                .send(Message::Binary(
                    ServerControlFrame::Error("exec task stopped".to_string())
                        .encode()
                        .into(),
                ))
                .await;
        }
    }

    let _ = sender.close().await;
}

/// Returns whether the requested command is executable under the configured allow list.
fn is_command_allowed(allowed_commands: &[String], command: &[String]) -> bool {
    if allowed_commands.is_empty() {
        return true;
    }

    command
        .first()
        .is_some_and(|program| allowed_commands.iter().any(|allowed| allowed == program))
}

/// Unit tests for allow-list command checks.
#[cfg(test)]
mod tests {
    use super::is_command_allowed;

    /// Verifies that an empty allow list permits any requested command.
    #[test]
    fn empty_allow_list_permits_all_commands() {
        assert!(
            is_command_allowed(&[], &["false".to_string()]),
            "an empty allow list should permit commands when allow-list mode is disabled"
        );
    }

    /// Verifies that commands outside the allow list are rejected.
    #[test]
    fn allow_list_rejects_unknown_command() {
        assert!(
            !is_command_allowed(&["true".to_string()], &["false".to_string()]),
            "commands missing from the allow list should be rejected"
        );
    }
}

/// Receives server actor messages and spawns isolated command execution tasks.
async fn server_actor(mut rx: mpsc::Receiver<ServerActorMessage>) {
    while let Some(message) = rx.recv().await {
        match message {
            ServerActorMessage::Exec {
                command,
                client_hostname,
                client_username,
                client_cwd,
                events,
                stdin,
                cancel,
                done,
            } => {
                logging::spawn_with_current_log_file(async move {
                    let result = execute_command(
                        command.clone(),
                        client_hostname,
                        client_username,
                        client_cwd,
                        events,
                        stdin,
                        cancel,
                    )
                    .await;
                    if let Err(err) = &result {
                        log_command_error(&command, err);
                    }
                    let _ = done.send(result);
                });
            }
        }
    }
}

/// Spawns a process and streams stdout, stderr, and exit status as actor events.
async fn execute_command(
    command: Vec<String>,
    client_hostname: String,
    client_username: String,
    client_cwd: String,
    events: mpsc::Sender<ExecEvent>,
    stdin: mpsc::Receiver<Vec<u8>>,
    mut cancel: oneshot::Receiver<()>,
) -> Result<()> {
    log!(
        logging::Level::Info,
        "executing command from client hostname {client_hostname:?}, username {client_username:?}, cwd {client_cwd:?}: {:?}",
        command
    );

    let (program, args) = command
        .split_first()
        .ok_or_else(|| anyhow!("missing command"))?;

    let mut command_builder = TokioCommand::new(program);
    configure_command(&mut command_builder);
    command_builder.env(CLIENT_HOSTNAME_ENV_VAR, &client_hostname);
    command_builder.env(CLIENT_USERNAME_ENV_VAR, &client_username);
    command_builder.env(CLIENT_CWD_ENV_VAR, &client_cwd);
    command_builder.env(CLIENT_EXEC_ENV_VAR, CLIENT_EXEC_ENV_VALUE);

    let mut child = command_builder
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {program}"))?;

    let child_stdin = child.stdin.take().context("failed to capture stdin")?;
    let stdout = child.stdout.take().context("failed to capture stdout")?;
    let stderr = child.stderr.take().context("failed to capture stderr")?;

    let stdin_task = logging::spawn_with_current_log_file(stream_stdin(stdin, child_stdin));
    let stdout_task = logging::spawn_with_current_log_file(stream_reader(
        stdout,
        StreamKind::Stdout,
        events.clone(),
    ));
    let stderr_task = logging::spawn_with_current_log_file(stream_reader(
        stderr,
        StreamKind::Stderr,
        events.clone(),
    ));

    let status = tokio::select! {
        status = child.wait() => status.context("failed to wait for command"),
        _ = &mut cancel => {
            log!(
                logging::Level::Info,
                "command task received cancellation signal: {:?}",
                command
            );
            terminate_child(&mut child).await
        }
    };
    stdin_task.abort();

    let result = async {
        let status = status?;
        stdout_task.await.context("stdout task panicked")??;
        stderr_task.await.context("stderr task panicked")??;
        Ok::<(ExitStatus, (), ()), anyhow::Error>((status, (), ()))
    }
    .await;

    let (status, (), ()) = match result {
        Ok(result) => result,
        Err(err) => {
            let _ = child.kill().await;
            log!(
                logging::Level::Error,
                "command failed before exit code: {:?}: {err}",
                command
            );
            return Err(err).context("command execution failed");
        }
    };

    let code = exit_code(status);

    if code == 0 {
        log!(
            logging::Level::Info,
            "command exited with code {code}: {:?}",
            command
        );
    } else {
        log!(
            logging::Level::Error,
            "command exited with non-zero code {code}: {:?}",
            command
        );
    }

    events
        .send(ExecEvent::Exit(code))
        .await
        .context("client disconnected before exit code was sent")?;

    Ok(())
}

/// Writes client-provided stdin chunks into the child process stdin pipe.
async fn stream_stdin(
    mut stdin: mpsc::Receiver<Vec<u8>>,
    mut child_stdin: tokio::process::ChildStdin,
) -> Result<()> {
    while let Some(chunk) = stdin.recv().await {
        child_stdin
            .write_all(&chunk)
            .await
            .context("failed to write command stdin")?;
    }

    child_stdin
        .shutdown()
        .await
        .context("failed to close command stdin")
}

/// Reads one child output pipe and sends bounded chunks to the websocket writer.
async fn stream_reader<R>(
    mut reader: R,
    kind: StreamKind,
    events: mpsc::Sender<ExecEvent>,
) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    loop {
        let mut frame = vec![0; BYTE_FRAME_HEADER_SIZE + CHUNK_SIZE];
        let read = reader.read(&mut frame[BYTE_FRAME_HEADER_SIZE..]).await?;
        if read == 0 {
            return Ok(());
        }

        match kind {
            StreamKind::Stdout => {
                put_stdout_frame_header(&mut frame[..BYTE_FRAME_HEADER_SIZE], read);
            }
            StreamKind::Stderr => {
                put_stderr_frame_header(&mut frame[..BYTE_FRAME_HEADER_SIZE], read);
            }
        };
        frame.truncate(BYTE_FRAME_HEADER_SIZE + read);

        events
            .send(ExecEvent::Output(frame))
            .await
            .context("client disconnected while streaming output")?;
    }
}

/// Converts a platform exit status into a process-style exit code.
fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

/// Logs a command execution error with its full context chain.
fn log_command_error(command: &[String], err: &Error) {
    log!(
        logging::Level::Error,
        "command execution failed: {:?}: {err:#}",
        command
    );
}

/// Requests graceful child termination and force-kills it after a short grace period.
async fn terminate_child(child: &mut Child) -> Result<ExitStatus> {
    if let Some(pid) = child.id() {
        log!(
            logging::Level::Info,
            "sending TERM to command process group: pid={pid}"
        );
        send_termination_signal(pid, "TERM").await;
    } else {
        log!(
            logging::Level::Info,
            "cancelled command has no process id before TERM"
        );
    }

    if let Ok(status) = tokio::time::timeout(TERMINATION_GRACE, child.wait()).await {
        match &status {
            Ok(status) => log!(
                logging::Level::Info,
                "cancelled command exited during grace period: status={status}"
            ),
            Err(err) => log!(
                logging::Level::Error,
                "failed waiting for cancelled command during grace period: {err}"
            ),
        }
        return status.context("failed to wait for cancelled command");
    }

    log!(
        logging::Level::Error,
        "cancelled command did not exit within {}s grace period",
        TERMINATION_GRACE.as_secs()
    );

    if let Some(pid) = child.id() {
        log!(
            logging::Level::Error,
            "sending KILL to command process group: pid={pid}"
        );
        send_termination_signal(pid, "KILL").await;
    } else {
        log!(
            logging::Level::Error,
            "cancelled command has no process id before KILL; using child.kill"
        );
        child
            .kill()
            .await
            .context("failed to kill cancelled command")?;
    }

    let status = child
        .wait()
        .await
        .context("failed to wait for killed command")?;
    log!(
        logging::Level::Error,
        "cancelled command exited after KILL: status={status}"
    );
    Ok(status)
}

/// Configures a spawned command so cancellation can target its process group.
#[cfg(unix)]
fn configure_command(command: &mut TokioCommand) {
    command.process_group(0);
}

/// Leaves command configuration unchanged on platforms without process groups.
#[cfg(not(unix))]
fn configure_command(_command: &mut TokioCommand) {}

/// Sends a Unix signal to the spawned command process group.
#[cfg(unix)]
async fn send_termination_signal(pid: u32, signal: &str) {
    let group = format!("-{pid}");
    let status = TokioCommand::new("kill")
        .args([format!("-{signal}"), group])
        .status()
        .await;
    match status {
        Ok(status) => log!(
            logging::Level::Info,
            "sent {signal} to command process group {pid}: status={status}"
        ),
        Err(err) => log!(
            logging::Level::Error,
            "failed to send {signal} to command process group {pid}: {err}"
        ),
    }
}

/// Sends no explicit signal on platforms without process group support.
#[cfg(not(unix))]
async fn send_termination_signal(_pid: u32, _signal: &str) {}
