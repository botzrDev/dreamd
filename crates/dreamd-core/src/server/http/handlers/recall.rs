//! `GET /api/v1/recall` — BM25 × salience search over the project index.

use axum::extract::{Extension, Query, State};
use axum::response::IntoResponse;

use crate::registry::ProjectEntry;

use super::super::router::error_500;
use super::super::state::AppState;
use super::super::types::{RecallMeta, RecallParams, RecallResponse, RecallResultJson};

/// `GET /api/v1/recall` — BM25 × salience search over the project index.
///
/// # Headers
/// * `X-Agent-Root` (required)
///
/// # Query
/// * `q` (required) — search string
/// * `k` (optional, default `5`) — max results
///
/// # Response (`200`)
/// `{"results":[{"score","bm25","salience","source","content","metadata":{…}}]}`
pub(crate) async fn get_recall(
    State(state): State<AppState>,
    Extension(entry): Extension<ProjectEntry>,
    Query(params): Query<RecallParams>,
) -> axum::response::Response {
    let k = params.k.unwrap_or(5);
    let now_sec = chrono::Utc::now().timestamp();

    // Resolve the reader for this project: the pinned primary handle when this
    // is the daemon's booted project (so recall sees the coordinator's live
    // appends — WEG-264 Defect 2), else a lazily-opened index_map handle.
    // IndexReader is Clone (Arc-backed); we clone it so the index_map mutex is
    // never held across the Tantivy search.
    let reader =
        match state.with_index_handle(std::path::Path::new(&entry.root), |h| h.reader().clone()) {
            Ok(r) => r,
            Err(e) => return error_500(&format!("index open failed: {e}")),
        };

    let (_, schema_fields) = crate::index::build_schema();

    match crate::recall(
        &reader,
        &schema_fields,
        &params.q,
        k as usize,
        None,
        now_sec,
    ) {
        Ok(results) => {
            let json_results: Vec<RecallResultJson> = results
                .into_iter()
                .map(|r| RecallResultJson {
                    score: r.score,
                    bm25: r.bm25,
                    salience: r.salience,
                    source: format!("{:?}", r.layer).to_lowercase(),
                    content: r.content,
                    metadata: RecallMeta {
                        timestamp_sec: r.timestamp_sec,
                        pain: r.pain,
                        importance: r.importance,
                        recurrence: r.recurrence,
                    },
                })
                .collect();
            (
                axum::http::StatusCode::OK,
                axum::Json(RecallResponse {
                    results: json_results,
                }),
            )
                .into_response()
        }
        Err(e) => error_500(&format!("recall failed: {e}")),
    }
}
