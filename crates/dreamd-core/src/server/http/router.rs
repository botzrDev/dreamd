//! Route mounting, middleware, and HTTP error responses.

use std::time::Duration;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;

use super::handlers::{
    get_health, get_preferences, get_recall, post_dream, post_learn, post_migrate,
};
use super::state::AppState;

/// Peer UID injected at connection-accept time by `serve_uds`.
/// Extracted by `peer_uid_middleware` to enforce daemon-owner-only access.
#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
pub struct PeerUid(pub u32);

/// Build the `/api/v1` router with `X-Agent-Root` validation middleware, the
/// AILAB-189 request-trace stack, and exactly one auth layer as the outermost
/// one: `peer_uid_middleware` (WEG-72 / DR-407) on Unix, or
/// [`bearer_auth_middleware`] (AILAB-192) everywhere else. The two arms are
/// mutually exclusive — no target mounts both, and none mounts neither.
pub fn build_router(state: AppState) -> axum::Router {
    let router = axum::Router::new()
        .route("/api/v1/learn", axum::routing::post(post_learn))
        .route("/api/v1/recall", axum::routing::get(get_recall))
        .route("/api/v1/preferences", axum::routing::get(get_preferences))
        .route("/api/v1/health", axum::routing::get(get_health))
        .route("/api/v1/dream", axum::routing::post(post_dream))
        .route("/api/v1/migrate", axum::routing::post(post_migrate))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            agent_root_middleware,
        ))
        // AILAB-189 — one INFO span per request. In axum the LAST `.layer()` is
        // the OUTERMOST, so these three read bottom-up:
        //
        //   SetRequestId       mints `x-request-id` on the request
        //     PropagateRequestId   copies that header onto the response
        //       TraceLayer           opens the span; MakeSpan reads the header
        //
        // `SetRequestIdLayer` has to stay outside both of the others, not
        // between them: `PropagateRequestId::call` reads the header off the
        // *request* it is handed and stashes it for the response, and
        // `http_make_span` reads the same header. Either one placed outside the
        // generator sees a bare request and silently records nothing — no
        // compile error, just an empty `request_id` and a response with no
        // header. `PropagateRequestIdLayer` is the test seam: it is how
        // `health_response_carries_generated_request_id` can assert the UUID
        // generator ran without installing a capturing subscriber.
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(http_make_span)
                .on_response(http_on_response),
        )
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid));

    // `peer_uid_middleware` stays the outermost layer on Unix: auth runs before
    // everything else, tracing included. The trade is that a rejected peer
    // never reaches `TraceLayer` and so gets no `http` span. Its UID-mismatch
    // arm has its own `warn!` below carrying peer/daemon UIDs; its
    // extension-missing arm logs nothing at all, which is unreachable through
    // `serve_uds` (that drops a `peer_cred()` failure with its own warn before
    // the router is ever called).
    #[cfg(unix)]
    let router = router.layer(axum::middleware::from_fn_with_state(
        state.clone(),
        peer_uid_middleware,
    ));

    // Off Unix there is no `SO_PEERCRED`, so the loopback-TCP listener's auth is
    // the AILAB-203 bearer token instead, and it takes the *exact* slot
    // `peer_uid_middleware` holds above — outermost, so a caller with no
    // credential never reaches `TraceLayer` (same trade: a rejected request
    // earns no `http` span, only this layer's own reason-category `warn!`), and
    // outside `SetRequestIdLayer` for the reason spelled out at the top of this
    // fn — the generator has to stay outside `PropagateRequestId` and
    // `http_make_span`, so nothing may be wedged between them.
    //
    // The two auth layers are mutually exclusive by construction: these are
    // sibling `cfg` arms over the same binding, so a Unix build mounts peer-UID
    // and no bearer, an off-Unix build mounts bearer and no peer-UID, and there
    // is no target that mounts both or neither. `state.auth_json` — not
    // `state` — is the layer's state: the middleware needs one path and nothing
    // else, and narrowing it there keeps it from reaching for a supervisor, an
    // index handle or a registry it has no business touching before auth.
    #[cfg(not(unix))]
    let router = router.layer(axum::middleware::from_fn_with_state(
        state.auth_json.clone(),
        bearer_auth_middleware,
    ));

    router.with_state(state)
}

/// Fields lifted off a request to open the per-request `http` span (AILAB-189).
///
/// Split out of [`http_make_span`] so the extraction rule is unit-testable: a
/// `Span`'s recorded fields cannot be read back off the handle, so the only
/// other way to assert them is a capturing `tracing` subscriber, and this
/// ticket deliberately adds no test-only subscriber dependency.
pub(crate) struct RequestTraceFields {
    pub method: axum::http::Method,
    pub path: String,
    /// `x-request-id` if present, else `"-"`. On the live stack
    /// `SetRequestIdLayer` runs *outside* `TraceLayer`, so the header is always
    /// set by the time a span is made; `"-"` only shows up for a router
    /// exercised without that layer.
    pub request_id: String,
    /// From the [`PeerUid`] extension `serve_uds` injects at connection-accept
    /// time. `None` when the extension is absent — `peer_uid_middleware` turns
    /// that into a 403 further out, so it is not a case the span has to explain.
    #[cfg(unix)]
    pub peer_uid: Option<u32>,
}

/// Extract [`RequestTraceFields`] from a request. Read-only; no allocation
/// beyond the two owned strings.
pub(crate) fn request_trace_fields<B>(req: &axum::http::Request<B>) -> RequestTraceFields {
    RequestTraceFields {
        method: req.method().clone(),
        path: req.uri().path().to_owned(),
        request_id: req
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-")
            .to_owned(),
        #[cfg(unix)]
        peer_uid: req.extensions().get::<PeerUid>().map(|p| p.0),
    }
}

/// Open one **INFO** span per request.
///
/// `TraceLayer`'s `DefaultMakeSpan` would never emit here: it opens at DEBUG,
/// and `DREAMD_LOG` defaults to `info` (`observability::init_tracing`), so a
/// stock `TraceLayer::new_for_http()` mounts a layer that is silent in every
/// default install. Hence the hand-rolled `MakeSpan`. `status_code` and
/// `latency_ms` are declared [`tracing::field::Empty`] and filled in by
/// [`http_on_response`] once the response exists.
#[cfg(unix)]
fn http_make_span<B>(req: &axum::http::Request<B>) -> tracing::Span {
    let f = request_trace_fields(req);
    tracing::info_span!(
        "http",
        method = %f.method,
        path = %f.path,
        request_id = %f.request_id,
        peer_uid = f.peer_uid,
        status_code = tracing::field::Empty,
        latency_ms = tracing::field::Empty,
    )
}

/// Windows has no `SO_PEERCRED`, so there is no `PeerUid` extension to read and
/// the span carries no `peer_uid` field at all rather than a permanently empty
/// one. (`PeerUid` is deliberately not intra-doc-linked here — the type is
/// `#[cfg(unix)]` and the link would not resolve on the target this arm is for.)
#[cfg(not(unix))]
fn http_make_span<B>(req: &axum::http::Request<B>) -> tracing::Span {
    let f = request_trace_fields(req);
    tracing::info_span!(
        "http",
        method = %f.method,
        path = %f.path,
        request_id = %f.request_id,
        status_code = tracing::field::Empty,
        latency_ms = tracing::field::Empty,
    )
}

/// Record the outcome onto the request's own span, then emit the one INFO
/// event that carries it.
///
/// The event is not optional decoration. `observability::init_tracing` builds
/// both layers with `fmt::layer()`, whose `with_span_events` default is
/// `FmtSpan::NONE` — the subscriber writes a line for an *event*, never for a
/// span on its own. A span reaches the log only by decorating an event that
/// happens inside it, and the only other event on this path is `TraceLayer`'s
/// default `on_request`, which is DEBUG and so silent under the default
/// `DREAMD_LOG=info`. Recording onto the span and stopping there would have
/// produced an INFO span that is correct in every field and logs nothing.
///
/// Still deliberately *not* `DefaultOnResponse::new().level(Level::INFO)`: that
/// restates `status` and `latency` as its own event fields, duplicating what
/// the span already carries under different names. Emitting *into* the span
/// keeps this to exactly one INFO line per request with every field on the
/// span, which is also why `on_request` keeps its DEBUG default rather than
/// being promoted. A 5xx additionally draws `DefaultOnFailure`'s ERROR
/// `response failed` line — that classifier is left at its default on purpose,
/// since a failed request earning a second line at a louder level is the point.
fn http_on_response<B>(
    response: &axum::http::Response<B>,
    latency: Duration,
    span: &tracing::Span,
) {
    span.record("status_code", response.status().as_u16());
    // f64 milliseconds, NOT `as_millis()`: that truncates, and PERF.md puts the
    // warm path at P50 0.30 ms / P99 0.34 ms, so an integer field would have
    // read `latency_ms=0` on essentially every request — an AC field carrying
    // no information, and nothing in the suite would have caught it.
    span.record("latency_ms", latency.as_secs_f64() * 1000.0);
    tracing::info!(parent: span, "http request");
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

/// The one body every bearer rejection returns, whatever went wrong.
///
/// Deliberately uninformative. A client must not be able to tell "no
/// `auth.json` yet" from "the file is the wrong shape" from "your token is
/// wrong" — three distinguishable answers turn the endpoint into a probe
/// oracle that reports on the daemon's install state, and the last one
/// confirms a guessed length or alphabet. The operator-facing detail lives in
/// [`crate::daemon_client::AuthTokenError`]'s `Display`, which reaches the
/// daemon's own log and `dreamd watch`'s start failure, never a response body.
const BEARER_UNAUTHORIZED: &str = "unauthorized: a valid `Authorization: Bearer <token>` header is required (see `dreamd service install`)";

/// Split `Bearer <credential>` and return the credential, or `None` if the
/// header value is not that shape.
///
/// The scheme is matched ASCII-case-insensitively (RFC 7235 makes it so, and
/// HTTP clients differ on the casing they send), followed by **exactly one**
/// space and a non-empty credential. `split_once` consumes exactly one space,
/// so a credential that still begins with one came from `Bearer  <token>` and
/// is rejected rather than silently carrying whitespace into
/// [`crate::daemon_client::AuthToken::matches_hex`].
fn bearer_credential(raw: &str) -> Option<&str> {
    let (scheme, credential) = raw.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    if credential.is_empty() || credential.starts_with(' ') {
        return None;
    }
    Some(credential)
}

/// Enforce the AILAB-203 bearer token from `~/.agent/auth.json` (AILAB-192).
///
/// Mounted by [`build_router`] on the non-Unix target **only**, as the
/// outermost layer, in the same slot Unix gives `peer_uid_middleware` and for
/// the same reason: auth runs before `TraceLayer`, and outside
/// `SetRequestIdLayer`'s generator so nothing is wedged between it and
/// `PropagateRequestId`. The function itself is `cfg`-free so that Linux
/// compiles and tests it — a `#[cfg(not(unix))]` body would be invisible to
/// every check that guards main.
///
/// [`crate::daemon_client::read_auth_token`] runs on **every** request. There
/// is no cache, no `OnceCell` and no memoized `AuthToken` anywhere on this
/// path, and that absence is the whole proof that a
/// `dreamd service install --force` which rotates `auth.json` mid-session
/// invalidates the old credential on the very next request — the alternative
/// is a daemon that keeps honouring a token the operator has already revoked
/// until it is restarted. The cost is a synchronous read of a few dozen bytes
/// from the daemon's own home inside an `async fn`, which is the same trade
/// `agent_root_middleware` already makes with its synchronous
/// `resolve_project` registry read one layer in.
///
/// # Errors
/// * `401` — `Authorization` missing, not visible ASCII, or not
///   `Bearer <non-empty credential>`; `auth_json` is `None` (fail closed —
///   never serve unauthenticated); `auth.json` absent, unreadable, or not
///   `{"token":"<64 hex>"}`; or the presented credential does not match.
///
/// **401, never 403.** Every branch above returns the same status and the same
/// [`BEARER_UNAUTHORIZED`] body. `403` is the peer-UID verdict, stays
/// `#[cfg(unix)]`, and means something this layer can never conclude.
///
/// The `warn!` on each rejection carries a *reason category* and nothing else
/// — never the presented credential, never the file's contents, never the
/// token. An [`AuthToken`](crate::daemon_client::AuthToken) is not `{:?}`-ed
/// into a log either, redacting `Debug` or no: the rule is that the secret has
/// no path to a log line at all, not that one particular formatter is
/// currently safe.
pub async fn bearer_auth_middleware(
    axum::extract::State(auth_json): axum::extract::State<Option<std::path::PathBuf>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .map(|value| value.to_str());

    // Owned, exactly as `agent_root_middleware` owns its `X-Agent-Root`: the
    // borrow would otherwise be alive across `next.run(req)`, which consumes
    // the request the header was read from. 64 bytes per authenticated request.
    let credential = match header {
        None => {
            tracing::warn!(
                reason = "missing_authorization",
                "bearer auth rejected -- 401"
            );
            return error_401(BEARER_UNAUTHORIZED);
        }
        // `to_str` refuses anything outside visible ASCII, which is every
        // non-UTF-8 byte sequence and every multi-byte character alike. A
        // credential is 64 hex digits, so there is nothing legitimate to lose.
        Some(Err(_)) => {
            tracing::warn!(
                reason = "malformed_authorization",
                "bearer auth rejected -- 401"
            );
            return error_401(BEARER_UNAUTHORIZED);
        }
        Some(Ok(raw)) => match bearer_credential(raw) {
            Some(credential) => credential.to_owned(),
            None => {
                tracing::warn!(
                    reason = "malformed_authorization",
                    "bearer auth rejected -- 401"
                );
                return error_401(BEARER_UNAUTHORIZED);
            }
        },
    };

    // Fail closed. A router built without a token path has no way to
    // authenticate anyone, so it authenticates no one — the alternative
    // reading, "no path configured means no auth required", would turn a
    // missing `with_auth_json` call into an unauthenticated daemon on a port
    // every local process can dial.
    let Some(auth_json) = auth_json else {
        tracing::warn!(reason = "no_auth_file", "bearer auth rejected -- 401");
        return error_401(BEARER_UNAUTHORIZED);
    };

    let token = match crate::daemon_client::read_auth_token(&auth_json) {
        Ok(token) => token,
        Err(e) => {
            // Three variants, two categories, one status. `Malformed` shares
            // `unreadable_auth_file` rather than earning a third name: the
            // reason vocabulary is closed on purpose, and "the file is there
            // but yielded no token" is one operator problem
            // (`dreamd service install --force`) whether the bytes were
            // unreadable or the wrong shape. The error's own message is not
            // logged — `Unreadable` carries an OS string and `Missing` a path,
            // and the category is all this layer is allowed to say.
            let reason = match e {
                crate::daemon_client::AuthTokenError::Missing(_) => "no_auth_file",
                crate::daemon_client::AuthTokenError::Unreadable(_)
                | crate::daemon_client::AuthTokenError::Malformed(_) => "unreadable_auth_file",
            };
            tracing::warn!(reason, "bearer auth rejected -- 401");
            return error_401(BEARER_UNAUTHORIZED);
        }
    };

    // Constant-time XOR fold inside `matches_hex`; see its doc comment for why
    // it must not be "simplified" to an array `==`.
    if !token.matches_hex(&credential) {
        tracing::warn!(reason = "token_mismatch", "bearer auth rejected -- 401");
        return error_401(BEARER_UNAUTHORIZED);
    }

    next.run(req).await
}

#[cfg(unix)]
pub(crate) fn error_403(msg: &str) -> axum::response::Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

/// The bearer verdict (AILAB-192). Unlike [`error_403`] this is **not**
/// `#[cfg(unix)]`-gated in reverse: `bearer_auth_middleware` is `cfg`-free so
/// Linux compiles and tests it, so its response helper has to be too.
pub(crate) fn error_401(msg: &str) -> axum::response::Response {
    (
        StatusCode::UNAUTHORIZED,
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
