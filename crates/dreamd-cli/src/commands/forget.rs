//! `dreamd forget <id> [--dry-run | --proof <path>]` — remove one episodic
//! event and the lesson state that names it (BZR-151).
//!
//! The removal is [`dreamd_core::forget::cascade`] (BZR-158). This module adds
//! the daemon guard, the index refresh, and the receipt file.
//!
//! **Daemon-coexistence guard:** a live daemon holds an append fd on the JSONL
//! and the Tantivy writer lock. The cascade replaces the log by rename, so the
//! command refuses while the daemon probes live, with the same probe as
//! `dreamd archive`. `--dry-run` writes nothing and does not probe.
//!
//! **Index:** after a removal, when `.agent/.dreamd/index` already exists, the
//! command opens it and sends the three messages the coordinator sends after a
//! forget: recurrence sidecar (when present), prune this one event id, re-index
//! the lesson layer. An absent index directory is left absent. Off Unix there
//! is no index step (the cascade itself returns `Unsupported` there, inherited
//! from `write_atomic`).
//!
//! **Receipt:** `--proof <path>` writes a `forget-receipt/1.0` file
//! ([`dreamd_core::forget::render_receipt`]). It is not the signed membership
//! proof in `docs/provenance.md`; this tree has no provenance key.

use std::io::Write;
use std::path::Path;

use dreamd_core::forget::{self, ForgetError, ForgetReport};
use dreamd_core::{AgentRoot, LayoutError};
use dreamd_protocol::EventId;

#[derive(Debug)]
pub enum ForgetCliError {
    /// No `.agent/` store found walking up from `cwd`.
    NotFound,
    /// The positional argument is not an `evt_` event id.
    BadId(String),
    /// The daemon is live; a rewrite underneath it would be lost.
    DaemonRunning,
    /// `state.json` says a dream cycle is in progress.
    InProgress,
    /// Any other cascade failure.
    Forget(ForgetError),
    /// The JSONL removal landed but the index refresh failed.
    Index(String),
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
            Self::InProgress => write!(f, "dream cycle already in progress"),
            Self::Forget(e) => write!(f, "{e}"),
            Self::Index(e) => write!(f, "index: {e}"),
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
/// A well-formed id that is not in the log prints `removed=false` and returns
/// `Ok`. A failed index refresh or receipt write is returned after the result
/// line; the JSONL removal is not rolled back.
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

    if dry_run {
        let report = forget::preview(&root, &event_id)?;
        writeln!(
            out,
            "dry-run removed={} lesson={}",
            report.removed,
            report.lesson.as_str()
        )?;
        writeln!(err, "dreamd: dry run; nothing written")?;
        return Ok(());
    }

    // Refuse while a live daemon holds the log: the cascade's rename would
    // leave the daemon appending to the old inode.
    if let Some(sock) = socket {
        if daemon_liveness(sock) {
            return Err(ForgetCliError::DaemonRunning);
        }
    }

    let report = forget::cascade(&root, &event_id, now_sec)?;
    if !report.removed {
        writeln!(out, "removed=false lesson={}", report.lesson.as_str())?;
        return Ok(());
    }

    let index_err = refresh_index(&root, &event_id).err();

    let mut proof_err = None;
    let mut proof_written = None;
    if let Some(path) = proof {
        match write_receipt(&root, &event_id, report, path) {
            Ok(()) => proof_written = Some(path),
            Err(e) => proof_err = Some(e),
        }
    }

    write!(out, "removed=true lesson={}", report.lesson.as_str())?;
    if let Some(path) = proof_written {
        write!(out, " proof={}", path.display())?;
    }
    writeln!(out)?;

    match (index_err, proof_err) {
        (Some(e), _) | (None, Some(e)) => Err(e),
        (None, None) => Ok(()),
    }
}

/// Recompute the ledger root and write the `forget-receipt/1.0` file.
fn write_receipt(
    root: &AgentRoot,
    event_id: &EventId,
    report: ForgetReport,
    path: &Path,
) -> Result<(), ForgetCliError> {
    let provenance = dreamd_core::provenance::verify(root)
        .map_err(|e| ForgetCliError::Io(std::io::Error::other(e.to_string())))?;
    let text = forget::render_receipt(event_id, report.lesson, &provenance);
    dreamd_core::io::write_atomic(path, text.as_bytes())?;
    Ok(())
}

/// Send the coordinator's post-forget index messages to an existing index.
/// An absent index directory is left absent.
#[cfg(unix)]
fn refresh_index(root: &AgentRoot, event_id: &EventId) -> Result<(), ForgetCliError> {
    use dreamd_core::server::{TantivyIndexHandle, DEFAULT_COMMIT_CADENCE, INDEX_DIR_NAME};

    if !root.dreamd_dir().join(INDEX_DIR_NAME).exists() {
        return Ok(());
    }
    let index_err = |e: dreamd_core::server::IndexError| ForgetCliError::Index(e.to_string());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let handle = TantivyIndexHandle::open(root, DEFAULT_COMMIT_CADENCE).map_err(index_err)?;
        let phases = async {
            if root.semantic_dir().join("recurrence_counts.json").exists() {
                handle.apply_recurrence_sidecar(root).await?;
            }
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

#[cfg(not(unix))]
fn refresh_index(_root: &AgentRoot, _event_id: &EventId) -> Result<(), ForgetCliError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use dreamd_core::episodic::{self, rewrite_atomic};
    use dreamd_protocol::AgentLearning;
    use std::fs;
    use std::path::PathBuf;

    // 2026-07-03Z; event timestamps match so nothing is aged.
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
        rewrite_atomic(&jsonl, records).unwrap();
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
        assert_eq!(out, "dry-run removed=true lesson=untouched\n");
        assert!(err.contains("dreamd: dry run; nothing written"), "{err}");
        assert_eq!(fs::read(&jsonl).unwrap(), before);
        assert!(!root.wal_path().exists());
    }

    #[test]
    fn write_removes_the_id_and_closes_the_wal() {
        let (tmp, root, jsonl) = seed(&[learning('A', "alpha"), learning('B', "beta")]);

        let (r, out, _err) = run_capture(tmp.path(), None, &evt_id('A'), false, None);

        r.unwrap();
        assert_eq!(out, "removed=true lesson=untouched\n");
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
            format!("removed=true lesson=untouched proof={}\n", proof.display())
        );
        let ledger = fs::read_to_string(root.provenance_ledger()).unwrap();
        assert!(ledger.contains(line.trim_end()), "{ledger}");

        let text = fs::read_to_string(&proof).unwrap();
        assert!(!text.contains("\"signature\""), "{text}");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["schema_version"], "forget-receipt/1.0");
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
        let (tmp, root, _jsonl) = seed(&[learning('A', "alpha")]);
        dreamd_core::daemon_state::update_daemon_state(&root, |s| {
            s.last_dream_cycle_status = "in_progress".to_string()
        })
        .unwrap();

        let (r, _out, _err) = run_capture(tmp.path(), None, &evt_id('A'), false, None);

        assert!(matches!(r, Err(ForgetCliError::InProgress)));
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
            rt.block_on(async {
                let handle = TantivyIndexHandle::open(root, DEFAULT_COMMIT_CADENCE).unwrap();
                handle.reader().reload().unwrap();
                let (_schema, fields) = dreamd_core::index::build_schema();
                let ids: Vec<String> = dreamd_core::recall(
                    handle.reader(),
                    &fields,
                    "unique forget needle",
                    10,
                    None,
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
        assert_eq!(out, "removed=true lesson=untouched\n");

        assert!(!hits(&root).contains(&evt_id('A')));
    }
}
