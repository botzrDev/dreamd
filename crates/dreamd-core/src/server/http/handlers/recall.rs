//! `GET /api/v1/recall` — BM25 × salience search over the project index.

use axum::extract::{Extension, RawQuery, State};
use axum::response::IntoResponse;
use tantivy::IndexReader;

use crate::collector::RecallExclusion;
use crate::ingress::RecallIngress;
use crate::registry::ProjectEntry;

use super::super::router::{error_400, error_500};
use super::super::state::AppState;
use super::super::types::RecallParams;

/// HTTP sibling of `crate::memory_store::recall_to_json` that also takes the
/// `exclude=` ids (BZR-194) and, on `explain=1`, adds a `citations` array
/// (BZR-198). Kept out of `memory_store.rs` since it has exactly one caller:
/// this handler. With no `exclude` and no `explain` the body is byte-identical
/// to `recall_to_json`.
fn recall_to_json_excluding(
    reader: &IndexReader,
    query: &str,
    k: u32,
    exclusion: &RecallExclusion,
    explain: bool,
    now_sec: i64,
) -> tantivy::Result<String> {
    let (_, schema_fields) = crate::index::build_schema();
    let results = crate::collector::recall_excluding(
        reader,
        &schema_fields,
        query,
        k as usize,
        None,
        exclusion,
        now_sec,
    )?;
    Ok(if explain {
        RecallIngress::to_json_with_citations(results, now_sec)
    } else {
        RecallIngress::to_json(results)
    })
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
/// * `exclude` (optional, repeatable) — drop this `event_id` before top-k
///   (BZR-194). Citations cover the filtered hits. No cluster parameter.
///
/// # Response (`200`)
/// `{"results":[{"score","bm25","salience","source","content","metadata":{…}}]}`,
/// plus `"citations":[…]` when `explain=1`.
pub(crate) async fn get_recall(
    State(state): State<AppState>,
    Extension(entry): Extension<ProjectEntry>,
    RawQuery(raw): RawQuery,
) -> axum::response::Response {
    let params = match RecallParams::from_query(raw.as_deref().unwrap_or("")) {
        Ok(p) => p,
        Err(e) => return error_400(&format!("Failed to deserialize query string: {e}")),
    };
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

    // Same recall body as the in-process MCP store (BZR-173) when `exclude` is
    // absent; only the reader differs. Content-Type matches what `axum::Json`
    // set before.
    let exclusion = RecallExclusion {
        ids: params.exclude.clone(),
        cluster_prefixes: Vec::new(),
    };
    let result = recall_to_json_excluding(
        &reader,
        &params.q,
        k,
        &exclusion,
        params.explain_requested(),
        now_sec,
    );

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
