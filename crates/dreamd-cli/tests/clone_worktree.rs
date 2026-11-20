//! DRM-107: with a daemon running, a fresh clone or worktree is refused with
//! a message that names its folder and the `init` command, `init` there
//! registers the store and re-creates the folders git does not carry, and
//! the next call works. `init` on an existing store creates folders only, and
//! refuses a scaffold path that is not a directory.
//!
//! The tests drive the real binary and real git. Every child gets its own
//! `HOME`, `XDG_CONFIG_HOME` (an empty tempdir) and `DREAMD_SOCK`, plus
//! `DREAMD_LOG=off`, `GIT_CONFIG_NOSYSTEM=1` and `GIT_CONFIG_GLOBAL=/dev/null`,
//! with `DREAMD_LAUNCHER` and `DREAMD_LOG_FORMAT` removed. The test process's
//! own environment is never changed, and no test reaches the user's daemon.
//! Test files do not share helpers: these are copied from `dedup_key.rs`.
//! Every wait is bounded. Unix-only: `DREAMD_SOCK` is a Unix socket.

#![cfg(unix)]

use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);
const COMMITTED: &str = "committed fact about tokio";
const REREGISTER: &str = "dreamd: registered existing .agent/ in ~/.agent/registry.toml";
/// What a clone or a worktree of a store with no dream cycle lacks: git keeps
/// no empty `semantic/` or `personal/`, and ignores `.dreamd/`.
const CLONE_RECREATED: &str =
    "dreamd: re-created missing .agent/ folders: semantic/, personal/, .dreamd/";

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

    /// `git args` in `dir`, with the test's `HOME` and no system or global
    /// git config. Panics unless it exits 0; returns stdout.
    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "user.name=dreamd-test",
                "-c",
                "user.email=dreamd-test@example.invalid",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.xdg.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|e| panic!("run git {args:?}: {e}"));
        assert!(out.status.success(), "git {args:?}: {}", describe(&out));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// `<projects>/<name>`, an empty folder, canonicalized.
    fn dir(&self, name: &str) -> PathBuf {
        let dir = self.projects.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
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
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"clone-worktree","version":"0"}}}"#;
        self.send(init);
        let resp = self.recv_response(1);
        assert!(
            resp.get("result").is_some() && resp.get("error").is_none(),
            "initialize must succeed; got {resp}"
        );
        self.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    }

    /// `tools/call` of `tool`; the whole JSON-RPC response.
    fn request(&mut self, tool: &str, arguments: &Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments },
        });
        self.send(&req.to_string());
        self.recv_response(id)
    }

    /// `tools/call` of `tool`; the result's text parsed as JSON. Panics on a
    /// JSON-RPC or tool error.
    fn call(&mut self, tool: &str, arguments: &Value) -> Value {
        let resp = self.request(tool, arguments);
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

/// `append_node` arguments for `content`.
fn args(content: &str) -> Value {
    serde_json::json!({
        "content": content,
        "source_harness": "claude-code",
        "skill_action": "testing::drm107",
    })
}

/// The §3.1 refusal for the store in `root`, with `dreamd init`.
fn refusal(root: &Path) -> String {
    let root = root.display();
    format!(
        "the store in {root} is not registered with the running daemon. Run `dreamd init` in \
         {root} once: it registers the existing store without changing its files (a clone or \
         a worktree is not registered until then), and the next call works without a restart."
    )
}

/// `search_nodes` for `query` until a hit's content is `content`, at most 10
/// tries 200 ms apart (a search waits at most 1 s for the index to commit an
/// append, so under load one try can answer from the last commit).
fn find(mcp: &mut McpChild, query: &str, content: &str) {
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
        if hits.iter().any(|h| h["content"] == content) {
            return;
        }
    }
    panic!("search_nodes {query:?} must find {content:?}: {hits:?}")
}

/// Every file under `dir`, by path relative to it, with its bytes.
fn files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                out.insert(path.strip_prefix(base).unwrap().to_path_buf(), bytes);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// The fixture: a git project `P` with one in-process `append_node`, its
/// `.agent/` committed without `.dreamd/`, and a `watch` in `P`.
fn committed_project(env: &Env) -> (PathBuf, Watch) {
    let p = env.dir("P");
    env.git(&p, &["init", "-q"]);
    let out = env.run(&p, &["init", "--quiet"]);
    assert_eq!(out.status.code(), Some(0), "init P: {}", describe(&out));
    // In process: nothing listens on this test's socket yet.
    let mut mcp = McpChild::start(env, &p);
    mcp.call("append_node", &args(COMMITTED));
    mcp.close();
    env.git(&p, &["add", ".agent", ".gitignore"]);
    env.git(&p, &["commit", "-q", "-m", "memory"]);
    let tracked = env.git(&p, &["ls-files"]);
    assert!(
        tracked.contains(".agent/episodic/AGENT_LEARNINGS.jsonl"),
        "the log is committed: {tracked}"
    );
    assert!(
        !tracked.contains(".dreamd/"),
        "nothing under .dreamd/ is committed: {tracked}"
    );
    let watch = Watch::start(env, &p);
    (p, watch)
}

/// Tests 2 and 3 in `checkout`, a clone or worktree of `p` with a daemon
/// running: refused precisely, then `init`, then the same MCP server works.
fn refused_then_works(env: &Env, p: &Path, checkout: &Path, what: &str) {
    let p_log = read_log(p).expect("P's log");
    let mut mcp = McpChild::start(env, checkout);
    let resp = mcp.request(
        "search_nodes",
        &serde_json::json!({ "query": "tokio", "k": 5 }),
    );
    assert_eq!(
        resp["error"]["code"], -32600,
        "{what}: an unregistered store is -32600: {resp}"
    );
    assert_eq!(
        resp["error"]["message"],
        refusal(checkout).as_str(),
        "{what}: the refusal names the folder and the command"
    );

    let out = env.run(checkout, &["init"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{what}: init: {}",
        describe(&out)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{CLONE_RECREATED}\n{REREGISTER}\n"),
        "{what}: init re-creates the folders and registers the store"
    );

    // The same MCP server, with no restart.
    find(&mut mcp, "tokio", COMMITTED);
    let learned = format!("a fact learned in the {what}");
    mcp.call("append_node", &args(&learned));
    mcp.close();
    let log = read_log(checkout).expect("the checkout's log");
    assert!(log.contains(&learned), "{what}: the append is in its log");
    assert_eq!(
        read_log(p).as_deref(),
        Some(p_log.as_str()),
        "{what}: P's log is unchanged"
    );

    let doctor = env.run(checkout, &["doctor"]);
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    assert!(
        stdout.lines().any(|l| l == "subdirs: ok"),
        "{what}: doctor finds every folder: {}",
        describe(&doctor)
    );
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// DRM-107 test 2: a clone is refused with its own path and `dreamd init`;
/// after `init` there the same MCP server finds the committed event, and
/// writes go to the clone's log only.
#[test]
fn a_clone_is_refused_precisely_then_works_after_init() {
    let env = Env::new();
    let (p, watch) = committed_project(&env);
    env.git(env.projects.path(), &["clone", "-q", "P", "C"]);
    let c = std::fs::canonicalize(env.projects.path().join("C")).unwrap();
    refused_then_works(&env, &p, &c, "clone");
    drop(watch);
}

/// DRM-107 test 3: the same for a `git worktree add`.
#[test]
fn a_worktree_is_refused_precisely_then_works_after_init() {
    let env = Env::new();
    let (p, watch) = committed_project(&env);
    let w = env.projects.path().join("W");
    env.git(&p, &["worktree", "add", "-q", w.to_str().unwrap()]);
    let w = std::fs::canonicalize(w).unwrap();
    refused_then_works(&env, &p, &w, "worktree");
    drop(watch);
}

/// DRM-107 test 4: `init` on an existing store re-creates its missing
/// folders, in scaffold order, and no file: every file left is unchanged,
/// the missing log stays missing, and no `config.toml` is written.
#[test]
fn init_recreates_missing_folders_and_no_file() {
    let env = Env::new();
    let p = env.project("P");
    let mut mcp = McpChild::start(&env, &p);
    mcp.call("append_node", &args("one learning"));
    mcp.close();
    let agent = p.join(".agent");
    std::fs::write(
        agent.join("semantic/LESSONS.md"),
        "# Lessons\n\nA hand-written lesson.\n",
    )
    .unwrap();
    for dir in ["working", "personal", ".dreamd"] {
        std::fs::remove_dir_all(agent.join(dir)).unwrap();
    }
    std::fs::remove_file(agent.join("episodic/AGENT_LEARNINGS.jsonl")).unwrap();
    let before = files(&agent);
    assert!(
        before.contains_key(Path::new("semantic/LESSONS.md")),
        "{before:?}"
    );

    let out = env.run(&p, &["init"]);
    assert_eq!(out.status.code(), Some(0), "init: {}", describe(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "dreamd: re-created missing .agent/ folders: working/, personal/, .dreamd/\n",
        "init names the folders, in order: {}",
        describe(&out)
    );
    for dir in ["working", "personal", ".dreamd"] {
        assert!(agent.join(dir).is_dir(), "{dir}/ is re-created");
    }
    assert_eq!(files(&agent), before, "init created and changed no file");
    assert!(
        !agent.join("episodic/AGENT_LEARNINGS.jsonl").exists(),
        "the missing log stays missing"
    );
    assert!(
        !agent.join(".dreamd/config.toml").exists(),
        "no config.toml is written"
    );
}

/// DRM-107 test 5: a scaffold path that is a regular file is an error, and
/// `init` creates nothing, not even a folder that comes before it.
#[test]
fn init_refuses_a_scaffold_path_that_is_a_file() {
    let env = Env::new();
    let p = env.project("P");
    let agent = p.join(".agent");
    std::fs::remove_dir_all(agent.join("working")).unwrap();
    std::fs::remove_dir_all(agent.join("personal")).unwrap();
    std::fs::write(agent.join("personal"), "not a folder\n").unwrap();
    let before = files(&agent);

    let out = env.run(&p, &["init"]);
    assert_eq!(out.status.code(), Some(1), "init: {}", describe(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "dreamd: error \u{2014} .agent/personal is not a directory\n",
        "{}",
        describe(&out)
    );
    assert!(
        !agent.join("working").exists(),
        "working/, which comes first, is not created"
    );
    assert_eq!(files(&agent), before, "nothing is written");
}
