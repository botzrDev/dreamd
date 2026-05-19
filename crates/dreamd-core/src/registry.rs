//! Registry reader/resolver for `~/.agent/registry.toml` (DR-412).
//!
//! The write path lives in `dreamd-cli::commands::init::register_project`.
//! This module is the read side: pure, no side effects.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub projects: Vec<ProjectEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectEntry {
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
    // register_project()'s write-side behavior.
    let canonical = std::fs::canonicalize(agent_root)
        .unwrap_or_else(|_| agent_root.to_path_buf());
    let canonical_str = canonical.to_string_lossy();
    Ok(registry.projects.into_iter().find(|p| p.root == canonical_str))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_registry(entries: &[&str]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        if entries.is_empty() {
            write!(f, "projects = []\n").unwrap();
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
        assert!(result.is_none());
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
        assert!(result.is_none());
    }

    #[test]
    fn malformed_toml_returns_err() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "this is not valid toml ][[\n").unwrap();
        let result = resolve_project(f.path(), std::path::Path::new("/any"));
        assert!(result.is_err());
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
