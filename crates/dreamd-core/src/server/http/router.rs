//! Route mounting, middleware, and HTTP error responses.

use axum::http::StatusCode;
use axum::response::IntoResponse;

use super::handlers::{get_health, get_preferences, get_recall, post_dream, post_learn};
use super::state::AppState;

/// Peer UID injected at connection-accept time by `serve_uds`.
/// Extracted by `peer_uid_middleware` to enforce daemon-owner-only access.
#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
pub struct PeerUid(pub u32);

/// Build the `/api/v1` router with `X-Agent-Root` validation middleware and,
/// on Unix, `peer_uid_middleware` (WEG-72 / DR-407) as the outermost layer.
pub fn build_router(state: AppState) -> axum::Router {
    let router = axum::Router::new()
        .route("/api/v1/learn", axum::routing::post(post_learn))
        .route("/api/v1/recall", axum::routing::get(get_recall))
        .route("/api/v1/preferences", axum::routing::get(get_preferences))
        .route("/api/v1/health", axum::routing::get(get_health))
        .route("/api/v1/dream", axum::routing::post(post_dream))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            agent_root_middleware,
        ));

    #[cfg(unix)]
    let router = router.layer(axum::middleware::from_fn_with_state(
        state.clone(),
        peer_uid_middleware,
    ));

    router.with_state(state)
}

/// Validate `X-Agent-Root` and resolve the project via `registry.toml`.
///
/// Inserts [`ProjectEntry`] into request extensions on success.
///
/// # Errors
/// * `400` — header missing or non-UTF-8
/// * `404` — path not registered
/// * `500` — registry read/parse failure
pub async fn agent_root_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let agent_root_str = match req
        .headers()
        .get("x-agent-root")
        .and_then(|v| v.to_str().ok())
    {
        Some(s) => s.to_owned(),
        None => {
            return error_400("missing or non-UTF-8 X-Agent-Root header");
        }
    };

    let agent_root_path = std::path::Path::new(&agent_root_str);

    match crate::registry::resolve_project(&state.registry_path, agent_root_path) {
        Ok(Some(entry)) => {
            req.extensions_mut().insert(entry);
            next.run(req).await
        }
        Ok(None) => error_404("agent root not registered"),
        Err(_) => error_500("registry read failed"),
    }
}

/// Enforce daemon-owner-only access via peer UID from `SO_PEERCRED`.
///
/// The UDS accept loop injects [`PeerUid`] before the HTTP stack runs.
/// Compares against [`AppState::daemon_uid`] set at `run_watch` startup.
///
/// # Errors
/// * `403` — UID mismatch or extension missing
#[cfg(unix)]
pub async fn peer_uid_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let peer_uid = req.extensions().get::<PeerUid>().map(|p| p.0);

    match peer_uid {
        Some(uid) if uid == state.daemon_uid => next.run(req).await,
        Some(uid) => {
            tracing::warn!(
                peer_uid = uid,
                daemon_uid = state.daemon_uid,
                "UID mismatch -- 403"
            );
            error_403("forbidden: peer UID does not match daemon owner")
        }
        None => error_403("forbidden: peer UID not available"),
    }
}

#[cfg(unix)]
pub(crate) fn error_403(msg: &str) -> axum::response::Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

pub(crate) fn error_400(msg: &str) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

pub(crate) fn error_404(msg: &str) -> axum::response::Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

pub(crate) fn error_500(msg: &str) -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}
