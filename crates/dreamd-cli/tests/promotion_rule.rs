//! DRM-117 (LP-GEARS-01): a dream cycle promotes the deepest cluster on its
//! own events, and `doctor --cluster-health` agrees with what it wrote.
//! Before the fix, `rust::eh` promoted on the three events `rust::eh::axum`
//! had claimed, took the fresh one-off, and became the cycle's lesson.
//!
//! The test drives the real binary with no daemon, in the shape of
//! `decay_pinned_duplicate.rs`. Every child gets its own `HOME`,
//! `XDG_CONFIG_HOME` (an empty tempdir) and `DREAMD_SOCK` (a path nothing
//! listens on), plus `DREAMD_LOG=off`, `GIT_CONFIG_NOSYSTEM=1` and
//! `GIT_CONFIG_GLOBAL=/dev/null`, with `DREAMD_LAUNCHER`, `DREAMD_LOG_FORMAT`
//! and `SOURCE_DATE_EPOCH` removed, so the cycle runs on the wall clock the
//! records are dated from, as in `dream_autobiography.rs`. The test
//! process's own environment is never changed. The log is written by hand:
//! an import, as a fixture may. Unix-only: `DREAMD_SOCK` is a Unix socket.

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

/// DRM-117 test 7 (the lever-pass 2a store): three old `rust::eh::axum`
/// events and one fresh, painful `rust::eh::tokio` one-off. The cycle files
/// its lesson under `rust::eh::axum`, the sidecar counts only that cluster,
/// and doctor finds no drift.
#[test]
fn dream_promotes_the_deepest_cluster_and_doctor_agrees() {
    let env = Env::new();
    std::fs::write(
        env.project().join("Cargo.toml"),
        "[package]\nname = \"promotion\"\nversion = \"0.0.0\"\n",
    )
    .expect("project marker");
    env.ok(&["init", "--quiet"]);

    let log = [
        line(
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FA0",
            25.0,
            "rust::eh::axum",
            5.0,
            "axum A: return impl IntoResponse from handlers",
        ),
        line(
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FA1",
            26.0,
            "rust::eh::axum",
            5.0,
            "axum B: map errors with thiserror",
        ),
        line(
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FA2",
            27.0,
            "rust::eh::axum",
            5.0,
            "axum C: never unwrap in a handler",
        ),
        line(
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FA3",
            0.1,
            "rust::eh::tokio",
            9.0,
            "ONE-OFF: tokio select! cancelled my future once",
        ),
    ]
    .concat();
    std::fs::write(env.agent().join("episodic/AGENT_LEARNINGS.jsonl"), log).expect("write the log");

    env.ok(&["dream", "--no-llm", "--no-commit"]);

    let lessons = std::fs::read_to_string(env.agent().join("semantic/LESSONS.md"))
        .expect("the cycle writes LESSONS.md");
    assert!(
        lessons.contains("cluster_key: \"rust::eh::axum\"\n"),
        "the lesson is filed under the deepest cluster:\n{lessons}"
    );

    let sidecar: serde_json::Value = serde_json::from_slice(
        &std::fs::read(env.agent().join("semantic/recurrence_counts.json"))
            .expect("the cycle writes the sidecar"),
    )
    .expect("the sidecar is JSON");
    assert_eq!(
        sidecar["clusters"],
        serde_json::json!([{ "skill_action": "rust::eh::axum", "count": 3 }]),
        "the sidecar counts only the deepest cluster: {sidecar}"
    );

    let doctor = env.run(&["doctor", "--cluster-health"]);
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    assert!(
        stdout.lines().any(|l| l.starts_with("cluster_health: ok")),
        "doctor agrees with the sidecar: {}",
        describe(&doctor)
    );
}
