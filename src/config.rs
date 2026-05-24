use std::{
    env,
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};

/// Environment variable exposed to executed commands with the client hostname.
pub const CLIENT_HOSTNAME_ENV_VAR: &str = "HOSTCMD_CLIENT_HOSTNAME";

/// Environment variable exposed to executed commands with the client username.
pub const CLIENT_USERNAME_ENV_VAR: &str = "HOSTCMD_CLIENT_USERNAME";

/// Environment variable exposed to executed commands with the client working directory.
pub const CLIENT_CWD_ENV_VAR: &str = "HOSTCMD_CLIENT_CWD";

/// Environment variable marking that a command was launched through hostcmd.
pub const CLIENT_EXEC_ENV_VAR: &str = "HOSTCMD_CLIENT_EXEC";

/// Environment variable value used to mark hostcmd-launched commands.
pub const CLIENT_EXEC_ENV_VALUE: &str = "true";

/// Internal environment variable carrying the daemon startup readiness file path.
pub const DAEMON_READY_FILE_ENV_VAR: &str = "HOSTCMD_DAEMON_READY_FILE";

/// Test-only environment variable carrying a foreground server readiness file path.
pub const TEST_READY_FILE_ENV_VAR: &str = "HOSTCMD_TEST_READY_FILE";

/// Environment variable used to configure the server and client shared secret.
pub const SECRET_ENV_VAR: &str = "HOSTCMD_SECRET";

/// Environment variable used to configure the server and client port.
pub const PORT_ENV_VAR: &str = "HOSTCMD_PORT";

/// Environment variable used to configure the server and client host.
pub const HOST_ENV_VAR: &str = "HOSTCMD_HOST";

/// Environment variable used to configure the server log file.
pub const LOG_FILE_ENV_VAR: &str = "HOSTCMD_LOG_FILE";

/// Environment variable used to configure the server pid file.
pub const PID_FILE_ENV_VAR: &str = "HOSTCMD_PID_FILE";

/// Environment variable used to configure SSH forwarding.
pub const SSH_FORWARD_ENV_VAR: &str = "HOSTCMD_SSH_FORWARD";

/// Environment variable used to configure the SSH port for reverse forwarding.
pub const SSH_FORWARD_PORT_ENV_VAR: &str = "HOSTCMD_SSH_FORWARD_PORT";

/// Environment variable used to configure allowed server commands.
pub const ALLOW_ENV_VAR: &str = "HOSTCMD_ALLOW";

/// Environment variable used to configure the client command.
pub const COMMAND_ENV_VAR: &str = "HOSTCMD_COMMAND";

/// Default server pid file path shown in CLI help and expanded at runtime.
pub const DEFAULT_PID_FILE: &str = "~/.local/share/hostcmd/server.pid";

/// Default server log file path shown in documentation and expanded at runtime.
pub const DEFAULT_LOG_FILE: &str = "~/.local/share/hostcmd/server.log";

/// Expands a leading `~/` path component against the caller's home directory.
pub fn expand_home_path(path: &Path) -> Result<PathBuf> {
    let Some(path_text) = path.to_str() else {
        return Ok(path.to_path_buf());
    };

    if path_text == "~" {
        let home = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
        return Ok(PathBuf::from(home));
    }

    if let Some(rest) = path_text.strip_prefix("~/") {
        let home = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
        return Ok(PathBuf::from(home).join(rest));
    }

    Ok(path.to_path_buf())
}
