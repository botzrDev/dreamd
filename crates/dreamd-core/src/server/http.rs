//! Axum 0.8 HTTP server skeleton (WEG-67 / DR-401).
//!
//! `AppState` — shared state cloned into every request.
//! `build_router` — mounts `/api/v1` routes with `X-Agent-Root` middleware.
//! `agent_root_middleware` — validates header + registry lookup on every request.
//!
//! Out of scope here: TraceLayer (WEG-144), SO_PEERCRED (WEG-72), TCP binding
//! (WEG-73), TantivyIndexHandle::reader (WEG-69).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::response::IntoResponse;

use crate::config::Config;
use crate::server::{ProjectIndexMap, Supervisor, TantivyIndexHandle};

/// Shared application state cloned into every Axum request via
/// `State<AppState>`. All fields behind `Arc` so cloning is cheap.
///
/// `registry_path` — path to `~/.agent/registry.toml`. Per-request
/// middleware calls `resolve_project(&state.registry_path, agent_root)`.
///
/// `supervisor` — owned by the process entry point; handlers call
/// `state.supervisor.try_send(msg)` for coordinator dispatch.
///
/// `config` — layered runtime config loaded at startup.
///
/// `index_map` — lazy-opened per-project Tantivy handles. `Mutex` (not
/// `RwLock`) because `ProjectIndexMap::get_or_open` is `&mut self` even
/// for reads (mutates LRU ordering).
#[derive(Clone)]
pub struct AppState {
    pub registry_path: PathBuf,
    pub supervisor: Arc<Supervisor>,
    pub config: Arc<Config>,
    pub index_map: Arc<Mutex<ProjectIndexMap<TantivyIndexHandle>>>,
}

impl AppState {
    pub fn new(
        registry_path: PathBuf,
        supervisor: Supervisor,
        config: Config,
        index_map: ProjectIndexMap<TantivyIndexHandle>,
    ) -> Self {
        Self {
            registry_path,
            supervisor: Arc::new(supervisor),
            config: Arc::new(config),
            index_map: Arc::new(Mutex::new(index_map)),
        }
    }
}

/// Build the `/api/v1` router with `X-Agent-Root` validation middleware.
/// All route handlers are stubs (`StatusCode::NOT_IMPLEMENTED`) except as
/// filled in by downstream tickets. Middleware runs on every request under
/// `/api/v1`.
pub fn build_router(state: AppState) -> axum::Router {
    axum::Router::new()
        .route("/api/v1/learn", axum::routing::post(stub_handler))
        .route("/api/v1/recall", axum::routing::get(stub_handler))
        .route("/api/v1/dream", axum::routing::post(stub_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            agent_root_middleware,
        ))
        .with_state(state)
}

async fn stub_handler() -> axum::http::StatusCode {
    axum::http::StatusCode::NOT_IMPLEMENTED
}

/// Extract and validate `X-Agent-Root` on every request under `/api/v1`.
///
/// * Missing or non-UTF-8 header → 400 Bad Request, JSON error body.
/// * `resolve_project` returns `Err` (malformed registry TOML) → 500.
/// * `resolve_project` returns `Ok(None)` (root not registered) → 404.
/// * `resolve_project` returns `Ok(Some(entry))` → inserts
///   `Extension(entry)` and calls `next.run(req)`.
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

fn error_400(msg: &str) -> axum::response::Response {
    (
        axum::http::StatusCode::BAD_REQUEST,
        axum::Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

fn error_404(msg: &str) -> axum::response::Response {
    (
        axum::http::StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

fn error_500(msg: &str) -> axum::response::Response {
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::index_map::ProjectIndexMapConfig;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn make_test_state(registry_path: PathBuf) -> AppState {
        let (supervisor, _rx) = Supervisor::for_backpressure_test();
        let config = Config::default();
        let index_map = ProjectIndexMap::new(ProjectIndexMapConfig::default());
        AppState {
            registry_path,
            supervisor: Arc::new(supervisor),
            config: Arc::new(config),
            index_map: Arc::new(Mutex::new(index_map)),
        }
    }

    #[tokio::test]
    async fn missing_x_agent_root_returns_400() {
        let dir = tempfile::tempdir().unwrap();
        let state = make_test_state(dir.path().join("registry.toml"));
        let router = build_router(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .body(Body::empty())
            .unwrap();

        let resp = router.into_service().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unregistered_agent_root_returns_404() {
        let dir = tempfile::tempdir().unwrap();
        // No registry.toml written — resolve_project returns Ok(None).
        let state = make_test_state(dir.path().join("registry.toml"));
        let router = build_router(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", dir.path().to_str().unwrap())
            .body(Body::empty())
            .unwrap();

        let resp = router.into_service().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn registered_agent_root_passes_middleware() {
        let dir = tempfile::tempdir().unwrap();
        let registry_path = dir.path().join("registry.toml");

        // Write a registry TOML with the temp dir as a registered project.
        let root_str = dir.path().to_str().unwrap();
        let toml_content = format!(
            "[[projects]]\nroot = \"{}\"\nregistered_at = \"2026-05-20T00:00:00Z\"\n",
            root_str
        );
        std::fs::write(&registry_path, toml_content).unwrap();

        let state = make_test_state(registry_path);
        let router = build_router(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", root_str)
            .body(Body::empty())
            .unwrap();

        let resp = router.into_service().oneshot(req).await.unwrap();
        // Middleware passes; stub handler returns 501.
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn stub_learn_endpoint_returns_501() {
        let dir = tempfile::tempdir().unwrap();
        let registry_path = dir.path().join("registry.toml");

        let root_str = dir.path().to_str().unwrap();
        let toml_content = format!(
            "[[projects]]\nroot = \"{}\"\nregistered_at = \"2026-05-20T00:00:00Z\"\n",
            root_str
        );
        std::fs::write(&registry_path, toml_content).unwrap();

        let state = make_test_state(registry_path);
        let router = build_router(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", root_str)
            .body(Body::empty())
            .unwrap();

        let resp = router.into_service().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }
}
