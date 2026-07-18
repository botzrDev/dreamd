//! `dreamd migrate --from <ver> --to <ver>` — episodic schema migration (WEG-133 / DR-108).
//!
//! v0.1 is a **stub**: the only registered path is the identity migration
//! `1.0.0` → `1.0.0` (a no-op success). `--from` / `--to` are **episodic record**
//! schema strings ([`dreamd_protocol::RECORD_SCHEMA_VERSION`]); they are not the
//! daemon `state.json` schema (`STATE_SCHEMA_VERSION`, which the `dreamd version`
//! display line prints) and not the Tantivy index schema. Any unregistered pair
//! errors with `no migration registered for that path`.
//!
//! Modeled on [`crate::commands::archive`] / [`crate::commands::doctor`]: a typed
//! error enum, [`AgentRoot::discover`] for root resolution, and injectable
//! `out`/`err` writers for testability.
//!
//! On the registered no-op path we still copy the present durable files
//! (`AGENT_LEARNINGS.jsonl` and `state.json`) to sibling `.bak` files before the
//! migration runs, so a future non-identity transform inherits the safety net.
//! The index is **read-only** here: its schema-version self-heal owns index
//! rebuilds (ARCHITECTURE.md §4), so `migrate` never rewrites, deletes, or
//! backs up the index tree. No `rewrite_atomic` of the JSONL happens on the
//! identity path, so a live daemon can stay up.

use std::io::Write;
use std::path::{Path, PathBuf};

use dreamd_core::index::{ManifestCheckOutcome, ManifestVersionError};
use dreamd_core::migrate::{MigrateError as CoreMigrateError, MigrationRegistry};
use dreamd_core::{AgentRoot, LayoutError};

/// Errors surfaced by `dreamd migrate`.
#[derive(Debug)]
pub enum MigrateError {
    /// No `.agent/` store found walking up from `cwd`.
    NotFound,
    /// No migration is registered for the requested `from → to` pair.
    NoMigration { from: String, to: String },
    /// A registered migration's `apply` failed (never hit by the v0.1 no-op).
    Apply(CoreMigrateError),
    /// Failure copying a durable file to `.bak`, or writing to `out`/`err`.
    Io(std::io::Error),
}

impl From<std::io::Error> for MigrateError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<CoreMigrateError> for MigrateError {
    fn from(e: CoreMigrateError) -> Self {
        Self::Apply(e)
    }
}

impl std::fmt::Display for MigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "no .agent/ directory found"),
            Self::NoMigration { from, to } => {
                write!(f, "no migration registered for that path ({from} → {to})")
            }
            Self::Apply(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for MigrateError {}

/// `dreamd migrate --from <ver> --to <ver>` entry point.
///
/// Resolves the store, reports the three observed on-disk version streams, looks
/// up the requested transform, `.bak`s the present durable files, then applies
/// the (v0.1 no-op) migration. Prints a `migrate: <from> → <to> (no-op)` summary
/// on success.
pub fn run(
    cwd: &Path,
    from: &str,
    to: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), MigrateError> {
    let root = match AgentRoot::discover(cwd) {
        Ok(r) => r,
        Err(LayoutError::NotFound) => {
            writeln!(
                err,
                "dreamd: error — no .agent/ directory found. Run `dreamd init` first."
            )?;
            return Err(MigrateError::NotFound);
        }
    };

    // Report the three independent on-disk schema streams (read-only) before the
    // registry lookup so an operator sees what `migrate` observed regardless of
    // whether the pair is registered.
    writeln!(out, "episodic schema: {}", observed_episodic_version(&root))?;
    writeln!(out, "state schema: {}", observed_state_version(&root))?;
    writeln!(out, "index schema: {}", observed_index_version(&root))?;

    let registry = MigrationRegistry::v0_1();
    let Some(migration) = registry.find(from, to) else {
        writeln!(
            err,
            "dreamd: error — no migration registered for that path ({from} → {to})"
        )?;
        return Err(MigrateError::NoMigration {
            from: from.to_string(),
            to: to.to_string(),
        });
    };

    // Copy the present durable files to sibling `.bak` files (overwriting any
    // existing backup). Missing sources are skipped. The identity migration does
    // not rewrite the JSONL, so this is purely a safety net for future
    // transforms; the index is deliberately not backed up here.
    backup_if_present(&root.episodic_jsonl())?;
    backup_if_present(&root.state_json())?;

    migration.apply(&root)?;

    writeln!(out, "migrate: {from} → {to} (no-op)")?;
    Ok(())
}

/// Sibling backup path: append `.bak` to the full filename (e.g.
/// `AGENT_LEARNINGS.jsonl` → `AGENT_LEARNINGS.jsonl.bak`).
///
/// Uses raw-`OsString` append rather than `Path::with_extension`, which would
/// wrongly *replace* the extension and yield `AGENT_LEARNINGS.bak`.
fn bak_path(src: &Path) -> PathBuf {
    let mut name = src.as_os_str().to_owned();
    name.push(".bak");
    PathBuf::from(name)
}

/// Copy `src` to its sibling `.bak` when present; skip (returning `Ok(false)`)
/// when the source does not exist. `std::fs::copy` overwrites any existing
/// backup. Returns `Ok(true)` when a copy was made.
fn backup_if_present(src: &Path) -> Result<bool, MigrateError> {
    if src.exists() {
        std::fs::copy(src, bak_path(src))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Read-only: first valid JSONL line's `schema_version`, or a clear label.
///
/// An empty or unreadable log does not fail the no-op path — it just reports.
fn observed_episodic_version(root: &AgentRoot) -> String {
    let text = match std::fs::read_to_string(root.episodic_jsonl()) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return "absent".to_string(),
        Err(_) => return "unreadable".to_string(),
    };
    let Some(line) = text.lines().find(|l| !l.trim().is_empty()) else {
        return "empty".to_string();
    };
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(v) => v
            .get("schema_version")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unreadable".to_string()),
        Err(_) => "unreadable".to_string(),
    }
}

/// Read-only: the `state.json` `schema_version`, or a clear label.
fn observed_state_version(root: &AgentRoot) -> String {
    let state = root.state_json();
    if !state.exists() {
        return "absent".to_string();
    }
    match std::fs::read(&state) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| {
                v.get("schema_version")
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "unreadable".to_string()),
        Err(_) => "unreadable".to_string(),
    }
}

/// Read-only: report the Tantivy index manifest schema version. `migrate` never
/// rewrites, deletes, or `.bak`s the index — schema-version self-heal owns index
/// rebuilds (ARCHITECTURE.md §4); this only reports.
fn observed_index_version(root: &AgentRoot) -> String {
    let manifest = root
        .dreamd_dir()
        .join(dreamd_core::index::INDEX_MANIFEST_FILENAME); // read-only report; never rewrite
    match dreamd_core::index::check_manifest_version(&manifest) {
        Ok(ManifestCheckOutcome::Absent) => "absent".to_string(),
        Ok(ManifestCheckOutcome::Current) => dreamd_core::index::SCHEMA_VERSION.to_string(),
        Ok(ManifestCheckOutcome::NeedsMigration { from }) => from,
        Err(ManifestVersionError::TooNew { manifest, .. }) => manifest,
        Err(_) => "unreadable".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Scaffold a `.agent/` root under a fresh tempdir. Returns the tempdir (as
    /// `cwd`) and the bound `AgentRoot`.
    fn scaffold() -> (tempfile::TempDir, AgentRoot) {
        let tmp = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(tmp.path());
        fs::create_dir_all(root.episodic_dir()).unwrap();
        fs::create_dir_all(root.dreamd_dir()).unwrap();
        (tmp, root)
    }

    const SAMPLE_JSONL: &str = r#"{"schema_version":"1.0.0","id":"evt_01ARZ3NDEKTSV4RRFFQ69G5FAV","timestamp":"2026-07-03T00:00:00Z","pain":5.0,"importance":6.0,"pinned":false,"skill_action":"rust::migrate","source_harness":"test-harness","content":"seed"}"#;

    #[test]
    fn display_covers_all_variants() {
        assert_eq!(
            MigrateError::NotFound.to_string(),
            "no .agent/ directory found"
        );
        assert_eq!(
            MigrateError::NoMigration {
                from: "1.0.0".to_string(),
                to: "2.0.0".to_string(),
            }
            .to_string(),
            "no migration registered for that path (1.0.0 → 2.0.0)"
        );
        let io = MigrateError::from(std::io::Error::other("disk full"));
        assert_eq!(io.to_string(), "disk full");
    }

    #[test]
    fn bak_path_appends_not_replaces_extension() {
        let p = Path::new("/x/.agent/episodic/AGENT_LEARNINGS.jsonl");
        assert_eq!(
            bak_path(p),
            PathBuf::from("/x/.agent/episodic/AGENT_LEARNINGS.jsonl.bak")
        );
        let s = Path::new("/x/.agent/.dreamd/state.json");
        assert_eq!(
            bak_path(s),
            PathBuf::from("/x/.agent/.dreamd/state.json.bak")
        );
    }

    #[test]
    fn noop_path_succeeds_and_baks_present_durable_files() {
        let (tmp, root) = scaffold();
        fs::write(root.episodic_jsonl(), SAMPLE_JSONL).unwrap();
        fs::write(
            root.state_json(),
            br#"{"schema_version":"1.0","daemon_version":"0.0.0"}"#,
        )
        .unwrap();
        // Seed an index manifest so we can prove migrate never `.bak`s it.
        let manifest = root.dreamd_dir().join("index_manifest.json"); // never bak'd by migrate
        fs::write(&manifest, br#"{"schema_version":"index/1.3"}"#).unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        run(tmp.path(), "1.0.0", "1.0.0", &mut out, &mut err).unwrap();

        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("episodic schema: 1.0.0"), "got: {out}");
        assert!(out.contains("state schema: 1.0"), "got: {out}");
        assert!(out.contains("index schema: index/1.3"), "got: {out}");
        assert!(out.contains("(no-op)"), "got: {out}");
        assert!(err.is_empty());

        // Durable files were copied to sibling `.bak` files.
        assert!(bak_path(&root.episodic_jsonl()).exists());
        assert!(bak_path(&root.state_json()).exists());
        assert_eq!(
            fs::read(bak_path(&root.episodic_jsonl())).unwrap(),
            SAMPLE_JSONL.as_bytes()
        );
        // The self-healing index manifest is never backed up by migrate.
        assert!(
            !bak_path(&manifest).exists(),
            "index manifest must not be backed up"
        );
    }

    #[test]
    fn overwrites_an_existing_bak() {
        let (tmp, root) = scaffold();
        fs::write(root.episodic_jsonl(), SAMPLE_JSONL).unwrap();
        fs::write(bak_path(&root.episodic_jsonl()), b"stale backup").unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        run(tmp.path(), "1.0.0", "1.0.0", &mut out, &mut err).unwrap();

        assert_eq!(
            fs::read(bak_path(&root.episodic_jsonl())).unwrap(),
            SAMPLE_JSONL.as_bytes(),
            "existing .bak must be overwritten with current contents"
        );
    }

    #[test]
    fn empty_log_still_no_ops() {
        let (tmp, root) = scaffold();
        fs::write(root.episodic_jsonl(), b"").unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        run(tmp.path(), "1.0.0", "1.0.0", &mut out, &mut err).unwrap();

        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("episodic schema: empty"), "got: {out}");
        // An empty log is still `.bak`'d (the file is present).
        assert!(bak_path(&root.episodic_jsonl()).exists());
    }

    #[test]
    fn absent_durable_files_are_skipped() {
        let (tmp, root) = scaffold();
        // No episodic JSONL, no state.json on disk.
        let mut out = Vec::new();
        let mut err = Vec::new();
        run(tmp.path(), "1.0.0", "1.0.0", &mut out, &mut err).unwrap();

        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("episodic schema: absent"), "got: {out}");
        assert!(out.contains("state schema: absent"), "got: {out}");
        assert!(!bak_path(&root.episodic_jsonl()).exists());
        assert!(!bak_path(&root.state_json()).exists());
    }

    #[test]
    fn unregistered_pair_errors_with_locked_phrase() {
        let (tmp, root) = scaffold();
        fs::write(root.episodic_jsonl(), SAMPLE_JSONL).unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        // A pair the v0.1 stub does not register (forward transform).
        let result = run(tmp.path(), "1.0.0", "1.1.0", &mut out, &mut err);

        assert!(matches!(result, Err(MigrateError::NoMigration { .. })));
        let err = String::from_utf8(err).unwrap();
        assert!(
            err.contains("no migration registered for that path"),
            "got: {err}"
        );
        // No backup is taken on the miss path.
        assert!(!bak_path(&root.episodic_jsonl()).exists());
    }

    #[test]
    fn display_state_token_is_not_registered() {
        // The daemon-state / `dreamd version` display schema is never a token.
        let (tmp, _root) = scaffold();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run(tmp.path(), "1.0", "1.0", &mut out, &mut err);
        assert!(matches!(result, Err(MigrateError::NoMigration { .. })));
    }

    #[test]
    fn missing_store_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run(tmp.path(), "1.0.0", "1.0.0", &mut out, &mut err);
        assert!(matches!(result, Err(MigrateError::NotFound)));
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("no .agent/ directory found"));
        assert!(err.contains("dreamd init"));
    }
}
