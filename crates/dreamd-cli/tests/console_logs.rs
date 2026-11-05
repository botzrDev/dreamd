//! BZR-1470: console logs are readable text, library INFO stays off by
//! default, and `dreamd watch` prints its status, not every request.
//!
//! Subprocess tests against the built binary. Every child gets `HOME` set to a
//! tempdir (so `$HOME/.agent/dreamd.sock` stays under `SUN_LEN` and off the
//! developer's real `~/.agent`) and has `DREAMD_SOCK`, `DREAMD_LOG` and
//! `DREAMD_LOG_FORMAT` removed unless the test sets one. The test process's
//! own environment is never changed. Test files don't share helpers, so the
//! `Watch` shape (`cross_home_stop.rs`) and the MCP handshake
//! (`search_nodes_id.rs`) are copied. Every wait is bounded. Unix-only: the
//! daemon binds a Unix socket and the in-process MCP server takes the project
//! writer lock.

#![cfg(unix)]

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const LOG: &str = ".agent/episodic/AGENT_LEARNINGS.jsonl";
const TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);

fn dreamd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dreamd")
}

/// A `dreamd` command in `project` under `home`, with every log variable
/// removed. A test that wants one sets it on the returned command.
fn dreamd(project: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(dreamd_bin());
    cmd.current_dir(project)
        .env("HOME", home)
        .env_remove("DREAMD_SOCK")
        .env_remove("DREAMD_LOG")
        .env_remove("DREAMD_LOG_FORMAT")
        .stdin(Stdio::null());
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

/// A project and a separate daemon home, both temporary.
struct Store {
    project: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl Store {
    fn run(&self, args: &[&str], dreamd_log: Option<&str>) -> Output {
        let mut cmd = dreamd(self.project.path(), self.home.path());
        cmd.args(args);
        if let Some(v) = dreamd_log {
            cmd.env("DREAMD_LOG", v);
        }
        cmd.output()
            .unwrap_or_else(|e| panic!("run dreamd {args:?}: {e}"))
    }
}

fn record(id: &str, skill_action: &str, content: &str, ts: &str) -> String {
    let mut line = serde_json::json!({
        "schema_version": "1.0.0",
        "id": id,
        "timestamp": ts,
        "pain": 7.0,
        "importance": 8.0,
        "skill_action": skill_action,
        "source_harness": "claude-code",
        "content": content,
    })
    .to_string();
    line.push('\n');
    line
}

/// `dreamd init --quiet` in a project with a `.git` root sentinel, then three
/// valid log lines with RFC 3339 timestamps.
fn seed() -> Store {
    let store = Store {
        project: tempfile::tempdir().expect("temp project"),
        home: tempfile::tempdir().expect("temp HOME"),
    };
    std::fs::create_dir(store.project.path().join(".git")).expect("write .git root sentinel");
    let out = store.run(&["init", "--quiet"], None);
    assert!(
        out.status.success(),
        "dreamd init --quiet: {}",
        describe(&out)
    );

    let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let lines = record(
        "evt_01ARZ3NDEKTSV4RRFFQ69G5FA1",
        "rust::axum::extractors",
        "axum extractor order matters for the body",
        &ts,
    ) + &record(
        "evt_01ARZ3NDEKTSV4RRFFQ69G5FA2",
        "rust::axum::routing",
        "axum nested routers need a leading slash",
        &ts,
    ) + &record(
        "evt_01ARZ3NDEKTSV4RRFFQ69G5FA3",
        "rust::tokio::runtime",
        "tokio block_in_place needs the multi-thread runtime",
        &ts,
    );
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(store.project.path().join(LOG))
        .expect("open episodic log");
    log.write_all(lines.as_bytes()).expect("write episodic log");
    store
}

/// [`seed`], then `dreamd doctor --repair` to build the index the read-only
/// CLI readers need.
fn seed_with_index() -> Store {
    let store = seed();
    // Other doctor checks may warn on a fresh store, so the exit code is not asserted.
    let out = store.run(&["doctor", "--repair"], None);
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .contains("repair: rebuilt index from the episodic log"),
        "doctor --repair did not build the index: {}",
        describe(&out)
    );
    store
}

// ── watch ───────────────────────────────────────────────────────────────────

/// A really-running `dreamd watch`, killed on drop. Copied from
/// `cross_home_stop.rs`.
struct Watch {
    child: Child,
    console: PathBuf,
}

impl Watch {
    /// Spawn `dreamd watch` with both output streams redirected to one file
    /// inside `home`, so stderr is not a terminal. `env` is applied after
    /// `dreamd()` clears `DREAMD_LOG` and `DREAMD_LOG_FORMAT`.
    fn spawn(project: &Path, home: &Path, env: &[(&str, &str)]) -> Self {
        let console = home.join("watch.console");
        let out = std::fs::File::create(&console).expect("create console file");
        let err = out.try_clone().expect("clone console file handle");
        let mut cmd = dreamd(project, home);
        cmd.arg("watch")
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err));
        for (key, value) in env {
            cmd.env(key, value);
        }
        let child = cmd.spawn().expect("spawn dreamd watch");
        Self { child, console }
    }

    fn console(&self) -> String {
        std::fs::read_to_string(&self.console).unwrap_or_default()
    }
}

impl Drop for Watch {
    /// SIGKILL unconditionally, as `cross_home_stop.rs` does: a leaked daemon
    /// holds the writer lock on a tempdir that is about to disappear.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_for_live_socket(watch: &mut Watch, socket: &Path) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if dreamd_core::server::is_daemon_socket_live(socket) {
            return;
        }
        if let Some(status) = watch.child.try_wait().expect("try_wait") {
            panic!(
                "dreamd watch exited ({status}) before binding {}; console:\n{}",
                socket.display(),
                watch.console()
            );
        }
        assert!(
            Instant::now() < deadline,
            "dreamd watch never bound {} within {TIMEOUT:?}; console:\n{}",
            socket.display(),
            watch.console()
        );
        std::thread::sleep(POLL);
    }
}

/// One raw HTTP/1.1 `GET /api/v1/health` over the daemon socket; returns the
/// whole response.
fn get_health(socket: &Path, project: &Path) -> String {
    let mut stream = UnixStream::connect(socket).expect("connect daemon socket");
    stream
        .set_read_timeout(Some(TIMEOUT))
        .expect("set read timeout");
    let request = format!(
        "GET /api/v1/health HTTP/1.1\r\nHost: localhost\r\nX-Agent-Root: {}\r\nConnection: close\r\n\r\n",
        project.display()
    );
    stream.write_all(request.as_bytes()).expect("write request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    response
}

#[test]
fn watch_console_shows_status_and_the_file_keeps_requests() {
    let store = seed();
    let project = std::fs::canonicalize(store.project.path()).expect("canonical project");
    let socket = store.home.path().join(".agent").join("dreamd.sock");
    let daemon_log = store.home.path().join(".agent").join("dreamd.log");

    let mut watch = Watch::spawn(store.project.path(), store.home.path(), &[]);
    wait_for_live_socket(&mut watch, &socket);

    for i in 0..3 {
        let response = get_health(&socket, &project);
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "request {i} must answer 200; got:\n{response}\nconsole:\n{}",
            watch.console()
        );
    }

    // The file appender is non-blocking, so poll for the three request lines.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let log = std::fs::read_to_string(&daemon_log).unwrap_or_default();
        let requests = log.lines().filter(|l| l.contains("http request")).count();
        if requests >= 3 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "dreamd.log has {requests} `http request` line(s) after {TIMEOUT:?}, want 3:\n{log}"
        );
        std::thread::sleep(POLL);
    }

    let console = watch.console();
    assert!(
        console
            .lines()
            .any(|l| l.contains("dreamd watch: serving on")),
        "console must show the start line:\n{console}"
    );
    assert!(
        !console.lines().any(|l| l.contains("http request")),
        "console must leave request lines to dreamd.log:\n{console}"
    );
    assert!(
        !console.lines().any(|l| l.starts_with('{')),
        "console must be text off a terminal:\n{console}"
    );
}

/// `DREAMD_LOG` and `DREAMD_LOG_FORMAT` reach the subscriber `watch` installs
/// (BZR-1470 test 13). The file-path call is the one this process uses.
#[test]
fn watch_honours_dreamd_log_and_log_format() {
    let store = seed();
    let project = std::fs::canonicalize(store.project.path()).expect("canonical project");
    let socket = store.home.path().join(".agent").join("dreamd.sock");

    let mut watch = Watch::spawn(
        store.project.path(),
        store.home.path(),
        &[("DREAMD_LOG", "info"), ("DREAMD_LOG_FORMAT", "json")],
    );
    wait_for_live_socket(&mut watch, &socket);

    for i in 0..3 {
        let response = get_health(&socket, &project);
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "request {i} must answer 200; got:\n{response}\nconsole:\n{}",
            watch.console()
        );
    }

    let deadline = Instant::now() + TIMEOUT;
    let console = loop {
        let console = watch.console();
        let requests = console
            .lines()
            .filter(|l| l.contains("http request"))
            .count();
        if requests >= 3 {
            break console;
        }
        assert!(
            Instant::now() < deadline,
            "console has {requests} `http request` line(s) after {TIMEOUT:?}, want 3:\n{console}"
        );
        std::thread::sleep(POLL);
    };

    assert!(
        console
            .lines()
            .any(|l| l.contains("dreamd watch: serving on")),
        "console must show the start line:\n{console}"
    );
    for line in console.lines().filter(|l| !l.is_empty()) {
        let v: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("console line is not JSON ({e}): {line}"));
        assert!(v.is_object(), "console line is not a JSON object: {line}");
    }
}

// ── blame ───────────────────────────────────────────────────────────────────

#[test]
fn blame_stdout_is_only_the_table_and_json_even_at_trace() {
    let store = seed_with_index();

    let out = store.run(&["blame", "axum"], Some("trace"));
    assert_eq!(out.status.code(), Some(0), "blame axum: {}", describe(&out));
    let stdout = String::from_utf8(out.stdout.clone()).expect("stdout is UTF-8");
    for line in stdout.lines() {
        let hits = line.starts_with('(') && line.ends_with(" hits)");
        assert!(
            line.starts_with('|') || hits,
            "stdout line is not the table: {line:?}\nstdout:\n{stdout}"
        );
    }
    assert!(
        stdout
            .lines()
            .any(|l| l.contains("evt_01ARZ3NDEKTSV4RRFFQ69G5FA")),
        "the table must have a hit row:\n{stdout}"
    );

    let out = store.run(&["blame", "axum", "--json"], Some("trace"));
    assert_eq!(
        out.status.code(),
        Some(0),
        "blame axum --json: {}",
        describe(&out)
    );
    serde_json::from_slice::<Value>(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "blame --json stdout must be one JSON value ({e}):\n{}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
}

#[test]
fn blame_default_stderr_is_empty() {
    let store = seed_with_index();
    let out = store.run(&["blame", "axum"], None);
    assert_eq!(out.status.code(), Some(0), "blame axum: {}", describe(&out));
    assert!(
        out.stderr.is_empty(),
        "blame stderr must be empty by default:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── mcp ─────────────────────────────────────────────────────────────────────

/// Wait up to [`TIMEOUT`] for `child` to exit; kill + panic on timeout.
/// Copied from `search_nodes_id.rs`.
fn wait_or_kill(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("MCP child did not exit within {TIMEOUT:?} of stdin EOF");
        }
        std::thread::sleep(POLL);
    }
}

/// Stdio MCP child with stderr redirected to a file. Copied from
/// `search_nodes_id.rs`, which sends stderr to null.
struct McpChild {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<String>,
}

impl McpChild {
    fn spawn(mut cmd: Command, stderr: &Path) -> Self {
        let err = std::fs::File::create(stderr).expect("create stderr file");
        let mut child = cmd
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(err))
            .spawn()
            .expect("spawn dreamd mcp");
        let stdout = child.stdout.take().expect("child stdout");
        let stdin = child.stdin.take().expect("child stdin");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
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

    /// The next JSON-RPC response with `id`, skipping notifications.
    fn recv_response(&self, id: u64) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timeout waiting for MCP response id={id}"
            );
            let line = self
                .rx
                .recv_timeout(remaining)
                .unwrap_or_else(|e| panic!("recv_timeout waiting for id={id}: {e}"));
            let v: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("invalid JSON from MCP child: {e}; line={line}"));
            if v.get("id").and_then(Value::as_u64) == Some(id) {
                return v;
            }
        }
    }

    /// Close stdin and wait for the server to exit.
    fn close(mut self) {
        drop(self.stdin.take());
        let status = wait_or_kill(&mut self.child);
        assert!(status.success(), "dreamd mcp exit: {status:?}");
    }
}

/// `initialize`, `notifications/initialized` and one `search_nodes` into
/// `dreamd mcp`, then stdin closed and the exit awaited. Returns stderr.
fn mcp_session(store: &Store, dreamd_log: Option<&str>) -> String {
    let stderr = store
        .home
        .path()
        .join(format!("mcp.stderr.{}", dreamd_log.unwrap_or("default")));
    let mut cmd = dreamd(store.project.path(), store.home.path());
    if let Some(v) = dreamd_log {
        cmd.env("DREAMD_LOG", v);
    }
    let mut mcp = McpChild::spawn(cmd, &stderr);

    mcp.send(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"console-logs","version":"0"}}}"#,
    );
    let resp = mcp.recv_response(1);
    assert!(resp.get("result").is_some(), "initialize failed: {resp}");
    mcp.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);

    let call = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": "search_nodes", "arguments": { "query": "axum" } },
    });
    mcp.send(&call.to_string());
    let resp = mcp.recv_response(2);
    assert!(resp.get("error").is_none(), "search_nodes failed: {resp}");

    mcp.close();
    std::fs::read_to_string(&stderr).expect("read mcp stderr")
}

/// Lines at INFO whose target is not one of dreamd's crates. A text line reads
/// `<time>  INFO [span{…}: ]<target>: <message>`, so a dreamd line has a
/// whitespace token after ` INFO ` that starts with `dreamd` and ends in `:`.
fn non_dreamd_info_lines(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|l| {
            l.split_once(" INFO ").is_some_and(|(_, rest)| {
                !rest
                    .split_whitespace()
                    .any(|t| t.starts_with("dreamd") && t.ends_with(':'))
            })
        })
        .collect()
}

#[test]
fn mcp_default_stderr_has_no_library_info() {
    let store = seed();

    let stderr = mcp_session(&store, None);
    assert!(
        !stderr.contains("rmcp"),
        "default dreamd mcp stderr must hold rmcp to WARN:\n{stderr}"
    );
    let lib_info = non_dreamd_info_lines(&stderr);
    assert!(
        lib_info.is_empty(),
        "default dreamd mcp stderr has library INFO lines: {lib_info:?}\n{stderr}"
    );

    let stderr = mcp_session(&store, Some("info"));
    assert!(
        stderr.contains("rmcp"),
        "DREAMD_LOG=info must show rmcp's INFO lines:\n{stderr}"
    );
}
