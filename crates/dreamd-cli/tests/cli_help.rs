//! WEG-20 (DR-803) — in-process snapshot tests for every published CLI surface.
//!
//! Five snapshots, all bound directly to in-process symbols (no subprocess):
//!   1. `dreamd --help`         — top-level clap help.
//!   2. `dreamd init --help`    — init subcommand help.
//!   3. `dreamd version --help` — version subcommand help.
//!   4. `dreamd --version`      — `VERSION_SHORT` const (WEG-18 contract).
//!   5. `dreamd version`        — `render_long()` fn (WEG-18 contract).
//!
//! Snapshots 1–3 are deterministic clap output and use no filters. Snapshots
//! 4–5 carry vergen-baked SHA/date/triple that drift per-build; they go
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

#[test]
fn snapshot_top_level_help() {
    assert_snapshot!("top_level_help", top_help());
}

#[test]
fn snapshot_init_help() {
    assert_snapshot!("init_help", subcommand_help("init"));
}

#[test]
fn snapshot_dream_help() {
    assert_snapshot!("dream_help", subcommand_help("dream"));
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
