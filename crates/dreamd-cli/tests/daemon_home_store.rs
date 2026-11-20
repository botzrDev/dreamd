//! DRM-101: the daemon home (`~/.agent`) is never a project store.
//!
//! Tests 9–18 drive the real binary. Every child gets `HOME` set to a tempdir
//! that plays the home, `DREAMD_LOG=off`, `GIT_CONFIG_NOSYSTEM=1` and
//! `GIT_CONFIG_GLOBAL=/dev/null`, and has `DREAMD_SOCK`, `DREAMD_LAUNCHER` and
//! `DREAMD_LOG_FORMAT` removed unless a test sets them. The test process's own
//! environment is never changed, and no test reaches the user's daemon.
//! `tree` maps every path under a folder to its bytes, so a test can show a
//! command wrote nothing. The MCP helpers are copied from `search_nodes_id.rs`
//! (test files do not share helpers). Unix-only: the MCP not-set-up answer is
//! the Unix `run_mcp_server`'s.

#![cfg(unix)]

use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(30);
const NOT_SET_UP: &str = "this folder is not set up for dreamd: there is no .agent/ store in";

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

fn run(dir: &Path, home: &Path, args: &[&str]) -> Output {
    dreamd(dir, home)
        .args(args)
        .output()
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

/// Every path under `dir`, relative to it, with a file's bytes (`None` for a
/// directory, the link target for a symlink).
fn tree(dir: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn walk(base: &Path, dir: &Path, map: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.expect("dir entry").path();
            let rel = path.strip_prefix(base).unwrap().to_path_buf();
            let meta = std::fs::symlink_metadata(&path).expect("metadata");
            if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&path).expect("read_link");
                map.insert(rel, Some(target.into_os_string().into_encoded_bytes()));
            } else if meta.is_dir() {
                map.insert(rel, None);
                walk(base, &path, map);
            } else {
                map.insert(rel, Some(std::fs::read(&path).expect("read file")));
            }
        }
    }
    let mut map = BTreeMap::new();
    walk(dir, dir, &mut map);
    map
}

/// A home whose daemon home holds only `registry.toml`.
fn home_with_registry() -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("home tempdir");
    std::fs::create_dir(home.path().join(".agent")).unwrap();
    std::fs::write(home.path().join(".agent/registry.toml"), "projects = []\n").unwrap();
    home
}

/// One log line `episodic::read_all` parses, with a distinct id.
fn record(suffix: char) -> String {
    format!(
        "{{\"schema_version\":\"1.0.0\",\"id\":\"evt_01ARZ3NDEKTSV4RRFFQ69G5FA{suffix}\",\
         \"timestamp\":\"2026-11-19T00:00:00Z\",\"pain\":5.0,\"importance\":5.0,\
         \"pinned\":false,\"skill_action\":\"rust::stray\",\"source_harness\":\"test\",\
         \"content\":\"stray {suffix}\"}}\n"
    )
}

fn move_line(daemon_home: &Path, events: usize) -> String {
    format!(
        "daemon_home_store: {} holds a project store (events: {events})  [WARNING: dreamd no \
         longer reads a store in the daemon home; move it into a project \
         (docs/troubleshooting.md, \"A project store in the daemon home\")]",
        daemon_home.display()
    )
}

fn remove_line(daemon_home: &Path) -> String {
    format!(
        "daemon_home_store: {h} holds a project store (events: 0)  [WARNING: dreamd no longer \
         reads a store in the daemon home; this one holds no events, so remove episodic/, \
         semantic/, personal/, working/ and .dreamd/ from {h}]",
        h = daemon_home.display()
    )
}

/// `$HOME/proj`, scaffolded by `dreamd init` and registered.
fn init_project(home: &Path) -> PathBuf {
    let proj = home.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(
        proj.join("Cargo.toml"),
        "[package]\nname = \"drm101\"\nversion = \"0.0.0\"\n",
    )
    .unwrap();
    let init = run(&proj, home, &["init"]);
    assert_eq!(init.status.code(), Some(0), "init: {}", describe(&init));
    proj
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
        std::thread::sleep(Duration::from_millis(50)); // poll process exit
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

fn handshake(mcp: &mut McpChild, init_id: u64, timeout: Duration) {
    let init = format!(
        r#"{{"jsonrpc":"2.0","id":{init_id},"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"daemon-home-store","version":"0"}}}}}}"#
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

fn spawn_mcp(cmd: Command) -> McpChild {
    let mut mcp = McpChild::from_command(cmd);
    handshake(&mut mcp, 1, TIMEOUT);
    mcp
}

/// Call `tool` and return its JSON-RPC error object; panic on a result.
fn tool_error(mcp: &mut McpChild, id: u64, tool: &str, timeout: Duration) -> Value {
    let arguments = match tool {
        "append_node" => serde_json::json!({
            "content": "drm101 a learning that must not be written",
            "source_harness": "claude-code",
            "skill_action": "testing::drm101",
        }),
        _ => serde_json::json!({ "query": "drm101 anything", "k": 5 }),
    };
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    });
    mcp.send(&req.to_string());
    let resp = mcp.recv_response(id, timeout);
    resp.get("error")
        .unwrap_or_else(|| panic!("{tool}: expected an error, got {resp}"))
        .clone()
}

/// Both tools, `append_node` first, answer `-32600` with the not-set-up
/// message; returns the two messages.
fn both_not_set_up(mcp: &mut McpChild, timeout: Duration) -> Vec<String> {
    ["append_node", "search_nodes"]
        .iter()
        .zip(2u64..)
        .map(|(tool, id)| {
            let err = tool_error(mcp, id, tool, timeout);
            assert_eq!(err["code"], Value::from(-32600), "{tool}: {err}");
            let message = err["message"].as_str().expect("message").to_string();
            assert!(message.starts_with(NOT_SET_UP), "{tool}: {message}");
            message
        })
        .collect()
}

// ── MCP (tests 9–12) ────────────────────────────────────────────────────────

/// DRM-101 test 9 (LP-INSTALL-02): `dreamd mcp` in a folder below `$HOME`
/// with no store answers "not set up" from both tools and writes nothing in
/// `$HOME`, daemon home included.
#[test]
fn mcp_below_home_with_no_store_writes_nothing() {
    let home = home_with_registry();
    let folder = home.path().join("r02");
    std::fs::create_dir(&folder).unwrap();
    let before = tree(home.path());

    let mut cmd = dreamd(&folder, home.path());
    cmd.arg("mcp");
    let mut mcp = spawn_mcp(cmd);
    for message in both_not_set_up(&mut mcp, TIMEOUT) {
        assert!(
            message.contains(&format!(
                "store in {} or a folder above it",
                folder.display()
            )),
            "{message}"
        );
        assert!(
            message.contains("Run `dreamd setup` in the project folder"),
            "{message}"
        );
    }
    mcp.close();

    assert_eq!(tree(home.path()), before, "$HOME must be unchanged");
}

/// DRM-101 test 10: outside `$HOME`, under the npm shim, both tools name
/// `npx -y dreamd-mcp setup`, and no `.agent/` is created.
#[test]
fn mcp_with_no_store_names_the_launchers_setup_command() {
    let home = home_with_registry();
    let folder = tempfile::tempdir().expect("folder tempdir");

    let mut cmd = dreamd(folder.path(), home.path());
    cmd.env("DREAMD_LAUNCHER", "npx").arg("mcp");
    let mut mcp = spawn_mcp(cmd);
    for message in both_not_set_up(&mut mcp, TIMEOUT) {
        assert!(
            message.contains("Run `npx -y dreamd-mcp setup` in the project folder"),
            "{message}"
        );
    }
    mcp.close();

    assert!(!folder.path().join(".agent").exists(), "no store created");
}

/// DRM-101 test 11 (LP-MCP-09, D4): with cwd `$HOME`, a literal
/// `--project-root "~/realproject"` is a relative path, and no shell expands
/// its `~`. Under DRM-102 the flag must name a store folder, so `mcp` exits
/// 2, says the `~` is not expanded, and `$HOME` is unchanged.
#[test]
fn mcp_tilde_project_root_does_not_reach_the_daemon_home() {
    let home = home_with_registry();
    std::fs::create_dir(home.path().join("realproject")).unwrap();
    let before = tree(home.path());

    let out = dreamd(home.path(), home.path())
        .args(["mcp", "--project-root", "~/realproject"])
        .stdin(Stdio::null())
        .output()
        .expect("run dreamd mcp");
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--project-root ~/realproject: no .agent/ store in"),
        "{stderr}"
    );
    assert!(
        stderr.contains("A leading ~ is not expanded here"),
        "{stderr}"
    );

    assert_eq!(tree(home.path()), before, "$HOME must be unchanged");
}

/// DRM-101 test 12 (D3): in daemon mode with no store, nothing connects to
/// the socket. `DREAMD_SOCK` names a bound listener that never answers; both
/// tools still answer "not set up" in time, and nothing is waiting to be
/// accepted.
#[test]
fn daemon_mode_with_no_store_never_connects() {
    let home = home_with_registry();
    let folder = tempfile::tempdir().expect("folder tempdir");
    let sock_dir = tempfile::tempdir().expect("socket tempdir");
    let sock = sock_dir.path().join("d.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind listener");
    listener.set_nonblocking(true).expect("non-blocking");

    let mut cmd = dreamd(folder.path(), home.path());
    cmd.env("DREAMD_SOCK", &sock).arg("mcp");
    let mut mcp = spawn_mcp(cmd);
    both_not_set_up(&mut mcp, Duration::from_secs(10));
    mcp.close();

    match listener.accept() {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("nothing may connect to the daemon socket: {other:?}"),
    }
}

// ── init and setup (tests 13–15) ────────────────────────────────────────────

/// DRM-101 test 13 (LP-WRITE-14): in a git repo at `$HOME` (dotfiles), with
/// and without a daemon home yet, `dreamd init` from `$HOME` and from a
/// folder inside it exits 2, names the daemon home, and writes nothing.
#[test]
fn init_in_home_refuses_and_writes_nothing() {
    for with_daemon_home in [false, true] {
        for sub in ["", "notes"] {
            let home = tempfile::tempdir().expect("home tempdir");
            std::fs::create_dir(home.path().join(".git")).unwrap();
            if with_daemon_home {
                std::fs::create_dir(home.path().join(".agent")).unwrap();
                std::fs::write(home.path().join(".agent/registry.toml"), "projects = []\n")
                    .unwrap();
            }
            let dir = home.path().join(sub);
            std::fs::create_dir_all(&dir).unwrap();
            let before = tree(home.path());

            let out = run(&dir, home.path(), &["init"]);
            let case = format!("daemon home {with_daemon_home}, from {sub:?}");
            assert_eq!(out.status.code(), Some(2), "{case}: {}", describe(&out));
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                stderr.contains(&format!(
                    "so its store would be the daemon home {}",
                    home.path().join(".agent").display()
                )),
                "{case}: {stderr}"
            );
            assert_eq!(tree(home.path()), before, "{case}: $HOME must be unchanged");
        }
    }
}

/// DRM-101 test 14 (LP-INSTALL-06): setup in a git repo at `$HOME` refuses
/// before its plan, with and without `--dry-run`, and writes nothing.
#[test]
fn setup_in_home_refuses_and_writes_nothing() {
    let home = home_with_registry();
    std::fs::create_dir(home.path().join(".git")).unwrap();
    let before = tree(home.path());

    for args in [
        ["setup", "--yes", "--no-start-watch"],
        ["setup", "--yes", "--dry-run"],
    ] {
        let out = run(home.path(), home.path(), &args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", describe(&out));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("would be the daemon home"),
            "{args:?}: {stderr}"
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(!stdout.contains("project root:"), "{args:?}: {stdout}");
        assert_eq!(
            tree(home.path()),
            before,
            "{args:?}: $HOME must be unchanged"
        );
    }
}

/// DRM-101 test 15: a project below `$HOME` still initializes, registers,
/// and leaves no store in the daemon home.
#[test]
fn init_below_home_still_works() {
    let home = tempfile::tempdir().expect("home tempdir");
    let proj = init_project(home.path());
    assert!(proj.join(".agent/episodic").is_dir(), "scaffolded");
    let registry =
        std::fs::read_to_string(home.path().join(".agent/registry.toml")).expect("registry");
    let canonical = std::fs::canonicalize(&proj).unwrap();
    assert!(
        registry.contains(&canonical.display().to_string()),
        "registry names the project: {registry}"
    );
    assert!(!home.path().join(".agent/episodic").exists());
}

// ── doctor and status (tests 16–18) ─────────────────────────────────────────

/// DRM-101 test 16: a stray store with 2 events in the daemon home. `status`
/// and `doctor` print the move line, doctor fails its run, and doctor from a
/// folder with no store exits 2 and prints the line after its no-store error.
#[test]
fn status_and_doctor_report_a_store_in_the_daemon_home() {
    let home = tempfile::tempdir().expect("home tempdir");
    let proj = init_project(home.path());
    let daemon_home = home.path().join(".agent");
    std::fs::create_dir(daemon_home.join("episodic")).unwrap();
    std::fs::write(
        daemon_home.join("episodic/AGENT_LEARNINGS.jsonl"),
        format!("{}{}", record('V'), record('W')),
    )
    .unwrap();
    let line = move_line(&daemon_home, 2);

    let status = run(&proj, home.path(), &["status"]);
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.lines().any(|l| l == line),
        "status: {}",
        describe(&status)
    );

    let doctor = run(&proj, home.path(), &["doctor"]);
    assert_eq!(
        doctor.status.code(),
        Some(1),
        "doctor: {}",
        describe(&doctor)
    );
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    assert!(
        stdout.lines().any(|l| l == line),
        "doctor: {}",
        describe(&doctor)
    );

    let empty = home.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    let doctor = run(&empty, home.path(), &["doctor"]);
    assert_eq!(doctor.status.code(), Some(2), "{}", describe(&doctor));
    let stderr = String::from_utf8_lossy(&doctor.stderr);
    let no_store = stderr
        .find("no .agent/ store found in")
        .unwrap_or_else(|| panic!("no-store error: {stderr}"));
    let at = stderr
        .find(&line)
        .unwrap_or_else(|| panic!("the line on stderr: {stderr}"));
    assert!(no_store < at, "the line comes after the error: {stderr}");
}

/// DRM-101 test 17: an empty stray log says `events: 0` and remove.
#[test]
fn an_empty_store_in_the_daemon_home_says_remove() {
    let home = tempfile::tempdir().expect("home tempdir");
    let proj = init_project(home.path());
    let daemon_home = home.path().join(".agent");
    std::fs::create_dir(daemon_home.join("episodic")).unwrap();
    std::fs::write(daemon_home.join("episodic/AGENT_LEARNINGS.jsonl"), "").unwrap();

    let status = run(&proj, home.path(), &["status"]);
    let stdout = String::from_utf8_lossy(&status.stdout);
    let line = remove_line(&daemon_home);
    assert!(
        stdout.lines().any(|l| l == line),
        "status: {}",
        describe(&status)
    );
}

/// DRM-101 test 18: with no store in the daemon home, neither `status` nor
/// `doctor` prints `daemon_home_store`.
#[test]
fn no_store_in_the_daemon_home_prints_no_line() {
    let home = tempfile::tempdir().expect("home tempdir");
    let proj = init_project(home.path());
    for args in [["status"], ["doctor"]] {
        let out = run(&proj, home.path(), &args);
        let all = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !all.contains("daemon_home_store"),
            "{args:?}: {}",
            describe(&out)
        );
    }
}
