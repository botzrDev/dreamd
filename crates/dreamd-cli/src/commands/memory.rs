//! `dreamd memory branch|checkout|branches|delete` — named memory branches (BZR-150).
//! `dreamd memory bisect start|good|bad|run` — search the `snap-*` timeline (BZR-153).
//! `dreamd memory diff <from> <to>` — compare two snapshot objects (BZR-156).
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
//! same function, so it refuses the same way. Diff only reads objects and refs,
//! so it does not check the socket either.

use std::io::Write;
use std::path::Path;

use dreamd_core::bisect::{self, BisectError, BisectOutcome};
use dreamd_core::memory_diff::{self, DiffError, FileChange, MemoryDiff};
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
    /// A diff argument is not a ref, `<name>:<id>`, or a 64-hex object id.
    Resolve(String),
    /// Diff failed reading an object.
    Diff(DiffError),
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

impl From<DiffError> for MemoryError {
    fn from(e: DiffError) -> Self {
        Self::Diff(e)
    }
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "no .agent/ directory found. Run `dreamd init` first."),
            Self::Snapshot(e) => write!(f, "{e}"),
            Self::Bisect(e) => write!(f, "{e}"),
            Self::Script(e) => write!(f, "could not run bisect script with `sh -c`: {e}"),
            Self::Resolve(msg) => write!(f, "{msg}"),
            Self::Diff(e) => write!(f, "{e}"),
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

fn is_object_id(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Line 1 of `refs/<name>`, or `None` when the ref file does not exist.
fn read_ref_line(root: &AgentRoot, name: &str) -> Result<Option<String>, MemoryError> {
    match std::fs::read_to_string(root.branches_refs_dir().join(name)) {
        Ok(text) => Ok(Some(text.lines().next().unwrap_or("").to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(MemoryError::Io(e)),
    }
}

/// Resolve one `dreamd memory diff` argument to an object id:
/// `<name>:<id>` (the ref must name exactly that id), a branch name whose ref
/// exists, or a bare 64-hex id passed through for `diff_objects` to check.
fn resolve_object(root: &AgentRoot, arg: &str) -> Result<String, MemoryError> {
    if let Some((name, id)) = arg.split_once(':') {
        if !snapshot::is_valid_branch_name(name) || !is_object_id(id) {
            return Err(MemoryError::Resolve(format!(
                "{arg:?} is not <name>:<64-hex object id>"
            )));
        }
        return match read_ref_line(root, name)? {
            Some(line) if line == id => Ok(line),
            Some(line) => Err(MemoryError::Resolve(format!(
                "ref {name} names object {line}, not {id}"
            ))),
            None => Err(MemoryError::Resolve(format!(
                "ref {name} not found (expected object {id})"
            ))),
        };
    }
    if snapshot::is_valid_branch_name(arg) {
        if let Some(line) = read_ref_line(root, arg)? {
            return Ok(line);
        }
    }
    if is_object_id(arg) {
        return Ok(arg.to_string());
    }
    Err(MemoryError::Resolve(format!(
        "{arg:?} is not a ref, <name>:<id>, or a 64-hex object id"
    )))
}

fn file_change_str(c: FileChange) -> &'static str {
    match c {
        FileChange::Same => "same",
        FileChange::Added => "added",
        FileChange::Removed => "removed",
        FileChange::Modified => "modified",
    }
}

/// Text form of `dreamd memory diff`: `added`, `removed`, and `salience`
/// lines in [`MemoryDiff`] order, then the three file verdicts. Every line
/// ends in `\n`.
#[must_use]
pub fn render_diff(diff: &MemoryDiff) -> String {
    let mut s = String::new();
    for id in &diff.events_added {
        s.push_str(&format!("added {id}\n"));
    }
    for id in &diff.events_removed {
        s.push_str(&format!("removed {id}\n"));
    }
    for c in &diff.events_salience_changed {
        s.push_str(&format!("salience {} {} {}\n", c.id, c.from, c.to));
    }
    s.push_str(&format!("lessons {}\n", file_change_str(diff.lessons)));
    s.push_str(&format!(
        "preferences {}\n",
        file_change_str(diff.preferences)
    ));
    s.push_str(&format!(
        "recurrence {}\n",
        file_change_str(diff.recurrence)
    ));
    s
}

/// `--unified` dump: a `---`/`+++` header, every line of the `from` object's
/// `LESSONS.md` prefixed `-`, then every line of the `to` one prefixed `+`.
/// Not a line diff; a missing side prints no lines for that side.
fn write_lessons_dump(
    out: &mut dyn Write,
    root: &AgentRoot,
    from_id: &str,
    to_id: &str,
) -> Result<(), MemoryError> {
    const REL: &str = "semantic/LESSONS.md";
    writeln!(out, "--- {from_id}/{REL}")?;
    writeln!(out, "+++ {to_id}/{REL}")?;
    for (id, prefix) in [(from_id, '-'), (to_id, '+')] {
        let path = root.branches_objects_dir().join(id).join(REL);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(MemoryError::Io(e)),
        };
        for line in text.lines() {
            writeln!(out, "{prefix}{line}")?;
        }
    }
    Ok(())
}

/// `dreamd memory diff <from> <to>`: compare two snapshot objects with
/// [`memory_diff::diff_objects`] at `now_sec`. Reads objects and refs only.
/// `json` prints one compact object and ignores `unified`.
pub fn run_diff(
    cwd: &Path,
    from: &str,
    to: &str,
    unified: bool,
    json: bool,
    now_sec: i64,
    out: &mut dyn Write,
) -> Result<(), MemoryError> {
    let root = discover(cwd)?;
    let from_id = resolve_object(&root, from)?;
    let to_id = resolve_object(&root, to)?;
    let diff = memory_diff::diff_objects(&root, &from_id, &to_id, now_sec)?;
    if json {
        let body = serde_json::to_string(&diff).map_err(std::io::Error::other)?;
        writeln!(out, "{body}")?;
        return Ok(());
    }
    out.write_all(render_diff(&diff).as_bytes())?;
    if unified && diff.lessons == FileChange::Modified {
        write_lessons_dump(out, &root, &from_id, &to_id)?;
    }
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

    fn learning_line(id: &str) -> String {
        format!(
            "{{\"schema_version\":\"1.0.0\",\"id\":\"{id}\",\
             \"timestamp\":\"2026-09-28T00:00:00+00:00\",\
             \"pain\":6.0,\"importance\":7.0,\"pinned\":false,\
             \"skill_action\":\"rust::diff\",\"source_harness\":\"test\",\
             \"content\":\"c\"}}\n"
        )
    }

    /// Two autosnaps: the second adds one event and rewrites LESSONS.md.
    /// Returns (root, from ref name, to ref name, from id, to id, added id).
    fn two_objects(dir: &Path) -> (AgentRoot, String, String, String, String, &'static str) {
        const OLD: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FAA";
        const NEW: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FAB";
        let root = AgentRoot::new(dir);
        std::fs::create_dir_all(root.episodic_jsonl().parent().unwrap()).unwrap();
        std::fs::create_dir_all(root.lessons_md().parent().unwrap()).unwrap();
        std::fs::write(root.episodic_jsonl(), learning_line(OLD)).unwrap();
        std::fs::write(root.lessons_md(), "one\ntwo\n").unwrap();
        let t1 = 1_790_690_580;
        let from_id = snapshot::create_autosnap(&root, t1).unwrap();

        let both = format!("{}{}", learning_line(OLD), learning_line(NEW));
        std::fs::write(root.episodic_jsonl(), both).unwrap();
        std::fs::write(root.lessons_md(), "three\n").unwrap();
        let t2 = t1 + 60;
        let to_id = snapshot::create_autosnap(&root, t2).unwrap();
        (
            root,
            snapshot::autosnap_ref_name(t1).unwrap(),
            snapshot::autosnap_ref_name(t2).unwrap(),
            from_id,
            to_id,
            NEW,
        )
    }

    #[test]
    fn diff_prints_text_json_and_unified_dump() {
        let dir = tempfile::tempdir().unwrap();
        let (_root, from, to, from_id, to_id, added) = two_objects(dir.path());
        let now = 1_790_700_000;

        let mut out = Vec::new();
        run_diff(dir.path(), &from, &to, false, false, now, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            format!("added {added}\nlessons modified\npreferences same\nrecurrence same\n")
        );

        // Reverse direction, by bare id and by pinpoint form.
        let mut out = Vec::new();
        let pinned_from = format!("{to}:{to_id}");
        run_diff(
            dir.path(),
            &pinned_from,
            &from_id,
            false,
            false,
            now,
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with(&format!("removed {added}\n")), "{text}");

        let mut out = Vec::new();
        run_diff(dir.path(), &from, &to, true, false, now, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.ends_with(&format!(
                "recurrence same\n--- {from_id}/semantic/LESSONS.md\n\
                 +++ {to_id}/semantic/LESSONS.md\n-one\n-two\n+three\n"
            )),
            "{text}"
        );

        // --json is one object and nothing else; --unified is ignored.
        let mut out = Vec::new();
        run_diff(dir.path(), &from, &to, true, true, now, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.matches('\n').count(), 1, "{text}");
        let v: serde_json::Value = serde_json::from_str(text.trim_end()).unwrap();
        let obj = v.as_object().unwrap();
        for key in [
            "events_added",
            "events_removed",
            "events_salience_changed",
            "lessons",
            "preferences",
            "recurrence",
        ] {
            assert!(obj.contains_key(key), "{key} missing in {text}");
        }
        assert_eq!(v["events_added"][0], added);
        assert_eq!(v["lessons"], "modified");
        assert_eq!(v["preferences"], "same");
    }

    #[test]
    fn diff_pinpoint_mismatch_and_unknown_arg_print_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (_root, from, to, from_id, to_id, _) = two_objects(dir.path());

        let mut out = Vec::new();
        let wrong = format!("{from}:{to_id}");
        let err = run_diff(dir.path(), &wrong, &to, false, false, 0, &mut out).unwrap_err();
        assert!(matches!(err, MemoryError::Resolve(_)), "{err}");
        let msg = err.to_string();
        assert!(msg.contains(&from_id) && msg.contains(&to_id), "{msg}");
        assert!(out.is_empty());

        let mut out = Vec::new();
        let err = run_diff(dir.path(), "no-such-ref", &to, false, false, 0, &mut out).unwrap_err();
        assert!(matches!(err, MemoryError::Resolve(_)), "{err}");
        assert!(out.is_empty());

        // A well-formed id with no object reaches diff_objects.
        let err = run_diff(
            dir.path(),
            &"f".repeat(64),
            &to,
            false,
            false,
            0,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            matches!(err, MemoryError::Diff(DiffError::MissingObject(_))),
            "{err}"
        );
    }

    #[test]
    fn missing_store_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            run_branches(dir.path(), &mut Vec::new()),
            Err(MemoryError::NotFound)
        ));
        let id = "0".repeat(64);
        assert!(matches!(
            run_diff(dir.path(), &id, &id, false, false, 0, &mut Vec::new()),
            Err(MemoryError::NotFound)
        ));
    }
}
