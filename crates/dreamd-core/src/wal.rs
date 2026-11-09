//! Dream cycle write-ahead log (WEG-60; redo format, BZR-1239).
//!
//! Guarantees `.agent/` is never left in a half-promoted state after a
//! mid-cycle crash. A cycle stages every output under
//! `.agent/.dreamd/stage/<cycle>/` ([`crate::stage::Stage`]) while nothing live
//! changes. [`commit_cycle`] then writes `dream_in_progress.wal` once — a redo
//! log of the cycle's renames, unlinks and ledger edges ([`RedoWal`], format
//! `redo/1`) — and that write is the commit point. The renames, unlinks,
//! directory fsyncs, the append of the WAL's edges and the `complete` status
//! follow, and the WAL is deleted. The edges are the cycle's plan
//! ([`crate::plan`]); nothing derives them from the files the commit puts live.
//!
//! When a writer takes the project lock, [`recover_if_needed`] rolls a redo WAL
//! forward through the same code (`roll_forward`), keeps the old meaning of a
//! legacy `intents` WAL ([`DreamWal`]), refuses any other WAL, and wipes every
//! stage directory. A `dreamd doctor --repair` writer moves an unreadable WAL
//! aside instead ([`quarantine_unreadable`]).
//!
//! The redo WAL covers the JSONL, `LESSONS.md`, the recurrence sidecar, the
//! decay archive, the autosnap ref and the unlinks of the old autosnap refs
//! that [`crate::retention`] prunes. The autosnap object itself is written
//! directly, outside the WAL. Tantivy index mutations are not WAL-protected —
//! see [`ARCHITECTURE.md`](../../ARCHITECTURE.md) §4 (index freshness
//! contract).

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::daemon_state::{read_daemon_state, update_daemon_state, DaemonStateError};
use crate::io::write_atomic;
use crate::layout::AgentRoot;
use crate::provenance::{Kind, Ledger, ProvenanceError};
use crate::stage::{crash_point, Stage};

/// Schema version for daemon state (`state.json`). Re-exported from
/// [`crate::daemon_state`], which owns the typed writer (AILAB-167);
/// `crate::wal::STATE_SCHEMA_VERSION` remains a valid path.
pub use crate::daemon_state::STATE_SCHEMA_VERSION;

/// `format` of the WAL [`commit_cycle`] writes.
pub const REDO_FORMAT: &str = "redo/1";

/// The redo WAL: every rename, unlink and ledger edge one cycle commits. Paths
/// are relative to `.agent/` and `/`-joined. A `from` is a staged file under
/// `.dreamd/stage/`; a `to` or an unlink is a live path outside it.
///
/// It has no `intents` key, so a reader built on [`DreamWal`] rejects it and an
/// older binary refuses to start rather than misread it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RedoWal {
    /// [`STATE_SCHEMA_VERSION`], `"1.0"`.
    pub schema_version: String,
    /// [`REDO_FORMAT`].
    pub format: String,
    pub renames: Vec<RedoRename>,
    pub unlinks: Vec<String>,
    /// The plan's ledger edges, in order. `roll_forward` appends them after
    /// the renames, at commit and again at recovery. Required: a `redo/1` WAL
    /// without the key fails closed.
    pub edges: Vec<RedoEdge>,
}

/// One staged file moving into place.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RedoRename {
    pub from: String,
    pub to: String,
}

/// One ledger edge a redo WAL appends.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RedoEdge {
    pub kind: Kind,
    pub from: String,
    pub to: String,
}

/// What opened a stage. Its token prefixes the stage directory name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleKind {
    Dream,
    Forget,
}

impl CycleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dream => "dream",
            Self::Forget => "forget",
        }
    }
}

impl fmt::Display for CycleKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One step of a **legacy** (pre-1.2.0) intents WAL. Nothing writes these any
/// more; [`recover_if_needed`] still reads them from a WAL an older binary left.
///
/// Without [`WalIntent::Commit`] — the legacy commit marker — recovery deletes
/// each step's temp file and the WAL, so `.agent/` stays pre-cycle.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "operation", content = "payload")]
pub enum WalIntent {
    /// A temp `LESSONS.md` (or semantic memory file) was about to be renamed in.
    ReplaceSemanticMemory { temp_file_path: String },
    /// A pruned temp copy of the episodic JSONL was about to be renamed in.
    PruneEpisodicMemory { temp_file_path: String },
    /// The legacy commit marker: every step had run.
    Commit,
}

/// A **legacy** (pre-1.2.0) WAL: an ordered list of [`WalIntent`]s. Read only
/// by recovery. [`commit_cycle`] writes a [`RedoWal`] instead.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DreamWal {
    /// Daemon state schema version (independent of episodic `schema_version`).
    pub schema_version: String,
    /// Ordered list of intents the older binary recorded.
    pub intents: Vec<WalIntent>,
}

/// Result of scanning / applying `dream_in_progress.wal` on startup.
#[derive(Debug, PartialEq)]
pub enum RecoveryOutcome {
    /// No WAL found — nothing to recover. Any stage directory was wiped.
    Clean,
    /// A committed redo WAL was applied: `renamed` renames and `unlinked`
    /// unlinks were still to do.
    RolledForward { renamed: usize, unlinked: usize },
    /// Legacy WAL with no `Commit`: temp files removed, WAL deleted.
    Recovered { cleaned_files: Vec<PathBuf> },
    /// Legacy WAL with `Commit` whose cleanup did not finish; state.json repaired.
    CommittedButUnclean,
}

/// Errors from beginning, committing, classifying or recovering a cycle.
#[derive(Debug, thiserror::Error)]
pub enum WalError {
    /// A file operation on the WAL, the stage or a live path failed.
    #[error("WAL I/O: {0}")]
    Io(#[from] std::io::Error),
    /// The WAL or `state.json` is not valid JSON, or does not have the shape
    /// its `format` names.
    #[error("WAL parse: {0}")]
    Json(#[from] serde_json::Error),
    /// The project root has no `.agent/` directory; a cycle never creates one.
    #[error("no .agent/ store at {0} — refusing to scaffold a phantom store; run `dreamd init`")]
    NoAgentStore(PathBuf),
    /// A cycle started while a WAL is still on disk.
    #[error(
        "a dream-cycle WAL is pending at {0}; the next writer to take the project lock recovers it"
    )]
    Pending(PathBuf),
    /// The WAL's `format` is not one this version reads (`"none"` when the
    /// WAL has neither `format` nor `intents`).
    #[error("dream-cycle WAL has unknown format {0:?}; refusing to touch the store")]
    UnknownFormat(String),
    /// A redo path that is absolute, climbs with `..`, or points at the wrong
    /// side of `.dreamd/stage/`.
    #[error("dream-cycle WAL names a path outside the store: {0:?}; refusing to touch the store")]
    BadRedoPath(String),
    /// Appending the WAL's edges to the provenance ledger failed.
    #[error("WAL provenance: {0}")]
    Provenance(#[from] ProvenanceError),
}

impl From<DaemonStateError> for WalError {
    fn from(e: DaemonStateError) -> Self {
        match e {
            DaemonStateError::Io(e) => WalError::Io(e),
            DaemonStateError::Json(e) => WalError::Json(e),
        }
    }
}

/// Set state.json to `in_progress` and open an empty stage at
/// `.dreamd/stage/<kind>-<now_sec>/`. Writes no WAL.
///
/// Refuses with [`WalError::Pending`] while a WAL exists: that WAL belongs to
/// a committed cycle the next lock holder must roll forward first.
pub fn begin_cycle(
    agent_root: &AgentRoot,
    kind: CycleKind,
    now_sec: i64,
) -> Result<Stage, WalError> {
    // WEG-281 — never conjure a store. Under an existing `.agent/` this may
    // create `.dreamd/`; if `.agent/` is absent the caller resolved the wrong
    // root (or skipped discovery). Last line of defense behind the cli.rs
    // discover gate.
    if !agent_root.agent_dir().exists() {
        return Err(WalError::NoAgentStore(agent_root.agent_dir()));
    }
    let wal_path = agent_root.wal_path();
    if wal_path.exists() {
        return Err(WalError::Pending(wal_path));
    }
    // Status-only patch, deliberately (AILAB-167): a cycle that is starting has
    // not committed, so the previous cycle's `last_dream_cycle_at` must
    // survive.
    // BZR-1532: once `in_progress` may be on disk, every error before the
    // commit point sets `failed` in this process.
    if let Err(e) = update_daemon_state(agent_root, |s| {
        s.last_dream_cycle_status = "in_progress".to_string()
    }) {
        abort_uncommitted(agent_root);
        return Err(e.into());
    }
    match Stage::create(agent_root, &format!("{kind}-{now_sec}")) {
        Ok(stage) => Ok(stage),
        Err(e) => {
            abort_uncommitted(agent_root);
            Err(e.into())
        }
    }
}

/// Begin a cycle, stage its outputs with `build`, and commit them
/// ([`begin_cycle`], then `build`, then [`commit_cycle`]). An error from
/// `build` ends the cycle in this process before it returns: the stage root
/// is removed and the status set to `failed` (BZR-1532). Every production
/// cycle goes through here.
pub fn stage_and_commit<T, E>(
    agent_root: &AgentRoot,
    kind: CycleKind,
    now_sec: i64,
    ledger: &Ledger,
    build: impl FnOnce(&mut Stage) -> Result<T, E>,
) -> Result<T, E>
where
    E: From<WalError>,
{
    let mut stage = begin_cycle(agent_root, kind, now_sec)?;
    let out = match build(&mut stage) {
        Ok(out) => out,
        Err(e) => {
            abort_uncommitted(agent_root);
            return Err(e);
        }
    };
    commit_cycle(stage, ledger, now_sec)?;
    Ok(out)
}

/// End a cycle that failed before its commit point, in the process that
/// began it (BZR-1532). It does what the next writer's recovery would do
/// with no WAL, then sets the status to `failed`:
///
/// 1. remove the WAL's temp file;
/// 2. remove the stage root;
/// 3. set `last_dream_cycle_status` to `failed`, leaving `last_dream_cycle_at`.
///
/// Each step runs even when an earlier one failed; a failure is logged at
/// `warn`, and the caller returns its own error either way. Never called once
/// the WAL is at its path. The caller holds the project writer lock, so no
/// other cycle is staging.
///
/// Test builds: after an injected crash the process is dead, so nothing here
/// runs, and the next writer's recovery handles the store (`crash_sweep.rs`).
fn abort_uncommitted(agent_root: &AgentRoot) {
    if crash_point().is_err() {
        return;
    }
    match fs::remove_file(agent_root.wal_path().with_extension("tmp")) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(error = %e, "could not remove a failed dream cycle's WAL temp"),
    }
    if let Err(e) = wipe_stage(agent_root) {
        tracing::warn!(error = %e, "could not remove a failed dream cycle's stage");
    }
    if let Err(e) = update_daemon_state(agent_root, |s| {
        s.last_dream_cycle_status = "failed".to_string()
    }) {
        tracing::warn!(error = %e, "could not set a failed dream cycle's status to failed");
    }
}

/// Commit `stage`: fsync it, write the redo WAL (the commit point), apply it
/// with `roll_forward`, then remove the stage directory.
///
/// An error before the WAL is at its path removes the stage and sets `failed`
/// in this process (BZR-1532). An error after that commit point is retried
/// once with `roll_forward`; if the retry fails too, the WAL stays for the
/// next writer.
///
/// `ledger` is the project writer lock's appender; the WAL's provenance edges
/// are appended after the renames. `now_sec` stamps `last_dream_cycle_at`.
pub fn commit_cycle(stage: Stage, ledger: &Ledger, now_sec: i64) -> Result<(), WalError> {
    let agent_root = stage.agent_root().clone();
    let redo = stage.redo();
    let applied = match write_redo(&stage, &redo) {
        Ok(()) => roll_forward(&agent_root, &redo, ledger, Some(now_sec)).map(drop),
        // The commit point is the WAL at its path, not this call's result.
        Err(e) if !agent_root.wal_path().exists() => {
            abort_uncommitted(&agent_root);
            return Err(e);
        }
        Err(e) => Err(e),
    };
    if let Err(first) = applied {
        retry_roll_forward(&agent_root, &redo, ledger, now_sec, first)?;
    }

    remove_dir_all_if_present(stage.dir())?;
    match fs::remove_dir(agent_root.stage_dir()) {
        Ok(()) => {}
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) => {}
        Err(e) => return Err(e.into()),
    }
    crash_point()?;
    Ok(())
}

/// Fsync the stage and write the redo WAL (the commit point).
fn write_redo(stage: &Stage, redo: &RedoWal) -> Result<(), WalError> {
    sync_dir_if_present(stage.dir())?;
    crash_point()?;
    let json = serde_json::to_string_pretty(redo)?;
    write_atomic(&stage.agent_root().wal_path(), json.as_bytes())?;
    crash_point()?;
    Ok(())
}

/// BZR-1532: one more `roll_forward` after an error past the commit point.
/// `roll_forward` is idempotent, so the retry finishes whatever the first
/// attempt left. A second failure leaves the WAL for the next writer to take
/// the project lock.
///
/// Test builds: after an injected crash the process is dead, so no retry runs.
fn retry_roll_forward(
    agent_root: &AgentRoot,
    redo: &RedoWal,
    ledger: &Ledger,
    now_sec: i64,
    first: WalError,
) -> Result<(), WalError> {
    if crash_point().is_err() {
        return Err(first);
    }
    tracing::warn!(error = %first, "dream-cycle commit failed after its commit point; retrying the roll-forward once");
    roll_forward(agent_root, redo, ledger, Some(now_sec)).map(drop)
}

/// Apply `redo`: the one code path for commit and recovery. Idempotent: a
/// rename whose `from` is gone already happened, and an unlink of an absent
/// file is done. Returns how many renames and unlinks it performed.
///
/// `cycle_at` stamps `last_dream_cycle_at` on commit. Recovery passes `None`
/// and sets the status only.
fn roll_forward(
    agent_root: &AgentRoot,
    redo: &RedoWal,
    ledger: &Ledger,
    cycle_at: Option<i64>,
) -> Result<(usize, usize), WalError> {
    validate(redo)?;
    let agent_dir = agent_root.agent_dir();
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();

    let mut renamed = 0;
    for rename in &redo.renames {
        let from = agent_dir.join(&rename.from);
        let to = agent_dir.join(&rename.to);
        if let Some(parent) = to.parent() {
            dirs.insert(parent.to_path_buf());
        }
        match fs::symlink_metadata(&from) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        }
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&from, &to)?;
        renamed += 1;
        crash_point()?;
    }

    let mut unlinked = 0;
    for unlink in &redo.unlinks {
        let path = agent_dir.join(unlink);
        if let Some(parent) = path.parent() {
            dirs.insert(parent.to_path_buf());
        }
        match fs::remove_file(&path) {
            Ok(()) => unlinked += 1,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        crash_point()?;
    }

    for dir in &dirs {
        sync_dir_if_present(dir)?;
    }
    crash_point()?;

    let edges: Vec<(Kind, String, String)> = redo
        .edges
        .iter()
        .map(|e| (e.kind, e.from.clone(), e.to.clone()))
        .collect();
    ledger.append(&edges)?;
    crash_point()?;

    let cycle_at = cycle_at.map(|now_sec| {
        DateTime::from_timestamp(now_sec, 0)
            .map(|dt: DateTime<Utc>| dt.to_rfc3339())
            .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".to_string())
    });
    update_daemon_state(agent_root, |s| {
        s.last_dream_cycle_status = "complete".to_string();
        if let Some(iso) = cycle_at {
            s.last_dream_cycle_at = Some(iso);
        }
    })?;
    crash_point()?;

    match fs::remove_file(agent_root.wal_path()) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    sync_dir_if_present(&agent_root.dreamd_dir())?;
    crash_point()?;

    Ok((renamed, unlinked))
}

/// Fail closed on any redo path that could reach outside the store: absolute,
/// non-plain components (`..`, `.`), a `from` outside `.dreamd/stage/`, or a
/// `to` / unlink inside it.
fn validate(redo: &RedoWal) -> Result<(), WalError> {
    for rename in &redo.renames {
        if !is_plain_relative(&rename.from) || !is_staged(&rename.from) {
            return Err(WalError::BadRedoPath(rename.from.clone()));
        }
        if !is_plain_relative(&rename.to) || points_into_stage(&rename.to) {
            return Err(WalError::BadRedoPath(rename.to.clone()));
        }
    }
    for unlink in &redo.unlinks {
        if !is_plain_relative(unlink) || points_into_stage(unlink) {
            return Err(WalError::BadRedoPath(unlink.clone()));
        }
    }
    Ok(())
}

fn is_plain_relative(path: &str) -> bool {
    let path = Path::new(path);
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path.components().all(|c| matches!(c, Component::Normal(_)))
}

fn points_into_stage(path: &str) -> bool {
    let mut parts = Path::new(path).components();
    parts.next() == Some(Component::Normal(".dreamd".as_ref()))
        && parts.next() == Some(Component::Normal("stage".as_ref()))
}

/// `.dreamd/stage/<something>…`: a staged file, never the stage root itself.
fn is_staged(path: &str) -> bool {
    points_into_stage(path) && Path::new(path).components().count() > 2
}

fn sync_dir_if_present(dir: &Path) -> io::Result<()> {
    match crate::io::sync_dir(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn remove_dir_all_if_present(dir: &Path) -> io::Result<()> {
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Remove `.dreamd/stage/` and everything a crashed or uncommitted cycle
/// staged there.
fn wipe_stage(agent_root: &AgentRoot) -> io::Result<()> {
    let stage_dir = agent_root.stage_dir();
    if stage_dir.exists() {
        tracing::info!(
            path = %stage_dir.display(),
            "removing staged outputs of a dream cycle that never committed"
        );
        remove_dir_all_if_present(&stage_dir)?;
    }
    Ok(())
}

/// What [`classify`] found. It has not been applied.
#[derive(Debug)]
pub enum WalClass {
    /// No `dream_in_progress.wal`.
    Absent,
    /// `redo/1` whose paths passed validation: none absolute or climbing, every
    /// `from` staged, no `to` or unlink inside the stage.
    Redo(RedoWal),
    /// A legacy `intents` body.
    Legacy(DreamWal),
}

/// Read the WAL and classify it. Does not rename, wipe, or write `state.json`.
///
/// [`recover_if_needed`] applies a [`WalClass::Redo`] or [`WalClass::Legacy`].
/// [`UnknownFormat`](WalError::UnknownFormat), [`BadRedoPath`](WalError::BadRedoPath)
/// and [`Json`](WalError::Json) return here, before any of that.
pub fn classify(agent_root: &AgentRoot) -> Result<WalClass, WalError> {
    let wal_path = agent_root.wal_path();
    if !wal_path.exists() {
        return Ok(WalClass::Absent);
    }
    let value: serde_json::Value = serde_json::from_str(&fs::read_to_string(&wal_path)?)?;
    match value.get("format") {
        Some(format) if format == REDO_FORMAT => {
            let redo: RedoWal = serde_json::from_value(value)?;
            validate(&redo)?;
            Ok(WalClass::Redo(redo))
        }
        Some(format) => Err(WalError::UnknownFormat(
            format
                .as_str()
                .map_or_else(|| format.to_string(), str::to_owned),
        )),
        None if value.get("intents").is_some() => {
            let wal: DreamWal = serde_json::from_value(value)?;
            Ok(WalClass::Legacy(wal))
        }
        None => Err(WalError::UnknownFormat("none".to_string())),
    }
}

/// `true` for the three errors that mean the WAL cannot be read:
/// [`WalError::UnknownFormat`], [`WalError::BadRedoPath`] and
/// [`WalError::Json`]. [`WriterLock::acquire`](crate::writer_lock::WriterLock::acquire)
/// quarantines the WAL on these only for a
/// [`WriterKind::Repair`](crate::writer_lock::WriterKind::Repair) writer.
pub(crate) fn is_unreadable_wal(err: &WalError) -> bool {
    matches!(
        err,
        WalError::UnknownFormat(_) | WalError::BadRedoPath(_) | WalError::Json(_)
    )
}

/// Move an unreadable WAL aside. The caller holds the project writer lock.
///
/// Re-reads the file. A body [`classify`] accepts, or a file that is already
/// gone, is left in place and returns `Ok(None)`. [`Io`](WalError::Io) and
/// [`Provenance`](WalError::Provenance) are returned as-is: a `redo/1` that
/// failed during apply may already have renamed.
///
/// Otherwise `rename`s the file to `dream_in_progress.wal.corrupt-YYYY-MM-DD`
/// (UTC, `-2`, `-3`, … when that name exists), wipes `.dreamd/stage/`, and
/// sets `last_dream_cycle_status` to `failed`. The file keeps its bytes; it
/// is renamed, not rewritten. The WAL path is not removed except by that
/// rename. A rename failure wipes
/// nothing and writes no status.
pub fn quarantine_unreadable(agent_root: &AgentRoot) -> Result<Option<PathBuf>, WalError> {
    let wal_path = agent_root.wal_path();
    if !wal_path.exists() {
        return Ok(None);
    }
    match classify(agent_root) {
        Err(err) if is_unreadable_wal(&err) => {}
        Err(err) => return Err(err),
        Ok(_) => return Ok(None),
    }
    let dest = sidecar_path(&wal_path)?;
    fs::rename(&wal_path, &dest)?;
    wipe_stage(agent_root)?;
    update_daemon_state(agent_root, |s| {
        s.last_dream_cycle_status = "failed".to_string()
    })?;
    Ok(Some(dest))
}

/// `dream_in_progress.wal.corrupt-YYYY-MM-DD`, then `-2`, `-3`, … Never an
/// existing path.
fn sidecar_path(wal_path: &Path) -> io::Result<PathBuf> {
    let date = Utc::now().format("%Y-%m-%d");
    let first = wal_path.with_file_name(format!("dream_in_progress.wal.corrupt-{date}"));
    if !first.exists() {
        return Ok(first);
    }
    let mut n = 2u32;
    loop {
        let candidate =
            wal_path.with_file_name(format!("dream_in_progress.wal.corrupt-{date}-{n}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
        n = n.saturating_add(1);
    }
}

/// Bring the store to a pre- or post-cycle state.
///
/// - No WAL: remove a WAL temp left by a crash inside the WAL's own write,
///   wipe `.dreamd/stage/`, return [`RecoveryOutcome::Clean`].
/// - A `redo/1` WAL: `roll_forward` it, wipe the stage root, return
///   [`RecoveryOutcome::RolledForward`].
/// - A legacy WAL (`intents`, no `format`): the pre-1.2.0 recovery, then wipe
///   the stage root.
/// - Anything [`classify`] refuses: [`WalError::UnknownFormat`],
///   [`WalError::BadRedoPath`] or [`WalError::Json`], touching nothing.
pub fn recover_if_needed(
    agent_root: &AgentRoot,
    ledger: &Ledger,
) -> Result<RecoveryOutcome, WalError> {
    let wal_path = agent_root.wal_path();
    match classify(agent_root)? {
        WalClass::Absent => {
            match fs::remove_file(wal_path.with_extension("tmp")) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            wipe_stage(agent_root)?;
            Ok(RecoveryOutcome::Clean)
        }
        WalClass::Redo(redo) => {
            let (renamed, unlinked) = roll_forward(agent_root, &redo, ledger, None)?;
            wipe_stage(agent_root)?;
            Ok(RecoveryOutcome::RolledForward { renamed, unlinked })
        }
        WalClass::Legacy(wal) => {
            let outcome = recover_legacy(agent_root, &wal)?;
            wipe_stage(agent_root)?;
            Ok(outcome)
        }
    }
}

/// The pre-1.2.0 recovery of an intents WAL.
fn recover_legacy(agent_root: &AgentRoot, wal: &DreamWal) -> Result<RecoveryOutcome, WalError> {
    let wal_path = agent_root.wal_path();
    let committed = wal.intents.contains(&WalIntent::Commit);

    if committed {
        update_daemon_state(agent_root, |s| {
            s.last_dream_cycle_status = "complete".to_string()
        })?;
        std::fs::remove_file(&wal_path)?;
        return Ok(RecoveryOutcome::CommittedButUnclean);
    }

    let mut cleaned = Vec::new();
    for intent in &wal.intents {
        let tmp_path = match intent {
            WalIntent::ReplaceSemanticMemory { temp_file_path } => PathBuf::from(temp_file_path),
            WalIntent::PruneEpisodicMemory { temp_file_path } => PathBuf::from(temp_file_path),
            WalIntent::Commit => continue,
        };
        if tmp_path.exists() {
            std::fs::remove_file(&tmp_path)?;
            cleaned.push(tmp_path.clone());
        }
        let tmp_neighbour = tmp_path.with_extension("tmp");
        if tmp_neighbour.exists() {
            std::fs::remove_file(&tmp_neighbour)?;
            cleaned.push(tmp_neighbour);
        }
    }

    std::fs::remove_file(&wal_path)?;
    update_daemon_state(agent_root, |s| {
        s.last_dream_cycle_status = "failed".to_string()
    })?;
    Ok(RecoveryOutcome::Recovered {
        cleaned_files: cleaned,
    })
}

/// [`recover_if_needed`] with logging.
///
/// Called only by [`WriterLock::acquire`](crate::writer_lock::WriterLock::acquire),
/// after the project writer lock is held and before any store access, so a
/// WAL or stage directory it finds cannot belong to a cycle that is still
/// running. `ledger` is the lock's appender: a roll-forward appends the WAL's
/// provenance edges, and the ledger skips any it already holds.
pub fn recover_on_startup(
    agent_root: &AgentRoot,
    ledger: &Ledger,
) -> Result<RecoveryOutcome, WalError> {
    let outcome = recover_if_needed(agent_root, ledger)?;
    match &outcome {
        RecoveryOutcome::Clean => {}
        RecoveryOutcome::RolledForward { renamed, unlinked } => {
            tracing::info!(renamed, unlinked, "rolled a committed dream cycle forward");
        }
        RecoveryOutcome::Recovered { cleaned_files } => {
            tracing::info!(
                count = cleaned_files.len(),
                "recovered incomplete dream cycle"
            );
        }
        RecoveryOutcome::CommittedButUnclean => {
            tracing::info!("dream cycle committed; finished WAL cleanup on startup");
        }
    }
    Ok(outcome)
}

/// Read `last_dream_cycle_status` from `state.json`.
///
/// Returns `"idle"` if the file is absent or the key is missing, through
/// [`DaemonState`](crate::daemon_state::DaemonState)`::default` and its
/// container-level `#[serde(default)]`.
pub fn read_last_cycle_status(agent_root: &AgentRoot) -> Result<String, WalError> {
    Ok(read_daemon_state(agent_root)?.last_dream_cycle_status)
}

/// Read `last_dream_cycle_at` from `state.json`.
///
/// The value is the clock of the last cycle that committed, as RFC 3339: the
/// `now_sec` the cycle planned against, which [`commit_cycle`] stamps when the
/// commit lands. Despite this function's name, [`begin_cycle`] does not write
/// it, a failed cycle leaves it unchanged, and so does a roll-forward at
/// recovery.
///
/// Returns `None` if the file is absent, the key is missing, or the value is
/// JSON null. Reads through the same typed state as
/// [`read_last_cycle_status`]; the writer is [`update_daemon_state`].
pub fn read_cycle_started_at(agent_root: &AgentRoot) -> Result<Option<String>, WalError> {
    Ok(read_daemon_state(agent_root)?.last_dream_cycle_at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_state::{read_daemon_state, AutobiographySkip};
    use crate::stage::{arm_crash, arm_error, crash_fired};
    use crate::test_support::{unique_tmpdir, DirGuard};
    use std::fs;

    fn setup_root(label: &str) -> (AgentRoot, DirGuard) {
        let dir = unique_tmpdir(label);
        fs::create_dir_all(&dir).unwrap();
        let root = AgentRoot::new(&dir);
        fs::create_dir_all(root.dreamd_dir()).unwrap();
        let guard = DirGuard(dir);
        (root, guard)
    }

    fn ledger(root: &AgentRoot) -> Ledger {
        Ledger::open(root.provenance_ledger())
    }

    fn status(root: &AgentRoot) -> String {
        read_daemon_state(root).unwrap().last_dream_cycle_status
    }

    fn write_wal(root: &AgentRoot, json: &str) {
        fs::write(root.wal_path(), json).unwrap();
    }

    const NOW_SEC: i64 = 1747137600; // 2026-05-13T12:00:00Z

    #[test]
    fn begin_cycle_sets_in_progress_and_writes_no_wal() {
        let (root, _g) = setup_root("begin");
        let stage = begin_cycle(&root, CycleKind::Dream, NOW_SEC).unwrap();

        assert!(
            !root.wal_path().exists(),
            "begin_cycle must write no WAL; the WAL is the commit point"
        );
        assert_eq!(status(&root), "in_progress");
        assert_eq!(
            stage.dir(),
            root.stage_dir().join(format!("dream-{NOW_SEC}"))
        );
        assert!(stage.dir().is_dir());
    }

    #[test]
    fn begin_cycle_refuses_while_a_wal_is_pending() {
        let (root, _g) = setup_root("begin-pending");
        write_wal(
            &root,
            r#"{"schema_version":"1.0","format":"redo/1","renames":[],"unlinks":[],"edges":[]}"#,
        );

        let err = begin_cycle(&root, CycleKind::Forget, NOW_SEC).unwrap_err();

        assert!(
            matches!(&err, WalError::Pending(p) if *p == root.wal_path()),
            "got {err:?}"
        );
        assert_eq!(status(&root), "idle", "a refused begin sets no status");
        assert!(!root.stage_dir().exists(), "a refused begin stages nothing");
    }

    #[test]
    fn a_build_error_discards_the_stage_and_sets_failed() {
        let (root, _g) = setup_root("build-error");
        fs::create_dir_all(root.episodic_dir()).unwrap();
        fs::write(root.episodic_jsonl(), b"live log\n").unwrap();
        update_daemon_state(&root, |s| {
            s.last_dream_cycle_status = "complete".to_string();
            s.last_dream_cycle_at = Some("2026-05-01T00:00:00+00:00".to_string());
        })
        .unwrap();
        fs::write(root.wal_path().with_extension("tmp"), b"{\"partial").unwrap();
        let ledger = ledger(&root);

        let err = stage_and_commit(
            &root,
            CycleKind::Dream,
            NOW_SEC,
            &ledger,
            |stage| -> Result<(), WalError> {
                stage
                    .put(&root.episodic_jsonl(), b"staged\n")
                    .map_err(WalError::Io)?;
                Err(WalError::Io(io::Error::other("build failed")))
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("build failed"), "{err}");
        assert_eq!(fs::read(root.episodic_jsonl()).unwrap(), b"live log\n");
        assert!(!root.stage_dir().exists(), "the stage root is removed");
        assert!(!root.wal_path().exists(), "no WAL");
        assert!(
            !root.wal_path().with_extension("tmp").exists(),
            "the WAL temp is removed"
        );
        let state = read_daemon_state(&root).unwrap();
        assert_eq!(state.last_dream_cycle_status, "failed");
        assert_eq!(
            state.last_dream_cycle_at.as_deref(),
            Some("2026-05-01T00:00:00+00:00")
        );

        stage_and_commit(
            &root,
            CycleKind::Dream,
            NOW_SEC,
            &ledger,
            |stage| -> Result<(), WalError> {
                stage
                    .put(&root.episodic_jsonl(), b"second\n")
                    .map_err(WalError::Io)?;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(fs::read(root.episodic_jsonl()).unwrap(), b"second\n");
        assert_eq!(status(&root), "complete");
    }

    #[test]
    fn abort_runs_every_step_when_an_earlier_one_fails() {
        let (root, _g) = setup_root("abort-steps");
        let tmp = root.wal_path().with_extension("tmp");
        fs::create_dir(&tmp).unwrap();
        fs::write(tmp.join("inside"), b"x").unwrap();

        let err = stage_and_commit(
            &root,
            CycleKind::Dream,
            NOW_SEC,
            &ledger(&root),
            |_stage| -> Result<(), WalError> {
                Err(WalError::Io(io::Error::other("build failed")))
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("build failed"), "{err}");
        assert!(!root.stage_dir().exists(), "the stage root is removed");
        assert_eq!(status(&root), "failed");
    }

    #[test]
    fn a_stage_that_cannot_be_created_sets_failed() {
        let (root, _g) = setup_root("stage-file");
        fs::write(root.stage_dir(), b"not a directory").unwrap();

        let err = begin_cycle(&root, CycleKind::Dream, NOW_SEC).unwrap_err();

        assert!(err.to_string().contains("Not a directory"), "{err}");
        assert_eq!(status(&root), "failed");
        assert!(!root.wal_path().exists());
    }

    #[test]
    fn an_error_after_the_wal_write_is_retried_and_commits() {
        let (root, _g) = setup_root("retry-commits");
        fs::create_dir_all(root.episodic_dir()).unwrap();
        let mut stage = begin_cycle(&root, CycleKind::Dream, NOW_SEC).unwrap();
        stage.put(&root.episodic_jsonl(), b"staged\n").unwrap();

        // Point 0 is the stage-dir fsync; point 1 is right after the WAL write.
        arm_error(1);
        let result = commit_cycle(stage, &ledger(&root), NOW_SEC);
        let fired = crash_fired();
        arm_crash(None);

        assert!(fired, "the injected error must fire");
        result.expect("the retry commits");
        assert_eq!(fs::read(root.episodic_jsonl()).unwrap(), b"staged\n");
        assert!(!root.wal_path().exists());
        assert!(!root.stage_dir().exists());
        assert_eq!(status(&root), "complete");
    }

    #[test]
    fn a_second_failure_after_the_commit_point_leaves_the_wal_for_recovery() {
        let (root, _g) = setup_root("retry-fails");
        fs::create_dir_all(root.episodic_dir()).unwrap();
        fs::write(root.provenance_dir(), b"not a directory").unwrap();
        let ledger = ledger(&root);
        let mut stage = begin_cycle(&root, CycleKind::Dream, NOW_SEC).unwrap();
        stage.put(&root.episodic_jsonl(), b"staged\n").unwrap();
        stage.record_edges(&[(
            Kind::IndexDoc,
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
        )]);

        let err = commit_cycle(stage, &ledger, NOW_SEC).unwrap_err();
        assert!(root.wal_path().exists(), "the WAL stays: {err}");
        assert_eq!(status(&root), "in_progress");
        assert_eq!(fs::read(root.episodic_jsonl()).unwrap(), b"staged\n");

        fs::remove_file(root.provenance_dir()).unwrap();
        let outcome = recover_if_needed(&root, &ledger).unwrap();
        assert!(
            matches!(outcome, RecoveryOutcome::RolledForward { .. }),
            "{outcome:?}"
        );
        assert_eq!(status(&root), "complete");
        assert!(!root.wal_path().exists());
    }

    #[test]
    fn commit_applies_renames_and_unlinks_then_deletes_the_wal() {
        let (root, _g) = setup_root("commit");
        fs::create_dir_all(root.semantic_dir()).unwrap();
        fs::write(root.lessons_md(), b"old lesson\n").unwrap();
        let jsonl = root.episodic_jsonl();

        let mut stage = begin_cycle(&root, CycleKind::Dream, NOW_SEC).unwrap();
        stage.put(&jsonl, b"new log\n").unwrap();
        stage.unlink(&root.lessons_md()).unwrap();
        assert!(!jsonl.exists(), "nothing live changes before the commit");
        commit_cycle(stage, &ledger(&root), NOW_SEC).unwrap();

        assert_eq!(fs::read(&jsonl).unwrap(), b"new log\n");
        assert!(!root.lessons_md().exists(), "the unlink must land");
        assert!(
            !root.wal_path().exists(),
            "WAL must be deleted after commit_cycle"
        );
        assert!(
            !root.stage_dir().exists(),
            "commit must remove the stage dir and the empty stage root"
        );
        let state = read_daemon_state(&root).unwrap();
        assert_eq!(state.last_dream_cycle_status, "complete");
        assert!(
            state.last_dream_cycle_at.is_some(),
            "last_dream_cycle_at must be set after commit"
        );
    }

    #[test]
    fn commit_writes_one_redo_wal_with_no_intents_key() {
        let (root, _g) = setup_root("commit-redo-bytes");
        let mut stage = begin_cycle(&root, CycleKind::Dream, NOW_SEC).unwrap();
        stage.put(&root.episodic_jsonl(), b"staged\n").unwrap();

        // Crash point 0 is the stage-dir fsync; point 1 is right after the
        // WAL write.
        arm_crash(Some(1));
        let err = commit_cycle(stage, &ledger(&root), NOW_SEC);
        arm_crash(None);
        assert!(err.is_err(), "the injected crash must surface");

        let bytes = fs::read_to_string(root.wal_path()).unwrap();
        assert!(bytes.contains("\"format\": \"redo/1\""), "{bytes}");
        assert!(!bytes.contains("\"intents\""), "{bytes}");
        let redo: RedoWal = serde_json::from_str(&bytes).unwrap();
        assert_eq!(redo.schema_version, STATE_SCHEMA_VERSION);
        assert_eq!(
            redo.renames,
            vec![RedoRename {
                from: format!(".dreamd/stage/dream-{NOW_SEC}/0-AGENT_LEARNINGS.jsonl"),
                to: "episodic/AGENT_LEARNINGS.jsonl".to_string(),
            }]
        );
        assert!(redo.edges.is_empty(), "{bytes}");
        assert!(
            !root.episodic_jsonl().exists(),
            "the crash came before any rename"
        );
    }

    const EVT_A: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV";

    /// The two edges a promoting cycle over one exemplar plans.
    fn two_edges() -> Vec<(Kind, String, String)> {
        vec![
            (
                Kind::LessonCitation,
                EVT_A.to_string(),
                format!("lsn_{EVT_A}"),
            ),
            (Kind::Recurrence, EVT_A.to_string(), "rust::eh".to_string()),
        ]
    }

    #[test]
    fn commit_writes_the_stage_edges_into_the_wal() {
        let (root, _g) = setup_root("commit-redo-edges");
        let mut stage = begin_cycle(&root, CycleKind::Dream, NOW_SEC).unwrap();
        stage.put(&root.episodic_jsonl(), b"staged\n").unwrap();
        stage.record_edges(&two_edges());

        // Point 1 is right after the WAL write, before any rename or append.
        arm_crash(Some(1));
        let err = commit_cycle(stage, &ledger(&root), NOW_SEC);
        arm_crash(None);
        assert!(err.is_err(), "the injected crash must surface");

        let redo: RedoWal = serde_json::from_slice(&fs::read(root.wal_path()).unwrap()).unwrap();
        assert_eq!(
            redo.edges,
            vec![
                RedoEdge {
                    kind: Kind::LessonCitation,
                    from: EVT_A.to_string(),
                    to: format!("lsn_{EVT_A}"),
                },
                RedoEdge {
                    kind: Kind::Recurrence,
                    from: EVT_A.to_string(),
                    to: "rust::eh".to_string(),
                },
            ]
        );
        assert!(
            !root.provenance_ledger().exists(),
            "the crash came before the append"
        );
    }

    /// The roll-forward appends what the WAL lists; there is no `LESSONS.md`
    /// or sidecar on disk to derive edges from.
    #[test]
    fn roll_forward_appends_the_wal_edges_without_reading_lessons_md() {
        let (root, _g) = setup_root("roll-forward-edges");
        write_wal(
            &root,
            &format!(
                r#"{{"schema_version":"1.0","format":"redo/1","renames":[],"unlinks":[],"edges":[{{"kind":"lesson_citation","from":"{EVT_A}","to":"lsn_{EVT_A}"}},{{"kind":"recurrence","from":"{EVT_A}","to":"rust::eh"}}]}}"#
            ),
        );
        assert!(!root.lessons_md().exists());
        assert!(!root.semantic_dir().join("recurrence_counts.json").exists());

        let outcome = recover_if_needed(&root, &ledger(&root)).unwrap();

        assert_eq!(
            outcome,
            RecoveryOutcome::RolledForward {
                renamed: 0,
                unlinked: 0
            }
        );
        assert_eq!(
            fs::read_to_string(root.provenance_ledger()).unwrap(),
            format!(
                "{{\"schema_version\":\"provenance/1.0\",\"kind\":\"lesson_citation\",\"from\":\"{EVT_A}\",\"to\":\"lsn_{EVT_A}\"}}\n\
                 {{\"schema_version\":\"provenance/1.0\",\"kind\":\"recurrence\",\"from\":\"{EVT_A}\",\"to\":\"rust::eh\"}}\n"
            )
        );
        assert!(!root.wal_path().exists());
    }

    /// A reader built on the legacy struct fails closed on the new WAL.
    #[test]
    fn legacy_reader_rejects_a_redo_wal() {
        let (root, _g) = setup_root("legacy-reader");
        let mut stage = begin_cycle(&root, CycleKind::Dream, NOW_SEC).unwrap();
        stage.put(&root.lessons_md(), b"lesson\n").unwrap();
        let json = serde_json::to_string_pretty(&stage.redo()).unwrap();

        assert!(serde_json::from_str::<DreamWal>(&json).is_err(), "{json}");
    }

    #[test]
    fn roll_forward_twice_is_a_no_op_the_second_time() {
        let (root, _g) = setup_root("roll-twice");
        fs::create_dir_all(root.semantic_dir()).unwrap();
        fs::write(root.lessons_md(), b"old lesson\n").unwrap();
        let mut stage = begin_cycle(&root, CycleKind::Dream, NOW_SEC).unwrap();
        stage.put(&root.episodic_jsonl(), b"new log\n").unwrap();
        stage.unlink(&root.lessons_md()).unwrap();
        let redo = stage.redo();
        write_atomic(
            &root.wal_path(),
            serde_json::to_string_pretty(&redo).unwrap().as_bytes(),
        )
        .unwrap();
        let ledger = ledger(&root);

        assert_eq!(roll_forward(&root, &redo, &ledger, None).unwrap(), (1, 1));
        let after_first = (
            fs::read(root.episodic_jsonl()).unwrap(),
            root.lessons_md().exists(),
            fs::read(root.state_json()).unwrap(),
        );
        assert_eq!(roll_forward(&root, &redo, &ledger, None).unwrap(), (0, 0));

        assert_eq!(
            (
                fs::read(root.episodic_jsonl()).unwrap(),
                root.lessons_md().exists(),
                fs::read(root.state_json()).unwrap(),
            ),
            after_first
        );
        assert_eq!(after_first.0, b"new log\n");
        assert!(!after_first.1);
        assert_eq!(status(&root), "complete");
        assert!(!root.wal_path().exists());
    }

    #[test]
    fn recover_clean_when_no_wal() {
        let (root, _g) = setup_root("clean");
        let outcome = recover_if_needed(&root, &ledger(&root)).unwrap();
        assert_eq!(outcome, RecoveryOutcome::Clean);
    }

    #[test]
    fn recover_wipes_a_stage_dir_without_a_wal() {
        let (root, _g) = setup_root("recover-wipe");
        let jsonl = root.episodic_jsonl();
        fs::create_dir_all(root.episodic_dir()).unwrap();
        fs::write(&jsonl, b"live log\n").unwrap();
        let mut stage = begin_cycle(&root, CycleKind::Dream, NOW_SEC).unwrap();
        stage.put(&jsonl, b"never committed\n").unwrap();
        drop(stage);
        // A crash inside the WAL's own atomic write leaves its temp.
        fs::write(root.wal_path().with_extension("tmp"), b"{\"partial").unwrap();

        let outcome = recover_if_needed(&root, &ledger(&root)).unwrap();

        assert_eq!(outcome, RecoveryOutcome::Clean);
        assert!(!root.stage_dir().exists(), "the stage root must be wiped");
        assert!(!root.wal_path().with_extension("tmp").exists());
        assert_eq!(fs::read(&jsonl).unwrap(), b"live log\n");
    }

    /// A hand-written legacy WAL plus temp: an older binary left it, and this
    /// binary recovers it the old way.
    #[test]
    fn recover_keeps_the_legacy_meaning_of_an_intents_wal() {
        let legacy = |root: &AgentRoot, intents: Vec<WalIntent>| {
            let wal = DreamWal {
                schema_version: "1.0".to_string(),
                intents,
            };
            write_wal(root, &serde_json::to_string_pretty(&wal).unwrap());
        };

        // No `Commit`: the temp goes, the destination stays, status `failed`.
        let (root, _g) = setup_root("recover-legacy");
        let dest = root.episodic_jsonl();
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, b"pre-cycle record\n").unwrap();
        let tmp_path = dest.with_extension("tmp");
        fs::write(&tmp_path, b"pruned record\n").unwrap();
        legacy(
            &root,
            vec![WalIntent::PruneEpisodicMemory {
                temp_file_path: tmp_path.to_string_lossy().into_owned(),
            }],
        );
        // A stage dir beside it is wiped too.
        fs::create_dir_all(root.stage_dir().join("dream-1")).unwrap();

        let outcome = recover_if_needed(&root, &ledger(&root)).unwrap();

        assert_eq!(
            outcome,
            RecoveryOutcome::Recovered {
                cleaned_files: vec![tmp_path.clone()]
            }
        );
        assert!(!tmp_path.exists(), ".tmp file must be deleted by recovery");
        assert!(!root.wal_path().exists(), "WAL must be deleted by recovery");
        assert!(!root.stage_dir().exists());
        assert_eq!(
            fs::read(&dest).unwrap(),
            b"pre-cycle record\n",
            "recovery must leave the destination pre-cycle"
        );
        assert_eq!(status(&root), "failed");

        // `Commit`: the cycle finished; only the status and the WAL remain.
        let (root, _g) = setup_root("recover-legacy-commit");
        legacy(&root, vec![WalIntent::Commit]);

        let outcome = recover_if_needed(&root, &ledger(&root)).unwrap();

        assert_eq!(outcome, RecoveryOutcome::CommittedButUnclean);
        assert!(!root.wal_path().exists());
        assert_eq!(status(&root), "complete");
    }

    #[test]
    fn unknown_wal_format_fails_closed() {
        for (label, json, expected) in [
            (
                "wal-redo2",
                r#"{"schema_version":"1.0","format":"redo/2","renames":[],"unlinks":[]}"#,
                "redo/2",
            ),
            ("wal-neither", r#"{"schema_version":"1.0"}"#, "none"),
        ] {
            let (root, _g) = setup_root(label);
            write_wal(&root, json);
            let staged = root.stage_dir().join("dream-1");
            fs::create_dir_all(&staged).unwrap();

            let err = recover_if_needed(&root, &ledger(&root)).unwrap_err();

            assert!(
                matches!(&err, WalError::UnknownFormat(f) if f == expected),
                "{label}: got {err:?}"
            );
            assert_eq!(fs::read_to_string(root.wal_path()).unwrap(), json);
            assert!(staged.is_dir(), "{label}: a refused WAL touches nothing");
            assert!(!root.state_json().exists(), "{label}: no status change");
        }
    }

    #[test]
    fn redo_path_outside_the_agent_dir_fails_closed() {
        let (root, _g) = setup_root("redo-bad-paths");
        let staged_rel = ".dreamd/stage/dream-1/0-LESSONS.md";
        let staged = root.agent_dir().join(staged_rel);
        fs::create_dir_all(staged.parent().unwrap()).unwrap();
        fs::write(&staged, b"staged lesson\n").unwrap();
        fs::create_dir_all(root.semantic_dir()).unwrap();
        fs::write(root.lessons_md(), b"live lesson\n").unwrap();
        let outside = root.project_root().join("escaped.md");

        let cases = [
            (
                staged_rel.to_string(),
                outside.to_string_lossy().into_owned(),
            ),
            (
                staged_rel.to_string(),
                "semantic/../../escaped.md".to_string(),
            ),
            (
                "semantic/LESSONS.md".to_string(),
                "semantic/MOVED.md".to_string(),
            ),
            (
                staged_rel.to_string(),
                ".dreamd/stage/dream-1/1-LESSONS.md".to_string(),
            ),
        ];
        for (from, to) in cases {
            let redo = RedoWal {
                schema_version: STATE_SCHEMA_VERSION.to_string(),
                format: REDO_FORMAT.to_string(),
                renames: vec![RedoRename {
                    from: from.clone(),
                    to: to.clone(),
                }],
                unlinks: vec![],
                edges: vec![],
            };
            write_wal(&root, &serde_json::to_string_pretty(&redo).unwrap());

            let err = recover_if_needed(&root, &ledger(&root)).unwrap_err();

            assert!(
                matches!(err, WalError::BadRedoPath(_)),
                "{from} -> {to}: got {err:?}"
            );
            assert_eq!(fs::read(&staged).unwrap(), b"staged lesson\n");
            assert_eq!(fs::read(root.lessons_md()).unwrap(), b"live lesson\n");
            assert!(!outside.exists());
            assert!(!root.semantic_dir().join("MOVED.md").exists());
            assert!(
                root.wal_path().exists(),
                "a refused WAL stays for the operator"
            );
        }
    }

    #[test]
    fn read_last_cycle_status_returns_idle_when_file_missing() {
        let (root, _g) = setup_root("idle-default");
        // state.json does not exist yet
        assert!(!root.state_json().exists());
        let status = read_last_cycle_status(&root).unwrap();
        assert_eq!(status, "idle");
    }

    #[test]
    fn read_last_cycle_status_returns_idle_when_key_missing() {
        let (root, _g) = setup_root("idle-key-missing");
        // state.json exists but has no last_dream_cycle_status key.
        let state = serde_json::json!({"schema_version": "1.0", "daemon_version": "0.0.0"});
        fs::write(
            root.state_json(),
            serde_json::to_string_pretty(&state).unwrap(),
        )
        .unwrap();
        let status = read_last_cycle_status(&root).unwrap();
        assert_eq!(status, "idle");
    }

    #[test]
    fn read_cycle_started_at_returns_none_when_file_missing() {
        let (root, _g) = setup_root("cycle-started-missing");
        assert!(!root.state_json().exists());
        let started = read_cycle_started_at(&root).unwrap();
        assert_eq!(started, None);
    }

    #[test]
    fn read_cycle_started_at_returns_some_after_commit() {
        let (root, _g) = setup_root("cycle-started-set");
        let stage = begin_cycle(&root, CycleKind::Dream, NOW_SEC).unwrap();
        commit_cycle(stage, &ledger(&root), NOW_SEC).unwrap();
        let started = read_cycle_started_at(&root).unwrap();
        assert!(started.is_some(), "must return Some after commit");
        assert!(
            started.as_ref().unwrap().contains('T'),
            "expected ISO date with time, got {:?}",
            started
        );
    }

    #[test]
    fn begin_cycle_refuses_missing_agent_store() {
        // A bare tmpdir with no `.agent/` — begin_cycle must error rather than
        // scaffold a phantom store. Do not use `setup_root`: it pre-creates
        // `.dreamd/`, and this test needs the un-set-up dir.
        let dir = unique_tmpdir("no-agent-store");
        fs::create_dir_all(&dir).unwrap();
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);

        let err = begin_cycle(&root, CycleKind::Dream, NOW_SEC).unwrap_err();
        assert!(
            matches!(err, WalError::NoAgentStore(_)),
            "expected NoAgentStore, got {err:?}",
        );
        assert!(
            !dir.join(".agent").exists(),
            ".agent/ must not be created when begin_cycle refuses",
        );
    }

    fn skip_fixture() -> AutobiographySkip {
        AutobiographySkip {
            at: 42,
            reason: "user_dirty_tree".to_string(),
            files: vec![".agent/semantic/LESSONS.md".to_string()],
        }
    }

    #[test]
    fn skip_marker_survives_begin_cycle() {
        let (root, _g) = setup_root("skip-survives-begin");
        update_daemon_state(&root, |s| s.last_autobiography_skip = Some(skip_fixture())).unwrap();

        begin_cycle(&root, CycleKind::Dream, NOW_SEC).unwrap();

        let state = read_daemon_state(&root).unwrap();
        let skip = state
            .last_autobiography_skip
            .expect("begin_cycle must not eat last_autobiography_skip");
        assert_eq!(skip.reason, "user_dirty_tree");
        assert_eq!(state.last_dream_cycle_status, "in_progress");
    }

    /// A full cycle is begin **and** commit, and both patch `state.json`
    /// through the typed read-modify-write writer, so neither drops
    /// `last_autobiography_skip` (AILAB-167).
    #[test]
    fn skip_marker_survives_begin_and_commit() {
        let (root, _g) = setup_root("skip-survives-commit");
        update_daemon_state(&root, |s| s.last_autobiography_skip = Some(skip_fixture())).unwrap();

        let stage = begin_cycle(&root, CycleKind::Dream, NOW_SEC).unwrap();
        commit_cycle(stage, &ledger(&root), NOW_SEC).unwrap();

        let state = read_daemon_state(&root).unwrap();
        let skip = state
            .last_autobiography_skip
            .expect("commit_cycle must not eat last_autobiography_skip");
        assert_eq!(skip.reason, "user_dirty_tree");
        assert_eq!(state.last_dream_cycle_status, "complete");
        let at = state
            .last_dream_cycle_at
            .expect("commit_cycle must stamp last_dream_cycle_at");
        assert!(at.contains('T'), "expected ISO date with time, got {at:?}");
    }

    #[test]
    fn last_dream_cycle_at_survives_begin_cycle() {
        let (root, _g) = setup_root("cycle-at-survives-begin");
        let stage = begin_cycle(&root, CycleKind::Dream, NOW_SEC).unwrap();
        commit_cycle(stage, &ledger(&root), NOW_SEC).unwrap();

        let committed_at = read_daemon_state(&root)
            .unwrap()
            .last_dream_cycle_at
            .expect("commit_cycle must stamp last_dream_cycle_at");

        begin_cycle(&root, CycleKind::Dream, NOW_SEC).unwrap();

        let state = read_daemon_state(&root).unwrap();
        assert_eq!(
            state.last_dream_cycle_at,
            Some(committed_at),
            "begin_cycle must preserve the prior cycle's completion timestamp",
        );
        assert_eq!(state.last_dream_cycle_status, "in_progress");
    }

    fn corrupt_sidecars(root: &AgentRoot) -> Vec<PathBuf> {
        fs::read_dir(root.dreamd_dir())
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                let name = path.file_name()?.to_string_lossy();
                name.starts_with("dream_in_progress.wal.corrupt-")
                    .then_some(path)
            })
            .collect()
    }

    /// A `redo/1` body repair can apply is rolled forward. Repair does not
    /// quarantine it.
    #[test]
    fn repair_rolls_a_valid_redo_wal_forward() {
        use crate::writer_lock::{WriterKind, WriterLock};

        let (root, _g) = setup_root("repair-valid-redo");
        let redo = RedoWal {
            schema_version: STATE_SCHEMA_VERSION.to_string(),
            format: REDO_FORMAT.to_string(),
            renames: vec![],
            unlinks: vec![],
            edges: vec![],
        };
        fs::write(root.wal_path(), serde_json::to_string(&redo).unwrap()).unwrap();

        let lock = WriterLock::acquire(&root, WriterKind::Repair).unwrap();

        assert!(lock.quarantined_wal().is_none());
        assert!(!root.wal_path().exists());
        assert_eq!(status(&root), "complete");
        assert!(corrupt_sidecars(&root).is_empty());
    }

    /// A legacy `intents` WAL is still the pre-1.2.0 recovery, including a
    /// `Commit` that finishes as `complete`. Repair does not quarantine it.
    #[test]
    fn repair_recovers_a_legacy_intents_wal() {
        use crate::writer_lock::{WriterKind, WriterLock};

        let (root, _g) = setup_root("repair-legacy-intents");
        let wal = DreamWal {
            schema_version: STATE_SCHEMA_VERSION.to_string(),
            intents: vec![WalIntent::Commit],
        };
        fs::write(root.wal_path(), serde_json::to_string(&wal).unwrap()).unwrap();

        let lock = WriterLock::acquire(&root, WriterKind::Repair).unwrap();

        assert!(lock.quarantined_wal().is_none());
        assert!(!root.wal_path().exists());
        assert_eq!(status(&root), "complete");
        assert!(corrupt_sidecars(&root).is_empty());
    }
}
