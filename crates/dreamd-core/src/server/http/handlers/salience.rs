//! `GET /api/v1/observability/salience` — salience distribution (BZR-195).

use axum::extract::{Extension, State};
use axum::response::IntoResponse;

use crate::registry::ProjectEntry;
use crate::salience::report::build_report;

use super::super::router::error_500;
use super::super::state::AppState;

/// `GET /api/v1/observability/salience` — total, histogram, top clusters, and
/// 7-day drift over every episodic doc. Writes today's snapshot under
/// `.agent/.dreamd/observability/`.
///
/// # Headers
/// * `X-Agent-Root` (required)
///
/// # Response (`200`)
/// `{"schema_version","date","total","mean","histogram","top_clusters","drift","drift_against"}`
pub(crate) async fn get_salience(
    State(state): State<AppState>,
    Extension(entry): Extension<ProjectEntry>,
) -> axum::response::Response {
    let now_sec = chrono::Utc::now().timestamp();

    // Same reader resolution as `get_recall`: clone out so the index_map mutex
    // is not held across the scan.
    let reader =
        match state.with_index_handle(std::path::Path::new(&entry.root), |h| h.reader().clone()) {
            Ok(r) => r,
            Err(e) => return error_500(&format!("index open failed: {e}")),
        };

    let observability_dir = crate::layout::AgentRoot::new(&entry.root)
        .dreamd_dir()
        .join("observability");
    match build_report(&reader, now_sec, &observability_dir) {
        Ok(report) => (axum::http::StatusCode::OK, axum::Json(report)).into_response(),
        Err(e) => error_500(&format!("salience report failed: {e}")),
    }
}
