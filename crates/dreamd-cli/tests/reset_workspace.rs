//! Subprocess integration tests for `dreamd reset workspace` (DR-113 / WEG-15).
//!
//! Parallel in spirit to `init_golden.rs` but without a stdout golden: the
//! single log line embeds an absolute path that varies per `tempdir`. We
//! instead assert the file contents are byte-identical to
//! `DEFAULT_WORKSPACE_MD` (the same content `dreamd init` scaffolds) and that
//! exit codes match the locked contract:
//!   * `--yes` → exit 0, WORKSPACE.md overwritten.
//!   * No `.agent/` present → exit 2, no `.agent/` created on disk.

use std::process::Command;

use dreamd_core::DEFAULT_WORKSPACE_MD;

fn dreamd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dreamd")
}

#[test]
fn reset_workspace_yes_overwrites_file_and_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    // WEG-32: daemon HOME, separate from the project dir. Sharing tmp would let
    // init_tracing pre-create ~/.agent (= tmp/.agent), so init would skip the
    // scaffold and `.agent/working/` would never exist for the reset to clear.
    let home = tempfile::tempdir().unwrap();

    let init = Command::new(dreamd_bin())
        .arg("init")
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .output()
        .expect("run dreamd init");
    assert!(init.status.success(), "init failed: {:?}", init);

    let workspace = tmp.path().join(".agent/working/WORKSPACE.md");
    std::fs::write(&workspace, b"agent scratched here\n").unwrap();

    let out = Command::new(dreamd_bin())
        .args(["reset", "workspace", "--yes"])
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .output()
        .expect("run dreamd reset workspace");

    assert!(
        out.status.success(),
        "exit={:?}\nstderr={}\nstdout={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );

    let contents = std::fs::read(&workspace).unwrap();
    assert_eq!(
        contents.as_slice(),
        DEFAULT_WORKSPACE_MD.as_bytes(),
        "WORKSPACE.md must be byte-identical to DEFAULT_WORKSPACE_MD after reset",
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("reset workspace: cleared "),
        "stdout must announce the cleared path; got: {stdout}",
    );
    assert!(
        stdout.trim_end().ends_with("WORKSPACE.md"),
        "stdout must end at WORKSPACE.md; got: {stdout}",
    );
}

#[test]
fn reset_workspace_with_no_agent_dir_exits_two_and_creates_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    // intentionally no `.agent/` and no project-root sentinel either.
    // WEG-32: daemon HOME, separate from the project dir, so init_tracing's
    // ~/.agent/dreamd.log lands off tmp and the `!tmp/.agent` assertion holds.
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(dreamd_bin())
        .args(["reset", "workspace", "--yes"])
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .output()
        .expect("run dreamd reset workspace");

    assert_eq!(
        out.status.code(),
        Some(2),
        "must exit 2 when no .agent/ found; got {:?}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        !tmp.path().join(".agent").exists(),
        ".agent/ must not be created on failure",
    );
    assert!(
        out.stdout.is_empty(),
        "stdout must be empty on error; got: {}",
        String::from_utf8_lossy(&out.stdout),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no .agent/ directory found"),
        "stderr must explain the failure; got: {stderr}",
    );
}
