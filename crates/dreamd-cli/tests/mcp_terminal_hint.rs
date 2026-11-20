//! DRM-151: `dreamd mcp` on a terminal says it is waiting for a harness.
//!
//! When stdin is a terminal and the transport is stdio, the server writes one
//! line to stderr, after its banner and mode line, and then serves. A piped
//! stdin (every harness) and `--bind` get no such line, and stdout stays
//! JSON-RPC only.
//!
//! The tests drive the real binary with no daemon. Every child gets its own
//! `HOME`, an empty `XDG_CONFIG_HOME` and a `DREAMD_SOCK` path that nothing
//! listens on, plus `DREAMD_LOG=off`, `GIT_CONFIG_NOSYSTEM=1` and
//! `GIT_CONFIG_GLOBAL=/dev/null`, with `DREAMD_LAUNCHER` (unless the test sets
//! it) and `DREAMD_LOG_FORMAT` removed. A terminal stdin is the slave side of
//! `nix::pty::openpty`; stdout and stderr are pipes, read by threads with
//! timeouts. Each child is killed at the end, and on drop.
//! Unix-only: a pty and `DREAMD_SOCK` are Unix.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::OwnedFd;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use dreamd_core::privacy::DR413_DISCLOSURE;

/// The hint with the binary's own `setup` command.
const BINARY_HINT: &str = "dreamd mcp: MCP server on stdio, waiting for a harness. To connect \
                           Claude Code or Cursor, run `dreamd setup`. Ctrl-C quits.";
/// The hint under the npx shim (`DREAMD_LAUNCHER=npx`).
const NPX_HINT: &str = "dreamd mcp: MCP server on stdio, waiting for a harness. To connect \
                        Claude Code or Cursor, run `npx -y dreamd-mcp setup`. Ctrl-C quits.";
/// A substring of the hint that no other line has.
const HINT_MARK: &str = "waiting for a harness";
/// The longest wait for a line that must come, on a loaded machine.
const LIMIT: Duration = Duration::from_secs(30);

fn dreamd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dreamd")
}

/// One test's `HOME`, empty `XDG_CONFIG_HOME`, socket folder, and a project
/// with a store (`dreamd init`), so `dreamd mcp` there runs in-process.
struct Env {
    home: tempfile::TempDir,
    xdg: tempfile::TempDir,
    sock_dir: tempfile::TempDir,
    project: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let env = Self {
            home: tempfile::tempdir().expect("home tempdir"),
            xdg: tempfile::tempdir().expect("XDG_CONFIG_HOME tempdir"),
            sock_dir: tempfile::tempdir().expect("socket tempdir"),
            project: tempfile::tempdir().expect("project tempdir"),
        };
        std::fs::create_dir(env.project.path().join(".git")).expect("project sentinel");
        let out = env
            .dreamd(env.project.path())
            .args(["init", "--quiet"])
            .stdin(Stdio::null())
            .output()
            .expect("run dreamd init");
        assert!(out.status.success(), "dreamd init: {out:?}");
        env
    }

    /// `dreamd` in `cwd` with the test's environment rules.
    fn dreamd(&self, cwd: &Path) -> Command {
        let mut cmd = Command::new(dreamd_bin());
        cmd.current_dir(cwd)
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.xdg.path())
            .env("DREAMD_SOCK", self.sock_dir.path().join("d.sock"))
            .env("DREAMD_LOG", "off")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("DREAMD_LAUNCHER")
            .env_remove("DREAMD_LOG_FORMAT");
        cmd
    }
}

/// A running `dreamd mcp` whose stdout and stderr are read by threads.
struct Running {
    child: Child,
    /// The pty's master side, held open while the child reads the slave.
    _master: Option<OwnedFd>,
    stdin: Option<ChildStdin>,
    stdout: Receiver<Vec<u8>>,
    stderr: Receiver<String>,
}

impl Running {
    /// Spawn `cmd` with stdin the slave side of a new pty: a terminal.
    fn on_terminal(mut cmd: Command) -> Self {
        let pty = nix::pty::openpty(None, None).expect("openpty");
        cmd.stdin(Stdio::from(pty.slave));
        Self::spawn(cmd, Some(pty.master))
    }

    /// Spawn `cmd` with stdin a pipe, as a harness runs it.
    fn on_pipe(mut cmd: Command) -> Self {
        cmd.stdin(Stdio::piped());
        Self::spawn(cmd, None)
    }

    fn spawn(mut cmd: Command, master: Option<OwnedFd>) -> Self {
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn dreamd mcp");
        // Drop the parent's copy of the slave now, so only the child holds it.
        drop(cmd);
        let mut stdout = child.stdout.take().expect("child stdout");
        let stderr = child.stderr.take().expect("child stderr");
        let (out_tx, out_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 || out_tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        let (err_tx, err_rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if err_tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            stdin: child.stdin.take(),
            child,
            _master: master,
            stdout: out_rx,
            stderr: err_rx,
        }
    }

    /// Stderr's lines until one satisfies `until` (at most [`LIMIT`]), then
    /// every line within `quiet` more. Never blocks without a limit.
    fn stderr_until(&self, until: impl Fn(&str) -> bool, quiet: Duration) -> Vec<String> {
        let mut lines = Vec::new();
        let deadline = Instant::now() + LIMIT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.stderr.recv_timeout(remaining) {
                Ok(line) => {
                    let done = until(&line);
                    lines.push(line);
                    if done {
                        break;
                    }
                }
                Err(_) => return lines,
            }
        }
        let quiet_end = Instant::now() + quiet;
        while let Some(left) = quiet_end.checked_duration_since(Instant::now()) {
            match self.stderr.recv_timeout(left) {
                Ok(line) => lines.push(line),
                Err(_) => break,
            }
        }
        lines
    }

    /// Every stdout byte received so far, without waiting.
    fn stdout_so_far(&self) -> Vec<u8> {
        self.stdout.try_iter().flatten().collect()
    }

    /// Stdout's bytes up to and including the first `\n`, within [`LIMIT`].
    fn stdout_first_line(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        let deadline = Instant::now() + LIMIT;
        while !bytes.contains(&b'\n') {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.stdout.recv_timeout(remaining) {
                Ok(chunk) => bytes.extend(chunk),
                Err(e) => panic!("no stdout line within {LIMIT:?}: {e}; got {bytes:?}"),
            }
        }
        bytes
    }

    /// Close stdin, then wait up to [`LIMIT`] for the child to exit, and
    /// return every stderr line, read to EOF.
    fn finish(mut self) -> Vec<String> {
        drop(self.stdin.take());
        let deadline = Instant::now() + LIMIT;
        while self.child.try_wait().expect("try_wait").is_none() {
            assert!(
                Instant::now() < deadline,
                "dreamd mcp did not exit on stdin EOF"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut lines = Vec::new();
        while let Ok(line) = self.stderr.recv_timeout(LIMIT) {
            lines.push(line);
        }
        lines
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Run `dreamd mcp` in `cwd` on a terminal, and return stderr's lines up to
/// the hint `hint` and for `quiet` after it. Stdout must stay empty.
fn terminal_run(cmd: Command, hint: &str, what: &str) -> Vec<String> {
    let running = Running::on_terminal(cmd);
    let lines = running.stderr_until(|line| line == hint, Duration::from_secs(3));
    let stdout = running.stdout_so_far();
    drop(running);
    assert_eq!(
        lines.iter().filter(|l| l.as_str() == hint).count(),
        1,
        "{what}: the hint exactly once on stderr: {lines:#?}"
    );
    assert_eq!(
        lines.last().map(String::as_str),
        Some(hint),
        "{what}: the hint is the last line before the server waits: {lines:#?}"
    );
    assert!(
        stdout.is_empty(),
        "{what}: stdout is JSON-RPC only: {:?}",
        String::from_utf8_lossy(&stdout)
    );
    lines
}

/// DRM-151 test 2: a terminal stdin gets one hint line on stderr, after the
/// banner and the mode line, naming the launcher's `setup`.
#[test]
fn a_terminal_stdin_gets_one_hint_line() {
    let env = Env::new();
    let project = env.project.path();

    // In-process: the hint is the only line.
    let mut cmd = env.dreamd(project);
    cmd.arg("mcp");
    terminal_run(cmd, BINARY_HINT, "in-process");

    // Under the shim the hint names the npx form.
    let mut cmd = env.dreamd(project);
    cmd.arg("mcp").env("DREAMD_LAUNCHER", "npx");
    terminal_run(cmd, NPX_HINT, "DREAMD_LAUNCHER=npx");

    // No store: the banner, then the no-store line, then the hint.
    let bare = tempfile::tempdir().expect("bare tempdir");
    let mut cmd = env.dreamd(bare.path());
    cmd.arg("mcp");
    let lines = terminal_run(cmd, BINARY_HINT, "no store");
    let banner_first = DR413_DISCLOSURE.lines().next().expect("banner line");
    let at = |what: &str, pred: &dyn Fn(&str) -> bool| {
        lines
            .iter()
            .position(|l| pred(l))
            .unwrap_or_else(|| panic!("no {what} line: {lines:#?}"))
    };
    let banner = at("banner", &|l| l == banner_first);
    let no_store = at("no-store", &|l| {
        l.starts_with("dreamd mcp: no .agent/ store in ")
    });
    let hint = at("hint", &|l| l == BINARY_HINT);
    assert!(
        banner < no_store && no_store < hint,
        "banner, no-store line, hint, in that order: {lines:#?}"
    );
}

/// DRM-151 test 3: a piped stdin, as every harness gives, gets no hint, and
/// stdout's first bytes are the JSON-RPC response.
#[test]
fn a_piped_stdin_gets_no_hint() {
    let env = Env::new();
    let mut cmd = env.dreamd(env.project.path());
    cmd.arg("mcp");
    let mut running = Running::on_pipe(cmd);

    let stdin = running.stdin.as_mut().expect("piped stdin");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"terminal-hint","version":"0"}}}}}}"#
    )
    .expect("write initialize");
    stdin.flush().expect("flush initialize");

    let first = running.stdout_first_line();
    let response: serde_json::Value = serde_json::from_slice(&first).unwrap_or_else(|e| {
        panic!(
            "stdout's first bytes are JSON-RPC: {e}: {:?}",
            String::from_utf8_lossy(&first)
        )
    });
    assert_eq!(response["id"], 1, "{response}");
    assert!(response.get("result").is_some(), "{response}");

    let stderr = running.finish();
    assert!(
        !stderr.iter().any(|l| l.contains(HINT_MARK)),
        "a piped stdin gets no hint: {stderr:#?}"
    );
}

/// DRM-151 test 4: `--bind` serves HTTP, so a terminal stdin gets no hint.
#[cfg(feature = "mcp-http")]
#[test]
fn bind_gets_no_hint() {
    let env = Env::new();
    let mut cmd = env.dreamd(env.project.path());
    cmd.args(["mcp", "--bind", "127.0.0.1:0"]);
    let running = Running::on_terminal(cmd);
    let lines = running.stderr_until(
        |line| line.starts_with("dreamd mcp: Streamable HTTP at http://"),
        Duration::from_secs(2),
    );
    drop(running);
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("dreamd mcp: Streamable HTTP at http://")),
        "the listen line: {lines:#?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains(HINT_MARK)),
        "--bind gets no hint: {lines:#?}"
    );
}
