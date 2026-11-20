//! `POST /api/v1/learn` — append one episodic learning durably.

use axum::extract::{Extension, Json, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::IntoResponse;
use tokio::sync::oneshot;

use crate::config::load_config;
use crate::coordinator::{CoordinatorError, MemoryCoordinatorMsg};
use crate::daemon_client::CONFIG_LOAD_FAILED;
use crate::ingress::{LearnIngress, LearnRequest};
use crate::registry::ProjectEntry;
use crate::server::lifecycle::CoordinatorSendError;

use super::super::router::{error_400, error_500};
use super::super::state::AppState;
use super::super::types::LearnResponse;

/// `POST /api/v1/learn` — append one episodic learning durably.
///
/// # Headers
/// * `X-Agent-Root` (required)
/// * `Content-Type: application/json` (required)
/// * `X-Client-Dedup-Key` (optional) — retry key scoped per project, read
///   when the body has no `client_dedup_key`
///
/// # Body
/// [`LearnRequest`] JSON. `id`, `schema_version`, and `timestamp` are
/// **not accepted from the client** — they are daemon-owned and server-stamped
/// on durable write. A body that still carries them is accepted and those
/// values ignored, never rejected.
///
/// # Responses
/// * `201` — `{"id","timestamp","deduplicated"}`
/// * `400` — invalid `skill_action` or score out of `0.0..=10.0`; an
///   `X-Client-Dedup-Key` that is not UTF-8; a body key and a header key that
///   differ
/// * `413` — serialized line exceeds cap
/// * `500` — the project's config does not load (`config load failed: …`);
///   nothing is written
/// * `503` — coordinator busy (`Retry-After: 1`)
///
/// Redaction follows the project's own `redaction`, read from its config on
/// every request, so an edit applies to the next append (DRM-103).
pub(crate) async fn post_learn(
    State(state): State<AppState>,
    Extension(project): Extension<ProjectEntry>,
    headers: HeaderMap,
    Json(req): Json<LearnRequest>,
) -> axum::response::Response {
    tracing::debug!(project_root = %project.root, "POST /api/v1/learn");

    // DRM-106: the key is the body's `client_dedup_key`, else the header,
    // decoded as UTF-8 from its bytes so a non-ASCII key is still a key. A
    // body key and a header key that differ are refused, so no caller is
    // half-deduped. Nothing is written on either refusal.
    let header_key = match headers.get("x-client-dedup-key") {
        None => None,
        Some(value) => match std::str::from_utf8(value.as_bytes()) {
            Ok(key) => Some(key.to_owned()),
            Err(_) => return error_400("X-Client-Dedup-Key is not UTF-8"),
        },
    };
    let client_dedup_key = match (req.client_dedup_key.clone(), header_key) {
        (Some(body), Some(header)) if body != header => {
            return error_400("client_dedup_key in the body and X-Client-Dedup-Key differ");
        }
        (Some(body), _) => Some(body),
        (None, header) => header,
    };

    // DRM-103: each project's own config, read on every append like the
    // in-process store does, so an edit applies to the next append.
    let config_root = std::path::PathBuf::from(&project.root);
    let config = match tokio::task::spawn_blocking(move || load_config(&config_root)).await {
        Ok(Ok(config)) => config,
        Ok(Err(e)) => return error_500(&format!("{CONFIG_LOAD_FAILED}: {e}")),
        Err(_) => return error_500(&format!("{CONFIG_LOAD_FAILED}: the read task failed")),
    };

    // ARCHITECTURE.md §9: the one ingress call for this path, for HTTP and
    // daemon-mode MCP alike; the coordinator does not re-check. It validates,
    // normalizes, redacts, and stamps the placeholder id. The scores arrive as
    // `f64` and are checked as sent (DRM-104). `Some(..)` (never `None`) keeps
    // the HTTP contract: both scores are required here, so `DEFAULT_SCORE`
    // never applies.
    let mut learning = match LearnIngress::build_agent_learning(
        &req.content,
        &req.source_harness,
        &req.skill_action,
        Some(req.pain),
        Some(req.importance),
        config.redaction,
    ) {
        Ok(l) => l,
        Err(e) => return error_400(&e.to_string()),
    };
    // `build_agent_learning` hardcodes `pinned: false` for MCP, which has no
    // such param; HTTP clients have always been able to set it, so carry it
    // across. Pinned by `learn_client_pinned_true_is_preserved`.
    learning.pinned = req.pinned;

    // Resolve the coordinator that OWNS this project root (WEG-272),
    // then build and dispatch. Routing on `project.root` is what keeps a
    // `POST /learn` for project B out of the boot project's JSONL.
    //
    // DRM-95: a coordinator whose store was replaced refuses the append before
    // it writes anything, and its refusal retires its runtime. The second
    // attempt resolves again, which reopens the project, so the caller does
    // not see the swap. One retry only.
    let root = std::path::Path::new(&project.root);
    let mut retried = false;
    loop {
        let supervisor = match state.resolve_supervisor(root).await {
            Ok(s) => s,
            Err(e) => return error_500(&format!("coordinator routing failed: {e}")),
        };

        let (resp_tx, resp_rx) = oneshot::channel();
        let msg = MemoryCoordinatorMsg::AppendLearning {
            learning: learning.clone(),
            client_dedup_key: client_dedup_key.clone(),
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

        return match resp_rx.await {
            Ok(Ok(outcome)) => (
                axum::http::StatusCode::CREATED,
                axum::Json(LearnResponse::from_append_outcome(&outcome)),
            )
                .into_response(),
            Ok(Err(CoordinatorError::PayloadTooLarge { .. })) => (
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                axum::Json(serde_json::json!({ "error": "payload too large" })),
            )
                .into_response(),
            Ok(Err(CoordinatorError::StoreReplaced(_))) if !retried => {
                retried = true;
                continue;
            }
            // DRM-51: the message names the store and what to do.
            Ok(Err(e @ CoordinatorError::StoreReplaced(_))) => error_500(&e.to_string()),
            Ok(Err(_)) | Err(_) => error_500("coordinator error"),
        };
    }
}
