//! Library crate for the `dreamd` CLI binary.
//!
//! Exists so integration tests (notably WEG-20 snapshot tests) can bind to
//! `cli::Cli`, `commands::version::VERSION_SHORT`, and
//! `commands::version::render_long()` directly, without spawning a subprocess.

pub mod cli;
pub mod commands;

pub use cli::run;

#[cfg(test)]
mod packaged_files {
    use std::path::Path;

    /// Each crate ships its own `LICENSE` because `cargo package` does not
    /// follow the workspace root. The bytes stay equal to the repo license.
    #[test]
    fn license_copies_match_the_repo_license() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let root_path = manifest.join("../../LICENSE");
        // A crates.io tarball has no parent repo. The copies inside each crate
        // are what that build distributes.
        let Ok(root) = std::fs::read(&root_path) else {
            return;
        };
        for rel in [
            "LICENSE",
            "../dreamd-core/LICENSE",
            "../dreamd-protocol/LICENSE",
        ] {
            let path = manifest.join(rel);
            let got =
                std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            assert_eq!(got, root, "{}", path.display());
        }
    }
}
