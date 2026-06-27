//! `GET /api/v1/health` — index freshness relative to the JSONL source of truth.

use axum::extract::{Extension, State};
use axum::response::IntoResponse;

use crate::registry::ProjectEntry;

use super::super::router::error_500;
use super::super::state::AppState;

/// `GET /api/v1/health` — report whether the Tantivy watermark has caught up
/// to the JSONL tail for this project.
///
/// # Headers
/// * `X-Agent-Root` (required)
///
/// # Response (`200`)
/// `{"index":{"stale":false,"jsonl_tail_id":"evt_…","last_indexed_id":"evt_…","unindexed_count":0}}`
pub(crate) async fn get_health(
    State(_state): State<AppState>,
    Extension(entry): Extension<ProjectEntry>,
) -> axum::response::Response {
    let agent_root = crate::layout::AgentRoot::new(std::path::Path::new(&entry.root));

    match crate::server::assess_index_freshness(&agent_root) {
        Ok(index) => (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({ "index": index })),
        )
            .into_response(),
        Err(e) => error_500(&format!("index freshness check failed: {e}")),
    }
}
