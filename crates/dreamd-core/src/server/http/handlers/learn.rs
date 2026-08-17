//! `POST /api/v1/learn` — append one episodic learning durably.

use axum::extract::{Extension, Json, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::IntoResponse;
use tokio::sync::oneshot;

use crate::coordinator::{CoordinatorError, MemoryCoordinatorMsg};
use crate::ingress::LearnIngress;
use crate::registry::ProjectEntry;
use crate::server::lifecycle::CoordinatorSendError;

use super::super::router::{error_400, error_500};
use super::super::state::AppState;
use super::super::types::LearnResponse;

/// Wire body for `POST /api/v1/learn`.
///
/// Deliberately NOT the domain [`AgentLearning`]: `id`, `schema_version`, and
/// `timestamp` are daemon-owned (minted and stamped in `coordinator.rs` on every
/// durable write), but `AgentLearning` is also the *disk* record, where `id` is
/// authoritative and must keep parsing strictly (`episodic.rs`). One
/// `Deserialize` impl cannot serve both surfaces, so the wire gets its own type
/// rather than the protocol struct gaining a `skip_deserializing` that would
/// weaken the disk read. AILAB-175.
///
/// No `deny_unknown_fields`: a client that still sends `id` (every shipped doc
/// example did until this change) must be **ignored, not rejected** — that is
/// the acceptance criterion, and serde's default ignore-unknown gives it for
/// free.
///
/// [`AgentLearning`]: dreamd_protocol::AgentLearning
#[derive(serde::Deserialize)]
pub(crate) struct AppendLearningRequest {
    /// 0.0..=10.0 subjective friction. Required — unlike MCP, HTTP has never
    /// defaulted it, and `Some(..)` below keeps that contract.
    pub pain: f32,
    /// 0.0..=10.0 long-term relevance. Required, same reasoning as `pain`.
    pub importance: f32,
    /// Sticky flag; the only field with a serde default, matching
    /// `AgentLearning::pinned`. Client-settable and preserved (see below).
    #[serde(default)]
    pub pinned: bool,
    /// Clustering key; validated and normalised at ingress.
    pub skill_action: String,
    /// Provenance tag, e.g. `"cursor"`, `"claude-code"`.
    pub source_harness: String,
    /// Free-text body; redacted at ingress, 4 KiB cap enforced downstream.
    pub content: String,
}

/// `POST /api/v1/learn` — append one episodic learning durably.
///
/// # Headers
/// * `X-Agent-Root` (required)
/// * `Content-Type: application/json` (required)
/// * `X-Client-Dedup-Key` (optional) — idempotency key scoped per project
///
/// # Body
/// [`AppendLearningRequest`] JSON. `id`, `schema_version`, and `timestamp` are
/// **not accepted from the client** — they are daemon-owned and server-stamped
/// on durable write. A body that still carries them is accepted and those
/// values ignored, never rejected.
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
    Json(req): Json<AppendLearningRequest>,
) -> axum::response::Response {
    tracing::debug!(project_root = %project.root, "POST /api/v1/learn");

    let client_dedup_key = headers
        .get("x-client-dedup-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned());

    // Map the wire body onto the domain type through the same shared seam the
    // MCP path uses: validate, normalise, redact, and stamp the placeholder id.
    // Sole skill_action gate (ARCHITECTURE.md §9); coordinator does not re-check.
    // `Some(..)` (never `None`) preserves the HTTP contract — the optional
    // validators apply the same 0.0..=10.0 check, and MCP's `unwrap_or(5.0)`
    // default is unreachable from here.
    let mut learning = match LearnIngress::build_agent_learning(
        &req.content,
        &req.source_harness,
        &req.skill_action,
        Some(req.pain as f64),
        Some(req.importance as f64),
        state.config.redaction,
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

    match resp_rx.await {
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
        Ok(Err(_)) | Err(_) => error_500("coordinator error"),
    }
}
