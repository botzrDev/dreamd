//! `POST /api/v1/learn` — append one episodic learning durably.

use axum::extract::{Extension, Json, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::IntoResponse;
use dreamd_protocol::AgentLearning;
use tokio::sync::oneshot;

use crate::coordinator::{CoordinatorError, MemoryCoordinatorMsg};
use crate::ingress::LearnIngress;
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
/// * `X-Client-Dedup-Key` (optional) — idempotency key scoped per project
///
/// # Body
/// [`AgentLearning`] JSON; inbound `id` and `schema_version` are server-stamped.
///
/// # Responses
/// * `201` — `{"id","timestamp","deduplicated"}`
/// * `400` — invalid `skill_action` or score out of `0.0..=10.0`
/// * `413` — serialized line exceeds cap
/// * `503` — coordinator busy (`Retry-After: 1`)
pub(crate) async fn post_learn(
    State(state): State<AppState>,
    Extension(project): Extension<ProjectEntry>,
    headers: HeaderMap,
    Json(mut learning): Json<AgentLearning>,
) -> axum::response::Response {
    tracing::debug!(project_root = %project.root, "POST /api/v1/learn");

    // Step 1 — extract optional idempotency key.
    let client_dedup_key = headers
        .get("x-client-dedup-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned());

    // Step 2–3 — validate, normalise, and redact via shared learn ingress.
    if let Err(e) = LearnIngress::prepare_agent_learning(&mut learning, state.config.redaction) {
        return error_400(&e.to_string());
    }

    // Step 4 — capture timestamp before `learning` is moved (Option A).
    let timestamp = learning.timestamp.to_rfc3339();

    // Step 5 — resolve the coordinator that OWNS this project root (WEG-272),
    // then build and dispatch. Routing on `project.root` is what keeps a
    // `POST /learn` for project B out of the boot project's JSONL.
    let supervisor = match state.resolve_supervisor(std::path::Path::new(&project.root)) {
        Ok(s) => s,
        Err(e) => return error_500(&format!("coordinator routing failed: {e}")),
    };

    let (resp_tx, resp_rx) = oneshot::channel();
    let msg = MemoryCoordinatorMsg::AppendLearning {
        learning,
        client_dedup_key,
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

    // Step 6 — await durable write outcome.
    match resp_rx.await {
        Ok(Ok(outcome)) => (
            axum::http::StatusCode::CREATED,
            axum::Json(LearnResponse {
                id: outcome.id.as_str().to_owned(),
                timestamp,
                deduplicated: outcome.deduplicated,
            }),
        )
            .into_response(),
        Ok(Err(CoordinatorError::PayloadTooLarge { .. })) => (
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            axum::Json(serde_json::json!({ "error": "payload too large" })),
        )
            .into_response(),
        Ok(Err(_)) | Err(_) => error_500("coordinator error"),
    }
}
