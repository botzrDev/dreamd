//! Integration tests for `dreamd init --uninstall-project`.
//!
//! Defends three invariant classes:
//!   1. Registry mutation correctness: a registered project is cleanly removed.
//!   2. Idempotency: double-uninstall and uninstall-when-absent are both Ok.
//!   3. Quiet-mode output suppression (golden-file byte-lock contract).
//!
//! These tests exercise the public `init::uninstall_project` function
//! directly (in-process), not via subprocess. Subprocess-level testing
//! of the `--uninstall-project` flag is covered by the CLI help snapshot
//! tests in `tests/snapshots/`.

use std::io::Cursor;

use dreamd::commands::init;

fn fake_daemon_home() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn fake_project_root() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    // Create a sentinel so find_project_root succeeds.
    std::fs::write(dir.path().join("Cargo.toml"), b"[package]").unwrap();
    dir
}

/// Helper: register a project via the normal path, return (daemon_home, project_dir).
fn register_project() -> (tempfile::TempDir, tempfile::TempDir) {
    let daemon_home = fake_daemon_home();
    let project = fake_project_root();
    let mut out = Cursor::new(Vec::new());
    let mut err = Cursor::new(Vec::new());
    init::run(
        project.path(),
        daemon_home.path(),
        true, // quiet — don't need the full scaffold output
        &mut out,
        &mut err,
    )
    .unwrap();
    (daemon_home, project)
}

/// Happy-path contract: a registered project is removed from the TOML
/// and the success message appears on stdout.
#[test]
fn uninstall_removes_registry_entry() {
    let (daemon_home, project) = register_project();
    let registry_path = daemon_home.path().join("registry.toml");
    assert!(registry_path.exists(), "registry must exist after init");

    let raw = std::fs::read_to_string(&registry_path).unwrap();
    assert!(raw.contains("root ="), "registry must have an entry");

    let mut out = Cursor::new(Vec::new());
    let mut err = Cursor::new(Vec::new());
    init::uninstall_project(
        project.path(),
        daemon_home.path(),
        false,
        &mut out,
        &mut err,
    )
    .unwrap();

    let stdout = String::from_utf8(out.into_inner()).unwrap();
    assert!(
        stdout.contains("unregistered .agent/"),
        "expected success message, got: {stdout:?}"
    );

    let raw2 = std::fs::read_to_string(&registry_path).unwrap();
    assert!(
        !raw2.contains(project.path().to_string_lossy().as_ref()),
        "project root must not appear in registry after uninstall"
    );
}

/// Registry-absent case must return Ok, not panic or Err.
#[test]
fn uninstall_when_not_registered_exits_ok() {
    let daemon_home = fake_daemon_home();
    let project = fake_project_root();

    let mut out = Cursor::new(Vec::new());
    let mut err = Cursor::new(Vec::new());
    let result = init::uninstall_project(
        project.path(),
        daemon_home.path(),
        false,
        &mut out,
        &mut err,
    );
    assert!(result.is_ok(), "must exit Ok when not registered");

    let stdout = String::from_utf8(out.into_inner()).unwrap();
    assert!(
        stdout.contains("nothing to do"),
        "expected benign message, got: {stdout:?}"
    );
}

/// Double-uninstall must not panic, error, or corrupt the registry.
#[test]
fn uninstall_is_idempotent() {
    let (daemon_home, project) = register_project();

    // First uninstall
    let mut out = Cursor::new(Vec::new());
    let mut err = Cursor::new(Vec::new());
    init::uninstall_project(project.path(), daemon_home.path(), true, &mut out, &mut err).unwrap();

    // Second uninstall — should still be Ok
    let mut out2 = Cursor::new(Vec::new());
    let mut err2 = Cursor::new(Vec::new());
    let result = init::uninstall_project(
        project.path(),
        daemon_home.path(),
        false,
        &mut out2,
        &mut err2,
    );
    assert!(result.is_ok(), "second uninstall must be Ok");

    let stdout2 = String::from_utf8(out2.into_inner()).unwrap();
    assert!(
        stdout2.contains("nothing to do"),
        "second uninstall must print benign message, got: {stdout2:?}"
    );
}

/// Quiet mode must suppress all stdout, defending the golden-file
/// byte-lock contract.
#[test]
fn quiet_uninstall_produces_no_output() {
    let (daemon_home, project) = register_project();

    let mut out = Cursor::new(Vec::new());
    let mut err = Cursor::new(Vec::new());
    init::uninstall_project(project.path(), daemon_home.path(), true, &mut out, &mut err).unwrap();

    let stdout = String::from_utf8(out.into_inner()).unwrap();
    assert!(
        stdout.is_empty(),
        "quiet mode must produce no output, got: {stdout:?}"
    );
}

/// M4 (DR-421): registry.toml must be 0600 after a register write.
#[cfg(unix)]
#[test]
fn register_project_registry_file_has_0600_perms() {
    use std::os::unix::fs::PermissionsExt;
    let (daemon_home, _project) = register_project();
    let registry = dreamd_core::DaemonHome::new(daemon_home.path()).registry_toml();
    let mode = std::fs::metadata(&registry).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "registry perms after register should be 0600, got {:o}",
        mode
    );
}

/// M4 (DR-421): uninstall rewrites registry.toml via write_atomic — perms must survive.
#[cfg(unix)]
#[test]
fn uninstall_project_registry_file_keeps_0600_perms() {
    use std::os::unix::fs::PermissionsExt;
    let (daemon_home, project) = register_project();
    let registry = dreamd_core::DaemonHome::new(daemon_home.path()).registry_toml();
    let mut out = Cursor::new(Vec::new());
    let mut err = Cursor::new(Vec::new());
    init::uninstall_project(project.path(), daemon_home.path(), true, &mut out, &mut err)
        .expect("uninstall ok");
    let mode = std::fs::metadata(&registry).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "registry perms after uninstall should stay 0600, got {:o}",
        mode
    );
}
