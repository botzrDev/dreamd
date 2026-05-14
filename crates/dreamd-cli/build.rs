//! Build script for `dreamd` CLI binary.
//!
//! Bakes compile-time metadata (git SHA, build date, target triple) into env
//! vars via `vergen-gitcl` so `commands::version` can assemble version strings
//! as `const` values. See the drift catalog entry "vergen-gitcl
//! `fail_on_error(false)` emits sentinels" in CLAUDE.md.

use vergen_gitcl::{BuildBuilder, CargoBuilder, Emitter, GitclBuilder, RustcBuilder};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    Emitter::default()
        .add_instructions(&BuildBuilder::all_build()?)?
        .add_instructions(&CargoBuilder::all_cargo()?)?
        .add_instructions(&RustcBuilder::all_rustc()?)?
        // `sha(false)` emits the full 40-char SHA; version.rs truncates to 7
        // chars via `str_index!` so both real SHAs and the "unknown" fallback
        // go through the same code path.
        .add_instructions(&GitclBuilder::default().sha(false).build()?)?
        // Suppress vergen warnings on tarball builds (no .git/) so they don't
        // clutter CI output. Failed instructions still emit the
        // VERGEN_IDEMPOTENT_OUTPUT sentinel; `or_unknown` in version.rs
        // converts it to "unknown".
        .quiet()
        .emit()?;
    Ok(())
}
