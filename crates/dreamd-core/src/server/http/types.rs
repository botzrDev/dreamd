//! Shared JSON response shapes for HTTP handlers and MCP tools.

/// Response body for a successful `POST /api/v1/learn`.
#[derive(serde::Serialize)]
pub(crate) struct LearnResponse {
    pub id: String,
    pub timestamp: String,
    pub deduplicated: bool,
}

#[derive(serde::Deserialize)]
pub(crate) struct RecallParams {
    pub q: String,
    pub k: Option<u32>,
}

#[derive(serde::Serialize)]
pub(crate) struct RecallResponse {
    pub results: Vec<RecallResultJson>,
}

#[derive(serde::Serialize)]
pub(crate) struct RecallResultJson {
    pub score: f64,
    pub bm25: f64,
    pub salience: f64,
    pub source: String,
    pub content: String,
    pub metadata: RecallMeta,
}

#[derive(serde::Serialize)]
pub(crate) struct RecallMeta {
    pub timestamp_sec: u64,
    pub pain: f64,
    pub importance: f64,
    pub recurrence: u64,
}
