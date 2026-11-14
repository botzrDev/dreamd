//! BZR-1534: daemon-mode `dreamd mcp` closes its reachability probe before it
//! serves, so an idle session holds no connection to the daemon. Each tool
//! call still opens its own connection.
//!
//! A fake daemon on a std thread reports, for each connection in accept order,
//! what it saw: the peer closed before sending a byte, the peer sent a request
//! (answered with one canned `500`), or nothing arrived and the connection
//! stayed open for [`PROBE_WAIT`]. Every child gets `HOME` set to a tempdir and
//! `DREAMD_LOG=off`, and has `DREAMD_LOG_FORMAT` and `DREAMD_LAUNCHER` removed.
//! `dreamd init` runs with `DREAMD_SOCK` removed, so it never meets the fake;
//! `dreamd mcp` gets `DREAMD_SOCK` set to the fake's socket. The test process's
//! own environment is never changed. The MCP helpers are copied from
//! `version_mismatch_launcher.rs` (test files don't share helpers). Unix-only:
//! the probe is a Unix-socket connect.

#![cfg(unix)]

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const TIMEOUT: Duration = Duration::from_secs(30);
/// How long the fake waits for a byte or a close before it calls a connection
/// still open.
const PROBE_WAIT: Duration = Duration::from_secs(10);
const FAKE_ERROR: &str = "bzr1534 fake daemon";

fn dreamd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dreamd")
}

/// A `dreamd` command in `project`, with the test's environment rules.
fn dreamd(project: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(dreamd_bin());
    cmd.current_dir(project)
        .env("HOME", home)
        .env("DREAMD_LOG", "off")
        .env_remove("DREAMD_LOG_FORMAT")
        .env_remove("DREAMD_LAUNCHER")
        .env_remove("DREAMD_SOCK");
    cmd
}

fn describe(out: &Output) -> String {
    format!(
        "exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A project with a stub `Cargo.toml` and an initialised store, in its own
/// tempdir, and a home tempdir. `dreamd init` refuses a bare tempdir.
fn project_and_home() -> (tempfile::TempDir, tempfile::TempDir) {
    let project = tempfile::tempdir().expect("project tempdir");
    let home = tempfile::tempdir().expect("home tempdir");
    std::fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname = \"bzr1534\"\nversion = \"0.0.0\"\n",
    )
    .expect("write stub Cargo.toml");
    let init = dreamd(project.path(), home.path())
        .args(["init", "--quiet"])
        .output()
        .expect("run dreamd init");
    assert!(init.status.success(), "dreamd init: {}", describe(&init));
    (project, home)
}

/// What the fake daemon saw on one connection.
#[derive(Debug, PartialEq)]
enum Seen {
    /// The peer closed before sending a byte.
    ClosedEmpty,
    /// The peer sent a request head; this is its first line.
    Request(String),
    /// No byte and no close within [`PROBE_WAIT`].
    StillOpen,
}

/// A fake daemon that reports each connection on `seen`, in accept order, and
/// answers each request with a `500` whose `error` is [`FAKE_ERROR`]. The
/// socket sits in its own short tempdir, so the path fits `SUN_LEN`.
struct FakeDaemon {
    _dir: tempfile::TempDir,
    sock: PathBuf,
    seen: Receiver<(usize, Seen)>,
}

impl FakeDaemon {
    fn start() -> Self {
        let dir = tempfile::tempdir().expect("fake daemon tempdir");
        let sock = dir.path().join("d.sock");
        let listener = UnixListener::bind(&sock).expect("bind fake daemon");
        let (tx, seen) = mpsc::channel();
        std::thread::spawn(move || {
            for (n, stream) in listener.incoming().enumerate() {
                let Ok(stream) = stream else { continue };
                let tx = tx.clone();
                // One thread per connection, so a connection that stays open
                // cannot hold up the accept loop.
                std::thread::spawn(move || {
                    let _ = tx.send((n, watch_connection(stream)));
                });
            }
        });
        Self {
            _dir: dir,
            sock,
            seen,
        }
    }

    /// The report for the next connection, in accept order. Panics after
    /// `PROBE_WAIT + TIMEOUT` with no report.
    fn next(&self) -> (usize, Seen) {
        self.seen
            .recv_timeout(PROBE_WAIT + TIMEOUT)
            .expect("the fake daemon reports a connection")
    }
}

/// Wait up to [`PROBE_WAIT`] for the first byte or a close, then read a request
/// head and answer it.
fn watch_connection(mut stream: UnixStream) -> Seen {
    let _ = stream.set_read_timeout(Some(PROBE_WAIT));
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        match stream.read(&mut buf) {
            Ok(0) if head.is_empty() => return Seen::ClosedEmpty,
            Ok(0) => break,
            Ok(n) => {
                head.extend_from_slice(&buf[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) if head.is_empty() => return Seen::StillOpen,
            Err(_) => break,
        }
    }
    // The version header has to match, or the client answers with the
    // version-unknown sentence instead of the daemon's error.
    let body = serde_json::json!({ "error": FAKE_ERROR }).to_string();
    let response = format!(
        "HTTP/1.1 500 Internal Server Error\r\n{}: {VERSION}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        dreamd_core::daemon_client::VERSION_HEADER,
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
    let head = String::from_utf8_lossy(&head);
    Seen::Request(head.lines().next().unwrap_or_default().to_string())
}

/// Wait up to `secs` for `child` to exit; kill + panic on timeout.
/// Copied from `version_mismatch_launcher.rs`.
fn wait_or_kill(child: &mut Child, secs: u64) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("MCP child did not exit within {secs}s of stdin EOF (orphan risk)");
        }
        std::thread::sleep(Duration::from_millis(50)); // poll process exit
    }
}

/// Stdio MCP child: line-oriented JSON-RPC over piped stdin/stdout.
/// Reader thread + mpsc avoids blocking forever on a pipe read.
/// Copied from `version_mismatch_launcher.rs`.
struct McpChild {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<String>,
}

impl McpChild {
    fn from_command(mut cmd: Command) -> Self {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn MCP child");
        let stdout = child.stdout.take().expect("child stdout");
        let stdin = child.stdin.take().expect("child stdin");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) if !l.is_empty() => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
        Self {
            child,
            stdin: Some(stdin),
            rx,
        }
    }

    fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin still open");
        writeln!(stdin, "{line}").expect("write MCP line");
        stdin.flush().expect("flush MCP stdin");
    }

    /// Receive the next JSON-RPC response with matching `id`, skipping notifications.
    fn recv_response(&self, id: u64, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                panic!("timeout waiting for MCP response id={id}");
            }
            let line = self
                .rx
                .recv_timeout(remaining)
                .unwrap_or_else(|e| panic!("recv_timeout waiting for id={id}: {e}"));
            let v: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("invalid JSON from MCP child: {e}; line={line}"));
            if v.get("id").and_then(|i| i.as_u64()) == Some(id) {
                return v;
            }
        }
    }

    /// Close stdin and wait for the server to exit.
    fn close(mut self) {
        drop(self.stdin.take());
        let status = wait_or_kill(&mut self.child, 30);
        assert!(status.success(), "dreamd mcp exit: {status:?}");
    }
}

/// Copied from `version_mismatch_launcher.rs`.
fn handshake(mcp: &mut McpChild, init_id: u64, timeout: Duration) {
    let init = format!(
        r#"{{"jsonrpc":"2.0","id":{init_id},"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"mcp-probe-closes","version":"0"}}}}}}"#
    );
    let initialized = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    mcp.send(&init);
    let resp = mcp.recv_response(init_id, timeout);
    assert!(
        resp.get("result").is_some(),
        "initialize must succeed; got {resp}"
    );
    assert!(
        resp.get("error").is_none(),
        "initialize must not error; got {resp}"
    );
    mcp.send(initialized);
}

/// Test 1: the probe closes while the server is still running, before any MCP
/// message; a tool call then opens its own connection and gets the daemon's
/// answer.
#[test]
fn the_probe_closes_before_the_server_serves() {
    let (project, home) = project_and_home();
    let fake = FakeDaemon::start();
    let mut cmd = dreamd(project.path(), home.path());
    cmd.env("DREAMD_SOCK", &fake.sock).arg("mcp");
    let mut mcp = McpChild::from_command(cmd);

    // No MCP message yet, and stdin stays open.
    assert_eq!(
        fake.next(),
        (0, Seen::ClosedEmpty),
        "the first connection is the probe, and it must close before the server serves"
    );
    assert!(
        mcp.child.try_wait().expect("try_wait").is_none(),
        "dreamd mcp is still serving when the probe closes"
    );

    handshake(&mut mcp, 1, TIMEOUT);
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": "search_nodes", "arguments": { "query": "bzr1534", "k": 5 } },
    });
    mcp.send(&req.to_string());
    let resp = mcp.recv_response(2, TIMEOUT);
    assert!(
        resp.to_string().contains(FAKE_ERROR),
        "search_nodes returns the daemon's answer: {resp}"
    );
    let (n, seen) = fake.next();
    assert_eq!(n, 1, "the tool call opens the second connection");
    match seen {
        Seen::Request(line) => assert!(
            line.starts_with("GET /api/v1/recall"),
            "the second connection carries the recall request: {line}"
        ),
        other => panic!("the tool call's connection must carry a request: {other:?}"),
    }
    mcp.close();
}
