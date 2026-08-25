//! Subprocess integration tests for AILAB-223: `dreamd doctor` orphaned-UDS
//! detection and `--repair` (unlink orphan socket, wipe + rebuild the derived
//! Tantivy cache, never touch the episodic log).
//!
//! Harness modeled on `dream_doctor_discover.rs`: raw `std::process::Command`
//! against `CARGO_BIN_EXE_dreamd` with an injectable `HOME`. The daemon socket
//! is injected via `DREAMD_SOCK` (absolute), exercising the same resolver the
//! status and MCP paths use — no hardcoded `~/.agent/dreamd.sock` anywhere.

#![cfg(unix)]

use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use dreamd_protocol::{AgentLearning, EventId, RECORD_SCHEMA_VERSION};

fn dreamd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dreamd")
}

/// Scaffold the five init-created subdirs plus a one-record episodic log with
/// a matching index watermark, so every default check reads healthy.
fn seed_store(root: &Path) {
    for sub in ["working", "episodic", "semantic", "personal", ".dreamd"] {
        std::fs::create_dir_all(root.join(".agent").join(sub)).unwrap();
    }
    let record = AgentLearning {
        schema_version: RECORD_SCHEMA_VERSION.to_string(),
        id: EventId::parse("evt_01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
        timestamp: DateTime::<Utc>::from_timestamp(1751500800, 0).unwrap(),
        pain: 5.0,
        importance: 6.0,
        pinned: false,
        skill_action: "rust::borrow".to_string(),
        source_harness: "test-harness".to_string(),
        content: "entry".to_string(),
    };
    let line = format!("{}\n", serde_json::to_string(&record).unwrap());
    std::fs::write(root.join(".agent/episodic/AGENT_LEARNINGS.jsonl"), line).unwrap();
    std::fs::write(
        root.join(".agent/.dreamd/index_progress.json"),
        format!("{{\"last_indexed_id\":\"{}\"}}", record.id.as_str()),
    )
    .unwrap();
}

/// Wall-clock budget for one `dreamd doctor` child. A healthy repair on this
/// one-record fixture finishes in well under a second; 30s is pure headroom for
/// a cold CI runner. Exceeding it means the child wedged (AILAB-575: the repair
/// path once deadlocked forever against the process-wide stderr lock), and the
/// test must FAIL loudly rather than inherit CI's 6h job ceiling.
const DOCTOR_TIMEOUT: Duration = Duration::from_secs(30);

/// Poll interval while waiting on the child. Short enough that a passing run
/// pays no visible cost, long enough not to spin a core.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Spawn `dreamd doctor` and wait for it with a hard deadline.
///
/// Deliberately hand-rolled: the workspace carries no wait-with-timeout crate,
/// and `Command::output()` cannot be interrupted — a wedged child hangs the
/// suite indefinitely. On timeout the child is SIGKILLed (so no orphaned
/// `dreamd doctor --repair` survives the run) and the test panics.
fn run_doctor(project: &Path, home: &Path, sock: &Path, args: &[&str]) -> std::process::Output {
    let mut child = Command::new(dreamd_bin())
        .arg("doctor")
        .args(args)
        .current_dir(project)
        .env("HOME", home)
        .env("DREAMD_SOCK", sock)
        // Pin the log level instead of inheriting it. `info` is what makes
        // Tantivy's `segment_updater` thread emit through the console layer,
        // which is precisely the AILAB-575 deadlock trigger — a developer
        // running with `DREAMD_LOG=off` would otherwise silently disarm this
        // regression guard. It also bounds child output so the un-drained
        // pipes below cannot fill and cause a false timeout.
        .env("DREAMD_LOG", "info")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn dreamd doctor");

    let deadline = Instant::now() + DOCTOR_TIMEOUT;
    loop {
        match child.try_wait().expect("try_wait on dreamd doctor") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                // Kill first: that closes the child's pipe ends, so the
                // `wait_with_output` below cannot itself block.
                let _ = child.kill();
                let out = child.wait_with_output().expect("reap timed-out doctor");
                panic!(
                    "`dreamd doctor {}` did not exit within {}s — killed. \
                     The repair path must never block forever (AILAB-575).\n\
                     partial stdout={}\npartial stderr={}",
                    args.join(" "),
                    DOCTOR_TIMEOUT.as_secs(),
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr),
                );
            }
            None => std::thread::sleep(POLL_INTERVAL),
        }
    }
    child
        .wait_with_output()
        .expect("collect dreamd doctor output")
}

#[test]
fn doctor_flags_orphaned_socket_via_dreamd_sock() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_store(tmp.path());

    // Bind then drop: std does not unlink the socket file, leaving an orphan.
    let sock = home.path().join("dreamd.sock");
    drop(UnixListener::bind(&sock).unwrap());

    let out = run_doctor(tmp.path(), home.path(), &sock, &[]);

    assert_eq!(
        out.status.code(),
        Some(1),
        "orphan socket must exit 1; stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("orphaned_uds:") && stdout.contains("WARNING"),
        "stdout must flag the orphaned socket; got: {stdout}",
    );
    assert!(sock.exists(), "read-only doctor must not unlink the socket");
}

#[test]
fn doctor_repair_rebuilds_index_and_leaves_jsonl_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_store(tmp.path());
    let jsonl = tmp.path().join(".agent/episodic/AGENT_LEARNINGS.jsonl");
    let jsonl_before = std::fs::read(&jsonl).unwrap();

    let index_dir = tmp.path().join(".agent/.dreamd/index");
    std::fs::create_dir_all(&index_dir).unwrap();
    std::fs::write(index_dir.join("junk.seg"), b"junk").unwrap();

    // Absolute path with nothing bound and no file: healthy socket state.
    let sock = home.path().join("dreamd.sock");
    let out = run_doctor(tmp.path(), home.path(), &sock, &["--repair"]);

    assert_eq!(
        out.status.code(),
        Some(0),
        "repair on a healthy store must exit 0; stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("repair:") && stdout.contains("rebuilt index from the episodic log"),
        "repair summary must report the rebuild; got: {stdout}",
    );
    assert!(
        !index_dir.join("junk.seg").exists(),
        "stale index contents must be wiped",
    );
    assert!(
        index_dir.join("meta.json").exists(),
        "a fresh Tantivy index must exist after rebuild",
    );
    assert_eq!(
        std::fs::read(&jsonl).unwrap(),
        jsonl_before,
        "repair must leave the episodic log byte-identical",
    );
}

#[test]
fn doctor_repair_unlinks_orphaned_socket() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_store(tmp.path());

    let sock = home.path().join("dreamd.sock");
    drop(UnixListener::bind(&sock).unwrap());

    let out = run_doctor(tmp.path(), home.path(), &sock, &["--repair"]);

    // The pre-repair orphaned_uds check WARNed, so the run keeps exit-1
    // semantics even though the repair itself succeeded.
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("unlinked orphaned socket"),
        "repair summary must report the unlink; got: {stdout}",
    );
    assert!(!sock.exists(), "orphan socket file must be removed");
}

#[test]
fn doctor_repair_refuses_when_daemon_live() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_store(tmp.path());

    let sock = home.path().join("dreamd.sock");
    let _listener = UnixListener::bind(&sock).unwrap();

    let index_dir = tmp.path().join(".agent/.dreamd/index");
    std::fs::create_dir_all(&index_dir).unwrap();
    std::fs::write(index_dir.join("sentinel"), b"x").unwrap();

    let out = run_doctor(tmp.path(), home.path(), &sock, &["--repair"]);

    assert_eq!(
        out.status.code(),
        Some(1),
        "repair against a live daemon must fail closed; stdout={}",
        String::from_utf8_lossy(&out.stdout),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Stop `dreamd watch` / `dreamd mcp` first"),
        "stderr must tell the user to stop the daemon; got: {stderr}",
    );
    assert!(
        index_dir.join("sentinel").exists(),
        "no index wipe may happen before the refusal",
    );
    assert!(sock.exists(), "a live socket must never be unlinked");
}
