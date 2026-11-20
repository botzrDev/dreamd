//! DRM-106: a dedup key identifies one write attempt, not a topic. The same
//! key with new content is stored, a non-ASCII key and a key with a newline
//! dedup a retry in both modes, and keys live only as long as the server.
//!
//! The tests drive the real binary. Every child gets its own `HOME`,
//! `XDG_CONFIG_HOME` (an empty tempdir, so the user-level config is empty) and
//! `DREAMD_SOCK`, plus `DREAMD_LOG=off`, `GIT_CONFIG_NOSYSTEM=1` and
//! `GIT_CONFIG_GLOBAL=/dev/null`, with `DREAMD_LAUNCHER` and
//! `DREAMD_LOG_FORMAT` removed. The test process's own environment is never
//! changed, and no test reaches the user's daemon. Test files do not share
//! helpers: these are copied from `ingress_parity.rs`. Every wait is bounded.
//! Unix-only: `DREAMD_SOCK` is a Unix socket.

#![cfg(unix)]

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);
const TOPIC_KEY: &str = "session:2026-11-19:api.errors";
const FIRST: &str = "API errors return JSON bodies";
const CORRECTION: &str =
    "The user corrected me: API errors must return RFC 7807 problem+json bodies";

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
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"dedup-key","version":"0"}}}"#;
        self.send(init);
        let resp = self.recv_response(1);
        assert!(
            resp.get("result").is_some() && resp.get("error").is_none(),
            "initialize must succeed; got {resp}"
        );
        self.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    }

    /// `tools/call` of `tool`; the result's text parsed as JSON. Panics on a
    /// JSON-RPC or tool error.
    fn call(&mut self, tool: &str, arguments: &Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments },
        });
        self.send(&req.to_string());
        let resp = self.recv_response(id);
        assert!(resp.get("error").is_none(), "{tool}: error: {resp}");
        assert_ne!(
            resp["result"]["isError"],
            Value::Bool(true),
            "{tool}: tool error: {resp}"
        );
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{tool}: no text content: {resp}"));
        serde_json::from_str(text)
            .unwrap_or_else(|e| panic!("{tool}: text is not JSON: {e}; {text}"))
    }

    /// Close stdin and wait for the server to exit.
    fn close(mut self) {
        drop(self.stdin.take());
        let status = wait_or_kill(&mut self.child, 30);
        assert!(status.success(), "dreamd mcp exit: {status:?}");
    }
}

/// `append_node` arguments for `content` with `key`.
fn args(content: &str, key: &str) -> Value {
    serde_json::json!({
        "content": content,
        "source_harness": "claude-code",
        "skill_action": "testing::drm106",
        "client_dedup_key": key,
    })
}

/// `append_node`; returns `(deduplicated, id)`.
fn append(mcp: &mut McpChild, content: &str, key: &str) -> (bool, String) {
    let body = mcp.call("append_node", &args(content, key));
    let dedup = body["deduplicated"]
        .as_bool()
        .unwrap_or_else(|| panic!("append_node: no deduplicated: {body}"));
    let id = body["id"]
        .as_str()
        .unwrap_or_else(|| panic!("append_node: no id: {body}"))
        .to_owned();
    (dedup, id)
}

/// The project's episodic records.
fn log_records(project: &Path) -> usize {
    read_log(project)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

/// `search_nodes` for `query` until a hit's content is `content`, at most 10
/// tries 200 ms apart (a search waits at most 1 s for the index to commit an
/// append, so under load one try can answer from the last commit). Returns
/// the hit's id.
fn find(mcp: &mut McpChild, query: &str, content: &str) -> String {
    let mut hits = Vec::new();
    for attempt in 0..10 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(200));
        }
        let body = mcp.call(
            "search_nodes",
            &serde_json::json!({ "query": query, "k": 5 }),
        );
        hits = body["results"]
            .as_array()
            .unwrap_or_else(|| panic!("search_nodes: no results array: {body}"))
            .clone();
        if let Some(hit) = hits.iter().find(|h| h["content"] == content) {
            return hit["id"].as_str().expect("hit id").to_owned();
        }
    }
    panic!("search_nodes {query:?} must find {content:?}: {hits:?}")
}

/// Test 8's steps in `project`, through `mcp`.
fn correction_is_stored(mcp: &mut McpChild, project: &Path, mode: &str) {
    let (dedup, first) = append(mcp, FIRST, TOPIC_KEY);
    assert!(!dedup, "{mode}: the first append is written");
    let (dedup, second) = append(mcp, CORRECTION, TOPIC_KEY);
    assert!(
        !dedup,
        "{mode}: the correction, sent with the same topic key, is written"
    );
    assert_ne!(first, second, "{mode}: the correction has its own id");
    assert_eq!(log_records(project), 2, "{mode}: two records");
    assert_eq!(
        find(mcp, "RFC 7807 problem json", CORRECTION),
        second,
        "{mode}: search_nodes finds the correction"
    );
}

/// Test 9's steps in `project`, through `mcp`.
fn retries_dedup(mcp: &mut McpChild, project: &Path, mode: &str) {
    for key in ["clé-é", "a\nb"] {
        let content = format!("a learning sent with the key {key:?}");
        let (dedup, id) = append(mcp, &content, key);
        assert!(!dedup, "{mode}: key {key:?}: the first append is written");
        assert_eq!(
            append(mcp, &content, key),
            (true, id),
            "{mode}: key {key:?}: the retry dedups to the first id"
        );
    }
    assert_eq!(log_records(project), 2, "{mode}: one record per key");
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// DRM-106 test 8 (LP-MCP-04): a correction sent with the per-topic key the
/// old instructions taught is stored, and found, in process and in daemon
/// mode.
#[test]
fn a_correction_with_a_topic_key_is_stored_in_both_modes() {
    let env = Env::new();
    let in_process = env.project("InProcess");
    let daemon = env.project("Daemon");

    // In process: nothing listens on this test's socket yet.
    let mut mcp = McpChild::start(&env, &in_process);
    correction_is_stored(&mut mcp, &in_process, "in process");
    mcp.close();

    let watch = Watch::start(&env, &daemon);
    let mut mcp = McpChild::start(&env, &daemon);
    correction_is_stored(&mut mcp, &daemon, "daemon mode");
    mcp.close();
    drop(watch);
}

/// DRM-106 test 9: a non-ASCII key and a key with a newline dedup a retry in
/// both modes.
#[test]
fn non_ascii_and_newline_keys_dedup_in_both_modes() {
    let env = Env::new();
    let in_process = env.project("InProcess");
    let daemon = env.project("Daemon");

    let mut mcp = McpChild::start(&env, &in_process);
    retries_dedup(&mut mcp, &in_process, "in process");
    mcp.close();

    let watch = Watch::start(&env, &daemon);
    let mut mcp = McpChild::start(&env, &daemon);
    retries_dedup(&mut mcp, &daemon, "daemon mode");
    mcp.close();
    drop(watch);
}

/// DRM-106 test 10 (R2): keys live in memory. After the in-process server
/// restarts, the same arguments with the same key are written again.
#[test]
fn keys_are_forgotten_when_the_server_restarts() {
    let env = Env::new();
    let p = env.project("P");
    let content = "a learning appended before a restart";

    let mut mcp = McpChild::start(&env, &p);
    let (dedup, first) = append(&mut mcp, content, "k");
    assert!(!dedup, "the first append is written");
    mcp.close();

    let mut mcp = McpChild::start(&env, &p);
    let (dedup, second) = append(&mut mcp, content, "k");
    mcp.close();
    assert!(!dedup, "after a restart the key is forgotten");
    assert_ne!(first, second, "a new id");
    assert_eq!(log_records(&p), 2, "two records");
}
