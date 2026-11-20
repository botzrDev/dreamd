//! DRM-120 test 8 (LP-HTTP-04): a FIFO at `PREFERENCES.md` neither hangs a
//! `dreamd watch` request nor its shutdown.
//!
//! Drives the real binary. The child gets the test's own `HOME`, an empty
//! `XDG_CONFIG_HOME` and `DREAMD_SOCK`, plus `DREAMD_LOG=off`,
//! `GIT_CONFIG_NOSYSTEM=1` and `GIT_CONFIG_GLOBAL=/dev/null`, with
//! `DREAMD_LAUNCHER` and `DREAMD_LOG_FORMAT` removed. The test process's own
//! environment is never changed. Test files do not share helpers: the `Watch`
//! guard is copied from `project_config.rs`, and SIGKILLs on drop. Every wait
//! is bounded. Unix-only: a FIFO and a Unix socket.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);

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
            .env_remove("DREAMD_LAUNCHER")
            .env_remove("DREAMD_LOG_FORMAT")
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
            .dreamd(&dir)
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
}

// ── `dreamd watch`, copied from `project_config.rs` ─────────────────────────

/// A really-running `dreamd watch`, killed on drop.
struct Watch {
    child: Child,
    console: PathBuf,
}

impl Watch {
    fn start(env: &Env, project: &Path) -> Self {
        let console = env.home.path().join("watch.console");
        let out = std::fs::File::create(&console).expect("create console file");
        let err = out.try_clone().expect("clone console file handle");
        let child = env
            .dreamd(project)
            .arg("watch")
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

/// One raw `GET /api/v1/preferences` with a 10 s read timeout. Returns the
/// status, or `None` when no full answer came, and the time it took.
fn get_preferences(socket: &Path, project: &Path) -> (Option<u16>, Duration) {
    let started = Instant::now();
    let mut stream = UnixStream::connect(socket).expect("connect daemon socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    let request = format!(
        "GET /api/v1/preferences HTTP/1.1\r\nHost: localhost\r\nX-Agent-Root: {}\r\nConnection: close\r\n\r\n",
        project.display()
    );
    stream.write_all(request.as_bytes()).expect("write request");
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let status = String::from_utf8_lossy(&response)
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok());
    (status, started.elapsed())
}

/// DRM-120 test 8 (LP-HTTP-04): the request answers `500` at once, and the
/// daemon still stops on SIGTERM.
#[test]
fn a_fifo_does_not_hang_watch_or_its_shutdown() {
    let env = Env::new();
    let p = env.project("P");
    let pref = p.join(".agent/personal/PREFERENCES.md");
    std::fs::create_dir_all(pref.parent().unwrap()).unwrap();
    let made = Command::new("mkfifo")
        .arg(&pref)
        .status()
        .expect("run mkfifo");
    assert!(made.success(), "mkfifo {}", pref.display());

    let mut watch = Watch::start(&env, &p);
    let (status, took) = get_preferences(&env.sock(), &p);
    assert!(
        took < Duration::from_secs(2),
        "the request took {took:?} (status {status:?}); console:\n{}",
        watch.console()
    );
    assert_eq!(status, Some(500), "console:\n{}", watch.console());

    let pid = watch.child.id().to_string();
    let killed = Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .expect("run kill -TERM");
    assert!(killed.success(), "kill -TERM {pid} failed");
    let deadline = Instant::now() + Duration::from_secs(35);
    let exit = loop {
        if let Some(exit) = watch.child.try_wait().expect("try_wait") {
            break exit;
        }
        assert!(
            Instant::now() < deadline,
            "watch did not exit within 35 s of SIGTERM; console:\n{}",
            watch.console()
        );
        std::thread::sleep(POLL);
    };
    assert!(
        exit.success(),
        "watch must exit 0 on SIGTERM, got {exit}; console:\n{}",
        watch.console()
    );
}
