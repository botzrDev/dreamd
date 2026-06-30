//! Minimal HTTP-over-UDS client so `dreamd dream` can proxy to a running
//! daemon instead of racing its coordinator (WEG-271 fast-follow). Transport
//! is delegated to [`crate::daemon_client`].

use std::path::{Path, PathBuf};

use bytes::Bytes;
use http_body_util::Full;

/// Outcome of attempting to proxy a dream cycle to a daemon. All four are
/// "the proxy made a decision"; only `Ran` and `InProgress` mean the daemon
/// acted. `NotReachable` / `ProjectNotRegistered` tell the caller to fall back
/// to the in-process cycle (both are race-free — no coordinator owns this JSONL).
#[derive(Debug, PartialEq, Eq)]
pub enum DreamProxyOutcome {
    /// Daemon ran the cycle (HTTP 200).
    Ran,
    /// No live daemon: socket absent or connection refused. → run in-process.
    NotReachable,
    /// HTTP 404 — daemon up but this project isn't registered with it. → run in-process.
    ProjectNotRegistered,
    /// HTTP 409 — a cycle is already running for this project. Caller surfaces
    /// an error and does NOT run in-process (that would race the coordinator).
    InProgress,
}

/// Genuine proxy failures (connect-after-reachable, handshake, transport, or a
/// daemon 4xx/5xx other than 404/409). The caller surfaces these and does NOT
/// fall back to in-process.
#[derive(Debug)]
pub enum DreamProxyError {
    Transport(String),
    DaemonError { status: u16, body: String },
}

impl std::fmt::Display for DreamProxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(s) => write!(f, "daemon proxy: {s}"),
            Self::DaemonError { status, body } => {
                write!(f, "daemon returned {status}: {body}")
            }
        }
    }
}

impl std::error::Error for DreamProxyError {}

/// Resolve the daemon UDS path: `$DREAMD_SOCK` override, else
/// `~/.agent/dreamd.sock`. `None` if the home dir can't be resolved (caller
/// then runs in-process). Mirrors `mcp::resolve_sock_path`.
pub fn resolve_daemon_socket() -> Option<PathBuf> {
    crate::daemon_client::resolve_daemon_socket().ok()
}

/// Map a daemon HTTP status to an outcome. Pure — unit-tested directly.
pub(crate) fn map_dream_status(
    status: u16,
    body: &Bytes,
) -> Result<DreamProxyOutcome, DreamProxyError> {
    match status {
        200 => Ok(DreamProxyOutcome::Ran),
        404 => Ok(DreamProxyOutcome::ProjectNotRegistered),
        409 => Ok(DreamProxyOutcome::InProgress),
        other => Err(DreamProxyError::DaemonError {
            status: other,
            body: String::from_utf8_lossy(body).into_owned(),
        }),
    }
}

/// Connect to `sock_path` and `POST /api/v1/dream` with `x-agent-root =
/// project_root` (sent raw — the daemon canonicalizes). A connect that fails
/// with NotFound/ConnectionRefused → `Ok(NotReachable)` (no live daemon);
/// other connect/handshake/transport failures → `Err(Transport)`.
pub async fn proxy_dream_cycle(
    sock_path: &Path,
    project_root: &Path,
) -> Result<DreamProxyOutcome, DreamProxyError> {
    use crate::daemon_client::{send_one, DaemonTransportError};

    let req = hyper::Request::builder()
        .method(hyper::Method::POST)
        .uri("/api/v1/dream")
        .header(hyper::header::HOST, "localhost")
        .header("x-agent-root", project_root.to_string_lossy().as_ref())
        .body(Full::new(Bytes::new()))
        .map_err(|e| DreamProxyError::Transport(format!("build request: {e}")))?;

    let (status, body) = match send_one(sock_path, req).await {
        Ok(pair) => pair,
        Err(DaemonTransportError::Unreachable) => return Ok(DreamProxyOutcome::NotReachable),
        Err(e) => return Err(DreamProxyError::Transport(format!("{e:?}"))),
    };
    map_dream_status(status.as_u16(), &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::Full;

    #[test]
    fn resolve_daemon_socket_honors_env_override() {
        let custom = "/tmp/dreamd-client-test.sock";
        std::env::set_var("DREAMD_SOCK", custom);
        let got = resolve_daemon_socket().expect("env override yields a path");
        std::env::remove_var("DREAMD_SOCK");
        assert_eq!(got, PathBuf::from(custom));
    }

    #[test]
    fn map_dream_status_maps_known_codes() {
        let empty = Bytes::new();
        assert_eq!(
            map_dream_status(200, &empty).unwrap(),
            DreamProxyOutcome::Ran
        );
        assert_eq!(
            map_dream_status(404, &empty).unwrap(),
            DreamProxyOutcome::ProjectNotRegistered
        );
        assert_eq!(
            map_dream_status(409, &empty).unwrap(),
            DreamProxyOutcome::InProgress
        );
        let body = Bytes::from_static(b"boom");
        match map_dream_status(500, &body) {
            Err(DreamProxyError::DaemonError { status, body }) => {
                assert_eq!(status, 500);
                assert_eq!(body, "boom");
            }
            other => panic!("expected DaemonError(500), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn proxy_dream_cycle_unreachable_socket_is_not_reachable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("nonexistent.sock");
        let outcome = proxy_dream_cycle(&sock, Path::new("/x"))
            .await
            .expect("unreachable socket is Ok(NotReachable), not Err");
        assert_eq!(outcome, DreamProxyOutcome::NotReachable);
    }

    /// Load-bearing round-trip: stand up a one-shot UDS server using the same
    /// `serve_connection` + `TokioIo` idiom the daemon uses (`uds_server.rs`),
    /// reply 200, and assert the proxied request method/URI/header.
    #[tokio::test]
    async fn proxy_dream_cycle_round_trips_over_uds() {
        use hyper::service::service_fn;
        use hyper_util::rt::{TokioExecutor, TokioIo};

        let dir = tempfile::tempdir().expect("tempdir");
        let sock_path = dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&sock_path).expect("bind");

        let (req_tx, mut req_rx) =
            tokio::sync::mpsc::unbounded_channel::<(String, String, Option<String>)>();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let req_tx = req_tx.clone();
                async move {
                    let method = req.method().to_string();
                    let uri = req.uri().to_string();
                    let agent_root = req
                        .headers()
                        .get("x-agent-root")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    let _ = req_tx.send((method, uri, agent_root));
                    Ok::<_, std::convert::Infallible>(
                        hyper::Response::builder()
                            .status(200)
                            .body(Full::new(Bytes::from_static(b"{\"status\":\"ok\"}")))
                            .unwrap(),
                    )
                }
            });
            let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await;
        });

        let project_root = Path::new("/some/project/root");
        let outcome = proxy_dream_cycle(&sock_path, project_root)
            .await
            .expect("proxy ok");
        assert_eq!(outcome, DreamProxyOutcome::Ran);

        let (method, uri, agent_root) = req_rx.recv().await.expect("request captured");
        assert_eq!(method, "POST");
        assert_eq!(uri, "/api/v1/dream");
        assert_eq!(agent_root.as_deref(), Some("/some/project/root"));
    }
}
