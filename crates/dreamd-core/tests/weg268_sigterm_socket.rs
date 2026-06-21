//! WEG-268 regression: `dreamd watch` must unlink its UDS socket on SIGTERM,
//! not just SIGINT.
//!
//! A service manager (systemd/launchd) stops a daemon with SIGTERM. Before this
//! fix, `run_watch` armed only `ctrl_c()` (SIGINT); a SIGTERM killed the process
//! without running the socket-cleanup path, leaving a stale
//! `~/.agent/dreamd.sock`. This spawns the real `run_watch` in a subprocess (the
//! `weg268_watch_helper` bin), waits for the socket to appear, delivers SIGTERM,
//! and asserts the socket file is gone after a clean exit.
//!
//! Subprocess wiring is required: SIGTERM to the test process itself would kill
//! the runner, so the daemon must live in a child whose PID we can signal.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

fn helper_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_weg268_watch_helper"))
}

#[test]
fn sigterm_unlinks_the_socket() {
    // Isolated HOME → run_watch binds $HOME/.agent/dreamd.sock (dirs::home_dir
    // reads $HOME on Linux). Keeping the socket directly under a short
    // tempfile::tempdir() avoids the macOS sun_path overflow that bites long
    // $TMPDIR prefixes (see weg21_writer_process.rs).
    let home = tempfile::tempdir().expect("home tempdir");
    let project = tempfile::tempdir().expect("project tempdir");

    // AgentRoot::discover only needs a `.agent/` directory in an ancestor of
    // cwd — no project-root sentinel (that's `dreamd init`'s rule).
    std::fs::create_dir(project.path().join(".agent")).expect("create .agent");

    let socket = home.path().join(".agent").join("dreamd.sock");

    let mut child = Command::new(helper_bin())
        .arg(project.path())
        .env("HOME", home.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn watch helper");

    // Wait up to 5s for the daemon to bind the socket.
    let bind_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < bind_deadline && !socket.exists() {
        std::thread::sleep(Duration::from_millis(25));
    }
    if !socket.exists() {
        let _ = child.kill();
        let out = child.wait_with_output().expect("wait helper");
        panic!(
            "daemon never bound the socket; stdout={:?} stderr={:?}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    // Deliver SIGTERM — the signal a service manager uses to stop the daemon.
    kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM).expect("send SIGTERM");

    // Bounded wait for a clean exit so a broken fix can't hang CI.
    let exit_deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if Instant::now() >= exit_deadline {
            let _ = child.kill();
            panic!("daemon did not exit within 5s of SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    assert!(
        status.success(),
        "daemon must exit cleanly on SIGTERM, got {status:?}"
    );

    assert!(
        !socket.exists(),
        "SIGTERM must unlink the socket; still present at {}",
        socket.display(),
    );
}
