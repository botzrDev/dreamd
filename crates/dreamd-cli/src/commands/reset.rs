//! `dreamd reset workspace` — clear `working/WORKSPACE.md` (DR-113 / WEG-15).
//!
//! Reframed from the original `POST /api/v1/session/end`: no harness actually
//! invoked that route. This is now a manual scratchpad-clear users run at
//! session boundaries; real session-model lifecycle ships in v0.2.
//!
//! Resolution walks ancestors of `cwd` for `.agent/` via
//! [`AgentRoot::discover`] (post-init resolver, distinct from the project-root
//! sentinel set `dreamd init` uses). The written content is byte-identical to
//! what `dreamd init` scaffolds — one definition of "fresh workspace" lives in
//! `dreamd_core::layout::DEFAULT_WORKSPACE_MD`.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use dreamd_core::io::write_atomic;
use dreamd_core::{AgentRoot, LayoutError, DEFAULT_WORKSPACE_MD};

#[derive(Debug)]
pub enum ResetError {
    NotFound,
    NotATty,
    Declined,
    Io(std::io::Error),
}

impl From<std::io::Error> for ResetError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl std::fmt::Display for ResetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "no .agent/ directory found"),
            Self::NotATty => write!(f, "stdin is not a tty; pass --yes to confirm"),
            Self::Declined => write!(f, "reset declined"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ResetError {}

/// `dreamd reset workspace [--yes]` entry point.
///
/// `yes` skips the interactive confirmation. Without it, prompts on stderr and
/// reads one line from stdin; refuses non-TTY stdin to avoid silent
/// destructive ops in pipelines.
pub fn run_workspace(
    cwd: &Path,
    yes: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), ResetError> {
    let root = match AgentRoot::discover(cwd) {
        Ok(r) => r,
        Err(LayoutError::NotFound) => {
            writeln!(
                err,
                "dreamd: error — no .agent/ directory found. Run `dreamd init` first."
            )?;
            return Err(ResetError::NotFound);
        }
    };

    let target = root.workspace_md();

    if !yes {
        if !std::io::stdin().is_terminal() {
            writeln!(err, "error: stdin is not a tty; pass --yes to confirm")?;
            return Err(ResetError::NotATty);
        }
        write!(err, "Reset WORKSPACE.md at {}? [y/N] ", target.display())?;
        err.flush()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        let answer = line.trim();
        if !(answer == "y" || answer == "Y") {
            writeln!(err, "aborted")?;
            return Err(ResetError::Declined);
        }
    }

    write_atomic(&target, DEFAULT_WORKSPACE_MD.as_bytes())?;
    writeln!(out, "reset workspace: cleared {}", target.display())?;
    Ok(())
}
