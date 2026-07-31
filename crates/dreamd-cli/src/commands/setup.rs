//! `dreamd setup` — the front door: scaffold `.agent/`, then print the harness
//! wiring next steps (AILAB-549).
//!
//! The scaffold is [`super::init::run`] verbatim, so `dreamd init` stdout stays
//! byte-locked against `tests/fixtures/init.golden.txt` (Clip A): setup wraps
//! init's lines with its own header/footer and never rewrites them.
//!
//! This slice is non-interactive only and writes no MCP config. `--harness` and
//! `--no-write-mcp` are parsed and reported now so the merge writer (AILAB-550)
//! and the TTY wizard (AILAB-551) land without reshuffling clap.

use std::io::Write;
use std::path::Path;

use dreamd_core::{AgentRoot, DaemonHome};

use super::init::{self, find_project_root, InitError};

/// Floating-pin MCP server block printed as copy-paste guidance. `npx -y`
/// re-resolves the `latest` dist-tag on each spawn — never hard-pin here
/// (AGENTS.md `npm-dreamd-mcp-unscoped`).
const MCP_BLOCK: &str = r#"{"mcpServers":{"dreamd":{"command":"npx","args":["-y","dreamd-mcp"]}}}"#;

/// Which harness(es) to wire up. Reported this slice; AILAB-550 turns the
/// choice into an actual `.mcp.json` merge.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Harness {
    Claude,
    Cursor,
    Both,
    None,
}

impl Harness {
    /// Project-relative MCP config files this harness reads (adapter READMEs).
    /// Empty for [`Harness::None`].
    fn config_targets(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &[".mcp.json"],
            Self::Cursor => &[".cursor/mcp.json"],
            Self::Both => &[".mcp.json", ".cursor/mcp.json"],
            Self::None => &[],
        }
    }
}

impl std::fmt::Display for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Claude => "claude",
            Self::Cursor => "cursor",
            Self::Both => "both",
            Self::None => "none",
        };
        f.write_str(s)
    }
}

/// Failure modes for `dreamd setup`. Mirrors [`InitError`] so the exit-code
/// contract (`2` usage / `1` runtime) is unchanged from `dreamd init`.
#[derive(Debug)]
pub enum SetupError {
    NoProjectRoot,
    Io(std::io::Error),
}

impl From<std::io::Error> for SetupError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<InitError> for SetupError {
    fn from(e: InitError) -> Self {
        match e {
            InitError::NoProjectRoot => Self::NoProjectRoot,
            InitError::Io(e) => Self::Io(e),
        }
    }
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoProjectRoot => write!(f, "no project root"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SetupError {}

/// Everything [`run`] needs, injected by the caller. `cli.rs` resolves the real
/// home-derived daemon path; tests pass temp dirs.
pub struct SetupRequest<'a> {
    /// Directory the command was invoked from; the project root is resolved by
    /// walking up from here (same sentinels as `dreamd init`).
    pub cwd: &'a Path,
    /// Daemon home (normally `~/.agent`) holding `registry.toml`.
    pub daemon_home: &'a Path,
    pub harness: Harness,
    /// `false` when `--no-write-mcp` was given.
    pub write_mcp: bool,
    /// `--start-watch` was requested. Reported only — this slice never spawns
    /// the daemon.
    pub start_watch: bool,
    /// Print the plan without writing anything.
    pub dry_run: bool,
}

/// `dreamd setup` entry point. Order: resolve project root → echo the resolved
/// plan → scaffold (or print the dry-run plan) → next steps.
pub fn run(
    req: &SetupRequest<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), SetupError> {
    let project_root = match find_project_root(req.cwd) {
        Some(r) => r,
        None => {
            writeln!(
                err,
                "dreamd: error \u{2014} no project root found. Run from inside a project directory (must contain .git/, Cargo.toml, package.json, or pyproject.toml). If this is a new folder, run `git init` first."
            )?;
            return Err(SetupError::NoProjectRoot);
        }
    };

    writeln!(
        out,
        "dreamd setup \u{2014} project root: {}",
        project_root.display()
    )?;
    writeln!(out, "harness: {}", req.harness)?;
    write_mcp_status(req, out)?;

    if req.dry_run {
        let agent_dir = AgentRoot::new(&project_root).agent_dir();
        if agent_dir.exists() {
            writeln!(
                out,
                "dry-run: {} already exists \u{2014} scaffold would be skipped",
                agent_dir.display()
            )?;
        } else {
            writeln!(out, "dry-run: would scaffold {}", agent_dir.display())?;
            writeln!(
                out,
                "dry-run: would register the project in {}",
                DaemonHome::new(req.daemon_home).registry_toml().display()
            )?;
        }
        writeln!(out, "dry-run: no changes made")?;
    } else {
        // Reuse init verbatim (non-quiet): the scaffold lines and the DR-413
        // privacy disclosure are exactly what a first-run user needs to see.
        init::run(req.cwd, req.daemon_home, false, out, err)?;
    }

    write_next_steps(req, out)?;
    Ok(())
}

/// One line stating what happens to the harness MCP config. No writes this
/// slice — AILAB-550 owns the merge writer.
fn write_mcp_status(req: &SetupRequest<'_>, out: &mut dyn Write) -> std::io::Result<()> {
    let targets = req.harness.config_targets();
    if !req.write_mcp {
        writeln!(out, "mcp config: skipped (--no-write-mcp)")
    } else if targets.is_empty() {
        writeln!(out, "mcp config: skipped (--harness none)")
    } else {
        writeln!(
            out,
            "mcp config: {} \u{2014} not written yet; automatic wiring ships in a follow-up release",
            targets.join(", ")
        )
    }
}

fn write_next_steps(req: &SetupRequest<'_>, out: &mut dyn Write) -> std::io::Result<()> {
    let targets = req.harness.config_targets();
    let show_mcp_step = req.write_mcp && !targets.is_empty();

    writeln!(out)?;
    writeln!(out, "next steps:")?;
    writeln!(
        out,
        "  1. start the shared daemon (one serialized writer): dreamd watch"
    )?;
    if req.start_watch {
        writeln!(
            out,
            "     note: --start-watch does not launch the daemon yet \u{2014} run the command above"
        )?;
    }
    let verify_step = if show_mcp_step {
        writeln!(
            out,
            "  2. add the dreamd MCP server to {}:",
            targets.join(" and ")
        )?;
        writeln!(out, "       {MCP_BLOCK}")?;
        3
    } else {
        2
    };
    writeln!(
        out,
        "  {verify_step}. reload your harness, then ask your agent to call search_nodes"
    )
}
