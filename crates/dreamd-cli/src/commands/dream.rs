//! Handler for `dreamd dream` — runs the deterministic dream cycle.

use std::fmt;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use dreamd_core::{
    consolidation::{run_deterministic_dream_cycle, DreamCycleError},
    decay::{run_decay_pruner, DecayError},
    layout::AgentRoot,
    server::{TantivyIndexHandle, DEFAULT_COMMIT_CADENCE},
};

/// Errors produced by the `dreamd dream` command.
#[derive(Debug)]
pub enum DreamCliError {
    DreamCycle(DreamCycleError),
    Decay(DecayError),
    Index(String),
    Io(std::io::Error),
}

impl fmt::Display for DreamCliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DreamCycle(e) => write!(f, "dream cycle error: {e}"),
            Self::Decay(e) => write!(f, "decay error: {e}"),
            Self::Index(s) => write!(f, "index error: {s}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for DreamCliError {}

impl From<std::io::Error> for DreamCliError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Run the full deterministic dream cycle from the CLI.
pub fn run(project_root: &Path, out: &mut impl Write) -> Result<(), DreamCliError> {
    let agent_root = AgentRoot::new(project_root);

    // One SystemTime::now() call — both values derive from it.
    let now_sec = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let cycle_date = chrono::DateTime::from_timestamp(now_sec, 0)
        .expect("valid unix timestamp")
        .format("%Y-%m-%d")
        .to_string();

    run_deterministic_dream_cycle(&agent_root, now_sec).map_err(DreamCliError::DreamCycle)?;

    let result =
        run_decay_pruner(&agent_root, now_sec, &cycle_date).map_err(DreamCliError::Decay)?;

    // Capture counts before moving result.decayed_ids.
    let decayed_count = result.decayed_ids.len();
    let kept_count = result.kept_count;

    // Tantivy prune — Phase 1 pattern: open fresh handle per call.
    // main() is sync, so spin up a new current-thread runtime.
    // Do NOT use Handle::current().block_on() — panics inside existing runtime.
    if decayed_count > 0 {
        let handle = TantivyIndexHandle::open(&agent_root, DEFAULT_COMMIT_CADENCE)
            .map_err(|e| DreamCliError::Index(e.to_string()))?;
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(handle.prune_decayed_events(result.decayed_ids))
            .map_err(|e| DreamCliError::Index(e.to_string()))?;
    }

    writeln!(
        out,
        "dream cycle complete ({decayed_count} events decayed, {kept_count} kept)",
    )?;
    Ok(())
}
