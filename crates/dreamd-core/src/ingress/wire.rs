//! Shared JSON wire shapes for learn/recall ingress (HTTP + MCP).

/// Response body for a successful `POST /api/v1/learn`.
#[derive(serde::Serialize)]
pub struct LearnResponse {
    pub id: String,
    pub timestamp: String,
    pub deduplicated: bool,
}

#[derive(serde::Deserialize)]
pub struct RecallParams {
    pub q: String,
    pub k: Option<u32>,
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
