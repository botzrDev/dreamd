//! DMD-125: a dream cycle stores the provenance ledger's root, and
//! `dreamd doctor --provenance` reports a hand edit of the ledger that the
//! edge checks alone cannot see.
//!
//! Before this change the root was recomputed on demand and stored nowhere,
//! so swapping two ledger lines, or cutting the last one when its event has
//! another edge, left `doctor --provenance` at `ok`.
//!
//! The test drives the real binary with no daemon, with the environment rules
//! of `promotion_rule.rs` (test files share no helpers, so `Env` is copied).
//! The log is written by hand: an import, as a fixture may.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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

    fn agent(&self) -> PathBuf {
        self.project().join(".agent")
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

/// One log line: a record `days` before now.
fn line(id: &str, days: f64, skill_action: &str, pain: f64, content: &str) -> String {
    let timestamp = chrono::Utc::now() - chrono::Duration::seconds((days * 86_400.0) as i64);
    let record = serde_json::json!({
        "schema_version": "1.0.0",
        "id": id,
        "timestamp": timestamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "pain": pain,
        "importance": pain,
        "pinned": false,
        "skill_action": skill_action,
        "source_harness": "claude-code",
        "content": content,
    });
    format!("{record}\n")
}

fn ledger_path(env: &Env) -> PathBuf {
    env.agent().join(".dreamd/provenance/ledger.jsonl")
}

fn roots_path(env: &Env) -> PathBuf {
    env.agent().join(".dreamd/provenance/roots.jsonl")
}

/// `dreamd init`, three events in one cluster, and `dreamd dream --no-llm`:
/// the cycle writes a lesson, so the ledger holds `index_doc`,
/// `lesson_citation` and `recurrence` edges.
fn store_after_one_cycle() -> Env {
    let env = Env::new();
    std::fs::write(
        env.project().join("Cargo.toml"),
        "[package]\nname = \"stored-root\"\nversion = \"0.0.0\"\n",
    )
    .expect("project marker");
    env.ok(&["init", "--quiet"]);
    let log = [
        line(
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FA0",
            1.0,
            "rust::clippy",
            6.0,
            "clippy A: deny warnings in CI",
        ),
        line(
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FA1",
            2.0,
            "rust::clippy",
            6.0,
            "clippy B: fix needless_borrow before merging",
        ),
        line(
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FA2",
            3.0,
            "rust::clippy",
            6.0,
            "clippy C: run clippy with all targets",
        ),
    ]
    .concat();
    std::fs::write(env.agent().join("episodic/AGENT_LEARNINGS.jsonl"), log).expect("write the log");
    env.ok(&["dream", "--no-llm", "--no-commit"]);
    env
}

fn doctor_provenance(env: &Env) -> (Option<i32>, String) {
    let out = env.run(&["doctor", "--provenance"]);
    let provenance: String = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.starts_with("provenance:"))
        .map(|l| format!("{l}\n"))
        .collect();
    (out.status.code(), provenance)
}

#[test]
fn a_dream_cycle_stores_the_root_and_doctor_agrees() {
    let env = store_after_one_cycle();

    let roots = std::fs::read_to_string(roots_path(&env)).expect("the cycle stores a root");
    assert_eq!(roots.lines().count(), 1, "one stored root: {roots}");
    let stored: serde_json::Value = serde_json::from_str(roots.trim_end()).expect("JSON");
    assert_eq!(stored["schema_version"], "provenance-root/1.0");
    let ledger = std::fs::read(ledger_path(&env)).expect("the cycle writes edges");
    assert!(stored["ledger_bytes"].as_u64().unwrap() > 0);
    assert!(stored["ledger_bytes"].as_u64().unwrap() <= ledger.len() as u64);

    let (_, provenance) = doctor_provenance(&env);
    let root = stored["root"].as_str().unwrap();
    assert!(provenance.contains("provenance: ok root="), "{provenance}");
    assert!(
        provenance.contains(&format!("provenance: stored_roots=1 last_root={root} ")),
        "{provenance}"
    );
    assert!(!provenance.contains("stored_root_mismatch"), "{provenance}");
}

#[test]
fn doctor_reports_two_swapped_ledger_lines() {
    let env = store_after_one_cycle();
    let (_, before) = doctor_provenance(&env);
    assert!(before.contains("provenance: ok root="), "{before}");

    let ledger = std::fs::read_to_string(ledger_path(&env)).unwrap();
    let mut lines: Vec<&str> = ledger.lines().collect();
    assert!(lines.len() >= 2, "{ledger}");
    lines.swap(0, 1);
    std::fs::write(ledger_path(&env), lines.join("\n") + "\n").unwrap();

    let (code, after) = doctor_provenance(&env);
    assert_eq!(code, Some(1), "{after}");
    assert!(
        after.contains("provenance: stored_root_mismatch roots_line=1 reason=bytes_changed "),
        "{after}"
    );
    assert!(
        after
            .contains("[WARNING: the ledger changed after a stored root; it may have been edited]"),
        "{after}"
    );
    assert!(!after.contains("provenance: ok"), "{after}");
    // The edge checks alone see nothing: same lines, same set of events.
    assert!(!after.contains("provenance: orphan"), "{after}");
    assert!(!after.contains("provenance: corrupt"), "{after}");
}

#[test]
fn doctor_reports_a_deleted_ledger() {
    let env = store_after_one_cycle();
    std::fs::remove_file(ledger_path(&env)).unwrap();

    let (code, after) = doctor_provenance(&env);
    assert_eq!(code, Some(1), "{after}");
    assert!(
        after.contains("provenance: stored_root_mismatch roots_line=1 reason=ledger_shorter "),
        "{after}"
    );
}
