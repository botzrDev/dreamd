//! Registry reader/resolver and the one locked writer for
//! `~/.agent/registry.toml` (DR-412).
//!
//! [`resolve_project`] is the read side: pure, no side effects.
//! [`RegistryCache`] is the same read for the daemon's per-request lookup: it
//! parses the file once per file identity (BZR-1243).
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
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::io::lock_exclusive;
#[cfg(unix)]
use crate::io::write_atomic;
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
    find_project(&registry, agent_root)
}

/// The entry of `registry` whose root is `agent_root`, canonicalized.
fn find_project(registry: &Registry, agent_root: &Path) -> io::Result<Option<ProjectEntry>> {
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
        .iter()
        .find(|p| p.root == canonical_str)
        .cloned())
}

/// The identity of one version of `registry.toml`. `update_registry` replaces
/// the file by rename, so on Unix every write gives it a new inode, even
/// inside one mtime tick.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileKey {
    len: u64,
    mtime: Option<SystemTime>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

impl FileKey {
    fn of(meta: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            len: meta.len(),
            mtime: meta.modified().ok(),
            #[cfg(unix)]
            dev: meta.dev(),
            #[cfg(unix)]
            ino: meta.ino(),
        }
    }
}

/// `registry.toml`, parsed once per file identity (BZR-1243).
///
/// [`Self::resolve`] is [`resolve_project`] with the parse cached: it stats the
/// file on every call, and reads and parses it only when the stat differs from
/// the cached one. Blocking: the daemon calls it on `spawn_blocking`.
pub struct RegistryCache {
    path: PathBuf,
    cached: Mutex<Option<(FileKey, Arc<Registry>)>>,
    #[cfg(test)]
    loads: AtomicUsize,
}

impl RegistryCache {
    /// A cache of the registry at `path`. Nothing is read until the first
    /// [`Self::resolve`].
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            cached: Mutex::new(None),
            #[cfg(test)]
            loads: AtomicUsize::new(0),
        }
    }

    /// Resolve `agent_root` the way [`resolve_project`] does.
    ///
    /// The stat comes before the read, so a file that changes in between is
    /// cached under its older key and read again at the next call. A parse
    /// failure is an `Err` and leaves nothing cached; an absent file clears the
    /// cache and resolves nothing.
    pub fn resolve(&self, agent_root: &Path) -> io::Result<Option<ProjectEntry>> {
        let mut cached = self.cached.lock().expect("registry cache poisoned");
        let key = match std::fs::metadata(&self.path) {
            Ok(meta) => FileKey::of(&meta),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                *cached = None;
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let registry = match cached.as_ref() {
            Some((held, registry)) if *held == key => Arc::clone(registry),
            _ => {
                let raw = std::fs::read_to_string(&self.path)?;
                #[cfg(test)]
                self.loads.fetch_add(1, Ordering::Relaxed);
                let registry: Arc<Registry> =
                    Arc::new(toml::from_str(&raw).map_err(io::Error::other)?);
                *cached = Some((key, Arc::clone(&registry)));
                registry
            }
        };
        drop(cached);
        find_project(&registry, agent_root)
    }

    /// The number of parses so far.
    #[cfg(test)]
    pub(crate) fn loads(&self) -> usize {
        self.loads.load(Ordering::Relaxed)
    }
}

/// Read-modify-write `registry.toml` under the registry flock.
///
/// Takes [`DaemonHome::registry_lock`], re-reads the registry under it (an
/// absent file reads as empty), canonicalizes `project_root` (falling back to
/// the path as given when that fails), and hands both to `mutate`. When
/// `mutate` returns `true` the registry is replaced and chmodded `0600` on
/// Unix (`write_atomic`); on Windows the same bytes go through
/// `io::replace_renaming` because `write_atomic` stays unsupported. When it
/// returns `false` nothing is written. Returns what
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
    // Unix keeps the parent-directory fsync in `write_atomic` and chmod 0600.
    // Windows cannot call `write_atomic` (that would also unlock the dream
    // cycle). `replace_renaming` is the registry-only temp-file rename.
    #[cfg(unix)]
    {
        write_atomic(&registry_path, serialized.as_bytes())?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&registry_path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        crate::io::replace_renaming(&registry_path, serialized.as_bytes())?;
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

    // ── BZR-1243: RegistryCache ───────────────────────────────────────────

    /// `[[projects]]` naming `roots`, canonicalized.
    fn registry_body(roots: &[&Path]) -> String {
        roots
            .iter()
            .map(|root| {
                let canonical = std::fs::canonicalize(root).unwrap();
                format!("[[projects]]\nroot = \"{}\"\n", canonical.display())
            })
            .collect()
    }

    fn register(home: &DaemonHome, root: &Path) {
        update_registry(home, root, |registry, root| {
            registry.projects.push(ProjectEntry {
                root: root.to_owned(),
            });
            true
        })
        .unwrap();
    }

    fn mtime(path: &Path) -> std::time::SystemTime {
        std::fs::metadata(path).unwrap().modified().unwrap()
    }

    #[test]
    fn an_unchanged_registry_is_parsed_once() {
        let home_dir = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let home = DaemonHome::new(home_dir.path());
        register(&home, project.path());
        let cache = RegistryCache::new(home.registry_toml());

        for _ in 0..3 {
            assert!(cache.resolve(project.path()).unwrap().is_some());
        }
        assert_eq!(cache.loads(), 1);
    }

    #[test]
    fn a_replaced_registry_is_reloaded() {
        let home_dir = tempfile::tempdir().unwrap();
        let (first, second) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let home = DaemonHome::new(home_dir.path());
        register(&home, first.path());
        let cache = RegistryCache::new(home.registry_toml());
        assert!(cache.resolve(first.path()).unwrap().is_some());

        register(&home, second.path());

        assert!(cache.resolve(second.path()).unwrap().is_some());
        assert_eq!(cache.loads(), 2);
    }

    #[test]
    fn a_same_size_same_mtime_replacement_is_still_seen() {
        let home_dir = tempfile::tempdir().unwrap();
        let (old_root, new_root) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let path = home_dir.path().join("registry.toml");
        std::fs::write(&path, registry_body(&[old_root.path()])).unwrap();
        let cache = RegistryCache::new(path.clone());
        assert!(cache.resolve(old_root.path()).unwrap().is_some());

        let replacement = registry_body(&[new_root.path()]);
        assert_eq!(
            replacement.len() as u64,
            std::fs::metadata(&path).unwrap().len(),
            "precondition: the same byte length"
        );
        let sibling = home_dir.path().join("registry.toml.new");
        std::fs::write(&sibling, &replacement).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&sibling)
            .unwrap()
            .set_modified(mtime(&path))
            .unwrap();
        assert_eq!(
            mtime(&sibling),
            mtime(&path),
            "precondition: the same mtime"
        );
        std::fs::rename(&sibling, &path).unwrap();

        assert!(cache.resolve(new_root.path()).unwrap().is_some());
        assert!(cache.resolve(old_root.path()).unwrap().is_none());
    }

    #[test]
    fn a_malformed_registry_is_an_error_and_is_not_cached() {
        let home_dir = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let path = home_dir.path().join("registry.toml");
        let valid = registry_body(&[project.path()]);
        // The same length as `valid`, and not TOML.
        let garbage = valid.replacen("[[projects]]", "]]projects[[", 1);
        std::fs::write(&path, &garbage).unwrap();
        let cache = RegistryCache::new(path.clone());

        assert!(cache.resolve(project.path()).is_err());
        assert!(
            cache.resolve(project.path()).is_err(),
            "a parse failure is not cached"
        );

        // Written back in place with the garbage's length and mtime, so the
        // stat matches the failed read's.
        let failed_at = mtime(&path);
        std::fs::write(&path, &valid).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(failed_at)
            .unwrap();
        assert!(cache.resolve(project.path()).unwrap().is_some());
    }

    #[test]
    fn a_missing_registry_resolves_nothing() {
        let home_dir = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let path = home_dir.path().join("registry.toml");
        let cache = RegistryCache::new(path.clone());

        assert!(cache.resolve(project.path()).unwrap().is_none());

        std::fs::write(&path, registry_body(&[project.path()])).unwrap();
        assert!(cache.resolve(project.path()).unwrap().is_some());
    }
}
