//! `dreamd memory branch|checkout|branches|delete` — named memory branches (BZR-150).
//! `dreamd memory bisect start|good|bad|run` — search the `snap-*` timeline (BZR-153).
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
//! the socket — they only write refs and HEAD. Bisect checks out through the
//! same function, so it refuses the same way.

use std::io::Write;
use std::path::Path;

use dreamd_core::bisect::{self, BisectError, BisectOutcome};
use dreamd_core::snapshot::{self, SnapshotError};
use dreamd_core::{AgentRoot, LayoutError};

#[derive(Debug)]
pub enum MemoryError {
    /// No `.agent/` store found walking up from `cwd`.
    NotFound,
    /// Branch / checkout / delete refused or failed.
    Snapshot(SnapshotError),
    /// Bisect refused or failed.
    Bisect(BisectError),
    /// `sh` could not be spawned for a bisect script. Nothing was marked.
    Script(std::io::Error),
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

impl From<BisectError> for MemoryError {
    fn from(e: BisectError) -> Self {
        Self::Bisect(e)
    }
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "no .agent/ directory found. Run `dreamd init` first."),
            Self::Snapshot(e) => write!(f, "{e}"),
            Self::Bisect(e) => write!(f, "{e}"),
            Self::Script(e) => write!(f, "could not run bisect script with `sh -c`: {e}"),
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

/// One line, `<name> <id>`, for either outcome.
fn print_outcome(out: &mut dyn Write, outcome: &BisectOutcome) -> std::io::Result<()> {
    let (BisectOutcome::Checkout { name, id } | BisectOutcome::Found { name, id }) = outcome;
    writeln!(out, "{name} {id}")
}

/// Run `script` once as `sh -c <script>` in `cwd`. `Ok(true)` when it exits 0;
/// any other exit (or a signal) is `Ok(false)`, a bad mark. `Err` only when
/// `sh` itself cannot be spawned.
fn run_script(cwd: &Path, script: &str) -> Result<bool, MemoryError> {
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(cwd)
        .status()
        .map_err(MemoryError::Script)?;
    Ok(status.success())
}

/// Run `script`, mark by its exit, and repeat until [`BisectOutcome::Found`].
/// Prints one line per outcome.
fn auto_test_loop(
    cwd: &Path,
    root: &AgentRoot,
    script: &str,
    daemon_socket: Option<&Path>,
    out: &mut dyn Write,
) -> Result<(), MemoryError> {
    loop {
        let good = run_script(cwd, script)?;
        let outcome = bisect::bisect_mark(root, good, daemon_socket)?;
        print_outcome(out, &outcome)?;
        if matches!(outcome, BisectOutcome::Found { .. }) {
            return Ok(());
        }
    }
}

/// `dreamd memory bisect start --good <endpoint> --bad <endpoint>`: check out
/// the midpoint of the `snap-*` range and print `<name> <id>`. With
/// `auto_test`, run the script after each checkout and mark until found.
pub fn run_bisect_start(
    cwd: &Path,
    good: &str,
    bad: &str,
    auto_test: Option<&str>,
    daemon_socket: Option<&Path>,
    out: &mut dyn Write,
) -> Result<(), MemoryError> {
    let root = discover(cwd)?;
    let outcome = bisect::bisect_start(&root, good, bad, daemon_socket)?;
    print_outcome(out, &outcome)?;
    match (auto_test, &outcome) {
        (Some(script), BisectOutcome::Checkout { .. }) => {
            auto_test_loop(cwd, &root, script, daemon_socket, out)
        }
        _ => Ok(()),
    }
}

/// `dreamd memory bisect good|bad`: mark the checked-out step and print the
/// next checkout or the first bad ref.
pub fn run_bisect_mark(
    cwd: &Path,
    mark_good: bool,
    daemon_socket: Option<&Path>,
    out: &mut dyn Write,
) -> Result<(), MemoryError> {
    let root = discover(cwd)?;
    let outcome = bisect::bisect_mark(&root, mark_good, daemon_socket)?;
    print_outcome(out, &outcome)?;
    Ok(())
}

/// `dreamd memory bisect run <script>`: requires a bisect in progress; runs
/// the script once and marks good on exit 0, bad otherwise.
pub fn run_bisect_run(
    cwd: &Path,
    script: &str,
    daemon_socket: Option<&Path>,
    out: &mut dyn Write,
) -> Result<(), MemoryError> {
    let root = discover(cwd)?;
    bisect::ensure_in_progress(&root)?;
    let good = run_script(cwd, script)?;
    let outcome = bisect::bisect_mark(&root, good, daemon_socket)?;
    print_outcome(out, &outcome)?;
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

    /// Three autosnaps over distinct JSONL bytes; returns the ref names.
    fn three_snaps(dir: &Path) -> (AgentRoot, Vec<String>) {
        let root = AgentRoot::new(dir);
        std::fs::create_dir_all(root.episodic_jsonl().parent().unwrap()).unwrap();
        let names = (0..3)
            .map(|i| {
                std::fs::write(root.episodic_jsonl(), format!("{{\"x\":{i}}}\n")).unwrap();
                let now = 1_790_690_580 + 60 * i;
                snapshot::create_autosnap(&root, now).unwrap();
                snapshot::autosnap_ref_name(now).unwrap()
            })
            .collect();
        (root, names)
    }

    #[test]
    fn bisect_start_then_mark_prints_found_line() {
        let dir = tempfile::tempdir().unwrap();
        let (root, names) = three_snaps(dir.path());
        let rows = snapshot::list_branches(&root).unwrap();
        let id_of = |n: &str| rows.iter().find(|r| r.name == n).unwrap().id.clone();

        let mut out = Vec::new();
        run_bisect_start(dir.path(), &names[0], &names[2], None, None, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("{} {}\n", names[1], id_of(&names[1]))
        );

        let mut out = Vec::new();
        run_bisect_mark(dir.path(), false, None, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("{} {}\n", names[1], id_of(&names[1]))
        );
        assert!(matches!(
            run_bisect_mark(dir.path(), true, None, &mut Vec::new()),
            Err(MemoryError::Bisect(BisectError::NotInProgress))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn bisect_run_marks_by_script_exit_and_auto_test_loops() {
        let dir = tempfile::tempdir().unwrap();
        let (_root, names) = three_snaps(dir.path());

        assert!(matches!(
            run_bisect_run(dir.path(), "true", None, &mut Vec::new()),
            Err(MemoryError::Bisect(BisectError::NotInProgress))
        ));

        run_bisect_start(
            dir.path(),
            &names[0],
            &names[2],
            None,
            None,
            &mut Vec::new(),
        )
        .unwrap();
        let mut out = Vec::new();
        run_bisect_run(dir.path(), "exit 3", None, &mut out).unwrap();
        let line = String::from_utf8(out).unwrap();
        assert!(line.starts_with(&format!("{} ", names[1])), "{line}");

        // The script sees the checked-out live file: only `{"x":0}` is good.
        let mut out = Vec::new();
        run_bisect_start(
            dir.path(),
            &names[0],
            &names[2],
            Some("grep -q '\"x\":0' .agent/episodic/AGENT_LEARNINGS.jsonl"),
            None,
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].starts_with(&format!("{} ", names[1])), "{text}");
        assert!(lines[1].starts_with(&format!("{} ", names[1])), "{text}");
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
