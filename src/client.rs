use std::env;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        Message as ClientMessage,
        client::IntoClientRequest,
        http::{HeaderValue, header::AUTHORIZATION},
    },
};

use crate::{
    cli::ExecArgs,
    protocol::{CHUNK_SIZE, ClientFrame, DecodedServerFrame},
};

/// Connects to the server, sends an exec request, and mirrors remote output locally.
pub async fn run_client(args: ExecArgs) -> Result<()> {
    match run_client_session(args).await {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("Error: {err:#}");
            std::process::exit(1);
        }
    }
}

/// Runs one client websocket session and returns the remote exit code.
async fn run_client_session(args: ExecArgs) -> Result<i32> {
    let client_hostname = resolve_client_hostname()?;
    let client_username = resolve_client_username()?;
    let client_cwd = resolve_client_cwd()?;
    let url = format!("ws://{}:{}/ws", args.connection.host, args.connection.port);
    let mut request = url.as_str().into_client_request()?;

    let authorization = format!("Bearer {}", args.connection.secret);
    request.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_str(&authorization).context("invalid authorization header value")?,
    );

    let (socket, _) = connect_async(request)
        .await
        .with_context(|| format!("failed to connect to {url}"))?;
    let (mut sender, mut receiver) = socket.split();
    let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<ClientFrame>(8);

    tokio::spawn(async move {
        let result = stream_stdin(stdin_tx.clone()).await;
        if result.is_err() {
            let _ = stdin_tx.send(ClientFrame::Cancel).await;
        }
        result
    });

    sender
        .send(ClientMessage::Binary(
            ClientFrame::Exec {
                command: args.command,
                client_hostname,
                client_username,
                client_cwd,
            }
            .encode()
            .into(),
        ))
        .await
        .context("failed to send exec request")?;

    let mut exit = None;
    let mut read_stdin = true;

    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal.context("failed to listen for ctrl-c")?;
                sender
                    .send(ClientMessage::Binary(ClientFrame::Cancel.encode().into()))
                    .await
                    .context("failed to send cancel request")?;
                let _ = sender.close().await;
                return Ok(130);
            }
            frame = stdin_rx.recv(), if read_stdin => {
                let Some(frame) = frame else {
                    read_stdin = false;
                    continue;
                };
                sender
                    .send(ClientMessage::Binary(frame.encode().into()))
                    .await
                    .context("failed to send stdin frame")?;
                if matches!(frame, ClientFrame::StdinEof | ClientFrame::Cancel) {
                    read_stdin = false;
                }
            }
            message = receiver.next() => {
                let Some(message) = message else {
                    break;
                };
                let message = message.context("websocket read failed")?;
                let ClientMessage::Binary(bytes) = message else {
                    continue;
                };

                match DecodedServerFrame::decode(&bytes)? {
                    DecodedServerFrame::Stdout(bytes) => {
                        let mut stdout = tokio::io::stdout();
                        stdout.write_all(bytes).await?;
                        stdout.flush().await?;
                    }
                    DecodedServerFrame::Stderr(bytes) => {
                        let mut stderr = tokio::io::stderr();
                        stderr.write_all(bytes).await?;
                        stderr.flush().await?;
                    }
                    DecodedServerFrame::Exit(code) => exit = Some(code),
                    DecodedServerFrame::Error(message) => bail!(message),
                }
            }
        }
    }

    Ok(exit.unwrap_or(1))
}

/// Resolves the local machine hostname to send with the exec request.
fn resolve_client_hostname() -> Result<String> {
    let hostname = nix::unistd::gethostname().context("failed to determine client hostname")?;
    let hostname = hostname.to_string_lossy().into_owned();

    if hostname.is_empty() {
        bail!("client hostname is empty")
    }

    Ok(hostname)
}

/// Resolves the local username to send with the exec request.
fn resolve_client_username() -> Result<String> {
    let user = nix::unistd::User::from_uid(nix::unistd::geteuid())
        .context("failed to determine client username")?
        .ok_or_else(|| anyhow!("client user record is missing"))?;

    if user.name.is_empty() {
        bail!("client username is empty")
    }

    Ok(user.name)
}

/// Resolves the local working directory to send with the exec request.
fn resolve_client_cwd() -> Result<String> {
    let cwd = env::current_dir().context("failed to determine client current working directory")?;
    let cwd = cwd.as_os_str().to_string_lossy().into_owned();

    if cwd.is_empty() {
        bail!("client current working directory is empty")
    }

    Ok(cwd)
}

/// Streams local stdin to bounded client protocol frames.
async fn stream_stdin(sender: tokio::sync::mpsc::Sender<ClientFrame>) -> Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut buffer = vec![0; CHUNK_SIZE];

    loop {
        let read = stdin
            .read(&mut buffer)
            .await
            .context("failed to read stdin")?;
        if read == 0 {
            sender
                .send(ClientFrame::StdinEof)
                .await
                .context("websocket closed before stdin eof was sent")?;
            return Ok(());
        }

        sender
            .send(ClientFrame::Stdin(buffer[..read].to_vec()))
            .await
            .context("websocket closed while streaming stdin")?;
    }
}
