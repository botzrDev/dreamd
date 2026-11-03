//! BZR-1467: after a fresh setup a dream cycle makes its git commit, and a real
//! edit holds the commit until the user commits it, with a notice that names
//! the file.
//!
//! Subprocess tests in the shape of `table_control_chars.rs` (raw
//! `std::process::Command` + `CARGO_BIN_EXE_dreamd`, `HOME` a tempdir,
//! `DREAMD_LOG=off`). The repo is built and read with git2, never the `git`
//! binary, and the test process's environment is never changed: each child
//! drops `DREAMD_SOCK` and `SOURCE_DATE_EPOCH`, so `dreamd dream` runs in
//! process and dates the cycle by the wall clock. Unix-only: the in-process
//! cycle takes the project writer lock.
#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output};

const LOG: &str = ".agent/episodic/AGENT_LEARNINGS.jsonl";

/// `autobiography::skip_message("user_dirty_tree", [LOG])`, spelled out so the
/// file also builds against a tree that lacks the function (the red-first run).
const HELD_NOTICE: &str = "no git commit for this cycle: .agent/episodic/AGENT_LEARNINGS.jsonl \
                           changed outside a dream cycle. Commit those changes with git, and \
                           the next cycle commits again.";

/// A git project and a separate daemon home, both temporary.
struct Project {
    dir: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl Project {
    fn repo(&self) -> git2::Repository {
        git2::Repository::open(self.dir.path()).unwrap()
    }

    fn head_id(&self) -> git2::Oid {
        self.repo().head().unwrap().peel_to_commit().unwrap().id()
    }

    fn head_message(&self) -> String {
        let repo = self.repo();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        head.message().unwrap().to_string()
    }
}

fn dreamd(project: &Project, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dreamd"))
        .args(args)
        .current_dir(project.dir.path())
        .env("HOME", project.home.path())
        .env("DREAMD_LOG", "off")
        .env_remove("DREAMD_SOCK")
        .env_remove("SOURCE_DATE_EPOCH")
        .output()
        .unwrap_or_else(|e| panic!("run dreamd {args:?}: {e}"))
}

/// Run `args`, require exit 0, and return stdout.
fn stdout_ok(project: &Project, args: &[&str]) -> String {
    let out = dreamd(project, args);
    assert!(
        out.status.success(),
        "dreamd {args:?} exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// One episodic record line, stamped now.
fn record(id: &str, content: &str) -> String {
    let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut line = serde_json::json!({
        "schema_version": "1.0.0",
        "id": id,
        "timestamp": ts,
        "pain": 6.0,
        "importance": 6.0,
        "skill_action": "rust::autobiography",
        "source_harness": "claude-code",
        "content": content,
    })
    .to_string();
    line.push('\n');
    line
}

fn append(path: &Path, bytes: &str) {
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes.as_bytes())
        .unwrap();
}

/// Commit `paths` as the user through the repo index. A temp HOME has no git
/// identity, so the signature is explicit.
fn user_commit(repo: &git2::Repository, paths: &[&str], msg: &str) -> git2::Oid {
    let mut index = repo.index().unwrap();
    for p in paths {
        index.add_path(Path::new(p)).unwrap();
    }
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let parent = repo.head().ok().map(|h| h.peel_to_commit().unwrap());
    let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
    let sig = git2::Signature::now("user", "user@example.com").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parents)
        .unwrap()
}

/// A repo with a committed `README`, then `dreamd init`, one record in the
/// log, and the first `dreamd dream`. Returns the project and that run's
/// stdout. Asserts only that each command exits 0.
fn first_dream() -> (Project, String) {
    let project = Project {
        dir: tempfile::tempdir().unwrap(),
        home: tempfile::tempdir().unwrap(),
    };
    let repo = git2::Repository::init(project.dir.path()).unwrap();
    std::fs::write(project.dir.path().join("README"), b"readme\n").unwrap();
    user_commit(&repo, &["README"], "readme");

    stdout_ok(&project, &["init"]);
    append(
        &project.dir.path().join(LOG),
        &record("evt_01ARZ3NDEKTSV4RRFFQ69G5FA1", "first learning"),
    );
    let stdout = stdout_ok(&project, &["dream"]);
    (project, stdout)
}

fn today_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// Test 27: a fresh setup's first cycle commits the log, and nothing else.
#[test]
fn fresh_setup_then_dream_commits() {
    let before = today_utc();
    let (project, stdout) = first_dream();
    let after = today_utc();

    let message = project.head_message();
    assert!(
        message == format!("dreamd: cycle {before}") || message == format!("dreamd: cycle {after}"),
        "HEAD must be the cycle's commit; got {message:?}\nstdout={stdout}"
    );
    let repo = project.repo();
    let tree = repo
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .tree()
        .unwrap();
    assert!(
        tree.get_path(Path::new(LOG)).is_ok(),
        "the commit holds the log"
    );
    assert!(
        project.dir.path().join(".gitignore").is_file(),
        "init wrote .gitignore"
    );
    assert!(
        tree.get_path(Path::new(".gitignore")).is_err(),
        "init's unstaged .gitignore stays out of the commit"
    );
    assert!(
        !stdout.contains("no git commit"),
        "a committed cycle prints no notice; got {stdout}"
    );
}

/// Test 28: a real edit holds the commit on every cycle, with the notice, until
/// the user commits it.
#[test]
fn a_real_edit_holds_the_commit_until_the_user_commits_it() {
    let (project, _) = first_dream();
    let log = project.dir.path().join(LOG);
    let held_at = project.head_id();

    let edited = std::fs::read_to_string(&log)
        .unwrap()
        .replace("first learning", "edited by hand");
    std::fs::write(&log, edited).unwrap();

    for run in 1..=2 {
        let stdout = stdout_ok(&project, &["dream"]);
        assert!(
            stdout.lines().any(|line| line == HELD_NOTICE),
            "run {run}: the notice names the log; got {stdout}"
        );
        assert_eq!(project.head_id(), held_at, "run {run}: the commit is held");
    }

    let user_head = user_commit(&project.repo(), &[LOG], "commit my edit");
    append(
        &log,
        &record("evt_01ARZ3NDEKTSV4RRFFQ69G5FA2", "second learning"),
    );
    let stdout = stdout_ok(&project, &["dream"]);
    assert!(
        !stdout.contains("no git commit"),
        "the commit resumed; got {stdout}"
    );
    assert_ne!(
        project.head_id(),
        user_head,
        "a new commit on top of the user's"
    );
    assert!(
        project.head_message().starts_with("dreamd: cycle "),
        "got {:?}",
        project.head_message()
    );
}
