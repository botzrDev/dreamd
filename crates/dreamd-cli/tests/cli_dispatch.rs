//! Subprocess tests for CLI dispatch paths that exit quickly.
//!
//! Covers usage errors, v0.1 feature guards, and the thin `mcp` / `watch`
//! wrappers' error-to-exit-code mapping — paths that unit tests cannot reach
//! without spawning the binary (they bind real stdio / block on servers).

use std::process::Command;

fn dreamd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dreamd")
}

#[test]
fn no_subcommand_exits_two() {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(dreamd_bin())
        .env("HOME", home.path())
        .output()
        .expect("run dreamd");

    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no subcommand"),
        "stderr must explain missing subcommand; got: {stderr}"
    );
    assert!(stderr.contains("--help"));
}

#[test]
fn dream_dry_exits_two() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".agent")).unwrap();
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(dreamd_bin())
        .args(["dream", "--dry"])
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .output()
        .expect("run dreamd dream --dry");

    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--dry is not yet implemented"),
        "got: {stderr}"
    );
}

#[test]
fn dream_auto_exits_two() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".agent")).unwrap();
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(dreamd_bin())
        .args(["dream", "--auto"])
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .output()
        .expect("run dreamd dream --auto");

    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--auto is not yet supported"),
        "got: {stderr}"
    );
}

#[test]
fn dream_auto_config_exits_two() {
    let tmp = tempfile::tempdir().unwrap();
    let dreamd_dir = tmp.path().join(".agent/.dreamd");
    std::fs::create_dir_all(&dreamd_dir).unwrap();
    std::fs::write(
        dreamd_dir.join("config.toml"),
        r#"dream_cycle_mode = "auto""#,
    )
    .unwrap();
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(dreamd_bin())
        .arg("dream")
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .output()
        .expect("run dreamd dream with auto config");

    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("dream_cycle_mode = auto is not supported"),
        "got: {stderr}"
    );
}

#[test]
fn dream_bad_source_date_epoch_exits_one() {
    let tmp = tempfile::tempdir().unwrap();
    let episodic = tmp.path().join(".agent/episodic");
    std::fs::create_dir_all(&episodic).unwrap();
    std::fs::write(episodic.join("AGENT_LEARNINGS.jsonl"), b"").unwrap();
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(dreamd_bin())
        .args(["dream", "--no-commit"])
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .env("SOURCE_DATE_EPOCH", "not-a-number")
        .output()
        .expect("run dreamd dream with bad SOURCE_DATE_EPOCH");

    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("SOURCE_DATE_EPOCH"),
        "stderr must mention SOURCE_DATE_EPOCH; got: {stderr}"
    );
}

#[test]
fn dream_pinned_epoch_completes() {
    let tmp = tempfile::tempdir().unwrap();
    let episodic = tmp.path().join(".agent/episodic");
    std::fs::create_dir_all(&episodic).unwrap();
    std::fs::write(episodic.join("AGENT_LEARNINGS.jsonl"), b"").unwrap();
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(dreamd_bin())
        .args(["dream", "--no-commit"])
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .env("SOURCE_DATE_EPOCH", "1748520000")
        .output()
        .expect("run dreamd dream with pinned epoch");

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("dream cycle complete"));
}

#[test]
fn watch_without_agent_dir_exits_two() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(dreamd_bin())
        .arg("watch")
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .output()
        .expect("run dreamd watch");

    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("dreamd watch:"),
        "stderr must be prefixed; got: {stderr}"
    );
    assert!(
        stderr.contains("no project root") && stderr.contains("dreamd init"),
        "got: {stderr}"
    );
}

#[test]
fn watch_auto_config_exits_one() {
    let tmp = tempfile::tempdir().unwrap();
    let dreamd_dir = tmp.path().join(".agent/.dreamd");
    std::fs::create_dir_all(&dreamd_dir).unwrap();
    std::fs::write(
        dreamd_dir.join("config.toml"),
        r#"dream_cycle_mode = "auto""#,
    )
    .unwrap();
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(dreamd_bin())
        .arg("watch")
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .output()
        .expect("run dreamd watch with auto config");

    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("dreamd watch:"),
        "stderr must be prefixed; got: {stderr}"
    );
    assert!(
        stderr.contains("auto") || stderr.contains("not supported"),
        "got: {stderr}"
    );
}

#[test]
fn mcp_relative_dreamd_sock_exits_one() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(dreamd_bin())
        .arg("mcp")
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .env("DREAMD_SOCK", "relative/path.sock")
        .output()
        .expect("run dreamd mcp with relative DREAMD_SOCK");

    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("dreamd mcp:"),
        "stderr must be prefixed; got: {stderr}"
    );
    assert!(
        stderr.contains("DREAMD_SOCK") || stderr.contains("absolute"),
        "got: {stderr}"
    );
}

#[test]
fn mcp_manual_only_overrides_auto_config_then_fails_on_sock() {
    // --manual-only must clear the Auto guard so dispatch reaches mcp::run,
    // which then fails on the relative DREAMD_SOCK (exit 1, not the Auto exit).
    let tmp = tempfile::tempdir().unwrap();
    let dreamd_dir = tmp.path().join(".agent/.dreamd");
    std::fs::create_dir_all(&dreamd_dir).unwrap();
    std::fs::write(
        dreamd_dir.join("config.toml"),
        r#"dream_cycle_mode = "auto""#,
    )
    .unwrap();
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(dreamd_bin())
        .args(["mcp", "--manual-only"])
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .env("DREAMD_SOCK", "relative/path.sock")
        .output()
        .expect("run dreamd mcp --manual-only");

    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("dream_cycle_mode = auto is not supported"),
        "manual-only must bypass Auto guard; got: {stderr}"
    );
    assert!(stderr.contains("dreamd mcp:"), "got: {stderr}");
}

#[test]
fn reset_workspace_without_yes_on_non_tty_exits_two() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".agent/working")).unwrap();
    std::fs::write(tmp.path().join(".agent/working/WORKSPACE.md"), b"scratch\n").unwrap();
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(dreamd_bin())
        .args(["reset", "workspace"])
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .output()
        .expect("run dreamd reset workspace without --yes");

    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("stdin is not a tty") || stderr.contains("--yes"),
        "got: {stderr}"
    );
    // File must be untouched.
    assert_eq!(
        std::fs::read(tmp.path().join(".agent/working/WORKSPACE.md")).unwrap(),
        b"scratch\n"
    );
}

#[test]
fn status_without_daemon_exits_one() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(dreamd_bin())
        .arg("status")
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .output()
        .expect("run dreamd status");

    // No socket → daemon not running → exit 1 (status reports unhealthy).
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("daemon: not running"), "got: {stdout}");
}
