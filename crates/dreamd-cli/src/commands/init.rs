//! `dreamd init` — scaffold the per-project `.agent/` store (DR-105 / WEG-9).
//!
//! Output is byte-locked against `tests/fixtures/init.golden.txt` and
//! `tests/fixtures/init.rerun.golden.txt`. Any change to stdout text, ordering,
//! or whitespace must be coordinated with the Clip A beat-sheet
//! (`context/video-scripts/clip-a/`). Em-dashes are U+2014 (3 bytes).

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use dreamd_core::config::CONFIG_TEMPLATE;
use dreamd_core::io::write_atomic;
use dreamd_core::privacy::DR413_DISCLOSURE;
use dreamd_core::registry::{ProjectEntry, Registry};
use dreamd_core::{AgentRoot, DaemonHome, DEFAULT_WORKSPACE_MD, GITIGNORE_SNIPPET};
use serde::Serialize;

const RERUN_MSG: &str = "dreamd: already initialized — .agent/ exists. nothing to do.";
const RERUN_MSG_QUIET: &str = "dreamd: already initialized.";

const ROOT_SENTINELS: &[&str] = &[".git", "Cargo.toml", "package.json", "pyproject.toml"];

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
                "dreamd: error — no project root found. Run from inside a project directory (must contain .git/, Cargo.toml, package.json, or pyproject.toml)."
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

    if !registry_path.exists() {
        if !quiet {
            writeln!(out, "dreamd: project not registered \u{2014} nothing to do.")?;
        }
        return Ok(());
    }

    let raw = fs::read_to_string(&registry_path)?;
    let mut registry: Registry =
        toml::from_str(&raw).map_err(|e| InitError::Io(std::io::Error::other(e)))?;

    let canonical =
        fs::canonicalize(&project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let canonical_str = canonical.to_string_lossy().into_owned();

    let before = registry.projects.len();
    registry.projects.retain(|p| p.root != canonical_str);

    if registry.projects.len() == before {
        if !quiet {
            writeln!(out, "dreamd: project not registered \u{2014} nothing to do.")?;
        }
        return Ok(());
    }

    let serialized =
        toml::to_string(&registry).map_err(|e| InitError::Io(std::io::Error::other(e)))?;
    write_atomic(&registry_path, serialized.as_bytes())?;

    if !quiet {
        writeln!(out, "unregistered .agent/ from ~/.agent/registry.toml")?;
    }
    Ok(())
}

fn register_project(daemon_home: &Path, project_root: &Path) -> Result<(), InitError> {
    fs::create_dir_all(daemon_home)?;
    let registry_path = DaemonHome::new(daemon_home).registry_toml();

    let mut registry: Registry = if registry_path.exists() {
        let raw = fs::read_to_string(&registry_path)?;
        toml::from_str(&raw).map_err(|e| InitError::Io(std::io::Error::other(e)))?
    } else {
        Registry::default()
    };

    let canonical =
        fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
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

fn find_project_root(start: &Path) -> Option<PathBuf> {
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
