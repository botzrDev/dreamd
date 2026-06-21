//! Subprocess integration tests for WEG-281: `dreamd dream` and `dreamd doctor`
//! resolve their store with the ancestor walk (`AgentRoot::discover`) the other
//! commands already use — operating on a parent project's `.agent/` from a
//! subdirectory, and erroring (exit 2, pointer to `dreamd init`) rather than
//! scaffolding a phantom empty store when no `.agent/` exists up-tree.
//!
//! Harness modeled on `reset_workspace.rs` (raw `std::process::Command` +
//! `CARGO_BIN_EXE_dreamd`); store setup modeled on
//! `dreamd-core/tests/dream_cycle_snapshot.rs`. These commands resolve via
//! `.agent/` existence, so the setup creates `.agent/...` directly — NOT an
//! `init` root-sentinel like `Cargo.toml`. `std::env::temp_dir()` is under
//! `/tmp` on Linux, so its ancestors never reach `~/.agent`; `discover`
//! correctly finds nothing in the bare-tmpdir cases.

use std::process::Command;

fn dreamd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dreamd")
}

#[test]
fn dream_from_subdir_operates_on_parent_store() {
    let tmp = tempfile::tempdir().unwrap();

    // Minimal runnable no-op store at the project root: `run_decay_pruner`
    // fsyncs the episodic dir, so it must exist; an empty JSONL makes the
    // cluster engine promote nothing and commit cleanly.
    let episodic = tmp.path().join(".agent/episodic");
    std::fs::create_dir_all(&episodic).unwrap();
    std::fs::write(episodic.join("AGENT_LEARNINGS.jsonl"), b"").unwrap();

    let sub = tmp.path().join("sub");
    std::fs::create_dir(&sub).unwrap();

    // --no-commit keeps the cycle fully in-process (no daemon proxy, no git).
    let out = Command::new(dreamd_bin())
        .args(["dream", "--no-commit"])
        .current_dir(&sub)
        .env("HOME", tmp.path())
        .output()
        .expect("run dreamd dream from subdir");

    assert_eq!(
        out.status.code(),
        Some(0),
        "dream from subdir must exit 0; stderr={}\nstdout={}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );
    assert!(
        !sub.join(".agent").exists(),
        "must NOT scaffold a `.agent/` in the subdirectory",
    );
    assert!(
        tmp.path().join(".agent/.dreamd/state.json").exists(),
        "the cycle must have operated on the PARENT store (state.json written there)",
    );
}

#[test]
fn dream_with_no_store_errors_and_scaffolds_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    // Bare tmpdir: no `.agent/` anywhere up-tree. discover fails before the
    // daemon proxy, so no --no-commit is needed.

    let out = Command::new(dreamd_bin())
        .arg("dream")
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .output()
        .expect("run dreamd dream in bare dir");

    assert_eq!(
        out.status.code(),
        Some(2),
        "dream with no store must exit 2; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("dreamd init"),
        "stderr must point at `dreamd init`; got: {stderr}",
    );
    assert!(
        !tmp.path().join(".agent").exists(),
        "must NOT scaffold a phantom `.agent/` on the error path",
    );
}

#[test]
fn doctor_with_no_store_errors() {
    let tmp = tempfile::tempdir().unwrap();

    let out = Command::new(dreamd_bin())
        .arg("doctor")
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .output()
        .expect("run dreamd doctor in bare dir");

    assert_eq!(
        out.status.code(),
        Some(2),
        "doctor with no store must exit 2; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("dreamd init"),
        "stderr must point at `dreamd init`; got: {stderr}",
    );
    assert!(
        !tmp.path().join(".agent").exists(),
        "doctor must NOT create a `.agent/` on the error path",
    );
}

#[test]
fn doctor_from_subdir_uses_parent_store() {
    let tmp = tempfile::tempdir().unwrap();
    // A bare `.agent/` at the root is enough for discover; doctor only reads.
    std::fs::create_dir_all(tmp.path().join(".agent")).unwrap();

    let sub = tmp.path().join("sub");
    std::fs::create_dir(&sub).unwrap();

    let out = Command::new(dreamd_bin())
        .arg("doctor")
        .current_dir(&sub)
        .env("HOME", tmp.path())
        .output()
        .expect("run dreamd doctor from subdir");

    // Default config → manual mode → doctor reports healthy → exit 0.
    assert_eq!(
        out.status.code(),
        Some(0),
        "doctor from subdir must exit 0; stderr={}\nstdout={}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("dream_cycle_mode:"),
        "doctor stdout must report dream_cycle_mode; got: {stdout}",
    );
}
