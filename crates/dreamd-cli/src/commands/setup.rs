//! `dreamd setup` — the front door: scaffold `.agent/`, then print the harness
//! wiring next steps (AILAB-549).
//!
//! The scaffold is [`super::init::run`] verbatim, so `dreamd init` stdout stays
//! byte-locked against `tests/fixtures/init.golden.txt` (Clip A): setup wraps
//! init's lines with its own header/footer and never rewrites them.
//!
//! Non-interactive only: the harness MCP config is merged by
//! [`super::setup_mcp`] under the ratified AILAB-548 taxonomy; the TTY wizard
//! (AILAB-551) lands later without reshuffling clap.

use std::io::Write;
use std::path::Path;

use dreamd_core::{AgentRoot, DaemonHome};

use super::init::{self, find_project_root, InitError};
use super::setup_mcp::{self, Action, McpError, TargetPlan};

/// Floating-pin MCP server block printed as copy-paste guidance. `npx -y`
/// re-resolves the `latest` dist-tag on each spawn — never hard-pin here
/// (AGENTS.md `npm-dreamd-mcp-unscoped`).
const MCP_BLOCK: &str = r#"{"mcpServers":{"dreamd":{"command":"npx","args":["-y","dreamd-mcp"]}}}"#;

/// Which harness(es) to wire up — i.e. which MCP config files get the
/// `mcpServers.dreamd` block.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Harness {
    Claude,
    Cursor,
    Both,
    None,
}

impl Harness {
    /// Project-relative MCP config files this harness reads (adapter READMEs).
    /// Empty for [`Harness::None`]. The global `~/.cursor/mcp.json` is out of
    /// scope by design (AILAB-548 §2) — `setup` only writes inside the project.
    pub(crate) fn config_targets(self) -> &'static [&'static str] {
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
    /// The harness MCP config could not be written (conflict without `--force`,
    /// malformed existing JSON, or an unwritable path). Details are already on
    /// stderr; `cli.rs` maps this to exit code 1.
    Mcp(McpError),
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
            Self::Mcp(e) => write!(f, "{e}"),
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
    /// `--force`: replace an incompatible `mcpServers.dreamd` entry. Never
    /// overrides a JSON parse failure (AILAB-548 §6C).
    pub force: bool,
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
        wire_mcp(req, &project_root, out, err)?;
        writeln!(out, "dry-run: no changes made")?;
    } else {
        // Reuse init verbatim (non-quiet): the scaffold lines and the DR-413
        // privacy disclosure are exactly what a first-run user needs to see.
        init::run(req.cwd, req.daemon_home, false, out, err)?;
        wire_mcp(req, &project_root, out, err)?;
    }

    write_next_steps(req, out)?;
    Ok(())
}

/// Merge `mcpServers.dreamd` into every harness config target (or report the
/// dry-run plan). Runs after the scaffold so a refusal here never leaves a
/// half-made `.agent/`.
///
/// `--harness both` is all-or-nothing (AILAB-548 §6A): every target is read and
/// classified before any of them is written.
fn wire_mcp(
    req: &SetupRequest<'_>,
    project_root: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), SetupError> {
    let targets = req.harness.config_targets();
    if !req.write_mcp {
        writeln!(out, "mcp config: skipped (--no-write-mcp)")?;
        return Ok(());
    }
    if targets.is_empty() {
        writeln!(out, "mcp config: skipped (--harness none)")?;
        return Ok(());
    }

    let mut plans = Vec::with_capacity(targets.len());
    for label in targets {
        match setup_mcp::plan(project_root, label) {
            Ok(planned) => plans.push(planned),
            // Unparseable config: refuse before writing anything, `--force`
            // included (AILAB-548 §6C).
            Err(e) => return Err(report_mcp_error(e, err)?),
        }
    }

    if req.dry_run {
        for planned in &plans {
            report_dry_run(req, planned, out)?;
        }
        return Ok(());
    }

    if !req.force {
        let blocker = plans.iter().find_map(|planned| match &planned.action {
            Action::Conflict(reason) => Some(McpError::Conflict {
                label: planned.label,
                reason: reason.clone(),
            }),
            _ => None,
        });
        if let Some(e) = blocker {
            return Err(report_mcp_error(e, err)?);
        }
    }

    for planned in &plans {
        if let Err(e) = setup_mcp::apply(planned) {
            return Err(report_mcp_error(e, err)?);
        }
        writeln!(out, "mcp config: {}", applied_status(planned))?;
    }
    Ok(())
}

/// Explain the refusal on stderr, then hand back the error for the exit code.
fn report_mcp_error(e: McpError, err: &mut dyn Write) -> std::io::Result<SetupError> {
    writeln!(err, "dreamd: error \u{2014} {e}")?;
    match &e {
        McpError::Malformed { .. } => writeln!(
            err,
            "dreamd: nothing was written. Fix or remove the file, then re-run \u{2014} --force cannot override a JSON parse error."
        )?,
        McpError::Conflict { .. } => writeln!(
            err,
            "dreamd: nothing was written. Re-run with --force to replace the \"dreamd\" entry (other MCP servers are preserved)."
        )?,
        McpError::Io { .. } => {}
    }
    Ok(SetupError::Mcp(e))
}

/// Status line for a target we just wrote (or deliberately did not).
fn applied_status(planned: &TargetPlan) -> String {
    match &planned.action {
        Action::Create => format!("wrote {}", planned.label),
        Action::Merge => format!("merged into {}", planned.label),
        Action::AlreadyWired => format!("{} already wired \u{2014} no change", planned.label),
        Action::Conflict(reason) => format!(
            "replaced the conflicting \"dreamd\" entry in {} (--force; was: {reason})",
            planned.label
        ),
    }
}

/// Per-target dry-run report: the verdict plus the JSON we would write.
fn report_dry_run(
    req: &SetupRequest<'_>,
    planned: &TargetPlan,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    match &planned.action {
        Action::Create => writeln!(out, "dry-run: would create {}", planned.label)?,
        Action::Merge => writeln!(out, "dry-run: would merge into {}", planned.label)?,
        Action::AlreadyWired => writeln!(
            out,
            "dry-run: {} already wired \u{2014} no change",
            planned.label
        )?,
        Action::Conflict(reason) => {
            writeln!(
                out,
                "dry-run: CONFLICT: {} \u{2014} {reason}",
                planned.label
            )?;
            if req.force {
                writeln!(out, "dry-run: --force would replace the \"dreamd\" entry")?;
            } else {
                // Nothing would be written here, so do not print a document.
                return writeln!(
                    out,
                    "dry-run: re-run with --force to replace the \"dreamd\" entry"
                );
            }
        }
    }
    if let Some(contents) = &planned.contents {
        write!(out, "{contents}")?;
    }
    Ok(())
}

fn write_next_steps(req: &SetupRequest<'_>, out: &mut dyn Write) -> std::io::Result<()> {
    let targets = req.harness.config_targets();
    // Only hand out the copy-paste block when we did NOT write the config
    // ourselves (`--no-write-mcp` / `--harness none`).
    let show_mcp_step = !req.write_mcp || targets.is_empty();

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
        let where_to = if targets.is_empty() {
            "your harness MCP config".to_string()
        } else {
            targets.join(" and ")
        };
        writeln!(out, "  2. add the dreamd MCP server to {where_to}:")?;
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
