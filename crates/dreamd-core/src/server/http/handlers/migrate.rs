//! `POST /api/v1/migrate` — reserved stub (BZR-187 / DR-405).

use axum::response::IntoResponse;

/// `POST /api/v1/migrate` — reserved; no schema migration is available yet.
///
/// Takes no extractors on purpose: a `Json` extractor would turn an empty or
/// untyped body into `415` (as `/learn` does) before the stub could answer.
/// Any body is ignored. This handler does not run the CLI migrator or touch
/// the store; operators use `dreamd migrate` (see `docs/migrate.md`).
///
/// # Headers
/// * `X-Agent-Root` (required; enforced by `agent_root_middleware`)
///
/// # Response (`501`)
/// `{"error":"no schema migration available","current_schema_version":"1.0.0"}`
///
/// `current_schema_version` is the episodic
/// [`dreamd_protocol::RECORD_SCHEMA_VERSION`].
pub(crate) async fn post_migrate() -> axum::response::Response {
    (
        axum::http::StatusCode::NOT_IMPLEMENTED,
        axum::Json(serde_json::json!({
            "error": "no schema migration available",
            "current_schema_version": dreamd_protocol::RECORD_SCHEMA_VERSION,
        })),
    )
        .into_response()
}
