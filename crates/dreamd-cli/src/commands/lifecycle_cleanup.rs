//! Shared lifecycle-cleanup helpers for `dreamd uninstall` / `dreamd update`
//! (AILAB-226).
//!
//! Scoped process stop, daemon-socket removal, and cache-tree clearing. Every
//! path is injected by the caller: `cli.rs` resolves the real home-derived
//! locations ($DREAMD_SOCK / `DaemonHome::socket_path()` for the socket,
//! `$HOME` and `~/.cache/dreamd-mcp` for the stop scope), tests pass temp dirs
//! — nothing in this module resolves `~` on its own, so tests never touch the
//! real `~/.agent`, `~/.cache`, or npx cache.
//!
//! **The stop pass is no longer machine-global (AILAB-584).** It used to shell
//! out to the manual README recipe's bare `pkill -f 'dreamd mcp'` / `pkill -f
//! 'dreamd watch'`, which signals *every* matching process on the box
//! regardless of whose home it serves. The 2026-08-03 clean-box audit caught
//! that (AILAB-554 F1): a sandboxed `HOME=/tmp/… dreamd update` SIGTERMed the
//! developer's real Cursor `dreamd mcp`. The pass now enumerates candidates,
//! keeps the ones that are the `dreamd` binary ([`is_dreamd_binary`]) *and*
//! whose full argv matches [`STOP_PATTERNS`], **attributes** each to this
//! invocation's [`StopScope`], and signals only those. When attribution says no
//! — including when the attribution data is unreadable (another user's
//! `/proc/<pid>/environ` is EACCES) — the process is left running with a stderr
//! warning and [`StopReport::spared`] set. Failing to stop a daemon is
//! recoverable in one command; killing another home's daemon is not.
//!
//! Three of those guards came out of the adversarial review of the first cut,
//! and each has a test below: the argv substring alone made candidates of
//! shells and `grep`s that merely quote `dreamd watch`; [`attributable`] took
//! its scope on faith, so an empty or relative `cache_dir` (what
//! `$HOME/.cache/dreamd-mcp` collapses to under an *empty* `$HOME`) attributed
//! foreign processes via `Path::starts_with`; and a spared process was reported
//! to the command modules as "nothing was running".

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Full-argv substrings identifying a dreamd server process. Matched against
/// the whole command line — the `pkill -f` semantics of the manual recipe,
/// unchanged by AILAB-584 as the *match rule*. What changed is that the
/// matched set is then filtered by [`StopScope`] attribution before anything
/// is signalled. Deliberately two-token patterns: a bare `dreamd` would match
/// unrelated invocations — including this very process, whose own argv
/// (`dreamd uninstall` / `dreamd update`) matches neither entry.
pub const STOP_PATTERNS: [&str; 2] = ["dreamd mcp", "dreamd watch"];

/// Outcome of a best-effort [`stop_local_servers`] pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StopReport {
    /// Whether a stop was attempted at all (`false` on non-unix platforms).
    ///
    /// Stays `true` when the pass ran but signalled nothing — including the
    /// unresolvable-scope case. `update.rs` prints "automatic stop is
    /// unix-only in v0.1" on `!attempted`, which would be a false statement
    /// on a unix box that simply had nothing attributable to stop.
    pub attempted: bool,
    /// Whether at least one in-scope `dreamd mcp` / `dreamd watch` process was
    /// actually signalled. A process that matched [`STOP_PATTERNS`] but was
    /// *not* attributable to this [`StopScope`] does not count — it is still
    /// running, so claiming otherwise would misreport the restart contract.
    pub matched: bool,
    /// Whether at least one candidate was **left running** — refused because it
    /// was not attributable to this [`StopScope`] (the normal case), or
    /// signalled unsuccessfully. Either way it is still up, and every one is
    /// named on stderr with its own reason.
    ///
    /// Without this field `attempted && !matched` is ambiguous between "nothing
    /// was running" and "something was running and we did not stop it", and
    /// both command modules printed the former on exactly the scenario
    /// AILAB-584 exists for: stdout claiming "no local `dreamd mcp` /
    /// `dreamd watch` processes were running" directly above a stderr warning
    /// naming the pid that is still running. `matched` and `spared` are
    /// independent — a pass can stop this home's `watch` and spare another
    /// home's `mcp` in the same sweep.
    pub spared: bool,
}

/// Injectable stop hook. `cli.rs` passes a closure that captures this
/// invocation's [`StopScope`] and calls [`stop_local_servers`]; tests pass a
/// fake so the suite never signals a real process (a live `dreamd watch` on a
/// dev machine must survive `cargo test`).
///
/// This alias deliberately did **not** grow a scope parameter in AILAB-584.
/// Capturing the scope in the production closure keeps `update.rs`,
/// `uninstall.rs`, and the fake stoppers in `tests/update_cmd.rs` /
/// `tests/uninstall_cmd.rs` untouched by the scoping fix — the seam stays
/// "give me somewhere to write warnings, tell me what happened".
pub type Stopper<'a> = &'a mut dyn FnMut(&mut dyn Write) -> StopReport;

/// The locations that make a running `dreamd mcp` / `dreamd watch` process
/// *ours* for this invocation (AILAB-584).
///
/// Resolved by `cli.rs` from the same `$HOME` the rest of the command already
/// uses, and injected — this module never reads the environment itself, so a
/// test can scope a pass at a tempdir. Either field being `None` just drops
/// that rule; both `None` means nothing is attributable and the pass signals
/// nothing at all (see [`stop_local_servers`]).
///
/// This is a plain public struct with no constructor, so a field can also be
/// `Some` and *useless* — an empty or relative path. [`attributable`] rejects
/// those itself rather than trusting the caller; see its doc comment.
///
/// **No `socket` field, by decision.** The ticket says "`/proc/<pid>/environ`
/// `HOME=` matches, **and/or** open socket" — one attribution signal is
/// enough, and `home` already subsumes the socket case: a daemon started with
/// a custom `$DREAMD_SOCK` still inherits this `$HOME`, so the HOME rule
/// catches it, while a daemon under a *different* `$HOME` is precisely what we
/// must not signal. Adding a socket field would mean walking `/proc/<pid>/fd`
/// for no additional coverage.
pub struct StopScope<'a> {
    /// This invocation's `$HOME`. A candidate matches when its own `HOME=`
    /// environment entry resolves to the same directory.
    pub home: Option<&'a Path>,
    /// The resolved npm-shim native cache (`~/.cache/dreamd-mcp`). A candidate
    /// matches when its executable lives under this tree — that binary was
    /// installed by *this* home's shim, whatever `$HOME` it is now running
    /// with.
    pub cache_dir: Option<&'a Path>,
}

/// Linux process table root. Threaded through [`stop_scoped`] as a parameter
/// rather than hardcoded so unit tests can point the scanner at a synthetic
/// tree in a tempdir and drive the whole pass without a real process.
#[cfg(unix)]
const PROC_ROOT: &str = "/proc";

/// A running process that matched [`STOP_PATTERNS`] **and** is the `dreamd`
/// binary (see [`is_dreamd_binary`]), plus the two facts [`attributable`] needs
/// to decide whether it is ours.
///
/// `exe` and `home` are `Option` because both are routinely unreadable:
/// `/proc/<pid>/exe` and `/proc/<pid>/environ` are EACCES for another user's
/// process, and the macOS `ps` fallback only sees the environment when the
/// kernel permits it. Unreadable is not "matches" — it fails attribution and
/// the process is left alone.
#[cfg(unix)]
#[derive(Debug, Clone)]
struct Candidate {
    pid: u32,
    argv: String,
    exe: Option<PathBuf>,
    home: Option<PathBuf>,
}

/// Best-effort terminate the local `dreamd mcp` / `dreamd watch` processes
/// that belong to `scope`.
///
/// Enumerates the process table, keeps entries that are the `dreamd` binary
/// ([`is_dreamd_binary`]) with a full argv matching [`STOP_PATTERNS`], and
/// signals only those [`attributable`] to `scope`. Anything else that matched
/// is left running with a stderr warning naming the pid — see
/// the module header for why "don't kill when unsure" is the ratified rule.
/// The current process is skipped explicitly, and its own argv
/// (`dreamd uninstall` / `dreamd update`) matches no pattern anyway. Nothing
/// to stop is success (idempotent); a missing or failing `kill` degrades to a
/// stderr warning, never an error — the same posture the old `pkill` path had.
#[cfg(unix)]
pub fn stop_local_servers(scope: &StopScope<'_>, err: &mut dyn Write) -> StopReport {
    let mut kill: fn(u32) -> std::io::Result<()> = signal_term;
    stop_scoped(Path::new(PROC_ROOT), scope, &mut kill, err)
}

/// Non-unix: no-op. Process stop is unix-only in v0.1 — Windows detached
/// lifecycle is AILAB-203's scope. `scope` is accepted so both arms share one
/// signature for `cli.rs`.
#[cfg(not(unix))]
pub fn stop_local_servers(scope: &StopScope<'_>, err: &mut dyn Write) -> StopReport {
    let _ = scope;
    let _ = writeln!(
        err,
        "dreamd: note — stopping dreamd mcp/watch processes is unix-only in v0.1; stop them manually."
    );
    StopReport {
        attempted: false,
        matched: false,
        spared: false,
    }
}

/// The whole scoped stop pass, with both of its side-effecting edges injected:
/// `proc_root` (the process table to scan) and `kill` (how a pid is
/// signalled). [`stop_local_servers`] passes [`PROC_ROOT`] and
/// [`signal_term`]; tests pass a synthetic proc tree and a recorder closure,
/// so the AILAB-554 regression is covered without signalling anything real.
#[cfg(unix)]
fn stop_scoped(
    proc_root: &Path,
    scope: &StopScope<'_>,
    kill: &mut dyn FnMut(u32) -> std::io::Result<()>,
    err: &mut dyn Write,
) -> StopReport {
    let mut report = StopReport {
        attempted: true,
        matched: false,
        spared: false,
    };

    // Neither rule can fire, so every matching process would have to be taken
    // on faith — which is exactly the machine-global behaviour AILAB-584
    // removed. Say so up front, then fall through to the ordinary loop:
    // `attributable` rejects every candidate anyway, so each one is spared *by
    // name* and `report.spared` stays honest. Returning early instead would
    // hand `update.rs` an `attempted && !matched && !spared` report, which it
    // prints as "no local `dreamd mcp` / `dreamd watch` processes were running"
    // — a claim this branch has no evidence for. `attempted` stays true: the
    // pass ran, it just had no scope to run against.
    if scope.home.is_none() && scope.cache_dir.is_none() {
        let _ = writeln!(
            err,
            "dreamd: warning — no `dreamd mcp` / `dreamd watch` process was stopped: \
             this invocation's $HOME and native cache could not be resolved, so no \
             running process can be attributed to it. Stop them manually if needed."
        );
    }

    for candidate in collect_candidates(proc_root, scope) {
        let label = matched_pattern(&candidate.argv).unwrap_or("dreamd");
        if !attributable(candidate.exe.as_deref(), candidate.home.as_deref(), scope) {
            report.spared = true;
            // Describes *this invocation's* scope and nothing about the spared
            // process beyond its pid and which pattern it matched. Printing the
            // foreign process's `HOME=` or executable path (as this warning did
            // before AILAB-584 phase 3) both mislabelled the values — the
            // format string said "this $HOME" and passed the *other* home — and
            // handed every user on a shared box a partial inventory of every
            // other user's dreamd servers.
            let _ = writeln!(
                err,
                "dreamd: warning — left `{label}` pid {pid} running: not attributable to \
                 this invocation's $HOME ({home}) or native binary cache ({cache}), so it \
                 may belong to another home or user. Stop it yourself if it is yours.",
                pid = candidate.pid,
                home = describe_scope(scope.home),
                cache = describe_scope(scope.cache_dir),
            );
            continue;
        }
        match kill(candidate.pid) {
            Ok(()) => report.matched = true,
            Err(e) => {
                // A signal that did not land leaves the process running just as
                // surely as a refusal does, so it counts as `spared` too —
                // otherwise the caller prints "no local `dreamd mcp` /
                // `dreamd watch` processes were running" above this very
                // warning. The stderr line carries the specific reason; the
                // report only says "something is still up".
                report.spared = true;
                let _ = writeln!(
                    err,
                    "dreamd: warning — could not stop `{label}` pid {} ({e}); \
                     stop it manually if still running.",
                    candidate.pid,
                );
            }
        }
    }
    report
}

/// Render one half of *this invocation's* [`StopScope`] for a warning line, so
/// a `$HOME`-less environment reads as a stated fact rather than a blank.
///
/// "unresolved" and not "unreadable": the value being described is our own
/// resolution of `$HOME` / `~/.cache/dreamd-mcp`, never anything read out of
/// another process. The pre-phase-3 wording rendered `this $HOME (unreadable)`
/// from a *candidate's* unreadable `/proc/<pid>/environ`, which read as "your
/// HOME is unreadable" — a statement about the user's own box that was never
/// true.
#[cfg(unix)]
fn describe_scope(path: Option<&Path>) -> String {
    match path {
        Some(p) => p.display().to_string(),
        None => "unresolved".to_string(),
    }
}

/// `true` iff a process's full argv (arguments joined by spaces) contains one
/// of the [`STOP_PATTERNS`] — the same rule the manual `pkill -f` recipe
/// applies, now used to pick *candidates* rather than to pick kill targets.
#[cfg(unix)]
fn argv_matches(joined_argv: &str) -> bool {
    matched_pattern(joined_argv).is_some()
}

/// Which [`STOP_PATTERNS`] entry `joined_argv` matched, if any. Used to label
/// warnings `dreamd watch` / `dreamd mcp` instead of echoing a full argv — an
/// MCP server's command line carries `--project-root <path>` and is unbounded.
#[cfg(unix)]
fn matched_pattern(joined_argv: &str) -> Option<&'static str> {
    STOP_PATTERNS
        .iter()
        .copied()
        .find(|pattern| joined_argv.contains(pattern))
}

/// `true` iff a matched process belongs to `scope` and may therefore be
/// signalled. Pure: all three inputs are already-read facts, so the decision
/// is unit-testable without a process table.
///
/// Either rule is sufficient:
/// - the executable lives under the resolved native cache — that binary was
///   installed by this home's npm shim, so stopping it is in scope even if it
///   is running with a different `$HOME`; or
/// - the process's own `HOME=` resolves to this invocation's `$HOME` — which
///   also covers a daemon on a custom `$DREAMD_SOCK` and a cargo-installed
///   `dreamd watch` whose exe is nowhere near the shim cache.
///
/// Neither holding — including "we could not read the process's exe or
/// environ" — is a deliberate `false`. See the module header.
///
/// **A degenerate scope field is rejected here, not just at the caller.**
/// [`StopScope`] is a plain public struct anyone can build, and both rules go
/// wrong on a path that is empty or relative:
/// - `Path::starts_with("")` is `true` for *every* path, so an empty
///   `cache_dir` attributes the whole process table;
/// - a relative `cache_dir` (e.g. `.cache/dreamd-mcp`, what
///   `$HOME/.cache/dreamd-mcp` collapses to when `$HOME` is set but empty) is
///   canonicalized by [`path_starts_with`] against *this process's* cwd, so a
///   foreign process whose exe happens to sit under `$PWD/.cache/dreamd-mcp`
///   attributes. That is the AILAB-584 cross-home kill re-opened by a blank
///   environment variable, and it was reproduced under `env HOME= dreamd
///   update` before this guard existed.
///
/// `cli.rs` already filters an empty `$HOME` to `None` at the one place it
/// reads the environment (`home_dir`), so on the production path these
/// conditions never fire. That redundancy is deliberate: the library must be
/// safe for a future caller that resolves its scope some other way, and this is
/// the last checkpoint before a `SIGTERM`.
///
/// A relative `home` is *not* rejected, and does not need to be: the home rule
/// compares whole paths ([`same_path`]) rather than testing a prefix, so a
/// relative scope home can only ever attribute a process whose own `HOME=` is
/// that same path. It cannot widen to a tree the way `starts_with` does.
#[cfg(unix)]
fn attributable(exe: Option<&Path>, proc_home: Option<&Path>, scope: &StopScope<'_>) -> bool {
    if let (Some(cache), Some(exe)) = (scope.cache_dir, exe) {
        if !cache.as_os_str().is_empty() && cache.is_absolute() && path_starts_with(exe, cache) {
            return true;
        }
    }
    if let (Some(scope_home), Some(proc_home)) = (scope.home, proc_home) {
        if !scope_home.as_os_str().is_empty() && same_path(proc_home, scope_home) {
            return true;
        }
    }
    false
}

/// `true` iff the process really is the `dreamd` executable, rather than
/// something that merely *mentions* a [`STOP_PATTERNS`] string in its command
/// line.
///
/// The argv match keeps the manual recipe's `pkill -f` semantics — a substring
/// test over the whole joined command line — so on its own it makes candidates
/// out of
/// `sh -c 'sleep 30 # dreamd watch'`, `grep -n "dreamd watch" log`, an editor
/// opened on a file whose name contains the phrase, and the interactive shell
/// that ran `dreamd watch; dreamd update` (its own cmdline carries the whole
/// line). Every one of those is attributable — same `$HOME`, same user — so
/// attribution cannot save them; before AILAB-584 phase 3 they were signalled.
///
/// The rule is the **file name** of the resolved `exe`, or the file name of
/// `argv[0]`. Either is enough, which is what makes this incapable of sparing a
/// real daemon:
/// - a same-user `dreamd watch` always has a readable `/proc/<pid>/exe` whose
///   basename is `dreamd`, whatever tree it was installed into;
/// - `setup::spawn_watch` spawns `Command::new(std::env::current_exe()?)` with
///   `.arg("watch")`, so `argv[0]` is the absolute path of the dreamd binary
///   and its basename is `dreamd` too — verified by reading `spawn_watch`;
/// - a binary replaced in place (the `dreamd update` path) has `/proc/<pid>/exe`
///   reading `…/dreamd (deleted)`, and `argv[0]` still carries the plain name.
///
/// A process the exe of which we cannot read *and* whose `argv[0]` is not
/// `dreamd` could never have been signalled anyway: with `/proc/<pid>/exe`
/// unreadable the cache rule cannot fire, and `/proc/<pid>/environ` is
/// unreadable under the same EACCES, so the home rule cannot either.
#[cfg(unix)]
fn is_dreamd_binary(exe: Option<&Path>, argv0: Option<&str>) -> bool {
    fn named_dreamd(path: &Path) -> bool {
        path.file_name().is_some_and(|name| name == "dreamd")
    }
    exe.is_some_and(named_dreamd) || argv0.is_some_and(|arg| named_dreamd(Path::new(arg)))
}

/// Prefix test that sees through symlinks (`/var` → `/private/var` on macOS,
/// a symlinked `$HOME` on any box) by canonicalizing both sides. Falls back to
/// a literal `starts_with` when either side does not resolve — during
/// `dreamd update` the cache tree may already be gone, and a deleted-on-disk
/// executable never canonicalizes.
#[cfg(unix)]
fn path_starts_with(path: &Path, prefix: &Path) -> bool {
    if let (Ok(path), Ok(prefix)) = (path.canonicalize(), prefix.canonicalize()) {
        if path.starts_with(prefix) {
            return true;
        }
    }
    path.starts_with(prefix)
}

/// Same-directory test, canonicalizing both sides for the symlinked-`$HOME`
/// case. A literal match short-circuits so the common path costs no syscalls
/// and a home that no longer exists still compares equal to itself.
#[cfg(unix)]
fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

/// Send `SIGTERM` to `pid`.
///
/// Shells out to `kill(1)` rather than calling `libc::kill`: the workspace
/// sets `unsafe_code = "forbid"` (root `Cargo.toml`, `[workspace.lints.rust]`)
/// and the single ratified exception is `detach_double_fork` in `dreamd-core`.
/// A stop is best-effort, so paying one `fork`/`exec` per in-scope daemon —
/// there is normally at most one `watch` and one `mcp` — is the cheap trade.
/// Output is captured so `kill`'s own diagnostics do not interleave with the
/// command's stderr; the caller prints one warning instead.
#[cfg(unix)]
fn signal_term(pid: u32) -> std::io::Result<()> {
    let out = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .output()?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(std::io::Error::other(if stderr.is_empty() {
        format!("kill -TERM {pid} exited with {}", out.status)
    } else {
        stderr
    }))
}

/// Platform dispatch for candidate enumeration. Linux reads `/proc` (the
/// tested path); other unixes shell out to `ps`. Each arm ignores the input
/// the other needs.
#[cfg(target_os = "linux")]
fn collect_candidates(proc_root: &Path, scope: &StopScope<'_>) -> Vec<Candidate> {
    let _ = scope;
    collect_candidates_in(proc_root)
}

/// See the Linux arm. macOS has no `/proc`, so the process table comes from
/// `ps` and `scope` is needed to spot the `HOME=` entry inside the combined
/// args+env string.
#[cfg(all(unix, not(target_os = "linux")))]
fn collect_candidates(proc_root: &Path, scope: &StopScope<'_>) -> Vec<Candidate> {
    let _ = proc_root;
    collect_candidates_via_ps(scope)
}

/// Scan a Linux-shaped process table rooted at `proc_root` for `dreamd`
/// processes whose full argv matches [`STOP_PATTERNS`], reading each one's
/// executable and `HOME=` for attribution.
///
/// `proc_root` is a parameter purely for testability: production passes
/// [`PROC_ROOT`], tests build `<tmp>/<pid>/{cmdline,environ,exe}` by hand.
///
/// Skips this process (a `dreamd update` must never signal itself), any entry
/// whose `cmdline` is unreadable or empty — kernel threads have an empty
/// `cmdline`, and a command line we cannot read cannot be matched — and
/// anything that is not the `dreamd` binary ([`is_dreamd_binary`]), which is
/// what keeps a shell or a `grep` that merely quotes `dreamd watch` out of the
/// candidate set. Sorted by pid so warning output is deterministic.
#[cfg(target_os = "linux")]
fn collect_candidates_in(proc_root: &Path) -> Vec<Candidate> {
    let mut found = Vec::new();
    let self_pid = std::process::id();
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return found;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let dir = entry.path();
        let Some(args) = read_nul_separated(&dir.join("cmdline")) else {
            continue;
        };
        let argv = args.join(" ");
        if !argv_matches(&argv) {
            continue;
        }
        // `read_link` returns the recorded target even if it no longer exists
        // (an upgraded-in-place binary), which is what we want.
        let exe = std::fs::read_link(dir.join("exe")).ok();
        if !is_dreamd_binary(exe.as_deref(), args.first().map(String::as_str)) {
            continue;
        }
        let home = read_nul_separated(&dir.join("environ")).and_then(|env| {
            env.iter()
                .find_map(|kv| kv.strip_prefix("HOME=").map(PathBuf::from))
        });
        found.push(Candidate {
            pid,
            argv,
            exe,
            home,
        });
    }
    found.sort_by_key(|candidate| candidate.pid);
    found
}

/// Read a `/proc` NUL-separated list (`cmdline`'s argv, `environ`'s `K=V`
/// pairs). `None` when the file is unreadable — EACCES on another user's
/// process, or ESRCH when the pid exited between `read_dir` and here. Empty
/// trailing segments are dropped; both files end with a NUL.
#[cfg(target_os = "linux")]
fn read_nul_separated(path: &Path) -> Option<Vec<String>> {
    let raw = std::fs::read(path).ok()?;
    Some(
        raw.split(|byte| *byte == 0)
            .filter(|segment| !segment.is_empty())
            .map(|segment| String::from_utf8_lossy(segment).into_owned())
            .collect(),
    )
}

/// macOS (and other non-Linux unixes) candidate scan: one
/// `ps -Eww -ax -o pid=,args=` call.
///
/// `-E` appends the process environment to the args column, so a single string
/// per line carries both the argv we match on and the `HOME=` we attribute on;
/// `-ww` disables truncation so a long MCP command line survives. When `ps -E`
/// is not permitted for a process the environment simply is not there, so
/// attribution fails and the process is skipped with a warning — the correct
/// safe degradation. Linux is the tested path; this arm is deliberately
/// minimal.
#[cfg(all(unix, not(target_os = "linux")))]
fn collect_candidates_via_ps(scope: &StopScope<'_>) -> Vec<Candidate> {
    let mut found = Vec::new();
    let self_pid = std::process::id();
    let Ok(out) = std::process::Command::new("ps")
        .args(["-Eww", "-ax", "-o", "pid=,args="])
        .output()
    else {
        return found;
    };
    let table = String::from_utf8_lossy(&out.stdout).into_owned();
    for line in table.lines() {
        let Some((pid_field, rest)) = line.trim_start().split_once(char::is_whitespace) else {
            continue;
        };
        let Ok(pid) = pid_field.parse::<u32>() else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let argv = rest.trim_start().to_string();
        if !argv_matches(&argv) {
            continue;
        }
        // The first whitespace token is this arm's `argv[0]`; `ps` gives no
        // separate exe column. Same rule as the Linux arm ([`is_dreamd_binary`]):
        // a shell or a `grep` that merely quotes `dreamd watch` is not a
        // candidate, however attributable it looks.
        let argv0 = argv.split_whitespace().next();
        if !is_dreamd_binary(None, argv0) {
            continue;
        }
        found.push(Candidate {
            pid,
            // argv[0] is the exec path for a shim-launched binary, which is
            // enough for the native-cache rule. A relative argv[0] is dropped
            // rather than guessed at.
            exe: argv0
                .filter(|token| token.starts_with('/'))
                .map(PathBuf::from),
            home: ps_scope_home(&argv, scope),
            argv,
        });
    }
    found.sort_by_key(|candidate| candidate.pid);
    found
}

/// Find `HOME=<scope home>` inside a `ps -E` args+env string.
///
/// Matches the scope's home *literally* and requires a space (or the end of
/// the string) on both sides, rather than parsing the value up to the next
/// space — that way a home containing a space still attributes correctly.
/// `None` means "this line does not prove the process runs under our `$HOME`",
/// which fails attribution rather than assuming.
#[cfg(all(unix, not(target_os = "linux")))]
fn ps_scope_home(joined: &str, scope: &StopScope<'_>) -> Option<PathBuf> {
    let home = scope.home?;
    let needle = format!("HOME={}", home.display());
    let bytes = joined.as_bytes();
    let mut from = 0usize;
    while let Some(offset) = joined[from..].find(&needle) {
        let start = from + offset;
        let end = start + needle.len();
        let left_bounded = start == 0 || bytes[start - 1] == b' ';
        let right_bounded = end == bytes.len() || bytes[end] == b' ';
        if left_bounded && right_bounded {
            return Some(home.to_path_buf());
        }
        from = end;
    }
    None
}

/// Outcome of [`remove_socket`].
#[derive(Debug)]
pub enum SocketRemoval {
    /// A socket file existed and was unlinked.
    Removed,
    /// Nothing to unlink — benign, the socket was already gone.
    NotFound,
    /// A daemon answered on the socket, so it was left in place. Callers warn
    /// and continue; see [`remove_socket`] for why this is not an error.
    RefusedLive,
    /// A real unlink failure (e.g. permission denied). Callers warn on
    /// stderr with the path and continue — an unremovable socket must not
    /// abort the wider uninstall/update flow.
    Failed(std::io::Error),
}

/// Unlink the daemon socket if present, refusing while a daemon still answers
/// on it.
///
/// The liveness guard is the point of this function. Both callers
/// (`uninstall`, `update`) run it *after* a best-effort [`stop_local_servers`]
/// pass whose failure is only a stderr warning — so on a box with no `kill`,
/// where the signal is denied, or where the daemon could not be attributed to
/// this [`StopScope`] and was deliberately spared, it is still serving.
/// Unlinking then
/// leaves it alive on an orphaned inode: it keeps working, but every client
/// resolves a path that no longer exists, so MCP silently falls back to the
/// in-process Phase 1 backend and cross-harness recall stops with no error.
/// Refusing is the same rule `dreamd archive` and `dreamd doctor --repair`
/// already apply before touching a live daemon's state.
///
/// `NotFound` is the only error kind treated as benign; any other unlink
/// failure is reported as [`SocketRemoval::Failed`] so callers can print an
/// actionable warning instead of a false "no daemon socket" line.
///
/// The probe is not instantaneous: it waits out a daemon that is merely
/// *stopping*. See [`STOP_GRACE`].
pub fn remove_socket(socket: &Path) -> SocketRemoval {
    remove_socket_within(socket, STOP_GRACE)
}

/// How long a signalled daemon gets to finish unwinding before the socket is
/// called live.
///
/// Without this window the guard misfires on the *common* upgrade path.
/// `kill` returns once the signal is **delivered**, not once the target
/// exits, and `dreamd watch` handles SIGTERM gracefully: it drains the
/// coordinator and indexer (a Tantivy commit, cadence
/// `DEFAULT_COMMIT_CADENCE` = 5s) and only then unlinks its own socket. The
/// listener therefore stays bound and answering for the whole drain, while
/// `remove_socket` runs microseconds after the signal lands. Probing once
/// would report a daemon that is already shutting down as live, and
/// `dreamd update` with a running `watch` — the path the 2026-08-03 clean-box
/// audit exercised as R6 — would print a "still live" warning telling the user
/// to stop a daemon that is mid-exit.
///
/// Matched to the commit cadence because that drain is the slow step. A daemon
/// that is still answering after this window is genuinely not going away
/// (no `kill` on the box, the signal was denied, or the daemon was spared as
/// unattributable), which is the case the guard exists to catch. Costs nothing
/// when the socket is already dead or
/// absent: the wait only starts once a probe has come back live.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// Re-probe interval while waiting out a stopping daemon.
const STOP_POLL: Duration = Duration::from_millis(50);

/// [`remove_socket`] with an injectable grace window so tests can pin the
/// refuse path (`Duration::ZERO`) without waiting out [`STOP_GRACE`].
fn remove_socket_within(socket: &Path, grace: Duration) -> SocketRemoval {
    if !wait_until_quiet(socket, grace) {
        return SocketRemoval::RefusedLive;
    }
    match std::fs::remove_file(socket) {
        Ok(()) => SocketRemoval::Removed,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SocketRemoval::NotFound,
        Err(e) => SocketRemoval::Failed(e),
    }
}

/// Poll until nothing answers on `socket`, or `grace` elapses. `true` means
/// the socket is safe to unlink; `false` means a daemon outlasted the window.
fn wait_until_quiet(socket: &Path, grace: Duration) -> bool {
    if !socket_is_live(socket) {
        return true;
    }
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        std::thread::sleep(STOP_POLL);
        if !socket_is_live(socket) {
            return true;
        }
    }
    false
}

/// Connect-probe the daemon socket. Delegates to the one canonical probe so
/// this agrees with `dreamd status`, `dreamd doctor`, `dreamd archive`, and
/// MCP backend selection rather than becoming a fifth opinion.
#[cfg(unix)]
fn socket_is_live(socket: &Path) -> bool {
    dreamd_core::server::is_daemon_socket_live(socket)
}

/// Non-unix: there is no UDS daemon in v0.1, so there is nothing live to
/// protect. Matches the `cfg(not(unix))` stance in `status` and `setup`.
#[cfg(not(unix))]
fn socket_is_live(_socket: &Path) -> bool {
    false
}

/// Attach the offending path so `dreamd: error — …` output is actionable
/// (e.g. permission denied names the tree the user must fix). Shared by the
/// uninstall and update cache-clear steps.
pub fn io_with_path(e: std::io::Error, path: &Path) -> std::io::Error {
    std::io::Error::new(
        e.kind(),
        format!("{} (while clearing {})", e, path.display()),
    )
}

/// Remove a whole cache tree.
///
/// `Ok(true)` when the tree existed and was removed; `Ok(false)` when it was
/// already absent (idempotent). Other I/O errors (e.g. permission denied)
/// are real failures and propagate to the caller for an actionable message.
pub fn clear_dir_tree(dir: &Path) -> std::io::Result<bool> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Scoped npx-cache clear: delete only entries that
/// [`entry_is_dreamd_mcp_npx_install`] recognises as dreamd-mcp installs —
/// the same scoping as the README manual recipe. Never wipes unrelated
/// packages; the blanket wipe is gated behind `dreamd uninstall --all-npx`
/// (see `commands::uninstall`). A missing cache root is `Ok` with nothing
/// removed. Returns the removed entry paths.
pub fn clear_scoped_npx(npx_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    let entries = match std::fs::read_dir(npx_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(removed),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if entry_is_dreamd_mcp_npx_install(&path) {
            std::fs::remove_dir_all(&path)?;
            removed.push(path);
        }
    }
    Ok(removed)
}

/// `true` iff the npx cache entry at `entry` is a dreamd-mcp install.
///
/// npm writes `~/.npm/_npx/<hash>/package.json` with NO top-level `name` —
/// the requested package is recorded as a dependencies key, e.g.
/// `{"dependencies":{"dreamd-mcp":"^0.1.0-rc.6"}}`. An entry matches when
/// any of:
/// - the top-level manifest has `"name": "dreamd-mcp"` (fallback), or
/// - its `"dependencies"` object contains the exact key `"dreamd-mcp"`
///   (precise: a tool that merely depends on dreamd-mcp gets its own name
///   as the sole dependency key), or
/// - `<entry>/node_modules/dreamd-mcp/package.json` exists with
///   `"name": "dreamd-mcp"`.
///
/// Unreadable or malformed manifests are a non-match (skip, never delete on
/// doubt).
fn entry_is_dreamd_mcp_npx_install(entry: &Path) -> bool {
    if let Some(value) = read_package_json(&entry.join("package.json")) {
        if value.get("name").and_then(|n| n.as_str()) == Some("dreamd-mcp") {
            return true;
        }
        if value
            .get("dependencies")
            .and_then(|d| d.as_object())
            .is_some_and(|deps| deps.contains_key("dreamd-mcp"))
        {
            return true;
        }
    }
    read_package_json(
        &entry
            .join("node_modules")
            .join("dreamd-mcp")
            .join("package.json"),
    )
    .is_some_and(|v| v.get("name").and_then(|n| n.as_str()) == Some("dreamd-mcp"))
}

/// Parse `package_json` as JSON. Unreadable or malformed files are `None` —
/// callers treat that as a non-match.
fn read_package_json(package_json: &Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(package_json).ok()?;
    serde_json::from_str(&raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// The argv match rule is the manual `pkill -f` recipe's rule and must
    /// stay two-token: `dreamd uninstall` / `dreamd update` — this very
    /// process during a stop pass — must never look like a server.
    #[cfg(unix)]
    #[test]
    fn argv_matches_only_the_two_token_server_patterns() {
        assert!(argv_matches("dreamd watch"));
        assert!(argv_matches(
            "/home/a/.cache/dreamd-mcp/0.1.0/dreamd mcp --project-root /home/a/demo"
        ));
        assert!(!argv_matches("dreamd"));
        assert!(!argv_matches("dreamd status"));
        assert!(!argv_matches("dreamd uninstall"));
        assert!(!argv_matches("dreamd update --restart"));
        assert_eq!(matched_pattern("dreamd watch --x"), Some("dreamd watch"));
        assert_eq!(matched_pattern("dreamd recall x"), None);
    }

    /// [`attributable`] is the whole safety rule, so pin each arm directly:
    /// either signal is sufficient, and *no* signal — including unreadable
    /// attribution data — is a refusal.
    #[cfg(unix)]
    #[test]
    fn attribution_needs_the_cache_rule_or_the_home_rule() {
        let ours = Path::new("/home/ours");
        let cache = Path::new("/home/ours/.cache/dreamd-mcp");
        let scope = StopScope {
            home: Some(ours),
            cache_dir: Some(cache),
        };

        // HOME rule alone (cargo-installed binary, our home).
        assert!(attributable(
            Some(Path::new("/home/ours/.cargo/bin/dreamd")),
            Some(ours),
            &scope
        ));
        // Cache rule alone (our shim's binary, running under another home).
        assert!(attributable(
            Some(Path::new("/home/ours/.cache/dreamd-mcp/0.1.0/dreamd")),
            Some(Path::new("/home/theirs")),
            &scope
        ));
        // Neither.
        assert!(!attributable(
            Some(Path::new("/usr/local/bin/dreamd")),
            Some(Path::new("/home/theirs")),
            &scope
        ));
        // Nothing readable is a refusal, not a default-allow.
        assert!(!attributable(None, None, &scope));
        // An unset scope field simply drops that rule.
        let home_only = StopScope {
            home: Some(ours),
            cache_dir: None,
        };
        assert!(!attributable(
            Some(Path::new("/home/ours/.cache/dreamd-mcp/0.1.0/dreamd")),
            None,
            &home_only
        ));
    }

    /// An **empty** `cache_dir` must attribute nothing. `Path::starts_with("")`
    /// is `true` for every path, so without the guard in [`attributable`] a
    /// `StopScope { cache_dir: Some("") }` would authorise a signal to every
    /// matching process on the box — the machine-global behaviour AILAB-584
    /// removed, reachable again through one blank environment variable.
    /// Deliberately redundant with `cli.rs`'s `home_dir()` filter: this is the
    /// last checkpoint before a SIGTERM.
    #[cfg(unix)]
    #[test]
    fn an_empty_cache_dir_attributes_nothing() {
        let scope = StopScope {
            home: None,
            cache_dir: Some(Path::new("")),
        };
        assert!(!attributable(
            Some(Path::new("/home/theirs/.cache/dreamd-mcp/0.1.0/dreamd")),
            Some(Path::new("/home/theirs")),
            &scope
        ));
        assert!(!attributable(
            Some(Path::new("/usr/bin/dreamd")),
            None,
            &scope
        ));
    }

    /// A **relative** `cache_dir` must attribute nothing either. This is the
    /// exact shape `$HOME/.cache/dreamd-mcp` collapses to when `HOME` is set but
    /// empty (`.cache/dreamd-mcp`), and [`path_starts_with`] canonicalizes it
    /// against *this process's* cwd — so a foreign process whose executable
    /// happens to live under `$PWD/.cache/dreamd-mcp` would attribute. The
    /// reviewer reproduced that with `env HOME= dreamd update`.
    #[cfg(unix)]
    #[test]
    fn a_relative_cache_dir_attributes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let exe = root.join(".cache/dreamd-mcp/1.0/dreamd");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"#!/bin/false\n").unwrap();

        // Absolute: the ordinary cache rule fires, proving the fixture is a
        // real match and the relative case below fails for the right reason.
        let cache = root.join(".cache/dreamd-mcp");
        let absolute = StopScope {
            home: None,
            cache_dir: Some(&cache),
        };
        assert!(attributable(Some(&exe), None, &absolute));

        let relative = StopScope {
            home: None,
            cache_dir: Some(Path::new(".cache/dreamd-mcp")),
        };
        assert!(
            !attributable(Some(&exe), None, &relative),
            "a relative cache dir must never attribute — it resolves against cwd"
        );
    }

    /// An empty `home` must attribute nothing: a process with no readable
    /// `HOME=` yields `None` and is already refused, but a process that
    /// genuinely reports `HOME=` (empty) would otherwise compare equal to an
    /// empty scope home and be signalled.
    #[cfg(unix)]
    #[test]
    fn an_empty_home_attributes_nothing() {
        let scope = StopScope {
            home: Some(Path::new("")),
            cache_dir: None,
        };
        assert!(!attributable(
            Some(Path::new("/usr/bin/dreamd")),
            Some(Path::new("")),
            &scope
        ));
        assert!(!attributable(
            Some(Path::new("/usr/bin/dreamd")),
            Some(Path::new("/home/theirs")),
            &scope
        ));
    }

    /// The argv rule keeps the manual recipe's `pkill -f` semantics — a
    /// substring test over the whole command line — so on its own it matches
    /// things that are not dreamd at
    /// all. Pin [`is_dreamd_binary`] directly: either basename vouches for the
    /// process, and a shell does not become one by quoting the phrase.
    #[cfg(unix)]
    #[test]
    fn only_the_dreamd_binary_can_be_a_candidate() {
        // exe alone (argv[0] rewritten or absent).
        assert!(is_dreamd_binary(
            Some(Path::new("/home/a/.cache/dreamd-mcp/0.1.0/dreamd")),
            Some("something-else")
        ));
        // argv[0] alone — the `dreamd update` in-place case, where
        // /proc/<pid>/exe reads `…/dreamd (deleted)`.
        assert!(is_dreamd_binary(
            Some(Path::new("/home/a/.cargo/bin/dreamd (deleted)")),
            Some("dreamd")
        ));
        assert!(is_dreamd_binary(None, Some("/usr/local/bin/dreamd")));
        // A shell that merely mentions the pattern is neither.
        assert!(!is_dreamd_binary(
            Some(Path::new("/bin/dash")),
            Some("/bin/sh")
        ));
        assert!(!is_dreamd_binary(
            Some(Path::new("/usr/bin/grep")),
            Some("grep")
        ));
        assert!(!is_dreamd_binary(None, None));
        // Prefix/suffix near-misses are not the file name `dreamd`.
        assert!(!is_dreamd_binary(None, Some("dreamd-mcp")));
        assert!(!is_dreamd_binary(Some(Path::new("/opt/mydreamd")), None));
    }

    /// Build one `<root>/<pid>/` entry of a synthetic Linux process table:
    /// NUL-separated `cmdline`, NUL-separated `environ` (omitted entirely when
    /// `home` is `None`, standing in for an EACCES read), and `exe` as a real
    /// symlink — the three files [`collect_candidates_in`] reads.
    #[cfg(target_os = "linux")]
    fn write_proc_entry(root: &Path, pid: u32, argv: &[&str], home: Option<&str>, exe: &Path) {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        let mut cmdline = Vec::new();
        for arg in argv {
            cmdline.extend_from_slice(arg.as_bytes());
            cmdline.push(0);
        }
        fs::write(dir.join("cmdline"), &cmdline).unwrap();
        if let Some(home) = home {
            let mut environ = Vec::new();
            for kv in ["PATH=/usr/bin", &format!("HOME={home}"), "TERM=xterm"] {
                environ.extend_from_slice(kv.as_bytes());
                environ.push(0);
            }
            fs::write(dir.join("environ"), &environ).unwrap();
        }
        std::os::unix::fs::symlink(exe, dir.join("exe")).unwrap();
    }

    /// Create `path` (and parents) so `canonicalize` resolves it, mirroring a
    /// real installed binary.
    #[cfg(target_os = "linux")]
    fn touch_binary(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"#!/bin/false\n").unwrap();
    }

    /// Drive a whole scoped stop pass against a synthetic proc tree with the
    /// signal recorded instead of sent. Returns the pids that *would* have
    /// been signalled, the report, and everything written to stderr.
    #[cfg(target_os = "linux")]
    fn run_stop(proc_root: &Path, scope: &StopScope<'_>) -> (Vec<u32>, StopReport, String) {
        let mut signalled = Vec::new();
        let mut err = Vec::new();
        let report = {
            let mut kill = |pid: u32| {
                signalled.push(pid);
                Ok(())
            };
            stop_scoped(proc_root, scope, &mut kill, &mut err)
        };
        (signalled, report, String::from_utf8(err).unwrap())
    }

    /// The ordinary case: a `dreamd watch` running under *this* `$HOME` is
    /// ours and gets signalled, even though its cargo-installed binary is
    /// nowhere near the shim cache.
    #[cfg(target_os = "linux")]
    #[test]
    fn stop_signals_a_process_whose_home_matches_the_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let proc_root = root.join("proc");
        let home = root.join("home/ours");
        let cache = home.join(".cache/dreamd-mcp");
        let exe = home.join(".cargo/bin/dreamd");
        fs::create_dir_all(&home).unwrap();
        touch_binary(&exe);
        write_proc_entry(
            &proc_root,
            4242,
            &["dreamd", "watch"],
            Some(home.to_str().unwrap()),
            &exe,
        );

        let scope = StopScope {
            home: Some(&home),
            cache_dir: Some(&cache),
        };
        let (signalled, report, err) = run_stop(&proc_root, &scope);

        assert_eq!(
            signalled,
            vec![4242],
            "our own home's watch must be stopped"
        );
        assert!(report.attempted);
        assert!(report.matched, "a signalled process must be reported");
        assert!(!report.spared, "nothing was left running");
        assert!(err.is_empty(), "no warning expected, got: {err}");
    }

    /// A signal that does not land (no `kill(1)` on the box, EPERM) leaves the
    /// daemon running exactly as a refusal does, so it must report `spared`
    /// too. Otherwise the caller prints "no local `dreamd mcp` / `dreamd watch`
    /// processes were running" directly above its own "could not stop pid N"
    /// warning — the same contradiction the spared flag exists to remove.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_failed_signal_is_reported_as_left_running_not_as_nothing_running() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let proc_root = root.join("proc");
        let home = root.join("home/ours");
        let exe = home.join(".cargo/bin/dreamd");
        fs::create_dir_all(&home).unwrap();
        touch_binary(&exe);
        write_proc_entry(
            &proc_root,
            4343,
            &["dreamd", "watch"],
            Some(home.to_str().unwrap()),
            &exe,
        );

        let scope = StopScope {
            home: Some(&home),
            cache_dir: None,
        };
        let mut err = Vec::new();
        let mut kill = |_pid: u32| Err(std::io::Error::other("operation not permitted"));
        let report = stop_scoped(&proc_root, &scope, &mut kill, &mut err);
        let err = String::from_utf8(err).unwrap();

        assert!(!report.matched, "a failed signal is not a stop");
        assert!(
            report.spared,
            "the process is still running, so the caller must not claim none were"
        );
        assert!(
            err.contains("could not stop `dreamd watch` pid 4343"),
            "the failure must name the pid: {err}"
        );
    }

    /// The AILAB-554 F1 regression, asserted directly: `HOME=/tmp/sandbox
    /// dreamd update` must not SIGTERM the developer's real Cursor
    /// `dreamd mcp`. Different `HOME=`, executable under a *different* home's
    /// cache — neither rule fires, so the process survives and the user is
    /// told which pid was spared.
    #[cfg(target_os = "linux")]
    #[test]
    fn stop_leaves_another_homes_mcp_running_and_warns() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let proc_root = root.join("proc");
        let sandbox = root.join("tmp/sandbox");
        let sandbox_cache = sandbox.join(".cache/dreamd-mcp");
        let real_home = root.join("home/austin");
        let real_exe = real_home.join(".cache/dreamd-mcp/0.1.0-rc.6/dreamd");
        fs::create_dir_all(&sandbox_cache).unwrap();
        touch_binary(&real_exe);
        write_proc_entry(
            &proc_root,
            5150,
            &["dreamd", "mcp", "--project-root", "/home/austin/code/demo"],
            Some(real_home.to_str().unwrap()),
            &real_exe,
        );

        // The sandboxed invocation's scope.
        let scope = StopScope {
            home: Some(&sandbox),
            cache_dir: Some(&sandbox_cache),
        };
        let (signalled, report, err) = run_stop(&proc_root, &scope);

        assert!(
            signalled.is_empty(),
            "a process serving another $HOME must never be signalled"
        );
        assert!(report.attempted);
        assert!(
            !report.matched,
            "a spared process must not be reported as stopped"
        );
        assert!(
            report.spared,
            "the caller must be able to tell 'spared' from 'nothing was running'"
        );
        // One anchored phrase, not two loose greps: pid and reason must come
        // from the same line (a dev box routinely has some other unattributable
        // dreamd process supplying the words on its own).
        assert!(
            err.contains("pid 5150 running: not attributable"),
            "the warning must name the pid and why it was spared, in one phrase: {err}"
        );
        // The labels must describe *this* invocation's scope, and the message
        // must disclose nothing about the other home. Before phase 3 the format
        // string said "this $HOME (…)" and passed the *foreign* home, so the
        // sandbox's user read the developer's home path as their own.
        assert!(
            err.contains(&sandbox.display().to_string()),
            "the warning must state this invocation's scope: {err}"
        );
        assert!(
            !err.contains(&real_home.display().to_string()),
            "the spared process's $HOME and exe must not be disclosed: {err}"
        );
    }

    /// The cache rule standing alone: a binary this home's npm shim installed
    /// is ours to stop even when it is running with a different `$HOME` (an
    /// MCP server launched by an IDE with its own environment).
    #[cfg(target_os = "linux")]
    #[test]
    fn stop_signals_a_process_whose_exe_is_under_the_scope_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let proc_root = root.join("proc");
        let home = root.join("home/ours");
        let cache = home.join(".cache/dreamd-mcp");
        let exe = cache.join("0.1.0-rc.8/dreamd");
        touch_binary(&exe);
        write_proc_entry(
            &proc_root,
            6001,
            &["dreamd", "mcp"],
            Some("/home/someone-else"),
            &exe,
        );

        let scope = StopScope {
            home: Some(&home),
            cache_dir: Some(&cache),
        };
        let (signalled, report, err) = run_stop(&proc_root, &scope);

        assert_eq!(
            signalled,
            vec![6001],
            "a binary under our native cache is ours to stop"
        );
        assert!(report.matched);
        assert!(err.is_empty(), "no warning expected, got: {err}");
    }

    /// The argv filter runs before attribution: a `dreamd status` under our
    /// own `$HOME` would pass attribution, so its survival proves the
    /// [`STOP_PATTERNS`] gate, not the scope gate.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_non_matching_argv_is_never_a_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let proc_root = root.join("proc");
        let home = root.join("home/ours");
        let exe = home.join(".cargo/bin/dreamd");
        touch_binary(&exe);
        write_proc_entry(
            &proc_root,
            7007,
            &["dreamd", "status"],
            Some(home.to_str().unwrap()),
            &exe,
        );
        write_proc_entry(
            &proc_root,
            7008,
            &["some-editor", "--watch"],
            Some(home.to_str().unwrap()),
            &exe,
        );

        assert!(
            collect_candidates_in(&proc_root).is_empty(),
            "no argv matches STOP_PATTERNS, so there are no candidates"
        );

        let scope = StopScope {
            home: Some(&home),
            cache_dir: None,
        };
        let (signalled, report, err) = run_stop(&proc_root, &scope);
        assert!(signalled.is_empty(), "non-server processes must be ignored");
        assert!(!report.matched);
        assert!(
            err.is_empty(),
            "a non-candidate must not even warn, got: {err}"
        );
    }

    /// A process that merely *mentions* a [`STOP_PATTERNS`] string is not a
    /// candidate, however attributable it is. Both entries here run under our
    /// own `$HOME` — so attribution would pass — and both were signalled before
    /// [`is_dreamd_binary`] existed: the reviewer demonstrated
    /// `sh -c 'sleep 30 # dreamd watch'` and a `grep` for the phrase being
    /// SIGTERMed under a sandbox HOME.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_process_that_is_not_the_dreamd_binary_is_never_a_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let proc_root = root.join("proc");
        let home = root.join("home/ours");
        let shell = root.join("bin/dash");
        let grep = root.join("bin/grep");
        let dreamd = home.join(".cargo/bin/dreamd");
        fs::create_dir_all(&home).unwrap();
        touch_binary(&shell);
        touch_binary(&grep);
        touch_binary(&dreamd);

        // A shell holding the phrase in a comment (its cmdline keeps the whole
        // `-c` string), and a grep whose pattern is the phrase.
        write_proc_entry(
            &proc_root,
            3001,
            &["/bin/sh", "-c", "sleep 30 # dreamd watch"],
            Some(home.to_str().unwrap()),
            &shell,
        );
        write_proc_entry(
            &proc_root,
            3002,
            &["grep", "-n", "dreamd watch", "notes.md"],
            Some(home.to_str().unwrap()),
            &grep,
        );
        // An interactive shell that ran `dreamd watch; dreamd update` — the
        // whole command line is its own argv.
        write_proc_entry(
            &proc_root,
            3003,
            &["sh", "-c", "dreamd watch; dreamd update"],
            Some(home.to_str().unwrap()),
            &shell,
        );
        // Positive control in the same table: the real daemon still matches, so
        // this test cannot pass by the scan being broken outright.
        write_proc_entry(
            &proc_root,
            3004,
            &["dreamd", "watch"],
            Some(home.to_str().unwrap()),
            &dreamd,
        );

        let scope = StopScope {
            home: Some(&home),
            cache_dir: None,
        };
        let (signalled, report, err) = run_stop(&proc_root, &scope);

        assert_eq!(
            signalled,
            vec![3004],
            "only the dreamd binary may be signalled; got {signalled:?}"
        );
        assert!(report.matched);
        assert!(
            !report.spared,
            "a non-dreamd process is not a spared candidate — it is not a candidate at all"
        );
        assert!(
            err.is_empty(),
            "a non-candidate must not even warn, got: {err}"
        );
    }

    /// Unreadable attribution data is a refusal, not a default-allow. Another
    /// user's `/proc/<pid>/environ` is EACCES, modelled here by omitting the
    /// file; with the executable outside our cache nothing can vouch for the
    /// process, so it is spared.
    #[cfg(target_os = "linux")]
    #[test]
    fn stop_leaves_a_process_with_unreadable_environ_running() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let proc_root = root.join("proc");
        let home = root.join("home/ours");
        let cache = home.join(".cache/dreamd-mcp");
        let exe = root.join("opt/dreamd/bin/dreamd");
        fs::create_dir_all(&cache).unwrap();
        touch_binary(&exe);
        write_proc_entry(&proc_root, 8008, &["dreamd", "watch"], None, &exe);

        let scope = StopScope {
            home: Some(&home),
            cache_dir: Some(&cache),
        };
        let (signalled, report, err) = run_stop(&proc_root, &scope);

        assert!(
            signalled.is_empty(),
            "unreadable attribution must never authorise a signal"
        );
        assert!(!report.matched);
        assert!(
            report.spared,
            "a candidate left running must be reported as spared, not as nothing-was-running"
        );
        // Anchored to one substring so pid and reason cannot come from
        // different lines. The pre-phase-3 message also rendered the
        // candidate's unreadable `environ` as `this $HOME (unreadable)`, which
        // read as a claim about the *user's own* home; the wording no longer
        // says anything about the spared process beyond its pid.
        assert!(
            err.contains("pid 8008 running: not attributable"),
            "the warning must name the pid and why it was spared, in one phrase: {err}"
        );
        assert!(
            !err.contains(&root.join("opt/dreamd/bin/dreamd").display().to_string()),
            "the spared process's executable path must not be disclosed: {err}"
        );
    }

    /// With neither scope field resolved every match would have to be taken on
    /// faith — the machine-global behaviour AILAB-584 removed. Stop nothing,
    /// warn, and keep `attempted: true`: `update.rs` prints "automatic stop is
    /// unix-only in v0.1" on `!attempted`, which would be a lie on unix.
    ///
    /// The candidate is still enumerated and spared *by name*, so `spared` is
    /// set: returning early with `spared: false` would make `update.rs` print
    /// "no local `dreamd mcp` / `dreamd watch` processes were running" about a
    /// process that is still running.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_unresolvable_scope_stops_nothing_but_still_counts_as_attempted() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let proc_root = root.join("proc");
        let exe = root.join("home/ours/.cargo/bin/dreamd");
        touch_binary(&exe);
        write_proc_entry(
            &proc_root,
            9009,
            &["dreamd", "watch"],
            Some(root.join("home/ours").to_str().unwrap()),
            &exe,
        );

        let scope = StopScope {
            home: None,
            cache_dir: None,
        };
        let (signalled, report, err) = run_stop(&proc_root, &scope);

        assert!(
            signalled.is_empty(),
            "an unresolvable scope must signal nothing at all"
        );
        assert!(report.attempted, "the pass ran; only the scope was missing");
        assert!(!report.matched);
        assert!(
            report.spared,
            "a running candidate was left alone; the caller must not print 'nothing was running'"
        );
        assert!(
            err.contains("could not be resolved"),
            "the user must be told why nothing was stopped: {err}"
        );
        assert!(
            err.contains("pid 9009 running: not attributable"),
            "the spared candidate must still be named: {err}"
        );
        assert!(
            err.contains("$HOME (unresolved)"),
            "an unresolved scope must read as unresolved, never as unreadable: {err}"
        );
    }

    /// A `dreamd update` must never signal itself. The pid filter is belt to
    /// the argv rule's braces, so pin it: even a self entry with a matching
    /// argv and a matching `HOME=` is not a candidate.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_current_process_is_never_a_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let proc_root = root.join("proc");
        let home = root.join("home/ours");
        let exe = home.join(".cargo/bin/dreamd");
        touch_binary(&exe);
        write_proc_entry(
            &proc_root,
            std::process::id(),
            &["dreamd", "watch"],
            Some(home.to_str().unwrap()),
            &exe,
        );

        assert!(
            collect_candidates_in(&proc_root).is_empty(),
            "this process must be skipped even when everything else matches"
        );
    }

    #[test]
    fn remove_socket_unlinks_present_file() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("dreamd.sock");
        fs::write(&sock, b"").unwrap();
        assert!(
            matches!(remove_socket(&sock), SocketRemoval::Removed),
            "present socket file must be removed"
        );
        assert!(!sock.exists());
    }

    #[test]
    fn remove_socket_absent_is_not_found_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("nope.sock");
        assert!(
            matches!(remove_socket(&sock), SocketRemoval::NotFound),
            "absent socket must report NotFound"
        );
    }

    /// A daemon that outlasts the grace window must survive `remove_socket`.
    /// The stop pass both callers run first is best-effort, so unlinking here
    /// would leave a live daemon on an orphaned inode — still serving, but on
    /// a path no client can resolve. Grace is pinned to zero so the refuse
    /// path is exercised without waiting out `STOP_GRACE`.
    #[cfg(unix)]
    #[test]
    fn remove_socket_refuses_a_daemon_that_outlasts_the_grace_window() {
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("dreamd.sock");
        let _listener = UnixListener::bind(&sock).expect("bind test listener");

        assert!(
            matches!(
                remove_socket_within(&sock, Duration::ZERO),
                SocketRemoval::RefusedLive
            ),
            "a socket a daemon still answers on must not be unlinked"
        );
        assert!(sock.exists(), "the live socket must be left in place");
    }

    /// The regression the grace window exists for: `kill` returns as soon as
    /// SIGTERM is *delivered*, so on the ordinary `dreamd update`-with-`watch`
    /// path the daemon is still bound and answering while it drains. Probing
    /// once would refuse a socket whose daemon is already mid-exit. The guard
    /// must wait it out and then unlink, not warn.
    #[cfg(unix)]
    #[test]
    fn remove_socket_waits_out_a_daemon_that_is_still_shutting_down() {
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("dreamd.sock");
        let listener = UnixListener::bind(&sock).expect("bind test listener");

        // Answers now, goes quiet shortly after — a daemon finishing its drain.
        let shutting_down = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(listener);
        });

        assert!(
            matches!(
                remove_socket_within(&sock, Duration::from_secs(5)),
                SocketRemoval::Removed
            ),
            "a daemon that exits within the grace window must not be refused"
        );
        assert!(
            !sock.exists(),
            "the socket must be unlinked once it goes quiet"
        );
        shutting_down.join().unwrap();
    }

    /// The liveness guard must not over-refuse: an orphan socket file — daemon
    /// gone, file left behind — is the ordinary uninstall case and must still
    /// unlink. Dropping the listener closes the fd without unlinking the path,
    /// which is exactly what a SIGKILLed daemon leaves on disk.
    #[cfg(unix)]
    #[test]
    fn remove_socket_unlinks_orphan_socket_file() {
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("dreamd.sock");
        drop(UnixListener::bind(&sock).expect("bind test listener"));
        assert!(sock.exists(), "orphan socket file expected for the test");

        assert!(
            matches!(remove_socket(&sock), SocketRemoval::Removed),
            "an orphan socket file must still be unlinked"
        );
        assert!(!sock.exists());
    }

    #[test]
    fn clear_dir_tree_removes_and_reports_absent() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("cache");
        fs::create_dir_all(tree.join("v1")).unwrap();
        fs::write(tree.join("v1/dreamd"), b"bin").unwrap();
        assert!(clear_dir_tree(&tree).unwrap(), "existing tree removed");
        assert!(!tree.exists());
        assert!(
            !clear_dir_tree(&tree).unwrap(),
            "second clear is Ok(false), not an error"
        );
    }

    #[test]
    fn scoped_npx_removes_only_dreamd_mcp_entries() {
        let dir = tempfile::tempdir().unwrap();
        // Real npm shape: no top-level name, requested package as a
        // dependencies key.
        let ours = dir.path().join("aaa111");
        fs::create_dir_all(&ours).unwrap();
        fs::write(
            ours.join("package.json"),
            br#"{"dependencies":{"dreamd-mcp":"^0.1.0-rc.6"}}"#,
        )
        .unwrap();
        let other = dir.path().join("bbb222");
        fs::create_dir_all(&other).unwrap();
        fs::write(
            other.join("package.json"),
            br#"{"dependencies":{"other-tool":"^1"}}"#,
        )
        .unwrap();
        let no_manifest = dir.path().join("ccc333");
        fs::create_dir_all(&no_manifest).unwrap();

        let removed = clear_scoped_npx(dir.path()).unwrap();
        assert_eq!(removed, vec![ours.clone()], "only the dreamd-mcp entry");
        assert!(!ours.exists());
        assert!(other.exists(), "unrelated package must survive");
        assert!(no_manifest.exists(), "manifest-less entry must survive");
    }

    #[test]
    fn scoped_npx_missing_root_is_ok_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("_missing");
        let removed = clear_scoped_npx(&missing).unwrap();
        assert!(removed.is_empty(), "missing npx root must be a no-op");
    }

    #[test]
    fn npx_entry_match_covers_real_npm_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let entry = dir.path().join("entry");
        fs::create_dir_all(&entry).unwrap();
        let manifest = entry.join("package.json");

        // Malformed JSON: never a match — never delete on doubt.
        fs::write(&manifest, b"not json").unwrap();
        assert!(
            !entry_is_dreamd_mcp_npx_install(&entry),
            "malformed manifest must never be deleted"
        );

        // (b) Real npx shape: no name field, dreamd-mcp as the exact
        // dependencies key.
        fs::write(
            &manifest,
            br#"{"dependencies":{"dreamd-mcp":"^0.1.0-rc.6"}}"#,
        )
        .unwrap();
        assert!(
            entry_is_dreamd_mcp_npx_install(&entry),
            "the dependencies-key shape npm actually writes must match"
        );

        // A different dependency key is not a match.
        fs::write(&manifest, br#"{"dependencies":{"other-tool":"^1"}}"#).unwrap();
        assert!(
            !entry_is_dreamd_mcp_npx_install(&entry),
            "an unrelated dependency key must not match"
        );

        // (a) Top-level name fallback still matches.
        fs::write(&manifest, br#"{"name":"dreamd-mcp"}"#).unwrap();
        assert!(entry_is_dreamd_mcp_npx_install(&entry));

        // (c) node_modules/dreamd-mcp/package.json naming the package
        // matches even when the top-level manifest doesn't.
        fs::write(&manifest, br#"{"name":"some-wrapper"}"#).unwrap();
        assert!(!entry_is_dreamd_mcp_npx_install(&entry));
        let nm = entry.join("node_modules").join("dreamd-mcp");
        fs::create_dir_all(&nm).unwrap();
        fs::write(nm.join("package.json"), br#"{"name":"dreamd-mcp"}"#).unwrap();
        assert!(
            entry_is_dreamd_mcp_npx_install(&entry),
            "an installed node_modules/dreamd-mcp must match"
        );
    }
}
