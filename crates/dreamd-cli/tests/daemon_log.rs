//! DRM-119: a `watch` that fails to start leaves the running daemon's log
//! alone; a start that succeeds appends, rotates a large log to `.1`, and
//! keeps the file `0600`; a daemon on another `DREAMD_SOCK` logs beside its
//! socket.
//!
//! Every test drives the real binary on `watch`'s own path. Each child gets
//! the test's own `HOME`, an empty `XDG_CONFIG_HOME`, an explicit
//! `DREAMD_SOCK` (the "default socket" is `<HOME>/.agent/dreamd.sock`, named
//! outright so no child can reach the user's), `GIT_CONFIG_NOSYSTEM=1` and
//! `GIT_CONFIG_GLOBAL=/dev/null`, with `DREAMD_LOG`, `DREAMD_LOG_FORMAT` and
//! `DREAMD_LAUNCHER` removed, so the file layer keeps its default filter. The
//! test process's own environment is never changed. Test files do not share
//! helpers: the `Watch` guard is copied from `project_config.rs`. Every wait
//! polls with a bound; nothing sleeps for a line. Unix-only: the daemon binds
//! a Unix socket.

#![cfg(unix)]

use serde_json::Value;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(30);
/// How long a SIGTERMed `watch` gets to drain and exit.
const STOP_TIMEOUT: Duration = Duration::from_secs(35);
const POLL: Duration = Duration::from_millis(50);
const SERVING_ON: &str = "dreamd watch: serving on";
/// `observability::DAEMON_LOG_ROTATE_BYTES`, as a literal.
const ROTATE_BYTES: usize = 8 * 1024 * 1024;

fn dreamd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dreamd")
}

/// One test's `HOME`, empty `XDG_CONFIG_HOME`, socket folder and projects.
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

    /// `<HOME>/.agent/dreamd.sock`: this test's default socket.
    fn default_sock(&self) -> PathBuf {
        self.home.path().join(".agent").join("dreamd.sock")
    }

    /// `<HOME>/.agent/dreamd.log`: the default socket's daemon log.
    fn daemon_log(&self) -> PathBuf {
        self.home.path().join(".agent").join("dreamd.log")
    }

    /// A `dreamd` command in `dir` that talks to `sock`.
    fn dreamd(&self, dir: &Path, sock: &Path) -> Command {
        let mut cmd = Command::new(dreamd_bin());
        cmd.current_dir(dir)
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.xdg.path())
            .env("DREAMD_SOCK", sock)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("DREAMD_LOG")
            .env_remove("DREAMD_LOG_FORMAT")
            .env_remove("DREAMD_LAUNCHER")
            .stdin(Stdio::null());
        cmd
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
        let out = self
            .dreamd(&dir, &self.default_sock())
            .args(["init", "--quiet"])
            .output()
            .expect("run dreamd init");
        assert_eq!(
            out.status.code(),
            Some(0),
            "init {name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::canonicalize(&dir).unwrap()
    }

    /// A console file inside the test's `HOME` that no other child uses.
    fn console_file(&self) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        self.home.path().join(format!(
            "console-{}.txt",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }
}

// ── `dreamd watch`, copied from `project_config.rs` ─────────────────────────

/// A really-running `dreamd watch`, killed on drop.
struct Watch {
    child: Child,
    console: PathBuf,
}

impl Watch {
    /// Spawn `dreamd watch` in `project` on `sock`, both output streams in
    /// one file, and wait for the socket.
    fn start(env: &Env, project: &Path, sock: &Path) -> Self {
        let console = env.console_file();
        let out = std::fs::File::create(&console).expect("create console file");
        let err = out.try_clone().expect("clone console file handle");
        let child = env
            .dreamd(project, sock)
            .arg("watch")
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .expect("spawn dreamd watch");
        let mut watch = Self { child, console };
        watch.wait_for_live_socket(sock);
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

    /// SIGTERM, then wait for a clean exit.
    fn stop(mut self) {
        let pid = self.child.id().to_string();
        let killed = Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .expect("run kill -TERM");
        assert!(killed.success(), "kill -TERM {pid} failed");
        let deadline = Instant::now() + STOP_TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "watch did not exit within {STOP_TIMEOUT:?} of SIGTERM; console:\n{}",
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
    /// SIGKILL unconditionally: a leaked daemon holds the writer lock on a
    /// tempdir that is about to disappear.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Run a `watch` that is expected to fail, with a bound. Returns its exit
/// status and its console.
fn failing_watch(env: &Env, dir: &Path, sock: &Path) -> (ExitStatus, String) {
    let console = env.console_file();
    let out = std::fs::File::create(&console).expect("create console file");
    let err = out.try_clone().expect("clone console file handle");
    let mut child = env
        .dreamd(dir, sock)
        .arg("watch")
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .expect("spawn dreamd watch");
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "a watch that should fail ran past {TIMEOUT:?}; console:\n{}",
                std::fs::read_to_string(&console).unwrap_or_default()
            );
        }
        std::thread::sleep(POLL);
    };
    (
        status,
        std::fs::read_to_string(&console).unwrap_or_default(),
    )
}

/// One raw HTTP/1.1 `GET /api/v1/health` for `project` over `socket`.
fn get_health(socket: &Path, project: &Path) {
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
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "health must answer 200; got:\n{response}"
    );
}

/// Poll `path` until at least `count` of its lines contain `needle`; return
/// its bytes.
fn wait_for_lines(path: &Path, needle: &str, count: usize) -> Vec<u8> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let bytes = std::fs::read(path).unwrap_or_default();
        let seen = String::from_utf8_lossy(&bytes)
            .lines()
            .filter(|l| l.contains(needle))
            .count();
        if seen >= count {
            return bytes;
        }
        assert!(
            Instant::now() < deadline,
            "{} has {seen} line(s) with {needle:?} after {TIMEOUT:?}, want {count}:\n{}",
            path.display(),
            String::from_utf8_lossy(&bytes)
        );
        std::thread::sleep(POLL);
    }
}

/// The log has no NUL byte, and every line parses as JSON.
fn assert_json_lines(bytes: &[u8], what: &str) {
    assert!(
        !bytes.contains(&0),
        "{what}: the log holds {} NUL byte(s)",
        bytes.iter().filter(|b| **b == 0).count()
    );
    let text = std::str::from_utf8(bytes).unwrap_or_else(|e| panic!("{what}: not UTF-8: {e}"));
    for line in text.lines() {
        serde_json::from_str::<Value>(line)
            .unwrap_or_else(|e| panic!("{what}: a log line is not JSON ({e}): {line:?}"));
    }
}

fn count_lines(bytes: &[u8], needle: &str) -> usize {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|l| l.contains(needle))
        .count()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .permissions()
        .mode()
        & 0o777
}

fn dot_one(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// DRM-119 test 4 (LP-WRITE-01): three `watch` runs that fail to start (at
/// the lock, at the bind, for a missing store) leave the live daemon's log as
/// it was, byte for byte, with nothing of their own in it.
#[test]
fn a_refused_watch_leaves_the_live_daemons_log_alone() {
    let env = Env::new();
    let p = env.project("P");
    let q = env.project("Q");
    // An event Q's index lacks, so Q's `watch` logs its coverage pass before
    // it reaches the bind: a line that must not reach the live daemon's log.
    std::fs::write(
        q.join(".agent/episodic/AGENT_LEARNINGS.jsonl"),
        "{\"schema_version\":\"1.0.0\",\"id\":\"evt_01ARZ3NDEKTSV4RRFFQ69G5FA1\",\
         \"timestamp\":\"2026-11-20T00:00:00Z\",\"pain\":5.0,\"importance\":5.0,\
         \"skill_action\":\"testing::drm119\",\"source_harness\":\"claude-code\",\
         \"content\":\"a line for the coverage pass\"}\n",
    )
    .unwrap();
    let empty = env.projects.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    let sock = env.default_sock();
    let log = env.daemon_log();

    let watch = Watch::start(&env, &p, &sock);
    get_health(&sock, &p);
    wait_for_lines(&log, SERVING_ON, 1);
    let l0 = wait_for_lines(&log, "http request", 1);

    let check = |what: &str| {
        let now = std::fs::read(&log).expect("read the daemon log");
        assert!(
            now.starts_with(&l0),
            "{what}: the log no longer starts with its earlier {} bytes; now {} bytes:\n{}",
            l0.len(),
            now.len(),
            String::from_utf8_lossy(&now)
        );
        // The live daemon is idle, so the log is exactly what it was.
        assert!(
            now == l0,
            "{what}: the log gained {} byte(s):\n{}",
            now.len() - l0.len(),
            String::from_utf8_lossy(&now[l0.len()..])
        );
        assert_json_lines(&now, what);
        let text = String::from_utf8_lossy(&now);
        assert!(!text.contains("writer lock is held"), "{what}:\n{text}");
        assert!(!text.contains("no project root found"), "{what}:\n{text}");
        assert_eq!(count_lines(&now, SERVING_ON), 1, "{what}:\n{text}");
    };

    // (a) A second `watch` in P stops at the writer lock.
    let (status, console) = failing_watch(&env, &p, &sock);
    assert_eq!(status.code(), Some(1), "second watch in P:\n{console}");
    assert!(console.contains("writer lock is held"), "{console}");
    check("after a watch refused at the lock");

    // (b) A `watch` in Q takes Q's lock and stops at the bind.
    let (status, console) = failing_watch(&env, &q, &sock);
    assert!(
        !status.success(),
        "watch in Q must fail at the bind:\n{console}"
    );
    check("after a watch refused at the bind");
    assert!(
        !dot_one(&log).exists(),
        "a watch that never bound rotated the log"
    );

    // (c) A `watch` from a folder with no store.
    let (status, console) = failing_watch(&env, &empty, &sock);
    assert_eq!(status.code(), Some(2), "watch with no store:\n{console}");
    assert!(console.contains("no project root found"), "{console}");
    check("after a watch with no store");

    // The live daemon still writes a clean log.
    get_health(&sock, &p);
    let after = wait_for_lines(&log, "http request", 2);
    assert_json_lines(&after, "after one more request");
    watch.stop();
}

/// DRM-119 test 5: a restart appends below the last run's lines.
#[test]
fn a_restart_appends_below_the_last_run() {
    let env = Env::new();
    let p = env.project("P");
    let sock = env.default_sock();
    let log = env.daemon_log();

    let first = Watch::start(&env, &p, &sock);
    wait_for_lines(&log, SERVING_ON, 1);
    first.stop();
    let l1 = std::fs::read(&log).expect("read the daemon log");

    let second = Watch::start(&env, &p, &sock);
    let now = wait_for_lines(&log, SERVING_ON, 2);
    second.stop();
    let now = std::fs::read(&log).unwrap_or(now);

    assert!(
        now.starts_with(&l1),
        "the restart did not keep the first run's {} bytes:\n{}",
        l1.len(),
        String::from_utf8_lossy(&now)
    );
    assert_eq!(count_lines(&now, SERVING_ON), 2);
    assert_json_lines(&now, "after a restart");
    assert!(!dot_one(&log).exists(), "a small log was rotated");
}

/// DRM-119 test 6: a log of 8 MiB is renamed to `.1` at a start, and both
/// files are `0600`.
#[test]
fn a_large_log_rotates_to_dot_one() {
    let env = Env::new();
    let p = env.project("P");
    let sock = env.default_sock();
    let log = env.daemon_log();

    let line = |i: usize| format!("{{\"old\":{i:08},\"pad\":\"{}\"}}\n", "x".repeat(102));
    assert_eq!(line(0).len(), 128);
    let old: String = (0..ROTATE_BYTES / 128).map(line).collect();
    assert_eq!(old.len(), ROTATE_BYTES);
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, &old).unwrap();

    let watch = Watch::start(&env, &p, &sock);
    wait_for_lines(&log, SERVING_ON, 1);
    watch.stop();

    let rotated = dot_one(&log);
    let kept =
        std::fs::read(&rotated).unwrap_or_else(|e| panic!("read {}: {e}", rotated.display()));
    assert!(
        kept == old.as_bytes(),
        "{} must hold the old log exactly ({} bytes, got {})",
        rotated.display(),
        old.len(),
        kept.len()
    );
    let now = std::fs::read(&log).expect("read the daemon log");
    assert_eq!(count_lines(&now, SERVING_ON), 1);
    assert_eq!(
        count_lines(&now, "\"old\":"),
        0,
        "the new log holds old lines"
    );
    assert_json_lines(&now, "after a rotation");
    assert_eq!(mode(&log), 0o600, "{}", log.display());
    assert_eq!(mode(&rotated), 0o600, "{}", rotated.display());
}

/// DRM-119 test 7 (LP-CONFIG-02): a daemon on another `DREAMD_SOCK` logs to
/// `<socket>.log`, and `status` with that `DREAMD_SOCK` reads that file.
#[test]
fn a_dreamd_sock_daemon_logs_beside_its_socket() {
    let env = Env::new();
    let p = env.project("P");
    let q = env.project("Q");
    let x_sock = env.default_sock();
    let y_sock = env.sock_dir.path().join("dev.sock");
    let x_log = env.daemon_log();
    let y_log = env.sock_dir.path().join("dev.sock.log");

    let x = Watch::start(&env, &p, &x_sock);
    let y = Watch::start(&env, &q, &y_sock);
    get_health(&x_sock, &p);
    get_health(&y_sock, &q);
    let x_bytes = wait_for_lines(&x_log, "http request", 1);
    let y_bytes = wait_for_lines(&y_log, "http request", 1);

    let x_text = String::from_utf8_lossy(&x_bytes).into_owned();
    let y_text = String::from_utf8_lossy(&y_bytes).into_owned();
    let x_serving = format!("{SERVING_ON} {}", x_sock.display());
    let y_serving = format!("{SERVING_ON} {}", y_sock.display());
    assert!(x_text.contains(&x_serving), "X's log:\n{x_text}");
    assert!(
        !x_text.contains(&y_sock.display().to_string()),
        "X's log:\n{x_text}"
    );
    assert!(y_text.contains(&y_serving), "Y's log:\n{y_text}");
    assert!(
        !y_text.contains(&x_sock.display().to_string()),
        "Y's log:\n{y_text}"
    );
    assert_json_lines(&x_bytes, "X's log");
    assert_json_lines(&y_bytes, "Y's log");

    let out = env
        .dreamd(&q, &y_sock)
        .arg("status")
        .output()
        .expect("run dreamd status");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let tail: Vec<&str> = stdout
        .lines()
        .skip_while(|l| !l.starts_with("recent log (last"))
        .skip(1)
        .filter_map(|l| l.strip_prefix("  "))
        .collect();
    assert!(!tail.is_empty(), "status printed no recent log:\n{stdout}");
    let y_now = std::fs::read_to_string(&y_log).expect("read Y's log");
    for line in &tail {
        assert!(
            y_now.lines().any(|l| l == *line),
            "status's recent log is not from Y's file: {line}\nstatus:\n{stdout}"
        );
        assert!(
            !x_text.lines().any(|l| l == *line),
            "status's recent log is from X's file: {line}"
        );
    }

    y.stop();
    x.stop();
}

/// DRM-119 test 8: an existing `0644` log is set to `0600` at the start, and
/// keeps its bytes.
#[test]
fn an_existing_log_becomes_private() {
    let env = Env::new();
    let p = env.project("P");
    let sock = env.default_sock();
    let log = env.daemon_log();
    let old = b"{\"old\":\"before\"}\n";
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, old).unwrap();
    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();

    let watch = Watch::start(&env, &p, &sock);
    let now = wait_for_lines(&log, SERVING_ON, 1);
    assert_eq!(mode(&log), 0o600, "{}", log.display());
    assert!(
        now.starts_with(old),
        "the log lost its old bytes:\n{}",
        String::from_utf8_lossy(&now)
    );
    watch.stop();
}
