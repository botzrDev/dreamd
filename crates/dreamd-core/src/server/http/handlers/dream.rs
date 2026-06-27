//! `POST /api/v1/dream` — run a full deterministic dream cycle.

use axum::extract::{Extension, State};
use axum::http::{header, HeaderValue};
use axum::response::IntoResponse;
use tokio::sync::oneshot;

use crate::coordinator::MemoryCoordinatorMsg;
use crate::server::lifecycle::CoordinatorSendError;

use super::super::router::error_500;
use super::super::state::AppState;

/// `POST /api/v1/dream` — run a full deterministic dream cycle.
///
/// # Headers
/// * `X-Agent-Root` (required) — canonical project root path (registered in `registry.toml`)
///
/// # Responses
/// * `200` — `{"status":"ok"}` cycle completed
/// * `409` — `{"error":"dream cycle in progress"}` WAL guard
/// * `403` — peer UID mismatch ([`peer_uid_middleware`](crate::server::http::peer_uid_middleware))
/// * `404` — project not registered ([`agent_root_middleware`](crate::server::http::agent_root_middleware))
/// * `503` — coordinator busy (`Retry-After: 1`)
/// * `500` — coordinator, WAL, or indexer failure
pub(crate) async fn post_dream(
    State(state): State<AppState>,
    Extension(entry): Extension<crate::registry::ProjectEntry>,
) -> impl IntoResponse {
    use crate::dream_cycle::{self, IndexBackend, PostPhaseOptions};
    use std::time::{SystemTime, UNIX_EPOCH};

    let agent_root = crate::layout::AgentRoot::new(&entry.root);

    // One SystemTime::now() call — both now_sec and cycle_date derive from it.
    let now_sec = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let cycle_date = dream_cycle::cycle_date_from_now_sec(now_sec);

    // 409 guard: reject concurrent cycles.
    match dream_cycle::ensure_not_in_progress(&agent_root) {
        Ok(()) => {}
        Err(dream_cycle::DreamCycleError::InProgress) => {
            return (
                axum::http::StatusCode::CONFLICT,
                axum::Json(serde_json::json!({"error": "dream cycle in progress"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    }

    // WEG-63 — capture dirty state BEFORE the cycle runs.
    let dirty_at_cycle_start =
        crate::autobiography::check_dirty_at_cycle_start(std::path::Path::new(&entry.root))
            .unwrap_or_default();

    // WEG-271: route consolidation + decay through the coordinator so its
    // long-lived append fd is reopened after the cycle's atomic rename(s).
    // Running the rewrites inline here would orphan that fd and silently drop
    // every subsequent POST /learn. The coordinator runs its own root's cycle.
    // WEG-272: route the cycle to the coordinator that OWNS this project root,
    // not the boot coordinator — otherwise project B's dream cycle would run
    // against (and prune) the boot project's JSONL.
    let supervisor = match state.resolve_supervisor(std::path::Path::new(&entry.root)) {
        Ok(s) => s,
        Err(e) => return error_500(&format!("coordinator routing failed: {e}")),
    };

    let (resp_tx, resp_rx) = oneshot::channel();
    let msg = MemoryCoordinatorMsg::RunDreamCycle {
        now_sec,
        cycle_date: cycle_date.clone(),
        response_tx: resp_tx,
    };
    match supervisor.try_send(msg).await {
        Ok(()) => {}
        Err(CoordinatorSendError::Full) => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, HeaderValue::from_static("1"))],
                axum::Json(serde_json::json!({ "error": "coordinator busy, retry" })),
            )
                .into_response();
        }
        Err(CoordinatorSendError::Closed) => {
            return error_500("coordinator unavailable");
        }
    }
    let decay_result = match resp_rx.await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
        Err(_) => return error_500("coordinator dropped dream-cycle response"),
    };

    // Index + autobiography — owned by dream_cycle orchestration.
    let index_sender =
        match state.with_index_handle(std::path::Path::new(&entry.root), |h| h.sender()) {
            Ok(s) => s,
            Err(e) => {
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(serde_json::json!({"error": e.to_string()})),
                )
                    .into_response();
            }
        };

    if let Err(e) = dream_cycle::run_post_phases(PostPhaseOptions {
        agent_root: &agent_root,
        project_root: std::path::Path::new(&entry.root),
        cycle_date: &cycle_date,
        decay_result: &decay_result,
        dirty_at_cycle_start: &dirty_at_cycle_start,
        commit_autobiography: true,
        index: IndexBackend::Sender(index_sender),
    })
    .await
    {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response();
    }

    (
        axum::http::StatusCode::OK,
        axum::Json(serde_json::json!({"status": "ok"})),
    )
        .into_response()
}
