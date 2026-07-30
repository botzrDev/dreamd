//! `dreamd doctor` — structured health-check output (WEG-66 / DR-315,
//! extended by AILAB-223).
//!
//! Prints one line per check to stdout. Exit 0 if all checks pass; exit 1 if
//! any check emits a WARNING or ERROR. Default checks (when applicable):
//! dream-cycle mode, scaffolded subdirs, on-disk index freshness vs JSONL
//! watermark, episodic log health (malformed lines / torn tail), record and
//! index schema versions, orphaned daemon socket, Tantivy lock-file presence,
//! and last autobiography skip.
//!
//! `--repair` unlinks an orphaned daemon socket and rebuilds the derived
//! Tantivy cache by replaying the episodic log (read-only toward the log;
//! Tantivy flock files are never unlinked on their own — see
//! [`check_tantivy_locks`]). `--cluster-health` compares `skill_action`
//! prefix counts derived from the episodic log against the recurrence
//! sidecar the dream cycle wrote.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::Path;

use dreamd_core::autobiography::AutobiographySkip;
use dreamd_core::config::{Config, DreamCycleMode};
use dreamd_core::consolidation;
use dreamd_core::index::{
    check_manifest_version, ManifestCheckOutcome, ManifestVersionError, RecurrenceSidecar,
    INDEX_MANIFEST_FILENAME, SCHEMA_VERSION as INDEX_SCHEMA_VERSION,
};
use dreamd_core::layout::AgentRoot;
use dreamd_protocol::{AgentLearning, RECORD_SCHEMA_VERSION};

/// Mode flags for a doctor run. Mirrors `cli::DoctorArgs` without pulling
/// clap into the command module; both flags compose with the default checks.
#[derive(Debug, Clone, Copy, Default)]
pub struct DoctorFlags {
    /// Unlink an orphaned daemon socket and rebuild the Tantivy cache.
    pub repair: bool,
    /// Compare episodic prefix counts against the recurrence sidecar.
    pub cluster_health: bool,
}

/// Run `dreamd doctor` and write output to `out` (repair refusals go to `err`).
///
/// `socket` is the resolved daemon UDS path (`$DREAMD_SOCK` else the
/// daemon-home socket), or `None` when no path could be resolved.
///
/// Returns `Ok(true)` if all checks passed (exit 0 caller), `Ok(false)` if any
/// WARNING or ERROR was emitted or a repair step failed (exit 1 caller).
pub fn run(
    config: &Config,
    agent_root: &AgentRoot,
    skip: Option<&AutobiographySkip>,
    socket: Option<&Path>,
    flags: DoctorFlags,
    out: &mut impl Write,
    err: &mut impl Write,
) -> io::Result<bool> {
    let mut all_ok = true;

    // DR-315 — dream-cycle mode line.
    match config.dream_cycle_mode {
        DreamCycleMode::Manual => {
            writeln!(out, "dream_cycle_mode: manual")?;
        }
        DreamCycleMode::Auto => {
            writeln!(
                out,
                "dream_cycle_mode: auto  [WARNING: not supported at v0.1]"
            )?;
            all_ok = false;
        }
    }

    // AILAB-223 — hard integrity over the init-scaffolded subdirs.
    all_ok &= check_subdirs(agent_root, out)?;

    // v0.1 index-vs-JSONL contract — on-disk watermark vs episodic tail.
    #[cfg(unix)]
    {
        match dreamd_core::server::assess_index_freshness(agent_root) {
            Ok(freshness) if !freshness.stale => {
                writeln!(out, "index_freshness: ok")?;
            }
            Ok(freshness) => {
                writeln!(
                    out,
                    "index_freshness: stale  [WARNING: {} unindexed event(s); \
                     jsonl_tail={}; watermark={}; recall may miss recent events \
                     until indexer commit or daemon restart replay]",
                    freshness.unindexed_count,
                    freshness.jsonl_tail_id.as_deref().unwrap_or("(none)"),
                    freshness.last_indexed_id.as_deref().unwrap_or("(none)"),
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
    }

    // WEG-132 / DR-812 — episodic log malformed-line check (DR-107 doctor).
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
                         line with no trailing newline; truncated on daemon open]",
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

    // One read of the episodic log backs both the record-schema check and
    // --cluster-health prefix derivation (an absent log reads as no records).
    let events = match dreamd_core::episodic::read_all(&episodic_path) {
        Ok(events) => Some(events),
        Err(e) => {
            writeln!(
                out,
                "schema_record: error  [WARNING: could not read the episodic log: {e}]"
            )?;
            all_ok = false;
            None
        }
    };
    if let Some(events) = &events {
        all_ok &= check_schema_record(events, out)?;
    }

    all_ok &= check_schema_index(agent_root, out)?;
    all_ok &= check_orphaned_uds(socket, out)?;
    #[cfg(unix)]
    check_tantivy_locks(agent_root, out)?;

    // WEG-63 — last autobiography skip (if any).
    if let Some(s) = skip {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let elapsed = now.saturating_sub(s.at);
        let duration_str = format_elapsed(elapsed);
        let file_count = s.files.len();
        writeln!(
            out,
            "last autobiography skip: {} ago — {} — {} files",
            duration_str, s.reason, file_count,
        )?;
    }

    if flags.cluster_health {
        all_ok &= check_cluster_health(agent_root, events.as_deref(), out)?;
    }

    if flags.repair {
        all_ok &= run_repair(agent_root, socket, out, err)?;
    }

    Ok(all_ok)
}

/// Hard integrity over the five dirs `dreamd init` scaffolds. `skills/` and
/// `protocols/` are deliberately not checked: init does not create them, so
/// their absence is normal on a healthy store (AILAB-223 open Q1 default).
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
        writeln!(out, "orphaned_uds: none (daemon live at {})", sock.display())?;
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

/// `--cluster-health`: derive `skill_action` prefix counts from the episodic
/// log (same prefix rules as the cluster engine) and compare against the
/// recurrence sidecar the dream cycle wrote. An absent sidecar on a
/// never-dreamed store is benign.
fn check_cluster_health(
    agent_root: &AgentRoot,
    events: Option<&[AgentLearning]>,
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

    let derived: BTreeMap<String, usize> = consolidation::build_prefix_tree(events)
        .into_iter()
        .map(|(prefix, members)| (prefix, members.len()))
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
                writeln!(out, "cluster_health: {key} jsonl={jsonl_count} sidecar={count}")?;
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

/// `--repair`: unlink an orphaned daemon socket, wipe the rebuildable index
/// cache, and reopen the index to force a full replay of the episodic log
/// (which repair only ever reads). Fails closed when a live daemon is
/// detected — wiping the cache underneath its writer would race it.
#[cfg(unix)]
fn run_repair(
    agent_root: &AgentRoot,
    socket: Option<&Path>,
    out: &mut impl Write,
    err: &mut impl Write,
) -> io::Result<bool> {
    use dreamd_core::server::{
        is_daemon_socket_live, TantivyIndexHandle, DEFAULT_COMMIT_CADENCE, INDEX_DIR_NAME,
        INDEX_PROGRESS_FILENAME,
    };

    if let Some(sock) = socket {
        if is_daemon_socket_live(sock) {
            writeln!(
                err,
                "dreamd: error — doctor --repair refused: a daemon is live on {}. \
                 Stop `dreamd watch` / `dreamd mcp` first, then re-run.",
                sock.display()
            )?;
            return Ok(false);
        }
    }

    let mut actions: Vec<String> = Vec::new();

    // The liveness probe above came back dead, so a still-present socket file
    // is an orphan and safe to unlink.
    if let Some(sock) = socket {
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
    // The replay commits before `open` returns, so the handle (and its
    // indexer task) can be dropped immediately afterwards.
    let rt = match tokio::runtime::Runtime::new() {
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
        TantivyIndexHandle::open(agent_root, DEFAULT_COMMIT_CADENCE)
    };
    match reopened {
        Ok(_handle) => actions.push("rebuilt index from the episodic log".to_string()),
        Err(e) => {
            writeln!(err, "dreamd: error — index rebuild failed: {e}")?;
            return Ok(false);
        }
    }

    if actions.is_empty() {
        writeln!(out, "repair: nothing to do")?;
    } else {
        writeln!(out, "repair: {}", actions.join("; "))?;
    }
    Ok(true)
}

#[cfg(not(unix))]
fn run_repair(
    _agent_root: &AgentRoot,
    _socket: Option<&Path>,
    _out: &mut impl Write,
    err: &mut impl Write,
) -> io::Result<bool> {
    writeln!(
        err,
        "dreamd: error — doctor --repair requires the unix daemon runtime"
    )?;
    Ok(false)
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
        let ok = run(cfg, root, skip, socket, flags, &mut out, &mut err).expect("run ok");
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

    // Fixed young timestamp (2026-07-03Z) — checks here are count-based.
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

    /// Seed the episodic log with `records` and set the index watermark to the
    /// last record so `index_freshness` reads fresh on unix.
    fn seed_events(root: &AgentRoot, records: &[AgentLearning]) {
        let mut body = String::new();
        for r in records {
            body.push_str(&serde_json::to_string(r).unwrap());
            body.push('\n');
        }
        fs::write(root.episodic_jsonl(), body).unwrap();
        if let Some(last) = records.last() {
            fs::write(
                root.dreamd_dir().join("index_progress.json"),
                format!("{{\"last_indexed_id\":\"{}\"}}", last.id.as_str()),
            )
            .unwrap();
        }
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
            at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
                - 300, // 5 minutes ago
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
        body.push_str(&serde_json::to_string(&learning('A', "rust::x", RECORD_SCHEMA_VERSION)).unwrap());
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
        assert!(ok, "store without skills//protocols/ must be ok; got: {output:?}");
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
            output.contains(&format!("schema_record: ok (1 record(s) at {RECORD_SCHEMA_VERSION}")),
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
        assert!(ok, "absent sidecar on a never-dreamed store is not an error");
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
            output.contains("cluster_health: rust jsonl=3 sidecar=(none)  [WARNING: drift]"),
            "jsonl-only prefix must be flagged; got: {output:?}"
        );
        assert!(
            output.contains("cluster_health: rust::borrow jsonl=3 sidecar=2  [WARNING: drift]"),
            "count mismatch must be flagged; got: {output:?}"
        );
        assert!(
            output.contains("cluster_health: python jsonl=(none) sidecar=1  [WARNING: drift]"),
            "sidecar-only key must be flagged; got: {output:?}"
        );
        assert!(
            output.contains("3 drifted key(s)"),
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
        fs::write(
            root.semantic_dir().join("recurrence_counts.json"),
            r#"{"schema_version":"1.0","clusters":[
                {"skill_action":"rust","count":3},
                {"skill_action":"rust::borrow","count":3}]}"#,
        )
        .unwrap();
        let flags = DoctorFlags {
            cluster_health: true,
            ..Default::default()
        };
        let (ok, output, _) = run_with(&cfg, &root, None, None, flags);
        assert!(
            output.contains("cluster_health: ok (2 prefix(es) match the sidecar)"),
            "matching sidecar must report ok; got: {output:?}"
        );
        assert!(ok, "matching sidecar must keep all_ok=true; got: {output:?}");
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
        assert!(ok, "repair on a healthy store must succeed; stderr: {err:?}");
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
        let manifest =
            fs::read_to_string(root.dreamd_dir().join(INDEX_MANIFEST_FILENAME)).unwrap();
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
}
