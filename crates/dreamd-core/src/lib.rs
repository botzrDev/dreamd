//! dreamd core engine: episodic actor, Tantivy BM25 index, dream cycle, MCP,
//! and the daemon's HTTP server (a Unix domain socket on Unix, loopback TCP
//! with a bearer token elsewhere).
//!
//! Path resolution goes through [`layout`]. All call sites must resolve
//! `.agent/` and `~/.agent/` via [`AgentRoot`] / [`DaemonHome`] rather than
//! building path strings.

pub mod autobiography;
pub mod bisect;
#[cfg(unix)]
pub mod client;
pub mod collector;
pub mod config;
pub mod consolidation;
pub mod coordinator;
#[cfg(all(test, unix))]
mod crash_sweep;
pub mod daemon_client;
pub mod daemon_state;
pub mod decay;
pub mod dream_cycle;
pub mod episodic;
pub mod forget;
pub mod id_minter;
pub mod index;
pub mod ingress;
pub mod invariants;
pub mod io;
pub mod layout;
pub mod lessons;
pub mod llm;
pub mod mcp;
pub mod memory_diff;
pub mod memory_store;
pub mod migrate;
pub mod observability;
pub mod plan;
pub mod privacy;
pub mod provenance;
pub mod recurrence;
pub mod redaction;
pub mod registry;
pub mod retention;
pub mod salience;
pub use salience::{salience_with_context, RecurrenceContext};
#[cfg(test)]
mod test_support;
pub mod wal;
pub mod writer_lock;

pub use collector::{recall, score_by_salience, RecallResult, SalienceCollector};
pub use migrate::{IdentityMigration, MigrateError, Migration, MigrationRegistry};
pub use recurrence::{ClusterCounts, RecallReader};

pub mod server;
pub mod snapshot;
pub mod stage;
#[cfg(feature = "vectors")]
pub mod vectors;

pub use layout::{AgentRoot, DaemonHome, LayoutError, DEFAULT_WORKSPACE_MD, GITIGNORE_SNIPPET};
