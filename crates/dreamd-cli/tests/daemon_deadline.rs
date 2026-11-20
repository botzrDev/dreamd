//! DRM-121: every daemon client call has a deadline, and `dreamd status`
//! checks that the daemon answers.
//!
//! A daemon that accepts connections and never answers (LP-CONFIG-04: a
//! `watch` stopped with `kill -STOP`, or a wedged one) fails `dreamd dream`,
//! both MCP tools and `dreamd status` within their deadlines, and nothing runs
//! in-process beside it. An answer inside the deadline still succeeds.
//!
//! Each child gets the test's own `HOME`, an empty `XDG_CONFIG_HOME`, an
//! explicit `DREAMD_SOCK` under a temp dir, `DREAMD_CLIENT_TIMEOUT=1` (5 in
//! test 9), `DREAMD_LOG=off` except for `watch`, and `DREAMD_LOG_FORMAT` and
//! `DREAMD_LAUNCHER` removed. A child that runs past 15 s is killed and the
//! test fails. The fakes serve each connection on its own thread. The test
//! process's own environment is never changed. Test files do not share
//! helpers: the `Watch` guard is copied from `project_config.rs` and the MCP
//! helpers from `version_mismatch_launcher.rs`. Unix-only: the client paths
//! under test speak HTTP over a Unix socket.

#![cfg(unix)]

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The longest a child under test may run.
const CHILD_LIMIT: Duration = Duration::from_secs(15);
/// The longest `dreamd init` may run. It is setup, not under test, and a
/// loaded machine has stalled it past [`CHILD_LIMIT`].
const SETUP_LIMIT: Duration = Duration::from_secs(60);
/// How long `watch` gets to bind, and a SIGTERMed `watch` to exit.
const WATCH_TIMEOUT: Duration = Duration::from_secs(35);
const POLL: Duration = Duration::from_millis(50);
const NOT_RESPONDING: &str = "daemon: not responding";

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

    /// The test's `DREAMD_SOCK`.
    fn sock(&self) -> PathBuf {
        self.sock_dir.path().join("d.sock")
    }

    /// A `dreamd` command in `dir`, with the test's environment rules.
    fn dreamd(&self, dir: &Path, client_timeout: &str) -> Command {
        let mut cmd = Command::new(dreamd_bin());
        cmd.current_dir(dir)
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.xdg.path())
            .env("DREAMD_SOCK", self.sock())
            .env("DREAMD_CLIENT_TIMEOUT", client_timeout)
            .env("DREAMD_LOG", "off")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("DREAMD_LOG_FORMAT")
            .env_remove("DREAMD_LAUNCHER")
            .stdin(Stdio::null());
        cmd
    }

    /// Project P, scaffolded by `dreamd init` before anything listens on the
    /// socket, with three events of one cluster, so an in-process dream cycle
    /// would write.
    fn project(&self) -> PathBuf {
        let dir = self.projects.path().join("P");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"p\"\nversion = \"0.0.0\"\n",
        )
        .unwrap();
        let out = run_within(
            self.dreamd(&dir, "1").args(["init", "--quiet"]),
            self,
            SETUP_LIMIT,
        );
        assert_eq!(out.code, Some(0), "init: {}", out.describe());
        let mut log = String::new();
        for n in 1..=3 {
            log.push_str(&format!(
                "{{\"schema_version\":\"1.0.0\",\"id\":\"evt_01ARZ3NDEKTSV4RRFFQ69G5FA{n}\",\
                 \"timestamp\":\"2026-11-20T00:00:0{n}Z\",\"pain\":8.0,\"importance\":8.0,\
                 \"skill_action\":\"testing::drm121\",\"source_harness\":\"claude-code\",\
                 \"content\":\"deadline event {n}\"}}\n"
            ));
        }
        std::fs::write(dir.join(".agent/episodic/AGENT_LEARNINGS.jsonl"), log).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    /// A file inside the test's `HOME` that no other child uses.
    fn scratch_file(&self, what: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        self.home.path().join(format!(
            "{what}-{}.txt",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }
}

/// A finished child: its exit code, output and run time.
struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    took: Duration,
}

impl Ran {
    fn describe(&self) -> String {
        format!(
            "exit {:?} after {:?}\n--- stdout\n{}\n--- stderr\n{}",
            self.code, self.took, self.stdout, self.stderr
        )
    }
}

/// Run `cmd` to its end with its output in files. Past [`CHILD_LIMIT`] it is
/// killed and the test fails.
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
        took: started.elapsed(),
    }
}

/// A file's bytes, or `None` when it is absent.
fn read_optional(path: &Path) -> Option<Vec<u8>> {
    match std::fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => panic!("read {}: {e}", path.display()),
    }
}

// ── fake daemons ────────────────────────────────────────────────────────────

/// A daemon that accepts each connection on its own thread and never reads.
/// The accepted streams stay open until the fake is dropped.
struct SilentDaemon {
    _held: Receiver<UnixStream>,
}

impl SilentDaemon {
    fn start(sock: &Path) -> Self {
        let listener = UnixListener::bind(sock).expect("bind the silent daemon");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(stream);
                });
            }
        });
        Self { _held: rx }
    }
}

/// Read one request head. `false` when the stream ends first.
fn read_request_head(reader: &mut BufReader<UnixStream>) -> bool {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return false,
            Ok(_) if line == "\r\n" => return true,
            Ok(_) => {}
        }
    }
}

/// A daemon that answers every request `200` with this binary's version and
/// a no-op dream cycle's body, `delay` after it reads the head.
fn slow_daemon(sock: &Path, delay: Duration) {
    let listener = UnixListener::bind(sock).expect("bind the slow daemon");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                if !read_request_head(&mut reader) {
                    return;
                }
                std::thread::sleep(delay);
                let body = "{\"notices\":[]}";
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nx-dreamd-version: {VERSION}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
}

// ── `dreamd mcp`, copied from `version_mismatch_launcher.rs` ────────────────

/// Stdio MCP child: line-oriented JSON-RPC over piped stdin/stdout.
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

    /// The next JSON-RPC response with `id`, skipping notifications.
    fn recv_response(&self, id: u64, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                panic!("no MCP response id={id} within {timeout:?}");
            }
            let line = self
                .rx
                .recv_timeout(remaining)
                .unwrap_or_else(|e| panic!("no MCP response id={id} within {timeout:?}: {e}"));
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
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn handshake(mcp: &mut McpChild) {
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"daemon-deadline","version":"0"}}}"#;
    mcp.send(init);
    let resp = mcp.recv_response(1, CHILD_LIMIT);
    assert!(
        resp.get("result").is_some(),
        "initialize must succeed; got {resp}"
    );
    mcp.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
}

/// Call `tool` and return its JSON-RPC error and how long the answer took.
fn call_tool_error(mcp: &mut McpChild, id: u64, tool: &str, arguments: Value) -> (Value, Duration) {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    });
    let started = Instant::now();
    mcp.send(&req.to_string());
    let resp = mcp.recv_response(id, CHILD_LIMIT);
    let took = started.elapsed();
    let error = resp
        .get("error")
        .cloned()
        .unwrap_or_else(|| panic!("{tool} must answer a JSON-RPC error: {resp}"));
    (error, took)
}

// ── `dreamd watch`, copied from `project_config.rs` ─────────────────────────

/// A really-running `dreamd watch`. Its drop sends SIGCONT, then SIGKILL.
struct Watch {
    child: Child,
    console: PathBuf,
}

impl Watch {
    fn start(env: &Env, project: &Path) -> Self {
        let console = env.scratch_file("console");
        let out = std::fs::File::create(&console).expect("create console file");
        let err = out.try_clone().expect("clone console file handle");
        let child = env
            .dreamd(project, "1")
            .env_remove("DREAMD_LOG")
            .arg("watch")
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .expect("spawn dreamd watch");
        let mut watch = Self { child, console };
        let deadline = Instant::now() + WATCH_TIMEOUT;
        while !dreamd_core::server::is_daemon_socket_live(&env.sock()) {
            if let Some(status) = watch.child.try_wait().expect("try_wait") {
                panic!(
                    "dreamd watch exited ({status}) before binding; console:\n{}",
                    watch.console()
                );
            }
            assert!(
                Instant::now() < deadline,
                "dreamd watch never bound within {WATCH_TIMEOUT:?}; console:\n{}",
                watch.console()
            );
            std::thread::sleep(POLL);
        }
        watch
    }

    fn pid(&self) -> String {
        self.child.id().to_string()
    }

    fn console(&self) -> String {
        std::fs::read_to_string(&self.console).unwrap_or_default()
    }

    fn signal(&self, signal: &str) {
        let sent = Command::new("kill")
            .args([signal, &self.pid()])
            .status()
            .expect("run kill");
        assert!(sent.success(), "kill {signal} {} failed", self.pid());
    }

    /// SIGTERM, then wait for a clean exit.
    fn stop(mut self) {
        self.signal("-TERM");
        let deadline = Instant::now() + WATCH_TIMEOUT;
        let status: ExitStatus = loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "watch did not exit within {WATCH_TIMEOUT:?} of SIGTERM; console:\n{}",
                self.console()
            );
            std::thread::sleep(POLL);
        };
        assert!(
            status.success(),
            "watch must exit 0 on SIGTERM, got {status}; console:\n{}",
            self.console()
        );
    }
}

impl Drop for Watch {
    /// SIGCONT first, so a stopped `watch` is not left stopped. Only while
    /// the child has not been reaped, so the pid is still ours.
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = Command::new("kill")
                .args(["-CONT", &self.child.id().to_string()])
                .status();
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// `dreamd status` in `dir`.
fn status(env: &Env, dir: &Path) -> Ran {
    run_bounded(env.dreamd(dir, "1").arg("status"), env)
}

// ── tests ───────────────────────────────────────────────────────────────────

/// DRM-121 test 7 (LP-CONFIG-04): a daemon that accepts and never answers
/// fails `dream`, both MCP tools and `status` within their deadlines, and
/// nothing runs in-process beside it.
#[test]
fn a_daemon_that_never_answers_fails_each_client() {
    let env = Env::new();
    let p = env.project();
    let sock = env.sock();
    let sock_text = sock.display().to_string();
    let log_path = p.join(".agent/episodic/AGENT_LEARNINGS.jsonl");
    let lessons_path = p.join(".agent/semantic/LESSONS.md");
    let log_before = read_optional(&log_path);
    let lessons_before = read_optional(&lessons_path);
    let _fake = SilentDaemon::start(&sock);

    let dream = run_bounded(env.dreamd(&p, "1").args(["dream", "--no-llm"]), &env);
    assert_eq!(dream.code, Some(1), "dream: {}", dream.describe());
    assert!(
        dream.took < Duration::from_secs(5),
        "dream: {}",
        dream.describe()
    );
    assert!(
        dream.stderr.contains(&sock_text) && dream.stderr.contains("did not answer"),
        "dream must name the socket and say it did not answer: {}",
        dream.describe()
    );
    assert_eq!(
        read_optional(&log_path),
        log_before,
        "the log changed: a cycle ran in-process"
    );
    assert_eq!(
        read_optional(&lessons_path),
        lessons_before,
        "LESSONS.md changed: a cycle ran in-process"
    );

    let mut mcp = McpChild::from_command(env.dreamd(&p, "1").arg("mcp"));
    handshake(&mut mcp);
    let (error, took) = call_tool_error(
        &mut mcp,
        2,
        "append_node",
        serde_json::json!({
            "content": "a learning the daemon never answers",
            "source_harness": "claude-code",
            "skill_action": "testing::drm121",
            "pain": 5,
            "importance": 5,
        }),
    );
    assert_eq!(error["code"], -32603, "{error}");
    let message = error["message"].as_str().unwrap_or_default();
    // DRM-122: the version probe meets the silent daemon first, so the
    // append is never sent, and its error does not say it may have been
    // written. A timed-out append that was sent is `memory_store`'s test.
    assert!(
        message.contains(&sock_text) && message.contains("did not answer"),
        "{error}"
    );
    assert!(!message.contains("may have been written"), "{error}");
    assert!(took < Duration::from_secs(5), "append_node took {took:?}");

    let (error, took) = call_tool_error(
        &mut mcp,
        3,
        "search_nodes",
        serde_json::json!({ "query": "deadline", "k": 5 }),
    );
    assert_eq!(error["code"], -32603, "{error}");
    let message = error["message"].as_str().unwrap_or_default();
    assert!(message.contains(&sock_text), "{error}");
    assert!(!message.contains("may have been written"), "{error}");
    assert!(took < Duration::from_secs(5), "search_nodes took {took:?}");
    mcp.close();

    let status = status(&env, &p);
    assert_eq!(status.code, Some(1), "status: {}", status.describe());
    assert!(
        status.stdout.contains(NOT_RESPONDING),
        "status: {}",
        status.describe()
    );
}

/// DRM-121 test 8: a `watch` stopped with SIGSTOP is `not responding`, and
/// `running` again after SIGCONT.
#[test]
fn a_stopped_watch_is_not_responding() {
    let env = Env::new();
    let p = env.project();
    let watch = Watch::start(&env, &p);

    let running = status(&env, &p);
    assert_eq!(running.code, Some(0), "status: {}", running.describe());
    assert!(
        running.stdout.contains("daemon: running"),
        "status: {}",
        running.describe()
    );

    watch.signal("-STOP");
    let stopped = status(&env, &p);
    assert_eq!(stopped.code, Some(1), "status: {}", stopped.describe());
    assert!(
        stopped.stdout.contains(NOT_RESPONDING),
        "status: {}",
        stopped.describe()
    );
    assert!(
        stopped.took < Duration::from_secs(5),
        "status: {}",
        stopped.describe()
    );

    watch.signal("-CONT");
    let again = status(&env, &p);
    assert_eq!(again.code, Some(0), "status: {}", again.describe());
    assert!(
        again.stdout.contains("daemon: running"),
        "status: {}",
        again.describe()
    );
    watch.stop();
}

/// DRM-121 test 9: a dream cycle the daemon answers after 2 s, inside a 5 s
/// deadline, succeeds.
#[test]
fn a_slow_dream_inside_the_deadline_succeeds() {
    let env = Env::new();
    let p = env.project();
    slow_daemon(&env.sock(), Duration::from_secs(2));

    let dream = run_bounded(env.dreamd(&p, "5").args(["dream", "--no-llm"]), &env);
    assert_eq!(dream.code, Some(0), "dream: {}", dream.describe());
    assert!(
        dream.stdout.contains("dream cycle complete (via daemon)"),
        "dream: {}",
        dream.describe()
    );
}
