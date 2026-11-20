//! DRM-104: an append is checked once, by one function, in the process that
//! owns the store, so in-process and daemon mode answer it the same way.
//!
//! Both tests drive the real binary. Every child gets its own `HOME`,
//! `XDG_CONFIG_HOME` (an empty tempdir, so the user-level config is empty) and
//! `DREAMD_SOCK`, plus `DREAMD_LOG=off`, `GIT_CONFIG_NOSYSTEM=1` and
//! `GIT_CONFIG_GLOBAL=/dev/null`, with `DREAMD_LAUNCHER` and
//! `DREAMD_LOG_FORMAT` removed. The test process's own environment is never
//! changed, and no test reaches the user's daemon. Test files do not share
//! helpers: these are copied from `project_config.rs`. Every wait is bounded.
//! Unix-only: `DREAMD_SOCK` is a Unix socket.

#![cfg(unix)]

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);
const SKILL_MSG: &str = "invalid skill_action: segment contains characters outside [a-z0-9_]";
const PAIN_MSG: &str = "pain must be in 0.0..=10.0";
const IMPORTANCE_MSG: &str = "importance must be in 0.0..=10.0";
/// JSON-RPC `invalid_params`.
const INVALID_PARAMS: i64 = -32602;

fn dreamd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dreamd")
}

/// One test's `HOME`, empty `XDG_CONFIG_HOME`, socket folder and project
/// folder.
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

    /// This test's `DREAMD_SOCK`. Nothing listens there until a test starts
    /// `watch`.
    fn sock(&self) -> PathBuf {
        self.sock_dir.path().join("d.sock")
    }

    /// A `dreamd` command in `dir`, with the test's environment rules.
    fn dreamd(&self, dir: &Path) -> Command {
        let mut cmd = Command::new(dreamd_bin());
        cmd.current_dir(dir)
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.xdg.path())
            .env("DREAMD_SOCK", self.sock())
            .env("DREAMD_LOG", "off")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("DREAMD_LAUNCHER")
            .env_remove("DREAMD_LOG_FORMAT");
        cmd
    }

    /// Run `dreamd args` in `dir` with stdin null.
    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        self.dreamd(dir)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|e| panic!("run dreamd {args:?}: {e}"))
    }

    /// `<projects>/<name>`, scaffolded by `dreamd init` and registered.
    fn project(&self, name: &str) -> PathBuf {
        let dir = self.projects.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{}\"\nversion = \"0.0.0\"\n",
                name.to_lowercase()
            ),
        )
        .unwrap();
        let out = self.run(&dir, &["init", "--quiet"]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "init {name}: {}",
            describe(&out)
        );
        // The path the daemon's registry holds, and the one to send it.
        std::fs::canonicalize(&dir).unwrap()
    }
}

fn describe(out: &Output) -> String {
    format!(
        "exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The project's episodic log, or `None` when the file does not exist.
fn read_log(project: &Path) -> Option<String> {
    std::fs::read_to_string(project.join(".agent/episodic/AGENT_LEARNINGS.jsonl")).ok()
}

// ── `dreamd watch` ──────────────────────────────────────────────────────────

/// A really-running `dreamd watch`, killed on drop.
struct Watch {
    child: Child,
    console: PathBuf,
}

impl Watch {
    /// Spawn `dreamd watch` in `project`, both output streams in one file
    /// inside the test's `HOME`, and wait for its socket.
    fn start(env: &Env, project: &Path) -> Self {
        let console = env.home.path().join("watch.console");
        let out = std::fs::File::create(&console).expect("create console file");
        let err = out.try_clone().expect("clone console file handle");
        let child = env
            .dreamd(project)
            .arg("watch")
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .expect("spawn dreamd watch");
        let mut watch = Self { child, console };
        watch.wait_for_live_socket(&env.sock());
        watch
    }

    fn console(&self) -> String {
        std::fs::read_to_string(&self.console).unwrap_or_default()
    }

    fn wait_for_live_socket(&mut self, socket: &Path) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if dreamd_core::server::is_daemon_socket_live(socket) {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                panic!(
                    "dreamd watch exited ({status}) before binding {}; console:\n{}",
                    socket.display(),
                    self.console()
                );
            }
            assert!(
                Instant::now() < deadline,
                "dreamd watch never bound {} within {TIMEOUT:?}; console:\n{}",
                socket.display(),
                self.console()
            );
            std::thread::sleep(POLL);
        }
    }
}

impl Drop for Watch {
    /// SIGKILL unconditionally: a leaked daemon holds the writer lock on a
    /// tempdir that is about to disappear.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One raw HTTP/1.1 `POST /api/v1/learn` for `project` over `socket`, with
/// `pain`; returns the status and the body parsed as JSON.
fn post_learn(socket: &Path, project: &Path, pain: f64) -> (u16, Value) {
    let body = serde_json::json!({
        "pain": pain,
        "importance": 5.0,
        "skill_action": "testing::drm104",
        "source_harness": "drm104-test",
        "content": "drm104 over http",
    })
    .to_string();
    let request = format!(
        "POST /api/v1/learn HTTP/1.1\r\nHost: localhost\r\nX-Agent-Root: {}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        project.display(),
        body.len()
    );
    let mut stream = UnixStream::connect(socket).expect("connect daemon socket");
    stream
        .set_read_timeout(Some(TIMEOUT))
        .expect("set read timeout");
    stream.write_all(request.as_bytes()).expect("write request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    let status = response
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("no status line: {response}"));
    let (_, payload) = response
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("no header end: {response}"));
    let json = serde_json::from_str(payload)
        .unwrap_or_else(|e| panic!("body is not JSON ({e}): {response}"));
    (status, json)
}

// ── MCP helpers ─────────────────────────────────────────────────────────────

/// Wait up to `secs` for `child` to exit; kill + panic on timeout.
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
        std::thread::sleep(POLL); // poll process exit
    }
}

/// Stdio MCP child: line-oriented JSON-RPC over piped stdin/stdout.
/// Reader thread + mpsc avoids blocking forever on a pipe read.
struct McpChild {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<String>,
    next_id: u64,
}

impl McpChild {
    /// Spawn `dreamd mcp` in `project` and complete the MCP handshake.
    fn start(env: &Env, project: &Path) -> Self {
        let mut child = env
            .dreamd(project)
            .arg("mcp")
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
        let mut mcp = Self {
            child,
            stdin: Some(stdin),
            rx,
            next_id: 2,
        };
        mcp.handshake();
        mcp
    }

    fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin still open");
        writeln!(stdin, "{line}").expect("write MCP line");
        stdin.flush().expect("flush MCP stdin");
    }

    /// Receive the next JSON-RPC response with matching `id`, skipping notifications.
    fn recv_response(&self, id: u64) -> Value {
        let deadline = Instant::now() + TIMEOUT;
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

    fn handshake(&mut self) {
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"ingress-parity","version":"0"}}}"#;
        self.send(init);
        let resp = self.recv_response(1);
        assert!(
            resp.get("result").is_some() && resp.get("error").is_none(),
            "initialize must succeed; got {resp}"
        );
        self.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    }

    /// `append_node` with `arguments`; returns the whole JSON-RPC response.
    fn append(&mut self, arguments: &Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": "append_node", "arguments": arguments },
        });
        self.send(&req.to_string());
        self.recv_response(id)
    }

    /// Close stdin and wait for the server to exit.
    fn close(mut self) {
        drop(self.stdin.take());
        let status = wait_or_kill(&mut self.child, 30);
        assert!(status.success(), "dreamd mcp exit: {status:?}");
    }
}

/// `append_node` arguments: good defaults, with `overrides` merged in.
fn args(overrides: Value) -> Value {
    let mut a = serde_json::json!({
        "content": "drm104 a learning that must not be written",
        "source_harness": "claude-code",
        "skill_action": "testing::drm104",
    });
    for (k, v) in overrides.as_object().expect("an object") {
        a[k] = v.clone();
    }
    a
}

/// The JSON-RPC error's `(code, message)`, or a panic naming the response.
fn error_of(resp: &Value, what: &str) -> (i64, String) {
    let err = resp
        .get("error")
        .unwrap_or_else(|| panic!("{what}: append_node must fail; got {resp}"));
    (
        err["code"].as_i64().expect("code"),
        err["message"].as_str().expect("message").to_owned(),
    )
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// DRM-104 test 4 (LP-MCP-10): every bad `append_node` gets the same code
/// and message in process and in daemon mode, and writes nothing. The order
/// is `skill_action`, then `pain`, then `importance`, so a call with two bad
/// fields names the first.
#[test]
fn bad_appends_answer_the_same_in_both_modes() {
    let rows: [(&str, Value, &str); 7] = [
        (
            "skill_action alone",
            serde_json::json!({ "skill_action": "Rust.Tokio" }),
            SKILL_MSG,
        ),
        ("pain 12 alone", serde_json::json!({ "pain": 12 }), PAIN_MSG),
        (
            "importance -1 alone",
            serde_json::json!({ "importance": -1 }),
            IMPORTANCE_MSG,
        ),
        (
            "pain 10.0000001 alone",
            serde_json::json!({ "pain": 10.000_000_1 }),
            PAIN_MSG,
        ),
        (
            "skill_action + pain 12",
            serde_json::json!({ "skill_action": "Rust.Tokio", "pain": 12 }),
            SKILL_MSG,
        ),
        (
            "pain 12 + importance 11",
            serde_json::json!({ "pain": 12, "importance": 11 }),
            PAIN_MSG,
        ),
        (
            "skill_action + importance 11",
            serde_json::json!({ "skill_action": "Rust.Tokio", "importance": 11 }),
            SKILL_MSG,
        ),
    ];

    let env = Env::new();
    let p = env.project("P");
    let before = read_log(&p);

    // In process: nothing listens on this test's socket yet.
    let mut mcp = McpChild::start(&env, &p);
    let in_process: Vec<(i64, String)> = rows
        .iter()
        .map(|(what, overrides, _)| error_of(&mcp.append(&args(overrides.clone())), what))
        .collect();
    mcp.close();
    assert_eq!(read_log(&p), before, "in process: P's log is unchanged");

    // Daemon mode: `watch` on the same HOME and socket.
    let watch = Watch::start(&env, &p);
    let mut mcp = McpChild::start(&env, &p);
    let daemon: Vec<(i64, String)> = rows
        .iter()
        .map(|(what, overrides, _)| error_of(&mcp.append(&args(overrides.clone())), what))
        .collect();
    mcp.close();
    assert_eq!(
        read_log(&p),
        before,
        "daemon mode: P's log is unchanged; console:\n{}",
        watch.console()
    );

    for (i, (what, _, want)) in rows.iter().enumerate() {
        assert_eq!(
            in_process[i],
            (INVALID_PARAMS, want.to_string()),
            "{what}: in process"
        );
        assert_eq!(daemon[i], in_process[i], "{what}: the two modes differ");
    }
}

/// DRM-104 test 5 (D4): `pain: 7.3` through in-process MCP, daemon-mode MCP
/// and HTTP learn is stored as one value, the `f32` nearest 7.3.
#[test]
fn http_and_mcp_store_the_same_nearest_f32() {
    let env = Env::new();
    let p = env.project("P");

    let mut mcp = McpChild::start(&env, &p);
    let resp = mcp.append(&args(serde_json::json!({ "pain": 7.3 })));
    assert!(resp.get("error").is_none(), "in process: {resp}");
    mcp.close();

    let watch = Watch::start(&env, &p);
    let mut mcp = McpChild::start(&env, &p);
    let resp = mcp.append(&args(serde_json::json!({ "pain": 7.3 })));
    assert!(resp.get("error").is_none(), "daemon mode: {resp}");
    mcp.close();

    let (status, body) = post_learn(&env.sock(), &p, 7.3);
    assert_eq!(status, 201, "HTTP: {body}; console:\n{}", watch.console());

    let log = read_log(&p).expect("P's log");
    let records: Vec<dreamd_protocol::AgentLearning> = log
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("record ({e}): {l}")))
        .collect();
    assert_eq!(records.len(), 3, "three appends: {log}");
    for record in &records {
        assert_eq!(record.pain, 7.3_f32, "{log}");
    }
    let raw: Vec<Value> = log
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).unwrap()["pain"].clone())
        .collect();
    assert!(
        raw.windows(2).all(|w| w[0] == w[1]),
        "one stored value: {raw:?}"
    );
}
