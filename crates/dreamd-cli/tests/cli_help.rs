//! WEG-20 (DR-803) — in-process snapshot tests for every published CLI surface.
//!
//! Fourteen snapshots, all bound directly to in-process symbols (no subprocess):
//! top-level `--help`, each subcommand `--help` (archive, init, dream, mcp, doctor,
//! recall, score, watch, reset, status, version), nested `reset workspace --help`,
//! plus the WEG-18 version output contract (`VERSION_SHORT` and `render_long()`).
//!
//! Help snapshots are deterministic clap output and use no filters. Version
//! output snapshots carry vergen-baked SHA/date/triple that drift per-build; they go
//! through `with_settings!({ filters => ... }, ...)` to redact those fields.
//! The `\S+` patterns capture the `unknown` tarball-build sentinel identically
//! to real values, so source-tarball builds (renamed `.git`) pass unchanged.
//!
//! Color is forced off on the test-side clap builder. CI has no TTY, but
//! local dev does, and an unforced builder would emit ANSI on developer
//! machines and break shared snapshots.

use clap::{ColorChoice, CommandFactory};
use dreamd::cli::Cli;
use dreamd::commands::version::{render_long, VERSION_SHORT};
use insta::{assert_snapshot, with_settings};

fn top_help() -> String {
    Cli::command()
        .color(ColorChoice::Never)
        .render_long_help()
        .to_string()
}

fn subcommand_help(name: &str) -> String {
    Cli::command()
        .color(ColorChoice::Never)
        .find_subcommand_mut(name)
        .unwrap_or_else(|| panic!("subcommand {name:?} missing from Cli builder"))
        .render_long_help()
        .to_string()
}

fn nested_subcommand_help(parent: &str, child: &str) -> String {
    Cli::command()
        .color(ColorChoice::Never)
        .find_subcommand_mut(parent)
        .unwrap_or_else(|| panic!("subcommand {parent:?} missing from Cli builder"))
        .find_subcommand_mut(child)
        .unwrap_or_else(|| panic!("subcommand {child:?} missing under {parent:?}"))
        .render_long_help()
        .to_string()
}

#[test]
fn snapshot_top_level_help() {
    assert_snapshot!("top_level_help", top_help());
}

#[test]
fn snapshot_init_help() {
    assert_snapshot!("init_help", subcommand_help("init"));
}

#[test]
fn snapshot_archive_help() {
    assert_snapshot!("archive_help", subcommand_help("archive"));
}

#[test]
fn snapshot_dream_help() {
    assert_snapshot!("dream_help", subcommand_help("dream"));
}

#[test]
fn snapshot_mcp_help() {
    assert_snapshot!("mcp_help", subcommand_help("mcp"));
}

#[test]
fn snapshot_doctor_help() {
    assert_snapshot!("doctor_help", subcommand_help("doctor"));
}

#[test]
fn snapshot_recall_help() {
    assert_snapshot!("recall_help", subcommand_help("recall"));
}

#[test]
fn snapshot_score_help() {
    assert_snapshot!("score_help", subcommand_help("score"));
}

#[test]
fn snapshot_watch_help() {
    assert_snapshot!("watch_help", subcommand_help("watch"));
}

#[test]
fn snapshot_reset_help() {
    assert_snapshot!("reset_help", subcommand_help("reset"));
}

#[test]
fn snapshot_status_help() {
    assert_snapshot!("status_help", subcommand_help("status"));
}

#[test]
fn snapshot_reset_workspace_help() {
    assert_snapshot!(
        "reset_workspace_help",
        nested_subcommand_help("reset", "workspace")
    );
}

#[test]
fn snapshot_version_help() {
    assert_snapshot!("version_help", subcommand_help("version"));
}

#[test]
fn snapshot_version_short() {
    with_settings!({
        filters => vec![
            (r"\(\S+ build:", "([sha] build:"),
            (r"build:\S+", "build:[date]"),
            (r"target:\S+", "target:[target]"),
        ]
    }, {
        assert_snapshot!("version_short", VERSION_SHORT);
    });
}

#[test]
fn snapshot_version_long() {
    with_settings!({
        filters => vec![
            (r"commit:\s+\S+", "commit:  [sha]"),
            (r"built:\s+\S+",  "built:   [date]"),
            (r"target:\s+\S+", "target:  [target]"),
        ]
    }, {
        assert_snapshot!("version_long", render_long());
    });
}
