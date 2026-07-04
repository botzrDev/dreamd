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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn display_covers_all_variants() {
        assert_eq!(
            ResetError::NotFound.to_string(),
            "no .agent/ directory found"
        );
        assert_eq!(
            ResetError::NotATty.to_string(),
            "stdin is not a tty; pass --yes to confirm"
        );
        assert_eq!(ResetError::Declined.to_string(), "reset declined");
        let io = ResetError::from(std::io::Error::other("disk full"));
        assert_eq!(io.to_string(), "disk full");
    }

    #[test]
    fn yes_clears_workspace_to_default() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join(".agent");
        let working = agent.join("working");
        fs::create_dir_all(&working).unwrap();
        let workspace = working.join("WORKSPACE.md");
        fs::write(&workspace, b"scratch notes\n").unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        run_workspace(tmp.path(), true, &mut out, &mut err).unwrap();

        assert_eq!(
            fs::read(&workspace).unwrap(),
            DEFAULT_WORKSPACE_MD.as_bytes()
        );
        let stdout = String::from_utf8(out).unwrap();
        assert!(stdout.contains("reset workspace: cleared "));
        assert!(stdout.contains("WORKSPACE.md"));
        assert!(err.is_empty());
    }

    #[test]
    fn missing_agent_dir_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run_workspace(tmp.path(), true, &mut out, &mut err);
        assert!(matches!(result, Err(ResetError::NotFound)));
        assert!(out.is_empty());
        let stderr = String::from_utf8(err).unwrap();
        assert!(stderr.contains("no .agent/ directory found"));
        assert!(stderr.contains("dreamd init"));
    }

    #[test]
    fn without_yes_on_non_tty_stdin_is_not_a_tty() {
        // Integration / unit tests run with a non-TTY stdin (piped or /dev/null).
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".agent/working")).unwrap();
        fs::write(tmp.path().join(".agent/working/WORKSPACE.md"), b"x\n").unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run_workspace(tmp.path(), false, &mut out, &mut err);
        assert!(matches!(result, Err(ResetError::NotATty)));
        assert!(out.is_empty());
        let stderr = String::from_utf8(err).unwrap();
        assert!(stderr.contains("stdin is not a tty"));
        assert!(stderr.contains("--yes"));
    }
}
