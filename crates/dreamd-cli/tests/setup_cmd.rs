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

/// `DREAMD_LAUNCHER` is cleared so a developer's shell cannot change which
/// command the hints name.
fn run_setup(project: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(dreamd_bin())
        .arg("setup")
        .args(args)
        .current_dir(project)
        .env("HOME", home)
        .env_remove("DREAMD_LAUNCHER")
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
    assert!(
        !project.path().join("CLAUDE.md").exists(),
        "--harness none must not write CLAUDE.md"
    );
    assert!(
        !project
            .path()
            .join(".cursor/rules/dreamd-recall.mdc")
            .exists(),
        "--harness none must not write the Cursor rule"
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
        !project.path().join("CLAUDE.md").exists(),
        "dry-run must not write CLAUDE.md"
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
    assert!(
        stdout.contains("dry-run: would merge a dreamd block into CLAUDE.md"),
        "dry-run must name the Claude habit file; got: {stdout:?}"
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

// ---------------------------------------------------------------------------
// DRM-153: with an `AGENTS.md` at the root, the dreamd block goes there and
// `CLAUDE.md` imports it, so Claude Code keeps loading `AGENTS.md`.
// LP-INSTALL-09: `--harness none` names the project root.
// ---------------------------------------------------------------------------

/// The marked `CLAUDE.md` block, byte for byte (unchanged by DRM-153).
const CLAUDE_BLOCK: &str = "<!-- dreamd:start -->
At the start of a task, call `search_nodes` with the user's request as the query.

If a hit overlaps the task, quote one line of its content and its id in the reply. If the search is empty, say nothing about memory.

When the user corrects you or states a decision, call `append_node` with those same words in `content`, `source_harness` set to \"claude-code\", and a `skill_action` of lowercase segments joined by `::`.
<!-- dreamd:end -->
";

/// The marked `AGENTS.md` block, byte for byte.
const AGENTS_BLOCK: &str = "<!-- dreamd:start -->
At the start of a task, call `search_nodes` with the user's request as the query.

If a hit overlaps the task, quote one line of its content and its id in the reply. If the search is empty, say nothing about memory.

When the user corrects you or states a decision, call `append_node` with those same words in `content`, `source_harness` set to the name of the tool you run in, such as \"claude-code\", \"codex\" or \"cursor\", and a `skill_action` of lowercase segments joined by `::`.
<!-- dreamd:end -->
";

const AGENTS_ROW: &str = "  habit        AGENTS.md  (dreamd block)";
const CLAUDE_ROW: &str = "  habit        CLAUDE.md  (dreamd block)";
const IMPORT_ROW: &str = "  habit        CLAUDE.md  (imports AGENTS.md)";
const REMOVED_ROW: &str = "  habit        CLAUDE.md  (dreamd block removed)";
const NO_IMPORT_NOTE: &str = "  Note: CLAUDE.md does not import AGENTS.md, so Claude Code does not load AGENTS.md here. Add a line `@AGENTS.md` to CLAUDE.md to load it.";

/// `dreamd setup` with its own `HOME`, `XDG_CONFIG_HOME` and `DREAMD_SOCK`,
/// all under `home`, so a run can reach no real daemon, config or registry.
fn run_setup_isolated(project: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(dreamd_bin())
        .arg("setup")
        .args(args)
        .current_dir(project)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("xdg"))
        .env("DREAMD_SOCK", home.join("d.sock"))
        .env_remove("DREAMD_LAUNCHER")
        .output()
        .expect("run dreamd setup")
}

fn assert_success(out: &Output) {
    assert!(
        out.status.success(),
        "exit={:?}\nstderr={}\nstdout={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
        stdout_of(out)
    );
}

/// A root file's bytes, or `None` when it is absent.
fn read_root_file(project: &Path, name: &str) -> Option<String> {
    match std::fs::read_to_string(project.join(name)) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => panic!("read {name}: {e}"),
    }
}

/// Write `text` to a root file, or leave the file absent for `None`.
fn seed_root_file(project: &Path, name: &str, text: Option<&str>) {
    if let Some(text) = text {
        std::fs::write(project.join(name), text).unwrap();
    }
}

fn habit_rows(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter(|l| l.starts_with("  habit"))
        .collect()
}

/// One case of §3.1: the two files before, the two files after, and the
/// card's habit rows on the first run and on a re-run.
struct Row {
    name: &'static str,
    agents_before: Option<&'static str>,
    claude_before: Option<String>,
    agents_after: Option<String>,
    claude_after: Option<String>,
    rows_first: &'static [&'static str],
    rows_rerun: &'static [&'static str],
    note: bool,
}

fn rows() -> Vec<Row> {
    vec![
        Row {
            name: "1: no AGENTS.md, no CLAUDE.md",
            agents_before: None,
            claude_before: None,
            agents_after: None,
            claude_after: Some(CLAUDE_BLOCK.to_string()),
            rows_first: &[CLAUDE_ROW],
            rows_rerun: &[CLAUDE_ROW],
            note: false,
        },
        Row {
            name: "1: no AGENTS.md, a CLAUDE.md of the user's",
            agents_before: None,
            claude_before: Some("# Mine\n".to_string()),
            agents_after: None,
            claude_after: Some(format!("# Mine\n\n{CLAUDE_BLOCK}")),
            rows_first: &[CLAUDE_ROW],
            rows_rerun: &[CLAUDE_ROW],
            note: false,
        },
        Row {
            name: "2: AGENTS.md, no CLAUDE.md",
            agents_before: Some("# Agents\n\nBe kind.\n"),
            claude_before: None,
            agents_after: Some(format!("# Agents\n\nBe kind.\n\n{AGENTS_BLOCK}")),
            claude_after: Some("@AGENTS.md\n".to_string()),
            rows_first: &[AGENTS_ROW, IMPORT_ROW],
            rows_rerun: &[AGENTS_ROW, IMPORT_ROW],
            note: false,
        },
        Row {
            name: "3: CLAUDE.md imports AGENTS.md and holds a block",
            agents_before: Some("# Agents\n"),
            claude_before: Some(format!("@AGENTS.md\n\n# Claude only\n\n{CLAUDE_BLOCK}")),
            agents_after: Some(format!("# Agents\n\n{AGENTS_BLOCK}")),
            claude_after: Some("@AGENTS.md\n\n# Claude only\n".to_string()),
            rows_first: &[AGENTS_ROW, REMOVED_ROW],
            rows_rerun: &[AGENTS_ROW],
            note: false,
        },
        Row {
            name: "3: CLAUDE.md imports ./AGENTS.md, no block",
            agents_before: Some("# Agents\n"),
            claude_before: Some("  @./AGENTS.md\n\n# Claude only\n".to_string()),
            agents_after: Some(format!("# Agents\n\n{AGENTS_BLOCK}")),
            claude_after: Some("  @./AGENTS.md\n\n# Claude only\n".to_string()),
            rows_first: &[AGENTS_ROW],
            rows_rerun: &[AGENTS_ROW],
            note: false,
        },
        Row {
            name: "4: CLAUDE.md holds nothing but a dreamd block (setup's own file)",
            agents_before: Some("# Agents\n"),
            claude_before: Some(CLAUDE_BLOCK.to_string()),
            agents_after: Some(format!("# Agents\n\n{AGENTS_BLOCK}")),
            claude_after: Some("@AGENTS.md\n".to_string()),
            rows_first: &[AGENTS_ROW, IMPORT_ROW],
            rows_rerun: &[AGENTS_ROW, IMPORT_ROW],
            note: false,
        },
        Row {
            name: "4: an empty CLAUDE.md",
            agents_before: Some("# Agents\n"),
            claude_before: Some(String::new()),
            agents_after: Some(format!("# Agents\n\n{AGENTS_BLOCK}")),
            claude_after: Some("@AGENTS.md\n".to_string()),
            rows_first: &[AGENTS_ROW, IMPORT_ROW],
            rows_rerun: &[AGENTS_ROW, IMPORT_ROW],
            note: false,
        },
        Row {
            name: "4: a CLAUDE.md of nothing but a blank line and a dreamd block",
            agents_before: Some("# Agents\n"),
            claude_before: Some(format!("\n\n{CLAUDE_BLOCK}")),
            agents_after: Some(format!("# Agents\n\n{AGENTS_BLOCK}")),
            claude_after: Some("@AGENTS.md\n".to_string()),
            rows_first: &[AGENTS_ROW, IMPORT_ROW],
            rows_rerun: &[AGENTS_ROW, IMPORT_ROW],
            note: false,
        },
        Row {
            name: "4: a CLAUDE.md of the user's that does not import AGENTS.md",
            agents_before: Some("# Agents\n"),
            claude_before: Some("# Mine\nsee @AGENTS.md here\n".to_string()),
            agents_after: Some("# Agents\n".to_string()),
            claude_after: Some(format!("# Mine\nsee @AGENTS.md here\n\n{CLAUDE_BLOCK}")),
            rows_first: &[CLAUDE_ROW],
            rows_rerun: &[CLAUDE_ROW],
            note: true,
        },
    ]
}

/// Run `setup --yes --harness claude` `runs` times on a fresh project seeded
/// with `row`'s files, and check both files and the card after every run.
fn check_row(row: &Row, runs: usize) {
    let (project, home) = project_and_home();
    seed_root_file(project.path(), "AGENTS.md", row.agents_before);
    seed_root_file(project.path(), "CLAUDE.md", row.claude_before.as_deref());
    for run in 1..=runs {
        let out = run_setup_isolated(
            project.path(),
            home.path(),
            &["--yes", "--harness", "claude"],
        );
        assert_success(&out);
        let stdout = stdout_of(&out);
        assert_eq!(
            read_root_file(project.path(), "AGENTS.md"),
            row.agents_after,
            "{} (run {run}): AGENTS.md",
            row.name
        );
        assert_eq!(
            read_root_file(project.path(), "CLAUDE.md"),
            row.claude_after,
            "{} (run {run}): CLAUDE.md",
            row.name
        );
        let want = if run == 1 {
            row.rows_first
        } else {
            row.rows_rerun
        };
        assert_eq!(
            habit_rows(&stdout),
            want,
            "{} (run {run}): {stdout}",
            row.name
        );
        assert_eq!(
            stdout.contains(NO_IMPORT_NOTE),
            row.note,
            "{} (run {run}): {stdout}",
            row.name
        );
    }
}

/// [`check_row`] for every row. A failing row does not stop the others, and
/// the test fails once, naming each row that failed.
fn check_rows(runs: usize) {
    let failed: Vec<&str> = rows()
        .iter()
        .filter(|row| std::panic::catch_unwind(|| check_row(row, runs)).is_err())
        .map(|row| row.name)
        .collect();
    assert!(failed.is_empty(), "rows that failed: {failed:#?}");
}

/// DRM-153 test 5: each case of §3.1, both files byte for byte.
#[test]
fn setup_writes_each_case_of_the_habit_table() {
    check_rows(1);
}

/// DRM-153 test 6: a re-run of each case leaves both files byte-identical.
#[test]
fn a_rerun_leaves_every_habit_file_byte_identical() {
    check_rows(2);
}

/// DRM-153 test 7: `--dry-run` prints the habit plan and changes nothing.
#[test]
fn dry_run_prints_the_habit_plan_and_changes_nothing() {
    for (name, agents, claude, want) in [
        (
            "2",
            "# Agents\n",
            None,
            &[
                "dry-run: would merge a dreamd block into AGENTS.md",
                "dry-run: would create CLAUDE.md with the line @AGENTS.md",
            ][..],
        ),
        (
            "3",
            "# Agents\n",
            Some(format!("@AGENTS.md\n\n{CLAUDE_BLOCK}")),
            &[
                "dry-run: would merge a dreamd block into AGENTS.md",
                "dry-run: would remove the dreamd block from CLAUDE.md",
            ][..],
        ),
        (
            "4, setup's own file",
            "# Agents\n",
            Some(CLAUDE_BLOCK.to_string()),
            &[
                "dry-run: would merge a dreamd block into AGENTS.md",
                "dry-run: would replace CLAUDE.md with the line @AGENTS.md",
            ][..],
        ),
        (
            "4, the user's file",
            "# Agents\n",
            Some("# Mine\n".to_string()),
            &[
                "dry-run: would merge a dreamd block into CLAUDE.md",
                "note: CLAUDE.md does not import AGENTS.md, so Claude Code does not load AGENTS.md here. Add a line `@AGENTS.md` to CLAUDE.md to load it.",
            ][..],
        ),
    ] {
        let (project, home) = project_and_home();
        seed_root_file(project.path(), "AGENTS.md", Some(agents));
        seed_root_file(project.path(), "CLAUDE.md", claude.as_deref());
        let out = run_setup_isolated(
            project.path(),
            home.path(),
            &["--yes", "--harness", "claude", "--dry-run"],
        );
        assert_success(&out);
        let stdout = stdout_of(&out);
        let plan: Vec<&str> = stdout
            .lines()
            .filter(|l| l.contains("AGENTS.md") || l.contains("CLAUDE.md"))
            .collect();
        assert_eq!(plan, want, "row {name}: {stdout}");
        assert_eq!(
            read_root_file(project.path(), "AGENTS.md").as_deref(),
            Some(agents),
            "row {name}"
        );
        assert_eq!(read_root_file(project.path(), "CLAUDE.md"), claude, "row {name}");
        assert!(!project.path().join(".agent").exists(), "row {name}");
    }
}

/// DRM-153 test 8: `--harness cursor` touches neither `AGENTS.md` nor
/// `CLAUDE.md`.
#[test]
fn harness_cursor_leaves_agents_md_and_claude_md_alone() {
    for claude in [None, Some("# Mine\n"), Some(CLAUDE_BLOCK)] {
        let (project, home) = project_and_home();
        seed_root_file(project.path(), "AGENTS.md", Some("# Agents\n"));
        seed_root_file(project.path(), "CLAUDE.md", claude);
        let out = run_setup_isolated(
            project.path(),
            home.path(),
            &["--yes", "--harness", "cursor"],
        );
        assert_success(&out);
        assert_eq!(
            read_root_file(project.path(), "AGENTS.md").as_deref(),
            Some("# Agents\n")
        );
        assert_eq!(
            read_root_file(project.path(), "CLAUDE.md").as_deref(),
            claude
        );
        let stdout = stdout_of(&out);
        assert_eq!(
            habit_rows(&stdout),
            ["  habit        .cursor/rules/dreamd-recall.mdc"],
            "{stdout}"
        );
    }
}

/// DRM-153 test 9 (LP-INSTALL-09): under `--harness none` the MCP block in
/// the card and in the dry-run steps names the project root, JSON-escaped.
#[cfg(unix)]
#[test]
fn harness_none_names_the_project_root() {
    let parent = tempfile::tempdir().unwrap();
    let project = parent.path().join("my \"dreamd\" project");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    let home = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(&project).unwrap();
    let escaped = root.to_str().unwrap().replace('"', "\\\"");
    let block = format!(
        r#"{{"mcpServers":{{"dreamd":{{"command":"npx","args":["-y","dreamd-mcp","--project-root","{escaped}"]}}}}}}"#
    );

    let dry = run_setup_isolated(
        &project,
        home.path(),
        &["--yes", "--harness", "none", "--dry-run"],
    );
    assert_success(&dry);
    let stdout = stdout_of(&dry);
    let steps = format!(
        "  2. add the dreamd MCP server to your harness MCP config.\n     That config is global, so the block names this project's root:\n       {block}\n"
    );
    assert!(stdout.contains(&steps), "{stdout}");

    let real = run_setup_isolated(&project, home.path(), &["--yes", "--harness", "none"]);
    assert_success(&real);
    let stdout = stdout_of(&real);
    let next = format!(
        "  Next: add the dreamd MCP server to your harness MCP config.\n        That config is global, so the block names this project's root:\n          {block}\n        then reload your harness and ask your agent:\n"
    );
    assert!(stdout.contains(&next), "{stdout}");
    assert!(!project.join("CLAUDE.md").exists());
    assert!(!project.join("AGENTS.md").exists());
}

/// A `CLAUDE.md` that is a link to `AGENTS.md`, or the other way round, is
/// one file: setup merges one block into it, as before DRM-153, and never
/// writes `@AGENTS.md` over the shared text.
#[cfg(unix)]
#[test]
fn a_claude_md_linked_to_agents_md_gets_one_block() {
    for (target, link, before) in [
        ("AGENTS.md", "CLAUDE.md", "# Shared\n"),
        ("CLAUDE.md", "AGENTS.md", CLAUDE_BLOCK),
    ] {
        let (project, home) = project_and_home();
        std::fs::write(project.path().join(target), before).unwrap();
        std::os::unix::fs::symlink(target, project.path().join(link)).unwrap();
        for run in 1..=2 {
            let out = run_setup_isolated(
                project.path(),
                home.path(),
                &["--yes", "--harness", "claude"],
            );
            assert_success(&out);
            let want = if before == CLAUDE_BLOCK {
                CLAUDE_BLOCK.to_string()
            } else {
                format!("{before}\n{CLAUDE_BLOCK}")
            };
            assert_eq!(
                read_root_file(project.path(), target),
                Some(want),
                "{link} -> {target} (run {run})"
            );
            assert!(
                std::fs::symlink_metadata(project.path().join(link))
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{link} stays a link"
            );
            let stdout = stdout_of(&out);
            assert_eq!(habit_rows(&stdout), [CLAUDE_ROW], "{stdout}");
        }
    }
}
