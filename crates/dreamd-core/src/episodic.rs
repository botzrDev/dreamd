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
//! for a genuine torn tail and rewrites nothing. Open-time [`recover`] leaves
//! every complete line where it is, skipped ones included. It ends a torn final
//! line with `\n` when that line parses as a record, and truncates it
//! otherwise. Never hard-error a torn tail; never silently swallow corruption.
//!
//! **Rewrites keep bytes.** [`read_raw`] returns every line with its exact
//! bytes and, for a record, the parsed [`AgentLearning`]. [`patch_lines`]
//! builds the new log from those lines. Lines it does not change, skipped lines
//! and a torn final line are copied verbatim, so fields this binary does not
//! know survive a rewrite (SPEC forward compatibility).
//!
//! Free-function module by design (matches `io.rs` / `wal.rs` / `lessons.rs`):
//! the single mutable owner is already the coordinator actor, so no struct is
//! introduced. Blocking `std::io` is intentional for v0.1 (DR: no `tokio::fs`).

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::Path;

use dreamd_protocol::{AgentLearning, EventId};
use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::value::RawValue;

/// Hard cap on one serialized JSONL line (including the trailing `\n`). This is
/// a property of the on-disk line format, so it lives here; the coordinator
/// re-exports it (`pub use crate::episodic::MAX_LEARNING_LINE_BYTES;`). Anything
/// larger is rejected at the write boundary ([`EpisodicError::PayloadTooLarge`])
/// and the HTTP handler maps it to 413. Oversized payloads are rejected;
/// there is no sidecar for them.
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
/// it so [`recover`] can end or truncate it; [`read_all`] never ingests it.
/// Mid-file blank or unparseable `\n`-terminated lines advance `clean_len` but
/// are not returned.
fn scan(bytes: &[u8]) -> (Vec<AgentLearning>, u64, Vec<ScanSkip>) {
    let mut out = Vec::new();
    let mut skips = Vec::new();
    let mut clean_len = 0u64;
    for (line_number, (span, kind)) in (1usize..).zip(lines(bytes)) {
        match kind {
            LineKind::Record(ev) => {
                out.push(ev);
                clean_len = span.end as u64;
            }
            LineKind::Skipped(reason) => {
                skips.push(ScanSkip {
                    line_number,
                    reason,
                });
                clean_len = span.end as u64;
            }
            LineKind::Torn => {}
        }
    }
    (out, clean_len, skips)
}

/// What one line of the log is, under the SPEC §88 policy.
enum LineKind {
    Record(AgentLearning),
    /// A `\n`-terminated line that is blank or not a record; the reason.
    Skipped(String),
    /// Final bytes with no `\n`. Never a record, even when they parse.
    Torn,
}

/// The one line loop. Yields each line's byte range (including its `\n`) and
/// its kind, in file order.
fn lines(bytes: &[u8]) -> impl Iterator<Item = (Range<usize>, LineKind)> + '_ {
    let mut cursor = 0usize;
    std::iter::from_fn(move || {
        if cursor >= bytes.len() {
            return None;
        }
        let start = cursor;
        let Some(rel_nl) = bytes[start..].iter().position(|b| *b == b'\n') else {
            cursor = bytes.len();
            return Some((start..cursor, LineKind::Torn));
        };
        let end = start + rel_nl;
        cursor = end + 1;
        let kind = match parse_record(&bytes[start..end]) {
            Ok(ev) => LineKind::Record(ev),
            Err(reason) => LineKind::Skipped(reason),
        };
        Some((start..cursor, kind))
    })
}

/// Whether `slice`, one line without its `\n`, is a record. The one parse rule
/// [`lines`] and [`recover`] share.
fn parse_record(slice: &[u8]) -> Result<AgentLearning, String> {
    if slice.is_empty() {
        return Err("blank line".to_string());
    }
    serde_json::from_slice::<AgentLearning>(slice).map_err(|e| e.to_string())
}

/// The `warn!` for one skipped line, shared by [`read_all`] and [`read_raw`].
fn warn_skipped_line(path: &Path, line_number: usize, reason: &str) {
    tracing::warn!(
        path = %path.display(),
        line = line_number,
        reason = %reason,
        "episodic log: skipping invalid line"
    );
}

/// The `warn!` for a torn tail, shared by [`read_all`] and [`read_raw`].
fn warn_torn_tail(path: &Path, dropped_bytes: u64, records: usize) {
    tracing::warn!(
        path = %path.display(),
        dropped_bytes,
        records,
        "episodic log has a torn tail; returning clean prefix"
    );
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
        warn_skipped_line(path, skip.line_number, &skip.reason);
    }
    if clean_len < bytes.len() as u64 {
        warn_torn_tail(path, bytes.len() as u64 - clean_len, records.len());
    }
    Ok(records)
}

/// One line of the episodic log, with its exact bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct RawLine {
    /// The line's bytes, including its `\n`. A torn final line has none.
    pub bytes: Vec<u8>,
    /// The record, when this is a line [`read_all`] returns.
    pub parsed: Option<AgentLearning>,
}

impl RawLine {
    fn new(bytes: &[u8], span: Range<usize>, kind: LineKind) -> Self {
        RawLine {
            bytes: bytes[span].to_vec(),
            parsed: match kind {
                LineKind::Record(ev) => Some(ev),
                LineKind::Skipped(_) | LineKind::Torn => None,
            },
        }
    }
}

/// Split `bytes` into lines. Concatenating every `bytes` gives the input back.
pub fn raw_lines(bytes: &[u8]) -> Vec<RawLine> {
    lines(bytes)
        .map(|(span, kind)| RawLine::new(bytes, span, kind))
        .collect()
}

/// [`raw_lines`] of the episodic log at `path`. An absent file is `Ok(empty)`.
/// Logs the same warnings [`read_all`] logs for the same file: one per skipped
/// line, and one for a torn tail.
pub fn read_raw(path: &Path) -> Result<Vec<RawLine>, EpisodicError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(EpisodicError::Io(e)),
    };
    let mut out = Vec::new();
    let mut records = 0usize;
    for (line_number, (span, kind)) in (1usize..).zip(lines(&bytes)) {
        match &kind {
            LineKind::Record(_) => records += 1,
            LineKind::Skipped(reason) => warn_skipped_line(path, line_number, reason),
            LineKind::Torn => warn_torn_tail(path, span.len() as u64, records),
        }
        out.push(RawLine::new(&bytes, span, kind));
    }
    Ok(out)
}

/// Build a new log from `raw`.
///
/// * A line with no record is copied verbatim; a torn final line stays last.
/// * A record whose id is in `drops` is left out.
/// * A record whose id is in `pins` with a different `pinned` gets that value.
///   Every other member keeps its position and its bytes.
/// * Every other line is copied verbatim.
pub fn patch_lines(
    raw: &[RawLine],
    pins: &HashMap<EventId, bool>,
    drops: &HashSet<EventId>,
) -> Result<Vec<u8>, EpisodicError> {
    let mut out = Vec::with_capacity(raw.iter().map(|line| line.bytes.len()).sum());
    for line in raw {
        let Some(record) = &line.parsed else {
            out.extend_from_slice(&line.bytes);
            continue;
        };
        if drops.contains(&record.id) {
            continue;
        }
        match pins.get(&record.id) {
            Some(&pinned) if pinned != record.pinned => {
                out.extend_from_slice(&set_pinned(&line.bytes, pinned)?);
            }
            _ => out.extend_from_slice(&line.bytes),
        }
    }
    Ok(out)
}

/// A JSON object's members in file order, each value kept as its raw bytes.
///
/// serde_json's own map type has no `preserve_order` in this graph: it would
/// sort the keys and reformat numbers (`6.50` → `6.5`).
struct OrderedObject(Vec<(String, Box<RawValue>)>);

impl<'de> Deserialize<'de> for OrderedObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MembersVisitor;

        impl<'de> Visitor<'de> for MembersVisitor {
            type Value = OrderedObject;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut members = Vec::new();
                while let Some(member) = map.next_entry::<String, Box<RawValue>>()? {
                    members.push(member);
                }
                Ok(OrderedObject(members))
            }
        }

        deserializer.deserialize_map(MembersVisitor)
    }
}

/// `line` with its `pinned` member set to `pinned`: replaced in place, or
/// appended as the last member when absent. Every other member keeps its
/// position and its value's bytes.
fn set_pinned(line: &[u8], pinned: bool) -> Result<Vec<u8>, EpisodicError> {
    let body = line.strip_suffix(b"\n").unwrap_or(line);
    let OrderedObject(mut members) =
        serde_json::from_slice(body).map_err(EpisodicError::Serialize)?;
    let value = serde_json::value::to_raw_value(&pinned).map_err(EpisodicError::Serialize)?;
    match members.iter_mut().find(|(k, _)| k == "pinned") {
        Some((_, v)) => *v = value,
        None => members.push(("pinned".to_string(), value)),
    }
    let mut out = Vec::with_capacity(line.len() + 16);
    out.push(b'{');
    for (i, (key, value)) in members.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        serde_json::to_writer(&mut out, key).map_err(EpisodicError::Serialize)?;
        out.push(b':');
        out.extend_from_slice(value.get().as_bytes());
    }
    out.extend_from_slice(b"}\n");
    Ok(out)
}

/// Coordinator open-time recovery. Reads the whole log, then:
///
/// * **Mid-file skips** — blank or schema-invalid `\n`-terminated lines — stay
///   where they are; readers skip them (SPEC §88), and one `warn!` gives the
///   count.
/// * **A torn final line** with no trailing `\n` gets one appended when it
///   parses as a record, and that record is returned with the others;
///   otherwise `set_len` truncates it, at `debug!`.
///
/// Nothing else is written: no rewrite and no sidecar. The coordinator runs
/// this on a plain read/write fd and closes it before it opens its append fd.
/// Returns every record in the file after recovery.
pub fn recover(file: &mut File, jsonl_path: &Path) -> std::io::Result<Vec<AgentLearning>> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let (mut records, clean_len, skips) = scan(&bytes);

    if let Some(first) = skips.first() {
        tracing::warn!(
            path = %jsonl_path.display(),
            skipped = skips.len(),
            first_line = first.line_number,
            "episodic log: lines this reader skips stay in place"
        );
    }

    let tail = &bytes[clean_len as usize..];
    if !tail.is_empty() {
        match parse_record(tail) {
            Ok(record) => {
                file.seek(SeekFrom::End(0))?;
                file.write_all(b"\n")?;
                tracing::warn!(
                    path = %jsonl_path.display(),
                    "episodic log: ended a final record that lacked its newline"
                );
                records.push(record);
            }
            Err(_) => {
                tracing::debug!(
                    dropped_bytes = tail.len(),
                    "truncating torn tail on episodic open"
                );
                file.set_len(clean_len)?;
            }
        }
        file.sync_data()?;
    }
    Ok(records)
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

/// Atomically replace the whole episodic log at `path` with `contents`, the new
/// file's bytes, with no WAL involvement.
///
/// For one-shot rewrites outside a dream cycle — `dreamd archive --force-unpin`.
/// A cycle never calls this: a dream cycle's or a forget's plan
/// ([`crate::plan`]) builds the log with [`patch_lines`],
/// [`crate::stage::Stage::apply_plan`] stages it, and the cycle's redo WAL
/// renames it in.
///
/// Returns [`std::io::ErrorKind::Unsupported`] on Windows (v0.1 defers Windows
/// durable writes; see `docs/windows.md`), matching `io::write_atomic`.
pub fn rewrite_atomic(path: &Path, contents: &[u8]) -> Result<(), EpisodicError> {
    crate::io::write_atomic(path, contents)?;
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

    fn open_rw(path: &std::path::Path) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap()
    }

    /// Recovery writes no file beside the log.
    fn assert_dir_holds_only(dir: &std::path::Path, log: &std::path::Path) {
        let entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(entries, vec![log.to_path_buf()], "recovery wrote a sidecar");
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
        recover(&mut file, &path).unwrap();

        assert_eq!(
            std::fs::read(&path).unwrap(),
            clean_prefix,
            "torn tail truncated to the clean prefix"
        );
        assert_dir_holds_only(&dir, &path);
    }

    // ── BZR-1235: recovery leaves complete lines in place ──────────────────
    #[test]
    fn recover_keeps_midfile_bad_lines_in_place() {
        let dir = unique_tmpdir("recover-midfile-in-place");
        let _g = DirGuard(dir.clone());
        let path = dir.join("AGENT_LEARNINGS.jsonl");

        let a = make_learning('A', "before-bad");
        let b = make_learning('B', "after-bad");
        let mut bytes = line_of(&a);
        bytes.push(b'\n'); // mid-file blank (\n\n) — hand-edit only
        bytes.extend_from_slice(b"{not valid json}\n");
        bytes.extend_from_slice(&line_of(&b));
        std::fs::write(&path, &bytes).unwrap();

        let mut file = open_rw(&path);
        let got = recover(&mut file, &path).unwrap();

        assert_eq!(
            std::fs::read(&path).unwrap(),
            bytes,
            "skipped lines stay where they are"
        );
        assert_eq!(got, vec![a, b]);
        assert_dir_holds_only(&dir, &path);
    }

    #[test]
    fn recover_ends_a_valid_final_record_with_a_newline() {
        let dir = unique_tmpdir("recover-end-final");
        let _g = DirGuard(dir.clone());
        let path = dir.join("AGENT_LEARNINGS.jsonl");

        let a = make_learning('A', "terminated");
        let b = make_learning('B', "unterminated");
        let mut bytes = line_of(&a);
        bytes.extend_from_slice(serde_json::to_string(&b).unwrap().as_bytes()); // no \n
        std::fs::write(&path, &bytes).unwrap();

        let mut file = open_rw(&path);
        let got = recover(&mut file, &path).unwrap();

        let mut expected = bytes.clone();
        expected.push(b'\n');
        assert_eq!(std::fs::read(&path).unwrap(), expected);
        assert_eq!(got, vec![a.clone(), b.clone()]);
        assert_eq!(read_all(&path).unwrap(), vec![a, b]);
    }

    #[test]
    fn recover_ends_a_valid_final_record_after_a_bad_line() {
        let dir = unique_tmpdir("recover-end-final-after-bad");
        let _g = DirGuard(dir.clone());
        let path = dir.join("AGENT_LEARNINGS.jsonl");

        let a = make_learning('A', "before-bad");
        let b = make_learning('B', "unterminated");
        let mut bytes = line_of(&a);
        bytes.extend_from_slice(b"{not valid json}\n");
        bytes.extend_from_slice(serde_json::to_string(&b).unwrap().as_bytes()); // no \n
        std::fs::write(&path, &bytes).unwrap();

        let mut file = open_rw(&path);
        let got = recover(&mut file, &path).unwrap();

        let mut expected = bytes.clone();
        expected.push(b'\n');
        assert_eq!(std::fs::read(&path).unwrap(), expected);
        assert_eq!(got, vec![a, b]);
    }

    #[test]
    fn recover_truncates_a_json_fragment_that_is_not_a_record() {
        let dir = unique_tmpdir("recover-json-fragment");
        let _g = DirGuard(dir.clone());
        let path = dir.join("AGENT_LEARNINGS.jsonl");

        let a = make_learning('A', "kept");
        let mut bytes = line_of(&a);
        bytes.extend_from_slice(b"{\"not\":\"a learning\"}"); // valid JSON, no \n
        std::fs::write(&path, &bytes).unwrap();

        let mut file = open_rw(&path);
        let got = recover(&mut file, &path).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), line_of(&a));
        assert_eq!(got, vec![a]);
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
    // final line used to abort the decay pass with a JSON error. Now the
    // shared line scan tolerates it, and `plan_cycle` plans over those lines.
    #[test]
    fn cli_path_torn_tail_decay_cycle_succeeds() {
        // Two young (never-decay) records + a torn final line.
        let a = make_learning('A', "one");
        let b = make_learning('B', "two");
        let mut bytes = line_of(&a);
        bytes.extend_from_slice(&line_of(&b));
        bytes.extend_from_slice(b"{\"schema_version\":\"1.0.0\",\"id\":\"evt_TOR"); // torn

        let result = crate::plan::plan_cycle(
            &raw_lines(&bytes),
            None,
            NOW_SEC,
            crate::consolidation::LessonBodySource::Deterministic,
        );
        let outcome = result
            .expect("torn tail must not abort the CLI dream path")
            .decay_result();
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

    // ── BZR-1234: raw lines and byte-preserving patches ─────────────────────

    const SKIPPED: &[u8] = b"{\"not\":\"a learning\"}\n";

    /// `line` with `member` inserted as its first member.
    fn with_first_member(line: &[u8], member: &str) -> Vec<u8> {
        let mut out = b"{".to_vec();
        out.extend_from_slice(member.as_bytes());
        out.push(b',');
        out.extend_from_slice(&line[1..]);
        out
    }

    /// A record, a blank line, a skipped line, a record with an unknown
    /// member, and a torn final fragment.
    fn mixed_log() -> Vec<u8> {
        let mut bytes = line_of(&make_learning('A', "first"));
        bytes.push(b'\n');
        bytes.extend_from_slice(SKIPPED);
        bytes.extend_from_slice(&with_first_member(
            &line_of(&make_learning('B', "second")),
            r#""x_note":"keep""#,
        ));
        bytes.extend_from_slice(b"{\"partial");
        bytes
    }

    fn pins_of(entries: &[(&AgentLearning, bool)]) -> HashMap<EventId, bool> {
        entries
            .iter()
            .map(|(ev, pinned)| (ev.id.clone(), *pinned))
            .collect()
    }

    #[test]
    fn raw_lines_keep_every_byte_and_parse_only_records() {
        let bytes = mixed_log();

        let raw = raw_lines(&bytes);

        let concat: Vec<u8> = raw.iter().flat_map(|l| l.bytes.clone()).collect();
        assert_eq!(concat, bytes);
        let parsed: Vec<usize> = raw
            .iter()
            .enumerate()
            .filter(|(_, l)| l.parsed.is_some())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(parsed, vec![0, 3], "exactly the two records parse");
        assert_eq!(raw[0].parsed, Some(make_learning('A', "first")));
        assert_eq!(raw[3].parsed, Some(make_learning('B', "second")));
        assert_eq!(raw.last().unwrap().bytes, b"{\"partial");
    }

    #[test]
    fn raw_lines_never_parse_a_torn_final_record() {
        let mut bytes = line_of(&make_learning('A', "torn"));
        bytes.pop();

        let raw = raw_lines(&bytes);

        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0].bytes, bytes);
        assert_eq!(raw[0].parsed, None, "a final line without \\n is torn");
    }

    #[test]
    fn patch_with_no_pins_or_drops_returns_the_input() {
        let bytes = mixed_log();

        let out = patch_lines(&raw_lines(&bytes), &HashMap::new(), &HashSet::new()).unwrap();

        assert_eq!(out, bytes);
    }

    #[test]
    fn pin_flip_of_a_canonical_line_matches_the_struct_serialization() {
        let a = make_learning('A', "flip");
        let mut a_pinned = a.clone();
        a_pinned.pinned = true;

        let out = patch_lines(
            &raw_lines(&line_of(&a)),
            &pins_of(&[(&a, true)]),
            &HashSet::new(),
        )
        .unwrap();

        assert_eq!(out, line_of(&a_pinned));
    }

    #[test]
    fn pin_flip_keeps_other_members_byte_for_byte() {
        let line = "{\"schema_version\":\"1.0.0\",\"id\":\"evt_01ARZ3NDEKTSV4RRFFQ69G5FAA\",\
                    \"timestamp\":\"2026-07-03T00:00:00Z\",\"x_note\":\"keep\",\"pain\":6.50,\
                    \"importance\":6,\"pinned\":false,\"skill_action\":\"rust::episodic\",\
                    \"source_harness\":\"test-harness\",\"content\":\"café\"}\n";
        let raw = raw_lines(line.as_bytes());
        let a = raw[0]
            .parsed
            .clone()
            .expect("the hand-written line is a record");

        let out = patch_lines(&raw, &pins_of(&[(&a, true)]), &HashSet::new()).unwrap();

        let expected = line.replace("\"pinned\":false", "\"pinned\":true");
        assert_eq!(String::from_utf8(out).unwrap(), expected);
    }

    #[test]
    fn pin_flip_appends_pinned_when_the_key_is_absent() {
        let a = make_learning('A', "absent flag");
        let line = String::from_utf8(line_of(&a))
            .unwrap()
            .replace(",\"pinned\":false", "");
        assert!(!line.contains("\"pinned\""), "precondition: no pinned key");

        let out = patch_lines(
            &raw_lines(line.as_bytes()),
            &pins_of(&[(&a, true)]),
            &HashSet::new(),
        )
        .unwrap();

        let expected = line.replace("}\n", ",\"pinned\":true}\n");
        assert_eq!(String::from_utf8(out).unwrap(), expected);
    }

    #[test]
    fn unpin_writes_false() {
        let mut a = make_learning('A', "unpin");
        a.pinned = true;
        let line = String::from_utf8(line_of(&a)).unwrap();

        let out = patch_lines(
            &raw_lines(line.as_bytes()),
            &pins_of(&[(&a, false)]),
            &HashSet::new(),
        )
        .unwrap();

        let expected = line.replace("\"pinned\":true", "\"pinned\":false");
        assert_ne!(expected, line);
        assert_eq!(String::from_utf8(out).unwrap(), expected);
    }

    #[test]
    fn drop_removes_each_line_with_the_id_and_nothing_else() {
        let a1 = make_learning('A', "first copy");
        let a2 = make_learning('A', "second copy");
        let b = make_learning('B', "kept");
        let mut bytes = line_of(&a1);
        bytes.extend_from_slice(&line_of(&b));
        bytes.extend_from_slice(SKIPPED);
        bytes.extend_from_slice(&line_of(&a2));

        let out = patch_lines(
            &raw_lines(&bytes),
            &HashMap::new(),
            &HashSet::from([a1.id.clone()]),
        )
        .unwrap();

        let mut expected = line_of(&b);
        expected.extend_from_slice(SKIPPED);
        assert_eq!(out, expected);
    }

    #[test]
    fn drop_beats_pin() {
        let a = make_learning('A', "dropped");
        let b = make_learning('B', "kept");
        let mut bytes = line_of(&a);
        bytes.extend_from_slice(&line_of(&b));

        let out = patch_lines(
            &raw_lines(&bytes),
            &pins_of(&[(&a, true)]),
            &HashSet::from([a.id.clone()]),
        )
        .unwrap();

        assert_eq!(out, line_of(&b));
    }
}
