//! `GET /api/v1/preferences` — read `.agent/personal/PREFERENCES.md`.

use axum::extract::{Extension, State};
use axum::response::IntoResponse;

use crate::registry::ProjectEntry;

use super::super::router::error_500;
use super::super::state::AppState;

/// Maximum bytes served from `PREFERENCES.md` in a single response.
/// Responses exceeding this cap are truncated and annotated with
/// `X-Dreamd-Truncated: true` and `X-Dreamd-Original-Size: <n>`.
pub(crate) const PREFERENCES_SIZE_CAP: usize = 16 * 1024; // 16 KB

/// `GET /api/v1/preferences` — read `.agent/personal/PREFERENCES.md`.
///
/// # Headers
/// * `X-Agent-Root` (required)
///
/// # Response (`200`)
/// `{"body":"<markdown>","last_modified":"<rfc3339>|null"}`
///
/// Files over 16 KiB are truncated; see `X-Dreamd-Truncated` / `X-Dreamd-Original-Size`.
pub(crate) async fn get_preferences(
    State(_state): State<AppState>,
    Extension(entry): Extension<ProjectEntry>,
) -> axum::response::Response {
    let root = crate::layout::AgentRoot::new(&entry.root);
    let pref_path = root.preferences_md();

    if !pref_path.exists() {
        let body = serde_json::json!({ "body": "", "last_modified": null });
        return (axum::http::StatusCode::OK, axum::Json(body)).into_response();
    }

    let bytes = match std::fs::read(&pref_path) {
        Ok(b) => b,
        Err(e) => return error_500(&format!("preferences read failed: {e}")),
    };

    let last_modified: Option<String> = std::fs::metadata(&pref_path)
        .ok()
        .and_then(|m| m.modified().ok())
        .map(|t| {
            let dt: chrono::DateTime<chrono::Utc> = t.into();
            dt.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
        });

    let original_size = bytes.len();
    let truncated = original_size > PREFERENCES_SIZE_CAP;
    let content_bytes = if truncated {
        &bytes[..PREFERENCES_SIZE_CAP]
    } else {
        &bytes
    };

    let body_str = String::from_utf8_lossy(content_bytes).into_owned();

    let body = serde_json::json!({
        "body": body_str,
        "last_modified": last_modified,
    });

    let mut headers = axum::http::HeaderMap::new();
    if truncated {
        headers.insert(
            axum::http::HeaderName::from_static("x-dreamd-truncated"),
            axum::http::HeaderValue::from_static("true"),
        );
        if let Ok(v) = axum::http::HeaderValue::from_str(&original_size.to_string()) {
            headers.insert(
                axum::http::HeaderName::from_static("x-dreamd-original-size"),
                v,
            );
        }
        tracing::warn!(
            original_size,
            cap = PREFERENCES_SIZE_CAP,
            path = %pref_path.display(),
            "PREFERENCES.md truncated to 16 KB cap"
        );
    }

    (axum::http::StatusCode::OK, headers, axum::Json(body)).into_response()
}
