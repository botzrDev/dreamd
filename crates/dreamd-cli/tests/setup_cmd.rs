//! Integration tests for `dreamd setup` (AILAB-549).
//!
//! `setup` is the front door, so this suite drives the real binary: it covers
//! clap flag parsing, dispatch, exit codes, and the scaffold reuse of
//! `dreamd init` in one pass. Every run gets a temp project (root sentinel) and
//! a separate temp `HOME`, so `~/.agent/registry.toml` and the tracing log land
//! off the project tree. No network, no daemon.
//!
//! The MCP merge writer has its own suite (`setup_mcp_merge.rs`); the runs here
//! opt out of it with `--harness none --no-write-mcp`, so this file asserts the
//! scaffold, the skip paths, and the exit-code contract. No wizard runs
//! (AILAB-551).

use std::path::Path;
use std::process::{Command, Output};

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

fn run_setup(project: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(dreamd_bin())
        .arg("setup")
        .args(args)
        .current_dir(project)
        .env("HOME", home)
        .output()
        .expect("run dreamd setup")
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn non_interactive_setup_scaffolds_agent_dir() {
    let (project, home) = project_and_home();

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "none", "--no-write-mcp"],
    );

    assert!(
        out.status.success(),
        "exit={:?}\nstderr={}\nstdout={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
        stdout_of(&out)
    );

    // Scaffold came from init's internals — same tree, same registry entry.
    assert!(
        project
            .path()
            .join(".agent/episodic/AGENT_LEARNINGS.jsonl")
            .exists(),
        "setup must scaffold the episodic log"
    );
    assert!(
        project.path().join(".agent/.dreamd/state.json").exists(),
        "setup must scaffold state.json"
    );
    let registry = std::fs::read_to_string(home.path().join(".agent/registry.toml"))
        .expect("setup must register the project");
    assert!(
        registry.contains("root ="),
        "registry must hold this project; got: {registry:?}"
    );

    // The MCP write was skipped by request (--harness none --no-write-mcp).
    assert!(
        !project.path().join(".mcp.json").exists(),
        "--no-write-mcp must skip .mcp.json"
    );
    assert!(
        !project.path().join(".cursor/mcp.json").exists(),
        "--no-write-mcp must skip .cursor/mcp.json"
    );

    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("harness: none"),
        "must echo the resolved harness; got: {stdout:?}"
    );
    assert!(
        stdout.contains("--no-write-mcp"),
        "must say the MCP config write was skipped by request; got: {stdout:?}"
    );
    assert!(
        stdout.contains("dreamd watch"),
        "next steps must name the daemon command; got: {stdout:?}"
    );
    assert!(
        stdout.contains("search_nodes"),
        "next steps must name the verify beat; got: {stdout:?}"
    );
}

#[test]
fn setup_is_idempotent_on_rerun() {
    let (project, home) = project_and_home();
    let args = ["--yes", "--harness", "none", "--no-write-mcp"];

    let first = run_setup(project.path(), home.path(), &args);
    assert!(first.status.success(), "first setup must succeed");

    let second = run_setup(project.path(), home.path(), &args);
    assert!(
        second.status.success(),
        "rerun must exit 0; exit={:?} stderr={}",
        second.status.code(),
        String::from_utf8_lossy(&second.stderr)
    );
    let stdout = stdout_of(&second);
    assert!(
        stdout.contains("already"),
        "rerun must report the store already exists; got: {stdout:?}"
    );
    assert!(
        stdout.contains("dreamd watch"),
        "rerun must still print next steps; got: {stdout:?}"
    );
}

#[test]
fn dry_run_writes_nothing() {
    let (project, home) = project_and_home();

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude", "--dry-run"],
    );

    assert!(out.status.success(), "dry-run must exit 0");
    assert!(
        !project.path().join(".agent").exists(),
        "dry-run must not scaffold .agent/"
    );
    assert!(
        !project.path().join(".mcp.json").exists(),
        "dry-run must not write .mcp.json"
    );
    assert!(
        !home.path().join(".agent/registry.toml").exists(),
        "dry-run must not register the project"
    );
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("dry-run"),
        "dry-run must label its plan; got: {stdout:?}"
    );
}

#[test]
fn no_project_root_exits_two_without_scaffolding() {
    // No .git / Cargo.toml / package.json / pyproject.toml anywhere up-tree.
    let bare = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let out = run_setup(bare.path(), home.path(), &["--yes", "--harness", "none"]);

    assert_eq!(
        out.status.code(),
        Some(2),
        "missing project root is a usage error (exit 2); stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !bare.path().join(".agent").exists(),
        ".agent/ must not be created on failure"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no project root"),
        "stderr must explain the failure; got: {stderr}"
    );
}
