//! WEG-60 regression: `dreamd watch` must run WAL recovery before serving traffic.
//!
//! If `recover_on_startup` is not called at daemon boot, a mid-cycle crash leaves
//! `dream_in_progress.wal` and partial `.tmp` files behind forever. This spawns
//! the real `run_watch` via `weg268_watch_helper`, waits for the socket to bind
//! (recovery completed), and asserts the WAL and temp artefacts are gone.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use dreamd_core::layout::AgentRoot;
use dreamd_core::wal::{DreamWal, WalIntent};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

fn helper_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_weg268_watch_helper"))
}

fn plant_mid_cycle_crash(project: &Path) -> AgentRoot {
    std::fs::create_dir_all(project.join(".agent/.dreamd")).expect("create .agent/.dreamd");
    let root = AgentRoot::new(project);

    let tmp_path = root.episodic_jsonl().with_extension("tmp");
    std::fs::create_dir_all(tmp_path.parent().unwrap()).expect("create episodic dir");
    std::fs::write(&tmp_path, b"partial write\n").expect("write partial tmp");

    let wal = DreamWal {
        schema_version: "1.0".to_string(),
        intents: vec![WalIntent::PruneEpisodicMemory {
            temp_file_path: tmp_path.to_string_lossy().into_owned(),
        }],
    };
    let wal_json = serde_json::to_string_pretty(&wal).expect("serialize wal");
    std::fs::write(root.wal_path(), wal_json.as_bytes()).expect("write wal");

    let state = serde_json::json!({
        "schema_version": "1.0",
        "last_dream_cycle_status": "in_progress",
        "last_dream_cycle_at": null,
    });
    std::fs::write(
        root.state_json(),
        serde_json::to_string_pretty(&state).expect("serialize state"),
    )
    .expect("write state.json");

    root
}

#[test]
fn watch_startup_recovers_mid_cycle_crash() {
    let home = tempfile::tempdir().expect("home tempdir");
    let project = tempfile::tempdir().expect("project tempdir");
    let root = plant_mid_cycle_crash(project.path());

    assert!(root.wal_path().exists(), "precondition: WAL must exist");
    assert!(
        root.episodic_jsonl().with_extension("tmp").exists(),
        "precondition: tmp file must exist"
    );

    let socket = home.path().join(".agent").join("dreamd.sock");

    let mut child = Command::new(helper_bin())
        .arg(project.path())
        .env("HOME", home.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn watch helper");

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

    assert!(
        !root.wal_path().exists(),
        "WAL must be removed after startup recovery"
    );
    assert!(
        !root.episodic_jsonl().with_extension("tmp").exists(),
        ".jsonl.tmp must be removed after startup recovery"
    );
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.state_json()).expect("read state.json"))
            .expect("parse state.json");
    assert_eq!(
        state["last_dream_cycle_status"], "failed",
        "state.json must reflect failed recovery"
    );

    kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM).expect("send SIGTERM");

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
