//! `dreamd init` — scaffold the per-project `.agent/` store (WEG-9).
//!
//! Output is byte-locked against the `tests/fixtures/init*.golden.txt` files
//! (`tests/init_golden.rs`). Any change to stdout text, ordering, or
//! whitespace updates those fixtures in the same commit. Em-dashes are U+2014
//! (3 bytes).

use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use dreamd_core::config::CONFIG_TEMPLATE;
// Re-exported for `setup` / `uninstall`, which must agree with init on where
// the project is. The walk itself lives in `layout` next to `discover`.
pub(crate) use dreamd_core::layout::find_project_root;
use dreamd_core::privacy::DR413_DISCLOSURE;
use dreamd_core::registry::{update_registry, ProjectEntry};
use dreamd_core::{AgentRoot, DaemonHome, DEFAULT_WORKSPACE_MD, GITIGNORE_SNIPPET};
use serde::Serialize;

const RERUN_MSG: &str = "dreamd: already initialized — .agent/ exists. nothing to do.";
const RERUN_MSG_QUIET: &str = "dreamd: already initialized.";
/// One line when `.agent/` is already on disk and the registry had no entry.
const REREGISTER_MSG: &str = "dreamd: registered existing .agent/ in ~/.agent/registry.toml";

/// Failure modes for init scaffolding and registry operations.
#[derive(Debug)]
pub enum InitError {
    /// No project root above `cwd` (no `.git/`, `Cargo.toml`, `package.json`
    /// or `pyproject.toml`); the message is already on stderr. Exit code 2.
    NoProjectRoot,
    /// Writing output, scaffolding the store, or reading or writing the
    /// registry failed. Exit code 1.
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
/// Idempotent. If `.agent/` already exists and the project is registered,
/// prints the rerun message and returns `Ok(())` without touching the store.
/// If `.agent/` exists but the registry has no entry, registers it, prints
/// one line, and does not rewrite anything under `.agent/`. Staging a new
/// store is done in a temp dir; the final rename is atomic within a single
/// filesystem.
///
/// Returns [`InitError::NoProjectRoot`] if no sentinel (`.git/`,
/// `Cargo.toml`, etc.) is found walking up from `cwd`.
pub fn run(
    cwd: &Path,
    daemon_home: &DaemonHome,
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

    // The store is already there. Register it when the registry lost the
    // entry; otherwise the rerun message, and neither path rewrites `.agent/`.
    if agent.agent_dir().exists() {
        let added = register_project(daemon_home, &project_root)?;
        if added {
            if !quiet {
                writeln!(out, "{REREGISTER_MSG}")?;
            }
        } else {
            writeln!(out, "{}", if quiet { RERUN_MSG_QUIET } else { RERUN_MSG })?;
        }
        return Ok(());
    }

    // Stage the entire scaffold inside a tmp directory so that any failure
    // before the rename leaves .agent/ untouched.  Rename is atomic within a
    // single filesystem, the only case handled here (the staging dir sits in
    // the project root, next to `.agent/`).
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

    // Register this project's `.agent/` root with the daemon home, so the
    // daemon can serve it. Runs regardless of --quiet; only the stdout line
    // is gated.
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
/// Does **not** delete the project's `.agent/` store. Idempotent: returns
/// `Ok(())` with a benign "nothing to do" message if the project is not
/// registered or the registry file is absent.
pub fn uninstall_project(
    cwd: &Path,
    daemon_home: &DaemonHome,
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

    let registry_path = daemon_home.registry_toml();

    // Fast no-op when nothing has ever been registered. Do **not** create
    // `daemon_home` just to take the flock — uninstall must not scaffold
    // `~/.agent/` as a locking side effect. Gate on `registry.toml`, not on
    // `daemon_home.exists()`: the latter is a TOCTOU against a concurrent
    // first `dreamd init` that creates the home between the check and the
    // RMW. A false-negative here (init wins the race) is fine — the user
    // re-runs uninstall.
    //
    // Registry present ⇒ its parent exists, so `update_registry` can take the
    // flock. It re-reads under the lock: a racer that removed the file between
    // this probe and `flock` reads as an empty registry, the retain below
    // changes nothing, and nothing is written.
    let removed = registry_path.exists()
        && update_registry(daemon_home, &project_root, |registry, root| {
            let before = registry.projects.len();
            registry.projects.retain(|p| p.root != root);
            registry.projects.len() != before
        })?;

    if !quiet {
        if removed {
            writeln!(out, "unregistered .agent/ from ~/.agent/registry.toml")?;
        } else {
            writeln!(
                out,
                "dreamd: project not registered \u{2014} nothing to do."
            )?;
        }
    }
    Ok(())
}

/// `true` when this call added the project. Already present is `Ok(false)`
/// and writes nothing. The flock and the re-read live in [`update_registry`].
fn register_project(daemon_home: &DaemonHome, project_root: &Path) -> Result<bool, InitError> {
    // `create_dir_all` stays outside the lock — it is the precondition for
    // creating the lockfile at all. Prints nothing: `run` decides which line,
    // if any, the golden fixtures expect.
    fs::create_dir_all(daemon_home.root())?;
    Ok(update_registry(
        daemon_home,
        project_root,
        |registry, root| {
            if registry.projects.iter().any(|p| p.root == root) {
                return false;
            }
            registry.projects.push(ProjectEntry {
                root: root.to_owned(),
            });
            true
        },
    )?)
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
    // Silent config template write (WEG-14). No stdout line: init.golden.txt
    // locks init's stdout byte for byte. Idempotency is enforced by the
    // top-level `.agent/` existence guard.
    fs::write(dreamd_dir.join("config.toml"), CONFIG_TEMPLATE.as_bytes())?;

    Ok(())
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
    use dreamd_core::registry::Registry;
    use std::sync::{Arc, Barrier};

    /// `init::run` resolves nothing from the environment: the daemon home is
    /// whatever `&DaemonHome` the caller hands it. No `HOME` is set here.
    #[cfg(unix)]
    #[test]
    fn run_registers_into_the_daemon_home_it_is_given() {
        let project = tempfile::tempdir().expect("project tempdir");
        fs::write(project.path().join("Cargo.toml"), b"[package]\n").expect("sentinel");
        let home = tempfile::tempdir().expect("daemon home tempdir");
        let daemon_home = DaemonHome::new(home.path().join("agent-home"));

        let mut out = Vec::new();
        let mut err = Vec::new();
        run(project.path(), &daemon_home, true, &mut out, &mut err).expect("init ok");

        assert!(project.path().join(".agent").is_dir(), "store scaffolded");
        let raw = fs::read_to_string(daemon_home.registry_toml()).expect("registry written");
        let registry: Registry = toml::from_str(&raw).expect("registry parses");
        let canonical = fs::canonicalize(project.path()).expect("canonicalize project");
        assert_eq!(
            registry.projects,
            vec![ProjectEntry {
                root: canonical.to_string_lossy().into_owned()
            }]
        );
    }

    /// Two concurrent `register_project` calls against one daemon home must
    /// both survive. Unlocked, both threads read the same empty snapshot and
    /// the second `write_atomic` clobbers the first — the lost update the
    /// AILAB-161 registry flock fixes.
    #[cfg(unix)]
    #[test]
    fn concurrent_register_project_keeps_both_roots() {
        let home = tempfile::tempdir().expect("daemon home tempdir");
        let daemon_home = DaemonHome::new(home.path());

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

        let registry_path = daemon_home.registry_toml();
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
            daemon_home.registry_lock().exists(),
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
        let daemon_home = DaemonHome::new(parent.path().join("agent-home"));
        assert!(
            !daemon_home.root().exists(),
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

        let registry_path = daemon_home.registry_toml();
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

    fn project_with_store(label: &str) -> (tempfile::TempDir, tempfile::TempDir, DaemonHome) {
        let project = tempfile::tempdir().expect("project");
        fs::write(project.path().join("Cargo.toml"), b"[package]\n").unwrap();
        let agent = project.path().join(".agent/episodic");
        fs::create_dir_all(&agent).unwrap();
        fs::write(agent.join("AGENT_LEARNINGS.jsonl"), b"keep\n").unwrap();
        let home = tempfile::tempdir().expect("home");
        let daemon_home = DaemonHome::new(home.path().join(label));
        (project, home, daemon_home)
    }

    fn registered_roots(daemon_home: &DaemonHome) -> Vec<String> {
        let raw = fs::read_to_string(daemon_home.registry_toml()).expect("registry");
        let registry: Registry = toml::from_str(&raw).expect("parse");
        registry.projects.into_iter().map(|p| p.root).collect()
    }

    #[cfg(unix)]
    #[test]
    fn existing_store_without_a_registry_entry_is_registered() {
        let (project, _home, daemon_home) = project_with_store("reregister");
        let jsonl = project.path().join(".agent/episodic/AGENT_LEARNINGS.jsonl");
        let before = fs::read(&jsonl).unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        run(project.path(), &daemon_home, false, &mut out, &mut err).expect("init");

        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("{REREGISTER_MSG}\n")
        );
        assert!(err.is_empty());
        assert_eq!(
            fs::read(&jsonl).unwrap(),
            before,
            "the store is not rewritten"
        );
        let canonical = fs::canonicalize(project.path()).unwrap();
        assert_eq!(
            registered_roots(&daemon_home),
            vec![canonical.to_string_lossy().into_owned()]
        );

        let mut out = Vec::new();
        run(
            project.path(),
            &daemon_home,
            false,
            &mut out,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), format!("{RERUN_MSG}\n"));
        assert_eq!(registered_roots(&daemon_home).len(), 1);
        assert_eq!(fs::read(&jsonl).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_then_init_registers_without_touching_the_store() {
        let (project, _home, daemon_home) = project_with_store("uninstall-then-init");
        let jsonl = project.path().join(".agent/episodic/AGENT_LEARNINGS.jsonl");
        run(
            project.path(),
            &daemon_home,
            true,
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap();
        // The first call scaffolded nothing (the store was already there) and
        // registered. Uninstall drops the entry. The bytes stay.
        uninstall_project(
            project.path(),
            &daemon_home,
            true,
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap();
        let before = fs::read(&jsonl).unwrap();

        let mut out = Vec::new();
        run(
            project.path(),
            &daemon_home,
            false,
            &mut out,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("{REREGISTER_MSG}\n")
        );
        assert_eq!(fs::read(&jsonl).unwrap(), before);
        assert_eq!(registered_roots(&daemon_home).len(), 1);

        uninstall_project(
            project.path(),
            &daemon_home,
            true,
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap();
        let mut quiet_out = Vec::new();
        run(
            project.path(),
            &daemon_home,
            true,
            &mut quiet_out,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(quiet_out.is_empty(), "quiet re-register prints nothing");
        assert_eq!(fs::read(&jsonl).unwrap(), before);
        assert_eq!(registered_roots(&daemon_home).len(), 1);
    }
}
