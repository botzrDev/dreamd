//! Shared HTTP/MCP ingress — validation, redaction, and wire mapping.
//!
//! [`LearnIngress`] and [`RecallIngress`] are the single source of truth for
//! learn/recall ingress rules. HTTP handlers and MCP tool implementations call
//! into these modules so both surfaces reject and redact identically.

mod learn;
mod recall;
pub mod wire;

pub use learn::{LearnIngress, LearnRequest, LearnValidationError, DEFAULT_SCORE};
pub use recall::RecallIngress;
pub use wire::{
    k_too_large_message, query_too_long_message, CitationJson, LearnResponse, RecallMeta,
    RecallParams, RecallResponse, RecallResultJson, DEFAULT_RECALL_K, MAX_RECALL_K,
    MAX_RECALL_QUERY_BYTES,
};
