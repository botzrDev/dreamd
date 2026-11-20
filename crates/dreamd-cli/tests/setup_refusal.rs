//! DRM-158 (LP-INSTALL-05): a `dreamd setup` that refuses an MCP config
//! writes nothing. Setup reads and classifies every harness config before
//! the scaffold, so a conflicting or malformed `.mcp.json` or
//! `.cursor/mcp.json` leaves no `.agent/`, no `.gitignore` line and no
//! registry entry behind. Before the fix the refusal said "nothing was
//! written" after all three had been made.
//!
//! The tests drive the real binary with no daemon. Every run gets its own
//! `HOME`, an empty `XDG_CONFIG_HOME` inside it, and a `DREAMD_SOCK` path in
//! a separate folder that nothing listens on. It also sets `DREAMD_LOG=off`,
//! `GIT_CONFIG_NOSYSTEM=1` and `GIT_CONFIG_GLOBAL=/dev/null`, and removes
//! `DREAMD_LAUNCHER` and `DREAMD_LOG_FORMAT`. The test process's own
//! environment is never changed. "Nothing written" is checked by
//! snapshotting the project and `HOME` (every file's path and bytes, and
//! every directory's path) before and after.
//! Unix-only: `DREAMD_SOCK` is a Unix socket.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A `dreamd` entry that is not `npx -y dreamd-mcp`: a conflict.
const CONFLICTING: &str = "{\"mcpServers\":{\"dreamd\":{\"command\":\"/opt/dreamd\",\"args\":[]}}}";
/// Not JSON: a malformed config, which `--force` cannot override.
const MALFORMED: &str = "{not json";

fn dreamd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dreamd")
}

/// One test's project (with a `.git` sentinel), `HOME` and socket folder.
struct Env {
    project: tempfile::TempDir,
    home: tempfile::TempDir,
    sock_dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let env = Self {
            project: tempfile::tempdir().expect("project tempdir"),
            home: tempfile::tempdir().expect("home tempdir"),
            sock_dir: tempfile::tempdir().expect("socket tempdir"),
        };
        std::fs::create_dir(env.project().join(".git")).expect("project sentinel");
        std::fs::create_dir(env.xdg()).expect("XDG_CONFIG_HOME");
        env
    }

    fn project(&self) -> &Path {
        self.project.path()
    }

    fn xdg(&self) -> PathBuf {
        self.home.path().join(".config")
    }

    /// Write `contents` to `project/rel`, creating parent directories.
    fn seed(&self, rel: &str, contents: &str) {
        let path = self.project().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("seed parent");
        }
        std::fs::write(path, contents).expect("seed file");
    }

    /// Run `dreamd setup args` in the project.
    fn setup(&self, args: &[&str]) -> Output {
        Command::new(dreamd_bin())
            .arg("setup")
            .args(args)
            .current_dir(self.project())
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.xdg())
            .env("DREAMD_SOCK", self.sock_dir.path().join("d.sock"))
            .env("DREAMD_LOG", "off")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("DREAMD_LAUNCHER")
            .env_remove("DREAMD_LOG_FORMAT")
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|e| panic!("run dreamd setup {args:?}: {e}"))
    }

    /// The project and `HOME`, each as a sorted tree snapshot.
    fn snapshot(&self) -> (Vec<Entry>, Vec<Entry>) {
        (tree(self.project()), tree(self.home.path()))
    }
}

/// One entry of a tree snapshot, by path relative to the tree's root.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Entry {
    Dir(PathBuf),
    File(PathBuf, Vec<u8>),
    Link(PathBuf, PathBuf),
}

/// Every directory, file (with its bytes) and symlink under `root`, sorted.
fn tree(root: &Path) -> Vec<Entry> {
    fn walk(root: &Path, dir: &Path, entries: &mut Vec<Entry>) {
        for entry in
            std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        {
            let path = entry.expect("dir entry").path();
            let rel = path.strip_prefix(root).expect("under root").to_path_buf();
            let kind = std::fs::symlink_metadata(&path)
                .expect("metadata")
                .file_type();
            if kind.is_symlink() {
                entries.push(Entry::Link(
                    rel,
                    std::fs::read_link(&path).expect("read link"),
                ));
            } else if kind.is_dir() {
                entries.push(Entry::Dir(rel));
                walk(root, &path, entries);
            } else {
                entries.push(Entry::File(rel, std::fs::read(&path).expect("read file")));
            }
        }
    }
    let mut entries = Vec::new();
    walk(root, root, &mut entries);
    entries.sort();
    entries
}

fn describe(out: &Output) -> String {
    format!(
        "exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The run exits 1, says nothing was written, and the project and `HOME`
/// are exactly as they were before it.
fn assert_refused_and_unchanged(env: &Env, args: &[&str], what: &str) {
    let before = env.snapshot();
    let out = env.setup(args);
    assert_eq!(out.status.code(), Some(1), "{what}: {}", describe(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("nothing was written"),
        "{what}: stderr must say nothing was written: {stderr}"
    );
    let (project, home) = env.snapshot();
    assert_eq!(project, before.0, "{what}: the project must be unchanged");
    assert_eq!(
        home, before.1,
        "{what}: HOME must be unchanged (no registry)"
    );
}

/// DRM-158 test 1: a conflicting `.mcp.json` refuses before the scaffold.
#[test]
fn a_conflicting_mcp_json_writes_nothing() {
    let env = Env::new();
    env.seed(".gitignore", "target/\n");
    env.seed(".mcp.json", CONFLICTING);

    assert_refused_and_unchanged(&env, &["--yes", "--harness", "claude"], "conflict");
    assert!(!env.project().join(".agent").exists(), "no .agent/");
}

/// DRM-158 test 2: a malformed `.mcp.json` refuses before the scaffold, with
/// and without `--force`.
#[test]
fn a_malformed_mcp_json_writes_nothing() {
    let env = Env::new();
    env.seed(".gitignore", "target/\n");
    env.seed(".mcp.json", MALFORMED);

    assert_refused_and_unchanged(&env, &["--yes", "--harness", "claude"], "malformed");
    assert_refused_and_unchanged(
        &env,
        &["--yes", "--harness", "claude", "--force"],
        "malformed under --force",
    );
    assert!(!env.project().join(".agent").exists(), "no .agent/");
}

/// DRM-158 test 3: under `--harness both`, a conflicting `.cursor/mcp.json`
/// refuses for both targets: the missing `.mcp.json` (a `Create`) is not
/// made, and neither is the store.
#[test]
fn both_harnesses_refuse_together() {
    let env = Env::new();
    env.seed(".gitignore", "target/\n");
    env.seed(".cursor/mcp.json", CONFLICTING);

    assert_refused_and_unchanged(&env, &["--yes", "--harness", "both"], "both");
    assert!(!env.project().join(".mcp.json").exists(), "no .mcp.json");
    assert!(!env.project().join(".agent").exists(), "no .agent/");
}

/// DRM-158 test 4: after a refusal, a `--force` rerun makes every step
/// itself, so its card lists every one of them under `Undo:` and none as
/// already there. The first run asserts only its refusal: tests 1-3 check
/// that a refusal writes nothing, and this test's own asserts are the card's.
#[test]
fn force_after_a_refusal_lists_every_undo_step() {
    let env = Env::new();
    env.seed(".gitignore", "target/\n");
    env.seed(".mcp.json", CONFLICTING);
    let refused = env.setup(&["--yes", "--harness", "claude"]);
    assert_eq!(
        refused.status.code(),
        Some(1),
        "first run: {}",
        describe(&refused)
    );

    let out = env.setup(&["--yes", "--harness", "claude", "--force"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "forced rerun: {}",
        describe(&out)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("(already there)"),
        "the forced rerun made the store and the registry entry: {stdout}"
    );
    let undo: Vec<&str> = stdout
        .split("\n  Undo:\n")
        .nth(1)
        .unwrap_or_else(|| panic!("no Undo: block: {stdout}"))
        .lines()
        .take_while(|line| !line.is_empty())
        .map(str::trim)
        .collect();
    for step in [
        "Remove the /.agent/.dreamd/ line from .gitignore.",
        "Delete .agent/ last: it holds this project's memory.",
    ] {
        assert!(undo.contains(&step), "Undo: must list {step:?}: {undo:#?}");
    }
}
