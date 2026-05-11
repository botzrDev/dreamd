//! `dreamd init` — scaffold the per-project `.agent/` store (DR-105 / WEG-9).
//!
//! Output is byte-locked against `tests/fixtures/init.golden.txt` and
//! `tests/fixtures/init.rerun.golden.txt`. Any change to stdout text, ordering,
//! or whitespace must be coordinated with the Clip A beat-sheet
//! (`context/video-scripts/clip-a/`). Em-dashes are U+2014 (3 bytes).

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use dreamd_core::{AgentRoot, GITIGNORE_SNIPPET};
use serde::Serialize;

/// PRD §5 privacy disclosure, ASCII-rendered, 60-col wrapped (locked verbatim).
/// WEG-17 (Sat 5/16) relocates this constant; the stdout text does not change.
pub const DR413_DISCLOSURE: &str = "\
dreamd: first run — When LLM mode is enabled, the content
of AGENT_LEARNINGS.jsonl entries meeting the salience
threshold is sent to the configured LLM provider. No data
is sent in --no-llm mode. Users working with sensitive
codebases should use --no-llm or a local model via Ollama.
The personal/ layer is excluded from LLM calls unless
--share-personal is passed.
See docs/security.md#privacy-disclosure for details.";

const RERUN_MSG: &str = "dreamd: already initialized — .agent/ exists. nothing to do.";

const WORKSPACE_MD: &str = "Reserved for agent scratch state. The dream cycle does not currently read or write this file. See ROADMAP.md for v0.2 plans.\n";

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
    _daemon_home: &Path,
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

    if agent.agent_dir().exists() {
        writeln!(out, "{RERUN_MSG}")?;
        return Ok(());
    }

    fs::create_dir_all(agent.working_dir())?;
    writeln!(out, "created .agent/working/")?;
    fs::create_dir_all(agent.episodic_dir())?;
    writeln!(out, "created .agent/episodic/")?;
    fs::create_dir_all(agent.semantic_dir())?;
    writeln!(out, "created .agent/semantic/")?;
    fs::create_dir_all(agent.personal_dir())?;
    writeln!(out, "created .agent/personal/")?;

    fs::create_dir_all(agent.skills_dir())?;
    fs::create_dir_all(agent.protocols_dir())?;

    fs::File::create(agent.episodic_jsonl())?;
    fs::write(agent.working_dir().join("WORKSPACE.md"), WORKSPACE_MD)?;

    fs::create_dir_all(agent.dreamd_dir())?;
    let state = State {
        schema_version: "1.0",
        daemon_version: env!("CARGO_PKG_VERSION"),
        last_dream_cycle_at: None,
        last_dream_cycle_status: "idle",
    };
    let json = serde_json::to_string_pretty(&state).expect("state serializes");
    let state_path = agent.state_json();
    let tmp_path = state_path.with_extension("json.tmp");
    fs::write(&tmp_path, json.as_bytes())?;
    fs::rename(&tmp_path, &state_path)?;
    writeln!(out, "initialized .agent/.dreamd/state.json")?;

    append_gitignore(&project_root.join(".gitignore"))?;
    writeln!(out, "appended .gitignore (1 entry: .agent/.dreamd/)")?;

    // Sprint 1: emit the line; real registry write lands with DR-412.
    // Stdout always shows tilde-form regardless of the resolved daemon home.
    writeln!(out, "registered .agent/ in ~/.agent/registry.toml")?;

    writeln!(out)?;
    writeln!(out, "{DR413_DISCLOSURE}")?;

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
