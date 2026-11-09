//! BZR-1239 crash sweep: every dream cycle and forget, crashed at each of its
//! crash points in turn, then recovered by the next `WriterLock::acquire`.
//!
//! After recovery the store must be byte-for-byte pre-cycle or post-cycle
//! (the image below), with `check_store` clean. A crash before the commit
//! point must leave it pre-cycle and `failed`; a crash that left the redo WAL
//! must leave it post-cycle and `complete`.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use chrono::DateTime;
use dreamd_protocol::{AgentLearning, EventId};

use crate::dream_cycle::{cycle_date_from_now_sec, run_filesystem_phases};
use crate::episodic;
use crate::forget;
use crate::index::RecurrenceSidecar;
use crate::invariants::check_store;
use crate::layout::AgentRoot;
use crate::lessons::{self, Lesson, LessonsFile};
use crate::retention::AUTOSNAP_KEEP;
use crate::snapshot::autosnap_ref_name;
use crate::stage::{arm_crash, arm_error, crash_fired};
use crate::test_support::{unique_tmpdir, DirGuard};
use crate::wal;
use crate::writer_lock::{WriterKind, WriterLock};

const NOW: i64 = 1_780_000_000;
const DAY: i64 = 86_400;
/// Far past the number of crash points any one run has.
const MAX_POINTS: u32 = 64;
/// `snap-*` refs `Scenario::Pruning` seeds before the cycle's own.
const SEEDED_SNAPS: i64 = 20;

type Image = BTreeMap<String, Vec<u8>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Promoting,
    /// `Promoting` plus [`SEEDED_SNAPS`] older `snap-*` refs (BZR-1245).
    Pruning,
    Retiring,
    ForgetCitation,
    ForgetExemplar,
}

fn eid(c: char) -> EventId {
    EventId::parse(&format!("evt_01ARZ3NDEKTSV4RRFFQ69G5FA{c}")).unwrap()
}

fn event(c: char, skill_action: &str, age_days: i64, pain: f32, pinned: bool) -> AgentLearning {
    AgentLearning {
        schema_version: "1.0.0".to_string(),
        id: eid(c),
        timestamp: DateTime::from_timestamp(NOW - age_days * DAY, 0).unwrap(),
        pain,
        importance: pain,
        pinned,
        skill_action: skill_action.to_string(),
        source_harness: "test".to_string(),
        content: format!("event {c}"),
    }
}

fn lines(events: &[AgentLearning]) -> Vec<u8> {
    events
        .iter()
        .flat_map(|e| format!("{}\n", serde_json::to_string(e).unwrap()).into_bytes())
        .collect()
}

fn write_lessons(root: &AgentRoot, cluster_key: &str, exemplar: char, citations: &[char]) {
    lessons::write_lessons_file(
        &root.lessons_md(),
        &LessonsFile {
            last_updated: DateTime::from_timestamp(NOW - 60 * DAY, 0).unwrap(),
            prompt_version: "deterministic-only".to_string(),
            cluster_key: cluster_key.to_string(),
            citations: citations.iter().map(|c| eid(*c).to_string()).collect(),
            lessons: vec![Lesson {
                id: eid(exemplar).to_string(),
                content: format!("event {exemplar}"),
                pinned: false,
            }],
        },
    )
    .unwrap();
}

/// A fresh `.agent/` holding `scenario`'s pre-cycle store.
fn seed(scenario: Scenario) -> (AgentRoot, DirGuard) {
    let dir = unique_tmpdir("crash-sweep");
    let root = AgentRoot::new(&dir);
    fs::create_dir_all(root.episodic_dir()).unwrap();
    fs::create_dir_all(root.semantic_dir()).unwrap();
    fs::create_dir_all(root.dreamd_dir()).unwrap();
    let log = match scenario {
        Scenario::Promoting | Scenario::Pruning => {
            fs::create_dir_all(root.snapshots_dir()).unwrap();
            let prior = event('P', "go::older", 200, 5.0, false);
            fs::write(root.snapshot_file(&date()), lines(&[prior])).unwrap();
            vec![
                event('1', "rust::eh", 1, 5.0, false),
                event('2', "rust::eh", 2, 5.0, false),
                event('3', "rust::eh", 3, 9.0, false),
                event('A', "go::old", 120, 5.0, false),
                event('B', "go::old", 120, 5.0, false),
            ]
        }
        Scenario::Retiring => {
            write_lessons(&root, "rust::stale", 'E', &['E']);
            vec![
                event('E', "rust::stale", 60, 5.0, true),
                event('R', "go::recent", 1, 5.0, false),
                event('D', "go::old", 120, 5.0, false),
            ]
        }
        Scenario::ForgetCitation | Scenario::ForgetExemplar => {
            write_lessons(&root, "rust::eh", 'X', &['X', 'Y']);
            vec![
                event('X', "rust::eh", 1, 7.0, true),
                event('Y', "rust::eh", 2, 5.0, true),
                event('Z', "go::other", 1, 5.0, false),
            ]
        }
    };
    fs::write(root.episodic_jsonl(), lines(&log)).unwrap();
    if scenario == Scenario::Pruning {
        fs::create_dir_all(root.branches_refs_dir()).unwrap();
        for i in 0..SEEDED_SNAPS {
            fs::write(
                root.branches_refs_dir().join(seeded_snap(i)),
                format!("{i:064x}\n2026-05-28T00:00:00Z\n"),
            )
            .unwrap();
        }
    }
    (root, DirGuard(dir))
}

/// The `i`th seeded `snap-*` ref name, oldest first; all are older than `NOW`.
fn seeded_snap(i: i64) -> String {
    autosnap_ref_name(NOW - (SEEDED_SNAPS - i) * 60).unwrap()
}

fn date() -> String {
    cycle_date_from_now_sec(NOW)
}

/// Every file whose bytes a cycle may change, keyed by its path under
/// `.agent/`. `state.json` is left out: its status is asserted separately.
fn image(root: &AgentRoot) -> Image {
    fn walk(agent: &Path, dir: &Path, out: &mut Image) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(agent, &path, out);
            } else {
                add(agent, &path, out);
            }
        }
    }
    fn add(agent: &Path, path: &Path, out: &mut Image) {
        if let Ok(bytes) = fs::read(path) {
            let rel = path.strip_prefix(agent).unwrap();
            out.insert(rel.to_string_lossy().into_owned(), bytes);
        }
    }

    let agent = root.agent_dir();
    let mut out = Image::new();
    for path in [
        root.episodic_jsonl(),
        root.lessons_md(),
        root.semantic_dir().join("recurrence_counts.json"),
        root.provenance_ledger(),
    ] {
        add(&agent, &path, &mut out);
    }
    walk(&agent, &root.snapshots_dir(), &mut out);
    walk(&agent, &root.branches_refs_dir(), &mut out);
    out
}

fn run(scenario: Scenario, root: &AgentRoot, lock: &WriterLock) -> Result<(), String> {
    match scenario {
        Scenario::Promoting | Scenario::Pruning | Scenario::Retiring => {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(run_filesystem_phases(
                    root,
                    lock.ledger(),
                    NOW,
                    &date(),
                    true,
                    false,
                ))
                .map(drop)
                .map_err(|e| e.to_string())
        }
        Scenario::ForgetCitation => forget::cascade(root, lock.ledger(), &eid('Y'), NOW)
            .map(drop)
            .map_err(|e| e.to_string()),
        Scenario::ForgetExemplar => forget::cascade(root, lock.ledger(), &eid('X'), NOW)
            .map(drop)
            .map_err(|e| e.to_string()),
    }
}

fn acquire(root: &AgentRoot) -> WriterLock {
    WriterLock::acquire(root, WriterKind::Dream).unwrap()
}

fn status(root: &AgentRoot) -> String {
    wal::read_last_cycle_status(root).unwrap()
}

fn log_ids(root: &AgentRoot) -> Vec<EventId> {
    episodic::read_all(&root.episodic_jsonl())
        .unwrap()
        .into_iter()
        .map(|e| e.id)
        .collect()
}

fn pinned(root: &AgentRoot, c: char) -> bool {
    episodic::read_all(&root.episodic_jsonl())
        .unwrap()
        .into_iter()
        .find(|e| e.id == eid(c))
        .unwrap_or_else(|| panic!("{c} is in the log"))
        .pinned
}

/// No WAL, no stage, and a clean store.
fn assert_settled(root: &AgentRoot, label: &str) {
    assert!(!root.wal_path().exists(), "{label}: the WAL must be gone");
    assert!(
        !root.stage_dir().exists(),
        "{label}: .dreamd/stage/ must be gone"
    );
    let violations = check_store(root, None).unwrap();
    assert!(violations.is_empty(), "{label}: {violations:?}");
}

/// Run `scenario` once with no crash, check its post-state by property, and
/// return its image.
fn clean_post(scenario: Scenario) -> Image {
    let (root, _g) = seed(scenario);
    let lock = acquire(&root);
    arm_crash(None);
    run(scenario, &root, &lock).expect("the clean run succeeds");
    drop(lock);

    let lessons = || lessons::read_lessons_file(&root.lessons_md()).unwrap();
    match scenario {
        Scenario::Promoting | Scenario::Pruning => {
            let file = lessons();
            assert_eq!(file.cluster_key, "rust::eh");
            assert_eq!(file.lessons[0].id, eid('3').to_string(), "the exemplar");
            assert!(pinned(&root, '3'), "the exemplar is pinned");
            let ids = log_ids(&root);
            assert!(!ids.contains(&eid('A')) && !ids.contains(&eid('B')));
            let archive = episodic::read_all(&root.snapshot_file(&date())).unwrap();
            let archived: Vec<EventId> = archive.into_iter().map(|e| e.id).collect();
            assert_eq!(archived, vec![eid('P'), eid('A'), eid('B')]);
            let sidecar: RecurrenceSidecar = serde_json::from_slice(
                &fs::read(root.semantic_dir().join("recurrence_counts.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(sidecar.clusters.len(), 1);
            assert_eq!(sidecar.clusters[0].skill_action, "rust::eh");
            assert_eq!(sidecar.clusters[0].count, 3);
            let refs: Vec<_> = fs::read_dir(root.branches_refs_dir())
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            if scenario == Scenario::Promoting {
                assert_eq!(refs.len(), 1, "{refs:?}");
                assert!(refs[0].starts_with("snap-"), "{refs:?}");
            } else {
                assert_eq!(refs.len(), AUTOSNAP_KEEP, "{refs:?}");
                assert!(
                    !refs.contains(&seeded_snap(0)),
                    "the oldest seeded ref is pruned"
                );
                assert!(refs.contains(&seeded_snap(1)), "{refs:?}");
                assert!(
                    refs.contains(&autosnap_ref_name(NOW).unwrap()),
                    "the new ref lands"
                );
            }
            let ledger = fs::read_to_string(root.provenance_ledger()).unwrap();
            assert!(ledger.contains("\"lesson_citation\""), "{ledger}");
            assert!(ledger.contains("\"recurrence\""), "{ledger}");
        }
        Scenario::Retiring => {
            assert!(!root.lessons_md().exists());
            assert!(!log_ids(&root).contains(&eid('D')));
            let archive = episodic::read_all(&root.snapshot_file(&date())).unwrap();
            assert_eq!(
                archive.into_iter().map(|e| e.id).collect::<Vec<_>>(),
                vec![eid('D')]
            );
        }
        Scenario::ForgetCitation => {
            assert_eq!(lessons().citations, vec![eid('X').to_string()]);
            assert!(!log_ids(&root).contains(&eid('Y')));
        }
        Scenario::ForgetExemplar => {
            assert!(!root.lessons_md().exists());
            assert!(!log_ids(&root).contains(&eid('X')));
        }
    }
    assert_eq!(status(&root), "complete");
    assert_settled(&root, "clean run");
    image(&root)
}

/// One crash point's outcome after recovery.
#[derive(Debug, Clone, Copy)]
struct Point {
    k: u32,
    landed: bool,
    wal_left: bool,
}

/// Crash `scenario` at `k = 0, 1, …` until a run finishes with no crash.
/// Returns every crashed point and the post image.
fn sweep(scenario: Scenario) -> (Vec<Point>, Image) {
    let post = clean_post(scenario);
    let mut points = Vec::new();
    for k in 0..MAX_POINTS {
        let (root, _g) = seed(scenario);
        let pre = image(&root);
        let lock = acquire(&root);
        arm_crash(Some(k));
        let result = run(scenario, &root, &lock);
        let fired = crash_fired();
        arm_crash(None);

        if !fired {
            result.unwrap_or_else(|e| panic!("k={k}: no crash fired, but the run failed: {e}"));
            assert!(
                image(&root) == post,
                "k={k}: an uncrashed run must match the clean post image"
            );
            let first_landed = points.iter().find(|p: &&Point| p.landed).map(|p| p.k);
            println!("crash_sweep {scenario:?}: final k = {k}, first landed k = {first_landed:?}");
            assert!(
                points.iter().any(|p| !p.landed),
                "{scenario:?}: no crash point ended pre-cycle"
            );
            assert!(
                points.iter().any(|p| p.landed && p.wal_left),
                "{scenario:?}: no crash point left the redo WAL"
            );
            let last_pre = points.iter().filter(|p| !p.landed).map(|p| p.k).max();
            let first_post = points.iter().filter(|p| p.landed).map(|p| p.k).min();
            if let (Some(a), Some(b)) = (last_pre, first_post) {
                assert!(
                    a < b,
                    "{scenario:?}: a pre-cycle point {a} follows a landed point {b}"
                );
            }
            return (points, post);
        }

        let err = result.expect_err("a fired crash must fail the run");
        assert!(err.contains("injected crash"), "k={k}: {err}");
        let wal_left = root.wal_path().exists();
        // A crashed process cleans up nothing (BZR-1532): record this before recovery.
        let status_at_crash = status(&root);
        let stage_at_crash = root.stage_dir().exists();
        drop(lock);
        drop(acquire(&root));

        let after = image(&root);
        let landed = after == post;
        assert!(
            landed || after == pre,
            "k={k}: the store is neither pre- nor post-cycle after recovery"
        );
        if wal_left {
            assert!(landed, "k={k}: a crash that left the WAL must roll forward");
        }
        if !landed {
            assert_eq!(
                status_at_crash, "in_progress",
                "k={k}: a crashed process cleans up nothing"
            );
            assert!(stage_at_crash, "k={k}: a crashed process cleans up nothing");
        }
        assert_eq!(
            status(&root),
            if landed { "complete" } else { "failed" },
            "k={k}"
        );
        assert_settled(&root, &format!("k={k}"));
        points.push(Point {
            k,
            landed,
            wal_left,
        });
    }
    panic!("{scenario:?}: still crashing after {MAX_POINTS} points");
}

/// Inject one I/O error at each crash point and check the store in this
/// process, with no recovery (BZR-1532).
fn error_sweep(scenario: Scenario) {
    let post = clean_post(scenario);
    let mut failed = Vec::new();
    let mut landed = Vec::new();
    let mut landed_err = Vec::new();
    for k in 0..MAX_POINTS {
        let (root, _g) = seed(scenario);
        let pre = image(&root);
        let lock = acquire(&root);
        arm_error(k);
        let result = run(scenario, &root, &lock);
        let fired = crash_fired();
        arm_crash(None);

        if !fired {
            result.unwrap_or_else(|e| panic!("k={k}: no error fired, but the run failed: {e}"));
            assert!(
                !failed.is_empty(),
                "{scenario:?}: no error point failed the cycle"
            );
            assert!(
                !landed.is_empty(),
                "{scenario:?}: no error point landed the cycle"
            );
            let max_failed = failed.iter().copied().max().unwrap();
            let min_landed = landed.iter().copied().min().unwrap();
            assert!(
                max_failed < min_landed,
                "{scenario:?}: a failed point {max_failed} follows a landed point {min_landed}"
            );
            assert_eq!(
                landed_err.len(),
                1,
                "{scenario:?}: exactly one landed point returns Err, got {landed_err:?}"
            );
            assert_eq!(
                landed_err[0],
                k - 1,
                "{scenario:?}: the landed error must be the last point"
            );
            println!(
                "error_sweep {scenario:?}: final k = {k}, failed {failed:?}, landed {landed:?}, landed with an error {landed_err:?}"
            );
            return;
        }

        assert_settled(&root, &format!("k={k}"));
        let after = image(&root);
        if after == post {
            assert_eq!(status(&root), "complete", "k={k}");
            if let Err(err) = &result {
                assert!(err.contains("injected error"), "k={k}: {err}");
                landed_err.push(k);
            }
            landed.push(k);
        } else {
            assert!(
                after == pre,
                "k={k}: the store is neither pre- nor post-cycle"
            );
            assert_eq!(status(&root), "failed", "k={k}");
            let err = result.expect_err("a fired error before the commit point fails the run");
            assert!(err.contains("injected error"), "k={k}: {err}");
            run(scenario, &root, &lock)
                .unwrap_or_else(|e| panic!("k={k}: the retry run failed: {e}"));
            assert!(
                image(&root) == post,
                "k={k}: the retry run must match the clean post image"
            );
            assert_eq!(status(&root), "complete", "k={k}");
            failed.push(k);
        }
    }
    panic!("{scenario:?}: still erroring after {MAX_POINTS} points");
}

fn obstructed(
    scenario: Scenario,
    obstruct: impl FnOnce(&AgentRoot),
    clear: impl FnOnce(&AgentRoot),
    expect: &str,
) {
    let post = clean_post(scenario);
    let (root, _g) = seed(scenario);
    // Acquire before the obstruction: recovery cannot remove a file at the stage path.
    let lock = acquire(&root);
    obstruct(&root);
    let pre = image(&root);
    let result = run(scenario, &root, &lock);

    let err = result.expect_err("the obstruction fails the cycle");
    assert!(err.contains(expect), "{err}");
    assert_eq!(image(&root), pre, "nothing live changes");
    assert_eq!(status(&root), "failed");
    assert!(
        !root.wal_path().exists(),
        "the cycle never reached its commit point"
    );
    assert!(
        !root.stage_dir().is_dir(),
        "the stage root is not a directory"
    );

    clear(&root);
    run(scenario, &root, &lock).expect("the cycle runs once the obstruction is gone");
    assert_eq!(image(&root), post);
    assert_eq!(status(&root), "complete");
    assert_settled(&root, "after the obstruction is cleared");
}

#[test]
fn crash_sweep_promoting_cycle() {
    sweep(Scenario::Promoting);
}

/// An unreadable WAL is not a commit point. Repair moves it aside and leaves
/// the pre-cycle image.
#[test]
fn repair_of_an_uncommitted_wal_leaves_the_pre_cycle_image() {
    let (root, _g) = seed(Scenario::Promoting);
    let pre = image(&root);
    let episodic = fs::read(root.episodic_jsonl()).unwrap();
    let staged = root.stage_dir().join("dream-1");
    fs::create_dir_all(&staged).unwrap();
    fs::write(staged.join("sentinel"), b"must-not-land\n").unwrap();
    let body = br#"{"intent":"faked"}"#;
    fs::write(root.wal_path(), body).unwrap();

    {
        let lock = WriterLock::acquire(&root, WriterKind::Repair).unwrap();
        assert!(lock.quarantined_wal().is_some());
    }

    assert_eq!(image(&root), pre);
    assert_eq!(status(&root), "failed");
    assert!(!root.stage_dir().exists());
    assert_eq!(fs::read(root.episodic_jsonl()).unwrap(), episodic);
    let sidecars: Vec<_> = fs::read_dir(root.dreamd_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("dream_in_progress.wal.corrupt-"))
        })
        .collect();
    assert_eq!(sidecars.len(), 1, "{sidecars:?}");
    assert_eq!(fs::read(&sidecars[0]).unwrap(), body);

    let _watch = WriterLock::acquire(&root, WriterKind::Watch).unwrap();
    assert_eq!(image(&root), pre);
}

/// BZR-1245: the ref unlink is staged, so a crash leaves all 20 seeded refs
/// or the pruned set with the cycle's own ref, never a pruned set without it.
#[test]
fn crash_sweep_promoting_cycle_prunes_a_snap_ref() {
    sweep(Scenario::Pruning);
}

#[test]
fn crash_sweep_retiring_cycle() {
    sweep(Scenario::Retiring);
}

#[test]
fn crash_sweep_forget_drops_a_citation() {
    sweep(Scenario::ForgetCitation);
}

#[test]
fn crash_sweep_forget_unlinks_the_lesson() {
    sweep(Scenario::ForgetExemplar);
}

#[test]
fn error_sweep_promoting_cycle() {
    error_sweep(Scenario::Promoting);
}

#[test]
fn error_sweep_promoting_cycle_prunes_a_snap_ref() {
    error_sweep(Scenario::Pruning);
}

#[test]
fn error_sweep_retiring_cycle() {
    error_sweep(Scenario::Retiring);
}

#[test]
fn error_sweep_forget_drops_a_citation() {
    error_sweep(Scenario::ForgetCitation);
}

#[test]
fn error_sweep_forget_unlinks_the_lesson() {
    error_sweep(Scenario::ForgetExemplar);
}

#[test]
fn a_stage_that_cannot_be_created_fails_the_cycle_in_process() {
    obstructed(
        Scenario::Promoting,
        |root| {
            fs::write(root.stage_dir(), b"not a directory").unwrap();
        },
        |root| {
            fs::remove_file(root.stage_dir()).unwrap();
        },
        "Not a directory",
    );
}

#[test]
fn an_autosnap_that_cannot_be_written_fails_the_cycle_in_process() {
    obstructed(
        Scenario::Promoting,
        |root| {
            fs::create_dir_all(root.branches_dir()).unwrap();
            fs::write(root.branches_objects_dir(), b"not a directory").unwrap();
        },
        |root| {
            fs::remove_file(root.branches_objects_dir()).unwrap();
        },
        "snapshot:",
    );
}

#[test]
fn an_unreadable_lessons_md_fails_the_cycle_in_process() {
    obstructed(
        Scenario::Promoting,
        |root| {
            fs::create_dir_all(root.lessons_md()).unwrap();
        },
        |root| {
            fs::remove_dir(root.lessons_md()).unwrap();
        },
        "Is a directory",
    );
}

#[test]
fn an_unreadable_decay_archive_fails_the_cycle_in_process() {
    let saved = std::cell::RefCell::new(None);
    obstructed(
        Scenario::Promoting,
        |root| {
            let path = root.snapshot_file(&date());
            *saved.borrow_mut() = Some(fs::read(&path).unwrap());
            fs::remove_file(&path).unwrap();
            fs::create_dir(&path).unwrap();
        },
        |root| {
            let path = root.snapshot_file(&date());
            fs::remove_dir(&path).unwrap();
            fs::write(&path, saved.borrow_mut().take().unwrap()).unwrap();
        },
        "Is a directory",
    );
}

#[test]
fn an_unparsable_lessons_md_fails_forget_in_process() {
    let saved = std::cell::RefCell::new(None);
    obstructed(
        Scenario::ForgetCitation,
        |root| {
            let path = root.lessons_md();
            *saved.borrow_mut() = Some(fs::read(&path).unwrap());
            fs::write(&path, b"\xff\xfe not utf-8").unwrap();
        },
        |root| {
            fs::write(root.lessons_md(), saved.borrow_mut().take().unwrap()).unwrap();
        },
        "lessons:",
    );
}

/// A crash during the cycle's roll-forward, then crashes during every
/// recovery's roll-forward in turn: each recovery resumes where the last one
/// stopped, and the store ends post-cycle.
#[test]
fn crash_sweep_crash_during_roll_forward() {
    let (points, post) = sweep(Scenario::Promoting);
    let wal_points: Vec<u32> = points.iter().filter(|p| p.wal_left).map(|p| p.k).collect();
    assert!(!wal_points.is_empty());

    for k in wal_points {
        let (root, _g) = seed(Scenario::Promoting);
        let lock = acquire(&root);
        arm_crash(Some(k));
        run(Scenario::Promoting, &root, &lock).expect_err("the cycle crashes");
        arm_crash(None);
        drop(lock);
        assert!(root.wal_path().exists(), "k={k}: the WAL is left");

        let mut recovered = false;
        for j in 0..MAX_POINTS {
            arm_crash(Some(j));
            let result = WriterLock::acquire(&root, WriterKind::Dream);
            let fired = crash_fired();
            arm_crash(None);
            match result {
                Err(e) => {
                    assert!(fired, "k={k} j={j}: recovery failed with no crash: {e}");
                }
                Ok(lock) => {
                    assert!(!fired, "k={k} j={j}: a crash fired but acquire succeeded");
                    drop(lock);
                    recovered = true;
                    break;
                }
            }
        }
        assert!(
            recovered,
            "k={k}: recovery still crashing after {MAX_POINTS} tries"
        );

        drop(acquire(&root));
        assert!(image(&root) == post, "k={k}: the store must end post-cycle");
        assert_eq!(status(&root), "complete", "k={k}");
        assert_settled(&root, &format!("k={k}"));
    }
}
