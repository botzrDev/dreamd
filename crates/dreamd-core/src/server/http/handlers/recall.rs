//! `GET /api/v1/recall` — BM25 × salience search over the project index.

use axum::extract::{Extension, Query, State};
use axum::response::IntoResponse;
use tantivy::IndexReader;

use crate::ingress::RecallIngress;
use crate::registry::ProjectEntry;

use super::super::router::error_500;
use super::super::state::AppState;
use super::super::types::RecallParams;

/// `explain=1` sibling of `crate::memory_store::recall_to_json`, adding a
/// `citations` array to the response. Kept out of `memory_store.rs` (BZR-198
/// leaves that file untouched) since it has exactly one caller: this handler.
fn recall_to_json_with_citations(
    reader: &IndexReader,
    query: &str,
    k: u32,
    now_sec: i64,
) -> tantivy::Result<String> {
    let (_, schema_fields) = crate::index::build_schema();
    let results = crate::recall(reader, &schema_fields, query, k as usize, None, now_sec)?;
    Ok(RecallIngress::to_json_with_citations(results, now_sec))
}

/// `GET /api/v1/recall` — BM25 × salience search over the project index.
///
/// # Headers
/// * `X-Agent-Root` (required)
///
/// # Query
/// * `q` (required) — search string
/// * `k` (optional, default [`crate::ingress::DEFAULT_RECALL_K`]) — max results
/// * `explain` (optional) — exact value `1` adds a `citations` array; any
///   other value, including absent, omits it
///
/// # Response (`200`)
/// `{"results":[{"score","bm25","salience","source","content","metadata":{…}}]}`,
/// plus `"citations":[…]` when `explain=1`.
pub(crate) async fn get_recall(
    State(state): State<AppState>,
    Extension(entry): Extension<ProjectEntry>,
    Query(params): Query<RecallParams>,
) -> axum::response::Response {
    let k = params.k_or_default();
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

    // Same recall body as the in-process MCP store (BZR-173); only the reader
    // differs. Content-Type matches what `axum::Json` set before.
    let result = if params.explain_requested() {
        recall_to_json_with_citations(&reader, &params.q, k, now_sec)
    } else {
        crate::memory_store::recall_to_json(&reader, &params.q, k, now_sec)
    };

    match result {
        Ok(json) => (
            axum::http::StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/json"),
            )],
            json,
        )
            .into_response(),
        Err(e) => error_500(&format!("recall failed: {e}")),
    }
}
