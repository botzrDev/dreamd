//! `dreamd migrate` framework (WEG-133 / DR-108) — episodic schema migration registry.
//!
//! v0.1 ships a **stub**: exactly one registered transform, the identity
//! migration for the current episodic record schema
//! ([`dreamd_protocol::RECORD_SCHEMA_VERSION`], `1.0.0`), which is a no-op
//! success. The trait + registry exist so v0.1.1 can register real durable
//! transforms without reshaping the CLI.
//!
//! `--from` / `--to` name the **episodic record** schema
//! ([`dreamd_protocol::RECORD_SCHEMA_VERSION`]) — three independent version
//! streams coexist and only this one is the migrate token:
//!
//! * episodic record schema — `dreamd_protocol::RECORD_SCHEMA_VERSION` (`1.0.0`)
//! * daemon [`STATE_SCHEMA`](crate::wal::STATE_SCHEMA_VERSION) — the `state.json`
//!   `schema_version`; the `dreamd version` **display** line prints this. Never a
//!   `--from`/`--to` token.
//! * Tantivy index schema — [`crate::index::SCHEMA_VERSION`] (`index/1.3`). The
//!   index self-heals on version mismatch (ARCHITECTURE.md §4) and is **never** a
//!   `migrate` target.
//!
//! The identity migration's [`apply`](Migration::apply) is side-effect-free: the
//! CLI owns the durable-file `.bak` copies, so the registry stays a pure lookup
//! table.

use crate::layout::AgentRoot;

/// Failure returned by [`Migration::apply`].
///
/// The v0.1 identity migration never constructs one (its `apply` is a no-op);
/// the variant gives real v0.1.1 transforms a home to `?`-propagate durable I/O
/// failures through.
#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    /// I/O failure while applying a durable transform.
    #[error("migration i/o failure: {0}")]
    Io(#[from] std::io::Error),
}

/// One registered schema transform between two episodic record versions.
///
/// `from_version` / `to_version` mirror the `dreamd migrate --from` / `--to`
/// flag names, so they read `&self` despite the `from_*` prefix that
/// `clippy::wrong_self_convention` would otherwise flag.
#[allow(clippy::wrong_self_convention)]
pub trait Migration {
    /// The episodic record schema this migration reads.
    fn from_version(&self) -> &str;
    /// The episodic record schema this migration produces.
    fn to_version(&self) -> &str;
    /// Apply the durable transform.
    ///
    /// The v0.1 identity migration is a no-op (`Ok(())`) that reads and writes
    /// nothing. Real v0.1.1 transforms rewrite `AGENT_LEARNINGS.jsonl` here (the
    /// CLI has already taken the `.bak` copies before calling this).
    fn apply(&self, agent_root: &AgentRoot) -> Result<(), MigrateError>;
}

/// A no-op migration whose `from` and `to` are the same version — the only
/// transform registered in v0.1 (`1.0.0` → `1.0.0`).
pub struct IdentityMigration {
    version: &'static str,
}

impl IdentityMigration {
    /// Register the identity (no-op) transform for `version`.
    pub fn new(version: &'static str) -> Self {
        Self { version }
    }
}

impl Migration for IdentityMigration {
    fn from_version(&self) -> &str {
        self.version
    }

    fn to_version(&self) -> &str {
        self.version
    }

    /// No-op: the current episodic schema needs no transform. Side-effect-free
    /// by contract — the CLI owns the `.bak` copies.
    fn apply(&self, _agent_root: &AgentRoot) -> Result<(), MigrateError> {
        Ok(())
    }
}

/// Ordered registry of the migrations a given `dreamd` release knows how to run.
pub struct MigrationRegistry {
    migrations: Vec<Box<dyn Migration>>,
}

impl MigrationRegistry {
    /// The v0.1 registry: exactly one transform, the identity migration for the
    /// current episodic record schema
    /// ([`dreamd_protocol::RECORD_SCHEMA_VERSION`]). Any other `(from, to)` pair
    /// is unregistered and must be rejected by the CLI.
    pub fn v0_1() -> Self {
        Self {
            migrations: vec![Box::new(IdentityMigration::new(
                dreamd_protocol::RECORD_SCHEMA_VERSION,
            ))],
        }
    }

    /// Find the migration registered for `from → to`, if any.
    pub fn find(&self, from: &str, to: &str) -> Option<&dyn Migration> {
        self.migrations
            .iter()
            .find(|m| m.from_version() == from && m.to_version() == to)
            .map(|m| m.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v0_1_registers_identity_for_current_record_schema() {
        // The one registered path is the current episodic record schema, which
        // is 1.0.0 today. Assert against both the literal and the constant so
        // the tie is locked if the constant ever drifts.
        assert_eq!(dreamd_protocol::RECORD_SCHEMA_VERSION, "1.0.0");
        let reg = MigrationRegistry::v0_1();
        let m = reg
            .find("1.0.0", "1.0.0")
            .expect("identity 1.0.0 → 1.0.0 must be registered");
        assert_eq!(m.from_version(), "1.0.0");
        assert_eq!(m.to_version(), "1.0.0");
    }

    #[test]
    fn identity_apply_is_a_noop_ok() {
        let reg = MigrationRegistry::v0_1();
        let m = reg.find("1.0.0", "1.0.0").unwrap();
        // apply is side-effect-free; the path need not exist.
        let root = AgentRoot::new("/tmp/dreamd-migrate-identity-noop");
        assert!(m.apply(&root).is_ok(), "identity apply must be a no-op Ok");
    }

    #[test]
    fn find_misses_unregistered_pairs() {
        let reg = MigrationRegistry::v0_1();
        // The daemon-state / display token is never a registered pair.
        assert!(reg.find("1.0", "1.0").is_none());
        // Forward transforms are not registered in the v0.1 stub.
        assert!(reg.find("1.0.0", "1.1.0").is_none());
        assert!(reg.find("1.0.0", "2.0.0").is_none());
        // Reversed / partial pairs miss too.
        assert!(reg.find("1.1.0", "1.0.0").is_none());
        // The index schema string is never a migrate target.
        assert!(reg.find("index/1.3", "index/1.3").is_none());
    }
}
