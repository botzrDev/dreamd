//! `dreamd forget <id> [--dry-run | --proof <path>]` — remove one episodic
//! event, the lesson state built from it, and the copies of it dreamd keeps
//! (BZR-151).
//!
//! The removal is [`dreamd_core::forget::cascade`] (BZR-158). It also clears
//! the decay archive, the migrate backup and every memory branch object that
//! holds the event. This module adds the daemon guard, the index refresh and
//! merge, the git history count, and the receipt file.
//!
//! **Daemon-coexistence guard:** a live daemon holds an append fd on the JSONL
//! and the Tantivy writer lock. The cascade replaces the log by rename, so the
//! command refuses while the daemon probes live, with the same probe as
//! `dreamd archive`, and then takes the project writer lock, which refuses
//! every other writer (an in-process `dreamd mcp`, a daemon the probe cannot
//! see). `--dry-run` writes nothing, does not probe and takes no lock. Its
//! stdout says `would_remove=`, never `removed=`.
//!
//! **Index:** after a forget that cleared anything, when `.agent/.dreamd/index`
//! already exists, the command opens it, prunes this one event id and
//! re-indexes the lesson layer, then merges every segment that holds a
//! deleted document so the event's text leaves the segment files. An absent
//! index directory is left absent. The refresh runs on Windows too.
//!
//! **Git:** the command never rewrites git history. It counts the commits
//! whose dreamd files hold the id, prints the first and last, and prints a
//! `git filter-repo` command the user can run.
//!
//! **Rescan:** after the index pass, the command reads every file under
//! `.agent/` and every stored index document, deleted ones included, for the
//! event id and the event's text ([`dreamd_core::verify::rescan`]). It prints
//! `verified: complete_in_store` when nothing holds either (the ledger keeps
//! the id by design), or `verified: partial` and one `still_present:` line
//! per file that does, and exits 3. Git history and the places in
//! [`dreamd_core::verify::NOT_CHECKED`] are outside that check and listed on
//! a `not_checked:` line.
//!
//! **Receipt:** `--proof <path>` writes a `forget-receipt/1.1` file
//! ([`dreamd_core::forget::render_receipt`]), which lists what was cleared,
//! where copies remain, and what the rescan found. It is unsigned, and it is
//! not the signed membership proof in `docs/provenance.md`; this tree has no
//! provenance key.

use std::io::Write;
use std::path::Path;

use dreamd_core::copies::{self, GitScan};
use dreamd_core::forget::{
    self, ForgetError, ForgetOutcome, ForgetReport, IndexOutcome, LessonEffect, Remaining,
};
use dreamd_core::verify::{self, Status, Verification};
use dreamd_core::writer_lock::{WriterKind, WriterLock, WriterLockError};
use dreamd_core::{AgentRoot, LayoutError};
use dreamd_protocol::EventId;

/// Why a `dreamd forget` run failed.
#[derive(Debug)]
pub enum ForgetCliError {
    /// No `.agent/` store found walking up from `cwd`.
    NotFound,
    /// The positional argument is not an `evt_` event id.
    BadId(String),
    /// The daemon is live; a rewrite underneath it would be lost.
    DaemonRunning,
    /// The project writer lock is held by another writer, or recovery under
    /// it failed.
    WriterLock(WriterLockError),
    /// `state.json` says a dream cycle is in progress.
    InProgress,
    /// Any other cascade failure.
    Forget(ForgetError),
    /// The JSONL removal landed but the index refresh failed.
    Index(String),
    /// The forget ran, and the rescan found a file under `.agent/` that still
    /// holds the event id or text, or could not read one. Exit code 3.
    Partial,
    /// Output sink or receipt write failure.
    Io(std::io::Error),
}

impl From<std::io::Error> for ForgetCliError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<ForgetError> for ForgetCliError {
    fn from(e: ForgetError) -> Self {
        match e {
            ForgetError::InProgress => Self::InProgress,
            other => Self::Forget(other),
        }
    }
}

impl std::fmt::Display for ForgetCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "no .agent/ directory found"),
            Self::BadId(id) => write!(f, "{id:?} is not an event id"),
            Self::DaemonRunning => write!(f, "daemon is running; stop it first"),
            Self::WriterLock(e) => write!(f, "{e}"),
            Self::InProgress => write!(f, "dream cycle already in progress"),
            Self::Forget(e) => write!(f, "{e}"),
            Self::Index(e) => write!(f, "index: {e}"),
            Self::Partial => write!(
                f,
                "the rescan found copies of the event under .agent/; see the still_present lines"
            ),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ForgetCliError {}

/// Daemon liveness probe for the resolved socket path. Mirrors
/// `commands::archive::daemon_liveness`.
#[cfg(unix)]
fn daemon_liveness(path: &Path) -> bool {
    dreamd_core::server::is_daemon_socket_live(path)
}

/// Non-Unix probe (AILAB-192): `path` is `~/.agent/server.json`; a bounded TCP
/// connect to the published loopback address. Mirrors
/// `commands::archive::daemon_liveness`.
#[cfg(not(unix))]
fn daemon_liveness(path: &Path) -> bool {
    dreamd_core::daemon_client::tcp_daemon_is_live(path)
}

/// `dreamd forget` entry point.
///
/// `socket` is the resolved daemon address path (UDS on Unix,
/// `~/.agent/server.json` off it; `None` means "no daemon"). `now_sec` is the
/// caller's clock, passed to the cascade. Must not be called from inside a
/// Tokio runtime: the index refresh builds its own current-thread runtime.
///
/// stdout: line 1 is `removed=<bool> lesson=<effect>[ proof=<path>]`; then a
/// `cleared:` line naming the places cleared, one `remaining:` line per
/// place a copy is still kept, the `verified:` line, one `still_present:`
/// line per file the rescan found holding the event, and the `not_checked:`
/// line. stderr carries the git history note and the rewrite command.
///
/// A well-formed id found nowhere prints `removed=false` and returns `Ok`.
/// A failed index refresh or receipt write is returned after the result
/// lines; the removal is not rolled back. Otherwise a rescan that found a
/// copy returns [`ForgetCliError::Partial`].
#[allow(clippy::too_many_arguments)]
pub fn run(
    cwd: &Path,
    socket: Option<&Path>,
    id: &str,
    dry_run: bool,
    proof: Option<&Path>,
    now_sec: i64,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<(), ForgetCliError> {
    let Ok(event_id) = EventId::parse(id) else {
        writeln!(err, "dreamd: error — {id:?} is not an event id")?;
        return Err(ForgetCliError::BadId(id.to_string()));
    };

    let root = match AgentRoot::discover(cwd) {
        Ok(r) => r,
        Err(LayoutError::NotFound) => {
            writeln!(
                err,
                "dreamd: error — no .agent/ directory found. Run `dreamd init` first."
            )?;
            return Err(ForgetCliError::NotFound);
        }
    };
    let index_present = root
        .dreamd_dir()
        .join(dreamd_core::server::INDEX_DIR_NAME)
        .is_dir();

    if dry_run {
        let report = forget::preview(&root, &event_id)?;
        let git = copies::git_history(&root, &event_id);
        let index = if index_present && !report.cleared.is_empty() {
            IndexOutcome::Cleared
        } else {
            IndexOutcome::Absent
        };
        let outcome = ForgetOutcome::new(&root, &report, index, &git);
        writeln!(
            out,
            "would_remove={} lesson={}",
            report.removed,
            report.lesson.as_str()
        )?;
        write_places(out, "would_clear", &outcome)?;
        for path in touched_paths(&root, &report, index_present) {
            writeln!(out, "would_touch: {path}")?;
        }
        write_remaining(out, &outcome.remaining)?;
        write_git_note(err, &event_id, &git)?;
        writeln!(err, "dreamd: dry run; nothing written")?;
        return Ok(());
    }

    // Refuse while a live daemon holds the log: the cascade's rename would drop
    // any append the daemon makes between the cascade's read and that rename.
    if let Some(sock) = socket {
        if daemon_liveness(sock) {
            return Err(ForgetCliError::DaemonRunning);
        }
    }
    // Held through the cascade, the index refresh and the receipt.
    let lock =
        WriterLock::acquire(&root, WriterKind::Forget).map_err(ForgetCliError::WriterLock)?;

    // Held in memory only, for the rescan. Never printed or written.
    let content = verify::capture(&root, &event_id);
    let report = forget::cascade(&root, lock.ledger(), &event_id, now_sec)?;
    let git = copies::git_history(&root, &event_id);
    if report.cleared.is_empty() {
        let outcome = ForgetOutcome::new(&root, &report, IndexOutcome::Absent, &git);
        writeln!(out, "removed=false lesson={}", report.lesson.as_str())?;
        write_remaining(out, &outcome.remaining)?;
        write_git_note(err, &event_id, &git)?;
        return Ok(());
    }

    let (index, index_err) = if index_present {
        clear_index(&root, &lock, &event_id, err)?
    } else {
        (IndexOutcome::Absent, None)
    };
    let outcome = ForgetOutcome::new(&root, &report, index, &git);
    // After the commit, the index merge and every writer's shutdown.
    let verification = verify::rescan(&root, &event_id, content.as_deref());
    drop(content);

    let mut proof_err = None;
    let mut proof_written = None;
    if let Some(path) = proof {
        match write_receipt(&root, &event_id, &report, &outcome, &verification, path) {
            Ok(()) => proof_written = Some(path),
            Err(e) => proof_err = Some(e),
        }
    }

    write!(
        out,
        "removed={} lesson={}",
        report.removed,
        report.lesson.as_str()
    )?;
    if let Some(path) = proof_written {
        write!(out, " proof={}", path.display())?;
    }
    writeln!(out)?;
    write_places(out, "cleared", &outcome)?;
    write_remaining(out, &outcome.remaining)?;
    write_verification(out, &verification)?;
    write_git_note(err, &event_id, &git)?;

    match (index_err, proof_err) {
        (Some(e), _) | (None, Some(e)) => Err(e),
        (None, None) if verification.status == Status::Partial => Err(ForgetCliError::Partial),
        (None, None) => Ok(()),
    }
}

/// `verified: <status> files=<n> index_documents=<n|none> text=<check>`, one
/// `still_present: <area> <path> match=<match>` line per finding, and the
/// `not_checked:` line.
fn write_verification(out: &mut impl Write, v: &Verification) -> std::io::Result<()> {
    let docs = v
        .index_documents_read
        .map_or_else(|| "none".to_string(), |n| n.to_string());
    writeln!(
        out,
        "verified: {} files={} index_documents={docs} text={}",
        v.status.as_str(),
        v.files_read,
        v.text.as_str()
    )?;
    for f in &v.still_present {
        writeln!(
            out,
            "still_present: {} {} match={}",
            f.area.as_str(),
            f.path,
            f.matched.as_str()
        )?;
    }
    writeln!(out, "not_checked: {}", verify::NOT_CHECKED.join(" "))
}

/// `<label>: <place> <place> …`, or nothing when no place is listed.
fn write_places(out: &mut impl Write, label: &str, outcome: &ForgetOutcome) -> std::io::Result<()> {
    if outcome.cleared.is_empty() {
        return Ok(());
    }
    write!(out, "{label}:")?;
    for place in &outcome.cleared {
        write!(out, " {}", place.as_str())?;
    }
    writeln!(out)
}

/// One `remaining: <place> …` line per place a copy is still kept.
fn write_remaining(out: &mut impl Write, remaining: &[Remaining]) -> std::io::Result<()> {
    for r in remaining {
        match r {
            Remaining::GitHistory {
                commits,
                first,
                last,
                paths,
            } => writeln!(
                out,
                "remaining: git_history commits={commits} first={first} last={last} paths={}",
                paths.join(",")
            )?,
            Remaining::GitUnchecked { unchecked } => writeln!(
                out,
                "remaining: git_history unchecked (could not read the repository: {unchecked})"
            )?,
            Remaining::BranchObject {
                object,
                refs,
                head,
                reason,
            } => {
                write!(out, "remaining: branch_object {object} reason={reason}")?;
                if !refs.is_empty() {
                    write!(out, " refs={}", refs.join(","))?;
                }
                if *head {
                    write!(out, " head")?;
                }
                writeln!(out)?
            }
            Remaining::DecayArchive { file } => writeln!(out, "remaining: decay_archive {file}")?,
            Remaining::MigrateBackup { file } => writeln!(out, "remaining: migrate_backup {file}")?,
            Remaining::Index { reason } => writeln!(out, "remaining: index reason={reason}")?,
        }
    }
    Ok(())
}

/// When git history holds the id: how many commits, and the command that
/// would rewrite them, with what a rewrite does not reach. dreamd runs none
/// of it.
fn write_git_note(err: &mut impl Write, id: &EventId, git: &GitScan) -> std::io::Result<()> {
    let GitScan::Scanned(history) = git else {
        return Ok(());
    };
    if history.commits == 0 {
        return Ok(());
    }
    let short = |s: &Option<String>| {
        s.as_deref()
            .unwrap_or("")
            .chars()
            .take(12)
            .collect::<String>()
    };
    let range = if history.commits == 1 {
        format!("commit {}", short(&history.first))
    } else {
        format!(
            "{} commits, from {} to {}",
            history.commits,
            short(&history.first),
            short(&history.last)
        )
    };
    writeln!(
        err,
        "dreamd: git history still holds {id} in {range}. dreamd does not rewrite git history."
    )?;
    writeln!(
        err,
        "dreamd: to remove it, commit or stash your changes, then run this in {}:",
        history.workdir.display()
    )?;
    writeln!(err, "  {}", copies::rewrite_command(id))?;
    writeln!(
        err,
        "dreamd: that needs git-filter-repo (a separate install). It rewrites every commit id on every \
         branch, ends with `git reset --hard`, and refuses a repository that is not a fresh clone \
         unless you add --force."
    )?;
    writeln!(
        err,
        "dreamd: clones, forks and remotes keep their copies until each one rewrites too, and \
         pushing the rewrite needs --force. Hosted copies (pull requests, caches) can keep it longer."
    )?;
    Ok(())
}

/// The paths a forget writes, relative to the project root, for `--dry-run`.
fn touched_paths(root: &AgentRoot, report: &ForgetReport, index_present: bool) -> Vec<String> {
    let rel = |p: &Path| {
        p.strip_prefix(root.project_root())
            .unwrap_or(p)
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/")
    };
    let mut paths = Vec::new();
    if report.removed {
        paths.push(rel(&root.episodic_jsonl()));
        if report.lesson != LessonEffect::Untouched {
            paths.push(rel(&root.lessons_md()));
        }
        paths.push(rel(&root.semantic_dir().join("recurrence_counts.json")));
    }
    for file in &report.copies.files {
        paths.push(rel(&file.path));
    }
    for obj in &report.copies.objects {
        if report.left.objects.iter().any(|left| left.id == obj.id) {
            continue;
        }
        paths.push(rel(&root.branches_objects_dir().join(&obj.id)));
        for name in &obj.refs {
            paths.push(rel(&root.branches_refs_dir().join(name)));
        }
        if obj.head {
            paths.push(rel(&root
                .branches_dir()
                .join(dreamd_core::snapshot::HEAD_FILE)));
        }
    }
    if index_present && !report.cleared.is_empty() {
        paths.push(format!(
            "{}/",
            rel(&root.dreamd_dir().join(dreamd_core::server::INDEX_DIR_NAME))
        ));
    }
    paths
}

/// Recompute the ledger root and write the `forget-receipt/1.1` file.
fn write_receipt(
    root: &AgentRoot,
    event_id: &EventId,
    report: &ForgetReport,
    outcome: &ForgetOutcome,
    verification: &Verification,
    path: &Path,
) -> Result<(), ForgetCliError> {
    let provenance = dreamd_core::provenance::verify(root)
        .map_err(|e| ForgetCliError::Io(std::io::Error::other(e.to_string())))?;
    let text = forget::render_receipt(event_id, report, outcome, verification, &provenance);
    dreamd_core::io::write_atomic(path, text.as_bytes())?;
    Ok(())
}

/// Refresh the index, then merge away the deleted documents' bytes. A failed
/// refresh is an error; a failed merge is a warning on `err` and a
/// `remaining: index` line, because the event no longer appears in recall
/// either way.
fn clear_index(
    root: &AgentRoot,
    lock: &WriterLock,
    event_id: &EventId,
    err: &mut impl Write,
) -> Result<(IndexOutcome, Option<ForgetCliError>), ForgetCliError> {
    if let Err(e) = refresh_index(root, lock, event_id) {
        return Ok((IndexOutcome::RefreshFailed, Some(e)));
    }
    let outcome = match dreamd_core::server::compact_deletes(root, lock) {
        Ok(c) if c.is_clean() => IndexOutcome::Cleared,
        Ok(c) if c.segments_with_deletes > 0 => IndexOutcome::MergePending,
        Ok(_) => IndexOutcome::FilesNotRemoved,
        Err(e) => {
            writeln!(err, "dreamd: warning — index merge failed: {e}")?;
            IndexOutcome::MergePending
        }
    };
    Ok((outcome, None))
}

/// Prune the forgotten event from an existing index and re-index the lesson
/// layer through a `TantivyIndexHandle` opened under the writer lock.
fn refresh_index(
    root: &AgentRoot,
    lock: &WriterLock,
    event_id: &EventId,
) -> Result<(), ForgetCliError> {
    use dreamd_core::server::{TantivyIndexHandle, DEFAULT_COMMIT_CADENCE};

    let index_err = |e: dreamd_core::server::IndexError| ForgetCliError::Index(e.to_string());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let handle =
            TantivyIndexHandle::open(root, lock, DEFAULT_COMMIT_CADENCE).map_err(index_err)?;
        let phases = async {
            handle.prune_decayed_events(vec![event_id.clone()]).await?;
            handle.index_semantic_lessons(root.clone()).await
        }
        .await;
        // Drain the writer even when a phase failed, so the runtime can drop.
        let drained = handle.shutdown().await;
        phases.map_err(index_err)?;
        drained.map_err(index_err)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use dreamd_core::episodic::{self, rewrite_atomic};
    use dreamd_protocol::AgentLearning;
    use std::fs;
    use std::path::PathBuf;

    // 2025-07-03Z; event timestamps match so nothing is aged.
    const NOW_SEC: i64 = 1_751_500_800;

    fn evt_id(suffix: char) -> String {
        format!("evt_01ARZ3NDEKTSV4RRFFQ69G5FA{suffix}")
    }

    fn learning(suffix: char, content: &str) -> AgentLearning {
        AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: EventId::parse(&evt_id(suffix)).unwrap(),
            timestamp: DateTime::<Utc>::from_timestamp(NOW_SEC, 0).unwrap(),
            pain: 6.0,
            importance: 7.0,
            pinned: false,
            skill_action: "rust::forget".to_string(),
            source_harness: "test-harness".to_string(),
            content: content.to_string(),
        }
    }

    fn seed(records: &[AgentLearning]) -> (tempfile::TempDir, AgentRoot, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(tmp.path());
        let jsonl = root.episodic_jsonl();
        fs::create_dir_all(jsonl.parent().unwrap()).unwrap();
        let bytes: String = records
            .iter()
            .map(|r| format!("{}\n", serde_json::to_string(r).unwrap()))
            .collect();
        rewrite_atomic(&jsonl, bytes.as_bytes()).unwrap();
        (tmp, root, jsonl)
    }

    fn ids(jsonl: &Path) -> Vec<String> {
        episodic::read_all(jsonl)
            .unwrap()
            .into_iter()
            .map(|e| e.id.to_string())
            .collect()
    }

    fn run_capture(
        cwd: &Path,
        socket: Option<&Path>,
        id: &str,
        dry_run: bool,
        proof: Option<&Path>,
    ) -> (Result<(), ForgetCliError>, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let r = run(cwd, socket, id, dry_run, proof, NOW_SEC, &mut out, &mut err);
        (
            r,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn display_names_bad_id_and_daemon() {
        assert_eq!(
            ForgetCliError::DaemonRunning.to_string(),
            "daemon is running; stop it first"
        );
        let bad = ForgetCliError::BadId("nope".to_string()).to_string();
        assert!(
            bad.contains("nope") && bad.contains("not an event id"),
            "{bad}"
        );
    }

    #[test]
    fn unparseable_id_is_bad_id() {
        let (tmp, _root, _jsonl) = seed(&[learning('A', "alpha")]);
        let (r, out, err) = run_capture(tmp.path(), None, "evt_nope", false, None);
        assert!(matches!(r, Err(ForgetCliError::BadId(_))));
        assert!(out.is_empty());
        assert!(err.contains("not an event id"), "{err}");
    }

    #[test]
    fn dry_run_prints_preview_and_writes_nothing() {
        let (tmp, root, jsonl) = seed(&[learning('A', "alpha"), learning('B', "beta")]);
        let before = fs::read(&jsonl).unwrap();

        let (r, out, err) = run_capture(tmp.path(), None, &evt_id('A'), true, None);

        r.unwrap();
        assert_eq!(
            out,
            "would_remove=true lesson=untouched\n\
             would_clear: live_log recurrence_sidecar\n\
             would_touch: .agent/episodic/AGENT_LEARNINGS.jsonl\n\
             would_touch: .agent/semantic/recurrence_counts.json\n"
        );
        assert!(
            !out.contains("removed="),
            "dry-run must not say the event was removed: {out}"
        );
        assert!(err.contains("dreamd: dry run; nothing written"), "{err}");
        assert_eq!(fs::read(&jsonl).unwrap(), before);
        assert!(!root.wal_path().exists());
    }

    #[test]
    fn write_removes_the_id_and_closes_the_wal() {
        let (tmp, root, jsonl) = seed(&[learning('A', "alpha"), learning('B', "beta")]);

        let (r, out, _err) = run_capture(tmp.path(), None, &evt_id('A'), false, None);

        r.unwrap();
        assert_eq!(
            out,
            "removed=true lesson=untouched\ncleared: live_log recurrence_sidecar\n\
             verified: complete_in_store files=3 index_documents=none text=too_short\n\
             not_checked: git_history_contents git_reflogs_and_unreachable_objects git_remotes_and_clones mcp_client_transcripts llm_providers daemon_home backups_and_sync freed_disk_blocks\n"
        );
        assert_eq!(ids(&jsonl), vec![evt_id('B')]);
        assert!(!root.wal_path().exists());
    }

    #[test]
    fn missing_id_is_ok_and_writes_no_proof() {
        let (tmp, _root, jsonl) = seed(&[learning('A', "alpha")]);
        let proof = tmp.path().join("receipt.json");

        let (r, out, _err) = run_capture(tmp.path(), None, &evt_id('Z'), false, Some(&proof));

        r.unwrap();
        assert_eq!(out, "removed=false lesson=untouched\n");
        assert!(!proof.exists());
        assert_eq!(ids(&jsonl), vec![evt_id('A')]);
    }

    #[test]
    fn proof_receipt_keeps_the_old_ledger_line_and_names_it_as_the_orphan() {
        let (tmp, root, _jsonl) = seed(&[learning('A', "alpha"), learning('B', "beta")]);
        let line = format!(
            "{{\"schema_version\":\"provenance/1.0\",\"kind\":\"index_doc\",\"from\":\"{0}\",\"to\":\"{0}\"}}\n",
            evt_id('A')
        );
        fs::create_dir_all(root.provenance_dir()).unwrap();
        fs::write(root.provenance_ledger(), &line).unwrap();
        let proof = tmp.path().join("receipt.json");

        let (r, out, _err) = run_capture(tmp.path(), None, &evt_id('A'), false, Some(&proof));

        r.unwrap();
        assert_eq!(
            out,
            format!(
                "removed=true lesson=untouched proof={}\ncleared: live_log recurrence_sidecar\n\
                 verified: complete_in_store files=4 index_documents=none text=too_short\n\
                 not_checked: git_history_contents git_reflogs_and_unreachable_objects git_remotes_and_clones mcp_client_transcripts llm_providers daemon_home backups_and_sync freed_disk_blocks\n",
                proof.display()
            )
        );
        let ledger = fs::read_to_string(root.provenance_ledger()).unwrap();
        assert!(ledger.contains(line.trim_end()), "{ledger}");

        let text = fs::read_to_string(&proof).unwrap();
        assert!(!text.contains("\"signature\""), "{text}");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["schema_version"], "forget-receipt/1.1");
        assert_eq!(
            v["cleared"],
            serde_json::json!(["live_log", "recurrence_sidecar"])
        );
        assert_eq!(v["remaining"], serde_json::json!([]));
        assert_eq!(v["event_id"], evt_id('A'));
        assert_eq!(
            v["root"],
            dreamd_core::provenance::verify(&root).unwrap().root
        );
        let orphans = v["orphans"].as_array().unwrap();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0]["line"], 1);
        assert_eq!(orphans[0]["kind"], "index_doc");
        assert_eq!(orphans[0]["from"], evt_id('A'));
        assert_eq!(orphans[0]["to"], evt_id('A'));
    }

    #[cfg(unix)]
    #[test]
    fn live_daemon_socket_refuses_and_leaves_the_log() {
        let (tmp, _root, jsonl) = seed(&[learning('A', "alpha")]);
        let sock = tmp.path().join("dreamd.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let before = fs::read(&jsonl).unwrap();

        let (r, _out, _err) = run_capture(tmp.path(), Some(&sock), &evt_id('A'), false, None);

        assert!(matches!(r, Err(ForgetCliError::DaemonRunning)));
        assert_eq!(fs::read(&jsonl).unwrap(), before);
    }

    /// `forget` replaces the log by rename. While `dreamd watch` holds the
    /// project it must refuse, whatever socket path the probe looked at.
    #[cfg(unix)]
    #[test]
    fn refuses_while_watch_holds_the_project() {
        let (tmp, root, _jsonl) = seed(&[
            learning('A', "alpha"),
            learning('B', "beta"),
            learning('C', "gamma"),
        ]);
        dreamd_core::dream_cycle::run_in_process(tmp.path(), NOW_SEC, true, true, false)
            .expect("seed cycle");
        let before = crate::commands::store_bytes(&root);

        let _watch = WriterLock::acquire(&root, WriterKind::Watch).unwrap();
        let (r, _out, _err) = run_capture(tmp.path(), None, &evt_id('A'), false, None);

        let err = r.unwrap_err();
        assert!(matches!(err, ForgetCliError::WriterLock(_)), "got {err:?}");
        assert!(
            err.to_string()
                .contains(&format!("pid {} (watch)", std::process::id())),
            "{err}"
        );
        assert_eq!(crate::commands::store_bytes(&root), before);
    }

    #[test]
    fn absent_index_directory_stays_absent() {
        let (tmp, root, _jsonl) = seed(&[learning('A', "alpha"), learning('B', "beta")]);
        let index_dir = root.dreamd_dir().join("index");
        assert!(!index_dir.exists());

        let (r, _out, _err) = run_capture(tmp.path(), None, &evt_id('A'), false, None);

        r.unwrap();
        assert!(!index_dir.exists());
    }

    #[test]
    fn in_progress_status_maps_to_in_progress() {
        assert!(matches!(
            ForgetCliError::from(ForgetError::InProgress),
            ForgetCliError::InProgress
        ));
    }

    /// An `in_progress` with no WAL is stale: no cycle can run without the
    /// project writer lock, which `forget` holds. Taking the lock resets it,
    /// and the forget runs.
    #[test]
    fn stale_in_progress_is_reset_and_the_forget_runs() {
        let (tmp, root, jsonl) = seed(&[learning('A', "alpha"), learning('B', "beta")]);
        dreamd_core::daemon_state::update_daemon_state(&root, |s| {
            s.last_dream_cycle_status = "in_progress".to_string()
        })
        .unwrap();

        let (r, out, _err) = run_capture(tmp.path(), None, &evt_id('A'), false, None);

        r.unwrap();
        assert_eq!(
            out,
            "removed=true lesson=untouched\ncleared: live_log recurrence_sidecar\n\
             verified: complete_in_store files=3 index_documents=none text=too_short\n\
             not_checked: git_history_contents git_reflogs_and_unreachable_objects git_remotes_and_clones mcp_client_transcripts llm_providers daemon_home backups_and_sync freed_disk_blocks\n"
        );
        assert_eq!(ids(&jsonl), vec![evt_id('B')]);
        assert_ne!(
            dreamd_core::wal::read_last_cycle_status(&root).unwrap(),
            "in_progress"
        );
    }

    #[cfg(unix)]
    #[test]
    fn forgotten_event_leaves_the_index() {
        use dreamd_core::server::{TantivyIndexHandle, DEFAULT_COMMIT_CADENCE};

        let (tmp, root, _jsonl) = seed(&[
            learning('A', "unique forget needle"),
            learning('B', "unrelated beta"),
        ]);
        let hits = |root: &AgentRoot| -> Vec<String> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            // Released after the shutdown below, before `forget` takes it.
            let lock = WriterLock::acquire(root, WriterKind::Watch).unwrap();
            rt.block_on(async {
                let handle = TantivyIndexHandle::open(root, &lock, DEFAULT_COMMIT_CADENCE).unwrap();
                handle.reader().reload().unwrap();
                let (_schema, fields) = dreamd_core::index::build_schema();
                let ids: Vec<String> = dreamd_core::recall(
                    handle.reader(),
                    &fields,
                    "unique forget needle",
                    10,
                    NOW_SEC,
                )
                .unwrap()
                .into_iter()
                .map(|r| r.event_id)
                .collect();
                handle.shutdown().await.unwrap();
                ids
            })
        };

        assert!(hits(&root).contains(&evt_id('A')));

        let (r, out, _err) = run_capture(tmp.path(), None, &evt_id('A'), false, None);
        r.unwrap();
        assert_eq!(
            out,
            "removed=true lesson=untouched\ncleared: live_log recurrence_sidecar index\n\
             verified: complete_in_store files=17 index_documents=1 text=checked\n\
             not_checked: git_history_contents git_reflogs_and_unreachable_objects git_remotes_and_clones mcp_client_transcripts llm_providers daemon_home backups_and_sync freed_disk_blocks\n"
        );

        assert!(!hits(&root).contains(&evt_id('A')));
    }
}
