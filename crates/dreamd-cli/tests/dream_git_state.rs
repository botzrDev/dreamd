//! DRM-116: the cycle's git commit skips a detached HEAD and a git operation
//! in progress. The cycle still runs, and the next cycle on a branch commits
//! what it left.
//!
//! Subprocess tests in the shape of `dream_autobiography.rs`: the repo is
//! built and read with git2, never the `git` binary, and the test process's
//! environment is never changed. Every child gets its own `HOME` (with
//! `XDG_CONFIG_HOME` inside it), `DREAMD_LOG=off`, and a `DREAMD_SOCK` in a
//! tempdir that no daemon listens on, so `dreamd dream` runs in process. The
//! expected lines are spelled out so the file also builds against a tree that
//! lacks them (the red-first run). Unix-only: the in-process cycle takes the
//! project writer lock.
#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const LOG: &str = ".agent/episodic/AGENT_LEARNINGS.jsonl";

/// `autobiography::skip_message("detached_head", [])`.
const DETACHED: &str = "no git commit for this cycle: HEAD is detached (a checked-out tag or \
                        commit), so the commit would be on no branch. The cycle's changes stay \
                        in the working tree; check out a branch, and the next cycle commits \
                        them.";
/// `autobiography::skip_message("merge_in_progress", [])`.
const MERGING: &str = "no git commit for this cycle: a merge is in progress. The cycle's \
                       changes stay in the working tree; finish or abort the merge, and the \
                       next cycle commits them.";

/// A git project on `main`, its own `HOME`, and a socket path no daemon
/// listens on.
struct Project {
    dir: tempfile::TempDir,
    home: tempfile::TempDir,
    sock: PathBuf,
    _sock_dir: tempfile::TempDir,
}

impl Project {
    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn repo(&self) -> git2::Repository {
        git2::Repository::open(self.root()).unwrap()
    }

    fn head_id(&self) -> git2::Oid {
        self.repo().head().unwrap().peel_to_commit().unwrap().id()
    }

    fn main_id(&self) -> git2::Oid {
        self.repo()
            .find_reference("refs/heads/main")
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
    }

    /// The log's bytes in `oid`'s tree.
    fn log_at(&self, oid: git2::Oid) -> String {
        let repo = self.repo();
        let tree = repo.find_commit(oid).unwrap().tree().unwrap();
        let blob = tree
            .get_path(Path::new(LOG))
            .unwrap()
            .to_object(&repo)
            .unwrap()
            .peel_to_blob()
            .unwrap();
        String::from_utf8(blob.content().to_vec()).unwrap()
    }

    fn live_log(&self) -> String {
        std::fs::read_to_string(self.root().join(LOG)).unwrap()
    }

    /// `dreamd dream`; requires exit 0 and returns stdout.
    fn dream(&self) -> String {
        self.ok(&["dream"])
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_dreamd"))
            .args(args)
            .current_dir(self.root())
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env("DREAMD_LOG", "off")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("DREAMD_SOCK", &self.sock)
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("DREAMD_LAUNCHER")
            .env_remove("SOURCE_DATE_EPOCH")
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|e| panic!("run dreamd {args:?}: {e}"));
        assert!(
            out.status.success(),
            "dreamd {args:?} exit={:?}\nstdout={}\nstderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn append(&self, line: &str) {
        std::fs::OpenOptions::new()
            .append(true)
            .open(self.root().join(LOG))
            .unwrap()
            .write_all(line.as_bytes())
            .unwrap();
    }
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
        "skill_action": "rust::git_state",
        "source_harness": "claude-code",
        "content": content,
    })
    .to_string();
    line.push('\n');
    line
}

fn sig() -> git2::Signature<'static> {
    git2::Signature::now("user", "user@example.com").unwrap()
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
    repo.commit(Some("HEAD"), &sig(), &sig(), msg, &tree, &parents)
        .unwrap()
}

/// A repo on `main` with a committed `README`, then `dreamd init`, one
/// record, and the store committed by the user. Returns that commit.
fn committed_store() -> (Project, git2::Oid) {
    let sock_dir = tempfile::tempdir().unwrap();
    let project = Project {
        dir: tempfile::tempdir().unwrap(),
        home: tempfile::tempdir().unwrap(),
        sock: sock_dir.path().join("d.sock"),
        _sock_dir: sock_dir,
    };
    let mut opts = git2::RepositoryInitOptions::new();
    opts.initial_head("main");
    let repo = git2::Repository::init_opts(project.root(), &opts).unwrap();
    std::fs::write(project.root().join("README"), b"readme\n").unwrap();
    user_commit(&repo, &["README"], "readme");

    project.ok(&["init", "--quiet"]);
    project.append(&record("evt_01ARZ3NDEKTSV4RRFFQ69G5FA1", "first learning"));
    let store = user_commit(&repo, &[LOG], "store");
    (project, store)
}

fn has_line(stdout: &str, line: &str) -> bool {
    stdout.lines().any(|l| l == line)
}

/// DRM-116 test 5 (LP-GEARS-02): a cycle on a detached HEAD makes no commit,
/// the learning survives the return to `main`, and the next cycle commits it
/// there.
#[test]
fn dream_on_a_detached_head_keeps_the_learning() {
    let (project, store) = committed_store();
    let repo = project.repo();
    repo.tag_lightweight("v1", repo.find_commit(store).unwrap().as_object(), false)
        .unwrap();
    let v1 = repo
        .find_reference("refs/tags/v1")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(v1, store);
    repo.set_head_detached(v1).unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().safe()))
        .unwrap();
    assert!(
        repo.head_detached().unwrap(),
        "precondition: detached at v1"
    );

    project.append(&record(
        "evt_01ARZ3NDEKTSV4RRFFQ69G5FA2",
        "learned while detached",
    ));
    let stdout = project.dream();
    assert!(has_line(&stdout, DETACHED), "stdout={stdout}");
    assert_eq!(project.head_id(), v1, "HEAD is still v1's commit");

    let main = repo.find_commit(project.main_id()).unwrap();
    repo.checkout_tree(
        main.as_object(),
        Some(git2::build::CheckoutBuilder::new().safe()),
    )
    .unwrap();
    repo.set_head("refs/heads/main").unwrap();
    assert!(
        project.live_log().contains("learned while detached"),
        "the learning survives the return to main"
    );

    let before = project.main_id();
    let stdout = project.dream();
    assert!(!stdout.contains("no git commit"), "stdout={stdout}");
    let after = project.main_id();
    assert_ne!(after, before, "the cycle commits on main: {stdout}");
    assert!(project.log_at(after).contains("learned while detached"));
}

/// DRM-116 test 6: a cycle during a merge makes no commit; once the merge is
/// gone, the next cycle commits.
#[test]
fn dream_during_a_merge_does_not_commit() {
    let (project, store) = committed_store();
    let repo = project.repo();
    // A second branch with one commit, built without touching the work tree.
    let base = repo.find_commit(store).unwrap();
    let blob = repo.blob(b"topic\n").unwrap();
    let mut builder = repo.treebuilder(Some(&base.tree().unwrap())).unwrap();
    builder.insert("topic.txt", blob, 0o100_644).unwrap();
    let tree = repo.find_tree(builder.write().unwrap()).unwrap();
    let topic = repo
        .commit(
            Some("refs/heads/topic"),
            &sig(),
            &sig(),
            "topic",
            &tree,
            &[&base],
        )
        .unwrap();
    let merge_head = repo.path().join("MERGE_HEAD");
    std::fs::write(&merge_head, format!("{topic}\n")).unwrap();
    assert_eq!(repo.state(), git2::RepositoryState::Merge);

    project.append(&record(
        "evt_01ARZ3NDEKTSV4RRFFQ69G5FA2",
        "learned mid-merge",
    ));
    let before = project.head_id();
    let stdout = project.dream();
    assert!(has_line(&stdout, MERGING), "stdout={stdout}");
    assert_eq!(project.head_id(), before, "no commit during the merge");

    std::fs::remove_file(&merge_head).unwrap();
    let stdout = project.dream();
    assert!(!stdout.contains("no git commit"), "stdout={stdout}");
    assert_ne!(
        project.head_id(),
        before,
        "the next cycle commits: {stdout}"
    );
    assert!(project
        .log_at(project.head_id())
        .contains("learned mid-merge"));
}

/// DRM-116 test 7: on a branch, every cycle commits as before.
#[test]
fn dream_on_a_branch_commits_as_today() {
    let (project, store) = committed_store();
    project.append(&record("evt_01ARZ3NDEKTSV4RRFFQ69G5FA2", "second learning"));
    let stdout = project.dream();
    assert!(!stdout.contains("no git commit"), "stdout={stdout}");
    let first = project.main_id();
    assert_ne!(first, store, "the first cycle commits: {stdout}");

    project.append(&record("evt_01ARZ3NDEKTSV4RRFFQ69G5FA3", "third learning"));
    let stdout = project.dream();
    assert!(!stdout.contains("no git commit"), "stdout={stdout}");
    assert_ne!(
        project.main_id(),
        first,
        "the second cycle commits: {stdout}"
    );
}
