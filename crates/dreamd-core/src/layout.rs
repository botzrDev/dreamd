//! Per-project `.agent/` store + global daemon home (`~/.agent/`) path resolution.
//!
//! DR-101 mandates that every adapter and call site resolve paths through this
//! module rather than concatenating strings. Two roots exist and MUST stay
//! distinct:
//!
//! * [`AgentRoot`] — `<project>/.agent/`. Owned by the project, committed to
//!   git (except `.dreamd/`). Multiple instances coexist across projects.
//! * [`DaemonHome`] — `~/.agent/`. Owned by the user's daemon process. Holds
//!   the unix socket, registry, auth token, the published loopback address
//!   (AILAB-192), and log. Never lives inside a project store.
//!
//! See `context/planning/PRD.md` Part III §1 + Part IV §1.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Errors returned by [`AgentRoot::discover`].
///
/// Kept deliberately small — `NotFound` is the only failure mode the ancestor
/// walk surfaces today. Add variants as new resolver callers land (DR-113 /
/// WEG-15).
#[derive(Debug, Error)]
pub enum LayoutError {
    /// No `.agent/` directory was found in `start` or any of its ancestors.
    #[error("no .agent/ directory found in start path or any ancestor")]
    NotFound,
}

/// Per-project memory store rooted at `<project_root>/.agent/`.
///
/// Cheap to clone; holds only the project root path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentRoot {
    project_root: PathBuf,
}

impl AgentRoot {
    /// Bind to a project root. The `.agent/` directory is the child of this path.
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
        }
    }

    /// Walk ancestors of `start` looking for a directory that contains
    /// `.agent/`, and bind to the first one found.
    ///
    /// This finds an **existing store**. [`find_project_root`] is the other
    /// walk: it finds a repo sentinel (`.git/`, `Cargo.toml`, …) for
    /// `dreamd init`, before any store exists. The two answer different
    /// questions and stay separate.
    ///
    /// Used by post-init commands (e.g. `dreamd reset workspace`, DR-113) that
    /// must operate against the *existing* store rather than the project-root
    /// sentinel set `dreamd init` uses. Symlinks in the walk are followed via
    /// `Path::exists`.
    pub fn discover(start: &Path) -> Result<Self, LayoutError> {
        for dir in start.ancestors() {
            if dir.join(".agent").exists() {
                return Ok(Self::new(dir));
            }
        }
        Err(LayoutError::NotFound)
    }

    /// The project root this store is bound to (the parent of `.agent/`).
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// `<project>/.agent/`
    pub fn agent_dir(&self) -> PathBuf {
        self.project_root.join(".agent")
    }

    /// `<project>/.agent/working/` — short-lived plans and in-flight task context.
    pub fn working_dir(&self) -> PathBuf {
        self.agent_dir().join("working")
    }

    /// `<project>/.agent/episodic/` — append-only event log (JSONL).
    pub fn episodic_dir(&self) -> PathBuf {
        self.agent_dir().join("episodic")
    }

    /// `<project>/.agent/semantic/` — consolidated lessons and preferences.
    pub fn semantic_dir(&self) -> PathBuf {
        self.agent_dir().join("semantic")
    }

    /// `<project>/.agent/personal/` — user-private notes. Excluded from LLM
    /// dream-cycle calls unless `--share-personal` is set.
    pub fn personal_dir(&self) -> PathBuf {
        self.agent_dir().join("personal")
    }

    /// `<project>/.agent/skills/` — reusable skill definitions.
    pub fn skills_dir(&self) -> PathBuf {
        self.agent_dir().join("skills")
    }

    /// `<project>/.agent/protocols/` — collaboration protocols / playbooks.
    pub fn protocols_dir(&self) -> PathBuf {
        self.agent_dir().join("protocols")
    }

    /// `<project>/.agent/.dreamd/` — daemon-private state for this project
    /// (WAL, state.json, cached indexes). Gitignored by `dreamd init` (DR-105).
    pub fn dreamd_dir(&self) -> PathBuf {
        self.agent_dir().join(".dreamd")
    }

    /// `<project>/.agent/episodic/AGENT_LEARNINGS.jsonl` — the canonical
    /// append-only event log. All writes go through the coordinator
    /// (see ARCHITECTURE.md "Load-bearing engineering decisions" §1).
    pub fn episodic_jsonl(&self) -> PathBuf {
        self.episodic_dir().join("AGENT_LEARNINGS.jsonl")
    }

    /// `<project>/.agent/semantic/LESSONS.md` — consolidated lesson output of
    /// the dream cycle. Replaced atomically (DR-104).
    pub fn lessons_md(&self) -> PathBuf {
        self.semantic_dir().join("LESSONS.md")
    }

    /// `<project>/.agent/working/WORKSPACE.md` — short-lived plan + in-flight
    /// task context the agent edits during a session. Replaced atomically.
    pub fn workspace_md(&self) -> PathBuf {
        self.working_dir().join("WORKSPACE.md")
    }

    /// `<project>/.agent/personal/PREFERENCES.md` — user-private preferences.
    /// Excluded from LLM dream-cycle calls unless `--share-personal` is set.
    /// Replaced atomically (DR-104).
    pub fn preferences_md(&self) -> PathBuf {
        self.personal_dir().join("PREFERENCES.md")
    }

    /// `<project>/.agent/personal/DECISIONS.md` — user-private decision log.
    /// Excluded from LLM dream-cycle calls unless `--share-personal` is set.
    /// Replaced atomically (DR-104).
    pub fn decisions_md(&self) -> PathBuf {
        self.personal_dir().join("DECISIONS.md")
    }

    /// `<project>/.agent/.dreamd/state.json` — per-project daemon state.
    ///
    /// Schema lives with DR-106 / WEG-10; the writer is `dreamd init`
    /// (DR-105 / WEG-9), whose locked stdout reads
    /// "initialized .agent/.dreamd/state.json" verbatim against this path.
    /// Distinct from `~/.agent/registry.toml` (DaemonHome) — there is no
    /// daemon-home `state.json` file.
    pub fn state_json(&self) -> PathBuf {
        self.dreamd_dir().join("state.json")
    }

    /// `<project>/.agent/.dreamd/agent.log` — rolling daemon log for this project.
    pub fn agent_log(&self) -> PathBuf {
        self.dreamd_dir().join("agent.log")
    }

    /// `<project>/.agent/.dreamd/dream_in_progress.wal` — WAL for the dream
    /// cycle. Exists only during an in-progress or crashed cycle; its presence
    /// on startup is the signal to run recovery before serving traffic.
    pub fn wal_path(&self) -> PathBuf {
        self.dreamd_dir().join("dream_in_progress.wal")
    }

    /// `<project>/.agent/.dreamd/snapshots/` — archived episodic decay snapshots.
    /// Gitignored via the existing `GITIGNORE_SNIPPET` (`.dreamd/` is excluded).
    pub fn snapshots_dir(&self) -> PathBuf {
        self.dreamd_dir().join("snapshots")
    }

    /// `<project>/.agent/.dreamd/snapshots/<date>.jsonl`
    /// `date` is caller-supplied as `"YYYY-MM-DD"` — no wall-clock calls here.
    pub fn snapshot_file(&self, date: &str) -> PathBuf {
        self.snapshots_dir().join(format!("{date}.jsonl"))
    }

    /// All seven `.agent/` subdirectories, in canonical order. Useful for
    /// scaffolding (`dreamd init`, DR-105) and integrity checks (DR-107).
    pub fn subdirs(&self) -> [PathBuf; 7] {
        [
            self.working_dir(),
            self.episodic_dir(),
            self.semantic_dir(),
            self.personal_dir(),
            self.skills_dir(),
            self.protocols_dir(),
            self.dreamd_dir(),
        ]
    }
}

impl std::fmt::Display for AgentRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.project_root.display())
    }
}

/// Repo sentinels [`find_project_root`] looks for.
const ROOT_SENTINELS: &[&str] = &[".git", "Cargo.toml", "package.json", "pyproject.toml"];

/// Walk up from `start` looking for a project-root sentinel (`.git/`,
/// `Cargo.toml`, `package.json`, `pyproject.toml`).
///
/// This finds a **repo sentinel** for `dreamd init` (and `setup` /
/// `uninstall`, which must agree with init on where the project is). It does
/// not look for `.agent/`; [`AgentRoot::discover`] is the walk that finds an
/// existing store. Do not merge the two.
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
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

/// This process's home directory, or `None` — the one place dreamd reads
/// `HOME` (and, on Windows, `USERPROFILE`). The environment reads live here;
/// the decision lives in [`resolve_home_from_vars`], which is what makes the
/// decision testable on any host.
pub fn home_dir() -> Option<PathBuf> {
    resolve_home_from_vars(
        std::env::var_os("HOME"),
        std::env::var_os("USERPROFILE"),
        cfg!(windows),
    )
}

/// `$HOME`, falling back on Windows **only** to `%USERPROFILE%` when `$HOME`
/// is unset or empty. Native `cmd.exe` and PowerShell set `USERPROFILE` and
/// no `HOME` at all, so without this `dreamd service install` (AILAB-203)
/// would never find `~/.agent` on the one OS whose service path depends on
/// it; Git-Bash and WSL do set `HOME`, and it keeps winning there. Unix
/// behaviour is byte-for-byte what it has always been: `USERPROFILE` is never
/// consulted.
///
/// Driven by a `windows` **argument** rather than a `#[cfg(windows)]` branch
/// on purpose. A cfg-gated fallback would never be compiled, let alone
/// executed, by the Linux checks that gate every merge — a Windows-only path
/// nobody can run is how a Windows-only bug ships. As a parameter, both
/// answers are ordinary unit tests on any host.
///
/// `HOME` set but *empty* is a real, ordinary environment: `env -i`, a
/// systemd unit with `Environment=HOME=`, a Docker `ENV HOME=`, several CI
/// runners. `var_os` returns `Some("")` for it, and every derived path then
/// silently becomes **relative** — `.agent`, `.cache/dreamd-mcp`,
/// `.npm/_npx` — which resolves against whatever directory the command happens
/// to be run from.
///
/// For the AILAB-584 stop scope that is not a cosmetic bug: a relative
/// `cache_dir` is canonicalized against this process's cwd, so a process whose
/// executable sits under `$PWD/.cache/dreamd-mcp` attributes to us and gets
/// SIGTERMed no matter whose `$HOME` it serves — the cross-home kill, re-opened
/// by a blank variable (reproduced under `env HOME= dreamd update`). For
/// `uninstall` it is worse still: `resolve_npx_dir` would hand the scoped npx
/// clear `./.npm/_npx`, and it *deletes* what it finds there.
///
/// Empty is therefore treated exactly like unset everywhere, and — since a
/// blank `%USERPROFILE%` derails a path in precisely the same way — that rule
/// applies to both variables here, not just to `HOME`. Callers already have a
/// defined "no home directory" behaviour (skip with a note, refuse, or exit 2),
/// which is the correct answer for a process that genuinely has no home.
pub fn resolve_home_from_vars(
    home: Option<OsString>,
    userprofile: Option<OsString>,
    windows: bool,
) -> Option<PathBuf> {
    fn non_empty(value: Option<OsString>) -> Option<PathBuf> {
        value.filter(|v| !v.is_empty()).map(PathBuf::from)
    }
    non_empty(home).or_else(|| {
        if windows {
            non_empty(userprofile)
        } else {
            None
        }
    })
}

/// Global daemon home at `~/.agent/`. Holds the unix socket, project registry,
/// auth token, the published loopback address (AILAB-192), and log file. MUST be
/// a separate directory from any project's `.agent/` store — co-locating them
/// would let a project's git history leak the auth token.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DaemonHome {
    home: PathBuf,
}

impl DaemonHome {
    /// Bind to a daemon home directory (normally `~/.agent/`).
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// The daemon home directory itself.
    pub fn root(&self) -> &Path {
        &self.home
    }

    /// `~/.agent/dreamd.sock` — unix domain socket the daemon binds.
    /// Permissions must be `0600` (DR-101 / ARCHITECTURE.md §5).
    pub fn socket_path(&self) -> PathBuf {
        self.home.join("dreamd.sock")
    }

    /// `~/.agent/registry.toml` — known project `.agent/` roots (DR-412).
    pub fn registry_toml(&self) -> PathBuf {
        self.home.join("registry.toml")
    }

    /// `~/.agent/registry.toml.lock` — flock target that serializes the
    /// registry read-modify-write shared by `dreamd init` and
    /// `dreamd ... uninstall` (AILAB-161). Held via
    /// [`io::lock_exclusive`](crate::io::lock_exclusive); never unlinked,
    /// because `flock` is advisory and keyed on the open file description, so
    /// removing the path would let a writer holding the old inode run
    /// concurrently with one that re-created it.
    pub fn registry_lock(&self) -> PathBuf {
        self.home.join("registry.toml.lock")
    }

    /// `~/.agent/auth.json` — bearer token for the Windows TCP fallback.
    pub fn auth_json(&self) -> PathBuf {
        self.home.join("auth.json")
    }

    /// `~/.agent/server.json` — the loopback TCP address the Windows daemon
    /// publishes so clients can find it (AILAB-192).
    ///
    /// Sits next to [`Self::auth_json`] for the same reason: the daemon home is
    /// never inside a project store (see
    /// `daemon_home_is_never_inside_a_project_store`), so neither the token nor
    /// the port can leak into a project's git history.
    ///
    /// Written with `std::fs::write`, **not**
    /// [`io::write_atomic`](crate::io::write_atomic) — that is still
    /// `ErrorKind::Unsupported` off Unix, and this file is a discardable address
    /// hint the daemon rewrites on every bind, not memory state whose torn write
    /// would lose data.
    pub fn server_json(&self) -> PathBuf {
        self.home.join("server.json")
    }

    /// `~/.agent/dreamd.log` — daemon log.
    pub fn log_file(&self) -> PathBuf {
        self.home.join("dreamd.log")
    }
}

/// Snippet appended to a project's `.gitignore` by `dreamd init` (DR-105).
///
/// Only `.dreamd/` is ignored. Episodic, semantic, and personal content are
/// meant to be committed so memory travels with the repo. The leading `/`
/// anchors the pattern at the repo root — a project store is, by definition,
/// at the repo root (DR-101), so we do not want to match a nested
/// `something/.agent/.dreamd/` the user might create elsewhere in the tree.
pub const GITIGNORE_SNIPPET: &str = "/.agent/.dreamd/\n";

/// Byte-exact contents of `working/WORKSPACE.md` as scaffolded by `dreamd init`
/// and re-written by `dreamd reset workspace` (DR-105 / DR-113). One definition
/// of "fresh workspace" — reset equals init for this file.
pub const DEFAULT_WORKSPACE_MD: &str = "Reserved for agent scratch state. The dream cycle does not currently read or write this file. See ROADMAP.md for v0.2 plans.\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> AgentRoot {
        AgentRoot::new("/tmp/proj")
    }

    #[test]
    fn agent_root_path_methods_resolve_under_agent_dir() {
        let r = root();
        let agent = PathBuf::from("/tmp/proj/.agent");
        let cases: &[(&str, PathBuf)] = &[
            ("agent_dir", agent.clone()),
            ("working_dir", agent.join("working")),
            ("episodic_dir", agent.join("episodic")),
            ("semantic_dir", agent.join("semantic")),
            ("personal_dir", agent.join("personal")),
            ("skills_dir", agent.join("skills")),
            ("protocols_dir", agent.join("protocols")),
            ("dreamd_dir", agent.join(".dreamd")),
            (
                "episodic_jsonl",
                agent.join("episodic/AGENT_LEARNINGS.jsonl"),
            ),
            ("lessons_md", agent.join("semantic/LESSONS.md")),
            ("state_json", agent.join(".dreamd/state.json")),
            ("workspace_md", agent.join("working/WORKSPACE.md")),
            ("preferences_md", agent.join("personal/PREFERENCES.md")),
            ("decisions_md", agent.join("personal/DECISIONS.md")),
            ("agent_log", agent.join(".dreamd/agent.log")),
        ];
        for (name, expected) in cases {
            let actual = match *name {
                "agent_dir" => r.agent_dir(),
                "working_dir" => r.working_dir(),
                "episodic_dir" => r.episodic_dir(),
                "semantic_dir" => r.semantic_dir(),
                "personal_dir" => r.personal_dir(),
                "skills_dir" => r.skills_dir(),
                "protocols_dir" => r.protocols_dir(),
                "dreamd_dir" => r.dreamd_dir(),
                "episodic_jsonl" => r.episodic_jsonl(),
                "lessons_md" => r.lessons_md(),
                "state_json" => r.state_json(),
                "workspace_md" => r.workspace_md(),
                "preferences_md" => r.preferences_md(),
                "decisions_md" => r.decisions_md(),
                "agent_log" => r.agent_log(),
                other => panic!("unknown path case: {other}"),
            };
            assert_eq!(actual, *expected, "{name}");
        }
    }

    #[test]
    fn agent_root_display_shows_project_root() {
        assert_eq!(root().to_string(), "/tmp/proj");
    }

    #[test]
    fn subdirs_returns_seven_distinct_canonical_paths() {
        let r = root();
        let dirs = r.subdirs();
        assert_eq!(dirs.len(), 7);
        assert_eq!(dirs[0], r.working_dir());
        assert_eq!(dirs[6], r.dreamd_dir());
        // All distinct.
        for i in 0..dirs.len() {
            for j in (i + 1)..dirs.len() {
                assert_ne!(dirs[i], dirs[j]);
            }
        }
    }

    #[test]
    fn snapshot_file_is_under_dreamd_dir() {
        let r = root();
        assert_eq!(
            r.snapshot_file("2026-05-24"),
            PathBuf::from("/tmp/proj/.agent/.dreamd/snapshots/2026-05-24.jsonl"),
        );
        assert!(r.snapshot_file("2026-05-24").starts_with(r.dreamd_dir()));
    }

    #[test]
    fn agent_root_accepts_relative_paths() {
        let r = AgentRoot::new("relative/proj");
        assert_eq!(r.agent_dir(), PathBuf::from("relative/proj/.agent"));
    }

    #[test]
    fn daemon_home_resolves_all_files() {
        let h = DaemonHome::new("/home/u/.agent");
        assert_eq!(h.root(), Path::new("/home/u/.agent"));
        assert_eq!(h.socket_path(), PathBuf::from("/home/u/.agent/dreamd.sock"));
        assert_eq!(
            h.registry_toml(),
            PathBuf::from("/home/u/.agent/registry.toml"),
        );
        assert_eq!(h.auth_json(), PathBuf::from("/home/u/.agent/auth.json"));
        // AILAB-192: the published loopback address lives beside the token, in
        // the daemon home — never in a project store.
        assert_eq!(h.server_json(), PathBuf::from("/home/u/.agent/server.json"));
        assert_eq!(h.log_file(), PathBuf::from("/home/u/.agent/dreamd.log"));
    }

    #[test]
    fn daemon_home_is_never_inside_a_project_store() {
        // Invariant: the daemon's home must not be nested under any project's
        // `.agent/`. Co-locating them risks leaking auth.json into git history.
        let proj = AgentRoot::new("/work/proj-a");
        let daemon = DaemonHome::new("/home/u/.agent");
        assert!(!daemon.root().starts_with(proj.agent_dir()));
        assert!(!daemon.root().starts_with(proj.project_root()));
    }

    #[test]
    fn discover_finds_agent_dir_at_start() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".agent")).unwrap();
        let root = AgentRoot::discover(tmp.path()).expect("discover");
        assert_eq!(root.project_root(), tmp.path());
    }

    #[test]
    fn discover_walks_ancestors_until_agent_dir_is_found() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".agent")).unwrap();
        let nested = tmp.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        let root = AgentRoot::discover(&nested).expect("discover");
        assert_eq!(root.project_root(), tmp.path());
    }

    #[test]
    fn discover_returns_not_found_when_no_ancestor_has_agent_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(matches!(
            AgentRoot::discover(&nested),
            Err(LayoutError::NotFound)
        ));
    }

    #[test]
    fn empty_home_is_unset_and_userprofile_only_counts_on_windows() {
        // Unix never consults USERPROFILE, so an empty HOME is no home at all.
        assert_eq!(
            resolve_home_from_vars(
                Some(OsString::new()),
                Some(OsString::from("/tmp/up")),
                false
            ),
            None
        );
        // Windows falls back to USERPROFILE because HOME is empty…
        assert_eq!(
            resolve_home_from_vars(Some(OsString::new()), Some(OsString::from("/tmp/up")), true),
            Some(PathBuf::from("/tmp/up"))
        );
        // …and only then: a non-empty HOME still wins.
        assert_eq!(
            resolve_home_from_vars(
                Some(OsString::from("/tmp/home")),
                Some(OsString::from("/tmp/up")),
                true
            ),
            Some(PathBuf::from("/tmp/home"))
        );
    }

    #[test]
    fn gitignore_snippet_targets_only_dreamd_subdir() {
        // Anchored at repo root with leading `/` so we never match a nested
        // `something/.agent/.dreamd/` the user might create elsewhere.
        assert!(GITIGNORE_SNIPPET.contains("/.agent/.dreamd/"));
        assert!(GITIGNORE_SNIPPET
            .lines()
            .all(|l| l.is_empty() || l.starts_with('/')));
        // Must NOT ignore the whole .agent/ tree — episodic/semantic/personal
        // are meant to be committed so memory travels with the repo.
        assert!(!GITIGNORE_SNIPPET.lines().any(|l| {
            let t = l.trim();
            t == ".agent" || t == ".agent/" || t == "/.agent" || t == "/.agent/"
        }));
        // Trailing newline so successive appends don't run together.
        assert!(GITIGNORE_SNIPPET.ends_with('\n'));
    }
}
