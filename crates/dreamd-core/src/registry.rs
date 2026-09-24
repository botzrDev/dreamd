//! Registry reader/resolver and the one locked writer for
//! `~/.agent/registry.toml` (DR-412).
//!
//! [`resolve_project`] is the read side: pure, no side effects.
//! [`update_registry`] is the only write path. `dreamd init`
//! (`register_project`) and `dreamd init --uninstall-project`
//! (`uninstall_project`) both go through it, so the flock, the re-read under
//! the lock, the root canonicalization, `write_atomic`, and the `0600` chmod
//! are stated once. Do not add a second writer; add a mutation closure.
//!
//! Key invariant: stored `ProjectEntry::root` values are canonicalized at
//! write time. Reads canonicalize the query path the same way so that
//! symlink-aliased directories resolve correctly.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::io::{lock_exclusive, write_atomic};
use crate::layout::DaemonHome;

/// Top-level shape of `~/.agent/registry.toml`. The daemon iterates
/// `projects` at startup to discover which project stores to serve.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Registry {
    /// Known project roots, each added by `dreamd init`.
    #[serde(default)]
    pub projects: Vec<ProjectEntry>,
}

/// Single project registration. Currently only carries the root path;
/// future fields (e.g., last-seen timestamp) will land here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectEntry {
    /// Canonicalized absolute path to the project root (written by
    /// `dreamd init`, resolved via `std::fs::canonicalize`).
    pub root: String,
}

/// Resolve an `X-Agent-Root` value (a project-root path) to its registered
/// entry. `Ok(None)` if the registry file is absent or the path is not
/// registered. `Err` only on a present-but-malformed registry or an I/O
/// error reading it.
pub fn resolve_project(
    registry_path: &Path,
    agent_root: &Path,
) -> io::Result<Option<ProjectEntry>> {
    if !registry_path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(registry_path)?;
    let registry: Registry = toml::from_str(&raw).map_err(io::Error::other)?;
    // Stored roots are canonicalized at write time; canonicalize the query
    // the same way. Fall back to the raw path on failure — mirrors
    // update_registry()'s write-side behavior.
    let canonical = std::fs::canonicalize(agent_root).unwrap_or_else(|_| agent_root.to_path_buf());
    let canonical_str = canonical.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "project path is not valid UTF-8",
        )
    })?;
    Ok(registry
        .projects
        .into_iter()
        .find(|p| p.root == canonical_str))
}

/// Read-modify-write `registry.toml` under the registry flock.
///
/// Takes [`DaemonHome::registry_lock`], re-reads the registry under it (an
/// absent file reads as empty), canonicalizes `project_root` (falling back to
/// the path as given when that fails), and hands both to `mutate`. When
/// `mutate` returns `true` the registry is `write_atomic`ed and chmodded
/// `0600` on Unix; when it returns `false` nothing is written. Returns what
/// `mutate` returned.
///
/// Does not `create_dir_all` the daemon home and does not print: the caller
/// owns both. `register_project` creates the home first; uninstall returns
/// before calling this when `registry.toml` is absent, so it never scaffolds
/// `~/.agent/` as a locking side effect (AILAB-161).
pub fn update_registry(
    daemon: &DaemonHome,
    project_root: &Path,
    mutate: impl FnOnce(&mut Registry, &str) -> bool,
) -> io::Result<bool> {
    // Serialize the whole read → parse → mutate → write_atomic → chmod cycle
    // against the other writer. `write_atomic` is durability, not exclusion:
    // two unlocked cycles read the same snapshot and the later rename silently
    // drops the earlier writer's change (AILAB-161).
    let _guard = lock_exclusive(&daemon.registry_lock())?;
    let registry_path = daemon.registry_toml();

    let mut registry: Registry = if registry_path.exists() {
        let raw = std::fs::read_to_string(&registry_path)?;
        toml::from_str(&raw).map_err(io::Error::other)?
    } else {
        Registry::default()
    };

    let canonical =
        std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let canonical_str = canonical.to_string_lossy().into_owned();

    if !mutate(&mut registry, &canonical_str) {
        return Ok(false);
    }

    let serialized = toml::to_string(&registry).map_err(io::Error::other)?;
    write_atomic(&registry_path, serialized.as_bytes())?;
    // Defense-in-depth: registry.toml lists every registered project root.
    // 0600 even though ~/.agent/ is already 0700. Unix-only; Windows perms are
    // deferred to v0.1.1 / DR-121.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&registry_path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_registry(entries: &[&str]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        if entries.is_empty() {
            writeln!(f, "projects = []").unwrap();
        } else {
            for root in entries {
                writeln!(f, "[[projects]]").unwrap();
                writeln!(f, r#"root = "{}""#, root).unwrap();
            }
        }
        f
    }

    #[test]
    fn absent_registry_returns_none() {
        let path = std::path::PathBuf::from("/tmp/does_not_exist_weg75.toml");
        let result = resolve_project(&path, std::path::Path::new("/some/project")).unwrap();
        assert!(result.is_none(), "absent registry must yield None, not Err");
    }

    #[test]
    fn registered_path_returns_entry() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let canonical_str = canonical.to_string_lossy().into_owned();

        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "[[projects]]").unwrap();
        writeln!(f, r#"root = "{}""#, canonical_str).unwrap();

        let result = resolve_project(f.path(), dir.path()).unwrap();
        assert_eq!(result.unwrap().root, canonical_str);
    }

    #[test]
    fn unregistered_path_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_registry(&["/some/other/project"]);
        let result = resolve_project(f.path(), dir.path()).unwrap();
        assert!(result.is_none(), "unregistered path must yield None");
    }

    #[test]
    fn malformed_toml_returns_err() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "this is not valid toml ][[").unwrap();
        let result = resolve_project(f.path(), std::path::Path::new("/any"));
        assert!(
            result.is_err(),
            "malformed TOML must produce Err, not panic"
        );
    }

    #[test]
    fn non_canonical_query_matches_canonical_stored_root() {
        // Create a real temp dir with a canonical path, register it under
        // its canonical string, then query via the temp dir path directly
        // (which fs::canonicalize will resolve to the same thing).
        let dir = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let canonical_str = canonical.to_string_lossy().into_owned();

        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "[[projects]]").unwrap();
        writeln!(f, r#"root = "{}""#, canonical_str).unwrap();

        // Query with the raw temp dir path (not pre-canonicalized by test)
        let result = resolve_project(f.path(), dir.path()).unwrap();
        assert_eq!(result.unwrap().root, canonical_str);
    }
}
