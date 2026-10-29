//! Subprocess tests for CLI dispatch paths that exit quickly.
//!
//! Covers usage errors, v0.1 feature guards, and the thin `mcp` / `watch`
//! wrappers' error-to-exit-code mapping — paths that unit tests cannot reach
//! without spawning the binary (they bind real stdio / block on servers).

use std::process::Command;

use dreamd::commands::deprecation;

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
fn dream_dry_exits_zero_and_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".agent")).unwrap();
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(dreamd_bin())
        .args(["dream", "--dry"])
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .output()
        .expect("run dreamd dream --dry");

    // AILAB-341: `--dry` is implemented. An empty `.agent/` promotes nothing, so
    // the preview reports the retire a real cycle would perform — and performs
    // none of it.
    assert_eq!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("dry run"), "got: {stderr}");
    assert!(stderr.contains("not written"), "got: {stderr}");
    assert!(
        !stderr.to_lowercase().contains("not yet implemented"),
        "the stub message must be gone; got: {stderr}"
    );
    assert!(
        out.stdout.is_empty(),
        "a retire preview prints nothing to stdout; got: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !tmp.path().join(".agent/semantic/LESSONS.md").exists(),
        "--dry must not create LESSONS.md"
    );
    assert!(
        !tmp.path()
            .join(".agent/.dreamd/dream_in_progress.wal")
            .exists(),
        "--dry must not open a WAL"
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
    assert!(stderr.contains("--auto is not supported"), "got: {stderr}");
}

/// stderr split into lines, for the BZR-1245 first-line checks.
fn stderr_lines(out: &std::process::Output) -> Vec<String> {
    String::from_utf8(out.stderr.clone())
        .expect("stderr is utf-8")
        .lines()
        .map(str::to_string)
        .collect()
}

const NO_AGENT_DIR: &str = "dreamd: error — no .agent/ directory found. Run `dreamd init` first.";

#[test]
fn memory_subcommands_print_the_deprecation_notice_first() {
    let cases: [(&[&str], &str); 9] = [
        (&["branch", "x"], deprecation::MEMORY_BRANCH),
        (&["checkout", "x"], deprecation::MEMORY_CHECKOUT),
        (&["branches"], deprecation::MEMORY_BRANCHES),
        (&["delete", "x"], deprecation::MEMORY_DELETE),
        (&["diff", "a", "b"], deprecation::MEMORY_DIFF),
        (
            &["bisect", "start", "--good", "a", "--bad", "b"],
            deprecation::MEMORY_BISECT,
        ),
        (&["bisect", "good"], deprecation::MEMORY_BISECT),
        (&["bisect", "bad"], deprecation::MEMORY_BISECT),
        (&["bisect", "run", "true"], deprecation::MEMORY_BISECT),
    ];
    for (args, notice) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let out = Command::new(dreamd_bin())
            .arg("memory")
            .args(args)
            .current_dir(tmp.path())
            .env("HOME", home.path())
            .output()
            .expect("run dreamd memory");

        let lines = stderr_lines(&out);
        assert_eq!(
            lines.first().map(String::as_str),
            Some(notice),
            "{args:?}: {lines:?}"
        );
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert_eq!(
            lines.get(1).map(String::as_str),
            Some(NO_AGENT_DIR),
            "{args:?}"
        );
        assert_eq!(lines.len(), 2, "{args:?}: {lines:?}");
    }
}

#[test]
fn memory_branch_output_is_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    // One live file: a snapshot of a store with none fails on its own
    // (`<id>.partial/` is only created by writing an entry).
    let episodic = tmp.path().join(".agent/episodic");
    std::fs::create_dir_all(&episodic).unwrap();
    std::fs::write(episodic.join("AGENT_LEARNINGS.jsonl"), b"").unwrap();
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(dreamd_bin())
        .args(["memory", "branch", "exp"])
        .current_dir(tmp.path())
        .env("HOME", home.path())
        .output()
        .expect("run dreamd memory branch");

    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!("{}\n", deprecation::MEMORY_BRANCH)
    );
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let id = stdout.strip_suffix('\n').expect("one line");
    assert!(
        id.len() == 64 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "stdout must be one 64-hex line; got {stdout:?}"
    );
}

#[test]
fn vectors_enable_prints_the_deprecation_notice_first() {
    let tmp = tempfile::tempdir().unwrap();
    // No HOME, so neither build can download a model.
    let out = Command::new(dreamd_bin())
        .args(["vectors", "enable"])
        .current_dir(tmp.path())
        .env_remove("HOME")
        .output()
        .expect("run dreamd vectors enable");

    let lines = stderr_lines(&out);
    assert_eq!(
        lines.first().map(String::as_str),
        Some(deprecation::VECTORS_ENABLE),
        "{lines:?}"
    );
    assert_eq!(out.status.code(), Some(2));
    #[cfg(not(feature = "vectors"))]
    let own = "dreamd vectors enable: this binary was built without the vectors feature; rebuild with --features vectors";
    #[cfg(feature = "vectors")]
    let own = "dreamd vectors enable: HOME is unset";
    assert_eq!(lines.get(1).map(String::as_str), Some(own), "{lines:?}");
}

#[test]
fn dream_auto_prints_the_deprecation_notice_first() {
    let cases: [(&[&str], &str); 2] = [
        (
            &["dream", "--auto"],
            "dreamd: --auto is not supported. Set dream_cycle_mode = \"manual\" in config.toml. Cycles run only when you invoke them.",
        ),
        (
            &["dream", "--dry", "--auto"],
            "dreamd: --dry and --auto are mutually exclusive: auto mode performs real writes on a schedule",
        ),
    ];
    for (args, old) in cases {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".agent")).unwrap();
        let home = tempfile::tempdir().unwrap();
        let out = Command::new(dreamd_bin())
            .args(args)
            .current_dir(tmp.path())
            .env("HOME", home.path())
            .output()
            .expect("run dreamd dream --auto");

        let lines = stderr_lines(&out);
        assert_eq!(
            lines.first().map(String::as_str),
            Some(deprecation::DREAM_AUTO),
            "{args:?}: {lines:?}"
        );
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert_eq!(lines.get(1).map(String::as_str), Some(old), "{args:?}");
        assert_eq!(lines.len(), 2, "{args:?}: {lines:?}");
    }
}

#[test]
fn other_commands_print_no_deprecation_notice() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".agent")).unwrap();
    let home = tempfile::tempdir().unwrap();
    for args in [&["dream", "--dry"][..], &["memory", "--help"][..]] {
        let out = Command::new(dreamd_bin())
            .args(args)
            .current_dir(tmp.path())
            .env("HOME", home.path())
            .output()
            .expect("run dreamd");

        assert_eq!(out.status.code(), Some(0), "{args:?}");
        let lines = stderr_lines(&out);
        assert!(
            !lines
                .iter()
                .any(|l| l.to_lowercase().contains("deprecated")),
            "{args:?}: {lines:?}"
        );
    }
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
