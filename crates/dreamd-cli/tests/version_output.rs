//! Integration tests for `dreamd --version` and `dreamd version`.
//!
//! Defends against the WEG-18 sentinel-leak class of bug (`VERGEN_*` literal
//! reaching stdout). Unit tests in `commands::version` cover the in-process
//! constants; this file covers the user-visible binary output.

use std::process::Command;

fn dreamd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dreamd")
}

#[test]
fn short_version_format_and_no_sentinel() {
    let out = Command::new(dreamd_bin())
        .arg("--version")
        .output()
        .expect("run dreamd --version");

    assert!(out.status.success(), "exit={:?}", out.status.code());
    assert!(out.stderr.is_empty(), "stderr must be empty on success");

    let stdout = String::from_utf8(out.stdout).expect("stdout is utf-8");
    assert!(
        !stdout.contains("VERGEN_"),
        "vergen sentinel leaked into --version: {stdout}"
    );

    // Format: "dreamd 0.0.0 (<sha7> build:<date> target:<triple> schema:1.0)\n"
    let pkg_version = env!("CARGO_PKG_VERSION");
    let expected_prefix = format!("dreamd {pkg_version} (");
    assert!(
        stdout.starts_with(&expected_prefix),
        "expected prefix {expected_prefix:?}, got: {stdout:?}"
    );
    assert!(stdout.contains(" build:"), "missing build: field: {stdout}");
    assert!(
        stdout.contains(" target:"),
        "missing target: field: {stdout}"
    );
    assert!(
        stdout.contains(" schema:1.0)"),
        "missing schema:1.0): {stdout}"
    );
    assert!(
        stdout.ends_with('\n'),
        "expected trailing newline: {stdout:?}"
    );
}

#[test]
fn short_version_v_flag_matches_long_flag() {
    let v = Command::new(dreamd_bin()).arg("-V").output().unwrap();
    let long = Command::new(dreamd_bin())
        .arg("--version")
        .output()
        .unwrap();
    assert!(v.status.success() && long.status.success());
    assert_eq!(v.stdout, long.stdout, "-V and --version must agree");
}

#[test]
fn long_version_subcommand_format_and_no_sentinel() {
    let out = Command::new(dreamd_bin())
        .arg("version")
        .output()
        .expect("run dreamd version");

    assert!(out.status.success(), "exit={:?}", out.status.code());
    assert!(out.stderr.is_empty(), "stderr must be empty on success");

    let stdout = String::from_utf8(out.stdout).expect("stdout is utf-8");
    assert!(
        !stdout.contains("VERGEN_"),
        "vergen sentinel leaked into version: {stdout}"
    );

    let pkg_version = env!("CARGO_PKG_VERSION");
    for needle in [
        &format!("dreamd {pkg_version}\n"),
        "  commit:  ",
        "  built:   ",
        "  target:  ",
        "  schema:  1.0\n",
    ] {
        assert!(
            stdout.contains(needle),
            "expected {needle:?} in long version output:\n{stdout}"
        );
    }
}
