//! CLI subcommand implementations.
//!
//! Dispatch lives in `cli.rs` (clap + exit codes). Prefer `dreamd init` for
//! first-time `.agent/` layout; `doctor` / discover paths diagnose an existing
//! workspace without rewriting golden files.

pub mod archive;
pub mod blame;
pub mod doctor;
pub mod dream;
pub mod forget;
pub mod init;
pub mod lifecycle_cleanup;
pub mod mcp;
pub mod memory;
pub mod migrate;
pub mod recall;
pub mod reset;
pub mod salience;
pub mod score;
pub mod service;
pub mod setup;
pub mod setup_mcp;
pub mod status;
pub mod uninstall;
pub mod update;
pub mod vectors;
pub mod version;
pub mod watch;

/// The bytes a refused writer must leave alone: the episodic log,
/// `state.json`, `LESSONS.md`, and every file under `.dreamd/index/`. A
/// missing file is recorded as absent, so creating one also shows as a change.
#[cfg(test)]
pub(crate) fn store_bytes(
    root: &dreamd_core::AgentRoot,
) -> std::collections::BTreeMap<std::path::PathBuf, Option<Vec<u8>>> {
    let mut paths = vec![root.episodic_jsonl(), root.state_json(), root.lessons_md()];
    let index_dir = root.dreamd_dir().join(dreamd_core::server::INDEX_DIR_NAME);
    if let Ok(entries) = std::fs::read_dir(&index_dir) {
        paths.extend(entries.map(|e| e.unwrap().path()));
    }
    paths
        .into_iter()
        .map(|p| {
            let bytes = std::fs::read(&p).ok();
            (p, bytes)
        })
        .collect()
}
