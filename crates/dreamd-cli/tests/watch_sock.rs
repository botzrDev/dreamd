//! BZR-1519: `dreamd watch` binds the socket `DREAMD_SOCK` names, the way
//! every client resolves it.
//!
//! Subprocess tests against the built binary. Every child gets `HOME` set to a
//! tempdir and has `DREAMD_SOCK`, `DREAMD_LOG` and `DREAMD_LOG_FORMAT` removed
//! unless the test sets one. The test process's own environment is never
//! changed. Test files don't share helpers, so the `Watch` shape
//! (`console_logs.rs`) is copied. Every wait is bounded. Unix-only:
//! `DREAMD_SOCK` is a Unix socket override.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);

fn dreamd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dreamd")
}

/// A `dreamd` command in `project` under `home`, with the socket and log
/// variables removed. A test that wants one sets it on the returned command.
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

/// A project with a `.agent/` store, and a separate daemon home.
struct Store {
    project: tempfile::TempDir,
    home: tempfile::TempDir,
}

/// `dreamd init --quiet` in a project with a `.git` root sentinel.
fn seed() -> Store {
    let store = Store {
        project: tempfile::tempdir().expect("temp project"),
        home: tempfile::tempdir().expect("temp HOME"),
    };
    std::fs::create_dir(store.project.path().join(".git")).expect("write .git root sentinel");
    let out = dreamd(store.project.path(), store.home.path())
        .args(["init", "--quiet"])
        .output()
        .expect("run dreamd init");
    assert!(
        out.status.success(),
        "dreamd init --quiet: {}",
        describe(&out)
    );
    store
}

/// A really-running `dreamd watch`, killed on drop.
struct Watch {
    child: Child,
    console: PathBuf,
}

impl Watch {
    /// Spawn `dreamd watch` with both output streams in one file inside
    /// `home`. `env` is applied after `dreamd()` clears the variables.
    fn spawn(project: &Path, home: &Path, env: &[(&str, &Path)]) -> Self {
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

    /// Wait at most [`TIMEOUT`] for the child to exit.
    fn wait_for_exit(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "dreamd watch still running after {TIMEOUT:?}; console:\n{}",
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

/// One raw HTTP/1.1 `GET /api/v1/health` over `socket`; returns the whole
/// response. No version header on the request: a client that sends none is
/// served.
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

/// An absolute `DREAMD_SOCK` is the path `watch` binds, logs, serves and
/// unlinks on SIGTERM. `~/.agent/dreamd.sock` is never created.
#[test]
fn watch_binds_an_absolute_dreamd_sock() {
    let store = seed();
    let project = std::fs::canonicalize(store.project.path()).expect("canonical project");
    // Short, so the path stays under `SUN_LEN`.
    let sock_dir = tempfile::tempdir().expect("socket dir");
    let socket = sock_dir.path().join("o.sock");
    let default_socket = store.home.path().join(".agent").join("dreamd.sock");

    let mut watch = Watch::spawn(
        store.project.path(),
        store.home.path(),
        &[("DREAMD_SOCK", socket.as_path())],
    );
    wait_for_live_socket(&mut watch, &socket);
    assert!(
        !default_socket.exists(),
        "watch must not bind {} when DREAMD_SOCK is set",
        default_socket.display()
    );

    let response = get_health(&socket, &project);
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "health over DREAMD_SOCK must answer 200; got:\n{response}\nconsole:\n{}",
        watch.console()
    );
    let version_line = format!(
        "{}: {}",
        dreamd_core::daemon_client::VERSION_HEADER,
        env!("CARGO_PKG_VERSION")
    );
    assert!(
        response
            .lines()
            .any(|l| l.trim_end().eq_ignore_ascii_case(&version_line)),
        "the response must carry `{version_line}`; got:\n{response}"
    );

    let serving = format!("dreamd watch: serving on {}", socket.display());
    let deadline = Instant::now() + TIMEOUT;
    while !watch.console().contains(&serving) {
        assert!(
            Instant::now() < deadline,
            "console must show `{serving}`:\n{}",
            watch.console()
        );
        std::thread::sleep(POLL);
    }

    let pid = watch.child.id().to_string();
    let killed = Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .expect("run kill -TERM");
    assert!(killed.success(), "kill -TERM {pid} failed");
    let status = watch.wait_for_exit();
    assert!(
        status.success(),
        "watch must exit 0 on SIGTERM, got {status}; console:\n{}",
        watch.console()
    );
    assert!(
        !socket.exists(),
        "watch must unlink {} on SIGTERM",
        socket.display()
    );
    assert!(!default_socket.exists());
}

/// A relative `DREAMD_SOCK` exits 1 with a line that names it, and no socket
/// is created, either at that path or at `~/.agent/dreamd.sock`.
#[test]
fn watch_refuses_a_relative_dreamd_sock() {
    let store = seed();
    let relative = Path::new("relative.sock");
    let default_socket = store.home.path().join(".agent").join("dreamd.sock");

    let mut watch = Watch::spawn(
        store.project.path(),
        store.home.path(),
        &[("DREAMD_SOCK", relative)],
    );
    let status = watch.wait_for_exit();
    let console = watch.console();
    assert_eq!(status.code(), Some(1), "console:\n{console}");
    assert!(
        console.contains("dreamd watch: DREAMD_SOCK is not an absolute path: relative.sock"),
        "console:\n{console}"
    );
    assert!(!store.project.path().join(relative).exists());
    assert!(!store.home.path().join(relative).exists());
    assert!(!default_socket.exists());
}
