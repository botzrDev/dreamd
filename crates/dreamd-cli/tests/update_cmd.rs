//! Integration tests for the `dreamd update` restart contract (AILAB-552).
//!
//! In-process library calls with injected temp paths — the process-stop hook is
//! a fake [`Stopper`], so the suite never runs `pkill`, never signals a real
//! `dreamd mcp` / `dreamd watch` on the dev machine, and never touches the real
//! `~/.agent` / `~/.cache` / npx cache. No network anywhere.
//!
//! `tests/uninstall_cmd.rs` keeps the AILAB-226 baseline coverage (dry-run
//! mutates nothing, real path clears socket + cache, cargo-install hint); this
//! file covers the restart contract text and `--restart`.

use std::io::Cursor;
use std::path::PathBuf;

use dreamd::commands::lifecycle_cleanup::StopReport;
use dreamd::commands::update;
use dreamd::commands::version::VERSION_SHORT;

/// Temp socket + populated native binary cache tree. No registry involved —
/// `update` never touches it.
struct Fixture {
    home: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("dreamd.sock"), b"").unwrap();
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
/// (AILAB-584). That stdout line used to print directly above a stderr warning
/// naming the pid that was still running — the tool contradicting itself on
/// exactly the scenario the scoping fix exists for.
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

/// A second update in a row is benign — nothing left to clear, contract still
/// printed.
#[test]
fn second_update_is_benign_and_still_prints_the_contract() {
    let f = Fixture::new();
    let (_, first) = run_update(&f, false, false, false, ATTEMPTED_NO_MATCH);
    assert!(first.contains("native cache cleared"), "got: {first:?}");

    let (calls, stdout) = run_update(&f, false, false, false, ATTEMPTED_NO_MATCH);
    assert_eq!(calls, 1, "the stop pass is idempotent, not skipped");
    assert!(
        stdout.contains("no daemon socket") && stdout.contains("native cache already clean"),
        "second run must be benign; got: {stdout:?}"
    );
    assert!(
        stdout.contains("restart contract"),
        "guidance must not vanish on the second run; got: {stdout:?}"
    );
}
