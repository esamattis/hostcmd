"""Smoke tests for the hostcmd command-line interface."""

import os
import pwd
import random
import selectors
import signal
import socket
import subprocess
import tempfile
import unittest
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[1]
HOSTCMD = PROJECT_ROOT / "target" / "debug" / "hostcmd"
HOST = os.environ.get("HOSTCMD_SMOKE_HOST", "127.0.0.1")
SECRET = os.environ.get("HOSTCMD_SMOKE_SECRET", "test-secret")
HOSTCMD_ENV_VARS = (
    "HOSTCMD_ALLOW",
    "HOSTCMD_COMMAND",
    "HOSTCMD_DAEMON_READY_FILE",
    "HOSTCMD_HOST",
    "HOSTCMD_LOG_FILE",
    "HOSTCMD_PID_FILE",
    "HOSTCMD_PORT",
    "HOSTCMD_SECRET",
    "HOSTCMD_SSH_FORWARD",
    "HOSTCMD_SSH_FORWARD_PORT",
    "HOSTCMD_TEST_READY_FILE",
)
XDG_ENV_VARS = (
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
)


class HostcmdSmokeTest(unittest.TestCase):
    """End-to-end tests ported from scripts/smoke_test.sh."""

    @classmethod
    def setUpClass(cls):
        """Build the Rust binary once before running the smoke tests."""
        subprocess.run(["cargo", "test"], cwd=PROJECT_ROOT, check=True)
        subprocess.run(["cargo", "build"], cwd=PROJECT_ROOT, check=True)

    def setUp(self):
        """Create isolated temporary files and default server settings."""
        self.temp_dir = tempfile.TemporaryDirectory()
        self.home_dir = self.temp_path("home")
        self.home_dir.mkdir()
        self.stdout_file = self.temp_path("stdout")
        self.stderr_file = self.temp_path("stderr")
        self.server_log = self.temp_path("server.log")
        self.server_pid_file = self.temp_path("server.pid")
        self.daemon_log = self.temp_path("daemon.log")
        self.daemon_pid_file = self.temp_path("daemon.pid")
        self.server_ready_file = self.temp_path("server.ready")
        os.mkfifo(self.server_ready_file)
        self.server_process = None
        self.daemon_pid = None
        self.port = int(os.environ.get("HOSTCMD_SMOKE_PORT", self.free_port()))

    def tearDown(self):
        """Stop any server processes started by a test."""
        if self.daemon_pid is not None:
            self.stop_pid(self.daemon_pid)

        if self.server_process is not None and self.server_process.poll() is None:
            self.server_process.terminate()
            try:
                self.server_process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.server_process.kill()
                self.server_process.wait(timeout=5)

        self.temp_dir.cleanup()

    def temp_path(self, name):
        """Return a path inside the test's temporary directory."""
        return Path(self.temp_dir.name) / name

    def hostcmd_env(self):
        """Return an environment with hostcmd configuration variables removed."""
        env = os.environ.copy()
        for name in HOSTCMD_ENV_VARS:
            env.pop(name, None)
        for name in XDG_ENV_VARS:
            env.pop(name, None)
        env["HOME"] = str(self.home_dir)
        return env

    def configured_env(self, **settings):
        """Return a clean environment with explicit smoke-test settings applied."""
        env = self.hostcmd_env()
        env.update({name: str(value) for name, value in settings.items()})
        return env

    def hostcmd(self, *args, env=None, **kwargs):
        """Run hostcmd with the smoke-test environment and optional overrides."""
        command_env = self.hostcmd_env()
        if env is not None:
            command_env.update(env)

        return subprocess.run(
            [str(HOSTCMD), *args],
            cwd=PROJECT_ROOT,
            env=command_env,
            **kwargs,
        )

    def exec_command(self, *args, **kwargs):
        """Run hostcmd exec against the active smoke-test server."""
        return self.hostcmd(
            "exec",
            "--secret",
            SECRET,
            "--port",
            str(self.port),
            "--host",
            HOST,
            *args,
            **kwargs,
        )

    def start_server(self):
        """Start a foreground server with the commands needed by smoke tests."""
        log = self.server_log.open("wb")
        env = self.hostcmd_env()
        env["HOSTCMD_TEST_READY_FILE"] = str(self.server_ready_file)
        self.server_process = subprocess.Popen(
            [
                str(HOSTCMD),
                "server",
                "--secret",
                SECRET,
                "--port",
                str(self.port),
                "--host",
                HOST,
                "--pid-file",
                str(self.server_pid_file),
                "--allow",
                "true",
                "--allow",
                "sh",
                "--allow",
                "sha1sum",
                "--allow",
                "printenv",
            ],
            cwd=PROJECT_ROOT,
            env=env,
            stdout=log,
            stderr=subprocess.STDOUT,
        )
        log.close()

    def wait_for_server(self):
        """Wait until the foreground server reports readiness."""
        read_fd = os.open(self.server_ready_file, os.O_RDONLY | os.O_NONBLOCK)
        write_fd = os.open(self.server_ready_file, os.O_WRONLY | os.O_NONBLOCK)
        try:
            if self.server_process.poll() is not None:
                self.fail(f"server exited early; log:\n{self.server_log.read_text()}")

            with selectors.DefaultSelector() as selector:
                selector.register(read_fd, selectors.EVENT_READ)
                events = selector.select(timeout=5)

            if not events:
                self.fail(
                    f"server did not become ready on {HOST}:{self.port}; log:\n{self.server_log.read_text()}"
                )

            ready_message = os.read(read_fd, 16)
        finally:
            os.close(write_fd)
            os.close(read_fd)

        self.assertEqual(
            ready_message,
            b"OK\n",
            "foreground server wrote an unexpected readiness marker",
        )

    def assert_output(
        self, result, expected_code, expected_stdout="", expected_stderr=""
    ):
        """Assert a completed process returned the expected code and output."""
        stdout = (
            result.stdout.decode()
            if isinstance(result.stdout, bytes)
            else result.stdout
        )
        stderr = (
            result.stderr.decode()
            if isinstance(result.stderr, bytes)
            else result.stderr
        )
        self.assertEqual(
            result.returncode, expected_code, "process returned an unexpected exit code"
        )
        self.assertEqual(stdout, expected_stdout, "process wrote unexpected stdout")
        self.assertEqual(stderr, expected_stderr, "process wrote unexpected stderr")

    def assert_pid_running(self, pid, message):
        """Assert that a process id is currently running."""
        try:
            os.kill(pid, 0)
        except OSError as exc:
            self.fail(f"{message}: {exc}")

    def assert_pid_stopped(self, pid, message):
        """Assert that a process id is no longer running."""
        try:
            os.kill(pid, 0)
        except OSError:
            return
        self.fail(message)

    def stop_pid(self, pid):
        """Terminate a process id if it is still running."""
        try:
            os.kill(pid, signal.SIGTERM)
        except OSError:
            return

    def free_port(self):
        """Return an available local TCP port in the smoke-test range."""
        for _ in range(50):
            port = random.randint(30000, 49999)
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
                try:
                    sock.bind((HOST, port))
                except OSError:
                    continue
                return port

        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
            sock.bind((HOST, 0))
            return sock.getsockname()[1]

    def test_exec_and_daemon_smoke_flow(self):
        """Exercise remote exec behavior and daemon lifecycle behavior."""
        self.start_server()
        self.wait_for_server()

        result = self.exec_command("true", capture_output=True)
        self.assert_output(result, 0)

        result = self.exec_command(
            "--",
            "sh",
            "-c",
            "printf out; printf err >&2; exit 7",
            capture_output=True,
        )
        self.assert_output(result, 7, "out", "err")

        result = self.exec_command("--", "sha1sum", input=b"abc", capture_output=True)
        self.assert_output(result, 0, "a9993e364706816aba3e25717850c26c9cd0d89d  -\n")

        result = self.exec_command(
            "--", "printenv", "HOSTCMD_CLIENT_HOSTNAME", capture_output=True
        )
        self.assert_output(result, 0, f"{socket.gethostname()}\n")

        result = self.exec_command(
            "--", "printenv", "HOSTCMD_CLIENT_USERNAME", capture_output=True
        )
        self.assert_output(result, 0, f"{pwd.getpwuid(os.geteuid()).pw_name}\n")

        result = self.exec_command(
            "--", "printenv", "HOSTCMD_CLIENT_CWD", capture_output=True
        )
        self.assert_output(result, 0, f"{PROJECT_ROOT}\n")

        result = self.exec_command(
            "--", "printenv", "HOSTCMD_CLIENT_EXEC", capture_output=True
        )
        self.assert_output(result, 0, "true\n")

        result = self.exec_command("false", capture_output=True, timeout=5)
        stderr = result.stderr.decode()
        self.assertEqual(
            result.returncode, 1, "disallowed command returned an unexpected exit code"
        )
        self.assertEqual(
            result.stdout, b"", "disallowed command wrote unexpected stdout"
        )
        self.assertIn(
            "command is not allowed",
            stderr,
            "disallowed command stderr did not explain the failure",
        )

        daemon_port = self.free_port()
        result = self.hostcmd(
            "server",
            "--secret",
            SECRET,
            "--port",
            str(daemon_port),
            "--host",
            HOST,
            "--daemon",
            "--pid-file",
            str(self.daemon_pid_file),
            "--log-file",
            str(self.daemon_log),
            capture_output=True,
        )
        self.assert_output(result, 0)
        self.assertTrue(
            self.daemon_pid_file.stat().st_size > 0, "daemon did not write pid file"
        )
        self.daemon_pid = int(self.daemon_pid_file.read_text().strip())
        self.assert_pid_running(self.daemon_pid, "daemon pid is not running")
        self.assertTrue(
            self.daemon_log.stat().st_size > 0, "daemon did not write log file"
        )

        result = self.hostcmd(
            "exec",
            "--secret",
            SECRET,
            "--port",
            str(daemon_port),
            "--host",
            HOST,
            "true",
            capture_output=True,
        )
        self.assert_output(result, 0)

        result = self.hostcmd(
            "server",
            "--secret",
            SECRET,
            "--port",
            str(daemon_port),
            "--host",
            HOST,
            "--daemon",
            "--pid-file",
            str(self.daemon_pid_file),
            "--log-file",
            str(self.daemon_log),
            capture_output=True,
        )
        stderr = result.stderr.decode()
        self.assertNotEqual(
            result.returncode,
            0,
            "daemon startup unexpectedly succeeded on an occupied port",
        )
        self.assertIn(
            "failed to bind",
            stderr,
            "occupied-port startup stderr did not explain the bind failure",
        )

        result = self.hostcmd(
            "stop", "--pid-file", str(self.daemon_pid_file), capture_output=True
        )
        self.assert_output(result, 0)
        self.assert_pid_stopped(self.daemon_pid, "daemon is still running after stop")
        self.daemon_pid = None

    def test_flag_free_usage_uses_environment_defaults(self):
        """Exercise flag-free daemon, exec, and stop flows configured by environment variables."""
        default_pid_file = self.home_dir / ".local" / "share" / "hostcmd" / "server.pid"
        default_log_file = self.home_dir / ".local" / "share" / "hostcmd" / "server.log"
        daemon_port = self.free_port()
        env = self.configured_env(
            HOSTCMD_SECRET=SECRET,
            HOSTCMD_PORT=daemon_port,
            HOSTCMD_HOST=HOST,
            HOSTCMD_ALLOW="true,printenv",
        )

        result = self.hostcmd("server", "--daemon", capture_output=True, env=env)
        self.assert_output(result, 0)
        self.assertTrue(
            default_pid_file.exists(),
            "flag-free daemon did not create the default pid file",
        )
        self.assertTrue(
            default_pid_file.stat().st_size > 0,
            "flag-free daemon wrote an empty default pid file",
        )
        self.daemon_pid = int(default_pid_file.read_text().strip())
        self.assert_pid_running(self.daemon_pid, "flag-free daemon pid is not running")
        self.assertTrue(
            default_log_file.exists(),
            "flag-free daemon did not create the default log file",
        )
        self.assertTrue(
            default_log_file.stat().st_size > 0,
            "flag-free daemon wrote an empty default log file",
        )

        result = self.hostcmd("exec", "true", capture_output=True, env=env)
        self.assert_output(result, 0)

        result = self.hostcmd(
            "exec", "printenv", "HOSTCMD_CLIENT_EXEC", capture_output=True, env=env
        )
        self.assert_output(result, 0, "true\n")

        result = self.hostcmd("exec", "false", capture_output=True, env=env, timeout=5)
        stderr = result.stderr.decode()
        self.assertEqual(
            result.returncode,
            1,
            "env-configured allow list returned an unexpected exit code",
        )
        self.assertEqual(
            result.stdout, b"", "env-configured allow list wrote unexpected stdout"
        )
        self.assertIn(
            "command is not allowed",
            stderr,
            "env-configured allow list stderr did not explain the failure",
        )

        result = self.hostcmd("stop", capture_output=True, env=env)
        self.assert_output(result, 0)
        self.assert_pid_stopped(
            self.daemon_pid, "flag-free daemon is still running after stop"
        )
        self.daemon_pid = None

    def test_command_and_pid_file_environment_variables_work_without_flags(self):
        """Exercise HOSTCMD_COMMAND and HOSTCMD_PID_FILE when commands omit their corresponding flags."""
        daemon_port = self.free_port()
        daemon_env = self.configured_env(
            HOSTCMD_PID_FILE=self.daemon_pid_file,
            HOSTCMD_LOG_FILE=self.daemon_log,
        )

        result = self.hostcmd(
            "server",
            "--secret",
            SECRET,
            "--port",
            str(daemon_port),
            "--host",
            HOST,
            "--daemon",
            capture_output=True,
            env=daemon_env,
        )
        self.assert_output(result, 0)
        self.assertTrue(
            self.daemon_pid_file.stat().st_size > 0,
            "env pid file did not receive the daemon pid",
        )
        self.daemon_pid = int(self.daemon_pid_file.read_text().strip())
        self.assert_pid_running(
            self.daemon_pid, "daemon started with env pid file is not running"
        )
        self.assertTrue(
            self.daemon_log.stat().st_size > 0,
            "env log file did not receive daemon logs",
        )

        exec_env = self.configured_env(
            HOSTCMD_SECRET=SECRET,
            HOSTCMD_PORT=daemon_port,
            HOSTCMD_HOST=HOST,
            HOSTCMD_COMMAND="true",
        )
        result = self.hostcmd("exec", capture_output=True, env=exec_env)
        self.assert_output(result, 0)

        result = self.hostcmd("stop", capture_output=True, env=daemon_env)
        self.assert_output(result, 0)
        self.assert_pid_stopped(
            self.daemon_pid, "daemon stopped through HOSTCMD_PID_FILE is still running"
        )
        self.daemon_pid = None


if __name__ == "__main__":
    unittest.main()
