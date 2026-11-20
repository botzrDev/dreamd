//! Integration tests for the `dreamd update` restart contract (AILAB-552).
//!
//! In-process library calls with injected temp paths — the process-stop hook is
//! a fake [`Stopper`], so the suite never runs `pkill`, never signals a real
//! `dreamd mcp` / `dreamd watch` on the dev machine, and never touches the real
//! `~/.agent` / `~/.cache` / npx cache. No network anywhere.
//!
//! `tests/uninstall_cmd.rs` keeps the AILAB-226 baseline coverage (dry-run
//! mutates nothing, real path clears the socket and the other cached versions,
//! cargo-install hint); this file covers the restart contract text, `--restart`
//! and which cached versions `update` keeps (DRM-152).

use std::io::Cursor;
use std::path::PathBuf;

use dreamd::commands::lifecycle_cleanup::StopReport;
use dreamd::commands::update;
use dreamd::commands::version::VERSION_SHORT;

/// An orphaned daemon socket at `path`: on Unix a bound-then-dropped listener,
/// as a crashed `watch` leaves. A regular file there is refused (DRM-99).
fn orphan_socket(path: &std::path::Path) {
    #[cfg(unix)]
    drop(std::os::unix::net::UnixListener::bind(path).expect("bind test socket"));
    #[cfg(not(unix))]
    std::fs::write(path, b"").unwrap();
}

/// Temp socket + populated native binary cache tree. No registry involved —
/// `update` never touches it.
struct Fixture {
    home: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        orphan_socket(&home.path().join("dreamd.sock"));
        let tree = home.path().join("dreamd-mcp").join("0.1.0-test");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("dreamd"), b"fake binary").unwrap();
        Self { home }
    }

    fn socket(&self) -> PathBuf {
        self.home.path().join("dreamd.sock")
    }

    fn cache_tree(&self) -> PathBuf {
        self.home.path().join("dreamd-mcp")
    }
}

/// Fake stop hook with a recorded outcome. Returns `(call_count, stdout)`.
fn run_update(
    f: &Fixture,
    dry_run: bool,
    quiet: bool,
    restart: bool,
    report: StopReport,
) -> (usize, String) {
    let socket = f.socket();
    let cache_tree = f.cache_tree();
    let mut calls = 0usize;
    let mut stop = |_: &mut dyn std::io::Write| {
        calls += 1;
        report
    };
    let req = update::UpdateRequest {
        socket: Some(&socket),
        cache_dir: Some(&cache_tree),
        dry_run,
        quiet,
        restart,
        cargo_install_hint: None,
    };
    let mut out = Cursor::new(Vec::new());
    let mut err = Cursor::new(Vec::new());
    update::run(&req, &mut stop, &mut out, &mut err).unwrap();
    let stdout = String::from_utf8(out.into_inner()).unwrap();
    (calls, stdout)
}

const ATTEMPTED_NO_MATCH: StopReport = StopReport {
    attempted: true,
    matched: false,
    spared: false,
};
const MATCHED: StopReport = StopReport {
    attempted: true,
    matched: true,
    spared: false,
};
const NOT_ATTEMPTED: StopReport = StopReport {
    attempted: false,
    matched: false,
    spared: false,
};
/// The AILAB-584 shape: a `dreamd mcp` / `dreamd watch` matched but belonged to
/// another `$HOME`, so it was deliberately left running.
const SPARED: StopReport = StopReport {
    attempted: true,
    matched: false,
    spared: true,
};
/// Mixed sweep: this home's daemon stopped, another home's spared.
const MATCHED_AND_SPARED: StopReport = StopReport {
    attempted: true,
    matched: true,
    spared: true,
};

/// The dry-run plan spells out the whole restart contract — stop, reload the
/// harness, re-run floating npx — while mutating nothing.
#[test]
fn dry_run_prints_restart_contract_and_changes_nothing() {
    let f = Fixture::new();
    let (calls, stdout) = run_update(&f, true, false, false, ATTEMPTED_NO_MATCH);

    assert_eq!(calls, 0, "dry-run must not attempt a process stop");
    assert!(f.socket().exists(), "dry-run must not remove the socket");
    assert!(
        f.cache_tree().exists(),
        "dry-run must not clear the cache tree"
    );
    assert!(
        stdout.contains("restart contract"),
        "dry-run must label the contract; got: {stdout:?}"
    );
    assert!(
        stdout.contains("would stop local `dreamd mcp` / `dreamd watch`"),
        "step 1 must name both patterns in the conditional; got: {stdout:?}"
    );
    assert!(
        stdout.contains("reload your MCP harness"),
        "step 2 (the missing step today) must appear; got: {stdout:?}"
    );
    assert!(
        stdout.contains("npx -y dreamd-mcp"),
        "step 3 must be the floating npx re-run; got: {stdout:?}"
    );
    assert!(
        stdout.contains(VERSION_SHORT) && stdout.contains("no changes"),
        "before/after version lines stay; got: {stdout:?}"
    );
}

/// Real update: the stop runs (unchanged AILAB-226 default) and step 1 of the
/// contract reports what it matched.
#[test]
fn update_stops_servers_and_reports_matched_processes() {
    let f = Fixture::new();
    let (calls, stdout) = run_update(&f, false, false, false, MATCHED);

    assert_eq!(calls, 1, "update always stops local servers");
    assert!(
        stdout.contains("restart contract"),
        "the real path prints the contract too; got: {stdout:?}"
    );
    assert!(
        stdout.contains("stopped local `dreamd mcp` / `dreamd watch` process(es)"),
        "step 1 must report the matched stop; got: {stdout:?}"
    );
    assert!(
        stdout.contains("reload your MCP harness"),
        "step 2 must appear on the real path; got: {stdout:?}"
    );
}

/// Nothing running is the common case and must read as success, not a warning.
#[test]
fn update_reports_when_no_servers_were_running() {
    let f = Fixture::new();
    let (_, stdout) = run_update(&f, false, false, false, ATTEMPTED_NO_MATCH);

    assert!(
        stdout.contains("no local `dreamd mcp` / `dreamd watch` processes were running"),
        "step 1 must report the benign no-match; got: {stdout:?}"
    );
}

/// A process that matched but was **spared** as not attributable to this
/// `$HOME` / native cache must never be reported as "nothing was running"
/// (AILAB-584). Printing that stdout line above a stderr warning that names the
/// still-running pid would contradict itself on exactly the scenario the
/// scoping exists for.
#[test]
fn update_reports_spared_processes_instead_of_claiming_none_were_running() {
    let f = Fixture::new();
    let (_, stdout) = run_update(&f, false, false, false, SPARED);

    assert!(
        !stdout.contains("no local `dreamd mcp` / `dreamd watch` processes were running"),
        "a spared process must not be reported as nothing running; got: {stdout:?}"
    );
    assert!(
        stdout.contains("left `dreamd mcp` / `dreamd watch` process(es) running"),
        "step 1 must say what was left running; got: {stdout:?}"
    );
    assert!(
        stdout.contains("see the warnings above"),
        "step 1 must point at the stderr detail naming each pid; got: {stdout:?}"
    );
    assert!(
        !stdout.contains("stopped local"),
        "nothing was stopped; got: {stdout:?}"
    );

    // Mixed sweep: the stop that happened is still reported, and the spare is
    // not swallowed by it.
    let g = Fixture::new();
    let (_, mixed) = run_update(&g, false, false, false, MATCHED_AND_SPARED);
    // The mixed line must stay a superset of the matched line: a mixed sweep is
    // the ordinary result on a box that has any other home's `dreamd mcp`
    // running, so `update_stops_servers_and_reports_matched_processes` and the
    // cross-`$HOME` integration suite must both keep matching.
    assert!(
        mixed.contains("stopped local `dreamd mcp` / `dreamd watch` process(es)")
            && mixed.contains("others were left running"),
        "a mixed sweep must report both halves; got: {mixed:?}"
    );
}

/// When the platform cannot stop processes (non-unix `StopReport`), step 1
/// hands the job to the user instead of claiming a stop happened.
#[test]
fn update_asks_user_to_stop_when_stop_not_attempted() {
    let f = Fixture::new();
    let (_, stdout) = run_update(&f, false, false, false, NOT_ATTEMPTED);

    assert!(
        stdout.contains("stop local `dreamd mcp` / `dreamd watch` yourself"),
        "an unattempted stop must not claim success; got: {stdout:?}"
    );
    assert!(
        !stdout.contains("stopped local"),
        "must not claim a stop that never happened; got: {stdout:?}"
    );
}

/// `--restart` is the explicit, louder form of the same stop path.
#[test]
fn restart_flag_announces_the_stop_and_still_stops() {
    let f = Fixture::new();
    let (calls, stdout) = run_update(&f, false, false, true, MATCHED);

    assert_eq!(calls, 1, "--restart performs the same single stop pass");
    assert!(
        stdout.contains("--restart"),
        "--restart must print a louder confirmation; got: {stdout:?}"
    );
    assert!(
        stdout.contains("restart contract"),
        "the contract still prints; got: {stdout:?}"
    );
}

/// `--restart` must not claim a stop the platform never attempted: on a
/// non-unix `StopReport` the louder line has to stay off, leaving step 1 to
/// hand the job to the user.
#[test]
fn restart_flag_does_not_claim_a_stop_that_was_never_attempted() {
    let f = Fixture::new();
    let (_, stdout) = run_update(&f, false, false, true, NOT_ATTEMPTED);

    assert!(
        !stdout.contains("restart: --restart"),
        "must not announce a stop pass that never ran; got: {stdout:?}"
    );
    assert!(
        stdout.contains("stop local `dreamd mcp` / `dreamd watch` yourself"),
        "step 1 still hands the stop to the user; got: {stdout:?}"
    );
}

/// `--restart --dry-run` stays a plan: the flag never turns a dry run into a
/// real stop.
#[test]
fn restart_flag_in_dry_run_still_changes_nothing() {
    let f = Fixture::new();
    let (calls, stdout) = run_update(&f, true, false, true, MATCHED);

    assert_eq!(calls, 0, "--restart must not defeat --dry-run");
    assert!(f.socket().exists(), "--restart --dry-run keeps the socket");
    assert!(
        f.cache_tree().exists(),
        "--restart --dry-run keeps the cache"
    );
    assert!(
        stdout.contains("--restart"),
        "the plan must acknowledge --restart; got: {stdout:?}"
    );
}

/// `--quiet` compresses the contract but must not drop the "what do I do now"
/// guidance entirely.
#[test]
fn quiet_keeps_versions_and_a_compressed_next_line() {
    let f = Fixture::new();
    let (calls, stdout) = run_update(&f, false, true, false, ATTEMPTED_NO_MATCH);

    assert_eq!(calls, 1, "--quiet does not change what update does");
    assert!(
        stdout.contains("reload your MCP harness") && stdout.contains("npx -y dreamd-mcp"),
        "quiet must still say reload + re-run; got: {stdout:?}"
    );
    assert!(
        !stdout.contains("restart contract:"),
        "quiet must compress the numbered block; got: {stdout:?}"
    );
    assert!(
        stdout.contains(VERSION_SHORT),
        "quiet keeps the version lines; got: {stdout:?}"
    );

    // Same compression on the quiet dry-run plan.
    let g = Fixture::new();
    let (_, dry) = run_update(&g, true, true, false, ATTEMPTED_NO_MATCH);
    assert!(
        dry.contains("reload your MCP harness") && dry.contains("npx -y dreamd-mcp"),
        "quiet dry-run must still say reload + re-run; got: {dry:?}"
    );
}

/// A second update in a row is benign — nothing left to remove, contract still
/// printed.
#[test]
fn second_update_is_benign_and_still_prints_the_contract() {
    let f = Fixture::new();
    let (_, first) = run_update(&f, false, false, false, ATTEMPTED_NO_MATCH);
    assert!(
        first.contains("removed 1 other cached version(s): 0.1.0-test"),
        "got: {first:?}"
    );

    let (calls, stdout) = run_update(&f, false, false, false, ATTEMPTED_NO_MATCH);
    assert_eq!(calls, 1, "the stop pass is idempotent, not skipped");
    assert!(
        stdout.contains("no daemon socket") && stdout.contains("no other cached versions in"),
        "second run must be benign; got: {stdout:?}"
    );
    assert!(
        stdout.contains("restart contract"),
        "guidance must not vanish on the second run; got: {stdout:?}"
    );
}

/// The cache directory the running binary's own version uses.
const THIS_VERSION: &str = env!("CARGO_PKG_VERSION");

/// DRM-152's cache: this version's binary (bytes `x`), an older version's,
/// and an empty older directory.
fn versioned_cache(f: &Fixture) {
    let cache = f.cache_tree();
    std::fs::remove_dir_all(&cache).unwrap();
    std::fs::create_dir_all(cache.join(THIS_VERSION)).unwrap();
    std::fs::write(cache.join(THIS_VERSION).join("dreamd"), b"x").unwrap();
    std::fs::create_dir_all(cache.join("1.0.0")).unwrap();
    std::fs::write(cache.join("1.0.0").join("dreamd"), b"old binary").unwrap();
    std::fs::create_dir_all(cache.join("0.9.0")).unwrap();
}

/// Every file under `dir` with its bytes, by relative path, and every
/// directory, so an empty one counts.
fn snapshot(dir: &std::path::Path) -> Vec<(String, Option<Vec<u8>>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            let rel = path.strip_prefix(dir).unwrap().display().to_string();
            if path.is_dir() {
                out.push((rel, None));
                stack.push(path);
            } else {
                out.push((rel, Some(std::fs::read(&path).unwrap())));
            }
        }
    }
    out.sort();
    out
}

fn has_line(stdout: &str, want: &str) -> bool {
    stdout.lines().any(|line| line == want)
}

/// DRM-152 test 1: the running version's binary stays; every other version
/// goes.
#[test]
fn update_keeps_the_running_versions_cache() {
    let f = Fixture::new();
    versioned_cache(&f);
    let cache = f.cache_tree();

    let (_, stdout) = run_update(&f, false, false, false, ATTEMPTED_NO_MATCH);

    assert_eq!(
        std::fs::read(cache.join(THIS_VERSION).join("dreamd")).unwrap(),
        b"x",
        "the running version's binary must stay byte for byte"
    );
    assert!(!cache.join("1.0.0").exists(), "an older version must go");
    assert!(!cache.join("0.9.0").exists(), "an empty older dir must go");
    let kept = format!(
        "kept the cached dreamd {THIS_VERSION} in {}",
        cache.join(THIS_VERSION).display()
    );
    assert!(has_line(&stdout, &kept), "want {kept:?}; got: {stdout:?}");
    assert!(
        has_line(&stdout, "removed 2 other cached version(s): 0.9.0, 1.0.0"),
        "got: {stdout:?}"
    );
}

/// DRM-152 test 2: the text promises no new binary, and names the floating
/// spawn and the pin.
#[test]
fn update_promises_no_new_binary() {
    let f = Fixture::new();
    let (_, stdout) = run_update(&f, false, false, false, ATTEMPTED_NO_MATCH);

    assert!(
        !stdout.contains("fetches the new binary"),
        "got: {stdout:?}"
    );
    assert!(!stdout.contains("until reinstall"), "got: {stdout:?}");
    for want in [
        "  3. re-run `npx -y dreamd-mcp`: floating npx runs the newest published dreamd-mcp, \
         and the shim downloads a binary only for a version it has not cached",
        "  4. an MCP config that pins `dreamd-mcp@<version>` stays on that version. Use \
         `npx -y dreamd-mcp` to follow new releases",
    ] {
        assert!(has_line(&stdout, want), "want {want:?}; got: {stdout:?}");
    }
    let after = format!(
        "after: {VERSION_SHORT} (this process keeps its version; the next spawn runs the \
         version npx resolves)"
    );
    assert!(has_line(&stdout, &after), "want {after:?}; got: {stdout:?}");
}

/// DRM-152 test 3: the dry run names what it would remove and changes
/// nothing.
#[test]
fn dry_run_names_what_it_would_remove() {
    let f = Fixture::new();
    versioned_cache(&f);
    let before = snapshot(&f.cache_tree());

    let (_, stdout) = run_update(&f, true, false, false, ATTEMPTED_NO_MATCH);

    let want = format!(
        "dry-run: would stop dreamd mcp/watch processes, remove the daemon socket, and remove \
         cached versions other than {THIS_VERSION} from {}",
        f.cache_tree().display()
    );
    assert!(has_line(&stdout, &want), "want {want:?}; got: {stdout:?}");
    assert_eq!(
        snapshot(&f.cache_tree()),
        before,
        "a dry run must leave the cache byte for byte"
    );
}

/// DRM-152 test 4: a cache that holds only this version is left alone.
#[test]
fn a_cache_with_only_this_version_is_left_alone() {
    let f = Fixture::new();
    let cache = f.cache_tree();
    std::fs::remove_dir_all(&cache).unwrap();
    std::fs::create_dir_all(cache.join(THIS_VERSION)).unwrap();
    std::fs::write(cache.join(THIS_VERSION).join("dreamd"), b"x").unwrap();

    let (_, stdout) = run_update(&f, false, false, false, ATTEMPTED_NO_MATCH);

    assert_eq!(
        std::fs::read(cache.join(THIS_VERSION).join("dreamd")).unwrap(),
        b"x"
    );
    let want = format!("no other cached versions in {}", cache.display());
    assert!(has_line(&stdout, &want), "want {want:?}; got: {stdout:?}");
}
