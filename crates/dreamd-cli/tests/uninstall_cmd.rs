//! Integration tests for `dreamd uninstall` / `dreamd update` (AILAB-226).
//!
//! In-process library calls with injected temp paths (same style as
//! `tests/uninstall.rs`, which stays the `init --uninstall-project` suite).
//! The process-stop hook is injected as a fake `Stopper`, so the suite never
//! runs `pkill`, never signals a real process, and never touches the real
//! `~/.agent` / `~/.cache` / npx cache. No network anywhere.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use dreamd::commands::lifecycle_cleanup::StopReport;
use dreamd::commands::version::VERSION_SHORT;
use dreamd::commands::{init, uninstall, update};

/// Fake stop hook: reports "attempted, nothing matched" without signalling
/// any real process.
fn no_op_stop(_: &mut dyn std::io::Write) -> StopReport {
    StopReport {
        attempted: true,
        matched: false,
    }
}

struct Fixture {
    daemon_home: tempfile::TempDir,
    project: tempfile::TempDir,
    cache: tempfile::TempDir,
    npx: tempfile::TempDir,
}

impl Fixture {
    fn socket(&self) -> PathBuf {
        self.daemon_home.path().join("dreamd.sock")
    }

    /// Native binary cache tree (the `~/.cache/dreamd-mcp` analogue).
    fn cache_tree(&self) -> PathBuf {
        self.cache.path().join("dreamd-mcp")
    }

    fn npx_ours(&self) -> PathBuf {
        self.npx.path().join("aaa111")
    }

    fn npx_other(&self) -> PathBuf {
        self.npx.path().join("bbb222")
    }
}

/// Registered project + fake socket + populated fake cache trees.
fn fixture() -> Fixture {
    let daemon_home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    // Root sentinel so find_project_root/init succeed.
    std::fs::write(project.path().join("Cargo.toml"), b"[package]").unwrap();

    // Register the project via the normal init path.
    let mut out = Cursor::new(Vec::new());
    let mut err = Cursor::new(Vec::new());
    init::run(project.path(), daemon_home.path(), true, &mut out, &mut err).unwrap();

    // Fake daemon socket file.
    std::fs::write(daemon_home.path().join("dreamd.sock"), b"").unwrap();

    // Fake native binary cache tree: <cache>/dreamd-mcp/<version>/dreamd.
    let cache = tempfile::tempdir().unwrap();
    let tree = cache.path().join("dreamd-mcp").join("0.1.0-test");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join("dreamd"), b"fake binary").unwrap();

    // Fake npx cache in the shape npm actually writes — no top-level name;
    // the requested package is the dependencies key. One dreamd-mcp entry,
    // one unrelated package.
    let npx = tempfile::tempdir().unwrap();
    let ours = npx.path().join("aaa111");
    std::fs::create_dir_all(&ours).unwrap();
    std::fs::write(
        ours.join("package.json"),
        br#"{"dependencies":{"dreamd-mcp":"^0.1.0-rc.6"}}"#,
    )
    .unwrap();
    let other = npx.path().join("bbb222");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(
        other.join("package.json"),
        br#"{"dependencies":{"another-tool":"^2.0.0"}}"#,
    )
    .unwrap();

    Fixture {
        daemon_home,
        project,
        cache,
        npx,
    }
}

fn uninstall_req<'a>(
    f: &'a Fixture,
    socket: &'a Path,
    cache_tree: &'a Path,
    keep_caches: bool,
    all_npx: bool,
) -> uninstall::UninstallRequest<'a> {
    uninstall::UninstallRequest {
        cwd: f.project.path(),
        daemon_home: f.daemon_home.path(),
        socket: Some(socket),
        cache_dir: Some(cache_tree),
        npx_dir: Some(f.npx.path()),
        keep_caches,
        all_npx,
        quiet: false,
    }
}

/// (a) Full uninstall: socket unlinked, registry entry removed, cache trees
/// cleared (scoped npx keeps unrelated packages) — and a second run is Ok
/// with benign messaging.
#[test]
fn uninstall_cleans_everything_and_is_idempotent() {
    let f = fixture();
    let socket = f.socket();
    let cache_tree = f.cache_tree();
    let registry = f.daemon_home.path().join("registry.toml");
    assert!(
        std::fs::read_to_string(&registry)
            .unwrap()
            .contains("root ="),
        "fixture must start registered"
    );

    let req = uninstall_req(&f, &socket, &cache_tree, false, false);
    let mut out = Cursor::new(Vec::new());
    let mut err = Cursor::new(Vec::new());
    uninstall::run(&req, &mut no_op_stop, &mut out, &mut err).unwrap();

    assert!(!socket.exists(), "socket must be unlinked");
    assert!(!cache_tree.exists(), "native cache tree must be removed");
    assert!(
        !f.npx_ours().exists(),
        "dreamd-mcp npx entry must be removed"
    );
    assert!(
        f.npx_other().exists(),
        "unrelated npx entry must survive a scoped clear"
    );
    // The tempdir path itself may differ from the canonicalized stored root
    // (macOS /var -> /private/var), so assert no project entry remains at
    // all rather than checking for the raw path.
    let raw = std::fs::read_to_string(&registry).unwrap();
    assert!(
        !raw.contains("root ="),
        "registry must hold no project entries after uninstall; got: {raw:?}"
    );
    let stdout = String::from_utf8(out.into_inner()).unwrap();
    assert!(
        stdout.contains("there is no `dreamd reset --all`"),
        "guidance must name the non-existent flag; got: {stdout:?}"
    );
    assert!(
        stdout.contains(".agent/ stores"),
        "guidance must say project stores are left in place; got: {stdout:?}"
    );

    // Second run: everything already gone — still Ok, benign messaging.
    let mut out2 = Cursor::new(Vec::new());
    let mut err2 = Cursor::new(Vec::new());
    uninstall::run(&req, &mut no_op_stop, &mut out2, &mut err2).unwrap();
    let stdout2 = String::from_utf8(out2.into_inner()).unwrap();
    assert!(
        stdout2.contains("nothing to do"),
        "second run must be benign; got: {stdout2:?}"
    );
    assert!(
        stdout2.contains("no daemon socket"),
        "second run must report the socket already gone; got: {stdout2:?}"
    );
}

/// (b) `--keep-caches` leaves both fake cache trees intact; the socket is
/// still removed.
#[test]
fn keep_caches_leaves_cache_trees_intact() {
    let f = fixture();
    let socket = f.socket();
    let cache_tree = f.cache_tree();

    let req = uninstall_req(&f, &socket, &cache_tree, true, false);
    let mut out = Cursor::new(Vec::new());
    let mut err = Cursor::new(Vec::new());
    uninstall::run(&req, &mut no_op_stop, &mut out, &mut err).unwrap();

    assert!(
        cache_tree.exists(),
        "--keep-caches must leave the native cache tree"
    );
    assert!(
        f.npx_ours().exists(),
        "--keep-caches must leave npx entries"
    );
    assert!(!socket.exists(), "socket removal is not cache-gated");
    let stdout = String::from_utf8(out.into_inner()).unwrap();
    assert!(
        stdout.contains("--keep-caches"),
        "must announce the skip; got: {stdout:?}"
    );
}

/// `--all-npx` wipes the entire npx cache root — after a loud stderr warning.
#[test]
fn all_npx_wipes_whole_npx_cache_with_warning() {
    let f = fixture();
    let socket = f.socket();
    let cache_tree = f.cache_tree();

    let req = uninstall_req(&f, &socket, &cache_tree, false, true);
    let mut out = Cursor::new(Vec::new());
    let mut err = Cursor::new(Vec::new());
    uninstall::run(&req, &mut no_op_stop, &mut out, &mut err).unwrap();

    assert!(
        !f.npx.path().exists(),
        "--all-npx must remove the entire npx cache root"
    );
    let stderr = String::from_utf8(err.into_inner()).unwrap();
    assert!(
        stderr.contains("--all-npx") && stderr.contains("ENTIRE"),
        "loud warning must land on stderr before deletion; got: {stderr:?}"
    );
}

/// Outside any project root the registry step is a benign skip, not an error.
#[test]
fn uninstall_outside_project_root_skips_registry_benignly() {
    let bare = tempfile::tempdir().unwrap(); // no sentinel anywhere up-tree
    let daemon_home = tempfile::tempdir().unwrap();
    let socket = daemon_home.path().join("dreamd.sock");
    let cache_tree = daemon_home.path().join("no-cache");
    let npx = daemon_home.path().join("no-npx");

    let req = uninstall::UninstallRequest {
        cwd: bare.path(),
        daemon_home: daemon_home.path(),
        socket: Some(&socket),
        cache_dir: Some(&cache_tree),
        npx_dir: Some(&npx),
        keep_caches: false,
        all_npx: false,
        quiet: false,
    };
    let mut out = Cursor::new(Vec::new());
    let mut err = Cursor::new(Vec::new());
    uninstall::run(&req, &mut no_op_stop, &mut out, &mut err).unwrap();

    let stdout = String::from_utf8(out.into_inner()).unwrap();
    assert!(
        stdout.contains("skipping registry unregister"),
        "must skip the registry step benignly; got: {stdout:?}"
    );
}

/// (c) `update --dry-run`: zero filesystem mutation, no stop attempt, stdout
/// mentions the version.
#[test]
fn update_dry_run_mutates_nothing_and_prints_version() {
    let f = fixture();
    let socket = f.socket();
    let cache_tree = f.cache_tree();

    let mut stop_called = false;
    let mut stop = |_: &mut dyn std::io::Write| {
        stop_called = true;
        StopReport {
            attempted: true,
            matched: false,
        }
    };

    let req = update::UpdateRequest {
        socket: Some(&socket),
        cache_dir: Some(&cache_tree),
        dry_run: true,
        quiet: false,
        restart: false,
        cargo_install_hint: None,
    };
    let mut out = Cursor::new(Vec::new());
    let mut err = Cursor::new(Vec::new());
    update::run(&req, &mut stop, &mut out, &mut err).unwrap();

    assert!(!stop_called, "dry-run must not attempt a process stop");
    assert!(socket.exists(), "dry-run must not remove the socket");
    assert!(
        cache_tree.join("0.1.0-test").join("dreamd").exists(),
        "dry-run must not clear the cache tree"
    );
    let stdout = String::from_utf8(out.into_inner()).unwrap();
    assert!(
        stdout.contains(VERSION_SHORT),
        "dry-run stdout must mention the version; got: {stdout:?}"
    );
    assert!(
        stdout.contains("no changes"),
        "dry-run must state nothing changed; got: {stdout:?}"
    );
    assert!(
        err.into_inner().is_empty(),
        "dry-run must not write to stderr"
    );
}

/// (d) Real update path: cache tree + socket cleared, npx cache untouched,
/// actionable `npx -y dreamd-mcp` next step printed.
#[test]
fn update_real_path_clears_cache_and_socket() {
    let f = fixture();
    let socket = f.socket();
    let cache_tree = f.cache_tree();

    let mut stop_called = false;
    let mut stop = |_: &mut dyn std::io::Write| {
        stop_called = true;
        StopReport {
            attempted: true,
            matched: false,
        }
    };

    let req = update::UpdateRequest {
        socket: Some(&socket),
        cache_dir: Some(&cache_tree),
        dry_run: false,
        quiet: false,
        restart: false,
        cargo_install_hint: None,
    };
    let mut out = Cursor::new(Vec::new());
    let mut err = Cursor::new(Vec::new());
    update::run(&req, &mut stop, &mut out, &mut err).unwrap();

    assert!(stop_called, "real update must attempt a process stop");
    assert!(!socket.exists(), "real update must remove the socket");
    assert!(
        !cache_tree.exists(),
        "real update must clear the cache tree"
    );
    assert!(
        f.npx_ours().exists(),
        "update must NOT touch the npx cache (scoped clear is uninstall-only)"
    );
    let stdout = String::from_utf8(out.into_inner()).unwrap();
    assert!(
        stdout.contains("npx -y dreamd-mcp"),
        "must print the actionable next step; got: {stdout:?}"
    );
    assert!(
        stdout.contains(VERSION_SHORT),
        "must print before/after version lines; got: {stdout:?}"
    );
    assert!(
        stdout.contains("native cache cleared"),
        "after-line must report the cache was cleared; got: {stdout:?}"
    );
}

/// Cargo-install hint surfaces the rebuild guidance on the real path.
#[test]
fn update_cargo_install_hint_prints_rebuild_guidance() {
    let daemon_home = tempfile::tempdir().unwrap();
    let socket = daemon_home.path().join("dreamd.sock");
    let cache_tree = daemon_home.path().join("cache-absent");

    let req = update::UpdateRequest {
        socket: Some(&socket),
        cache_dir: Some(&cache_tree),
        dry_run: false,
        quiet: false,
        restart: false,
        cargo_install_hint: Some("DREAMD_BIN is set".to_string()),
    };
    let mut out = Cursor::new(Vec::new());
    let mut err = Cursor::new(Vec::new());
    update::run(&req, &mut no_op_stop, &mut out, &mut err).unwrap();

    let stdout = String::from_utf8(out.into_inner()).unwrap();
    assert!(
        stdout.contains("cargo install --path crates/dreamd-cli"),
        "cargo-installed binaries need rebuild guidance; got: {stdout:?}"
    );
    assert!(
        stdout.contains("native cache already clean"),
        "absent cache must report already clean; got: {stdout:?}"
    );

    // --dry-run must surface the same hint: a cargo-installed user planning
    // an update needs to know cache-clear won't replace their binary.
    let dry_req = update::UpdateRequest {
        socket: Some(&socket),
        cache_dir: Some(&cache_tree),
        dry_run: true,
        quiet: false,
        restart: false,
        cargo_install_hint: Some("DREAMD_BIN is set".to_string()),
    };
    let mut dry_out = Cursor::new(Vec::new());
    let mut dry_err = Cursor::new(Vec::new());
    update::run(&dry_req, &mut no_op_stop, &mut dry_out, &mut dry_err).unwrap();
    let dry_stdout = String::from_utf8(dry_out.into_inner()).unwrap();
    assert!(
        dry_stdout.contains("cargo install --path crates/dreamd-cli"),
        "dry-run must include the cargo-install rebuild guidance; got: {dry_stdout:?}"
    );
}
