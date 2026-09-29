//! `dreamd memory branch|checkout|branches|delete` — named memory branches (BZR-150).
//!
//! Thin wrappers over [`dreamd_core::snapshot`]: the format is
//! `docs/branching.md` (`branches/1.0`). Refs live in
//! `.agent/.dreamd/branches/refs/<name>` and the pointer is `branches/HEAD`.
//! These commands never touch the decay archive directory and never open a
//! Tantivy index.
//!
//! Checkout replaces the live memory files, so it refuses while the daemon
//! socket exists: `dreamd watch` holds the episodic log open and would keep
//! appending to the replaced file. Branch, branches, and delete do not check
//! the socket — they only write refs and HEAD.

use std::io::Write;
use std::path::Path;

use dreamd_core::snapshot::{self, SnapshotError};
use dreamd_core::{AgentRoot, LayoutError};

#[derive(Debug)]
pub enum MemoryError {
    /// No `.agent/` store found walking up from `cwd`.
    NotFound,
    /// Branch / checkout / delete refused or failed.
    Snapshot(SnapshotError),
    /// Failure writing to the `out`/`err` sinks.
    Io(std::io::Error),
}

impl From<std::io::Error> for MemoryError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<SnapshotError> for MemoryError {
    fn from(e: SnapshotError) -> Self {
        Self::Snapshot(e)
    }
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "no .agent/ directory found. Run `dreamd init` first."),
            Self::Snapshot(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for MemoryError {}

fn discover(cwd: &Path) -> Result<AgentRoot, MemoryError> {
    AgentRoot::discover(cwd).map_err(|e| match e {
        LayoutError::NotFound => MemoryError::NotFound,
    })
}

/// `dreamd memory branch <name>`: snapshot the live files, write
/// `refs/<name>`, attach HEAD. Prints the object id.
pub fn run_branch(
    cwd: &Path,
    name: &str,
    now_sec: i64,
    out: &mut dyn Write,
) -> Result<(), MemoryError> {
    let root = discover(cwd)?;
    let id = snapshot::create_branch(&root, name, now_sec)?;
    writeln!(out, "{id}")?;
    Ok(())
}

/// `dreamd memory checkout <name>`: replace the live files from the branch's
/// object and attach HEAD. `daemon_socket` is the daemon's UDS path; when it
/// exists nothing is replaced. Prints the object id.
pub fn run_checkout(
    cwd: &Path,
    name: &str,
    daemon_socket: Option<&Path>,
    out: &mut dyn Write,
) -> Result<(), MemoryError> {
    let root = discover(cwd)?;
    let id = snapshot::checkout(&root, name, daemon_socket)?;
    writeln!(out, "{id}")?;
    Ok(())
}

/// `dreamd memory branches`: one line per ref, `* <name>` for the one HEAD
/// is attached to and `  <name>` otherwise.
pub fn run_branches(cwd: &Path, out: &mut dyn Write) -> Result<(), MemoryError> {
    let root = discover(cwd)?;
    out.write_all(render_branches(&snapshot::list_branches(&root)?).as_bytes())?;
    Ok(())
}

/// `dreamd memory delete <name>`: remove the ref. The object stays on disk.
pub fn run_delete(cwd: &Path, name: &str) -> Result<(), MemoryError> {
    let root = discover(cwd)?;
    snapshot::delete_branch(&root, name)?;
    Ok(())
}

/// Text form of `dreamd memory branches`. Empty for no rows; otherwise every
/// line, the last included, ends in `\n`.
#[must_use]
pub fn render_branches(rows: &[snapshot::BranchRow]) -> String {
    rows.iter()
        .map(|r| format!("{} {}\n", if r.current { "*" } else { " " }, r.name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use snapshot::BranchRow;

    fn row(name: &str, current: bool) -> BranchRow {
        BranchRow {
            name: name.to_string(),
            id: "0".repeat(64),
            current,
        }
    }

    #[test]
    fn render_marks_current_and_ends_with_newline() {
        assert_eq!(render_branches(&[]), "");
        assert_eq!(
            render_branches(&[row("a", false), row("main", true)]),
            "  a\n* main\n"
        );
    }

    #[test]
    fn branch_checkout_and_delete_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        std::fs::create_dir_all(root.episodic_jsonl().parent().unwrap()).unwrap();
        std::fs::write(root.episodic_jsonl(), b"{\"x\":1}\n").unwrap();

        let mut out = Vec::new();
        run_branch(dir.path(), "a", 1_790_690_580, &mut out).unwrap();
        let id = String::from_utf8(out).unwrap();
        assert_eq!(id.trim_end().len(), 64);

        std::fs::write(root.episodic_jsonl(), b"{\"x\":2}\n").unwrap();
        run_branch(dir.path(), "b", 1_790_690_581, &mut Vec::new()).unwrap();

        let sock = dir.path().join("dreamd.sock");
        std::fs::write(&sock, b"").unwrap();
        let err = run_checkout(dir.path(), "a", Some(&sock), &mut Vec::new()).unwrap_err();
        assert!(err.to_string().contains("stop `dreamd watch`"), "{err}");
        assert_eq!(
            std::fs::read(root.episodic_jsonl()).unwrap(),
            b"{\"x\":2}\n"
        );

        let mut out = Vec::new();
        run_checkout(dir.path(), "a", None, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), id);
        assert_eq!(
            std::fs::read(root.episodic_jsonl()).unwrap(),
            b"{\"x\":1}\n"
        );

        run_delete(dir.path(), "b").unwrap();
        let mut out = Vec::new();
        run_branches(dir.path(), &mut out).unwrap();
        let listing = String::from_utf8(out).unwrap();
        assert!(listing.starts_with("* a\n"), "{listing}");
        assert!(!listing.contains(" b\n"), "{listing}");
    }

    #[test]
    fn missing_store_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            run_branches(dir.path(), &mut Vec::new()),
            Err(MemoryError::NotFound)
        ));
    }
}
