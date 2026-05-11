//! Library crate for the `dreamd` CLI binary.
//!
//! Exists so integration tests (notably WEG-20 snapshot tests) can bind to
//! `cli::Cli`, `commands::version::VERSION_SHORT`, and
//! `commands::version::render_long()` directly, without spawning a subprocess.

pub mod cli;
pub mod commands;

pub use cli::run;
