//! `dreamd doctor` — structured health-check output (WEG-66, extended by
//! AILAB-223).
//!
//! Prints one line per check to stdout. Exit 0 if all checks pass; exit 1 if
//! any check emits a WARNING or ERROR. Default checks (when applicable):
//! dream-cycle mode, scaffolded subdirs, index coverage of the episodic log
//! (the set difference between log ids and committed index ids), lessons the
//! last semantic pass could not index, episodic log health (malformed lines /
//! torn tail), record and index schema versions, hand-edited log lines (a
//! `pain` or `importance` the ingest validators refuse, an event id on more
//! than one line, a `timestamp` more than 24 h ahead of the clock), whether
//! the daemon-home `registry.toml` parses, orphaned daemon socket, Tantivy
//! lock-file presence, last autobiography skip, and whether the dream-cycle
//! WAL is readable. The hand-edit and registry checks only report: the
//! readers keep skipping, clamping and upserting as before (BZR-1302).
//!
//! `--repair` unlinks an orphaned daemon socket and rebuilds the derived
//! Tantivy cache by replaying the episodic log, which it only ever reads. The
//! rebuild wipes the whole index dir — the same set the daemon clears on an
//! index-schema migration — so any `.tantivy-*.lock` living inside it goes
//! with it. What doctor must never do, and does not, is target those flock
//! files on their own: flock is advisory, carries no PID, and self-clears when
//! its holder exits (WEG-24-A / AILAB-402; see `check_tantivy_locks`). When
//! the dream-cycle WAL cannot be parsed or validated, taking the writer lock
//! for the repair moves it aside to a `.corrupt-<date>` sidecar, and the
//! repair then reports the store's invariant check.
//!
//! `--cluster-health` recomputes the clusters the dream cycle would promote
//! and compares them against the recurrence sidecar it last wrote.
//!
//! `--provenance` prints the `provenance::verify` report (recomputed Merkle
//! root, orphan edges, missing edges, corrupt lines) and fails the run on any
//! fault. It only reads the ledger; it does not rewrite it, and `--repair`
//! does not touch it either (BZR-148).

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use dreamd_core::autobiography::AutobiographySkip;
use dreamd_core::config::{load_config, Config, DreamCycleMode};
use dreamd_core::consolidation;
use dreamd_core::episodic::MALFORMED_LINE_NUMBERS_CAP;
use dreamd_core::id_minter::MAX_SEED_SKEW;
use dreamd_core::index::{
    check_manifest_version, ManifestCheckOutcome, ManifestVersionError, RecurrenceSidecar,
    INDEX_MANIFEST_FILENAME, SCHEMA_VERSION as INDEX_SCHEMA_VERSION,
};
use dreamd_core::ingress::LearnIngress;
use dreamd_core::layout::AgentRoot;
use dreamd_core::llm;
use dreamd_core::registry::Registry;
use dreamd_protocol::{AgentLearning, RECORD_SCHEMA_VERSION};

/// Mode flags for a doctor run. Mirrors `cli::DoctorArgs` without pulling
/// clap into the command module; every flag composes with the default checks.
#[derive(Debug, Clone, Copy, Default)]
pub struct DoctorFlags {
    /// Unlink an orphaned daemon socket, move an unparseable dream-cycle WAL
    /// aside, and rebuild the Tantivy cache.
    pub repair: bool,
    /// Compare the clusters the dream cycle would promote against the
    /// recurrence sidecar.
    pub cluster_health: bool,
    /// Print the provenance ledger verify report; flag orphans, missing
    /// edges, and corrupt lines.
    pub provenance: bool,
}

/// Run `dreamd doctor` and write output to `out` (repair refusals go to `err`).
///
/// `socket` is the resolved daemon UDS path (`$DREAMD_SOCK` else the
/// daemon-home socket), or `None` when no path could be resolved. Off Unix the
/// caller always passes `None`: the daemon there is loopback TCP, and
/// `--repair` probes it through `server.json` instead.
///
/// `registry` is the daemon-home `registry.toml`, or `None` when the home
/// directory can't be resolved. Doctor only reads it, to report a file the
/// daemon and `dreamd status` cannot parse.
///
/// `now_sec` is caller-provided (unix seconds) so the recurrence windows
/// `--cluster-health` recomputes stay deterministic under test, matching the
/// dream cycle's own convention — do not call `Utc::now()` below. The
/// `future_timestamp` check measures "ahead of the clock" from it too.
///
/// Returns `Ok(true)` if all checks passed (exit 0 caller), `Ok(false)` if any
/// WARNING or ERROR was emitted or a repair step failed (exit 1 caller).
#[allow(clippy::too_many_arguments)]
pub fn run(
    config: &Config,
    agent_root: &AgentRoot,
    skip: Option<&AutobiographySkip>,
    socket: Option<&Path>,
    registry: Option<&Path>,
    flags: DoctorFlags,
    now_sec: i64,
    out: &mut impl Write,
    err: &mut impl Write,
) -> io::Result<bool> {
    let mut all_ok = true;

    // Dream-cycle mode line.
    match config.dream_cycle_mode {
        DreamCycleMode::Manual => {
            writeln!(out, "dream_cycle_mode: manual")?;
        }
        DreamCycleMode::Auto => {
            writeln!(out, "dream_cycle_mode: auto  [WARNING: not supported]")?;
            all_ok = false;
        }
    }

    // AILAB-223 — hard integrity over the init-scaffolded subdirs.
    all_ok &= check_subdirs(agent_root, out)?;

    // Index vs JSONL: the set difference between the log's ids and the
    // committed index's (BZR-1236), read without a writer or the lock.
    #[cfg(unix)]
    {
        match dreamd_core::server::assess_index_coverage(agent_root) {
            Ok(coverage) if !coverage.present => {
                writeln!(
                    out,
                    "index_freshness: no index yet  (the next writer open builds it)"
                )?;
            }
            Ok(coverage)
                if coverage.missing == 0
                    && coverage.not_in_log == 0
                    && coverage.duplicated == 0 =>
            {
                writeln!(out, "index_freshness: ok")?;
            }
            Ok(coverage) => {
                writeln!(
                    out,
                    "index_freshness: stale  [WARNING: {} event(s) not indexed, \
                     {} indexed event(s) not in the log, {} duplicated; the next \
                     writer open re-covers the log, or run `dreamd doctor --repair`]",
                    coverage.missing, coverage.not_in_log, coverage.duplicated,
                )?;
                all_ok = false;
            }
            Err(e) => {
                writeln!(
                    out,
                    "index_freshness: error  [WARNING: could not assess: {e}]"
                )?;
                all_ok = false;
            }
        }

        // AILAB-700 — lessons the last semantic pass could not index because
        // their exemplar event was gone from the episodic log. Read from the
        // sidecar the pass writes, never recomputed here: doctor re-resolving
        // LESSONS.md would be a second parser for the same rule. Absent file
        // (never dreamed) or an empty list prints nothing — a healthy store
        // stays quiet.
        match dreamd_core::server::read_semantic_pass_record(agent_root) {
            Ok(Some(record)) if !record.skipped_lesson_ids.is_empty() => {
                let count = record.skipped_lesson_ids.len();
                let first = record.skipped_lesson_ids[0].as_str();
                let and_more = match count - 1 {
                    0 => String::new(),
                    n => format!(" and {n} more"),
                };
                writeln!(
                    out,
                    "semantic_lessons: {count} not indexed  [WARNING: exemplar event \
                     missing from the episodic log for lesson {first}{and_more} \
                     (cluster {}); recall cannot return these lessons. Likely an \
                     unpinned or archived exemplar — see `dreamd archive \
                     --force-unpin`.]",
                    record.cluster_key,
                )?;
                all_ok = false;
            }
            Ok(_) => {}
            Err(e) => {
                // Fails the check: an unreadable sidecar means doctor cannot
                // answer the question it was asked — whether any lesson is
                // missing from recall — and silence there is indistinguishable
                // from a clean pass. `index_freshness`'s own error arm above
                // already takes the same posture.
                writeln!(
                    out,
                    "semantic_lessons: unreadable  [WARNING: could not read the semantic \
                     pass report: {e}]"
                )?;
                all_ok = false;
            }
        }
    }

    // Episodic log health: malformed `\n`-terminated lines and a torn tail
    // (WEG-132).
    let episodic_path = agent_root.episodic_jsonl();
    if episodic_path.exists() {
        match dreamd_core::episodic::assess_log_health(&episodic_path) {
            Ok(health) if health.malformed_line_count == 0 && health.torn_tail_bytes == 0 => {
                writeln!(out, "episodic_log: ok")?;
            }
            Ok(health) => {
                if health.malformed_line_count > 0 {
                    let first_lines = health
                        .malformed_line_numbers
                        .iter()
                        .map(|n| n.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    writeln!(
                        out,
                        "episodic_log: {} malformed line(s) (first at line(s) {first_lines})  \
                         [WARNING: hand-edited or corrupt \\n-terminated lines in \
                         AGENT_LEARNINGS.jsonl; skipped at read time]",
                        health.malformed_line_count,
                    )?;
                }
                if health.torn_tail_bytes > 0 {
                    writeln!(
                        out,
                        "episodic_log: torn tail ({} byte(s))  [WARNING: incomplete final \
                         line with no trailing newline; on daemon open it gets its newline \
                         if it parses as a record, and is truncated otherwise]",
                        health.torn_tail_bytes,
                    )?;
                }
                all_ok = false;
            }
            Err(e) => {
                writeln!(out, "episodic_log: error  [WARNING: could not assess: {e}]")?;
                all_ok = false;
            }
        }
    }

    // One read of the episodic log backs the record-schema check, the three
    // hand-edit checks and --cluster-health (an absent log reads as no records).
    // `events` is what `read_all` returns, in the same order; `lines[i]` is the
    // file line of `events[i]`, the numbering `malformed line(s)` uses (blank,
    // malformed and torn lines count too).
    let records: Option<(Vec<usize>, Vec<AgentLearning>)> =
        match dreamd_core::episodic::read_raw(&episodic_path) {
            Ok(raw) => Some(
                raw.into_iter()
                    .enumerate()
                    .filter_map(|(i, line)| line.parsed.map(|record| (i + 1, record)))
                    .unzip(),
            ),
            Err(e) => {
                writeln!(
                    out,
                    "schema_record: error  [WARNING: could not read the episodic log: {e}]"
                )?;
                all_ok = false;
                None
            }
        };
    if let Some((lines, events)) = &records {
        all_ok &= check_schema_record(events, out)?;
        all_ok &= check_score_range(lines, events, out)?;
        all_ok &= check_duplicate_ids(lines, events, out)?;
        all_ok &= check_future_timestamps(lines, events, now_sec, out)?;
    }
    let events = records.as_ref().map(|(_, events)| events.as_slice());

    all_ok &= check_registry(registry, out)?;
    all_ok &= check_schema_index(agent_root, out)?;
    all_ok &= check_orphaned_uds(socket, out)?;
    #[cfg(unix)]
    check_tantivy_locks(agent_root, out)?;

    // WEG-63 — last autobiography skip (if any).
    if let Some(s) = skip {
        let elapsed = now_sec.saturating_sub(s.at);
        let duration_str = format_elapsed(elapsed);
        let file_count = s.files.len();
        writeln!(
            out,
            "last autobiography skip: {} ago — {} — {} files",
            duration_str, s.reason, file_count,
        )?;
    }

    if flags.cluster_health {
        all_ok &= check_cluster_health(agent_root, events, now_sec, out)?;
        // AILAB-196 — additive report, deliberately not `&=`-ed into `all_ok`.
        report_next_cycle_cost(agent_root, now_sec, out)?;
    }

    if flags.provenance {
        all_ok &= check_provenance(agent_root, out)?;
    }

    all_ok &= check_dream_cycle_wal(agent_root, flags.repair, out)?;

    if flags.repair {
        all_ok &= run_repair(agent_root, socket, out, err)?;
    }

    Ok(all_ok)
}

/// Hard integrity over the five dirs `dreamd init` scaffolds. `skills/` and
/// `protocols/` are deliberately not checked: init does not create them, so
/// their absence is normal on a healthy store (AILAB-223).
fn check_subdirs(agent_root: &AgentRoot, out: &mut impl Write) -> io::Result<bool> {
    let scaffolded = [
        ("working/", agent_root.working_dir()),
        ("episodic/", agent_root.episodic_dir()),
        ("semantic/", agent_root.semantic_dir()),
        ("personal/", agent_root.personal_dir()),
        (".dreamd/", agent_root.dreamd_dir()),
    ];
    let mut ok = true;
    for (name, path) in &scaffolded {
        if !path.is_dir() {
            writeln!(
                out,
                "missing_subdir: {name}  [WARNING: run `dreamd init` to re-scaffold]"
            )?;
            ok = false;
        }
    }
    if ok {
        writeln!(out, "subdirs: ok")?;
    }
    Ok(ok)
}

/// Compare stored records against the episodic record schema
/// ([`RECORD_SCHEMA_VERSION`]). This is the durable-data stream — distinct
/// from the derived index manifest stream reported by [`check_schema_index`].
fn check_schema_record(events: &[AgentLearning], out: &mut impl Write) -> io::Result<bool> {
    let foreign: BTreeSet<&str> = events
        .iter()
        .map(|e| e.schema_version.as_str())
        .filter(|v| *v != RECORD_SCHEMA_VERSION)
        .collect();
    if foreign.is_empty() {
        if events.is_empty() {
            writeln!(out, "schema_record: ok (no records)")?;
        } else {
            writeln!(
                out,
                "schema_record: ok ({} record(s) at {RECORD_SCHEMA_VERSION})",
                events.len()
            )?;
        }
        Ok(true)
    } else {
        let list = foreign.into_iter().collect::<Vec<_>>().join(", ");
        writeln!(
            out,
            "schema_record: mismatch ({list})  [WARNING: episodic records not at record \
             schema {RECORD_SCHEMA_VERSION}; see `dreamd migrate`]"
        )?;
        Ok(false)
    }
}

/// `{n} line(s) (first at line(s) {list})`: `n` uncapped, the list the first
/// `MALFORMED_LINE_NUMBERS_CAP` line numbers in file order, joined with ", ".
fn lines_head(lines: &[usize]) -> String {
    let first = lines
        .iter()
        .take(MALFORMED_LINE_NUMBERS_CAP)
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!("{} line(s) (first at line(s) {first})", lines.len())
}

/// The file lines of the records in `events` that `flag` picks, in file
/// order. `lines[i]` is the file line of `events[i]`.
fn flagged_lines(
    lines: &[usize],
    events: &[AgentLearning],
    flag: impl Fn(&AgentLearning) -> bool,
) -> Vec<usize> {
    lines
        .iter()
        .zip(events)
        .filter(|(_, record)| flag(record))
        .map(|(&line, _)| line)
        .collect()
}

/// Records whose `pain` or `importance` the ingest validators refuse. Doctor
/// calls the validators `POST /api/v1/learn` runs, on the parsed `f32` the
/// index stores, so its bounds are theirs. Recall clamps these values
/// (`salience::clamp_score`); this only reports them (BZR-1302).
fn check_score_range(
    lines: &[usize],
    events: &[AgentLearning],
    out: &mut impl Write,
) -> io::Result<bool> {
    let flagged = flagged_lines(lines, events, |record| {
        LearnIngress::validate_pain(record.pain).is_err()
            || LearnIngress::validate_importance(record.importance).is_err()
    });
    if flagged.is_empty() {
        return Ok(true);
    }
    writeln!(
        out,
        "score_range: {}  [WARNING: pain or importance outside 0.0..=10.0 in \
         AGENT_LEARNINGS.jsonl; recall scores these lines at the nearest bound]",
        lines_head(&flagged)
    )?;
    Ok(false)
}

/// Every line whose event id another line also carries, first copies
/// included: fixing the warning means choosing which copy to keep. The ids
/// come from `invariants::duplicate_event_ids`, the one duplicate detector;
/// doctor counts no ids itself (BZR-1302).
fn check_duplicate_ids(
    lines: &[usize],
    events: &[AgentLearning],
    out: &mut impl Write,
) -> io::Result<bool> {
    use dreamd_core::invariants::{duplicate_event_ids, Violation};

    let violations = duplicate_event_ids(events);
    if violations.is_empty() {
        return Ok(true);
    }
    let repeated: BTreeSet<&str> = violations
        .iter()
        .filter_map(|violation| match violation {
            Violation::DuplicateEventId { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    let flagged = flagged_lines(lines, events, |record| {
        repeated.contains(record.id.as_str())
    });
    writeln!(
        out,
        "duplicate_id: {}  [WARNING: {} event id(s) on more than one line of \
         AGENT_LEARNINGS.jsonl; the index keeps one document per id]",
        lines_head(&flagged),
        violations.len()
    )?;
    Ok(false)
}

/// Records whose `timestamp` is more than [`MAX_SEED_SKEW`] (24 h, the
/// minter's "ahead of the clock") past `now_sec`. Reads the `timestamp`
/// field, not the id. Recall floors such a record's age at 0; this only
/// reports it (BZR-1302).
fn check_future_timestamps(
    lines: &[usize],
    events: &[AgentLearning],
    now_sec: i64,
    out: &mut impl Write,
) -> io::Result<bool> {
    let horizon = now_sec.saturating_add(MAX_SEED_SKEW.as_secs() as i64);
    let flagged = flagged_lines(lines, events, |record| {
        record.timestamp.timestamp() > horizon
    });
    if flagged.is_empty() {
        return Ok(true);
    }
    writeln!(
        out,
        "future_timestamp: {}  [WARNING: timestamp more than 24 h ahead of the clock in \
         AGENT_LEARNINGS.jsonl; recall scores these lines as new until the clock reaches them]",
        lines_head(&flagged)
    )?;
    Ok(false)
}

/// Report a daemon-home `registry.toml` that does not parse as a [`Registry`],
/// the type `registry::resolve_project` and `RegistryCache` parse with. A
/// `None` path or an absent file passes silently. Only reads the file; it does
/// not check whether this project is registered (BZR-1302).
fn check_registry(registry: Option<&Path>, out: &mut impl Write) -> io::Result<bool> {
    let Some(path) = registry else {
        return Ok(true);
    };
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(e) => {
            writeln!(
                out,
                "registry: error  [WARNING: could not read {}: {e}]",
                path.display()
            )?;
            return Ok(false);
        }
    };
    let e = match toml::from_str::<Registry>(&raw) {
        Ok(_) => return Ok(true),
        Err(e) => e,
    };
    // The 1-based line of the span's start, counted over bytes so a span
    // inside a multi-byte character cannot panic.
    let at = e
        .span()
        .map(|span| {
            let end = span.start.min(raw.len());
            let line = raw.as_bytes()[..end]
                .iter()
                .filter(|&&b| b == b'\n')
                .count()
                + 1;
            format!(" at line {line}")
        })
        .unwrap_or_default();
    writeln!(
        out,
        "registry: parse error{at}  [WARNING: {} does not parse; `dreamd status` reports \
         every project as not registered and the daemon answers 500 until it parses]",
        path.display()
    )?;
    Ok(false)
}

/// Report the Tantivy index manifest stream (`index::SCHEMA_VERSION` via
/// `check_manifest_version`). The index is a rebuildable derived cache:
/// older-than-binary self-heals on the next daemon open (or `--repair`);
/// newer-than-binary is the only hard failure.
fn check_schema_index(agent_root: &AgentRoot, out: &mut impl Write) -> io::Result<bool> {
    let manifest_path = agent_root.dreamd_dir().join(INDEX_MANIFEST_FILENAME);
    match check_manifest_version(&manifest_path) {
        Ok(ManifestCheckOutcome::Absent) => {
            writeln!(
                out,
                "schema_index: absent (no index manifest yet; the daemon writes one on first open)"
            )?;
            Ok(true)
        }
        Ok(ManifestCheckOutcome::Current) => {
            writeln!(out, "schema_index: ok ({INDEX_SCHEMA_VERSION})")?;
            Ok(true)
        }
        Ok(ManifestCheckOutcome::NeedsMigration { from }) => {
            writeln!(
                out,
                "schema_index: {from} older than binary {INDEX_SCHEMA_VERSION}  [WARNING: \
                 derived cache; rebuilt from the episodic log on next daemon open, or run \
                 `dreamd doctor --repair` now]"
            )?;
            Ok(false)
        }
        Err(ManifestVersionError::TooNew { manifest, binary }) => {
            writeln!(
                out,
                "schema_index: {manifest} newer than binary {binary}  [ERROR: upgrade dreamd, \
                 or run `dreamd doctor --repair` to rebuild under this binary]"
            )?;
            Ok(false)
        }
        Err(e) => {
            writeln!(
                out,
                "schema_index: error  [WARNING: could not read index manifest: {e}]"
            )?;
            Ok(false)
        }
    }
}

/// An orphaned UDS is a socket file with nothing listening behind it — the
/// operational trace a daemon leaves when it dies without cleanup.
#[cfg(unix)]
fn check_orphaned_uds(socket: Option<&Path>, out: &mut impl Write) -> io::Result<bool> {
    let Some(sock) = socket else {
        writeln!(out, "orphaned_uds: none (no daemon socket path resolved)")?;
        return Ok(true);
    };
    if !sock.exists() {
        writeln!(out, "orphaned_uds: none")?;
        return Ok(true);
    }
    if dreamd_core::server::is_daemon_socket_live(sock) {
        writeln!(
            out,
            "orphaned_uds: none (daemon live at {})",
            sock.display()
        )?;
        Ok(true)
    } else {
        writeln!(
            out,
            "orphaned_uds: {}  [WARNING: socket file exists but nothing is listening; \
             `dreamd doctor --repair` unlinks it]",
            sock.display()
        )?;
        Ok(false)
    }
}

#[cfg(not(unix))]
fn check_orphaned_uds(_socket: Option<&Path>, out: &mut impl Write) -> io::Result<bool> {
    writeln!(out, "orphaned_uds: n/a (non-unix)")?;
    Ok(true)
}

/// True only if `path` is present and is a unix-domain socket. Guards the one
/// destructive step `--repair` takes outside the derived index cache.
#[cfg(unix)]
fn is_unix_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(path).is_ok_and(|m| m.file_type().is_socket())
}

/// Presence report for Tantivy flock files under the index dir. flock is
/// advisory and clears when its holder exits (docs/architecture.md), so
/// doctor never unlinks these — INFO only, never a dead-holder claim.
#[cfg(unix)]
fn check_tantivy_locks(agent_root: &AgentRoot, out: &mut impl Write) -> io::Result<()> {
    let index_dir = agent_root
        .dreamd_dir()
        .join(dreamd_core::server::INDEX_DIR_NAME);
    for name in [".tantivy-writer.lock", ".tantivy-meta.lock"] {
        if index_dir.join(name).exists() {
            writeln!(
                out,
                "tantivy_lock: {name} present  [INFO: advisory flock; harmless and left \
                 in place by doctor]"
            )?;
        }
    }
    Ok(())
}

/// `--cluster-health`: recompute the clusters the dream cycle would promote
/// from the episodic log and compare them against the recurrence sidecar it
/// last wrote. An absent sidecar on a never-dreamed store is benign.
///
/// The derivation goes through [`consolidation::compute_promoted_clusters`] —
/// the same threshold-window and deepest-wins rules the engine promotes with —
/// so a match means the sidecar is current. Comparing raw prefix-tree counts
/// instead would report drift on every healthy store, because the sidecar only
/// ever holds promoted, deepest-wins-claimed counts.
fn check_cluster_health(
    agent_root: &AgentRoot,
    events: Option<&[AgentLearning]>,
    now_sec: i64,
    out: &mut impl Write,
) -> io::Result<bool> {
    let sidecar_path = agent_root.semantic_dir().join("recurrence_counts.json");
    let bytes = match std::fs::read(&sidecar_path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            writeln!(
                out,
                "cluster_health: no sidecar (semantic/recurrence_counts.json is written by \
                 the first dream cycle)"
            )?;
            return Ok(true);
        }
        Err(e) => {
            writeln!(
                out,
                "cluster_health: error  [WARNING: could not read recurrence sidecar: {e}]"
            )?;
            return Ok(false);
        }
    };
    let sidecar: RecurrenceSidecar = match serde_json::from_slice(&bytes) {
        Ok(s) => s,
        Err(e) => {
            writeln!(
                out,
                "cluster_health: error  [WARNING: recurrence sidecar is corrupt: {e}]"
            )?;
            return Ok(false);
        }
    };
    let Some(events) = events else {
        writeln!(
            out,
            "cluster_health: skipped (episodic log unreadable; see schema_record above)"
        )?;
        return Ok(false);
    };

    let derived: BTreeMap<String, usize> =
        consolidation::compute_promoted_clusters(events, now_sec)
            .into_iter()
            .map(|c| (c.cluster_key, c.events.len()))
            .collect();
    let sidecar_counts: BTreeMap<&str, u32> = sidecar
        .clusters
        .iter()
        .map(|c| (c.skill_action.as_str(), c.count))
        .collect();

    let mut drifted = 0usize;
    for (key, jsonl_count) in &derived {
        match sidecar_counts.get(key.as_str()) {
            Some(&count) if count as usize == *jsonl_count => {
                writeln!(
                    out,
                    "cluster_health: {key} jsonl={jsonl_count} sidecar={count}"
                )?;
            }
            Some(&count) => {
                writeln!(
                    out,
                    "cluster_health: {key} jsonl={jsonl_count} sidecar={count}  [WARNING: drift]"
                )?;
                drifted += 1;
            }
            None => {
                writeln!(
                    out,
                    "cluster_health: {key} jsonl={jsonl_count} sidecar=(none)  [WARNING: drift]"
                )?;
                drifted += 1;
            }
        }
    }
    for (key, count) in &sidecar_counts {
        if !derived.contains_key(*key) {
            writeln!(
                out,
                "cluster_health: {key} jsonl=(none) sidecar={count}  [WARNING: drift]"
            )?;
            drifted += 1;
        }
    }

    if drifted == 0 {
        writeln!(
            out,
            "cluster_health: ok ({} prefix(es) match the sidecar)",
            derived.len()
        )?;
        Ok(true)
    } else {
        writeln!(
            out,
            "cluster_health: {drifted} drifted key(s)  [WARNING: recurrence sidecar out of \
             sync with the episodic log; the next dream cycle rewrites it]"
        )?;
        Ok(false)
    }
}

/// `--cluster-health` addendum (AILAB-196): price what the **next**
/// dream cycle's LLM composition would cost, before it happens.
///
/// This is a *report, not a gate*. It returns `io::Result<()>`, not a health
/// `bool`, and its call site does not fold it into `all_ok` — an over-cap
/// estimate must never change doctor's exit code. Sidecar drift still owns the
/// WARNING/exit contract; a spend the operator has already capped is
/// information, not ill health. Every failure arm below still returns `Ok(())`
/// for the same reason.
///
/// Doctor never stages or commits a cycle's plan (a plan can unlink
/// `semantic/LESSONS.md`) and never writes the sidecar (`run_cluster_engine`
/// writes `semantic/recurrence_counts.json`). Selection therefore goes through
/// [`consolidation::preview_select_lesson`], the read-only `--dry` seam: a
/// health check must not mutate the store it is auditing.
///
/// The estimate is purely local: [`llm::estimate_prompt_cost`] tokenizes with
/// the bundled `cl100k_base` table. No genai client is constructed and no
/// network call is made.
///
/// `now_sec` is caller-provided for the same determinism reason as [`run`] —
/// do not call `Utc::now()` here.
fn report_next_cycle_cost(
    agent_root: &AgentRoot,
    now_sec: i64,
    out: &mut impl Write,
) -> io::Result<()> {
    // Same degrade-to-defaults rule the dream cycle itself uses: a broken
    // config.toml must not fail a check that would otherwise report cleanly.
    let config = load_config(agent_root.project_root()).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "config load failed; using defaults for the cost estimate");
        Config::default()
    });

    let selected = match consolidation::preview_select_lesson(agent_root, now_sec) {
        Ok(selected) => selected,
        Err(e) => {
            // Informational: an unreadable episodic log is already reported by
            // `schema_record` above, and pricing must not double-count it into
            // the exit code.
            writeln!(
                out,
                "cluster_health: next_cycle_est_usd=unavailable (could not preview the next \
                 cycle: {e})"
            )?;
            return Ok(());
        }
    };

    let Some(selected) = selected else {
        writeln!(
            out,
            "cluster_health: next_cycle_est_usd=0.00 (no promotion)"
        )?;
        return Ok(());
    };

    let prompt = llm::build_lesson_prompt(&selected.cluster_key, &selected.events);
    let model = config.model.as_str();
    let cap_usd = config.cost_cap_usd;

    match llm::estimate_prompt_cost(model, &prompt) {
        Some(est) => {
            let usd = est.estimated_usd;
            let tokens = est.input_tokens;
            // Over cap keeps the line shape but swaps the parenthetical:
            // repeating `(within cap)` next to an abort warning would be false.
            let verdict = if llm::exceeds_cap(&est, cap_usd) {
                "(over cap) [WARNING: would abort LLM]"
            } else {
                "(within cap)"
            };
            writeln!(
                out,
                "cluster_health: next_cycle_est_usd={usd:.4} cap_usd={cap_usd:.2} \
                 model={model} tokens={tokens} {verdict}"
            )?;
        }
        // An unknown model has no price row, so there is no USD figure and no
        // token count that would be honest to print — a `0.0000` here reads as
        // "free", which is the opposite of what this arm means. Both unknowable
        // fields therefore say `unknown` while the line keeps its shape and both
        // greppable markers (`next_cycle_est_usd=` and the abort WARNING). The
        // cycle treats `None` as over-cap, so the warning is accurate.
        None => writeln!(
            out,
            "cluster_health: next_cycle_est_usd=unknown cap_usd={cap_usd:.2} model={model} \
             tokens=unknown (unknown model — not in the price table) [WARNING: would abort LLM]"
        )?,
    }

    Ok(())
}

/// `--provenance`: print the `provenance::verify` report. Orphans, missing
/// edges, corrupt lines, and a verify error fail the check;
/// `skipped_unknown_kind` is printed and does not. Reads the ledger only.
fn check_provenance(agent_root: &AgentRoot, out: &mut impl Write) -> io::Result<bool> {
    let report = match dreamd_core::provenance::verify(agent_root) {
        Ok(report) => report,
        Err(e) => {
            writeln!(
                out,
                "provenance: error  [WARNING: could not verify the ledger: {e}]"
            )?;
            return Ok(false);
        }
    };
    let faults = report.orphans.len() + report.missing.len() + report.corrupt_lines.len();
    if faults == 0 {
        writeln!(out, "provenance: ok root={}", report.root)?;
    } else {
        writeln!(out, "provenance: root={}", report.root)?;
        for edge in &report.orphans {
            writeln!(
                out,
                "provenance: orphan line={} kind={} from={} to={}",
                edge.line, edge.kind, edge.from, edge.to
            )?;
        }
        for edge in &report.missing {
            writeln!(
                out,
                "provenance: missing line={} kind={} from={} to={}",
                edge.line, edge.kind, edge.from, edge.to
            )?;
        }
        for line in &report.corrupt_lines {
            writeln!(out, "provenance: corrupt line={line}")?;
        }
        writeln!(
            out,
            "provenance: {faults} fault(s)  [WARNING: ledger disagrees with the live memory files]"
        )?;
    }
    if report.skipped_unknown_kind > 0 {
        writeln!(
            out,
            "provenance: skipped_unknown_kind={}",
            report.skipped_unknown_kind
        )?;
    }
    Ok(faults == 0)
}

/// Read the dream-cycle WAL without taking the project writer lock.
///
/// A lock acquire would run recovery and this line would never print. With
/// `--repair`, a WAL with an unknown format, a bad redo path or invalid JSON
/// does not by itself fail the run: repair quarantines it, and a repair that
/// then fails still fails the run. An I/O error reading the WAL (the lines
/// labeled `unreadable`) always fails the run, because repair cannot move it.
fn check_dream_cycle_wal(
    agent_root: &AgentRoot,
    repair: bool,
    out: &mut impl Write,
) -> io::Result<bool> {
    use dreamd_core::wal::{classify, WalError};

    let path = agent_root.wal_path();
    let shown = path.display();
    match classify(agent_root) {
        Ok(_) => {
            writeln!(out, "dream_cycle_wal: ok")?;
            Ok(true)
        }
        Err(WalError::UnknownFormat(format)) => {
            writeln!(
                out,
                "dream_cycle_wal: unknown format \"{format}\"  [ERROR: {shown}; run `dreamd doctor --repair`]"
            )?;
            Ok(repair)
        }
        Err(WalError::BadRedoPath(bad)) => {
            writeln!(
                out,
                "dream_cycle_wal: bad path \"{bad}\"  [ERROR: {shown}; run `dreamd doctor --repair`]"
            )?;
            Ok(repair)
        }
        Err(WalError::Json(_)) => {
            writeln!(
                out,
                "dream_cycle_wal: not valid JSON  [ERROR: {shown}; run `dreamd doctor --repair`]"
            )?;
            Ok(repair)
        }
        Err(WalError::Io(err)) => {
            writeln!(out, "dream_cycle_wal: unreadable ({err})  [ERROR: {shown}]")?;
            Ok(false)
        }
        Err(err) => {
            writeln!(out, "dream_cycle_wal: unreadable ({err})  [ERROR: {shown}]")?;
            Ok(false)
        }
    }
}

/// `--repair`: unlink an orphaned daemon socket, wipe the rebuildable index
/// cache, and reopen the index to force a full replay of the episodic log
/// (which repair only ever reads). Taking the writer lock as
/// `WriterKind::Repair` also moves an unparseable dream-cycle WAL aside; when
/// it does, the store invariants are checked and reported against the
/// quarantined file. On Windows the wipe is also how a segment
/// file that Tantivy could not delete while it was still mapped leaves the
/// index directory: stop the daemon first, then this removes the directory
/// and rebuilds it. Fails closed when a live daemon is detected — wiping the
/// cache underneath its writer would race it — and when the project writer
/// lock is held by any other writer.
fn run_repair(
    agent_root: &AgentRoot,
    socket: Option<&Path>,
    out: &mut impl Write,
    err: &mut impl Write,
) -> io::Result<bool> {
    use dreamd_core::server::{
        TantivyIndexHandle, DEFAULT_COMMIT_CADENCE, INDEX_DIR_NAME, INDEX_PROGRESS_FILENAME,
    };
    use dreamd_core::writer_lock::{WriterKind, WriterLock, WriterLockError};

    #[cfg(unix)]
    if let Some(sock) = socket {
        if dreamd_core::server::is_daemon_socket_live(sock) {
            writeln!(
                err,
                "dreamd: error — doctor --repair refused: a daemon is live on {}. \
                 Stop `dreamd watch` / `dreamd mcp` first, then re-run.",
                sock.display()
            )?;
            return Ok(false);
        }
    }

    // Held through the unlink, the wipe and the rebuild. Every writer of this
    // project takes it, so the wipe cannot run under one.
    let lock = match WriterLock::acquire(agent_root, WriterKind::Repair) {
        Ok(lock) => lock,
        Err(e @ WriterLockError::Busy { .. }) => {
            writeln!(
                err,
                "dreamd: error — doctor --repair refused: {e}. Stop it first, then re-run."
            )?;
            return Ok(false);
        }
        Err(e) => {
            writeln!(err, "dreamd: error — doctor --repair refused: {e}")?;
            return Ok(false);
        }
    };

    let mut actions: Vec<String> = Vec::new();

    #[cfg(not(unix))]
    {
        let _ = socket;
        if let Some(home) = dreamd_core::layout::home_dir() {
            let server_json =
                dreamd_core::layout::DaemonHome::new(home.join(".agent")).server_json();
            if dreamd_core::daemon_client::tcp_daemon_is_live(&server_json) {
                writeln!(
                    err,
                    "dreamd: error — doctor --repair refused: a daemon is live. \
                     Stop `dreamd watch` first, then re-run."
                )?;
                return Ok(false);
            }
        }
    }

    // The liveness probe above came back dead, so a still-present socket file
    // is an orphan and safe to unlink. Unix only: Windows has no socket file.
    #[cfg(unix)]
    {
        // Confirm it really is a socket first:
        // `is_daemon_socket_live` also answers "dead" for a regular file (connect
        // fails with ENOTSOCK), so an operator pointing $DREAMD_SOCK at the wrong
        // path must not have that file deleted out from under them.
        if let Some(sock) = socket {
            if !is_unix_socket(sock) {
                if sock.exists() {
                    writeln!(
                        err,
                        "dreamd: error — {} is not a unix socket; refusing to unlink it.",
                        sock.display()
                    )?;
                    return Ok(false);
                }
            } else {
                match std::fs::remove_file(sock) {
                    Ok(()) => actions.push(format!("unlinked orphaned socket {}", sock.display())),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => {
                        writeln!(
                            err,
                            "dreamd: error — could not unlink {}: {e}",
                            sock.display()
                        )?;
                        return Ok(false);
                    }
                }
            }
        }
    }

    // Wipe only the rebuildable derived cache — the same set the daemon
    // clears on an index schema migration. The episodic log is replay input,
    // never a repair target.
    let dreamd_dir = agent_root.dreamd_dir();
    match std::fs::remove_dir_all(dreamd_dir.join(INDEX_DIR_NAME)) {
        Ok(()) => actions.push("removed index dir".to_string()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            writeln!(err, "dreamd: error — could not remove index dir: {e}")?;
            return Ok(false);
        }
    }
    for name in [INDEX_MANIFEST_FILENAME, INDEX_PROGRESS_FILENAME] {
        match std::fs::remove_file(dreamd_dir.join(name)) {
            Ok(()) => actions.push(format!("removed {name}")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                writeln!(err, "dreamd: error — could not remove {name}: {e}")?;
                return Ok(false);
            }
        }
    }

    // Reopen to force a full replay of the episodic log into a fresh index.
    // A current-thread runtime is enough — the rebuild is one blocking replay,
    // not a serving daemon — and matches `TantivyIndexHandle::close`.
    //
    // Caller contract (AILAB-575): `out`/`err` must not be held
    // `StdoutLock`/`StderrLock` values. `open` below blocks on Tantivy's
    // single-threaded `segment_updater`, and that thread emits `tracing` events
    // which the console layer writes to `std::io::stderr`. A held lock makes the
    // updater block on this thread while this thread blocks on the updater —
    // `doctor --repair` then never exits. `run_doctor` in `cli.rs` passes
    // unlocked `Stdout`/`Stderr` handles for exactly this reason.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            writeln!(
                err,
                "dreamd: error — could not start runtime for index rebuild: {e}"
            )?;
            return Ok(false);
        }
    };
    let reopened = {
        let _guard = rt.enter();
        TantivyIndexHandle::open(agent_root, &lock, DEFAULT_COMMIT_CADENCE)
    };
    let mut invariants: Option<(
        PathBuf,
        Result<Vec<dreamd_core::invariants::Violation>, String>,
    )> = None;
    match reopened {
        Ok(handle) => {
            if let Some(sidecar) = lock.quarantined_wal() {
                let checked =
                    dreamd_core::invariants::check_store(agent_root, Some(handle.reader()))
                        .map_err(|e| e.to_string());
                invariants = Some((sidecar.to_path_buf(), checked));
            }
            // Drain and join the indexer task before returning. Dropping the
            // handle instead leaves that task holding the `IndexWriter`, and
            // tearing the runtime down around a live writer never completes —
            // `dreamd doctor --repair` hangs forever instead of exiting.
            if let Err(e) = rt.block_on(handle.shutdown()) {
                writeln!(err, "dreamd: error — index rebuild did not drain: {e}")?;
                return Ok(false);
            }
            actions.push("rebuilt index from the episodic log".to_string());
        }
        Err(e) => {
            writeln!(err, "dreamd: error — index rebuild failed: {e}")?;
            return Ok(false);
        }
    }

    // The rebuild arm above always records an action on success, so `actions`
    // is never empty here.
    writeln!(out, "repair: {}", actions.join("; "))?;
    let Some((sidecar, checked)) = invariants else {
        return Ok(true);
    };
    writeln!(out, "quarantined dream-cycle WAL to {}", sidecar.display())?;
    match checked {
        Ok(violations) if violations.is_empty() => {
            writeln!(out, "store invariants: ok")?;
            Ok(true)
        }
        Ok(violations) => {
            let listed = violations
                .iter()
                .map(violation_plain)
                .collect::<Vec<_>>()
                .join("; ");
            writeln!(
                out,
                "store invariants: {} violation(s): {listed}. A WAL corrupted after its commit \
                 point can leave a half-applied cycle. The quarantined WAL is {}.",
                violations.len(),
                sidecar.display()
            )?;
            Ok(false)
        }
        Err(reason) => {
            writeln!(
                err,
                "dreamd: error — store invariants could not be checked ({reason}); the \
                 quarantined WAL is {}",
                sidecar.display()
            )?;
            Ok(false)
        }
    }
}

fn violation_plain(violation: &dreamd_core::invariants::Violation) -> String {
    use dreamd_core::invariants::Violation;
    match violation {
        Violation::DuplicateEventId { id, count } => {
            format!("duplicate event id {id} appears {count} times")
        }
        Violation::MissingFromIndex { id } => format!("event {id} is missing from the index"),
        Violation::NotInLog { id } => format!("index document {id} is not in the log"),
        Violation::DuplicateIndexDoc { id, count } => {
            format!("index document {id} appears {count} times")
        }
        Violation::MissingLessonDoc { id } => format!("lesson document {id} is missing"),
        Violation::StaleLessonDoc { id } => format!("lesson document {id} is stale"),
        Violation::StuckInProgress => "state.json says in_progress and there is no WAL".to_string(),
        Violation::UnpinnedCitation { id } => format!("citation {id} is not pinned"),
        Violation::LessonsUnreadable { reason } => format!("LESSONS.md is unreadable ({reason})"),
        Violation::RecurrenceMismatch {
            id,
            skill_action,
            index,
            log,
        } => format!(
            "recurrence for {id} ({skill_action}) is {index} in the index and {log} in the log"
        ),
    }
}

fn format_elapsed(secs: i64) -> String {
    if secs < 60 {
        format!("{secs} seconds")
    } else if secs < 3600 {
        format!("{} minutes", secs / 60)
    } else if secs < 86400 {
        format!("{} hours", secs / 3600)
    } else {
        format!("{} days", secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use dreamd_core::layout::AgentRoot;
    use dreamd_protocol::EventId;
    use std::fs;

    fn setup_agent_root(label: &str) -> (AgentRoot, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path().join(label);
        fs::create_dir_all(&project).unwrap();
        let root = AgentRoot::new(&project);
        for sub in [
            root.working_dir(),
            root.episodic_dir(),
            root.semantic_dir(),
            root.personal_dir(),
            root.dreamd_dir(),
        ] {
            fs::create_dir_all(sub).unwrap();
        }
        (root, dir)
    }

    fn run_with(
        cfg: &Config,
        root: &AgentRoot,
        skip: Option<&AutobiographySkip>,
        socket: Option<&Path>,
        flags: DoctorFlags,
    ) -> (bool, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let ok = run(
            cfg, root, skip, socket, None, flags, NOW_SEC, &mut out, &mut err,
        )
        .expect("run ok");
        (
            ok,
            String::from_utf8(out).expect("utf8 stdout"),
            String::from_utf8(err).expect("utf8 stderr"),
        )
    }

    fn run_default(
        cfg: &Config,
        root: &AgentRoot,
        skip: Option<&AutobiographySkip>,
    ) -> (bool, String, String) {
        run_with(cfg, root, skip, None, DoctorFlags::default())
    }

    // Fixed young timestamp (2025-07-03Z) — checks here are count-based.
    const NOW_SEC: i64 = 1751500800;

    /// Distinct valid `evt_` ids for fixtures (26-char Crockford ULID tail).
    fn evt_id(suffix: char) -> String {
        format!("evt_01ARZ3NDEKTSV4RRFFQ69G5FA{suffix}")
    }

    fn learning(suffix: char, skill_action: &str, schema_version: &str) -> AgentLearning {
        AgentLearning {
            schema_version: schema_version.to_string(),
            id: EventId::parse(&evt_id(suffix)).expect("valid EventId"),
            timestamp: DateTime::<Utc>::from_timestamp(NOW_SEC, 0).expect("valid ts"),
            pain: 5.0,
            importance: 6.0,
            pinned: false,
            skill_action: skill_action.to_string(),
            source_harness: "test-harness".to_string(),
            content: format!("entry {suffix}"),
        }
    }

    /// Seed the episodic log with `records`. There is no index, so
    /// `index_freshness` reads `no index yet` on unix, which is not a warning.
    fn seed_events(root: &AgentRoot, records: &[AgentLearning]) {
        let mut body = String::new();
        for r in records {
            body.push_str(&serde_json::to_string(r).unwrap());
            body.push('\n');
        }
        fs::write(root.episodic_jsonl(), body).unwrap();
    }

    /// BZR-1236: the line reports the set difference. An event appended below
    /// the watermark leaves the watermark check fresh; doctor still finds it.
    #[cfg(unix)]
    #[test]
    fn doctor_reports_an_index_gap_the_watermark_hides() {
        use dreamd_core::server::{assess_index_freshness, TantivyIndexHandle};
        use dreamd_core::writer_lock::{WriterKind, WriterLock};

        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("index-gap");
        seed_events(
            &root,
            &[
                learning('1', "rust::eh", "1.0.0"),
                learning('2', "rust::eh", "1.0.0"),
            ],
        );
        {
            let lock = WriterLock::acquire(&root, WriterKind::Watch).unwrap();
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    TantivyIndexHandle::open(&root, &lock, std::time::Duration::from_secs(60))
                        .unwrap()
                        .shutdown()
                        .await
                        .unwrap();
                });
        }
        let mut line = serde_json::to_string(&learning('0', "rust::eh", "1.0.0")).unwrap();
        line.push('\n');
        fs::OpenOptions::new()
            .append(true)
            .open(root.episodic_jsonl())
            .unwrap()
            .write_all(line.as_bytes())
            .unwrap();
        assert!(
            !assess_index_freshness(&root).unwrap().stale,
            "precondition: the watermark check calls this store fresh"
        );

        let (ok, output, _) = run_default(&cfg, &root, None);

        assert!(
            output.contains(
                "index_freshness: stale  [WARNING: 1 event(s) not indexed, 0 indexed \
                 event(s) not in the log, 0 duplicated; the next writer open re-covers \
                 the log, or run `dreamd doctor --repair`]\n"
            ),
            "got: {output:?}"
        );
        assert!(!ok, "an index gap must fail doctor; got: {output:?}");
    }

    #[cfg(unix)]
    #[test]
    fn doctor_reports_no_index_yet_without_a_warning() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("no-index");
        seed_events(&root, &[learning('0', "rust::eh", "1.0.0")]);

        let (ok, output, _) = run_default(&cfg, &root, None);

        assert!(
            output.contains("index_freshness: no index yet  (the next writer open builds it)\n"),
            "got: {output:?}"
        );
        assert!(ok, "no index yet is not a warning; got: {output:?}");
    }

    #[test]
    fn doctor_output_contains_dream_cycle_mode_manual() {
        let cfg = Config::default(); // default is Manual
        let (root, _dir) = setup_agent_root("manual");
        let (ok, output, _) = run_default(&cfg, &root, None);
        assert!(
            output.contains("dream_cycle_mode:"),
            "doctor output must contain 'dream_cycle_mode:'; got: {output:?}"
        );
        assert!(
            output.contains("manual"),
            "default config must report manual mode; got: {output:?}"
        );
        assert!(ok, "manual mode must return all_ok=true; got: {output:?}");
    }

    fn provenance_flags() -> DoctorFlags {
        DoctorFlags {
            provenance: true,
            ..Default::default()
        }
    }

    #[test]
    fn provenance_missing_ledger_is_ok_and_prints_verify_root() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("prov-missing");
        let ledger = root.provenance_ledger();
        let (ok, output, _) = run_with(&cfg, &root, None, None, provenance_flags());
        let expected = dreamd_core::provenance::verify(&root).unwrap().root;
        assert!(
            output.contains(&format!("provenance: ok root={expected}\n")),
            "got: {output:?}"
        );
        assert!(ok, "missing ledger must not fail doctor; got: {output:?}");
        assert!(
            !ledger.exists(),
            "doctor --provenance must not create the ledger"
        );
    }

    #[test]
    fn provenance_orphan_index_doc_warns_and_leaves_ledger_bytes() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("prov-orphan");
        let ledger = root.provenance_ledger();
        fs::create_dir_all(ledger.parent().unwrap()).unwrap();
        let id = "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let body = format!(
            "{{\"schema_version\":\"provenance/1.0\",\"kind\":\"index_doc\",\"from\":\"{id}\",\"to\":\"{id}\"}}\n"
        );
        fs::write(&ledger, &body).unwrap();

        let (ok, output, _) = run_with(&cfg, &root, None, None, provenance_flags());
        let expected = dreamd_core::provenance::verify(&root).unwrap().root;

        assert!(
            output.contains(&format!(
                "provenance: orphan line=1 kind=index_doc from={id} to={id}\n"
            )),
            "got: {output:?}"
        );
        assert!(
            output.contains(
                "provenance: 1 fault(s)  [WARNING: ledger disagrees with the live memory files]"
            ),
            "got: {output:?}"
        );
        assert!(
            output.contains(&format!("provenance: root={expected}\n")),
            "got: {output:?}"
        );
        assert!(!output.contains("provenance: ok"), "got: {output:?}");
        assert!(!ok, "an orphan edge must fail doctor; got: {output:?}");
        assert_eq!(fs::read(&ledger).unwrap(), body.as_bytes());
    }

    #[test]
    fn provenance_flag_off_prints_no_provenance_line() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("prov-off");
        let ledger = root.provenance_ledger();
        fs::create_dir_all(ledger.parent().unwrap()).unwrap();
        let id = "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV";
        fs::write(
            &ledger,
            format!(
                "{{\"schema_version\":\"provenance/1.0\",\"kind\":\"index_doc\",\"from\":\"{id}\",\"to\":\"{id}\"}}\n"
            ),
        )
        .unwrap();

        let (_, output, _) = run_default(&cfg, &root, None);
        assert!(
            !output.lines().any(|l| l.starts_with("provenance:")),
            "got: {output:?}"
        );
    }

    #[test]
    fn doctor_output_auto_mode_warning() {
        let cfg = Config {
            dream_cycle_mode: DreamCycleMode::Auto,
            ..Default::default()
        };
        let (root, _dir) = setup_agent_root("auto");
        let (ok, output, _) = run_default(&cfg, &root, None);
        assert!(
            output.contains("dream_cycle_mode:"),
            "doctor output must contain 'dream_cycle_mode:'; got: {output:?}"
        );
        assert!(
            output.contains("WARNING"),
            "auto mode must emit WARNING; got: {output:?}"
        );
        assert!(!ok, "auto mode must return all_ok=false");
    }

    #[test]
    fn doctor_skip_some_renders_line() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("skip-some");
        let skip = AutobiographySkip {
            at: NOW_SEC - 300, // 5 minutes before the injected clock
            reason: "user_dirty_tree".to_string(),
            files: vec![
                ".agent/semantic/LESSONS.md".to_string(),
                ".agent/episodic/AGENT_LEARNINGS.jsonl".to_string(),
            ],
        };
        let (ok, output, _) = run_default(&cfg, &root, Some(&skip));
        assert!(
            output.contains("last autobiography skip:"),
            "skip line must be present; got: {output:?}"
        );
        assert!(
            output.contains("user_dirty_tree"),
            "reason must appear; got: {output:?}"
        );
        assert!(
            output.contains("2 files"),
            "file count must appear; got: {output:?}"
        );
        assert!(ok, "skip line must not change all_ok");
    }

    #[test]
    fn doctor_skip_none_no_skip_line() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("skip-none");
        let (_ok, output, _) = run_default(&cfg, &root, None);
        assert!(
            !output.contains("last autobiography skip"),
            "no skip line when skip is None; got: {output:?}"
        );
    }

    #[test]
    fn doctor_episodic_log_ok_when_clean() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("episodic-clean");
        let jsonl = root.episodic_jsonl();
        fs::write(&jsonl, b"").unwrap();
        let (ok, output, _) = run_default(&cfg, &root, None);
        assert!(
            output.contains("episodic_log: ok"),
            "clean episodic log must report ok; got: {output:?}"
        );
        assert!(ok, "clean episodic log must return all_ok=true");
    }

    #[test]
    fn doctor_episodic_log_warns_on_malformed_lines() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("episodic-bad");
        let jsonl = root.episodic_jsonl();
        fs::write(&jsonl, b"{not valid json}\n").unwrap();
        let (ok, output, _) = run_default(&cfg, &root, None);
        assert!(
            output.contains("episodic_log:") && output.contains("WARNING"),
            "malformed lines must emit WARNING; got: {output:?}"
        );
        assert!(
            output.contains("1 malformed line"),
            "must report malformed line count; got: {output:?}"
        );
        assert!(!ok, "malformed episodic log must return all_ok=false");
    }

    #[test]
    fn doctor_reports_first_malformed_line_numbers() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("episodic-lines");
        // Lines 1 and 3 malformed, line 2 valid.
        let mut body = String::from("{not valid json}\n");
        body.push_str(
            &serde_json::to_string(&learning('A', "rust::x", RECORD_SCHEMA_VERSION)).unwrap(),
        );
        body.push('\n');
        body.push_str("{also not valid}\n");
        fs::write(root.episodic_jsonl(), body).unwrap();
        let (ok, output, _) = run_default(&cfg, &root, None);
        assert!(
            output.contains("2 malformed line(s) (first at line(s) 1, 3)"),
            "must surface first-N malformed line numbers; got: {output:?}"
        );
        assert!(!ok);
    }

    // AILAB-700 — lessons the semantic pass could not index. Note these are
    // not the `AutobiographySkip` lines above: that skip is the dirty-tree git
    // commit skip and has nothing to do with the semantic layer.

    /// Write `.dreamd/semantic_pass.json` by hand — the writer lives in
    /// `dreamd-core` and is private to the pass, so the CLI side asserts
    /// against the on-disk contract, which is the thing that has to hold.
    fn seed_semantic_pass(root: &AgentRoot, json: &str) {
        fs::write(root.dreamd_dir().join("semantic_pass.json"), json).unwrap();
    }

    #[test]
    fn doctor_semantic_lessons_warns_when_skipped() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("sem-skipped");
        seed_semantic_pass(
            &root,
            r#"{"skipped_lesson_ids":["evt_01ARZ3NDEKTSV4RRFFQ69G5FZZ",
                "evt_01ARZ3NDEKTSV4RRFFQ69G5FZY"],
                "cluster_key":"rust::eh","indexed":1}"#,
        );
        let (ok, output, _) = run_default(&cfg, &root, None);
        assert!(
            output.contains("semantic_lessons: 2 not indexed"),
            "the count must lead the line; got: {output:?}"
        );
        assert!(
            output.contains("evt_01ARZ3NDEKTSV4RRFFQ69G5FZZ") && output.contains("and 1 more"),
            "one id must be named outright, the rest counted; got: {output:?}"
        );
        assert!(
            output.contains("rust::eh") && output.contains("WARNING"),
            "the cluster and the WARNING marker must appear; got: {output:?}"
        );
        assert!(
            !ok,
            "un-indexable lessons are silently missing from recall, so doctor \
             must exit non-zero; got: {output:?}"
        );
    }

    #[test]
    fn doctor_semantic_lessons_silent_when_empty() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("sem-empty");
        seed_semantic_pass(
            &root,
            r#"{"skipped_lesson_ids":[],"cluster_key":"rust::eh","indexed":2}"#,
        );
        let (ok, output, _) = run_default(&cfg, &root, None);
        assert!(
            !output.contains("semantic_lessons"),
            "a clean pass prints nothing at all — no 'ok' line; got: {output:?}"
        );
        assert!(ok, "a clean pass must not fail doctor; got: {output:?}");
    }

    #[test]
    fn doctor_semantic_lessons_warns_when_unreadable() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("sem-unreadable");
        seed_semantic_pass(&root, "\u{0}not json at all\u{1}");
        let (ok, output, _) = run_default(&cfg, &root, None);
        assert!(
            output.contains("semantic_lessons: unreadable") && output.contains("WARNING"),
            "an unparseable sidecar must report as unreadable under WARNING; \
             got: {output:?}"
        );
        assert!(
            !ok,
            "doctor cannot tell an unreadable report from a clean one, so it \
             must not claim health; got: {output:?}"
        );
    }

    #[test]
    fn doctor_semantic_lessons_silent_when_absent() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("sem-absent");
        let (ok, output, _) = run_default(&cfg, &root, None);
        assert!(
            !output.contains("semantic_lessons"),
            "a store that never dreamed has no report and is not unhealthy; \
             got: {output:?}"
        );
        assert!(ok, "absent report must not fail doctor; got: {output:?}");
    }

    #[test]
    fn doctor_warns_on_missing_scaffolded_subdir() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("missing-subdir");
        fs::remove_dir_all(root.personal_dir()).unwrap();
        let (ok, output, _) = run_default(&cfg, &root, None);
        assert!(
            output.contains("missing_subdir: personal/"),
            "missing personal/ must be reported; got: {output:?}"
        );
        assert!(output.contains("WARNING"), "must WARN; got: {output:?}");
        assert!(!ok, "missing scaffolded subdir must return all_ok=false");
    }

    #[test]
    fn doctor_ignores_missing_skills_and_protocols_dirs() {
        // init does not scaffold skills/ or protocols/ — their absence on an
        // otherwise-healthy store must not fail doctor or appear as a warning.
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("optional-subdirs");
        let (ok, output, _) = run_default(&cfg, &root, None);
        assert!(
            ok,
            "store without skills//protocols/ must be ok; got: {output:?}"
        );
        assert!(
            !output.contains("skills") && !output.contains("protocols"),
            "skills//protocols/ must not be flagged; got: {output:?}"
        );
    }

    #[test]
    fn doctor_schema_record_ok_and_index_absent() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("schema-ok");
        seed_events(&root, &[learning('A', "rust::x", RECORD_SCHEMA_VERSION)]);
        let (ok, output, _) = run_default(&cfg, &root, None);
        assert!(
            output.contains(&format!(
                "schema_record: ok (1 record(s) at {RECORD_SCHEMA_VERSION}"
            )),
            "records at the current schema must report ok; got: {output:?}"
        );
        assert!(
            output.contains("schema_index: absent"),
            "missing manifest must report absent, not warn; got: {output:?}"
        );
        assert!(ok, "absent index manifest is benign; got: {output:?}");
    }

    #[test]
    fn doctor_schema_record_mismatch_warns() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("schema-mismatch");
        seed_events(
            &root,
            &[
                learning('A', "rust::x", "0.9.0"),
                learning('B', "rust::x", RECORD_SCHEMA_VERSION),
            ],
        );
        let (ok, output, _) = run_default(&cfg, &root, None);
        assert!(
            output.contains("schema_record: mismatch (0.9.0)"),
            "foreign record schema must be reported; got: {output:?}"
        );
        assert!(!ok, "record schema mismatch must return all_ok=false");
    }

    #[test]
    fn doctor_schema_index_older_warns_and_newer_errors() {
        let cfg = Config::default();

        let (root, _dir) = setup_agent_root("index-old");
        fs::write(
            root.dreamd_dir().join(INDEX_MANIFEST_FILENAME),
            r#"{"schema_version":"index/1.0"}"#,
        )
        .unwrap();
        let (ok, output, _) = run_default(&cfg, &root, None);
        assert!(
            output.contains("older than binary") && output.contains("WARNING"),
            "older manifest must WARN; got: {output:?}"
        );
        assert!(!ok);

        let (root, _dir) = setup_agent_root("index-new");
        fs::write(
            root.dreamd_dir().join(INDEX_MANIFEST_FILENAME),
            r#"{"schema_version":"index/999.0"}"#,
        )
        .unwrap();
        let (ok, output, _) = run_default(&cfg, &root, None);
        assert!(
            output.contains("newer than binary") && output.contains("ERROR"),
            "newer manifest must ERROR; got: {output:?}"
        );
        assert!(!ok);
    }

    #[cfg(unix)]
    #[test]
    fn doctor_flags_orphaned_uds_socket() {
        let cfg = Config::default();
        let (root, dir) = setup_agent_root("orphan-uds");
        let sock = dir.path().join("dreamd.sock");
        // Bind then drop: std does not unlink the socket file, leaving an
        // orphan with nothing listening.
        drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
        assert!(sock.exists(), "orphan socket file must persist");
        let (ok, output, _) = run_with(&cfg, &root, None, Some(&sock), DoctorFlags::default());
        assert!(
            output.contains("orphaned_uds:") && output.contains("WARNING"),
            "orphan socket must WARN; got: {output:?}"
        );
        assert!(!ok, "orphan socket must return all_ok=false");
    }

    #[cfg(unix)]
    #[test]
    fn doctor_live_socket_is_not_orphaned() {
        let cfg = Config::default();
        let (root, dir) = setup_agent_root("live-uds");
        let sock = dir.path().join("dreamd.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let (ok, output, _) = run_with(&cfg, &root, None, Some(&sock), DoctorFlags::default());
        assert!(
            output.contains("orphaned_uds: none (daemon live"),
            "live socket must not be flagged; got: {output:?}"
        );
        assert!(ok, "live daemon socket is healthy; got: {output:?}");
    }

    #[cfg(unix)]
    #[test]
    fn doctor_reports_tantivy_lock_presence_without_failing() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("lock-info");
        let index_dir = root.dreamd_dir().join("index");
        fs::create_dir_all(&index_dir).unwrap();
        fs::write(index_dir.join(".tantivy-writer.lock"), b"").unwrap();
        let (ok, output, _) = run_default(&cfg, &root, None);
        assert!(
            output.contains("tantivy_lock: .tantivy-writer.lock present"),
            "lock presence must be reported; got: {output:?}"
        );
        assert!(
            output.contains("INFO"),
            "lock presence is informational; got: {output:?}"
        );
        assert!(ok, "lock presence alone must not fail doctor");
        assert!(
            index_dir.join(".tantivy-writer.lock").exists(),
            "doctor must leave the lock file in place"
        );
    }

    #[test]
    fn cluster_health_absent_sidecar_is_benign() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("ch-no-sidecar");
        let flags = DoctorFlags {
            cluster_health: true,
            ..Default::default()
        };
        let (ok, output, _) = run_with(&cfg, &root, None, None, flags);
        assert!(
            output.contains("cluster_health: no sidecar"),
            "absent sidecar must print the benign INFO line; got: {output:?}"
        );
        assert!(
            ok,
            "absent sidecar on a never-dreamed store is not an error"
        );
    }

    #[test]
    fn cluster_health_flags_drift() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("ch-drift");
        seed_events(
            &root,
            &[
                learning('A', "rust::borrow", RECORD_SCHEMA_VERSION),
                learning('B', "rust::borrow", RECORD_SCHEMA_VERSION),
                learning('C', "rust::borrow", RECORD_SCHEMA_VERSION),
            ],
        );
        fs::write(
            root.semantic_dir().join("recurrence_counts.json"),
            r#"{"schema_version":"1.0","clusters":[
                {"skill_action":"rust::borrow","count":2},
                {"skill_action":"python","count":1}]}"#,
        )
        .unwrap();
        let flags = DoctorFlags {
            cluster_health: true,
            ..Default::default()
        };
        let (ok, output, _) = run_with(&cfg, &root, None, None, flags);
        assert!(
            output.contains("cluster_health: rust::borrow jsonl=3 sidecar=2  [WARNING: drift]"),
            "count mismatch must be flagged; got: {output:?}"
        );
        assert!(
            output.contains("cluster_health: python jsonl=(none) sidecar=1  [WARNING: drift]"),
            "sidecar-only key must be flagged; got: {output:?}"
        );
        // `rust` is a parent of the promoted `rust::borrow` cluster. Deepest-wins
        // means the engine never writes a `rust` row, so doctor must not derive
        // one either (AILAB-223).
        assert!(
            !output.contains("cluster_health: rust jsonl="),
            "unpromoted parent prefix must not be derived; got: {output:?}"
        );
        assert!(
            output.contains("2 drifted key(s)"),
            "drift summary must count all drifted keys; got: {output:?}"
        );
        assert!(!ok, "cluster drift must return all_ok=false");
    }

    #[test]
    fn cluster_health_matching_sidecar_is_ok() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("ch-match");
        seed_events(
            &root,
            &[
                learning('A', "rust::borrow", RECORD_SCHEMA_VERSION),
                learning('B', "rust::borrow", RECORD_SCHEMA_VERSION),
                learning('C', "rust::borrow", RECORD_SCHEMA_VERSION),
            ],
        );
        // Exactly what `run_cluster_engine` writes for three `rust::borrow`
        // events: one deepest-wins cluster, and no parent `rust` row.
        fs::write(
            root.semantic_dir().join("recurrence_counts.json"),
            r#"{"schema_version":"1.0","clusters":[
                {"skill_action":"rust::borrow","count":3}]}"#,
        )
        .unwrap();
        let flags = DoctorFlags {
            cluster_health: true,
            ..Default::default()
        };
        let (ok, output, _) = run_with(&cfg, &root, None, None, flags);
        assert!(
            output.contains("cluster_health: ok (1 prefix(es) match the sidecar)"),
            "matching sidecar must report ok; got: {output:?}"
        );
        assert!(
            ok,
            "matching sidecar must keep all_ok=true; got: {output:?}"
        );
    }

    /// Let the real cluster engine write the sidecar, then audit it: a store
    /// that was just dreamed reports no drift. Deriving raw prefix-tree counts
    /// instead would report `rust jsonl=3 sidecar=(none) [WARNING: drift]` here.
    #[test]
    fn cluster_health_matches_a_sidecar_written_by_the_real_engine() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("ch-engine");
        seed_events(
            &root,
            &[
                learning('A', "rust::borrow", RECORD_SCHEMA_VERSION),
                learning('B', "rust::borrow", RECORD_SCHEMA_VERSION),
                learning('C', "rust::borrow", RECORD_SCHEMA_VERSION),
            ],
        );
        consolidation::run_cluster_engine(&root, NOW_SEC).expect("cluster engine runs");

        let flags = DoctorFlags {
            cluster_health: true,
            ..Default::default()
        };
        let (ok, output, _) = run_with(&cfg, &root, None, None, flags);
        assert!(
            output.contains("cluster_health: ok"),
            "a freshly-dreamed store must audit clean; got: {output:?}"
        );
        assert!(
            !output.contains("drift"),
            "no drift may be reported right after a dream cycle; got: {output:?}"
        );
        assert!(ok, "freshly-dreamed store must keep all_ok=true");
    }

    /// Appending past the last dream cycle is the drift the flag exists to
    /// catch: the sidecar still holds the pre-append count.
    #[test]
    fn cluster_health_flags_jsonl_that_moved_past_the_sidecar() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("ch-stale");
        let mut records = vec![
            learning('A', "rust::borrow", RECORD_SCHEMA_VERSION),
            learning('B', "rust::borrow", RECORD_SCHEMA_VERSION),
            learning('C', "rust::borrow", RECORD_SCHEMA_VERSION),
        ];
        seed_events(&root, &records);
        consolidation::run_cluster_engine(&root, NOW_SEC).expect("cluster engine runs");

        // Two more events land after the cycle; nobody re-dreamed.
        records.push(learning('D', "rust::borrow", RECORD_SCHEMA_VERSION));
        records.push(learning('E', "rust::borrow", RECORD_SCHEMA_VERSION));
        seed_events(&root, &records);

        let flags = DoctorFlags {
            cluster_health: true,
            ..Default::default()
        };
        let (ok, output, _) = run_with(&cfg, &root, None, None, flags);
        assert!(
            output.contains("cluster_health: rust::borrow jsonl=5 sidecar=3  [WARNING: drift]"),
            "a stale sidecar must be flagged; got: {output:?}"
        );
        assert!(!ok, "genuine drift must return all_ok=false");
    }

    /// AILAB-196: the cost report rides along with `--cluster-health`
    /// and must stay read-only. The fixture is seeded with `seed_events` alone
    /// rather than `run_cluster_engine`, unlike the drift tests above, so that
    /// the three absence assertions actually mean something: any write found
    /// afterwards came from the code under test.
    #[test]
    fn cluster_health_reports_next_cycle_cost_without_writing() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("ch-cost");
        // PROMOTION_THRESHOLD is 3, so this store promotes and the reporter
        // takes the `Some(selected)` arm that actually builds a prompt.
        seed_events(
            &root,
            &[
                learning('A', "rust::borrow", RECORD_SCHEMA_VERSION),
                learning('B', "rust::borrow", RECORD_SCHEMA_VERSION),
                learning('C', "rust::borrow", RECORD_SCHEMA_VERSION),
            ],
        );
        let flags = DoctorFlags {
            cluster_health: true,
            ..Default::default()
        };
        let (ok, output, _) = run_with(&cfg, &root, None, None, flags);
        assert!(
            output.contains("next_cycle_est_usd="),
            "a promoting store must be priced; got: {output:?}"
        );
        assert!(
            !root.lessons_md().exists(),
            "doctor must never stage or commit a cycle's plan, which can \
             unlink LESSONS.md; got: {output:?}"
        );
        assert!(
            !root.semantic_dir().join("recurrence_counts.json").exists(),
            "doctor must never take run_cluster_engine's write path, which \
             writes the recurrence sidecar; got: {output:?}"
        );
        assert!(
            !root.wal_path().exists(),
            "pricing must not open a dream-cycle WAL; got: {output:?}"
        );
        assert!(
            ok,
            "the cost line is informational: it must not flip the exit code, \
             which here is owned by the benign absent-sidecar arm; got: {output:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn repair_wipes_and_rebuilds_index_from_jsonl() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("repair-rebuild");
        seed_events(
            &root,
            &[
                learning('A', "rust::x", RECORD_SCHEMA_VERSION),
                learning('B', "rust::x", RECORD_SCHEMA_VERSION),
            ],
        );
        let jsonl_before = fs::read(root.episodic_jsonl()).unwrap();
        let index_dir = root.dreamd_dir().join("index");
        fs::create_dir_all(&index_dir).unwrap();
        fs::write(index_dir.join("junk.seg"), b"junk").unwrap();

        let flags = DoctorFlags {
            repair: true,
            ..Default::default()
        };
        let (ok, output, err) = run_with(&cfg, &root, None, None, flags);
        assert!(
            ok,
            "repair on a healthy store must succeed; stderr: {err:?}"
        );
        assert!(
            output.contains("repair:") && output.contains("rebuilt index from the episodic log"),
            "repair summary must report the rebuild; got: {output:?}"
        );
        assert!(
            !index_dir.join("junk.seg").exists(),
            "stale index contents must be wiped"
        );
        assert!(
            index_dir.join("meta.json").exists(),
            "a fresh Tantivy index must exist after rebuild"
        );
        // `meta.json` alone proves only that an index was created — it exists on
        // an empty one too. The rewritten watermark is what proves the replay
        // actually ingested the seeded records and committed.
        let progress =
            fs::read_to_string(root.dreamd_dir().join("index_progress.json")).expect("watermark");
        assert!(
            progress.contains(&evt_id('B')),
            "replay must commit through the last seeded record; got: {progress:?}"
        );
        let manifest = fs::read_to_string(root.dreamd_dir().join(INDEX_MANIFEST_FILENAME)).unwrap();
        assert!(
            manifest.contains(INDEX_SCHEMA_VERSION),
            "manifest must be rewritten at the binary schema; got: {manifest:?}"
        );
        assert_eq!(
            fs::read(root.episodic_jsonl()).unwrap(),
            jsonl_before,
            "repair must leave the episodic log byte-identical"
        );
    }

    /// Pins the documented lock-file contract: `--repair` wipes the whole index
    /// dir, so flock files inside it go as collateral, but doctor never singles
    /// them out (WEG-24-A / AILAB-402 forbid targeted unlinking, not the wipe).
    #[cfg(unix)]
    #[test]
    fn repair_wipes_index_dir_including_lock_files_as_collateral() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("repair-locks");
        seed_events(&root, &[learning('A', "rust::x", RECORD_SCHEMA_VERSION)]);
        let index_dir = root.dreamd_dir().join("index");
        fs::create_dir_all(&index_dir).unwrap();
        fs::write(index_dir.join(".tantivy-writer.lock"), b"").unwrap();

        let flags = DoctorFlags {
            repair: true,
            ..Default::default()
        };
        let (ok, output, err) = run_with(&cfg, &root, None, None, flags);
        assert!(ok, "repair must succeed; stderr: {err:?}");
        assert!(
            output.contains("removed index dir"),
            "the wipe must be reported; got: {output:?}"
        );
        // Whatever Tantivy recreates on open is its business; what matters is
        // that no step targeted the lock file by name.
        assert!(
            index_dir.join("meta.json").exists(),
            "index must be rebuilt after the wipe"
        );
    }

    #[cfg(unix)]
    #[test]
    fn repair_refuses_on_live_daemon_socket() {
        let cfg = Config::default();
        let (root, dir) = setup_agent_root("repair-live");
        let sock = dir.path().join("dreamd.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let index_dir = root.dreamd_dir().join("index");
        fs::create_dir_all(&index_dir).unwrap();
        fs::write(index_dir.join("sentinel"), b"x").unwrap();

        let flags = DoctorFlags {
            repair: true,
            ..Default::default()
        };
        let (ok, _output, err) = run_with(&cfg, &root, None, Some(&sock), flags);
        assert!(!ok, "repair must fail closed on a live daemon");
        assert!(
            err.contains("Stop `dreamd watch` / `dreamd mcp` first"),
            "stderr must tell the user to stop the daemon; got: {err:?}"
        );
        assert!(
            index_dir.join("sentinel").exists(),
            "no index wipe may happen before the refusal"
        );
        assert!(sock.exists(), "a live socket must never be unlinked");
    }

    /// The wipe must never run under another writer. While `dreamd watch`
    /// holds the project, `--repair` refuses even with no socket to probe.
    #[cfg(unix)]
    #[test]
    fn repair_refuses_while_watch_holds_the_project() {
        use dreamd_core::server::{TantivyIndexHandle, DEFAULT_COMMIT_CADENCE};
        use dreamd_core::writer_lock::{WriterKind, WriterLock};

        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("repair-held");
        seed_events(&root, &[learning('A', "rust::x", RECORD_SCHEMA_VERSION)]);
        {
            let lock = WriterLock::acquire(&root, WriterKind::Watch).unwrap();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                TantivyIndexHandle::open(&root, &lock, DEFAULT_COMMIT_CADENCE)
                    .unwrap()
                    .shutdown()
                    .await
                    .unwrap();
            });
        }
        let before = crate::commands::store_bytes(&root);

        let _watch = WriterLock::acquire(&root, WriterKind::Watch).unwrap();
        let flags = DoctorFlags {
            repair: true,
            ..Default::default()
        };
        let (ok, _output, err) = run_with(&cfg, &root, None, None, flags);

        assert!(
            !ok,
            "repair must fail closed while another writer holds the project"
        );
        assert!(
            err.contains(&format!(
                "dreamd: error — doctor --repair refused: project writer lock is held by pid {} \
                 (watch). Stop it first, then re-run.",
                std::process::id()
            )),
            "{err:?}"
        );
        assert_eq!(crate::commands::store_bytes(&root), before);
    }

    /// `$DREAMD_SOCK` pointed at a regular file: the liveness probe reports
    /// "dead" for it too, so only the socket-type guard stops `--repair` from
    /// deleting an operator's file.
    #[cfg(unix)]
    #[test]
    fn repair_refuses_to_unlink_a_non_socket() {
        let cfg = Config::default();
        let (root, dir) = setup_agent_root("repair-notsock");
        let not_a_sock = dir.path().join("important.txt");
        fs::write(&not_a_sock, b"precious").unwrap();

        let flags = DoctorFlags {
            repair: true,
            ..Default::default()
        };
        let (ok, _output, err) = run_with(&cfg, &root, None, Some(&not_a_sock), flags);
        assert!(!ok, "repair must fail closed on a non-socket path");
        assert!(
            err.contains("is not a unix socket"),
            "stderr must explain the refusal; got: {err:?}"
        );
        assert_eq!(
            fs::read(&not_a_sock).unwrap(),
            b"precious",
            "the file must be left untouched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn repair_unlinks_orphaned_socket() {
        let cfg = Config::default();
        let (root, dir) = setup_agent_root("repair-orphan");
        let sock = dir.path().join("dreamd.sock");
        drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
        assert!(sock.exists());

        let flags = DoctorFlags {
            repair: true,
            ..Default::default()
        };
        let (ok, output, _) = run_with(&cfg, &root, None, Some(&sock), flags);
        assert!(
            output.contains("unlinked orphaned socket"),
            "repair summary must report the unlink; got: {output:?}"
        );
        assert!(!sock.exists(), "orphan socket file must be removed");
        // The pre-repair orphaned_uds check already WARNed, so the run still
        // reports exit-1 semantics even though the repair itself succeeded.
        assert!(!ok);
    }

    fn corrupt_sidecars(root: &AgentRoot) -> Vec<std::path::PathBuf> {
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

    fn plant_wal(root: &AgentRoot, body: &str) {
        fs::create_dir_all(root.stage_dir().join("dream-1")).unwrap();
        fs::write(root.wal_path(), body).unwrap();
    }

    fn assert_quarantined(root: &AgentRoot, body: &str) -> std::path::PathBuf {
        assert!(!root.wal_path().exists(), "the WAL path must be gone");
        assert!(!root.stage_dir().exists(), "the stage dir must be wiped");
        let sidecars = corrupt_sidecars(root);
        assert_eq!(sidecars.len(), 1, "exactly one sidecar: {sidecars:?}");
        assert_eq!(fs::read(&sidecars[0]).unwrap(), body.as_bytes());
        let state = dreamd_core::daemon_state::read_daemon_state(root).unwrap();
        assert_eq!(state.last_dream_cycle_status, "failed");
        sidecars.into_iter().next().unwrap()
    }

    fn assert_next_cycle_runs(root: &AgentRoot) {
        use dreamd_core::dream_cycle::{cycle_date_from_now_sec, run_filesystem_phases};
        use dreamd_core::writer_lock::{WriterKind, WriterLock};

        let lock = WriterLock::acquire(root, WriterKind::Dream).expect("next writer");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(run_filesystem_phases(
            root,
            lock.ledger(),
            NOW_SEC,
            &cycle_date_from_now_sec(NOW_SEC),
            true,
            false,
        ))
        .expect("the next cycle runs");
        assert!(!root.wal_path().exists(), "a finished cycle leaves no WAL");
    }

    #[cfg(unix)]
    #[test]
    fn repair_quarantines_unknown_format_wal_and_the_next_cycle_runs() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("wal-unknown");
        seed_events(&root, &[learning('A', "rust::x", RECORD_SCHEMA_VERSION)]);
        let body = r#"{"intent":"faked"}"#;
        plant_wal(&root, body);

        let (ok, output, _) = run_default(&cfg, &root, None);
        assert!(!ok, "an unreadable WAL fails doctor");
        let path = root.wal_path().display().to_string();
        assert!(
            output.contains(&format!(
                "dream_cycle_wal: unknown format \"none\"  [ERROR: {path}; run `dreamd doctor --repair`]"
            )),
            "{output}"
        );
        assert_eq!(fs::read_to_string(root.wal_path()).unwrap(), body);
        assert!(root.stage_dir().join("dream-1").is_dir());

        let flags = DoctorFlags {
            repair: true,
            ..Default::default()
        };
        let (ok, output, err) = run_with(&cfg, &root, None, None, flags);
        assert!(ok, "repair succeeds; stderr: {err}; stdout: {output}");
        assert!(
            output.contains("quarantined dream-cycle WAL to"),
            "{output}"
        );
        assert!(output.contains("store invariants: ok"), "{output}");
        assert_quarantined(&root, body);
        assert_next_cycle_runs(&root);
    }

    #[cfg(unix)]
    #[test]
    fn repair_quarantines_bad_redo_path_and_the_next_cycle_runs() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("wal-bad-path");
        seed_events(&root, &[learning('A', "rust::x", RECORD_SCHEMA_VERSION)]);
        let live = root.project_root().join("live.md");
        fs::write(&live, b"live\n").unwrap();
        let body = format!(
            r#"{{"schema_version":"1.0","format":"redo/1","renames":[{{"from":".dreamd/stage/dream-1/0-LESSONS.md","to":"{}"}}],"unlinks":[],"edges":[]}}"#,
            live.display()
        );
        plant_wal(&root, &body);

        let (ok, output, _) = run_default(&cfg, &root, None);
        assert!(!ok);
        assert!(
            output.contains(&format!("dream_cycle_wal: bad path \"{}\"", live.display())),
            "{output}"
        );
        assert!(output.contains("run `dreamd doctor --repair`"), "{output}");
        assert_eq!(fs::read(&live).unwrap(), b"live\n");

        let flags = DoctorFlags {
            repair: true,
            ..Default::default()
        };
        let (ok, output, err) = run_with(&cfg, &root, None, None, flags);
        assert!(ok, "repair succeeds; stderr: {err}; stdout: {output}");
        assert_eq!(fs::read(&live).unwrap(), b"live\n");
        assert_quarantined(&root, &body);
        assert_next_cycle_runs(&root);
    }

    #[cfg(unix)]
    #[test]
    fn repair_quarantines_truncated_wal_and_the_next_cycle_runs() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("wal-truncated");
        seed_events(&root, &[learning('A', "rust::x", RECORD_SCHEMA_VERSION)]);
        let body = r#"{"format":"redo/1""#;
        plant_wal(&root, body);

        let (ok, output, _) = run_default(&cfg, &root, None);
        assert!(!ok);
        assert!(
            output.contains("dream_cycle_wal: not valid JSON  [ERROR:"),
            "{output}"
        );
        assert!(output.contains("run `dreamd doctor --repair`"), "{output}");
        assert!(
            !output.contains("EOF"),
            "serde's message stays off the line: {output}"
        );

        let flags = DoctorFlags {
            repair: true,
            ..Default::default()
        };
        let (ok, output, err) = run_with(&cfg, &root, None, None, flags);
        assert!(ok, "repair succeeds; stderr: {err}; stdout: {output}");
        assert_quarantined(&root, body);
        assert_next_cycle_runs(&root);
    }

    #[cfg(unix)]
    #[test]
    fn repair_refuses_unreadable_wal_while_watch_holds_the_lock() {
        use dreamd_core::writer_lock::{WriterKind, WriterLock};

        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("wal-held");
        seed_events(&root, &[learning('A', "rust::x", RECORD_SCHEMA_VERSION)]);
        let body = r#"{"intent":"faked"}"#;
        // Watch itself refuses this WAL, so the holder has to be taken first.
        // The file is planted while that holder is alive: repair then sees a
        // busy lock and must not move it.
        let _watch = WriterLock::acquire(&root, WriterKind::Watch).unwrap();
        plant_wal(&root, body);

        let flags = DoctorFlags {
            repair: true,
            ..Default::default()
        };
        let (ok, _output, err) = run_with(&cfg, &root, None, None, flags);
        assert!(!ok);
        assert!(err.contains("project writer lock is held"), "{err}");
        assert_eq!(fs::read_to_string(root.wal_path()).unwrap(), body);
        assert!(corrupt_sidecars(&root).is_empty());
    }

    /// A log that already disagrees with itself stays that way after the WAL
    /// is moved aside. The report names the violation and the sidecar.
    #[cfg(unix)]
    #[test]
    fn repair_reports_store_violations_against_the_quarantined_wal() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("wal-violations");
        let record = learning('A', "rust::x", RECORD_SCHEMA_VERSION);
        let line = serde_json::to_string(&record).unwrap();
        fs::write(root.episodic_jsonl(), format!("{line}\n{line}\n")).unwrap();
        let body = r#"{"intent":"faked"}"#;
        plant_wal(&root, body);

        let flags = DoctorFlags {
            repair: true,
            ..Default::default()
        };
        let (ok, output, err) = run_with(&cfg, &root, None, None, flags);
        assert!(!ok, "violations fail the run; stderr: {err}");
        let sidecar = assert_quarantined(&root, body);
        assert!(
            output.contains(&format!(
                "store invariants: 1 violation(s): duplicate event id {} appears 2 times. \
                 A WAL corrupted after its commit point can leave a half-applied cycle. \
                 The quarantined WAL is {}.",
                record.id,
                sidecar.display()
            )),
            "{output}"
        );
    }

    // ---- BZR-1302: hand-edit anomalies and the registry ----

    const SCORE_RANGE_HINT: &str = "[WARNING: pain or importance outside 0.0..=10.0 in \
                                    AGENT_LEARNINGS.jsonl; recall scores these lines at the \
                                    nearest bound]";
    const FUTURE_TIMESTAMP_HINT: &str = "[WARNING: timestamp more than 24 h ahead of the \
                                         clock in AGENT_LEARNINGS.jsonl; recall scores these \
                                         lines as new until the clock reaches them]";

    fn duplicate_id_hint(ids: usize) -> String {
        format!(
            "[WARNING: {ids} event id(s) on more than one line of AGENT_LEARNINGS.jsonl; \
             the index keeps one document per id]"
        )
    }

    fn registry_parse_line(path: &Path, line: usize) -> String {
        format!(
            "registry: parse error at line {line}  [WARNING: {} does not parse; `dreamd \
             status` reports every project as not registered and the daemon answers 500 \
             until it parses]",
            path.display()
        )
    }

    /// One log line written as raw JSON, so values such as `1e40` and
    /// `10.0000001` reach the file as written.
    fn raw_record(suffix: char, pain: &str, importance: &str, timestamp: &str) -> String {
        format!(
            "{{\"schema_version\":\"1.0.0\",\"id\":\"{}\",\"timestamp\":\"{timestamp}\",\
             \"pain\":{pain},\"importance\":{importance},\"pinned\":false,\
             \"skill_action\":\"rust::eh\",\"source_harness\":\"test-harness\",\
             \"content\":\"entry {suffix}\"}}",
            evt_id(suffix)
        )
    }

    /// `NOW_SEC + offset` as RFC 3339 UTC.
    fn at(offset: i64) -> String {
        DateTime::<Utc>::from_timestamp(NOW_SEC + offset, 0)
            .expect("valid ts")
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    /// A record with in-range scores, written at `NOW_SEC`.
    fn clean_record(suffix: char) -> String {
        raw_record(suffix, "5.0", "6.0", &at(0))
    }

    /// Write `lines` as the episodic log, each ended by `\n`.
    fn write_log(root: &AgentRoot, lines: &[String]) {
        let mut body = String::new();
        for line in lines {
            body.push_str(line);
            body.push('\n');
        }
        fs::write(root.episodic_jsonl(), body).unwrap();
    }

    fn run_with_registry(
        cfg: &Config,
        root: &AgentRoot,
        registry: Option<&Path>,
    ) -> (bool, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let ok = run(
            cfg,
            root,
            None,
            None,
            registry,
            DoctorFlags::default(),
            NOW_SEC,
            &mut out,
            &mut err,
        )
        .expect("run ok");
        (ok, String::from_utf8(out).expect("utf8 stdout"))
    }

    /// Full-line equality, not a substring match.
    fn assert_has_line(output: &str, expected: &str) {
        assert!(
            output.lines().any(|line| line == expected),
            "expected the line {expected:?}\ngot:\n{output}"
        );
    }

    /// `expected` is the only line of `output` that carries a WARNING.
    fn assert_only_warning(output: &str, expected: &str) {
        let warnings: Vec<&str> = output
            .lines()
            .filter(|line| line.contains("WARNING"))
            .collect();
        assert_eq!(warnings, vec![expected], "got:\n{output}");
    }

    /// `s` as a TOML basic string (a Windows path holds backslashes).
    fn toml_string(s: &str) -> String {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }

    /// The test 5 registry: one valid entry, then garbage on line 3.
    fn garbage_registry(root: &AgentRoot) -> String {
        let canonical = fs::canonicalize(root.project_root()).unwrap();
        format!(
            "[[projects]]\nroot = {}\nthis is garbage\n",
            toml_string(&canonical.to_string_lossy())
        )
    }

    /// BZR-1302 test 1a.
    #[test]
    fn score_range_lists_every_line_the_validators_refuse() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("score-range");
        write_log(
            &root,
            &[
                "not json".to_string(),
                raw_record('0', "5.0", "6.0", &at(0)),
                raw_record('1', "50", "6.0", &at(0)),
                raw_record('2', "5.0", "-1", &at(0)),
                raw_record('3', "1e40", "6.0", &at(0)),
                raw_record('4', "11", "11", &at(0)),
                raw_record('5', "5.0", "10.000001", &at(0)),
                raw_record('6', "10.0000001", "6.0", &at(0)),
                raw_record('7', "10.0", "0.0", &at(0)),
                raw_record('8', "-0.5", "6.0", &at(0)),
                raw_record('9', "5.0", "12", &at(0)),
            ],
        );

        let (ok, output, _) = run_default(&cfg, &root, None);

        // Every line but `not json` is a record, so lines 8 and 9 were read.
        assert_has_line(&output, "schema_record: ok (10 record(s) at 1.0.0)");
        assert_has_line(
            &output,
            &format!("score_range: 7 line(s) (first at line(s) 3, 4, 5, 6, 7)  {SCORE_RANGE_HINT}"),
        );
        assert!(!output.contains("duplicate_id:"), "got:\n{output}");
        assert!(!output.contains("future_timestamp:"), "got:\n{output}");
        assert!(!ok);
    }

    /// BZR-1302 test 1b.
    #[test]
    fn score_range_alone_fails_the_run() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("score-range-alone");
        write_log(
            &root,
            &[clean_record('0'), raw_record('1', "50", "6.0", &at(0))],
        );

        let (ok, output, _) = run_default(&cfg, &root, None);

        assert_only_warning(
            &output,
            &format!("score_range: 1 line(s) (first at line(s) 2)  {SCORE_RANGE_HINT}"),
        );
        assert!(!ok, "got:\n{output}");
    }

    /// BZR-1302 test 2a.
    #[test]
    fn duplicate_id_lists_every_copy_of_each_repeated_id() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("duplicate-id");
        write_log(
            &root,
            &[
                "not json".to_string(),
                clean_record('A'),
                clean_record('B'),
                clean_record('A'),
                clean_record('C'),
                clean_record('A'),
                clean_record('B'),
                clean_record('D'),
                clean_record('D'),
            ],
        );

        let (ok, output, _) = run_default(&cfg, &root, None);

        assert_has_line(
            &output,
            &format!(
                "duplicate_id: 7 line(s) (first at line(s) 2, 3, 4, 6, 7)  {}",
                duplicate_id_hint(3)
            ),
        );
        assert!(!output.contains("score_range:"), "got:\n{output}");
        assert!(!output.contains("future_timestamp:"), "got:\n{output}");
        assert!(!ok);
    }

    /// BZR-1302 test 2b.
    #[test]
    fn duplicate_id_alone_fails_the_run() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("duplicate-id-alone");
        write_log(&root, &[clean_record('A'), clean_record('A')]);

        let (ok, output, _) = run_default(&cfg, &root, None);

        assert_only_warning(
            &output,
            &format!(
                "duplicate_id: 2 line(s) (first at line(s) 1, 2)  {}",
                duplicate_id_hint(1)
            ),
        );
        assert!(!ok, "got:\n{output}");
    }

    /// BZR-1302 test 3a: more than 24 h ahead is flagged; exactly 24 h is not.
    #[test]
    fn future_timestamp_lists_lines_more_than_a_day_ahead() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("future-timestamp");
        write_log(
            &root,
            &[
                "not json".to_string(),
                raw_record('0', "5.0", "6.0", &at(0)),
                raw_record('1', "5.0", "6.0", &at(3_600)),
                raw_record('2', "5.0", "6.0", &at(86_400)),
                raw_record('3', "5.0", "6.0", &at(86_401)),
                raw_record('4', "5.0", "6.0", "2099-01-01T00:00:00Z"),
                raw_record('5', "5.0", "6.0", &at(86_402)),
                raw_record('6', "5.0", "6.0", &at(90_000)),
                raw_record('7', "5.0", "6.0", &at(100_000)),
                raw_record('8', "5.0", "6.0", &at(200_000)),
            ],
        );

        let (ok, output, _) = run_default(&cfg, &root, None);

        assert_has_line(
            &output,
            &format!(
                "future_timestamp: 6 line(s) (first at line(s) 5, 6, 7, 8, 9)  \
                 {FUTURE_TIMESTAMP_HINT}"
            ),
        );
        assert!(!output.contains("score_range:"), "got:\n{output}");
        assert!(!output.contains("duplicate_id:"), "got:\n{output}");
        assert!(!ok);
    }

    /// BZR-1302 test 3b.
    #[test]
    fn future_timestamp_alone_fails_the_run() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("future-timestamp-alone");
        write_log(
            &root,
            &[
                clean_record('0'),
                raw_record('1', "5.0", "6.0", &at(86_401)),
            ],
        );

        let (ok, output, _) = run_default(&cfg, &root, None);

        assert_only_warning(
            &output,
            &format!("future_timestamp: 1 line(s) (first at line(s) 2)  {FUTURE_TIMESTAMP_HINT}"),
        );
        assert!(!ok, "got:\n{output}");
    }

    /// BZR-1302 test 4: doctor warns about a score exactly when recall's
    /// clamp changes it.
    #[test]
    fn score_range_warning_matches_the_clamp() {
        use dreamd_core::salience::clamp_score;

        for v in [
            -1.0_f32,
            -0.0,
            0.0,
            5.0,
            10.0,
            10.000_001,
            11.0,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ] {
            let unclamped = clamp_score(f64::from(v)) == f64::from(v);
            assert_eq!(
                LearnIngress::validate_pain(v).is_ok(),
                unclamped,
                "pain {v}"
            );
            assert_eq!(
                LearnIngress::validate_importance(v).is_ok(),
                unclamped,
                "importance {v}"
            );
        }
    }

    /// BZR-1302 test 5.
    #[test]
    fn registry_with_a_garbage_line_names_the_line() {
        let cfg = Config::default();
        let (root, dir) = setup_agent_root("registry-garbage");
        let path = dir.path().join("registry.toml");
        let raw = garbage_registry(&root);
        fs::write(&path, &raw).unwrap();

        let (ok, output) = run_with_registry(&cfg, &root, Some(&path));

        assert_only_warning(&output, &registry_parse_line(&path, 3));
        assert!(!ok);
        let message = toml::from_str::<Registry>(&raw).unwrap_err().to_string();
        assert!(
            message.starts_with("TOML parse error at line 3,"),
            "{message}"
        );
        assert!(
            dreamd_core::registry::resolve_project(&path, root.project_root()).is_err(),
            "the readers refuse the same file"
        );
    }

    /// BZR-1302 test 6: TOML that is not a `Registry`.
    #[test]
    fn registry_of_the_wrong_shape_names_the_line() {
        let cfg = Config::default();
        let (root, dir) = setup_agent_root("registry-shape");
        let path = dir.path().join("registry.toml");
        for (raw, line) in [
            ("projects = 5\n", 1),
            ("[[projects]]\nfoo = \"x\"\n", 1),
            ("[[projects]]\nroot = \"/a\"\n\n[[projects]]\nroot = 5\n", 5),
        ] {
            fs::write(&path, raw).unwrap();

            let (ok, output) = run_with_registry(&cfg, &root, Some(&path));

            assert_has_line(&output, &registry_parse_line(&path, line));
            assert!(!ok, "{raw:?}");
            assert!(
                dreamd_core::registry::resolve_project(&path, root.project_root()).is_err(),
                "{raw:?}"
            );
        }
    }

    /// BZR-1302 test 7: a registry that is not UTF-8 is a read error.
    #[test]
    fn registry_that_is_not_utf8_is_an_error() {
        let cfg = Config::default();
        let (root, dir) = setup_agent_root("registry-utf8");
        let path = dir.path().join("registry.toml");
        fs::write(&path, b"[[projects]]\nroot = \"\xff\"\n").unwrap();

        let (ok, output) = run_with_registry(&cfg, &root, Some(&path));

        assert_has_line(
            &output,
            &format!(
                "registry: error  [WARNING: could not read {}: stream did not contain valid UTF-8]",
                path.display()
            ),
        );
        assert!(!ok);
    }

    /// BZR-1302 test 8: none of the four lines on a clean store, whatever the
    /// registry argument.
    #[test]
    fn clean_store_prints_none_of_the_hand_edit_lines() {
        let cfg = Config::default();
        let (root, dir) = setup_agent_root("hand-edit-clean");
        seed_events(
            &root,
            &[
                learning('0', "rust::eh", RECORD_SCHEMA_VERSION),
                learning('1', "rust::eh", RECORD_SCHEMA_VERSION),
            ],
        );
        let registered = dir.path().join("registry.toml");
        let canonical = fs::canonicalize(root.project_root()).unwrap();
        fs::write(
            &registered,
            format!(
                "[[projects]]\nroot = {}\n",
                toml_string(&canonical.to_string_lossy())
            ),
        )
        .unwrap();
        let absent = dir.path().join("no-such-home/.agent/registry.toml");

        for registry in [Some(registered.as_path()), None, Some(absent.as_path())] {
            let (ok, output) = run_with_registry(&cfg, &root, registry);

            for prefix in [
                "score_range:",
                "duplicate_id:",
                "future_timestamp:",
                "registry:",
            ] {
                assert!(!output.contains(prefix), "{registry:?}: got:\n{output}");
            }
            assert!(ok, "{registry:?}: got:\n{output}");
        }
    }

    /// BZR-1302 test 9: the four lines, in order, between `schema_record:`
    /// and `schema_index:`.
    #[test]
    fn all_four_hand_edit_lines_print_in_order() {
        let cfg = Config::default();
        let (root, dir) = setup_agent_root("hand-edit-all");
        write_log(
            &root,
            &[
                clean_record('A'),
                raw_record('B', "50", "6.0", &at(0)),
                clean_record('A'),
                raw_record('C', "5.0", "6.0", "2099-01-01T00:00:00Z"),
            ],
        );
        let path = dir.path().join("registry.toml");
        fs::write(&path, garbage_registry(&root)).unwrap();

        let (ok, output) = run_with_registry(&cfg, &root, Some(&path));

        let lines: Vec<&str> = output.lines().collect();
        let record = lines
            .iter()
            .position(|line| line.starts_with("schema_record:"))
            .expect("schema_record line");
        let index = lines
            .iter()
            .position(|line| line.starts_with("schema_index:"))
            .expect("schema_index line");
        assert_eq!(
            lines[record + 1..index].to_vec(),
            vec![
                format!("score_range: 1 line(s) (first at line(s) 2)  {SCORE_RANGE_HINT}"),
                format!(
                    "duplicate_id: 2 line(s) (first at line(s) 1, 3)  {}",
                    duplicate_id_hint(1)
                ),
                format!(
                    "future_timestamp: 1 line(s) (first at line(s) 4)  {FUTURE_TIMESTAMP_HINT}"
                ),
                registry_parse_line(&path, 3),
            ],
            "got:\n{output}"
        );
        assert!(!ok);
    }
}
