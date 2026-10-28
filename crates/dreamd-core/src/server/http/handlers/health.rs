//! `GET /api/v1/health` — index freshness relative to the JSONL source of truth.

use axum::extract::{Extension, State};
use axum::response::IntoResponse;

use crate::registry::ProjectEntry;

use super::super::router::error_500;
use super::super::state::AppState;

/// `GET /api/v1/health` — report whether the index has caught up to the log
/// for this project.
///
/// On Unix the answer comes from the daemon's own coordinator and index
/// (BZR-1243): the coordinator's [`AppendMark`](crate::server::AppendMark)
/// and the indexer's [`CommitMark`](crate::server::CommitMark), read through
/// [`AppState::live_marks`] and combined by
/// [`live_index_freshness`](crate::server::live_index_freshness). No file is
/// read. A project that is not loaded is opened, as recall opens it. `stale`
/// is `true` while appends this daemon made are not yet committed, which is
/// normal for up to one commit cadence. A change to the log made outside the
/// daemon shows in `dreamd doctor`, not here.
///
/// Off Unix there is no index, so health compares the JSONL tail with
/// `index_progress.json` on disk through
/// [`assess_index_freshness`](crate::server::assess_index_freshness).
///
/// # Headers
/// * `X-Agent-Root` (required)
///
/// # Response (`200`)
/// `{"index":{"stale":false,"jsonl_tail_id":"evt_…","last_indexed_id":"evt_…","unindexed_count":0}}`
pub(crate) async fn get_health(
    State(state): State<AppState>,
    Extension(entry): Extension<ProjectEntry>,
) -> axum::response::Response {
    #[cfg(unix)]
    let result = state
        .live_marks(std::path::Path::new(&entry.root))
        .map(|(commit, append)| crate::server::live_index_freshness(&commit, &append));
    #[cfg(not(unix))]
    let result = {
        let _ = &state;
        let agent_root = crate::layout::AgentRoot::new(std::path::Path::new(&entry.root));
        crate::server::assess_index_freshness(&agent_root)
    };

    match result {
        Ok(index) => (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({ "index": index })),
        )
            .into_response(),
        Err(e) => error_500(&format!("index freshness check failed: {e}")),
    }
}
