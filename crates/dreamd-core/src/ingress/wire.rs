//! Shared JSON wire shapes for learn/recall ingress (HTTP + MCP).

use crate::coordinator::AppendOutcome;

/// Response body for a successful `POST /api/v1/learn`.
#[derive(serde::Serialize)]
pub struct LearnResponse {
    pub id: String,
    pub timestamp: String,
    pub deduplicated: bool,
}

impl LearnResponse {
    /// Single construction site for HTTP and MCP learn-success responses.
    pub fn from_append_outcome(outcome: &AppendOutcome) -> Self {
        Self {
            id: outcome.id.as_str().to_owned(),
            timestamp: outcome.timestamp.to_rfc3339(),
            deduplicated: outcome.deduplicated,
        }
    }
}

#[derive(serde::Deserialize)]
pub struct RecallParams {
    pub q: String,
    pub k: Option<u32>,
}

/// Default max results for recall when `k` is omitted (HTTP and MCP).
pub const DEFAULT_RECALL_K: u32 = 5;

impl RecallParams {
    /// Resolved `k` query param — [`DEFAULT_RECALL_K`] when omitted.
    pub fn k_or_default(&self) -> u32 {
        self.k.unwrap_or(DEFAULT_RECALL_K)
    }
}

#[derive(serde::Serialize)]
pub struct RecallResponse {
    pub results: Vec<RecallResultJson>,
}

#[derive(serde::Serialize)]
pub struct RecallResultJson {
    pub score: f64,
    pub bm25: f64,
    pub salience: f64,
    pub source: String,
    pub content: String,
    pub metadata: RecallMeta,
}

#[derive(serde::Serialize)]
pub struct RecallMeta {
    pub timestamp_sec: u64,
    pub pain: f64,
    pub importance: f64,
    pub recurrence: u64,
    pub skill_action: String,
    pub source_harness: String,
}
