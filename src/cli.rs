use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::config::{
    ALLOW_ENV_VAR, COMMAND_ENV_VAR, DEFAULT_PID_FILE, HOST_ENV_VAR, LOG_FILE_ENV_VAR,
    PID_FILE_ENV_VAR, PORT_ENV_VAR, SECRET_ENV_VAR, SSH_FORWARD_ENV_VAR, SSH_FORWARD_PORT_ENV_VAR,
};

/// Command line interface for the hostcmd binary.
#[derive(Parser)]
#[command(author, version, about)]
pub struct Cli {
    /// Subcommand selected by the caller.
    #[command(subcommand)]
    pub command: Commands,
}

/// Top-level actions supported by the hostcmd binary.
#[derive(Subcommand)]
pub enum Commands {
    /// Start a websocket command execution server.
    Server(ServerArgs),
    /// Stop a daemon server using its pid file.
    Stop(StopArgs),
    /// Execute a command through a remote websocket server.
    Exec(ExecArgs),
}

/// Shared connection and authentication options for server and client modes.
///
/// These three fields are required for both the server and client to establish
/// an authenticated WebSocket connection. They can be provided as flags or
/// through their corresponding environment variables for flag-free usage.
#[derive(Args, Clone)]
pub struct ConnectionArgs {
    /// Shared secret for authenticating between client and server.
    ///
    /// The client must present the same secret value that the server was
    /// started with. Requests that do not match are rejected.
    #[arg(long, env = SECRET_ENV_VAR)]
    pub secret: String,

    /// TCP port used for the websocket connection.
    ///
    /// On the server side this is the port to bind and listen on.
    /// On the client side this is the port to connect to.
    #[arg(long, env = PORT_ENV_VAR)]
    pub port: u16,

    /// Host address to bind or connect to.
    ///
    /// On the server side this is the address to bind the listener to
    /// (e.g. `127.0.0.1` for local-only or `0.0.0.0` for all interfaces).
    /// On the client side this is the address of the running server.
    #[arg(long, env = HOST_ENV_VAR)]
    pub host: String,
}

/// Arguments for running the websocket server.
///
/// The server listens for authenticated WebSocket connections and executes
/// commands on behalf of remote clients. It can run in the foreground or as a
/// background daemon, and optionally expose itself through an SSH reverse port
/// forward.
#[derive(Args)]
pub struct ServerArgs {
    /// Shared connection settings for the server listener.
    #[command(flatten)]
    pub connection: ConnectionArgs,

    /// File that receives server log output instead of stdout.
    ///
    /// When `--daemon` is used and no `--log-file` is set, logs are
    /// automatically written to `~/.local/share/hostcmd/server.log`.
    #[arg(long, env = LOG_FILE_ENV_VAR, value_name = "file")]
    pub log_file: Option<PathBuf>,

    /// File that receives the running server process id.
    ///
    /// Used by `hostcmd stop` to locate and terminate the daemon.
    /// Override this when running multiple server instances.
    #[arg(
        long,
        env = PID_FILE_ENV_VAR,
        value_name = "file",
        default_value = DEFAULT_PID_FILE
    )]
    pub pid_file: PathBuf,

    /// Start the server in the background as a daemon process.
    ///
    /// The daemon detaches from the terminal and writes its pid to
    /// `--pid-file`. Stop it later with `hostcmd stop`.
    #[arg(long)]
    pub daemon: bool,

    /// SSH hostname used to expose the server through a reverse port forward.
    ///
    /// When set, the server runs:
    ///
    ///   ssh <hostname> -R <port>:<host>:<port> -N -o ExitOnForwardFailure=yes
    ///
    /// This makes the server reachable from the remote SSH host. The server
    /// and SSH process are coupled: if the SSH process exits for any reason
    /// (connection drop, authentication failure, remote shutdown) the server
    /// exits immediately as well.
    #[arg(long, env = SSH_FORWARD_ENV_VAR, value_name = "hostname", verbatim_doc_comment)]
    pub ssh_forward: Option<String>,

    /// SSH port used when connecting to the reverse port forward host.
    ///
    /// Adds `-p <port>` to the `ssh` command started by `--ssh-forward`.
    /// Only meaningful when `--ssh-forward` is also provided.
    #[arg(long, env = SSH_FORWARD_PORT_ENV_VAR, value_name = "port")]
    pub ssh_forward_port: Option<u16>,

    /// Command names allowed for remote execution.
    ///
    /// Can be repeated to allow multiple commands:
    ///
    ///   --allow pbcopy --allow uname
    ///
    /// Via the environment variable, use comma-separated values:
    ///
    ///   HOSTCMD_ALLOW=pbcopy,uname
    ///
    /// When any allow rule is set, the server rejects commands whose
    /// executable name is not in the list. When no allow rules are set,
    /// all commands are permitted.
    #[arg(
        long = "allow",
        env = ALLOW_ENV_VAR,
        value_name = "cmd",
        value_delimiter = ',',
        verbatim_doc_comment
    )]
    pub allow: Vec<String>,
}

/// Arguments for executing a command through the websocket server.
///
/// Stdin is forwarded to the remote command. Stdout and stderr are mirrored
/// locally. The exit code of the remote command becomes the exit code of
/// `hostcmd exec`. Pressing Ctrl-C sends a cancellation request to the server
/// and exits with code 130.
#[derive(Args)]
pub struct ExecArgs {
    /// Shared connection settings for the client connection.
    #[command(flatten)]
    pub connection: ConnectionArgs,

    /// Command and arguments to execute on the server.
    ///
    /// The first token is the executable name and subsequent tokens are
    /// passed as positional arguments. `--` is optional when the command is
    /// written after all `hostcmd exec` flags, but it can still be used to
    /// stop option parsing explicitly.
    #[arg(required = true, trailing_var_arg = true, num_args = 1.., env = COMMAND_ENV_VAR)]
    pub command: Vec<String>,
}

/// Arguments for stopping a daemon server.
///
/// Reads the process id from the pid file, sends a termination signal,
/// and removes the pid file once the process has stopped.
#[derive(Args)]
pub struct StopArgs {
    /// File that contains the daemon server process id.
    ///
    /// Use this when the daemon was started with a custom `--pid-file` path.
    /// Defaults to `~/.local/share/hostcmd/server.pid`.
    #[arg(
        long,
        env = PID_FILE_ENV_VAR,
        value_name = "file",
        default_value = DEFAULT_PID_FILE
    )]
    pub pid_file: PathBuf,
}

/// Tests for CLI argument parsing.
#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Commands};

    /// Accepts an `exec` command without `--` when the command follows all flags.
    #[test]
    fn exec_accepts_command_without_separator() {
        let cli = Cli::try_parse_from([
            "hostcmd",
            "exec",
            "--secret",
            "secret",
            "--port",
            "8080",
            "--host",
            "127.0.0.1",
            "uname",
        ])
        .expect("exec command without separator should parse");

        match cli.command {
            Commands::Exec(args) => assert_eq!(
                args.command,
                ["uname"],
                "exec command should preserve the remote command without a separator"
            ),
            Commands::Server(_) | Commands::Stop(_) => {
                panic!("expected exec command when parsing exec arguments without a separator")
            }
        }
    }

    /// Preserves remote command arguments without requiring `--`.
    #[test]
    fn exec_accepts_remote_command_arguments_without_separator() {
        let cli = Cli::try_parse_from([
            "hostcmd",
            "exec",
            "--secret",
            "secret",
            "--port",
            "8080",
            "--host",
            "127.0.0.1",
            "sh",
            "-c",
            "printf test",
        ])
        .expect("exec command arguments without separator should parse");

        match cli.command {
            Commands::Exec(args) => {
                assert_eq!(
                    args.command,
                    ["sh", "-c", "printf test"],
                    "exec command should preserve all remote command arguments without a separator"
                );
            }
            Commands::Server(_) | Commands::Stop(_) => {
                panic!(
                    "expected exec command when parsing remote command arguments without a separator"
                )
            }
        }
    }

    /// Continues accepting `--` as an explicit separator for remote commands.
    #[test]
    fn exec_still_accepts_explicit_separator() {
        let cli = Cli::try_parse_from([
            "hostcmd",
            "exec",
            "--secret",
            "secret",
            "--port",
            "8080",
            "--host",
            "127.0.0.1",
            "--",
            "uname",
            "-a",
        ])
        .expect("exec command with separator should parse");

        match cli.command {
            Commands::Exec(args) => assert_eq!(
                args.command,
                ["uname", "-a"],
                "exec command should preserve the remote command when using an explicit separator"
            ),
            Commands::Server(_) | Commands::Stop(_) => {
                panic!(
                    "expected exec command when parsing exec arguments with an explicit separator"
                )
            }
        }
    }
}
