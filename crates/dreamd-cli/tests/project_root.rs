//! DRM-102: `dreamd mcp --project-root` names a store folder, and
//! `X-Agent-Root` is the project's absolute path sent as UTF-8 bytes.
//!
//! Tests 10–12 drive the real binary. Every child gets `HOME` set to a
//! tempdir, `DREAMD_LOG=off`, `GIT_CONFIG_NOSYSTEM=1` and
//! `GIT_CONFIG_GLOBAL=/dev/null`, and has `DREAMD_SOCK`, `DREAMD_LAUNCHER`
//! and `DREAMD_LOG_FORMAT` removed unless a test sets one. A test that needs
//! a socket names its own with `DREAMD_SOCK`. The test process's own
//! environment is never changed, and no test reaches the user's daemon. Test
//! files do not share helpers: the MCP helpers are copied from
//! `search_nodes_id.rs` and the `Watch` guard from `watch_sock.rs`. Every
//! wait is bounded. Unix-only: `DREAMD_SOCK` is a Unix socket.

#![cfg(unix)]

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);

fn dreamd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dreamd")
}

/// A `dreamd` command in `dir`, with the test's environment rules.
fn dreamd(dir: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(dreamd_bin());
    cmd.current_dir(dir)
        .env("HOME", home)
        .env("DREAMD_LOG", "off")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("DREAMD_SOCK")
        .env_remove("DREAMD_LAUNCHER")
        .env_remove("DREAMD_LOG_FORMAT");
    cmd
}

/// Run `dreamd args` in `dir` with stdin null, extra `env` applied last.
fn run(dir: &Path, home: &Path, args: &[&str], env: &[(&str, &Path)]) -> Output {
    let mut cmd = dreamd(dir, home);
    cmd.args(args).stdin(Stdio::null());
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.output()
        .unwrap_or_else(|e| panic!("run dreamd {args:?}: {e}"))
}

fn describe(out: &Output) -> String {
    format!(
        "exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// `dreamd init --quiet` in `project`, which gets a `.git` root sentinel.
fn init_project(project: &Path, home: &Path) {
    std::fs::create_dir_all(project.join(".git")).expect("write .git root sentinel");
    let out = run(project, home, &["init", "--quiet"], &[]);
    assert_eq!(out.status.code(), Some(0), "init: {}", describe(&out));
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

fn handshake(mcp: &mut McpChild) {
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"project-root","version":"0"}}}"#;
    let initialized = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    mcp.send(init);
    let resp = mcp.recv_response(1, TIMEOUT);
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

fn spawn_mcp(cmd: Command) -> McpChild {
    let mut mcp = McpChild::from_command(cmd);
    handshake(&mut mcp);
    mcp
}

/// Call `tool` with `arguments` and return the JSON in `result.content[0].text`.
fn call_tool(mcp: &mut McpChild, id: u64, tool: &str, arguments: Value) -> Value {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    });
    mcp.send(&req.to_string());
    let resp = mcp.recv_response(id, TIMEOUT);
    assert!(resp.get("error").is_none(), "{tool}: error: {resp}");
    assert_ne!(
        resp["result"]["isError"],
        Value::Bool(true),
        "{tool}: tool error: {resp}"
    );
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{tool}: no text content: {resp}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{tool}: text is not JSON: {e}; {text}"))
}

// ── `dreamd watch`, copied from `watch_sock.rs` ─────────────────────────────

/// A really-running `dreamd watch`, killed on drop.
struct Watch {
    child: Child,
    console: PathBuf,
}

impl Watch {
    /// Spawn `dreamd watch` in `project` with both output streams in one file
    /// inside `home`, and `DREAMD_SOCK` set to `sock`.
    fn spawn(project: &Path, home: &Path, sock: &Path) -> Self {
        let console = home.join("watch.console");
        let out = std::fs::File::create(&console).expect("create console file");
        let err = out.try_clone().expect("clone console file handle");
        let child = dreamd(project, home)
            .env("DREAMD_SOCK", sock)
            .arg("watch")
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .expect("spawn dreamd watch");
        Self { child, console }
    }

    fn console(&self) -> String {
        std::fs::read_to_string(&self.console).unwrap_or_default()
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

// ── A fake daemon (test 11) ─────────────────────────────────────────────────

/// The value bytes of header `name` in a raw request head, or `None`.
fn header_value(head: &[u8], name: &str) -> Option<Vec<u8>> {
    head.split(|&b| b == b'\n').find_map(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let colon = line.iter().position(|&b| b == b':')?;
        if !line[..colon].eq_ignore_ascii_case(name.as_bytes()) {
            return None;
        }
        let value = &line[colon + 1..];
        let start = value
            .iter()
            .position(|&b| b != b' ' && b != b'\t')
            .unwrap_or(value.len());
        Some(value[start..].to_vec())
    })
}

/// Accept connections on `listener` until the test ends. A connection that
/// reaches EOF before a request head is `dreamd mcp`'s reachability probe and
/// is skipped. Each request's `x-agent-root` bytes go to the returned
/// channel, and the answer is an empty recall from a daemon of this version.
fn fake_daemon(listener: UnixListener) -> Receiver<Option<Vec<u8>>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                return;
            };
            stream
                .set_read_timeout(Some(TIMEOUT))
                .expect("set read timeout");
            let mut head = Vec::new();
            let mut buf = [0u8; 4096];
            let end = loop {
                if let Some(end) = head.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(end);
                }
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break None,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            };
            let Some(end) = end else {
                continue;
            };
            let body = br#"{"results":[]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nx-dreamd-version: {}\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n",
                env!("CARGO_PKG_VERSION"),
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(body);
            if tx.send(header_value(&head[..end], "x-agent-root")).is_err() {
                return;
            }
        }
    });
    rx
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// DRM-102 test 10 (LP-MCP-09, D4): `--project-root` that names a folder
/// with no store, a path that does not exist, or the home whose `.agent/` is
/// the daemon home exits 2 and says why, with `DREAMD_SOCK` unset and with
/// it naming a bound listener. The check runs before any socket probe, so
/// nothing connects to the listener.
#[test]
fn project_root_flag_exits_two_in_both_modes() {
    let home = tempfile::tempdir().expect("home tempdir");
    let project = tempfile::tempdir().expect("project tempdir");
    init_project(project.path(), home.path());
    let src = project.path().join("src");
    std::fs::create_dir(&src).unwrap();
    assert!(
        home.path().join(".agent").is_dir(),
        "init made the daemon home"
    );
    let sock_dir = tempfile::tempdir().expect("socket tempdir");
    let sock = sock_dir.path().join("d.sock");
    let listener = UnixListener::bind(&sock).expect("bind listener");
    listener.set_nonblocking(true).expect("non-blocking");

    // Each case's own sentence. Test 8 pins the folder a relative flag names.
    let home_str = home.path().to_str().expect("a UTF-8 tempdir");
    let cases: [(&Path, &str, String); 3] = [
        (
            &src,
            ".",
            "--project-root .: no .agent/ store in ".to_string(),
        ),
        (
            project.path(),
            "/nonexistent/project",
            "--project-root /nonexistent/project: no .agent/ store in /nonexistent/project."
                .to_string(),
        ),
        (
            project.path(),
            home_str,
            format!(
                "--project-root {home_str}: {}/.agent is the daemon home",
                home.path().display()
            ),
        ),
    ];
    for (cwd, flag, want) in &cases {
        for env in [&[][..], &[("DREAMD_SOCK", sock.as_path())][..]] {
            let out = run(cwd, home.path(), &["mcp", "--project-root", *flag], env);
            let case = format!("--project-root {flag} from {}, env {env:?}", cwd.display());
            assert_eq!(out.status.code(), Some(2), "{case}: {}", describe(&out));
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                stderr.contains(&format!("dreamd mcp: {want}")),
                "{case}: {}",
                describe(&out)
            );
        }
    }

    match listener.accept() {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("nothing may connect to the daemon socket: {other:?}"),
    }
}

/// DRM-102 test 11: a `--project-root` reached through a symlink and a `..`
/// is sent to the daemon as the project's canonical absolute path, as UTF-8
/// bytes, from a cwd that is not the project.
#[test]
fn mcp_sends_the_canonical_absolute_root() {
    let home = tempfile::tempdir().expect("home tempdir");
    let tmp = tempfile::tempdir().expect("project tempdir");
    let elsewhere = tempfile::tempdir().expect("cwd tempdir");
    let project = tmp.path().join("P");
    std::fs::create_dir(&project).unwrap();
    init_project(&project, home.path());
    std::fs::create_dir(project.join("sub")).unwrap();
    let link = tmp.path().join("L");
    std::os::unix::fs::symlink(&project, &link).expect("symlink L -> P");
    let flag = link.join("sub").join("..");

    let sock_dir = tempfile::tempdir().expect("socket tempdir");
    let sock = sock_dir.path().join("d.sock");
    let requests = fake_daemon(UnixListener::bind(&sock).expect("bind fake daemon"));

    let mut cmd = dreamd(elsewhere.path(), home.path());
    cmd.env("DREAMD_SOCK", &sock)
        .arg("mcp")
        .arg("--project-root")
        .arg(&flag);
    let mut mcp = spawn_mcp(cmd);
    let body = call_tool(
        &mut mcp,
        2,
        "search_nodes",
        serde_json::json!({ "query": "drm102 anything", "k": 5 }),
    );
    assert_eq!(body["results"], Value::Array(Vec::new()), "{body}");
    mcp.close();

    let sent = requests
        .recv_timeout(TIMEOUT)
        .expect("the fake daemon got a request")
        .expect("the request carried x-agent-root");
    let canonical = std::fs::canonicalize(&project).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&sent),
        canonical.to_str().expect("a UTF-8 tempdir"),
        "X-Agent-Root for --project-root {}",
        flag.display()
    );
    assert_eq!(sent, canonical.to_str().unwrap().as_bytes());
}

/// DRM-102 test 12 (LP-MCP-02): a project whose path holds `é` works in
/// daemon mode, end to end with a real `dreamd watch`: `append_node`, then
/// `search_nodes` finds the event, and `dreamd dream --no-llm` (the dream
/// proxy) exits 0 with no 400.
#[test]
fn non_ascii_project_works_in_daemon_mode() {
    const WORD: &str = "xylophonic";
    let content = format!("drm102 a learning from an accented project path, {WORD}");

    let home = tempfile::tempdir().expect("home tempdir");
    let tmp = tempfile::tempdir().expect("project tempdir");
    let project = tmp.path().join("été").join("proj");
    std::fs::create_dir_all(&project).unwrap();
    init_project(&project, home.path());
    let sock_dir = tempfile::tempdir().expect("socket tempdir");
    let sock = sock_dir.path().join("d.sock");

    let mut watch = Watch::spawn(&project, home.path(), &sock);
    wait_for_live_socket(&mut watch, &sock);

    let mut cmd = dreamd(&project, home.path());
    cmd.env("DREAMD_SOCK", &sock).arg("mcp");
    let mut mcp = spawn_mcp(cmd);
    let appended = call_tool(
        &mut mcp,
        2,
        "append_node",
        serde_json::json!({
            "content": content,
            "source_harness": "claude-code",
            "skill_action": "testing::drm102",
        }),
    );
    let id = appended["id"]
        .as_str()
        .unwrap_or_else(|| panic!("append_node: no id: {appended}"))
        .to_owned();
    // The daemon commits the index on its own cadence, so one search can
    // answer from the last commit. This test checks the path, not that
    // bound: retry in the same session, at most 25 tries, 200 ms apart.
    let mut hits = Vec::new();
    for attempt in 0..25u64 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(200));
        }
        let body = call_tool(
            &mut mcp,
            3 + attempt,
            "search_nodes",
            serde_json::json!({ "query": WORD, "k": 5 }),
        );
        hits = body["results"]
            .as_array()
            .unwrap_or_else(|| panic!("search_nodes: no results array: {body}"))
            .clone();
        if hits.iter().any(|h| h["id"] == Value::String(id.clone())) {
            break;
        }
    }
    assert!(
        hits.iter().any(|h| h["id"] == Value::String(id.clone())),
        "search_nodes must find {id}: {hits:?}; watch console:\n{}",
        watch.console()
    );
    mcp.close();

    let dream = run(
        &project,
        home.path(),
        &["dream", "--no-llm"],
        &[("DREAMD_SOCK", sock.as_path())],
    );
    assert_eq!(dream.status.code(), Some(0), "dream: {}", describe(&dream));
    for (stream, bytes) in [("stdout", &dream.stdout), ("stderr", &dream.stderr)] {
        assert!(
            !String::from_utf8_lossy(bytes).contains("400"),
            "dream {stream} names a 400: {}",
            describe(&dream)
        );
    }
}
