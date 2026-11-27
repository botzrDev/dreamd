//! DRM-164: `dreamd status` and `dreamd doctor` report an installed `dreamd
//! service` whose binary is gone, with the command that fixes it.
//!
//! Each test runs the built binary with a temp `HOME` that holds the unit
//! file, as `service install` writes it. Nothing here spawns `systemctl`:
//! `status` and `doctor` only read the unit file. Linux only, because the unit
//! file is the systemd backend's; the plist and task XML readers are unit
//! tested in `commands::service`.
#![cfg(all(unix, not(target_os = "macos")))]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn dreamd(dir: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dreamd"))
        .current_dir(dir)
        .env("HOME", home)
        .env("DREAMD_LOG", "off")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("DREAMD_SOCK")
        .env_remove("DREAMD_LAUNCHER")
        .env_remove("DREAMD_LOG_FORMAT")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run dreamd {args:?}: {e}"))
}

fn describe(out: &Output) -> String {
    format!(
        "exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// `$HOME/proj`, scaffolded by `dreamd init`.
fn init_project(home: &Path) -> PathBuf {
    let proj = home.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(
        proj.join("Cargo.toml"),
        "[package]\nname = \"drm164\"\nversion = \"0.0.0\"\n",
    )
    .unwrap();
    let init = dreamd(&proj, home, &["init"]);
    assert_eq!(init.status.code(), Some(0), "init: {}", describe(&init));
    proj
}

/// The unit `service install` writes, naming `exe`, in `proj`.
fn write_unit(home: &Path, exe: &Path, proj: &Path) -> PathBuf {
    let dir = home.join(".config/systemd/user");
    std::fs::create_dir_all(&dir).unwrap();
    let unit = dir.join("dreamd.service");
    std::fs::write(
        &unit,
        format!(
            "[Unit]\nDescription=dreamd memory daemon\n\n[Service]\nType=simple\n\
             ExecStart={} watch\nWorkingDirectory={}\nRestart=on-failure\nRestartSec=5\n\n\
             [Install]\nWantedBy=default.target\n",
            exe.display(),
            proj.display()
        ),
    )
    .unwrap();
    unit
}

#[test]
fn status_and_doctor_report_a_service_whose_binary_is_gone() {
    let home = tempfile::tempdir().unwrap();
    let proj = init_project(home.path());

    let doctor = dreamd(&proj, home.path(), &["doctor"]);
    assert_eq!(
        doctor.status.code(),
        Some(0),
        "baseline: {}",
        describe(&doctor)
    );

    let gone = home.path().join(".cache/dreamd-mcp/1.3.1/dreamd");
    let unit = write_unit(home.path(), &gone, &proj);
    // The test binary is not in the shim cache, so the fix names `dreamd`.
    let report = format!(
        "the dreamd service ({}) runs {}, which no longer exists, so it cannot start. To fix \
         it, from {}, run `dreamd service install`, then `dreamd service restart`",
        unit.display(),
        gone.display(),
        proj.display()
    );

    let status = dreamd(&proj, home.path(), &["status"]);
    let want = format!("service: {report}");
    assert!(
        String::from_utf8_lossy(&status.stdout)
            .lines()
            .any(|l| l == want),
        "want {want:?}\n{}",
        describe(&status)
    );

    let doctor = dreamd(&proj, home.path(), &["doctor"]);
    assert_eq!(doctor.status.code(), Some(1), "{}", describe(&doctor));
    let want = format!("service_binary: missing  [WARNING: {report}]");
    assert!(
        String::from_utf8_lossy(&doctor.stdout)
            .lines()
            .any(|l| l == want),
        "want {want:?}\n{}",
        describe(&doctor)
    );
}

#[test]
fn status_and_doctor_say_nothing_about_a_service_whose_binary_is_there() {
    let home = tempfile::tempdir().unwrap();
    let proj = init_project(home.path());
    write_unit(home.path(), Path::new(env!("CARGO_BIN_EXE_dreamd")), &proj);

    let status = dreamd(&proj, home.path(), &["status"]);
    assert!(
        !String::from_utf8_lossy(&status.stdout).contains("service"),
        "{}",
        describe(&status)
    );
    let doctor = dreamd(&proj, home.path(), &["doctor"]);
    assert_eq!(doctor.status.code(), Some(0), "{}", describe(&doctor));
    assert!(
        !String::from_utf8_lossy(&doctor.stdout).contains("service_binary"),
        "{}",
        describe(&doctor)
    );
}
