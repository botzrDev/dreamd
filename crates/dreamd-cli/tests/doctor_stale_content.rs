//! DRM-118 (LP-GEARS-04): `dreamd doctor` notices a hand edit of a log
//! line's content or scores.
//!
//! The set difference behind `index_freshness` compares ids only, so a line
//! whose content or pain was edited by hand, with the same id, read `ok` while
//! recall kept the old text and score. Doctor now compares the five fields
//! recall reads and names `dreamd doctor --repair`.
//!
//! The tests drive the real binary with no daemon. Every child gets its own
//! `HOME`, `XDG_CONFIG_HOME` (an empty tempdir) and `DREAMD_SOCK` (a path
//! nothing listens on), plus `DREAMD_LOG=off`. The log is written by hand, as
//! the hand edit this covers is. Unix-only: `DREAMD_SOCK` is a Unix socket,
//! and doctor prints `index_freshness` only on Unix.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const PORT_ID: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5PRT";
const ROTATE_ID: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5RTT";
const THIRD_ID: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5TRD";

/// 2026-11-01T09:30:00Z: past, so doctor's future-timestamp check is quiet.
const TIMESTAMP_SEC: i64 = 1_793_525_400;

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
    /// A store with the five init-created folders and no index yet, on which
    /// every default doctor check reads healthy (as `doctor_repair.rs`).
    fn new() -> Self {
        let env = Self {
            home: tempfile::tempdir().expect("home tempdir"),
            xdg: tempfile::tempdir().expect("XDG_CONFIG_HOME tempdir"),
            sock_dir: tempfile::tempdir().expect("socket tempdir"),
            project: tempfile::tempdir().expect("project tempdir"),
        };
        for sub in ["working", "episodic", "semantic", "personal", ".dreamd"] {
            std::fs::create_dir_all(env.project().join(".agent").join(sub)).expect("scaffold");
        }
        env
    }

    fn project(&self) -> &Path {
        self.project.path()
    }

    fn log(&self) -> PathBuf {
        self.project().join(".agent/episodic/AGENT_LEARNINGS.jsonl")
    }

    fn write_log(&self, lines: &[String]) {
        std::fs::write(self.log(), lines.concat()).expect("write the log");
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
            .env_remove("DREAMD_LAUNCHER")
            .env_remove("DREAMD_LOG_FORMAT")
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|e| panic!("run dreamd {args:?}: {e}"))
    }

    /// `dreamd doctor args`: its exit code and stdout.
    fn doctor(&self, args: &[&str]) -> (Option<i32>, String) {
        let out = self.run(&[&["doctor"], args].concat());
        (
            out.status.code(),
            format!("{}{}", stdout(&out), describe_stderr(&out)),
        )
    }

    /// `dreamd recall query`'s stdout; panics unless it exits 0.
    fn recall(&self, query: &str) -> String {
        let out = self.run(&["recall", query]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "recall {query}: {}{}",
            stdout(&out),
            describe_stderr(&out)
        );
        stdout(&out)
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn describe_stderr(out: &Output) -> String {
    let err = String::from_utf8_lossy(&out.stderr);
    if err.is_empty() {
        String::new()
    } else {
        format!("--- stderr ---\n{err}")
    }
}

/// One log line as dreamd writes it.
fn line(id: &str, content: &str, pain: f32) -> String {
    let timestamp = chrono::DateTime::from_timestamp(TIMESTAMP_SEC, 0).expect("timestamp");
    let record = serde_json::json!({
        "schema_version": "1.0.0",
        "id": id,
        "timestamp": timestamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "pain": pain,
        "importance": 6.0,
        "pinned": false,
        "skill_action": "infra::staging",
        "source_harness": "claude-code",
        "content": content,
    });
    format!("{record}\n")
}

fn port_line(port: &str) -> String {
    line(
        PORT_ID,
        &format!("use port {port} for the staging database"),
        5.0,
    )
}

fn rotate_line(pain: f32) -> String {
    line(ROTATE_ID, "staging password rotates monthly", pain)
}

fn has_line(output: &str, want: &str) -> bool {
    output.lines().any(|l| l == want)
}

/// Seed the two LP-GEARS-04 records and build the index from the log.
fn indexed_store() -> Env {
    let env = Env::new();
    env.write_log(&[port_line("5432"), rotate_line(5.0)]);
    let (code, output) = env.doctor(&["--repair"]);
    assert_eq!(code, Some(0), "the first build: {output}");
    assert!(
        output.contains("rebuilt index from the episodic log"),
        "{output}"
    );
    let (code, output) = env.doctor(&[]);
    assert_eq!(code, Some(0), "precondition: a healthy store: {output}");
    assert!(has_line(&output, "index_freshness: ok"), "{output}");
    env
}

/// DRM-118 test 5 (LP-GEARS-04): an edited content and an edited pain read
/// `stale content` until `dreamd doctor --repair`, and recall follows.
#[test]
fn doctor_reports_a_hand_edited_line_until_repair() {
    let env = indexed_store();

    env.write_log(&[port_line("6543"), rotate_line(10.0)]);

    let (code, output) = env.doctor(&[]);
    assert_eq!(code, Some(1), "{output}");
    let want = format!(
        "index_freshness: stale content  [WARNING: 2 event(s) changed in the log since they \
         were indexed ({PORT_ID}, {ROTATE_ID}); recall returns the old text and scores until \
         you run `dreamd doctor --repair`]"
    );
    assert!(has_line(&output, &want), "want {want}\ngot:\n{output}");
    let before = env.recall("6543");
    assert!(before.contains("(0 hits)"), "{before}");

    // The run's own check still sees the stale index before it repairs, so
    // the repair run exits 1, as a repaired orphan socket does.
    let (code, output) = env.doctor(&["--repair"]);
    assert_eq!(code, Some(1), "{output}");
    assert!(has_line(&output, &want), "{output}");
    assert!(
        output.contains("rebuilt index from the episodic log"),
        "{output}"
    );

    let (code, output) = env.doctor(&[]);
    assert_eq!(code, Some(0), "{output}");
    assert!(has_line(&output, "index_freshness: ok"), "{output}");
    let after = env.recall("6543");
    assert!(after.contains(PORT_ID), "{after}");
}

/// DRM-118 test 6: formatting and a field recall does not read are not a
/// change.
#[test]
fn formatting_and_unknown_fields_are_not_stale() {
    let env = indexed_store();

    let instant = chrono::DateTime::from_timestamp(TIMESTAMP_SEC, 0)
        .expect("timestamp")
        .with_timezone(&chrono::FixedOffset::east_opt(2 * 3600).expect("+02:00"))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
    assert!(instant.ends_with("+02:00"), "{instant}");
    let reordered = format!(
        "{{\"x_note\": \"kept\", \"content\": \"use port 5432 for the staging database\", \
         \"skill_action\": \"infra::staging\", \"source_harness\": \"claude-code\", \
         \"pinned\": false, \"importance\": 6.0, \"pain\": 5, \"timestamp\": \"{instant}\", \
         \"id\": \"{PORT_ID}\", \"schema_version\": \"1.0.0\"}}\n"
    );
    env.write_log(&[reordered, rotate_line(5.0)]);

    let (code, output) = env.doctor(&[]);
    assert_eq!(code, Some(0), "{output}");
    assert!(has_line(&output, "index_freshness: ok"), "{output}");
}

/// DRM-118 test 7: an id the index lacks and a changed line together name
/// only the repair, since a writer open does not re-index a changed line.
#[test]
fn set_drift_and_a_changed_line_name_only_repair() {
    let env = indexed_store();

    env.write_log(&[
        port_line("6543"),
        rotate_line(5.0),
        line(THIRD_ID, "the staging bucket is versioned", 5.0),
    ]);

    let (code, output) = env.doctor(&[]);
    assert_eq!(code, Some(1), "{output}");
    let want = "index_freshness: stale  [WARNING: 1 event(s) not indexed, 0 indexed event(s) \
                not in the log, 0 duplicated, 1 changed in the log since indexed; run \
                `dreamd doctor --repair`]";
    assert!(has_line(&output, want), "want {want}\ngot:\n{output}");
}
