//! DRM-157 (LP-GEARS-05): a dream cycle's decay keeps a pinned event whose
//! id is also on an unpinned line, as a merge of two clones that keeps both
//! sides leaves it. Before the fix the unpinned copy decayed, the drop took
//! every line with the id, and recall lost the event.
//!
//! The test drives the real binary with no daemon. Every child gets its own
//! `HOME`, `XDG_CONFIG_HOME` (an empty tempdir) and `DREAMD_SOCK` (a path
//! nothing listens on), plus `DREAMD_LOG=off`, `GIT_CONFIG_NOSYSTEM=1` and
//! `GIT_CONFIG_GLOBAL=/dev/null`, with `DREAMD_LAUNCHER`, `DREAMD_LOG_FORMAT`
//! and `SOURCE_DATE_EPOCH` removed, so the cycle runs on the wall clock the
//! fixture is dated from. The test process's own environment is never
//! changed. The log is written by hand: an import, as a fixture may.
//! Unix-only: `DREAMD_SOCK` is a Unix socket.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The id on both lines.
const PINNED_ID: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FV0";
/// An unrelated old event, so the cycle has something to decay.
const OTHER_ID: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FV1";

fn dreamd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dreamd")
}

/// One test's `HOME`, empty `XDG_CONFIG_HOME`, socket folder and project.
struct Env {
    home: tempfile::TempDir,
    xdg: tempfile::TempDir,
    sock_dir: tempfile::TempDir,
    project: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().expect("home tempdir"),
            xdg: tempfile::tempdir().expect("XDG_CONFIG_HOME tempdir"),
            sock_dir: tempfile::tempdir().expect("socket tempdir"),
            project: tempfile::tempdir().expect("project tempdir"),
        }
    }

    fn project(&self) -> &Path {
        self.project.path()
    }

    /// Run `dreamd args` in the project with the test's environment rules.
    fn run(&self, args: &[&str]) -> Output {
        Command::new(dreamd_bin())
            .current_dir(self.project())
            .args(args)
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.xdg.path())
            .env("DREAMD_SOCK", self.sock_dir.path().join("d.sock"))
            .env("DREAMD_LOG", "off")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("DREAMD_LAUNCHER")
            .env_remove("DREAMD_LOG_FORMAT")
            .env_remove("SOURCE_DATE_EPOCH")
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|e| panic!("run dreamd {args:?}: {e}"))
    }

    /// Run `dreamd args`; panics unless it exits 0. Returns stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {}", describe(&out));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn log(&self) -> PathBuf {
        self.project().join(".agent/episodic/AGENT_LEARNINGS.jsonl")
    }
}

fn describe(out: &Output) -> String {
    format!(
        "exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// One log line: a record 95 days old, past the 90-day decay gate.
fn line(id: &str, skill_action: &str, content: &str, pinned: bool) -> String {
    let timestamp = chrono::Utc::now() - chrono::Duration::days(95);
    let record = serde_json::json!({
        "schema_version": "1.0.0",
        "id": id,
        "timestamp": timestamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "pain": 7.0,
        "importance": 7.0,
        "pinned": pinned,
        "skill_action": skill_action,
        "source_harness": "claude-code",
        "content": content,
    });
    format!("{record}\n")
}

/// Every file in the decay archive folder, concatenated in name order.
fn archive(project: &Path) -> String {
    let dir = project.join(".agent/.dreamd/snapshots");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.expect("dir entry").path())
        .collect();
    files.sort();
    files
        .iter()
        .map(|f| std::fs::read_to_string(f).expect("read archive file"))
        .collect()
}

/// DRM-157 test 3 (LP-GEARS-05): the cycle archives the unrelated event and
/// keeps both lines of the pinned id byte for byte, and recall still finds it.
#[test]
fn a_cycle_keeps_a_pinned_event_with_an_unpinned_copy() {
    let env = Env::new();
    std::fs::write(
        env.project().join("Cargo.toml"),
        "[package]\nname = \"decay\"\nversion = \"0.0.0\"\n",
    )
    .expect("project marker");
    env.ok(&["init", "--quiet"]);

    let pinned = line(PINNED_ID, "o::p", "versioned fixture", true);
    let unpinned = line(PINNED_ID, "o::p", "versioned fixture", false);
    let other = line(OTHER_ID, "q::r", "an unrelated old note", false);
    let kept = format!("{pinned}{unpinned}");
    std::fs::write(env.log(), format!("{pinned}{other}{unpinned}")).expect("write the log");

    env.ok(&["dream", "--no-llm"]);

    assert_eq!(
        std::fs::read_to_string(env.log()).expect("read the log"),
        kept,
        "both lines of the pinned id stay, byte for byte; only the other event leaves"
    );
    assert_eq!(
        archive(env.project()),
        other,
        "the unrelated event is archived"
    );

    let found = env.ok(&["recall", "versioned"]);
    assert!(
        found.contains(PINNED_ID),
        "recall finds the pinned id: {found}"
    );
}
