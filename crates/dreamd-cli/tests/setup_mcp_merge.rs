//! Integration tests for the `dreamd setup` MCP merge writer (AILAB-550).
//!
//! Drives the real binary so clap flags, exit codes, and the on-disk merge are
//! covered in one pass. Every run gets a temp project (`.git` sentinel) plus a
//! separate temp `HOME`, so neither the real home nor the real repo is touched.
//!
//! The conflict taxonomy under test is ratified in AILAB-548 §3: an existing
//! `mcpServers.dreamd` block is compatible iff `command == "npx"` AND some
//! `args` element is exactly `dreamd-mcp`.

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

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Write `contents` to `project/rel`, creating parent dirs.
fn seed(project: &Path, rel: &str, contents: &str) {
    let path = project.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn read(project: &Path, rel: &str) -> String {
    std::fs::read_to_string(project.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

fn json_of(project: &Path, rel: &str) -> serde_json::Value {
    serde_json::from_str(&read(project, rel)).expect("written config must be valid JSON")
}

/// Exactly what the writer must emit for a fresh `.mcp.json`: 2-space pretty
/// (serde_json orders object keys) plus a trailing newline.
const CLEAN_MCP_JSON: &str = r#"{
  "mcpServers": {
    "dreamd": {
      "args": [
        "-y",
        "dreamd-mcp"
      ],
      "command": "npx"
    }
  }
}
"#;

fn floating_block() -> serde_json::Value {
    serde_json::json!({"command": "npx", "args": ["-y", "dreamd-mcp"]})
}

fn assert_exit(out: &Output, code: i32) {
    assert_eq!(
        out.status.code(),
        Some(code),
        "expected exit {code}\nstdout={}\nstderr={}",
        stdout_of(out),
        stderr_of(out)
    );
}

#[test]
fn claude_harness_creates_mcp_json_with_floating_block() {
    let (project, home) = project_and_home();

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude"],
    );

    assert_exit(&out, 0);
    assert_eq!(
        json_of(project.path(), ".mcp.json")["mcpServers"]["dreamd"],
        floating_block(),
        "setup must wire the floating npx block"
    );
    assert!(
        !project.path().join(".cursor/mcp.json").exists(),
        "--harness claude must not touch the cursor config"
    );
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains(".mcp.json"),
        "stdout must report the written target; got: {stdout:?}"
    );
}

#[test]
fn written_json_is_two_space_pretty_with_trailing_newline() {
    let (project, home) = project_and_home();

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude"],
    );

    assert_exit(&out, 0);
    assert_eq!(
        read(project.path(), ".mcp.json"),
        CLEAN_MCP_JSON,
        "AILAB-548 §6B: 2-space pretty + trailing newline"
    );
}

#[test]
fn both_harness_writes_claude_and_cursor_targets() {
    let (project, home) = project_and_home();

    let out = run_setup(project.path(), home.path(), &["--yes", "--harness", "both"]);

    assert_exit(&out, 0);
    assert_eq!(read(project.path(), ".mcp.json"), CLEAN_MCP_JSON);
    assert_eq!(
        read(project.path(), ".cursor/mcp.json"),
        CLEAN_MCP_JSON,
        "the writer must create .cursor/ as needed"
    );
}

#[test]
fn unrelated_server_is_preserved_on_merge() {
    let (project, home) = project_and_home();
    seed(
        project.path(),
        ".mcp.json",
        r#"{"mcpServers":{"other":{"command":"node","args":["server.js"]}},"someTopLevelKey":42}"#,
    );

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude"],
    );

    assert_exit(&out, 0);
    let doc = json_of(project.path(), ".mcp.json");
    assert_eq!(doc["mcpServers"]["dreamd"], floating_block());
    assert_eq!(
        doc["mcpServers"]["other"],
        serde_json::json!({"command": "node", "args": ["server.js"]}),
        "sibling servers must survive the merge"
    );
    assert_eq!(
        doc["someTopLevelKey"],
        serde_json::json!(42),
        "other top-level keys must survive the merge"
    );
}

#[test]
fn file_without_mcp_servers_key_gains_one() {
    let (project, home) = project_and_home();
    seed(project.path(), ".mcp.json", "{\"unrelated\": true}\n");

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude"],
    );

    assert_exit(&out, 0);
    let doc = json_of(project.path(), ".mcp.json");
    assert_eq!(doc["mcpServers"]["dreamd"], floating_block());
    assert_eq!(doc["unrelated"], serde_json::json!(true));
}

#[test]
fn hard_pinned_entry_conflicts_without_force() {
    let (project, home) = project_and_home();
    let pinned = "{\"mcpServers\":{\"dreamd\":{\"command\":\"npx\",\"args\":[\"-y\",\"dreamd-mcp@1.2.3\"]}}}";
    seed(project.path(), ".mcp.json", pinned);

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude"],
    );

    assert_exit(&out, 1);
    assert_eq!(
        read(project.path(), ".mcp.json"),
        pinned,
        "a refused merge must leave the file byte-identical"
    );
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains(".mcp.json"),
        "stderr must name the offending file; got: {stderr}"
    );
    assert!(
        stderr.contains("--force"),
        "stderr must offer the --force remedy; got: {stderr}"
    );
    assert!(
        stderr.contains("nothing was written"),
        "stderr must say nothing was written; got: {stderr}"
    );
}

#[test]
fn force_replaces_pinned_entry_and_keeps_siblings() {
    let (project, home) = project_and_home();
    seed(
        project.path(),
        ".mcp.json",
        "{\"mcpServers\":{\"dreamd\":{\"command\":\"npx\",\"args\":[\"-y\",\"dreamd-mcp@1.2.3\"]},\"other\":{\"command\":\"node\"}}}",
    );

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude", "--force"],
    );

    assert_exit(&out, 0);
    let doc = json_of(project.path(), ".mcp.json");
    assert_eq!(
        doc["mcpServers"]["dreamd"],
        floating_block(),
        "--force must replace the dreamd entry with the floating pin"
    );
    assert_eq!(
        doc["mcpServers"]["other"],
        serde_json::json!({"command": "node"}),
        "--force must not disturb other servers"
    );
}

#[test]
fn non_npx_command_conflicts_then_force_replaces() {
    let (project, home) = project_and_home();
    let existing =
        "{\"mcpServers\":{\"dreamd\":{\"command\":\"/usr/local/bin/dreamd\",\"args\":[\"mcp\"]}}}";
    seed(project.path(), ".mcp.json", existing);

    let refused = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude"],
    );
    assert_exit(&refused, 1);
    assert_eq!(
        read(project.path(), ".mcp.json"),
        existing,
        "refused merge leaves the file untouched"
    );

    let forced = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude", "--force"],
    );
    assert_exit(&forced, 0);
    assert_eq!(
        json_of(project.path(), ".mcp.json")["mcpServers"]["dreamd"],
        floating_block()
    );
}

#[test]
fn existing_floating_block_with_project_root_is_a_noop() {
    let (project, home) = project_and_home();
    // Hand-formatted (4-space, different key order) so any rewrite shows up.
    let existing = "{\n    \"mcpServers\": {\n        \"dreamd\": {\n            \"command\": \"npx\",\n            \"args\": [\"-y\", \"dreamd-mcp\", \"--project-root\", \"/x\"]\n        }\n    }\n}\n";
    seed(project.path(), ".mcp.json", existing);

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude"],
    );

    assert_exit(&out, 0);
    assert_eq!(
        read(project.path(), ".mcp.json"),
        existing,
        "a compatible block with extra args must be left byte-identical"
    );
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("already wired"),
        "stdout must report the no-op; got: {stdout:?}"
    );
}

#[test]
fn existing_exact_floating_block_is_already_wired() {
    let (project, home) = project_and_home();
    let existing =
        "{\"mcpServers\":{\"dreamd\":{\"command\":\"npx\",\"args\":[\"-y\",\"dreamd-mcp\"]}}}";
    seed(project.path(), ".mcp.json", existing);

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude"],
    );

    assert_exit(&out, 0);
    assert_eq!(
        read(project.path(), ".mcp.json"),
        existing,
        "an already-wired file must not be reformatted"
    );
    assert!(stdout_of(&out).contains("already wired"));
}

#[test]
fn both_harness_conflict_leaves_the_sibling_untouched() {
    let (project, home) = project_and_home();
    // .mcp.json is clean and pre-existing; .cursor/mcp.json conflicts.
    let clean_sibling = "{\n    \"mcpServers\": {}\n}\n";
    seed(project.path(), ".mcp.json", clean_sibling);
    seed(
        project.path(),
        ".cursor/mcp.json",
        "{\"mcpServers\":{\"dreamd\":{\"command\":\"cargo\",\"args\":[\"run\"]}}}",
    );

    let out = run_setup(project.path(), home.path(), &["--yes", "--harness", "both"]);

    assert_exit(&out, 1);
    assert_eq!(
        read(project.path(), ".mcp.json"),
        clean_sibling,
        "AILAB-548 §6A: a conflict on either target writes NEITHER"
    );
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains(".cursor/mcp.json"),
        "stderr must name the offender; got: {stderr}"
    );
}

#[test]
fn both_harness_conflict_does_not_create_the_missing_sibling() {
    let (project, home) = project_and_home();
    seed(
        project.path(),
        ".cursor/mcp.json",
        "{\"mcpServers\":{\"dreamd\":{\"command\":\"cargo\",\"args\":[\"run\"]}}}",
    );

    let out = run_setup(project.path(), home.path(), &["--yes", "--harness", "both"]);

    assert_exit(&out, 1);
    assert!(
        !project.path().join(".mcp.json").exists(),
        "AILAB-548 §6A: the clean target must not be created when the other conflicts"
    );
}

#[test]
fn malformed_json_refuses_and_force_does_not_help() {
    let (project, home) = project_and_home();
    let broken = "{\"mcpServers\": {,,,}";
    seed(project.path(), ".mcp.json", broken);

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude"],
    );
    assert_exit(&out, 1);
    assert_eq!(
        read(project.path(), ".mcp.json"),
        broken,
        "a file we cannot parse must never be overwritten"
    );
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains(".mcp.json"),
        "stderr must name the file; got: {stderr}"
    );

    let forced = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude", "--force"],
    );
    assert_exit(&forced, 1);
    assert_eq!(
        read(project.path(), ".mcp.json"),
        broken,
        "AILAB-548 §6C: --force does not override a parse failure"
    );
}

#[test]
fn dry_run_reports_conflict_and_writes_nothing() {
    let (project, home) = project_and_home();
    let pinned = "{\"mcpServers\":{\"dreamd\":{\"command\":\"npx\",\"args\":[\"-y\",\"dreamd-mcp@1.2.3\"]}}}";
    seed(project.path(), ".mcp.json", pinned);

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude", "--dry-run"],
    );

    assert_exit(&out, 0);
    assert_eq!(
        read(project.path(), ".mcp.json"),
        pinned,
        "--dry-run writes nothing, ever"
    );
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("CONFLICT"),
        "dry-run must report the conflict on stdout; got: {stdout:?}"
    );
}

#[test]
fn dry_run_prints_the_json_it_would_write() {
    let (project, home) = project_and_home();

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude", "--dry-run"],
    );

    assert_exit(&out, 0);
    assert!(
        !project.path().join(".mcp.json").exists(),
        "--dry-run must not create the config"
    );
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("would create"),
        "dry-run must name the action; got: {stdout:?}"
    );
    assert!(
        stdout.contains("\"dreamd-mcp\""),
        "dry-run must show the JSON it would write; got: {stdout:?}"
    );
}

#[test]
fn no_write_mcp_skips_every_target() {
    let (project, home) = project_and_home();

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "both", "--no-write-mcp"],
    );

    assert_exit(&out, 0);
    assert!(!project.path().join(".mcp.json").exists());
    assert!(!project.path().join(".cursor/mcp.json").exists());
}

#[test]
fn rerun_after_wiring_is_a_noop() {
    let (project, home) = project_and_home();
    let args = ["--yes", "--harness", "both"];

    let first = run_setup(project.path(), home.path(), &args);
    assert_exit(&first, 0);
    let after_first = read(project.path(), ".mcp.json");

    let second = run_setup(project.path(), home.path(), &args);
    assert_exit(&second, 0);
    assert_eq!(
        read(project.path(), ".mcp.json"),
        after_first,
        "setup must be idempotent once wired"
    );
    assert!(stdout_of(&second).contains("already wired"));
}

#[test]
fn force_on_an_already_wired_file_is_a_byte_identical_noop() {
    let (project, home) = project_and_home();
    // Compact + unsorted keys: any rewrite by --force would show up immediately.
    let existing =
        "{\"mcpServers\":{\"dreamd\":{\"command\":\"npx\",\"args\":[\"-y\",\"dreamd-mcp\"]}}}";
    seed(project.path(), ".mcp.json", existing);

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude", "--force"],
    );

    assert_exit(&out, 0);
    assert_eq!(
        read(project.path(), ".mcp.json"),
        existing,
        "AILAB-548 §5: --force is a no-op when there is nothing to force — the \
         AlreadyWired short-circuit must not reformat a file it had no reason to touch"
    );
    assert!(stdout_of(&out).contains("already wired"));
}

/// Deliberate resolution of the one place two AILAB-548 clauses collide:
/// "--dry-run always exits 0" vs "malformed config always exits 1". Malformed
/// wins — a dry run cannot produce a plan for a file it cannot parse, so there
/// is nothing to report and exit 0 would imply a clean run. Do not "fix" this
/// to 0.
#[test]
fn dry_run_on_malformed_json_still_exits_one() {
    let (project, home) = project_and_home();
    let broken = "{\"mcpServers\": {,,,}";
    seed(project.path(), ".mcp.json", broken);

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude", "--dry-run"],
    );

    assert_exit(&out, 1);
    assert_eq!(
        read(project.path(), ".mcp.json"),
        broken,
        "--dry-run writes nothing, ever"
    );
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains(".mcp.json"),
        "stderr must name the unparseable file; got: {stderr}"
    );
}

#[test]
fn cursor_harness_alone_writes_only_the_cursor_target() {
    let (project, home) = project_and_home();

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "cursor"],
    );

    assert_exit(&out, 0);
    assert_eq!(
        read(project.path(), ".cursor/mcp.json"),
        CLEAN_MCP_JSON,
        "--harness cursor must create .cursor/mcp.json with the floating block"
    );
    assert!(
        !project.path().join(".mcp.json").exists(),
        "--harness cursor must not touch the claude config"
    );
    assert!(
        !home.path().join(".cursor/mcp.json").exists(),
        "AILAB-548 §2: the global ~/.cursor/mcp.json is out of scope — setup only writes inside the project"
    );
}

#[test]
fn force_on_a_clean_project_is_a_plain_success() {
    let (project, home) = project_and_home();

    let out = run_setup(
        project.path(),
        home.path(),
        &["--yes", "--harness", "claude", "--force"],
    );

    assert_exit(&out, 0);
    assert_eq!(read(project.path(), ".mcp.json"), CLEAN_MCP_JSON);
}
