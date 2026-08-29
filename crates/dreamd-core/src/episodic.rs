//! `episodic` — the single seam for `AGENT_LEARNINGS.jsonl` I/O (WEG-378).
//!
//! The episodic log is an append-only, single-writer JSONL file (DR-103): the
//! [`MemoryCoordinator`](crate::coordinator::MemoryCoordinator) actor is its
//! only mutator. Every read, durable append, open-time recovery, and full-file
//! rewrite of that log funnels through this module so the torn-tail recovery
//! policy lives in exactly one place — [`scan`].
//!
//! **Recovery policy (SPEC §88): skip mid-file corruption; halt torn tail.**
//! A `\n`-terminated line that is blank or fails schema validation is skipped
//! (logged with line number + reason); scanning continues. A torn final line
//! with no trailing `\n` (crash mid-append) halts ingestion — everything after
//! the last complete line is dropped. [`read_all`] emits ONE `tracing::warn!`
//! for a genuine torn tail and rewrites nothing. Open-time [`recover`] goes
//! further (AILAB-163): it appends the raw skipped lines to a
//! `.corrupt-<YYYY-MM-DD>.jsonl` sidecar beside the log, then rewrites the live
//! file in place to the well-formed record stream — so a mid-file hole IS
//! excised, and the valid records on both sides of it survive. A torn tail with
//! no mid-file skips is still truncated by `set_len` alone, with no sidecar.
//! Never hard-error a torn tail; never silently swallow corruption.
//!
//! Free-function module by design (matches `io.rs` / `wal.rs` / `lessons.rs`):
//! the single mutable owner is already the coordinator actor, so no struct is
//! introduced. Blocking `std::io` is intentional for v0.1 (DR: no `tokio::fs`).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use dreamd_protocol::AgentLearning;

use crate::layout::AgentRoot;

/// Hard cap on one serialized JSONL line (including the trailing `\n`). This is
/// a property of the on-disk line format, so it lives here; the coordinator
/// re-exports it (`pub use crate::episodic::MAX_LEARNING_LINE_BYTES;`). Anything
/// larger is rejected at the write boundary ([`EpisodicError::PayloadTooLarge`])
/// and the HTTP handler maps it to 413. Sidecar storage for oversized payloads
/// is deferred to v0.1.1.
pub const MAX_LEARNING_LINE_BYTES: usize = 4096;

/// Errors surfaced by the episodic I/O seam.
#[derive(Debug, thiserror::Error)]
pub enum EpisodicError {
    #[error("episodic I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialize record: {0}")]
    Serialize(#[source] serde_json::Error),
    /// Serialized line exceeds [`MAX_LEARNING_LINE_BYTES`].
    #[error("payload too large: {size} bytes exceeds {max} byte limit")]
    PayloadTooLarge { size: usize, max: usize },
}

/// One skipped `\n`-terminated line (blank or schema-invalid).
struct ScanSkip {
    line_number: usize,
    reason: String,
    /// The exact `\n`-terminated bytes that were skipped, so [`recover`] can
    /// quarantine them without a second parse (AILAB-163). [`assess_bytes`]
    /// and [`read_all`] ignore this field.
    raw: Vec<u8>,
}

/// Cap on [`EpisodicLogHealth::malformed_line_numbers`]: only the first N
/// skipped line numbers are retained (`malformed_line_count` stays uncapped).
pub const MALFORMED_LINE_NUMBERS_CAP: usize = 5;

/// Episodic log health report for `dreamd doctor` and diagnostics (WEG-132).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EpisodicLogHealth {
    /// `\n`-terminated lines that were blank or failed schema validation.
    pub malformed_line_count: usize,
    /// 1-indexed line numbers of the first [`MALFORMED_LINE_NUMBERS_CAP`]
    /// malformed lines.
    pub malformed_line_numbers: Vec<usize>,
    /// Bytes after the last complete `\n`-terminated line (torn tail at EOF).
    pub torn_tail_bytes: u64,
    /// Well-formed records ingested from complete lines.
    pub valid_record_count: usize,
}

/// Assess episodic log bytes without logging. Used by [`assess_log_health`] and
/// the WEG-132 proptest suite.
pub fn assess_bytes(bytes: &[u8]) -> EpisodicLogHealth {
    let (records, clean_len, skips) = scan(bytes);
    EpisodicLogHealth {
        malformed_line_count: skips.len(),
        malformed_line_numbers: skips
            .iter()
            .take(MALFORMED_LINE_NUMBERS_CAP)
            .map(|s| s.line_number)
            .collect(),
        torn_tail_bytes: bytes.len() as u64 - clean_len,
        valid_record_count: records.len(),
    }
}

/// Read and assess the episodic log at `path` without emitting `tracing` warnings.
/// An absent file is healthy with zero counts.
pub fn assess_log_health(path: &Path) -> Result<EpisodicLogHealth, EpisodicError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(EpisodicLogHealth::default())
        }
        Err(e) => return Err(EpisodicError::Io(e)),
    };
    Ok(assess_bytes(&bytes))
}

/// The single episodic scan policy (SPEC §88). Returns parsed records, the byte
/// end of the last `\n`-terminated line (`clean_len`), and any skipped lines.
/// A final line without a trailing `\n` is a torn tail: `clean_len` stops before
/// it so [`recover`] can truncate; [`read_all`] never ingests it. Mid-file blank
/// or unparseable `\n`-terminated lines advance `clean_len` but are not returned.
fn scan(bytes: &[u8]) -> (Vec<AgentLearning>, u64, Vec<ScanSkip>) {
    let mut out = Vec::new();
    let mut skips = Vec::new();
    let (mut cursor, mut clean_len, mut line_number) = (0usize, 0u64, 1usize);
    while cursor < bytes.len() {
        let Some(rel_nl) = bytes[cursor..].iter().position(|b| *b == b'\n') else {
            break; // torn tail at EOF — no trailing newline
        };
        let end = cursor + rel_nl;
        let line_end = end + 1;
        let slice = &bytes[cursor..end];
        if slice.is_empty() {
            skips.push(ScanSkip {
                line_number,
                reason: "blank line".to_string(),
                raw: bytes[cursor..line_end].to_vec(),
            });
            clean_len = line_end as u64;
            cursor = line_end;
            line_number += 1;
            continue;
        }
        match serde_json::from_slice::<AgentLearning>(slice) {
            Ok(ev) => {
                out.push(ev);
                clean_len = line_end as u64;
                cursor = line_end;
                line_number += 1;
            }
            Err(e) => {
                skips.push(ScanSkip {
                    line_number,
                    reason: e.to_string(),
                    raw: bytes[cursor..line_end].to_vec(),
                });
                clean_len = line_end as u64;
                cursor = line_end;
                line_number += 1;
            }
        }
    }
    (out, clean_len, skips)
}

/// Read every well-formed record from the episodic log at `path`. An absent
/// file is `Ok(empty)`. Mid-file blank or invalid `\n`-terminated lines are
/// skipped (each logged at `warn!`). A torn final line (no trailing `\n`)
/// halts ingestion; ONE `tracing::warn!` records the dropped bytes. Never
/// errors on a torn tail; only a genuine I/O failure opening/reading the file
/// is an error.
pub fn read_all(path: &Path) -> Result<Vec<AgentLearning>, EpisodicError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(EpisodicError::Io(e)),
    };
    let (records, clean_len, skips) = scan(&bytes);
    for skip in &skips {
        tracing::warn!(
            path = %path.display(),
            line = skip.line_number,
            reason = %skip.reason,
            "episodic log: skipping invalid line"
        );
    }
    if clean_len < bytes.len() as u64 {
        tracing::warn!(
            path = %path.display(),
            dropped_bytes = bytes.len() as u64 - clean_len,
            records = records.len(),
            "episodic log has a torn tail; returning clean prefix"
        );
    }
    Ok(records)
}

/// Sidecar path for quarantined lines: sibling of the episodic log, named
/// `.corrupt-<YYYY-MM-DD>.jsonl` from the UTC date of the recovery. One file
/// per day, so repeated opens on the same day append to the same sidecar.
fn quarantine_path(jsonl_path: &Path, date: chrono::NaiveDate) -> PathBuf {
    let name = format!(".corrupt-{}.jsonl", date.format("%Y-%m-%d"));
    match jsonl_path.parent() {
        Some(dir) => dir.join(name),
        None => PathBuf::from(name),
    }
}

/// Append the raw skipped lines to `sidecar` (create-or-append, so a same-day
/// second recovery concatenates), fsync, and clamp the mode to `0600` — the
/// quarantined bytes are episodic content, so they get the same posture as the
/// UDS socket and the secrets file.
fn append_quarantine(sidecar: &Path, skips: &[ScanSkip]) -> std::io::Result<()> {
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(sidecar)?;
    for skip in skips {
        out.write_all(&skip.raw)?;
    }
    out.sync_data()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(sidecar, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Coordinator open-time recovery (AILAB-163). Reads the whole log, then:
///
/// * **Mid-file skips** — blank or schema-invalid `\n`-terminated lines — are
///   appended verbatim to [`quarantine_path`]'s sidecar and excised from the
///   live log, which is rewritten in place to the well-formed record stream.
///   Every valid record on *either* side of the hole survives; the suffix from
///   the first anomaly onward is never quarantined. Logged once at `warn!`,
///   because bytes moved out of the live log and the operator should know.
/// * **A torn final line** with no trailing `\n` and no mid-file skips is
///   truncated by `set_len` alone at `debug!` — routine on restart, no sidecar.
///   When both are present, the rewrite already drops the torn fragment,
///   because it was never a complete line.
///
/// The rewrite is `seek(0)` → `write_all` → `set_len` → `sync_data` on the
/// caller's own fd. It must never `rewrite_atomic` / `rewrite_guarded` / rename
/// and swap inodes: the coordinator holds this fd across the call, so a fresh
/// inode would leave every subsequent append writing into an unlinked file.
/// Leaves `file`'s cursor unspecified; the caller re-seeks to end before
/// appending.
pub fn recover(file: &mut File, jsonl_path: &Path) -> std::io::Result<()> {
    recover_at(file, jsonl_path, chrono::Utc::now().date_naive())
}

/// [`recover`] with the sidecar's date injected, so tests get a deterministic
/// filename instead of racing the wall clock across a UTC midnight.
fn recover_at(file: &mut File, jsonl_path: &Path, date: chrono::NaiveDate) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let (records, clean_len, skips) = scan(&bytes);

    if !skips.is_empty() {
        let sidecar = quarantine_path(jsonl_path, date);
        append_quarantine(&sidecar, &skips)?;

        // Re-serialize the survivors in place — byte-identical to what
        // `rewrite_atomic` would produce, but never that call: this fd is the
        // coordinator's, so an inode swap here strands its appends (AILAB-163).
        let mut clean = String::with_capacity(records.len() * 256);
        for record in &records {
            // These values just round-tripped out of this file, so `to_string`
            // cannot fail; surface it as I/O rather than panicking on open.
            clean.push_str(&serde_json::to_string(record).map_err(std::io::Error::other)?);
            clean.push('\n');
        }

        file.seek(SeekFrom::Start(0))?;
        file.write_all(clean.as_bytes())?;
        file.set_len(clean.len() as u64)?;
        file.sync_data()?;

        tracing::warn!(
            path = %jsonl_path.display(),
            sidecar = %sidecar.display(),
            skipped = skips.len(),
            records = records.len(),
            torn_tail_bytes = bytes.len() as u64 - clean_len,
            "episodic log: quarantined malformed lines and rewrote the clean record stream in place"
        );
        return Ok(());
    }

    if clean_len < bytes.len() as u64 {
        tracing::debug!(
            dropped_bytes = bytes.len() as u64 - clean_len,
            "truncating torn tail on episodic open"
        );
        file.set_len(clean_len)?;
        file.sync_data()?;
    }
    Ok(())
}

/// Coordinator-only durable append onto an open, end-positioned fd. Serializes
/// `learning`, ensures a trailing `\n`, enforces the [`MAX_LEARNING_LINE_BYTES`]
/// cap ([`EpisodicError::PayloadTooLarge`], leaving the file untouched),
/// `write_all`s, then `sync_data`s. Returns the number of bytes written.
pub fn append(file: &mut File, learning: &AgentLearning) -> Result<usize, EpisodicError> {
    let mut line = serde_json::to_string(learning).map_err(EpisodicError::Serialize)?;
    if !line.ends_with('\n') {
        line.push('\n');
    }
    let size = line.len();
    if size > MAX_LEARNING_LINE_BYTES {
        return Err(EpisodicError::PayloadTooLarge {
            size,
            max: MAX_LEARNING_LINE_BYTES,
        });
    }
    file.write_all(line.as_bytes())?;
    file.sync_data()?;
    Ok(size)
}

/// Serialize `records` to the on-disk JSONL line stream shared by
/// [`rewrite_atomic`] and [`rewrite_guarded`]. One serializer so the guarded and
/// unguarded rewrites cannot drift into writing different bytes for the same
/// records.
fn serialize_records(records: &[AgentLearning]) -> Result<String, EpisodicError> {
    let mut out = String::with_capacity(records.len() * 256);
    for record in records {
        out.push_str(&serde_json::to_string(record).map_err(EpisodicError::Serialize)?);
        out.push('\n');
    }
    Ok(out)
}

/// Atomically rewrite the whole episodic log at `path` with `records`, with no
/// WAL involvement.
///
/// For one-shot rewrites outside a dream cycle — `dreamd archive --force-unpin` —
/// where
/// there is no open WAL for an intent to land in. A cycle's own prune goes
/// through [`rewrite_guarded`] instead; do not reintroduce a caller-supplied
/// hook here to bolt one on (AILAB-164).
///
/// Returns [`std::io::ErrorKind::Unsupported`] on Windows (v0.1 defers Windows
/// durable writes; see `docs/windows.md`), matching `io::write_atomic`.
pub fn rewrite_atomic(path: &Path, records: &[AgentLearning]) -> Result<(), EpisodicError> {
    crate::io::write_atomic(path, serialize_records(records)?.as_bytes())?;
    Ok(())
}

/// [`rewrite_atomic`] under the dream-cycle WAL: the
/// [`PruneEpisodicMemory`](crate::wal::WalIntent::PruneEpisodicMemory) intent is
/// appended between the temp's fsync and the rename, naming the temp
/// [`wal::guarded_replace`](crate::wal::guarded_replace) just wrote.
///
/// The pin pass and the decay pruner call this. Neither names a temp path, and
/// neither passes a closure: the intent and the write are one call (AILAB-164).
/// No-ops the intent — but still writes — when no cycle is open.
pub fn rewrite_guarded(
    agent_root: &AgentRoot,
    path: &Path,
    records: &[AgentLearning],
) -> Result<(), EpisodicError> {
    let out = serialize_records(records)?;
    crate::wal::guarded_replace(
        agent_root,
        path,
        out.as_bytes(),
        crate::wal::GuardedReplaceKind::PruneEpisodicMemory,
    )
    .map_err(|e| match e {
        // Unwrap rather than `Error::other(e)`: the inner error IS the rewrite's
        // own I/O failure, and re-boxing it would flatten every `ErrorKind` to
        // `Other` — including the `Unsupported` the Windows arm returns, which
        // AILAB-164 keeps as it is today.
        crate::wal::WalError::Io(io) => EpisodicError::Io(io),
        other => EpisodicError::Io(std::io::Error::other(other)),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    use chrono::DateTime;
    use dreamd_protocol::EventId;

    use crate::test_support::{unique_tmpdir, DirGuard};

    // Fixed "now"/timestamp so records are young (never decay). 2026-07-03Z.
    const NOW_SEC: i64 = 1751500800;

    fn make_learning(suffix: char, content: &str) -> AgentLearning {
        let raw = format!("evt_01ARZ3NDEKTSV4RRFFQ69G5FA{suffix}");
        AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: EventId::parse(&raw).expect("valid EventId"),
            timestamp: DateTime::from_timestamp(NOW_SEC, 0).expect("valid ts"),
            pain: 5.0,
            importance: 6.0,
            pinned: false,
            skill_action: "rust::episodic".to_string(),
            source_harness: "test-harness".to_string(),
            content: content.to_string(),
        }
    }

    fn line_of(l: &AgentLearning) -> Vec<u8> {
        let mut s = serde_json::to_string(l).unwrap();
        s.push('\n');
        s.into_bytes()
    }

    /// Fixed recovery date so the sidecar filename is deterministic.
    fn test_date() -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(2026, 8, 27).expect("valid date")
    }

    const SIDECAR_NAME: &str = ".corrupt-2026-08-27.jsonl";

    fn open_rw(path: &std::path::Path) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap()
    }

    #[cfg(unix)]
    fn assert_mode_0600(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "sidecar must be 0600, got {mode:o}");
    }

    // ── Test 1: absent → empty; clean → all records ────────────────────────
    #[test]
    fn read_all_absent_then_clean() {
        let dir = unique_tmpdir("absent-clean");
        let _g = DirGuard(dir.clone());
        let path = dir.join("AGENT_LEARNINGS.jsonl");

        // Absent file.
        assert!(read_all(&path).unwrap().is_empty(), "absent file → empty");

        // Clean file with two records.
        let a = make_learning('A', "first");
        let b = make_learning('B', "second");
        let mut bytes = line_of(&a);
        bytes.extend_from_slice(&line_of(&b));
        std::fs::write(&path, &bytes).unwrap();

        let got = read_all(&path).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].content, "first");
        assert_eq!(got[1].content, "second");
    }

    // ── Test 2: good + partial-no-newline tail → good only, no error ───────
    #[test]
    fn read_all_tolerates_torn_no_newline_tail() {
        let dir = unique_tmpdir("torn-tail");
        let _g = DirGuard(dir.clone());
        let path = dir.join("AGENT_LEARNINGS.jsonl");

        let good = make_learning('A', "kept");
        let mut bytes = line_of(&good);
        // A torn final line with no trailing newline (post-SIGKILL shape).
        bytes.extend_from_slice(b"{\"schema_version\":\"1.0.0\",\"id\":\"evt_TRUNCAT");
        std::fs::write(&path, &bytes).unwrap();

        let got = read_all(&path).expect("torn tail must not error");
        assert_eq!(got.len(), 1, "only the complete record survives");
        assert_eq!(got[0].content, "kept");
    }

    // ── Test 3: good + blank line + more → skip blank, both records survive ─
    #[test]
    fn read_all_skips_blank_midfile_line() {
        let dir = unique_tmpdir("blank-skip");
        let _g = DirGuard(dir.clone());
        let path = dir.join("AGENT_LEARNINGS.jsonl");

        let good = make_learning('A', "before-gap");
        let after = make_learning('B', "after-gap");
        let mut bytes = line_of(&good);
        bytes.push(b'\n'); // mid-file blank (\n\n) — hand-edit only
        bytes.extend_from_slice(&line_of(&after));
        std::fs::write(&path, &bytes).unwrap();

        let got = read_all(&path).unwrap();
        assert_eq!(got.len(), 2, "mid-file blank is skipped");
        assert_eq!(got[0].content, "before-gap");
        assert_eq!(got[1].content, "after-gap");
    }

    // ── Test 4a: good + unparseable torn tail (no \n) → halts ──────────────
    #[test]
    fn read_all_halts_at_unparseable_torn_tail() {
        let dir = unique_tmpdir("bad-torn-tail");
        let _g = DirGuard(dir.clone());
        let path = dir.join("AGENT_LEARNINGS.jsonl");

        let good = make_learning('A', "before-bad");
        let mut bytes = line_of(&good);
        bytes.extend_from_slice(b"{not valid json"); // no trailing \n
        std::fs::write(&path, &bytes).unwrap();

        let got = read_all(&path).unwrap();
        assert_eq!(got.len(), 1, "torn unparseable tail halts the scan");
        assert_eq!(got[0].content, "before-bad");
    }

    // ── Test 4b: good + {bad}\n + later-good → skip bad, later survives ────
    #[test]
    fn read_all_skips_unparseable_midfile_line() {
        let dir = unique_tmpdir("bad-midfile-skip");
        let _g = DirGuard(dir.clone());
        let path = dir.join("AGENT_LEARNINGS.jsonl");

        let good = make_learning('A', "before-bad");
        let later = make_learning('B', "after-bad");
        let mut bytes = line_of(&good);
        bytes.extend_from_slice(b"{not valid json}\n");
        bytes.extend_from_slice(&line_of(&later));
        std::fs::write(&path, &bytes).unwrap();

        let got = read_all(&path).unwrap();
        assert_eq!(got.len(), 2, "newline-terminated bad line is skipped");
        assert_eq!(got[0].content, "before-bad");
        assert_eq!(got[1].content, "after-bad");
    }

    // ── Test 5a: mid-file blank → quarantined; both goods stay live ────────
    #[test]
    fn recover_quarantines_midfile_blank_line() {
        let dir = unique_tmpdir("recover-midfile-blank");
        let _g = DirGuard(dir.clone());
        let path = dir.join("AGENT_LEARNINGS.jsonl");

        let good = make_learning('A', "before-gap");
        let after = make_learning('B', "after-gap");
        let mut bytes = line_of(&good);
        bytes.push(b'\n'); // mid-file blank (\n\n) — hand-edit only
        bytes.extend_from_slice(&line_of(&after));
        std::fs::write(&path, &bytes).unwrap();

        let mut file = open_rw(&path);
        recover_at(&mut file, &path, test_date()).unwrap();

        // Live file is exactly A then B — the blank is gone, neither good is.
        let mut expected = line_of(&good);
        expected.extend_from_slice(&line_of(&after));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            expected,
            "live log must be the clean record stream, blank excised"
        );

        let got = read_all(&path).unwrap();
        assert_eq!(got.len(), 2, "both valid records survive recovery");
        assert_eq!(got[0].content, "before-gap");
        assert_eq!(got[1].content, "after-gap");

        // The excised bytes are recoverable from the sidecar.
        let sidecar = dir.join(SIDECAR_NAME);
        assert_eq!(
            std::fs::read(&sidecar).unwrap(),
            b"\n",
            "sidecar holds the raw blank line"
        );
        #[cfg(unix)]
        assert_mode_0600(&sidecar);
    }

    // ── Test 5b: mid-file unparseable → quarantined; both goods stay live ──
    #[test]
    fn recover_quarantines_midfile_unparseable_line() {
        let dir = unique_tmpdir("recover-midfile-bad");
        let _g = DirGuard(dir.clone());
        let path = dir.join("AGENT_LEARNINGS.jsonl");

        let good = make_learning('A', "before-bad");
        let after = make_learning('B', "after-bad");
        let mut bytes = line_of(&good);
        bytes.extend_from_slice(b"{not valid json}\n");
        bytes.extend_from_slice(&line_of(&after));
        std::fs::write(&path, &bytes).unwrap();

        let mut file = open_rw(&path);
        recover_at(&mut file, &path, test_date()).unwrap();

        let mut expected = line_of(&good);
        expected.extend_from_slice(&line_of(&after));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            expected,
            "live log keeps the record after the hole, not just the prefix"
        );

        let got = read_all(&path).unwrap();
        assert_eq!(got.len(), 2, "both valid records survive recovery");
        assert_eq!(got[0].content, "before-bad");
        assert_eq!(got[1].content, "after-bad");

        let sidecar = dir.join(SIDECAR_NAME);
        assert_eq!(
            std::fs::read(&sidecar).unwrap(),
            b"{not valid json}\n",
            "sidecar holds the raw malformed line"
        );
        #[cfg(unix)]
        assert_mode_0600(&sidecar);
    }

    // ── Test 5c: torn tail only → truncate as before, no sidecar ───────────
    #[test]
    fn recover_torn_tail_only_writes_no_sidecar() {
        let dir = unique_tmpdir("recover-torn-only");
        let _g = DirGuard(dir.clone());
        let path = dir.join("AGENT_LEARNINGS.jsonl");

        let good = make_learning('A', "kept");
        let clean_prefix = line_of(&good);
        let mut bytes = clean_prefix.clone();
        bytes.extend_from_slice(b"{\"schema_version\":\"1.0.0\",\"id\":\"evt_TORN"); // no \n
        std::fs::write(&path, &bytes).unwrap();

        let mut file = open_rw(&path);
        recover_at(&mut file, &path, test_date()).unwrap();

        assert_eq!(
            std::fs::read(&path).unwrap(),
            clean_prefix,
            "torn tail truncated to the clean prefix"
        );
        assert!(
            !dir.join(SIDECAR_NAME).exists(),
            "a torn tail alone must not create a quarantine sidecar"
        );
    }

    // ── Test 5d: mid-file bad + torn tail → both dropped, goods kept ───────
    #[test]
    fn recover_quarantines_midfile_bad_and_drops_torn_tail() {
        let dir = unique_tmpdir("recover-midfile-mixed");
        let _g = DirGuard(dir.clone());
        let path = dir.join("AGENT_LEARNINGS.jsonl");

        let good = make_learning('A', "before-bad");
        let after = make_learning('B', "after-bad");
        let mut bytes = line_of(&good);
        bytes.extend_from_slice(b"{not valid json}\n");
        bytes.extend_from_slice(&line_of(&after));
        bytes.extend_from_slice(b"{\"schema_version\":\"1.0.0\",\"id\":\"evt_TORN"); // no \n
        std::fs::write(&path, &bytes).unwrap();

        let mut file = open_rw(&path);
        recover_at(&mut file, &path, test_date()).unwrap();

        let mut expected = line_of(&good);
        expected.extend_from_slice(&line_of(&after));
        let live = std::fs::read(&path).unwrap();
        assert_eq!(live, expected, "live log is exactly the two good records");
        assert!(
            !live.windows(8).any(|w| w == b"evt_TORN"),
            "no torn-tail bytes may remain in the live log"
        );

        // The rewrite drops the torn fragment; only the complete bad line is
        // recoverable, so an append still lands on a clean boundary.
        assert_eq!(
            std::fs::read(dir.join(SIDECAR_NAME)).unwrap(),
            b"{not valid json}\n",
            "sidecar holds the bad line only — the torn fragment was never a line"
        );

        file.seek(SeekFrom::End(0)).unwrap();
        append(&mut file, &make_learning('C', "appended")).unwrap();
        let got = read_all(&path).unwrap();
        assert_eq!(got.len(), 3, "two survivors + one clean append");
        assert_eq!(got[2].content, "appended");
    }

    // ── Test 5e: two recoveries, same UTC date → one appended sidecar ──────
    #[test]
    fn recover_appends_to_same_day_sidecar() {
        let dir = unique_tmpdir("recover-sidecar-append");
        let _g = DirGuard(dir.clone());
        let path = dir.join("AGENT_LEARNINGS.jsonl");
        let sidecar = dir.join(SIDECAR_NAME);

        let good = make_learning('A', "kept");

        // First recovery quarantines a malformed line.
        let mut bytes = line_of(&good);
        bytes.extend_from_slice(b"{first bad}\n");
        std::fs::write(&path, &bytes).unwrap();
        let mut file = open_rw(&path);
        recover_at(&mut file, &path, test_date()).unwrap();
        drop(file);
        assert_eq!(std::fs::read(&sidecar).unwrap(), b"{first bad}\n");

        // Second recovery on the same date appends rather than clobbering.
        let mut bytes = line_of(&good);
        bytes.extend_from_slice(b"{second bad}\n");
        std::fs::write(&path, &bytes).unwrap();
        let mut file = open_rw(&path);
        recover_at(&mut file, &path, test_date()).unwrap();

        assert_eq!(
            std::fs::read(&sidecar).unwrap(),
            b"{first bad}\n{second bad}\n",
            "same-day sidecar concatenates both quarantines"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            line_of(&good),
            "live log is the single good record after each recovery"
        );
        #[cfg(unix)]
        assert_mode_0600(&sidecar);
    }

    // ── Test 6: recover truncates torn tail; append lands on clean boundary ─
    #[test]
    fn recover_truncates_torn_tail_to_clean_prefix() {
        let dir = unique_tmpdir("recover");
        let _g = DirGuard(dir.clone());
        let path = dir.join("AGENT_LEARNINGS.jsonl");

        let good = make_learning('A', "survivor");
        let clean_prefix = line_of(&good);
        let mut bytes = clean_prefix.clone();
        bytes.extend_from_slice(b"{\"schema_version\":\"1.0.0\",\"id\":\"evt_TORN"); // no \n
        std::fs::write(&path, &bytes).unwrap();

        let mut file = open_rw(&path);
        recover(&mut file, &path).unwrap();

        // On-disk length is exactly the clean prefix.
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            clean_prefix.len() as u64,
            "torn tail truncated to clean prefix"
        );

        // A subsequent append lands on a clean boundary.
        file.seek(SeekFrom::End(0)).unwrap();
        let appended = make_learning('B', "appended");
        append(&mut file, &appended).unwrap();

        let got = read_all(&path).unwrap();
        assert_eq!(got.len(), 2, "recovered survivor + one clean append");
        assert_eq!(got[0].content, "survivor");
        assert_eq!(got[1].content, "appended");
    }

    // ── Test 7: append rejects >4 KiB serialized line, file untouched ──────
    #[test]
    fn append_rejects_oversized_payload_untouched() {
        let dir = unique_tmpdir("toobig");
        let _g = DirGuard(dir.clone());
        let path = dir.join("AGENT_LEARNINGS.jsonl");

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();

        let mut huge = make_learning('A', "");
        huge.content = "x".repeat(5 * 1024); // guarantees > 4096 serialized

        let err = append(&mut file, &huge).expect_err("oversized must be rejected");
        match err {
            EpisodicError::PayloadTooLarge { size, max } => {
                assert!(size > max);
                assert_eq!(max, MAX_LEARNING_LINE_BYTES);
            }
            other => panic!("expected PayloadTooLarge, got {other:?}"),
        }

        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            0,
            "rejected payload must leave the file untouched"
        );
    }

    // ── Test 8: CLI-path regression — torn tail no longer aborts the cycle ─
    //
    // The whole point of WEG-378. On the `dreamd dream` CLI path there is no
    // coordinator to pre-truncate a torn tail, so a normal post-SIGKILL torn
    // final line used to abort `run_decay_pruner` with `DecayError::Json`.
    // Now the shared `episodic::read_all` scan tolerates it.
    #[test]
    fn cli_path_torn_tail_decay_cycle_succeeds() {
        let dir = unique_tmpdir("cli-torn");
        let _g = DirGuard(dir.clone());
        let root = crate::layout::AgentRoot::new(&dir);
        let jsonl = root.episodic_jsonl();
        std::fs::create_dir_all(jsonl.parent().unwrap()).unwrap();

        // Two young (never-decay) records + a torn final line.
        let a = make_learning('A', "one");
        let b = make_learning('B', "two");
        let mut bytes = line_of(&a);
        bytes.extend_from_slice(&line_of(&b));
        bytes.extend_from_slice(b"{\"schema_version\":\"1.0.0\",\"id\":\"evt_TOR"); // torn
        std::fs::write(&jsonl, &bytes).unwrap();

        let result = crate::decay::run_decay_pruner(&root, NOW_SEC, "2026-07-03");
        let outcome = result.expect("torn tail must not abort the CLI dream path");
        assert_eq!(outcome.kept_count, 2, "both complete records kept");
        assert!(outcome.decayed_ids.is_empty(), "young records do not decay");
    }

    // ── Test 9: health reports 1-indexed line numbers of malformed lines ───
    #[test]
    fn assess_bytes_reports_malformed_line_numbers() {
        let mut bytes = line_of(&make_learning('A', "one")); // line 1: valid
        bytes.extend_from_slice(b"{not valid json}\n"); // line 2: malformed
        bytes.extend_from_slice(&line_of(&make_learning('B', "two"))); // line 3
        bytes.push(b'\n'); // line 4: blank
        bytes.extend_from_slice(&line_of(&make_learning('C', "three"))); // line 5

        let health = assess_bytes(&bytes);
        assert_eq!(health.valid_record_count, 3);
        assert_eq!(health.malformed_line_count, 2);
        assert_eq!(health.malformed_line_numbers, vec![2, 4]);
    }

    // ── Test 10: >CAP malformed lines → numbers capped, count uncapped ─────
    #[test]
    fn assess_bytes_caps_malformed_line_numbers() {
        let mut bytes = line_of(&make_learning('A', "one")); // line 1: valid
        for _ in 0..MALFORMED_LINE_NUMBERS_CAP + 2 {
            bytes.extend_from_slice(b"{not valid json}\n"); // lines 2..=8
        }

        let health = assess_bytes(&bytes);
        assert_eq!(health.valid_record_count, 1);
        assert_eq!(
            health.malformed_line_count,
            MALFORMED_LINE_NUMBERS_CAP + 2,
            "count stays uncapped"
        );
        assert_eq!(health.malformed_line_numbers, vec![2, 3, 4, 5, 6]);
    }

    // ── Test 11: clean log → no malformed line numbers ─────────────────────
    #[test]
    fn assess_bytes_clean_log_has_no_malformed_line_numbers() {
        let mut bytes = line_of(&make_learning('A', "one"));
        bytes.extend_from_slice(&line_of(&make_learning('B', "two")));

        let health = assess_bytes(&bytes);
        assert_eq!(health.valid_record_count, 2);
        assert_eq!(health.malformed_line_count, 0);
        assert!(health.malformed_line_numbers.is_empty());
    }
}
