//! Shared HTTP/MCP ingress — validation, redaction, and wire mapping.
//!
//! [`LearnIngress`] and [`RecallIngress`] are the single source of truth for
//! learn/recall ingress rules. HTTP handlers and MCP tool implementations call
//! into these modules so both surfaces reject and redact identically.

mod learn;
mod recall;
pub mod wire;

pub use learn::{LearnIngress, LearnValidationError};
pub use recall::RecallIngress;
pub use wire::{LearnResponse, RecallMeta, RecallParams, RecallResponse, RecallResultJson};
