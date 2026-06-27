//! Byte-exact golden-file tests for `dreamd init` (DR-105 / WEG-9).
//!
//! Three scenarios via subprocess:
//!   1. **First run** -- stdout must match `tests/fixtures/init.golden.txt`
//!      verbatim (16 lines / 651 bytes). Validates directory scaffold, state.json
//!      schema, gitignore append, WORKSPACE.md, and privacy disclosure.
//!   2. **Re-run** -- stdout must match `tests/fixtures/init.rerun.golden.txt`
//!      (1 line / 63 bytes). Validates the idempotency guard.
//!   3. **No project root** -- exit non-zero, empty stdout, stderr explains
//!      the failure. Validates that `.agent/` is never partially created.
//!
//! Any change to `init.rs` stdout text, ordering, or whitespace must update the
//! golden fixtures AND coordinate with the Clip A beat-sheet
//! (`context/video/scripts/clip-a/`).

use std::process::Command;

const FIRST_RUN_FIXTURE: &[u8] = include_bytes!("../../../tests/fixtures/init.golden.txt");
const RERUN_FIXTURE: &[u8] = include_bytes!("../../../tests/fixtures/init.rerun.golden.txt");
const QUIET_FIRST_RUN_FIXTURE: &[u8] =
    include_bytes!("../../../tests/fixtures/init.quiet.golden.txt");
const QUIET_RERUN_FIXTURE: &[u8] =
    include_bytes!("../../../tests/fixtures/init.quiet.rerun.golden.txt");

fn dreamd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dreamd")
}

#[test]
fn first_run_matches_golden() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    // WEG-32: daemon HOME, separate from the project dir. init_tracing creates
    // ~/.agent/dreamd.log at startup; sharing tmp would pre-create the project
    // .agent/ and make init report "already initialized".
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(dreamd_bin())
        .arg("init")
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .output()
        .expect("run dreamd init");

    assert!(
        out.status.success(),
        "exit={:?}\nstderr={}\nstdout={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        out.stdout.as_slice(),
        FIRST_RUN_FIXTURE,
        "stdout does not match init.golden.txt\n--- actual ---\n{}\n--- expected ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(FIRST_RUN_FIXTURE)
    );

    let jsonl = tmp.path().join(".agent/episodic/AGENT_LEARNINGS.jsonl");
    assert!(jsonl.exists(), "AGENT_LEARNINGS.jsonl should pre-exist");
    assert_eq!(
        std::fs::metadata(&jsonl).unwrap().len(),
        0,
        "AGENT_LEARNINGS.jsonl must be zero bytes on creation (Clip A beat 0:22)"
    );

    let state_path = tmp.path().join(".agent/.dreamd/state.json");
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_eq!(state["schema_version"], "1.0");
    assert_eq!(state["last_dream_cycle_status"], "idle");
    assert!(state["last_dream_cycle_at"].is_null());
    assert!(state["daemon_version"].is_string());

    let gitignore = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
    assert!(gitignore.contains("/.agent/.dreamd/"));

    let workspace = tmp.path().join(".agent/working/WORKSPACE.md");
    assert!(workspace.exists());

    let reg_path = home.path().join(".agent/registry.toml");
    assert!(reg_path.exists(), "registry.toml must be created");
    let reg: toml::Value = toml::from_str(&std::fs::read_to_string(&reg_path).unwrap()).unwrap();
    let projects = reg["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 1);
    assert!(projects[0]["root"]
        .as_str()
        .unwrap()
        .contains(tmp.path().to_str().unwrap()));
}

#[test]
fn rerun_matches_golden() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    // WEG-32: daemon HOME, separate from the project dir (see first_run_matches_golden).
    let home = tempfile::tempdir().unwrap();

    let first = Command::new(dreamd_bin())
        .arg("init")
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .output()
        .expect("first init");
    assert!(first.status.success());

    let out = Command::new(dreamd_bin())
        .arg("init")
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .output()
        .expect("rerun init");

    assert!(out.status.success(), "rerun must exit 0");
    assert_eq!(
        out.stdout.as_slice(),
        RERUN_FIXTURE,
        "stdout does not match init.rerun.golden.txt\n--- actual ---\n{}\n--- expected ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(RERUN_FIXTURE)
    );

    let reg_path = home.path().join(".agent/registry.toml");
    let reg: toml::Value = toml::from_str(&std::fs::read_to_string(&reg_path).unwrap()).unwrap();
    let projects = reg["projects"].as_array().unwrap();
    assert_eq!(
        projects.len(),
        1,
        "registry must remain idempotent across reruns"
    );
}

#[test]
fn quiet_first_run_matches_golden() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    // WEG-32: daemon HOME, separate from the project dir (see first_run_matches_golden).
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(dreamd_bin())
        .arg("init")
        .arg("--quiet")
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .output()
        .expect("run dreamd init --quiet");

    assert!(
        out.status.success(),
        "exit={:?}\nstderr={}\nstdout={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        out.stdout.as_slice(),
        QUIET_FIRST_RUN_FIXTURE,
        "stdout does not match init.quiet.golden.txt\n--- actual ---\n{}\n--- expected ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(QUIET_FIRST_RUN_FIXTURE)
    );

    // Side effects must still occur even when output is suppressed.
    assert!(tmp.path().join(".agent/.dreamd/state.json").exists());
    let gitignore = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
    assert!(gitignore.contains("/.agent/.dreamd/"));
}

#[test]
fn quiet_rerun_matches_golden() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    // WEG-32: daemon HOME, separate from the project dir (see first_run_matches_golden).
    let home = tempfile::tempdir().unwrap();

    let first = Command::new(dreamd_bin())
        .arg("init")
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .output()
        .expect("first init");
    assert!(first.status.success());

    let out = Command::new(dreamd_bin())
        .arg("init")
        .arg("--quiet")
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .output()
        .expect("rerun init --quiet");

    assert!(out.status.success(), "rerun must exit 0");
    assert_eq!(
        out.stdout.as_slice(),
        QUIET_RERUN_FIXTURE,
        "stdout does not match init.quiet.rerun.golden.txt\n--- actual ---\n{}\n--- expected ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(QUIET_RERUN_FIXTURE)
    );
}

#[test]
fn no_project_root_fails_and_skips_scaffold() {
    let tmp = tempfile::tempdir().unwrap();
    // intentionally no .git / Cargo.toml / package.json / pyproject.toml
    // WEG-32: daemon HOME, separate from the project dir, so init_tracing's
    // ~/.agent/dreamd.log lands off tmp and the `!tmp/.agent` assertion holds.
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(dreamd_bin())
        .arg("init")
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .output()
        .expect("run dreamd init");

    assert!(
        !out.status.success(),
        "must exit non-zero when no project root found"
    );
    assert!(
        !tmp.path().join(".agent").exists(),
        ".agent/ must not be created on failure"
    );
    assert!(
        out.stdout.is_empty(),
        "stdout must be empty on error; got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no project root found"),
        "stderr must explain the failure; got: {stderr}"
    );
}
