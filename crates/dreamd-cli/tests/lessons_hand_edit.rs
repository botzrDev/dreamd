//! DRM-115: a dream cycle keeps a hand-edited `LESSONS.md` instead of
//! overwriting it, on the `user_dirty_tree` and the `no_baseline` routes, in
//! process and through `POST /api/v1/dream`.
//!
//! Subprocess tests in the shape of `dream_autobiography.rs`: the repo is
//! built and read with git2, and the test process's environment is never
//! changed. Every child gets its own `HOME` (with `XDG_CONFIG_HOME` inside
//! it), `DREAMD_LOG=off` and a `DREAMD_SOCK` in a tempdir, and has
//! `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `DREAMD_LAUNCHER` and
//! `SOURCE_DATE_EPOCH` removed, so every cycle is deterministic. Only test 7
//! starts a daemon. The expected lines are spelled out so the file also
//! builds against a tree that lacks the functions (the red-first run).
#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const LOG: &str = ".agent/episodic/AGENT_LEARNINGS.jsonl";
const LESSONS: &str = ".agent/semantic/LESSONS.md";
const STATE: &str = ".agent/.dreamd/state.json";
const TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);

/// The three records of the one cluster, and their contents.
const CLUSTER: [(&str, &str); 3] = [
    (
        "evt_01ARZ3NDEKTSV4RRFFQ69G5FA1",
        "flaky test: retry it once with --nocapture",
    ),
    (
        "evt_01ARZ3NDEKTSV4RRFFQ69G5FA2",
        "flaky test: rerun it alone before blaming the diff",
    ),
    (
        "evt_01ARZ3NDEKTSV4RRFFQ69G5FA3",
        "flaky test: look for a shared temp dir",
    ),
];
/// A record past `decay.rs`'s 90-day threshold, in another `skill_action`.
const OLD: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FA9";
const CORRECTION: &str = "flaky test: NEVER retry, file a ticket (my correction)";

/// `dream_cycle::lesson_held_notice` for `user_dirty_tree`.
const KEPT_DIRTY: &str = "lesson kept: .agent/semantic/LESSONS.md changed outside a dream \
                          cycle, so this cycle did not rewrite it. dreamd rebuilds LESSONS.md \
                          from the log, so commit your edit with git to keep it in history, or \
                          delete the file to drop it, and the next cycle writes the lesson \
                          again. To keep a correction for good, record it with append_node.";
/// `dream_cycle::lesson_held_notice` for `no_baseline`.
const KEPT_NO_BASELINE: &str = "lesson kept: .agent/semantic/LESSONS.md differs from HEAD, and \
                                dreamd has no record yet of what its last cycle wrote, so this \
                                cycle did not rewrite it. Commit it with git, or delete it, and \
                                the next cycle writes the lesson again.";
/// `autobiography::skip_message("user_dirty_tree", [LESSONS])`.
const SKIP_DIRTY: &str = "no git commit for this cycle: .agent/semantic/LESSONS.md changed \
                          outside a dream cycle. Commit those changes with git, and the next \
                          cycle commits again.";
/// `autobiography::skip_message("no_baseline", [LESSONS])`.
const SKIP_NO_BASELINE: &str = "no git commit for this cycle: .agent/semantic/LESSONS.md differ \
                                from HEAD, and dreamd has no record yet of what its last cycle \
                                wrote. Commit those files with git, and the next cycle commits \
                                again.";

/// A git project, its own `HOME`, and a socket path no daemon listens on
/// unless a test starts one.
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

    fn head_message(&self) -> String {
        let repo = self.repo();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        head.message().unwrap().to_string()
    }

    fn lessons(&self) -> Option<Vec<u8>> {
        std::fs::read(self.root().join(LESSONS)).ok()
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.root().join(LOG)).unwrap()
    }

    /// A `dreamd` command in the project, with the file's environment rules.
    fn dreamd(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_dreamd"));
        cmd.current_dir(self.root())
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env("DREAMD_LOG", "off")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("DREAMD_SOCK", &self.sock)
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("DREAMD_LAUNCHER")
            .env_remove("SOURCE_DATE_EPOCH")
            .stdin(Stdio::null());
        cmd
    }

    /// Run `args`, require exit 0, and return stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out: Output = self
            .dreamd()
            .args(args)
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

    /// Replace the lesson body with [`CORRECTION`]; returns the new bytes.
    fn hand_edit(&self) -> Vec<u8> {
        let before = String::from_utf8(self.lessons().expect("a lesson to edit")).unwrap();
        let mut after = before.clone();
        for (_, content) in CLUSTER {
            after = after.replace(content, CORRECTION);
        }
        assert_ne!(after, before, "the body was the exemplar's content");
        std::fs::write(self.root().join(LESSONS), &after).unwrap();
        after.into_bytes()
    }
}

/// One episodic record line, `age_days` old.
fn record(id: &str, skill_action: &str, pain: f64, age_days: i64, content: &str) -> String {
    let ts = (chrono::Utc::now() - chrono::Duration::days(age_days))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut line = serde_json::json!({
        "schema_version": "1.0.0",
        "id": id,
        "timestamp": ts,
        "pain": pain,
        "importance": pain,
        "skill_action": skill_action,
        "source_harness": "claude-code",
        "content": content,
    })
    .to_string();
    line.push('\n');
    line
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

/// A repo with a committed `README`, `dreamd init`, the three cluster
/// records, and the first `dreamd dream`, which writes a lesson and commits.
fn first_dream() -> Project {
    let sock_dir = tempfile::tempdir().unwrap();
    let project = Project {
        dir: tempfile::tempdir().unwrap(),
        home: tempfile::tempdir().unwrap(),
        sock: sock_dir.path().join("d.sock"),
        _sock_dir: sock_dir,
    };
    let repo = git2::Repository::init(project.root()).unwrap();
    std::fs::write(project.root().join("README"), b"readme\n").unwrap();
    let readme = user_commit(&repo, &["README"], "readme");

    project.ok(&["init", "--quiet"]);
    for (id, content) in CLUSTER {
        project.append(&record(id, "rust::tests::flaky", 6.0, 0, content));
    }
    let stdout = project.ok(&["dream"]);
    assert!(
        project.lessons().is_some(),
        "the first dream writes a lesson"
    );
    assert_ne!(
        project.head_id(),
        readme,
        "the first dream commits: {stdout}"
    );
    assert!(project.head_message().starts_with("dreamd: cycle "));
    project
}

fn has_line(stdout: &str, line: &str) -> bool {
    stdout.lines().any(|l| l == line)
}

/// DRM-115 test 5 (LP-GEARS-03): the hand edit survives the next cycle, the
/// log's decay still runs, and once the edit is committed the next cycle
/// rewrites the lesson and commits.
#[test]
fn dream_keeps_a_hand_edited_lessons_md() {
    let project = first_dream();
    let edit = project.hand_edit();
    project.append(&record(OLD, "go::old", 2.0, 120, "an old note"));
    let before = project.head_id();

    let stdout = project.ok(&["dream"]);
    assert_eq!(
        project.lessons().as_deref(),
        Some(edit.as_slice()),
        "the edit is kept; stdout={stdout}"
    );
    assert!(has_line(&stdout, KEPT_DIRTY), "held line; stdout={stdout}");
    assert!(has_line(&stdout, SKIP_DIRTY), "skip line; stdout={stdout}");
    assert!(
        !stdout.contains("lesson: copy of"),
        "no copy line; stdout={stdout}"
    );
    assert!(!project.log().contains(OLD), "the old record decayed");
    assert_eq!(project.head_id(), before, "no commit");

    let mine = user_commit(&project.repo(), &[LESSONS], "my correction");
    let stdout = project.ok(&["dream"]);
    let lessons = String::from_utf8(project.lessons().unwrap()).unwrap();
    assert!(
        !lessons.contains(CORRECTION),
        "the committed edit is rebuilt from the log: {lessons}"
    );
    assert!(!stdout.contains("lesson kept"), "stdout={stdout}");
    assert_ne!(project.head_id(), mine, "the cycle commits: {stdout}");
    assert!(project.head_message().starts_with("dreamd: cycle "));
}

/// DRM-115 test 6: with no baseline, an edited `LESSONS.md` is kept too, and
/// the hold lasts until the file is deleted.
#[test]
fn dream_keeps_lessons_md_on_the_no_baseline_route() {
    let project = first_dream();
    let state_path = project.root().join(STATE);
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    assert!(
        state
            .as_object_mut()
            .unwrap()
            .remove("autobiography_baseline")
            .is_some(),
        "precondition: the first dream recorded a baseline"
    );
    std::fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    let edit = project.hand_edit();

    let stdout = project.ok(&["dream"]);
    assert_eq!(project.lessons().as_deref(), Some(edit.as_slice()));
    assert!(
        has_line(&stdout, KEPT_NO_BASELINE),
        "no_baseline held line; stdout={stdout}"
    );
    assert!(has_line(&stdout, SKIP_NO_BASELINE), "stdout={stdout}");

    let stdout = project.ok(&["dream"]);
    assert_eq!(project.lessons().as_deref(), Some(edit.as_slice()));
    assert!(
        has_line(&stdout, KEPT_DIRTY),
        "user_dirty_tree held line; stdout={stdout}"
    );

    std::fs::remove_file(project.root().join(LESSONS)).unwrap();
    let stdout = project.ok(&["dream"]);
    let lessons = project.lessons().expect("the cycle writes a lesson again");
    assert!(
        !String::from_utf8(lessons).unwrap().contains(CORRECTION),
        "stdout={stdout}"
    );
    assert!(!stdout.contains("lesson kept"), "stdout={stdout}");
}

/// A really-running `dreamd watch`, killed on drop.
struct Watch {
    child: Child,
    console: PathBuf,
}

impl Watch {
    fn spawn(project: &Project) -> Self {
        let console = project.home.path().join("watch.console");
        let out = std::fs::File::create(&console).expect("create console file");
        let err = out.try_clone().expect("clone console file handle");
        let child = project
            .dreamd()
            .arg("watch")
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .expect("spawn dreamd watch");
        Self { child, console }
    }

    fn console(&self) -> String {
        std::fs::read_to_string(&self.console).unwrap_or_default()
    }

    fn wait_for_live_socket(&mut self, socket: &Path) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if dreamd_core::server::is_daemon_socket_live(socket) {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                panic!(
                    "dreamd watch exited ({status}) before binding {}; console:\n{}",
                    socket.display(),
                    self.console()
                );
            }
            assert!(
                Instant::now() < deadline,
                "dreamd watch never bound {} within {TIMEOUT:?}; console:\n{}",
                socket.display(),
                self.console()
            );
            std::thread::sleep(POLL);
        }
    }
}

impl Drop for Watch {
    /// SIGKILL unconditionally: a leaked daemon holds the writer lock on a
    /// tempdir that is about to disappear.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One raw HTTP/1.1 `POST /api/v1/dream` for `project` over its socket;
/// returns the status and the body parsed as JSON.
fn post_dream(project: &Project) -> (u16, serde_json::Value) {
    let request = format!(
        "POST /api/v1/dream HTTP/1.1\r\nHost: localhost\r\nX-Agent-Root: {}\r\n\
         Content-Length: 0\r\nConnection: close\r\n\r\n",
        project.root().display()
    );
    let mut stream = UnixStream::connect(&project.sock).expect("connect daemon socket");
    stream
        .set_read_timeout(Some(TIMEOUT))
        .expect("set read timeout");
    stream.write_all(request.as_bytes()).expect("write request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    let status = response
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("no status line: {response}"));
    let (_, payload) = response
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("no header end: {response}"));
    let json = serde_json::from_str(payload)
        .unwrap_or_else(|e| panic!("body is not JSON ({e}): {response}"));
    (status, json)
}

/// DRM-115 test 7 (LP-HTTP-05): `POST /api/v1/dream` keeps the hand edit and
/// reports it.
#[test]
fn the_dream_endpoint_reports_the_held_lesson() {
    let project = first_dream();
    let edit = project.hand_edit();
    let mut watch = Watch::spawn(&project);
    watch.wait_for_live_socket(&project.sock);

    let (status, body) = post_dream(&project);
    assert_eq!(status, 200, "body={body}\nconsole:\n{}", watch.console());
    assert_eq!(
        body["lesson_held"],
        serde_json::json!({"reason": "user_dirty_tree", "file": LESSONS}),
        "body={body}"
    );
    let notices = body["notices"].as_array().cloned().unwrap_or_default();
    assert!(
        notices
            .iter()
            .any(|n| n.as_str().is_some_and(|s| s.starts_with("lesson kept:"))),
        "body={body}"
    );
    assert_eq!(
        body["autobiography"]["outcome"],
        serde_json::json!("skipped"),
        "body={body}"
    );
    assert_eq!(project.lessons().as_deref(), Some(edit.as_slice()));
}

/// DRM-115 test 8: a store with no hand edit still rewrites and commits.
#[test]
fn a_clean_store_still_rewrites_and_commits() {
    let project = first_dream();
    let first = project.head_id();
    let lessons = project.lessons();
    project.append(&record(
        "evt_01ARZ3NDEKTSV4RRFFQ69G5FA4",
        "rust::tests::flaky",
        9.0,
        0,
        "flaky test: pin the seed",
    ));

    let stdout = project.ok(&["dream"]);
    assert!(!stdout.contains("lesson kept"), "stdout={stdout}");
    assert_ne!(project.lessons(), lessons, "the cycle rewrote LESSONS.md");
    assert_ne!(
        project.head_id(),
        first,
        "the second cycle commits: {stdout}"
    );
    assert!(project.head_message().starts_with("dreamd: cycle "));
}
