//! WEG-65 / DR-313 — Snapshot tests for deterministic dream-cycle output.
//!
//! Runs `run_deterministic_dream_cycle` against a frozen 7-event JSONL corpus
//! and pins the three output files: LESSONS.md, recurrence_counts.json, state.json.
//!
//! NOW_SEC = 1_747_137_600 (2025-05-13T12:00:00Z) — fixed clock; no wall-time calls.
//! Two clusters promote; Cluster B wins by salience_sum (~39.97 vs ~39.40).
//! Exemplar = evt_01ARZ3NDEKTSV4RRFFQ69G5FA4 (highest pain in B).

use std::fs;
use std::path::{Path, PathBuf};

use dreamd_core::consolidation::run_deterministic_dream_cycle;
use dreamd_core::layout::AgentRoot;

/// Fixed reference clock. All age calculations resolve against this.
/// Do NOT replace with Utc::now() — snapshot must be reproducible across machines.
const NOW_SEC: i64 = 1_747_137_600;

/// Path to the frozen fixture corpus, relative to CARGO_MANIFEST_DIR.
fn fixture_jsonl() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/dream-cycle-snapshot/AGENT_LEARNINGS.jsonl")
}

/// Scaffold a tempdir with the fixture JSONL at the correct agent path,
/// run the dream cycle, and return the AgentRoot for output reading.
fn run_cycle_on_fixture() -> (tempfile::TempDir, AgentRoot) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = AgentRoot::new(dir.path());

    // Create the episodic directory and write the fixture corpus.
    let episodic_dir = root.episodic_jsonl().parent().unwrap().to_owned();
    fs::create_dir_all(&episodic_dir).unwrap();
    fs::copy(fixture_jsonl(), root.episodic_jsonl()).expect("copy fixture");

    // Create the dreamd dir (state.json lives here).
    fs::create_dir_all(root.dreamd_dir()).unwrap();

    run_deterministic_dream_cycle(&root, NOW_SEC)
        .expect("dream cycle must succeed on valid fixture");

    (dir, root)
}

#[test]
fn snapshot_lessons_md() {
    let (_dir, root) = run_cycle_on_fixture();
    let content = fs::read_to_string(root.lessons_md())
        .expect("LESSONS.md must exist after cycle on 7-event corpus");
    // No filters needed — last_updated is derived from the fixed NOW_SEC.
    insta::assert_snapshot!("lessons_md", content);
}

#[test]
fn snapshot_recurrence_counts() {
    let (_dir, root) = run_cycle_on_fixture();
    let sidecar_path = root.semantic_dir().join("recurrence_counts.json");
    let content = fs::read_to_string(&sidecar_path)
        .expect("recurrence_counts.json must exist after cluster engine run");
    // JSON content is fully deterministic from the fixed corpus. No filters needed.
    insta::assert_snapshot!("recurrence_counts", content);
}

#[test]
fn snapshot_state_json() {
    let (_dir, root) = run_cycle_on_fixture();
    let content =
        fs::read_to_string(root.state_json()).expect("state.json must exist after cycle commit");
    // daemon_version drifts across releases — filter it out.
    insta::with_settings!({
        filters => vec![
            (r#""daemon_version": "[^"]*""#, r#""daemon_version": "<version>""#),
        ],
    }, {
        insta::assert_snapshot!("state_json", content);
    });
}
