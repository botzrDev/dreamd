//! Shared JSON response shapes for HTTP handlers and MCP tools.
//!
//! Wire types live in [`crate::ingress::wire`]; this module re-exports them
//! for handler-local imports.

pub(crate) use crate::ingress::wire::{LearnResponse, RecallParams};
