//! DRM-122: a client asks the daemon its version before its first call, so a
//! daemon of another version never serves a write, and the advice names the
//! older side.
//!
//! A fake daemon on a std thread serves each connection on its own thread,
//! records each request's method and path, and answers from a table. Each
//! child gets the test's own `HOME`, an empty `XDG_CONFIG_HOME`, `DREAMD_SOCK`
//! aimed at the fake, `DREAMD_LOG=off`, and `DREAMD_LOG_FORMAT` and
//! `DREAMD_LAUNCHER` removed (test 7 sets `DREAMD_LAUNCHER=npx`). `dreamd init`
//! runs before the fake binds, so it never meets it. A child that runs past
//! 15 s is killed and the test fails. The test process's own environment is
//! never changed. The MCP helpers are copied from `daemon_deadline.rs` (test
//! files do not share helpers). Unix-only: the client paths under test speak
//! HTTP over a Unix socket.

#![cfg(unix)]

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The longest a child under test may run.
const CHILD_LIMIT: Duration = Duration::from_secs(15);
/// The longest `dreamd init` may run. It is setup, not under test, and a
/// loaded machine has stalled it past [`CHILD_LIMIT`].
const SETUP_LIMIT: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(50);

fn dreamd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dreamd")
}

/// One test's `HOME`, empty `XDG_CONFIG_HOME`, socket folder and project.
struct Env {
    home: tempfile::TempDir,
    xdg: tempfile::TempDir,
    sock_dir: tempfile::TempDir,
    projects: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().expect("home tempdir"),
            xdg: tempfile::tempdir().expect("XDG_CONFIG_HOME tempdir"),
            sock_dir: tempfile::tempdir().expect("socket tempdir"),
            projects: tempfile::tempdir().expect("projects tempdir"),
        }
    }

    fn sock(&self) -> PathBuf {
        self.sock_dir.path().join("d.sock")
    }

    fn dreamd(&self, dir: &Path) -> Command {
        let mut cmd = Command::new(dreamd_bin());
        cmd.current_dir(dir)
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.xdg.path())
            .env("DREAMD_SOCK", self.sock())
            .env("DREAMD_LOG", "off")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("DREAMD_LOG_FORMAT")
            .env_remove("DREAMD_LAUNCHER")
            .env_remove("DREAMD_CLIENT_TIMEOUT")
            .stdin(Stdio::null());
        cmd
    }

    /// Project P, scaffolded by `dreamd init` before the fake binds.
    fn project(&self) -> PathBuf {
        let dir = self.projects.path().join("P");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"p\"\nversion = \"0.0.0\"\n",
        )
        .unwrap();
        let ran = run_within(
            self.dreamd(&dir).args(["init", "--quiet"]),
            self,
            SETUP_LIMIT,
        );
        assert_eq!(ran.code, Some(0), "init: {}", ran.stderr);
        std::fs::canonicalize(&dir).unwrap()
    }

    fn scratch_file(&self, what: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        self.home.path().join(format!(
            "{what}-{}.txt",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }
}

/// A finished child.
struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Run `cmd` to its end, killed past [`CHILD_LIMIT`].
fn run_bounded(cmd: &mut Command, env: &Env) -> Ran {
    run_within(cmd, env, CHILD_LIMIT)
}

/// [`run_bounded`] with its own limit.
fn run_within(cmd: &mut Command, env: &Env, limit: Duration) -> Ran {
    let out_path = env.scratch_file("stdout");
    let err_path = env.scratch_file("stderr");
    let started = Instant::now();
    let mut child = cmd
        .stdout(Stdio::from(std::fs::File::create(&out_path).unwrap()))
        .stderr(Stdio::from(std::fs::File::create(&err_path).unwrap()))
        .spawn()
        .expect("spawn dreamd");
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if started.elapsed() >= limit {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "a dreamd child ran past {limit:?}; stderr:\n{}",
                std::fs::read_to_string(&err_path).unwrap_or_default()
            );
        }
        std::thread::sleep(POLL);
    };
    Ran {
        code: status.code(),
        stdout: std::fs::read_to_string(&out_path).unwrap_or_default(),
        stderr: std::fs::read_to_string(&err_path).unwrap_or_default(),
    }
}

// ── the fake daemon ─────────────────────────────────────────────────────────

/// One raw HTTP/1.1 response.
fn response(status: &str, version: Option<&str>, body: &str) -> Vec<u8> {
    let version = version
        .map(|v| format!("x-dreamd-version: {v}\r\n"))
        .unwrap_or_default();
    format!(
        "HTTP/1.1 {status}\r\n{version}content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A fake daemon that records `METHOD /path` for each request and answers
/// with `answer(method, path)`.
struct FakeDaemon {
    seen: Arc<Mutex<Vec<String>>>,
}

impl FakeDaemon {
    fn start(sock: &Path, answer: fn(&str, &str) -> Vec<u8>) -> Self {
        let listener = UnixListener::bind(sock).expect("bind the fake daemon");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let log = Arc::clone(&log);
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request_line = String::new();
                    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut length = 0usize;
                    let mut line = String::new();
                    loop {
                        line.clear();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        if line == "\r\n" {
                            break;
                        }
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                            length = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0u8; length];
                    let _ = reader.read_exact(&mut body);
                    let mut parts = request_line.split_whitespace();
                    let method = parts.next().unwrap_or_default().to_string();
                    let path = parts
                        .next()
                        .unwrap_or_default()
                        .split('?')
                        .next()
                        .unwrap_or_default()
                        .to_string();
                    log.lock().unwrap().push(format!("{method} {path}"));
                    let _ = stream.write_all(&answer(&method, &path));
                });
            }
        });
        Self { seen }
    }

    fn count(&self, request: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| *seen == request)
            .count()
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

/// A daemon from 1.2.2: no version header and no version layer, so it serves
/// a learn and a dream cycle.
fn pre_handshake_daemon(method: &str, path: &str) -> Vec<u8> {
    match (method, path) {
        ("POST", "/api/v1/learn") => response(
            "201 Created",
            None,
            r#"{"id":"evt_01ARZ3NDEKTSV4RRFFQ69G5FAV","timestamp":"2026-11-20T00:00:00+00:00","deduplicated":false}"#,
        ),
        ("POST", "/api/v1/dream") => response("200 OK", None, r#"{"status":"ok"}"#),
        ("GET", "/api/v1/health") => response(
            "400 Bad Request",
            None,
            r#"{"error":"X-Agent-Root header missing"}"#,
        ),
        _ => response("200 OK", None, r#"{"results":[]}"#),
    }
}

/// A daemon of version 99.0.0 that refuses a POST from this client with 426.
fn newer_daemon(method: &str, _path: &str) -> Vec<u8> {
    if method == "POST" {
        response(
            "426 Upgrade Required",
            Some("99.0.0"),
            r#"{"error":"version mismatch"}"#,
        )
    } else {
        response("400 Bad Request", Some("99.0.0"), r#"{"error":"no root"}"#)
    }
}

// ── `dreamd mcp`, copied from `daemon_deadline.rs` ──────────────────────────

struct McpChild {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<String>,
}

impl McpChild {
    fn from_command(cmd: &mut Command) -> Self {
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

    fn recv_response(&self, id: u64) -> Value {
        let deadline = Instant::now() + CHILD_LIMIT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                panic!("no MCP response id={id} within {CHILD_LIMIT:?}");
            }
            let line = self
                .rx
                .recv_timeout(remaining)
                .unwrap_or_else(|e| panic!("no MCP response id={id}: {e}"));
            let v: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("invalid JSON from MCP child: {e}; line={line}"));
            if v.get("id").and_then(|i| i.as_u64()) == Some(id) {
                return v;
            }
        }
    }

    fn close(mut self) {
        drop(self.stdin.take());
        let started = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                break status;
            }
            if started.elapsed() >= CHILD_LIMIT {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("dreamd mcp did not exit within {CHILD_LIMIT:?} of stdin EOF");
            }
            std::thread::sleep(POLL);
        };
        assert!(status.success(), "dreamd mcp exit: {status:?}");
    }
}

impl Drop for McpChild {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn spawn_mcp(cmd: &mut Command) -> McpChild {
    let mut mcp = McpChild::from_command(cmd.arg("mcp"));
    mcp.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"version-preflight","version":"0"}}}"#);
    let resp = mcp.recv_response(1);
    assert!(
        resp.get("result").is_some(),
        "initialize must succeed; got {resp}"
    );
    mcp.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    mcp
}

/// `append_node`'s JSON-RPC error.
fn append_error(mcp: &mut McpChild, id: u64) -> Value {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": "append_node",
            "arguments": {
                "content": "one learning, sent three times",
                "source_harness": "claude-code",
                "skill_action": "testing::drm122",
            },
        },
    });
    mcp.send(&req.to_string());
    let resp = mcp.recv_response(id);
    resp.get("error")
        .cloned()
        .unwrap_or_else(|| panic!("append_node must answer a JSON-RPC error: {resp}"))
}

// ── tests ───────────────────────────────────────────────────────────────────

/// DRM-122 test 6 (LP-INSTALL-01): against a daemon from before the version
/// handshake, `append_node` and `dreamd dream` fail before anything is sent.
#[test]
fn append_against_a_pre_handshake_daemon_writes_nothing() {
    let env = Env::new();
    let p = env.project();
    let fake = FakeDaemon::start(&env.sock(), pre_handshake_daemon);

    let mut mcp = spawn_mcp(&mut env.dreamd(&p));
    for id in 2..=4 {
        let error = append_error(&mut mcp, id);
        assert_eq!(error["code"], -32603, "{error}");
        let message = error["message"].as_str().unwrap_or_default();
        assert!(message.contains("daemon unknown"), "{error}");
    }
    mcp.close();
    assert_eq!(
        fake.count("POST /api/v1/learn"),
        0,
        "no learn may reach the old daemon: {:?}",
        fake.seen()
    );

    let dream = run_bounded(env.dreamd(&p).args(["dream", "--no-llm"]), &env);
    assert_eq!(
        dream.code,
        Some(1),
        "dream: stdout {}\nstderr {}",
        dream.stdout,
        dream.stderr
    );
    assert!(dream.stderr.contains("daemon unknown"), "{}", dream.stderr);
    assert_eq!(
        fake.count("POST /api/v1/dream"),
        0,
        "no cycle may reach the old daemon: {:?}",
        fake.seen()
    );
}

/// DRM-122 test 7 (LP-INSTALL-08): a client older than the daemon is told to
/// update itself, or under npx to remove its pin, not to restart the daemon.
#[test]
fn an_older_client_is_told_to_update_itself() {
    let env = Env::new();
    let p = env.project();
    let _fake = FakeDaemon::start(&env.sock(), newer_daemon);

    let mut mcp = spawn_mcp(env.dreamd(&p).env("DREAMD_LAUNCHER", "npx"));
    let error = append_error(&mut mcp, 2);
    mcp.close();
    assert_eq!(error["code"], -32603, "{error}");
    assert_eq!(
        error["message"].as_str().unwrap_or_default(),
        format!(
            "version mismatch: client {VERSION}, daemon 99.0.0. This client is older than the \
             daemon. If your MCP config pins `dreamd-mcp@{VERSION}`, change it to \
             `npx -y dreamd-mcp` and reload your MCP harness. Or point DREAMD_SOCK at a \
             daemon of version {VERSION}."
        ),
        "{error}"
    );
}
