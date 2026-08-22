//! `dreamd init` — scaffold the per-project `.agent/` store (DR-105 / WEG-9).
//!
//! Output is byte-locked against `tests/fixtures/init.golden.txt` and
//! `tests/fixtures/init.rerun.golden.txt`. Any change to stdout text, ordering,
//! or whitespace must be coordinated with the Clip A beat-sheet
//! (`context/video/scripts/clip-a/`). Em-dashes are U+2014 (3 bytes).

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use dreamd_core::config::CONFIG_TEMPLATE;
use dreamd_core::io::{lock_exclusive, write_atomic};
use dreamd_core::privacy::DR413_DISCLOSURE;
use dreamd_core::registry::{ProjectEntry, Registry};
use dreamd_core::{AgentRoot, DaemonHome, DEFAULT_WORKSPACE_MD, GITIGNORE_SNIPPET};
use serde::Serialize;

const RERUN_MSG: &str = "dreamd: already initialized — .agent/ exists. nothing to do.";
const RERUN_MSG_QUIET: &str = "dreamd: already initialized.";

const ROOT_SENTINELS: &[&str] = &[".git", "Cargo.toml", "package.json", "pyproject.toml"];

/// Failure modes for init scaffolding and registry operations.
#[derive(Debug)]
pub enum InitError {
    NoProjectRoot,
    Io(std::io::Error),
}

impl From<std::io::Error> for InitError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl std::fmt::Display for InitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoProjectRoot => write!(f, "no project root"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for InitError {}

#[derive(Serialize)]
struct State {
    schema_version: &'static str,
    daemon_version: &'static str,
    last_dream_cycle_at: Option<String>,
    last_dream_cycle_status: &'static str,
}

/// Scaffold the per-project `.agent/` directory tree and register it with
/// the daemon home registry (`~/.agent/registry.toml`).
///
/// Idempotent: if `.agent/` already exists, prints a rerun message and
/// returns `Ok(())`. Staging is done in a temp dir; the final rename is
/// atomic within a single filesystem.
///
/// Returns [`InitError::NoProjectRoot`] if no sentinel (`.git/`,
/// `Cargo.toml`, etc.) is found walking up from `cwd`.
pub fn run(
    cwd: &Path,
    daemon_home: &Path,
    quiet: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), InitError> {
    let project_root = match find_project_root(cwd) {
        Some(r) => r,
        None => {
            writeln!(
                err,
                "dreamd: error — no project root found. Run from inside a project directory (must contain .git/, Cargo.toml, package.json, or pyproject.toml). If this is a new folder, run `git init` first."
            )?;
            return Err(InitError::NoProjectRoot);
        }
    };

    let agent = AgentRoot::new(&project_root);

    // Idempotency guard: only the committed path counts.
    if agent.agent_dir().exists() {
        writeln!(out, "{}", if quiet { RERUN_MSG_QUIET } else { RERUN_MSG })?;
        return Ok(());
    }

    // Stage the entire scaffold inside a tmp directory so that any failure
    // before the rename leaves .agent/ untouched.  Rename is atomic within a
    // single filesystem, which is the only case we need to handle (project
    // root and its parent share a mount in all realistic scenarios).
    let tmp_dir = project_root.join(format!(".agent.tmp-{}", std::process::id()));

    // Clean up the tmp dir on any error so repeated failed runs don't
    // accumulate stale staging dirs.
    let result = scaffold_into(&tmp_dir, quiet, out);

    if let Err(e) = result {
        // Best-effort removal -- if this also fails, the caller still sees the
        // original error.
        let _ = fs::remove_dir_all(&tmp_dir);
        return Err(e);
    }

    // Atomic commit: move the staging tree into the final position.
    fs::rename(&tmp_dir, agent.agent_dir())?;

    // Register this project's `.agent/` root with the daemon home (DR-412).
    // Side-effect runs regardless of --quiet; only the stdout line is gated.
    register_project(daemon_home, &project_root)?;

    // .gitignore is an append-style side-effect that lives outside the rename
    // window.  Safe to retry on rerun.
    append_gitignore(&project_root.join(".gitignore"))?;
    if !quiet {
        writeln!(out, "appended .gitignore (1 entry: .agent/.dreamd/)")?;
        // Tilde form is intentional (human-readable; resolved path may differ).
        writeln!(out, "registered .agent/ in ~/.agent/registry.toml")?;
        writeln!(out)?;
        writeln!(out, "{DR413_DISCLOSURE}")?;
    }

    Ok(())
}

/// Remove the current project's entry from `~/.agent/registry.toml`.
///
/// Does NOT delete the project's `.agent/` store. Idempotent: returns
/// `Ok(())` with a benign "nothing to do" message if the project is not
/// registered or the registry file is absent.
pub fn uninstall_project(
    cwd: &Path,
    daemon_home: &Path,
    quiet: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), InitError> {
    let project_root = match find_project_root(cwd) {
        Some(r) => r,
        None => {
            writeln!(
                err,
                "dreamd: error \u{2014} no project root found. Run from inside a project directory (must contain .git/, Cargo.toml, package.json, or pyproject.toml)."
            )?;
            return Err(InitError::NoProjectRoot);
        }
    };

    let daemon = DaemonHome::new(daemon_home);
    let registry_path = daemon.registry_toml();

    // Fast no-op when nothing has ever been registered. Do **not** create
    // `daemon_home` just to take the flock — uninstall must not scaffold
    // `~/.agent/` as a locking side effect. Gate on `registry.toml`, not on
    // `daemon_home.exists()`: the latter is a TOCTOU against a concurrent
    // first `dreamd init` that creates the home between the check and the RMW
    // (AILAB-161 report-back). A false-negative here (init wins the race) is
    // fine — the user re-runs uninstall.
    if !registry_path.exists() {
        if !quiet {
            writeln!(
                out,
                "dreamd: project not registered \u{2014} nothing to do."
            )?;
        }
        return Ok(());
    }

    // Registry present ⇒ its parent exists. Serialize the whole RMW against
    // concurrent `dreamd init` / uninstall. `write_atomic` is durability, not
    // exclusion (AILAB-161). Re-check existence under the lock: a racer may
    // have removed the file between the probe above and `flock`.
    let _guard = lock_exclusive(&daemon.registry_lock())?;
    if !registry_path.exists() {
        if !quiet {
            writeln!(
                out,
                "dreamd: project not registered \u{2014} nothing to do."
            )?;
        }
        return Ok(());
    }

    let raw = fs::read_to_string(&registry_path)?;
    let mut registry: Registry =
        toml::from_str(&raw).map_err(|e| InitError::Io(std::io::Error::other(e)))?;

    let canonical = fs::canonicalize(&project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let canonical_str = canonical.to_string_lossy().into_owned();

    let before = registry.projects.len();
    registry.projects.retain(|p| p.root != canonical_str);

    if registry.projects.len() == before {
        if !quiet {
            writeln!(
                out,
                "dreamd: project not registered \u{2014} nothing to do."
            )?;
        }
        return Ok(());
    }

    let serialized =
        toml::to_string(&registry).map_err(|e| InitError::Io(std::io::Error::other(e)))?;
    write_atomic(&registry_path, serialized.as_bytes())?;
    // Defense-in-depth: registry.toml lists every registered project root.
    // 0600 even though ~/.agent/ is already 0700. Unix-only; Windows perms are
    // deferred to v0.1.1 / DR-121.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&registry_path, std::fs::Permissions::from_mode(0o600))?;
    }

    if !quiet {
        writeln!(out, "unregistered .agent/ from ~/.agent/registry.toml")?;
    }
    Ok(())
}

fn register_project(daemon_home: &Path, project_root: &Path) -> Result<(), InitError> {
    fs::create_dir_all(daemon_home)?;
    let daemon = DaemonHome::new(daemon_home);
    // Serialize the whole read → parse → push → write_atomic → chmod cycle
    // against a concurrent `dreamd init` or `dreamd ... uninstall`, the other
    // writer of this file. Two unlocked cycles read the same snapshot and the
    // later rename silently drops the earlier writer's project root
    // (AILAB-161). `create_dir_all` stays outside the lock — it is the
    // precondition for creating the lockfile at all. Silent: no stdout line,
    // `tests/fixtures/init.golden.txt` is byte-locked.
    let _guard = lock_exclusive(&daemon.registry_lock())?;
    let registry_path = daemon.registry_toml();

    let mut registry: Registry = if registry_path.exists() {
        let raw = fs::read_to_string(&registry_path)?;
        toml::from_str(&raw).map_err(|e| InitError::Io(std::io::Error::other(e)))?
    } else {
        Registry::default()
    };

    let canonical = fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let canonical_str = canonical.to_string_lossy().into_owned();

    if registry.projects.iter().any(|p| p.root == canonical_str) {
        return Ok(());
    }

    registry.projects.push(ProjectEntry {
        root: canonical_str,
    });
    let serialized =
        toml::to_string(&registry).map_err(|e| InitError::Io(std::io::Error::other(e)))?;
    write_atomic(&registry_path, serialized.as_bytes())?;
    // Defense-in-depth: registry.toml lists every registered project root.
    // 0600 even though ~/.agent/ is already 0700. Unix-only; Windows perms are
    // deferred to v0.1.1 / DR-121.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&registry_path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn scaffold_into(tmp: &Path, quiet: bool, out: &mut dyn Write) -> Result<(), InitError> {
    fs::create_dir_all(tmp.join("working"))?;
    writeln!(out, "created .agent/working/")?;
    fs::create_dir_all(tmp.join("episodic"))?;
    writeln!(out, "created .agent/episodic/")?;
    fs::create_dir_all(tmp.join("semantic"))?;
    writeln!(out, "created .agent/semantic/")?;
    fs::create_dir_all(tmp.join("personal"))?;
    writeln!(out, "created .agent/personal/")?;

    fs::File::create(tmp.join("episodic/AGENT_LEARNINGS.jsonl"))?;
    fs::write(tmp.join("working/WORKSPACE.md"), DEFAULT_WORKSPACE_MD)?;

    let dreamd_dir = tmp.join(".dreamd");
    fs::create_dir_all(&dreamd_dir)?;
    let state = State {
        schema_version: "1.0",
        daemon_version: env!("CARGO_PKG_VERSION"),
        last_dream_cycle_at: None,
        last_dream_cycle_status: "idle",
    };
    let json = serde_json::to_string_pretty(&state).expect("state serializes");
    fs::write(dreamd_dir.join("state.json"), json.as_bytes())?;
    if !quiet {
        writeln!(out, "initialized .agent/.dreamd/state.json")?;
    }
    // WEG-14 / DR-112 — silent config template write. No stdout line; the
    // init.golden.txt byte-lock (651 B) forbids adding a line here (D1).
    // Idempotency is enforced by the top-level `.agent/` existence guard.
    fs::write(dreamd_dir.join("config.toml"), CONFIG_TEMPLATE.as_bytes())?;

    Ok(())
}

/// Walk up from `start` looking for a project-root sentinel (`.git/`,
/// `Cargo.toml`, `package.json`, `pyproject.toml`). Shared with
/// `commands::uninstall`, which uses it to decide whether the registry
/// unregister step applies or is a benign skip.
pub(crate) fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut cur: Option<&Path> = Some(start);
    while let Some(dir) = cur {
        for sentinel in ROOT_SENTINELS {
            if dir.join(sentinel).exists() {
                return Some(dir.to_path_buf());
            }
        }
        cur = dir.parent();
    }
    None
}

fn append_gitignore(path: &Path) -> std::io::Result<()> {
    let needs_leading_newline = if path.exists() {
        let mut existing = String::new();
        fs::File::open(path)?.read_to_string(&mut existing)?;
        !existing.is_empty() && !existing.ends_with('\n')
    } else {
        false
    };

    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    if needs_leading_newline {
        f.write_all(b"\n")?;
    }
    f.write_all(GITIGNORE_SNIPPET.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    /// Two concurrent `register_project` calls against one daemon home must
    /// both survive. Unlocked, both threads read the same empty snapshot and
    /// the second `write_atomic` clobbers the first — the lost update the
    /// AILAB-161 registry flock fixes.
    #[cfg(unix)]
    #[test]
    fn concurrent_register_project_keeps_both_roots() {
        let home = tempfile::tempdir().expect("daemon home tempdir");
        let daemon_home = home.path().to_path_buf();

        let projects = tempfile::tempdir().expect("project roots tempdir");
        let root_a = projects.path().join("alpha");
        let root_b = projects.path().join("beta");
        fs::create_dir_all(&root_a).expect("create alpha");
        fs::create_dir_all(&root_b).expect("create beta");

        // Barrier so both threads hit the read-modify-write window together.
        let start = Arc::new(Barrier::new(2));
        let handles: Vec<_> = [root_a.clone(), root_b.clone()]
            .into_iter()
            .map(|root| {
                let daemon_home = daemon_home.clone();
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    register_project(&daemon_home, &root).expect("register_project ok");
                })
            })
            .collect();
        for h in handles {
            h.join().expect("register thread joined");
        }

        let registry_path = DaemonHome::new(&daemon_home).registry_toml();
        let raw = fs::read_to_string(&registry_path).expect("registry.toml written");
        let registry: Registry = toml::from_str(&raw).expect("registry.toml parses");
        let roots: Vec<&str> = registry.projects.iter().map(|p| p.root.as_str()).collect();

        for root in [&root_a, &root_b] {
            let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.clone());
            let canonical = canonical.to_string_lossy().into_owned();
            assert!(
                roots.contains(&canonical.as_str()),
                "lost update: {canonical} missing from {roots:?}"
            );
        }
        assert_eq!(
            roots.len(),
            2,
            "expected exactly both project roots, got {roots:?}"
        );

        // The flock target is a plain neighbour file that is never unlinked.
        assert!(
            DaemonHome::new(&daemon_home).registry_lock().exists(),
            "lockfile must remain on disk after both writers finish"
        );
    }

    /// First-ever `register_project` racing `uninstall_project` against a
    /// daemon home that does not exist yet must not drop the registered root.
    ///
    /// The broken shape gated the flock on `daemon_home.exists()`: uninstall
    /// took `None`, then saw the registry `init` just created and ran the RMW
    /// unlocked — the exact lost-update AILAB-161 closed for init∥init.
    #[cfg(unix)]
    #[test]
    fn concurrent_first_init_and_uninstall_keeps_registered_root() {
        let parent = tempfile::tempdir().expect("parent tempdir");
        // Path must not exist yet — that was the TOCTOU precondition.
        let daemon_home = parent.path().join("agent-home");
        assert!(
            !daemon_home.exists(),
            "precondition: daemon home must be absent before the race"
        );

        let projects = tempfile::tempdir().expect("project roots tempdir");
        let root_a = projects.path().join("alpha");
        let root_b = projects.path().join("beta");
        fs::create_dir_all(&root_a).expect("create alpha");
        fs::create_dir_all(&root_b).expect("create beta");
        fs::write(root_a.join("Cargo.toml"), b"[package]\n").expect("alpha Cargo.toml");
        fs::write(root_b.join("Cargo.toml"), b"[package]\n").expect("beta Cargo.toml");

        let start = Arc::new(Barrier::new(2));
        let reg_home = daemon_home.clone();
        let reg_root = root_a.clone();
        let reg_start = Arc::clone(&start);
        let reg = std::thread::spawn(move || {
            reg_start.wait();
            register_project(&reg_home, &reg_root).expect("register_project ok");
        });

        let un_home = daemon_home.clone();
        let un_cwd = root_b.clone();
        let un_start = Arc::clone(&start);
        let un = std::thread::spawn(move || {
            un_start.wait();
            let mut out = Vec::new();
            let mut err = Vec::new();
            uninstall_project(&un_cwd, &un_home, true, &mut out, &mut err)
                .expect("uninstall_project ok");
        });

        reg.join().expect("register thread joined");
        un.join().expect("uninstall thread joined");

        let registry_path = DaemonHome::new(&daemon_home).registry_toml();
        assert!(
            registry_path.exists(),
            "register must have created registry.toml"
        );
        let raw = fs::read_to_string(&registry_path).expect("registry.toml readable");
        let registry: Registry = toml::from_str(&raw).expect("registry.toml parses");
        let canonical_a = fs::canonicalize(&root_a).unwrap_or_else(|_| root_a.clone());
        let canonical_a = canonical_a.to_string_lossy().into_owned();
        let roots: Vec<&str> = registry.projects.iter().map(|p| p.root.as_str()).collect();
        assert!(
            roots.contains(&canonical_a.as_str()),
            "lost update: registered root missing after init∥uninstall race: {roots:?}"
        );
    }
}
