//! Integration tests for the `dreamd setup` TTY gate (AILAB-551 Phase 1) and
//! the success card (Phase 2).
//!
//! Integration tests run with a non-TTY stdin, which is exactly the shape this
//! suite needs: the wizard must refuse to prompt into a pipe, and `--yes` must
//! clear that gate and answer from the flags alone. Every run gets a temp
//! project (root sentinel) plus a separate temp `HOME`, mirroring
//! `setup_cmd.rs`.
//!
//! The prompt copy itself is unit-tested in `commands::setup` (injected reader
//! and writer); here we only assert the gate and the "no prompt ever ran"
//! contract. Note prompts are written to **stderr**, so any sweep for prompt
//! text has to cover both streams.
//!
//! **No test here passes `--start-watch`.** Spawning a real daemon from a test
//! would leave one running on the machine long after the suite exits, so the
//! card's daemon line is only ever exercised on the no-spawn path.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn dreamd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dreamd")
}

/// Temp project carrying a `.git` sentinel + a separate temp daemon HOME.
fn project_and_home() -> (tempfile::TempDir, tempfile::TempDir) {
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir(project.path().join(".git")).unwrap();
    let home = tempfile::tempdir().unwrap();
    (project, home)
}

/// Spawn `dreamd setup` with a piped (non-TTY) stdin, write `stdin_payload`,
/// close the pipe, and wait. Closing stdin is what proves the `--yes` path can
/// never block: a reader on this handle would see EOF, not a hang.
///
/// `DREAMD_SOCK` is cleared for the same reason as in [`run_setup`]: a dev box
/// exporting it would point the daemon probe at a real, running daemon instead
/// of the temp `HOME`.
fn run_setup_with_stdin(project: &Path, home: &Path, args: &[&str], stdin_payload: &str) -> Output {
    let mut child = Command::new(dreamd_bin())
        .arg("setup")
        .args(args)
        .current_dir(project)
        .env("HOME", home)
        .env_remove("DREAMD_SOCK")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn dreamd setup");
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin
            .write_all(stdin_payload.as_bytes())
            .expect("write stdin payload");
        // Dropped here: the child sees EOF.
    }
    child.wait_with_output().expect("wait for dreamd setup")
}

/// Run `dreamd setup` non-interactively (`Command::output` gives the child a
/// null, non-TTY stdin). `DREAMD_SOCK` is cleared so the daemon probe resolves
/// off the temp `HOME` and can never see a real daemon on the dev box.
fn run_setup(project: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(dreamd_bin())
        .arg("setup")
        .args(args)
        .current_dir(project)
        .env("HOME", home)
        .env_remove("DREAMD_SOCK")
        .output()
        .expect("run dreamd setup")
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn assert_ok(out: &Output) {
    assert!(
        out.status.success(),
        "expected exit 0; exit={:?}\nstderr={}\nstdout={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
        stdout_of(out)
    );
}

/// Every prompt line that must never reach stdout on a non-interactive run.
const PROMPT_MARKERS: [&str; 4] = [
    "Which coding agent",
    "[1-4, default 1]",
    "Start the shared memory daemon",
    "[y/N]",
];

#[test]
fn non_tty_without_yes_exits_two() {
    let (project, home) = project_and_home();

    let out = run_setup_with_stdin(project.path(), home.path(), &[], "");

    assert_eq!(
        out.status.code(),
        Some(2),
        "a non-TTY stdin without --yes is a usage error; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("error: stdin is not a tty; pass --yes to confirm"),
        "stderr must carry the reset.rs-identical refusal line; got: {stderr}"
    );
    assert!(
        !project.path().join(".agent").exists(),
        ".agent/ must not be scaffolded when the tty gate refuses"
    );
}

/// What `--yes` buys on a piped stdin: it clears the non-TTY gate that would
/// otherwise exit 2, no prompt is rendered on either stream, and the flags —
/// not the bytes waiting in the pipe — decide the run.
///
/// Deliberately NOT named "never reads stdin". That claim cannot fail here: the
/// child's stdin is a pipe, so the non-TTY gate refuses before any prompt with
/// or without `--yes`, and a test proving it would need a pty. The
/// no-`read_line`-under-`--yes` property is verified by inspection of
/// `commands::setup::run` (the prompt block sits inside `if !req.yes`).
#[test]
fn yes_bypasses_the_tty_gate_and_ignores_piped_answers() {
    let (project, home) = project_and_home();

    // A payload that WOULD be consumed by the harness + watch prompts.
    let out = run_setup_with_stdin(
        project.path(),
        home.path(),
        &["--yes", "--harness", "none", "--no-write-mcp"],
        "2\ny\n",
    );

    assert!(
        out.status.success(),
        "--yes must run non-interactively; exit={:?} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Prompts are written to stderr, so both streams have to be checked — a
    // stdout-only sweep would pass even if every question were asked.
    let stderr = String::from_utf8_lossy(&out.stderr);
    for marker in PROMPT_MARKERS {
        assert!(
            !stdout.contains(marker) && !stderr.contains(marker),
            "--yes must never print the {marker:?} prompt; \
             stdout={stdout:?} stderr={stderr:?}"
        );
    }
    // The flags, not the unread stdin payload, decided the run.
    assert!(
        stdout.contains("harness: none"),
        "--harness none must survive; the piped \"2\" must not become cursor; got: {stdout:?}"
    );
}

#[test]
fn non_tty_dry_run_without_yes_exits_two() {
    let (project, home) = project_and_home();

    // The tty gate precedes the dry-run branch: --dry-run alone still refuses.
    let out = run_setup_with_stdin(project.path(), home.path(), &["--dry-run"], "");

    assert_eq!(
        out.status.code(),
        Some(2),
        "--dry-run without --yes on a non-TTY must still be gated; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("error: stdin is not a tty; pass --yes to confirm"),
        "stderr must carry the refusal line; got: {stderr}"
    );
}

#[test]
fn success_card_names_search_nodes_and_remembered() {
    let (project, home) = project_and_home();

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude"],
    );

    assert_ok(&out);
    let stdout = stdout_of(&out);
    for expected in [
        "\u{2713} dreamd is set up.",
        "scaffolded   .agent/",
        "wired        .mcp.json",
        // Byte-identical to adapters/cursor/README.md §6 — one verify sentence.
        "What has dreamd remembered about this codebase?",
        "search_nodes",
    ] {
        assert!(
            stdout.contains(expected),
            "success card must contain {expected:?}; got: {stdout}"
        );
    }
}

/// Launch-copy gate: dreamd *remembers*. It never learns, reflects, or thinks —
/// those words claim a model where there is only a durable log.
#[test]
fn success_card_never_says_learned() {
    let (project, home) = project_and_home();

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude"],
    );

    assert_ok(&out);
    let lowered = stdout_of(&out).to_lowercase();
    for banned in ["learns", "learned", "reflects", "thinks"] {
        assert!(
            !lowered.contains(banned),
            "setup copy must never say {banned:?}; got: {lowered}"
        );
    }
}

#[test]
fn success_card_reports_daemon_not_running() {
    let (project, home) = project_and_home();

    // Temp HOME, no `--start-watch`: nothing is listening on the resolved socket.
    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude"],
    );

    assert_ok(&out);
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("daemon       not running \u{2014} start it with: dreamd watch"),
        "the card must report the daemon honestly; got: {stdout}"
    );
}

#[test]
fn skipped_wiring_card_omits_wired_and_undo() {
    let (project, home) = project_and_home();

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "none", "--no-write-mcp"],
    );

    assert_ok(&out);
    let stdout = stdout_of(&out);
    assert!(
        !stdout.lines().any(|l| l.starts_with("  wired")),
        "nothing was wired, so the card must have no `wired` row; got: {stdout}"
    );
    assert!(
        !stdout.contains("Undo:"),
        "there is nothing to undo when no config was written; got: {stdout}"
    );
    assert!(
        stdout
            .contains(r#"{"mcpServers":{"dreamd":{"command":"npx","args":["-y","dreamd-mcp"]}}}"#),
        "the skipped-wiring card must hand over the copy-paste block; got: {stdout}"
    );
    assert!(
        stdout.contains("search_nodes"),
        "the verify beat survives the skipped-wiring card; got: {stdout}"
    );
}

/// `doctor` runs as an advisory beat: whichever verdict it reaches, `setup`
/// still exits 0.
///
/// The warning has to be forced for this test to mean anything. A fresh
/// scaffold passes every check, so a single run only ever observes
/// `doctor: all checks passed` — and would go on passing if `Ok(false)` were
/// wired to a nonzero exit. Between the two runs the store is damaged by
/// removing the scaffolded `.agent/working/`, which `doctor`'s `check_subdirs`
/// reports as `missing_subdir: working/`. `init` is idempotent on the presence
/// of `.agent/` itself, so the second `setup` does not silently re-scaffold the
/// warning away.
#[test]
fn doctor_warnings_do_not_change_exit_code() {
    let (project, home) = project_and_home();

    let clean = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude"],
    );
    assert_ok(&clean);
    assert!(
        stdout_of(&clean).contains("doctor: all checks passed"),
        "a fresh scaffold is the clean branch — if this drifts, the damage \
         below no longer proves anything; got: {}",
        stdout_of(&clean)
    );

    std::fs::remove_dir_all(project.path().join(".agent").join("working"))
        .expect("remove the scaffolded working/ dir");

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude"],
    );

    // The ratified behaviour: advisory only. A warning must not become an exit
    // code.
    assert_ok(&out);
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("doctor: warnings found \u{2014} run `dreamd doctor` for detail"),
        "the damaged store must reach doctor's warning branch; got: {stdout}"
    );
    assert_eq!(
        stdout.lines().filter(|l| l.starts_with("doctor: ")).count(),
        1,
        "setup must print exactly one doctor verdict line; got: {stdout}"
    );
}

#[test]
fn dry_run_prints_plan_not_success_card() {
    let (project, home) = project_and_home();

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude", "--dry-run"],
    );

    assert_ok(&out);
    let stdout = stdout_of(&out);
    assert!(stdout.contains("dry-run"), "got: {stdout}");
    assert!(stdout.contains("next steps:"), "got: {stdout}");
    assert!(
        !stdout.contains("\u{2713} dreamd is set up."),
        "a plan is not a completed setup; got: {stdout}"
    );
}
