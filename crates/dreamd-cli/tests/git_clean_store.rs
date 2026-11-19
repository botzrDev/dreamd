//! DRM-100: `git clean -fd` deletes an episodic log that git has no commit
//! of, while the gitignored index under `.agent/.dreamd/` keeps its events.
//! Setup names the commit, doctor warns until it is made and after a clean,
//! and `doctor --repair` keeps the index until `--allow-empty-log`.
//!
//! Drives the real `dreamd` binary and the real `git`: the line setup prints
//! is a shell command the user runs, and only `git` shows what it commits.
//! Every `git` call gets a temp `HOME`, no system or global config and a fixed
//! identity, so the developer's own git settings stay out. Unix-only: the
//! command uses `&&`, and the repair takes the project writer lock.
#![cfg(unix)]

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output};

const LOG: &str = ".agent/episodic/AGENT_LEARNINGS.jsonl";

/// What setup prints in a repo whose root has a `.gitignore`.
const COMMIT_COMMAND: &str =
    "git add .agent .gitignore && git commit -m \"Add dreamd memory\" -- .agent .gitignore";

/// A temp project and a separate temp daemon home.
struct Sandbox {
    project: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Sandbox {
            project: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn root(&self) -> &Path {
        self.project.path()
    }

    /// `cmd` in the project, with the isolated git environment.
    fn run_in_project(&self, mut cmd: Command) -> Output {
        cmd.current_dir(self.root())
            .env("HOME", self.home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "user")
            .env("GIT_AUTHOR_EMAIL", "user@example.com")
            .env("GIT_COMMITTER_NAME", "user")
            .env("GIT_COMMITTER_EMAIL", "user@example.com");
        cmd.output().unwrap_or_else(|e| panic!("run {cmd:?}: {e}"))
    }

    /// `git <args>`, which must succeed; returns its stdout.
    fn git(&self, args: &[&str]) -> String {
        let mut cmd = Command::new("git");
        cmd.args(args);
        let out = self.run_in_project(cmd);
        assert!(
            out.status.success(),
            "git {args:?} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// `dreamd <args>`: (exit code, stdout, stderr).
    fn dreamd(&self, args: &[&str]) -> (Option<i32>, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_dreamd"))
            .args(args)
            .current_dir(self.root())
            .env("HOME", self.home.path())
            .env("DREAMD_LOG", "off")
            .env_remove("DREAMD_LAUNCHER")
            .env_remove("DREAMD_SOCK")
            .output()
            .unwrap_or_else(|e| panic!("run dreamd {args:?}: {e}"));
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// `dreamd setup` with no harness files; returns the card.
    fn setup(&self) -> String {
        let (code, stdout, stderr) =
            self.dreamd(&["setup", "--yes", "--harness", "none", "--no-write-mcp"]);
        assert_eq!(code, Some(0), "setup:\n{stdout}\n{stderr}");
        stdout
    }

    /// Append two events to the log, as the coordinator writes them.
    fn append_two_events(&self) {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let mut body = String::new();
        for (id, content) in [
            (
                "evt_01ARZ3NDEKTSV4RRFFQ69G5FB1",
                "deploys go out from the main branch",
            ),
            (
                "evt_01ARZ3NDEKTSV4RRFFQ69G5FB2",
                "staging credentials rotate weekly",
            ),
        ] {
            let line = serde_json::json!({
                "schema_version": "1.0.0",
                "id": id,
                "timestamp": now,
                "pain": 5.0,
                "importance": 5.0,
                "skill_action": "ops::deploy",
                "source_harness": "claude-code",
                "content": content,
            });
            body.push_str(&line.to_string());
            body.push('\n');
        }
        std::fs::OpenOptions::new()
            .append(true)
            .open(self.root().join(LOG))
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
    }
}

/// A new repo with dreamd set up in it; returns the sandbox and the card.
fn new_repo_with_setup() -> (Sandbox, String) {
    let sandbox = Sandbox::new();
    sandbox.git(&["init", "-q"]);
    let card = sandbox.setup();
    (sandbox, card)
}

/// DRM-100 test 24 (LP-JOURNEY-11): setup names the commit, doctor warns once
/// the log holds events, and after `git clean -fd` doctor reports the loss and
/// `--repair` keeps the index until `--allow-empty-log`.
#[test]
fn git_clean_after_setup_is_reported_and_repair_keeps_the_index() {
    // 1. Setup in a new repo prints the commit, and its doctor beat passes.
    let (sandbox, card) = new_repo_with_setup();
    assert!(
        card.contains(&format!("\n          {COMMIT_COMMAND}\n")),
        "{card}"
    );
    assert!(card.contains("doctor: all checks passed"), "{card}");

    // 2. Two events, and an index built from them.
    sandbox.append_two_events();
    let (_, stdout, stderr) = sandbox.dreamd(&["doctor", "--repair"]);
    assert!(
        stdout.contains("rebuilt index from the episodic log"),
        "{stdout}\n{stderr}"
    );

    // 3. Doctor warns while git has no commit of them.
    let (code, stdout, _) = sandbox.dreamd(&["doctor"]);
    assert!(
        stdout.contains(
            "memory_in_git: not committed  [WARNING: git has no commit of the episodic \
             log's 2 event(s)"
        ),
        "{stdout}"
    );
    assert_eq!(code, Some(1), "{stdout}");

    // 4. The clean deletes the untracked log.
    sandbox.git(&["clean", "-fd"]);
    assert!(
        !sandbox.root().join(LOG).exists(),
        "git clean -fd deletes a log git has no commit of"
    );

    // 5. Doctor reports the loss, not ordinary drift.
    let (code, stdout, _) = sandbox.dreamd(&["doctor"]);
    assert!(
        stdout.contains(
            "episodic_log: missing  [WARNING: AGENT_LEARNINGS.jsonl is gone, and the \
             index still holds 2 event(s)"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains("index_freshness: stale  [WARNING: the index holds 2 event(s)"),
        "{stdout}"
    );
    assert_eq!(code, Some(1), "{stdout}");

    // 6. `--repair` refuses and the index keeps the events.
    let (code, stdout, stderr) = sandbox.dreamd(&["doctor", "--repair"]);
    assert_eq!(code, Some(1), "{stdout}\n{stderr}");
    assert!(
        stderr.contains(
            "doctor --repair refused: the episodic log is missing, and the index holds 2 \
             event(s) it does not"
        ),
        "{stderr}"
    );
    let (_, stdout, _) = sandbox.dreamd(&["doctor"]);
    assert!(
        stdout.contains("still holds 2 event(s)"),
        "a refused repair leaves the index as it was:\n{stdout}"
    );

    // 7. `--allow-empty-log` rebuilds an empty index.
    let (_, stdout, stderr) = sandbox.dreamd(&["doctor", "--repair", "--allow-empty-log"]);
    assert!(
        stdout.contains("rebuilt index from the episodic log"),
        "{stdout}\n{stderr}"
    );
    let (_, stdout, _) = sandbox.dreamd(&["doctor"]);
    assert!(!stdout.contains("still holds"), "{stdout}");
}

/// DRM-100 test 25: the card's command commits the log and `.gitignore`, and
/// leaves the user's other staged file staged and out of the commit. The log
/// then survives `git clean -fd`, and a second setup prints no commit.
#[test]
fn the_cards_commit_keeps_the_log_through_git_clean() {
    let (sandbox, _) = new_repo_with_setup();
    sandbox.append_two_events();
    std::fs::write(sandbox.root().join("notes.txt"), "draft\n").unwrap();
    sandbox.git(&["add", "notes.txt"]);

    let mut sh = Command::new("sh");
    sh.arg("-c").arg(COMMIT_COMMAND);
    let out = sandbox.run_in_project(sh);
    assert!(
        out.status.success(),
        "the card's command failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let names = sandbox.git(&["show", "--name-only", "--format=", "HEAD"]);
    let names: Vec<&str> = names.lines().collect();
    assert!(names.contains(&LOG), "{names:?}");
    assert!(names.contains(&".gitignore"), "{names:?}");
    assert!(!names.contains(&"notes.txt"), "{names:?}");
    assert!(
        !names.iter().any(|name| name.starts_with(".agent/.dreamd/")),
        "{names:?}"
    );
    assert_eq!(
        sandbox.git(&["diff", "--cached", "--name-only"]).trim_end(),
        "notes.txt",
        "the user's staged file stays staged"
    );

    let (_, stdout, _) = sandbox.dreamd(&["doctor"]);
    assert!(
        stdout
            .lines()
            .any(|line| line == "memory_in_git: committed"),
        "{stdout}"
    );

    sandbox.git(&["clean", "-fd"]);
    assert!(
        sandbox.root().join(LOG).exists(),
        "a committed log survives git clean -fd"
    );
    let card = sandbox.setup();
    assert!(!card.contains("Commit:"), "{card}");
}
