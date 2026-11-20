//! DRM-103: the daemon and the CLI use each project's own config.
//!
//! Tests 1–5 drive the real binary. Every child gets its own `HOME`,
//! `XDG_CONFIG_HOME` (an empty tempdir, so the user-level config is empty) and
//! `DREAMD_SOCK`, plus `DREAMD_LOG=off`, `GIT_CONFIG_NOSYSTEM=1` and
//! `GIT_CONFIG_GLOBAL=/dev/null`, with `DREAMD_LAUNCHER` and
//! `DREAMD_LOG_FORMAT` removed. The test process's own environment is never
//! changed, and no test reaches the user's daemon. Test files do not share
//! helpers: the MCP helpers are copied from `search_nodes_id.rs` and the
//! `Watch` guard from `watch_sock.rs`. Every wait is bounded. Unix-only:
//! `DREAMD_SOCK` is a Unix socket.

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
/// An AWS access key id shape: the redactor replaces it with `[REDACTED]`.
const KEY: &str = "AKIAFAKEFAKEFAKE1234";
const REDACTED: &str = "[REDACTED]";
const CONFIG_LOAD_FAILED: &str = "config load failed: ";

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
        assert!(
            config_path(&dir).is_file(),
            "init wrote the config template"
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

fn config_path(project: &Path) -> PathBuf {
    project.join(".agent/.dreamd/config.toml")
}

fn log_path(project: &Path) -> PathBuf {
    project.join(".agent/episodic/AGENT_LEARNINGS.jsonl")
}

/// The project's episodic log, or `None` when the file does not exist.
fn read_log(project: &Path) -> Option<String> {
    std::fs::read_to_string(log_path(project)).ok()
}

// ── `dreamd watch`, copied from `watch_sock.rs` ─────────────────────────────

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
/// `content`; returns the status and the body parsed as JSON.
fn post_learn(socket: &Path, project: &Path, content: &str) -> (u16, Value) {
    let body = serde_json::json!({
        "pain": 5.0,
        "importance": 5.0,
        "skill_action": "testing::drm103",
        "source_harness": "drm103-test",
        "content": content,
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

// ── MCP helpers, copied from `search_nodes_id.rs` ───────────────────────────

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
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"project-config","version":"0"}}}"#;
        self.send(init);
        let resp = self.recv_response(1);
        assert!(
            resp.get("result").is_some() && resp.get("error").is_none(),
            "initialize must succeed; got {resp}"
        );
        self.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    }

    /// `append_node` with `content`; returns the whole JSON-RPC response.
    fn append(&mut self, id: u64, content: &str) -> Value {
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "append_node",
                "arguments": {
                    "content": content,
                    "source_harness": "claude-code",
                    "skill_action": "testing::drm103",
                },
            },
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

/// Assert an `append_node` answer is a tool success.
fn assert_append_ok(resp: &Value) {
    assert!(resp.get("error").is_none(), "append_node: error: {resp}");
    assert_ne!(
        resp["result"]["isError"],
        Value::Bool(true),
        "append_node: tool error: {resp}"
    );
}

/// Assert an `append_node` answer is `-32600` with the config-load message.
fn assert_config_load_failed(resp: &Value, mode: &str) {
    let err = resp
        .get("error")
        .unwrap_or_else(|| panic!("{mode}: append_node must fail; got {resp}"));
    assert_eq!(err["code"], Value::from(-32600), "{mode}: {resp}");
    let message = err["message"].as_str().expect("message");
    assert!(message.starts_with(CONFIG_LOAD_FAILED), "{mode}: {message}");
}

/// Projects A (`redaction = false`) and B (the template, so redaction is
/// on), with `watch` started in A.
fn two_projects_watch_in_a() -> (Env, PathBuf, PathBuf, Watch) {
    let env = Env::new();
    let a = env.project("A");
    let b = env.project("B");
    std::fs::write(config_path(&a), "redaction = false\n").unwrap();
    let watch = Watch::start(&env, &a);
    (env, a, b, watch)
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// DRM-103 test 1 (LP-DATA-07): a daemon started in A, whose config turns
/// redaction off, still redacts B by B's own config, and leaves A's key as
/// A's config says.
#[test]
fn daemon_redacts_each_project_by_its_own_config() {
    let (env, a, b, watch) = two_projects_watch_in_a();
    let content = format!("drm103 the deploy key is {KEY}");

    for project in [&b, &a] {
        let (status, body) = post_learn(&env.sock(), project, &content);
        assert_eq!(
            status,
            201,
            "learn for {}: {body}; console:\n{}",
            project.display(),
            watch.console()
        );
    }

    let b_log = read_log(&b).expect("B's log");
    assert!(b_log.contains(REDACTED), "B is redacted: {b_log}");
    assert!(!b_log.contains(KEY), "B must not store the key: {b_log}");
    let a_log = read_log(&a).expect("A's log");
    assert!(
        a_log.contains(KEY),
        "A's config turns redaction off: {a_log}"
    );
}

/// DRM-103 test 2: an edit to a project's config applies to the next append,
/// with no restart.
#[test]
fn daemon_picks_up_a_config_edit_on_the_next_append() {
    let (env, a, _b, watch) = two_projects_watch_in_a();
    std::fs::write(config_path(&a), "redaction = true\n").unwrap();

    let (status, body) = post_learn(&env.sock(), &a, &format!("drm103 after the edit {KEY}"));
    assert_eq!(
        status,
        201,
        "learn for A: {body}; console:\n{}",
        watch.console()
    );
    let a_log = read_log(&a).expect("A's log");
    assert!(
        a_log.contains(REDACTED),
        "A is redacted after the edit: {a_log}"
    );
    assert!(!a_log.contains(KEY), "A must not store the key: {a_log}");
}

/// DRM-103 test 3: `append_node` through `dreamd mcp` in daemon mode is
/// redacted by the project's config, not the boot project's.
#[test]
fn daemon_mcp_append_uses_the_projects_config() {
    let (env, _a, b, _watch) = two_projects_watch_in_a();

    let mut mcp = McpChild::start(&env, &b);
    let resp = mcp.append(2, &format!("drm103 through mcp {KEY}"));
    assert_append_ok(&resp);
    mcp.close();

    let b_log = read_log(&b).expect("B's log");
    assert!(b_log.contains(REDACTED), "B is redacted: {b_log}");
    assert!(!b_log.contains(KEY), "B must not store the key: {b_log}");
}

/// DRM-103 test 4: a project config that stops parsing while `dreamd mcp`
/// runs fails the next append with `-32600` `config load failed: …`, in
/// process and in daemon mode, and writes nothing. Over HTTP the learn is
/// 500 with the same prefix.
#[test]
fn a_malformed_project_config_fails_the_append_with_32600_in_both_modes() {
    let env = Env::new();
    let p = env.project("P");
    let q = env.project("Q");
    let before = read_log(&p);
    let content = "drm103 a learning that must not be written";

    // In process: nothing listens on this test's socket yet.
    let mut mcp = McpChild::start(&env, &p);
    std::fs::write(config_path(&p), "redaction = [\n").unwrap();
    assert_config_load_failed(&mcp.append(2, content), "in process");
    mcp.close();
    assert_eq!(read_log(&p), before, "in process: P's log is unchanged");

    // Daemon mode: `watch` runs in Q. `mcp` starts with a config that loads.
    std::fs::write(config_path(&p), "# valid again\n").unwrap();
    let watch = Watch::start(&env, &q);
    let mut mcp = McpChild::start(&env, &p);
    std::fs::write(config_path(&p), "redaction = [\n").unwrap();
    assert_config_load_failed(&mcp.append(2, content), "daemon mode");
    mcp.close();
    assert_eq!(read_log(&p), before, "daemon mode: P's log is unchanged");

    let (status, body) = post_learn(&env.sock(), &p, content);
    assert_eq!(
        status,
        500,
        "learn for P: {body}; console:\n{}",
        watch.console()
    );
    let error = body["error"].as_str().expect("error string");
    assert!(error.starts_with(CONFIG_LOAD_FAILED), "{error}");
    assert_eq!(read_log(&p), before, "HTTP: P's log is unchanged");
}

/// DRM-103 test 5 (LP-CLI-06): `doctor`, `dream` and `mcp` read the config
/// of the store they find, so they answer the same from the project and from
/// a subfolder of it. With a malformed file, only the cwd-relative spelling
/// of its path differs (Amendment 2).
#[test]
fn doctor_dream_and_mcp_read_the_projects_config_from_a_subfolder() {
    let env = Env::new();
    let p = env.project("P");
    let src = p.join("src");
    std::fs::create_dir(&src).unwrap();

    for (case, config) in [
        ("auto", "dream_cycle_mode = \"auto\"\n"),
        ("malformed", "redaction = [\n"),
    ] {
        std::fs::write(config_path(&p), config).unwrap();
        for args in [&["doctor"][..], &["dream", "--no-llm"], &["mcp"]] {
            let from_p = env.run(&p, args);
            let from_src = env.run(&src, args);
            let what = format!("{case}: {args:?}");
            assert_eq!(
                from_p.status.code(),
                from_src.status.code(),
                "{what}: exit codes differ\nfrom P: {}\nfrom P/src: {}",
                describe(&from_p),
                describe(&from_src)
            );
            let stderr = String::from_utf8_lossy(&from_p.stderr);
            let mut src_stderr = String::from_utf8_lossy(&from_src.stderr).into_owned();
            if case == "malformed" {
                // Amendment 2: figment names a file source relative to the
                // cwd when that is shorter, so from P/src the parse error
                // names `../.agent/…`. That spelling shows it read P's file.
                assert!(
                    src_stderr.contains("../.agent/.dreamd/config.toml"),
                    "{what}: P/src must read P's config; {}",
                    describe(&from_src)
                );
                src_stderr = src_stderr.replace(
                    "../.agent/.dreamd/config.toml",
                    ".agent/.dreamd/config.toml",
                );
            }
            assert_eq!(stderr, src_stderr, "{what}: stderr differs");
            let want = match (case, args[0]) {
                ("auto", "doctor") => None,
                ("auto", "dream") => Some((2, "dream_cycle_mode = auto")),
                ("auto", _) => Some((1, "dream_cycle_mode = auto")),
                _ => Some((1, "config.toml")),
            };
            if let Some((code, text)) = want {
                assert_eq!(
                    from_p.status.code(),
                    Some(code),
                    "{what}: {}",
                    describe(&from_p)
                );
                assert!(stderr.contains(text), "{what}: {}", describe(&from_p));
            }
        }
    }
}
