//! BZR-1531: under the npm shim (`DREAMD_LAUNCHER=npx`), a version mismatch
//! names `npx -y dreamd-mcp update` and `npx -y dreamd-mcp watch`, on both
//! client paths: `dreamd dream`'s daemon proxy and the MCP tools. A `dreamd`
//! binary still names `dreamd watch`.
//!
//! A fake daemon on a std thread answers every request with one canned
//! response. Every child gets `HOME` set to a tempdir and `DREAMD_LOG=off`,
//! and has `DREAMD_LOG_FORMAT` and `DREAMD_LAUNCHER` removed. `dreamd init`
//! runs with `DREAMD_SOCK` removed, so it never meets the fake; the commands
//! under test get `DREAMD_SOCK` set to the fake's socket. The test process's
//! own environment is never changed. The MCP helpers are copied from
//! `search_nodes_id.rs` (test files don't share helpers). Unix-only: the
//! client paths under test speak HTTP over a Unix socket.

#![cfg(unix)]

use dreamd_core::daemon_client::{version_mismatch_message, version_unknown_message};
use dreamd_core::launcher::Launcher;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const STATE: &str = ".agent/.dreamd/state.json";
const UPDATE_NPX: &str = "`npx -y dreamd-mcp update`";
const TIMEOUT: Duration = Duration::from_secs(30);

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
        "[package]\nname = \"bzr1531\"\nversion = \"0.0.0\"\n",
    )
    .expect("write stub Cargo.toml");
    let init = dreamd(project.path(), home.path())
        .args(["init", "--quiet"])
        .output()
        .expect("run dreamd init");
    assert!(init.status.success(), "dreamd init: {}", describe(&init));
    (project, home)
}

/// A fake daemon that answers every request with `response`. The socket sits
/// in its own short tempdir, so the path fits `SUN_LEN`.
struct FakeDaemon {
    _dir: tempfile::TempDir,
    sock: PathBuf,
}

impl FakeDaemon {
    fn start(response: Vec<u8>) -> Self {
        let dir = tempfile::tempdir().expect("fake daemon tempdir");
        let sock = dir.path().join("d.sock");
        let listener = UnixListener::bind(&sock).expect("bind fake daemon");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let response = response.clone();
                // One thread per connection: `dreamd mcp`'s reachability probe
                // sends nothing and stays open while the server runs (the
                // stream is the scrutinee of a `match` whose arm serves), so
                // it must not hold up the accept loop. A connection that closes
                // before a full request head gets no answer.
                std::thread::spawn(move || {
                    if read_request_head(&mut stream) {
                        let _ = stream.write_all(&response);
                        let _ = stream.flush();
                    }
                });
            }
        });
        Self { _dir: dir, sock }
    }
}

/// Read to the end of the request head (`\r\n\r\n`). `false` when the peer
/// closes first.
fn read_request_head(stream: &mut UnixStream) -> bool {
    let _ = stream.set_read_timeout(Some(TIMEOUT));
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return false,
            Ok(n) => {
                head.extend_from_slice(&buf[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                    return true;
                }
            }
        }
    }
}

/// An HTTP/1.1 response with `status`, an optional [`VERSION_HEADER`], and a
/// JSON `body`.
///
/// [`VERSION_HEADER`]: dreamd_core::daemon_client::VERSION_HEADER
fn response(status: &str, version: Option<&str>, body: &str) -> Vec<u8> {
    let mut head = format!("HTTP/1.1 {status}\r\n");
    if let Some(v) = version {
        head.push_str(&format!(
            "{}: {v}\r\n",
            dreamd_core::daemon_client::VERSION_HEADER
        ));
    }
    head.push_str(&format!(
        "content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    ));
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(body.as_bytes());
    bytes
}

/// A daemon from before the handshake: 200, no version header.
fn no_version_daemon() -> FakeDaemon {
    FakeDaemon::start(response("200 OK", None, r#"{"ok":true}"#))
}

/// `dreamd dream --no-llm` against `fake`, with `launcher` as
/// `DREAMD_LAUNCHER` (`None` leaves it unset).
fn dream(project: &Path, home: &Path, fake: &FakeDaemon, launcher: Option<&str>) -> Output {
    let mut cmd = dreamd(project, home);
    cmd.env("DREAMD_SOCK", &fake.sock)
        .args(["dream", "--no-llm"]);
    if let Some(l) = launcher {
        cmd.env("DREAMD_LAUNCHER", l);
    }
    cmd.output().expect("run dreamd dream")
}

/// Test 9.
#[test]
fn dream_under_npx_names_npx_commands_for_a_daemon_with_no_version() {
    let (project, home) = project_and_home();
    let (project, home) = (project.path(), home.path());
    let fake = no_version_daemon();
    let before = std::fs::read(project.join(STATE)).expect("read state.json");

    let out = dream(project, home, &fake, Some("npx"));
    assert!(!out.status.success(), "{}", describe(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&version_unknown_message(VERSION, Launcher::Npx)),
        "{}",
        describe(&out)
    );
    assert!(stderr.contains(UPDATE_NPX), "{}", describe(&out));
    assert_eq!(
        std::fs::read(project.join(STATE)).expect("read state.json"),
        before,
        "a mismatch must not run the cycle in-process"
    );
}

/// Test 10.
#[test]
fn dream_under_npx_shows_its_own_sentence_for_a_426() {
    let (project, home) = project_and_home();
    let (project, home) = (project.path(), home.path());
    let body = serde_json::json!({
        "error": version_mismatch_message(VERSION, "0.0.0", Launcher::Binary)
    })
    .to_string();
    let fake = FakeDaemon::start(response("426 Upgrade Required", Some("0.0.0"), &body));

    let out = dream(project, home, &fake, Some("npx"));
    assert!(!out.status.success(), "{}", describe(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&version_mismatch_message(VERSION, "0.0.0", Launcher::Npx)),
        "{}",
        describe(&out)
    );
    assert!(stderr.contains(UPDATE_NPX), "{}", describe(&out));
    assert!(
        !stderr.contains("start this one with `dreamd watch`"),
        "the daemon's 426 body must not be shown: {}",
        describe(&out)
    );
}

/// Test 11.
#[test]
fn dream_without_a_launcher_names_dreamd_watch() {
    let (project, home) = project_and_home();
    let (project, home) = (project.path(), home.path());
    let fake = no_version_daemon();

    let out = dream(project, home, &fake, None);
    assert!(!out.status.success(), "{}", describe(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&version_unknown_message(VERSION, Launcher::Binary)),
        "{}",
        describe(&out)
    );
    assert!(
        stderr.contains("start this binary with `dreamd watch`"),
        "{}",
        describe(&out)
    );
}

/// Wait up to `secs` for `child` to exit; kill + panic on timeout.
/// Copied from `search_nodes_id.rs`.
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
/// Copied from `search_nodes_id.rs`.
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

/// Copied from `search_nodes_id.rs`.
fn handshake(mcp: &mut McpChild, init_id: u64, timeout: Duration) {
    let init = format!(
        r#"{{"jsonrpc":"2.0","id":{init_id},"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"version-mismatch-launcher","version":"0"}}}}}}"#
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

/// Call `tool` with `arguments` and return the whole JSON-RPC response.
/// Unlike `search_nodes_id.rs`'s `call_tool`, an error is the expected answer.
fn call_tool(mcp: &mut McpChild, id: u64, tool: &str, arguments: Value) -> Value {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    });
    mcp.send(&req.to_string());
    mcp.recv_response(id, TIMEOUT)
}

/// `dreamd mcp` against `fake`, under `DREAMD_LAUNCHER=npx`, after the MCP
/// handshake.
fn spawn_mcp(project: &Path, home: &Path, fake: &FakeDaemon) -> McpChild {
    let mut cmd = dreamd(project, home);
    cmd.env("DREAMD_SOCK", &fake.sock)
        .env("DREAMD_LAUNCHER", "npx")
        .arg("mcp");
    let mut mcp = McpChild::from_command(cmd);
    handshake(&mut mcp, 1, TIMEOUT);
    mcp
}

/// Test 12.
#[test]
fn search_nodes_under_npx_names_npx_commands() {
    let (project, home) = project_and_home();
    let (project, home) = (project.path(), home.path());
    let fake = no_version_daemon();

    let mut mcp = spawn_mcp(project, home, &fake);
    let resp = call_tool(
        &mut mcp,
        2,
        "search_nodes",
        serde_json::json!({ "query": "bzr1531", "k": 5 }),
    );
    let error = resp
        .get("error")
        .unwrap_or_else(|| panic!("search_nodes must answer a JSON-RPC error: {resp}"));
    assert_eq!(error["code"], -32603, "{resp}");
    let message = error["message"]
        .as_str()
        .unwrap_or_else(|| panic!("error has no message: {resp}"));
    assert_eq!(
        message,
        version_unknown_message(VERSION, Launcher::Npx),
        "{resp}"
    );
    assert!(message.contains(UPDATE_NPX), "{resp}");
    mcp.close();
}
