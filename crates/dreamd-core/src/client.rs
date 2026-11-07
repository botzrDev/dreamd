//! Minimal HTTP-over-UDS client so `dreamd dream` can proxy to a running
//! daemon instead of racing its coordinator (WEG-271 fast-follow). Transport
//! is delegated to [`crate::daemon_client`].

use std::path::{Path, PathBuf};

use bytes::Bytes;
use http_body_util::Full;

use crate::autobiography::AutobiographyReport;

pub use crate::dream_cycle::{
    NO_LLM_HEADER, NO_LLM_HEADER_VALUE, SHARE_PERSONAL_HEADER, SHARE_PERSONAL_HEADER_VALUE,
};

/// Outcome of attempting to proxy a dream cycle to a daemon. All four are
/// "the proxy made a decision"; only `Ran` and `InProgress` mean the daemon
/// acted. `NotReachable` / `ProjectNotRegistered` tell the caller to fall back
/// to the in-process cycle (both are race-free — no coordinator owns this JSONL).
#[derive(Debug, PartialEq, Eq)]
pub enum DreamProxyOutcome {
    /// Daemon ran the cycle (HTTP 200). `autobiography` is absent from a
    /// daemon before 1.3.0 or any body without a valid object. `notices` is
    /// the body's string array, empty when the key is absent.
    Ran {
        autobiography: Option<AutobiographyReport>,
        notices: Vec<String>,
    },
    /// No live daemon: socket absent or connection refused. → run in-process.
    NotReachable,
    /// HTTP 404 — daemon up but this project isn't registered with it. → run in-process.
    ProjectNotRegistered,
    /// HTTP 409 — a cycle is already running for this project. Caller surfaces
    /// an error and does not run in-process (that would race the coordinator).
    InProgress,
}

/// Genuine proxy failures (connect-after-reachable, handshake, transport, or a
/// daemon 4xx/5xx other than 404/409). The caller surfaces these and does not
/// fall back to in-process.
#[derive(Debug)]
pub enum DreamProxyError {
    /// A transport stage failed after the daemon was found reachable.
    Transport(String),
    /// The daemon answered with a status other than 200, 404 or 409.
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
        200 => Ok(DreamProxyOutcome::Ran {
            autobiography: parse_autobiography(body),
            notices: parse_notices(body),
        }),
        404 => Ok(DreamProxyOutcome::ProjectNotRegistered),
        409 => Ok(DreamProxyOutcome::InProgress),
        other => Err(DreamProxyError::DaemonError {
            status: other,
            body: String::from_utf8_lossy(body).into_owned(),
        }),
    }
}

/// The 200 body's `autobiography` report (BZR-1467). The `is_object` filter is
/// load-bearing: serde's internally tagged visitor also accepts a sequence whose
/// first element is the tag, so `["unchanged"]` would otherwise parse.
fn parse_notices(body: &Bytes) -> Vec<String> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Vec::new();
    };
    v.get("notices")
        .and_then(|n| n.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_autobiography(body: &Bytes) -> Option<AutobiographyReport> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let report = v.get("autobiography").filter(|a| a.is_object())?;
    serde_json::from_value(report.clone()).ok()
}

/// Connect to `sock_path` and `POST /api/v1/dream` with `x-agent-root =
/// project_root` (sent raw — the daemon canonicalizes). A connect that fails
/// with NotFound/ConnectionRefused → `Ok(NotReachable)` (no live daemon);
/// other connect/handshake/transport failures → `Err(Transport)`.
///
/// `no_llm` travels as the [`NO_LLM_HEADER`] header, not as a reason to skip
/// the proxy: skipping would run a second cycle in this process while the
/// daemon's coordinator still owns the JSONL. `--no-commit` is the only
/// write-path flag that bypasses the daemon; `--dry` also skips the proxy, but
/// writes nothing.
pub async fn proxy_dream_cycle(
    sock_path: &Path,
    project_root: &Path,
    no_llm: bool,
    share_personal: bool,
) -> Result<DreamProxyOutcome, DreamProxyError> {
    use crate::daemon_client::{send_one, DaemonTransportError};

    let mut builder = hyper::Request::builder()
        .method(hyper::Method::POST)
        .uri("/api/v1/dream")
        .header(hyper::header::HOST, "localhost")
        .header("x-agent-root", project_root.to_string_lossy().as_ref());
    if no_llm {
        builder = builder.header(NO_LLM_HEADER, NO_LLM_HEADER_VALUE);
    }
    if share_personal {
        builder = builder.header(SHARE_PERSONAL_HEADER, SHARE_PERSONAL_HEADER_VALUE);
    }
    let req = builder
        .body(Full::new(Bytes::new()))
        .map_err(|e| DreamProxyError::Transport(format!("build request: {e}")))?;

    let (status, body) = match send_one(sock_path, req).await {
        Ok(pair) => pair,
        Err(DaemonTransportError::Unreachable) => return Ok(DreamProxyOutcome::NotReachable),
        // A daemon answered, so this is an error, never `NotReachable`:
        // running the cycle in-process would race that daemon's coordinator.
        // The sentence is printed as is (BZR-1519).
        Err(DaemonTransportError::VersionMismatch(msg)) => {
            return Err(DreamProxyError::Transport(msg))
        }
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
            DreamProxyOutcome::Ran {
                autobiography: None,
                notices: Vec::new(),
            }
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

    /// BZR-1467 test 26: a 200 body's report, and every body that carries none.
    #[test]
    fn map_dream_status_reads_the_autobiography_report() {
        let skipped = Bytes::from_static(
            br#"{"status":"ok","autobiography":{"outcome":"skipped","reason":"user_dirty_tree","files":[".agent/semantic/LESSONS.md"]}}"#,
        );
        assert_eq!(
            map_dream_status(200, &skipped).unwrap(),
            DreamProxyOutcome::Ran {
                autobiography: Some(AutobiographyReport::Skipped {
                    reason: "user_dirty_tree".to_string(),
                    files: vec![".agent/semantic/LESSONS.md".to_string()],
                }),
                notices: Vec::new(),
            }
        );
        for body in [
            &br#"{"status":"ok"}"#[..],
            b"not json",
            br#"{"autobiography":["unchanged"]}"#,
            br#"{"autobiography":{"outcome":"exploded"}}"#,
        ] {
            assert_eq!(
                map_dream_status(200, &Bytes::copy_from_slice(body)).unwrap(),
                DreamProxyOutcome::Ran {
                    autobiography: None,
                    notices: Vec::new(),
                },
                "body {:?}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn map_dream_status_reads_notices() {
        let body = Bytes::from_static(br#"{"status":"ok","notices":["pinned: evt_a","nope",1]}"#);
        match map_dream_status(200, &body).unwrap() {
            DreamProxyOutcome::Ran { notices, .. } => {
                assert_eq!(
                    notices,
                    vec!["pinned: evt_a".to_string(), "nope".to_string()]
                );
            }
            other => panic!("expected Ran, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn proxy_dream_cycle_unreachable_socket_is_not_reachable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("nonexistent.sock");
        let outcome = proxy_dream_cycle(&sock, Path::new("/x"), false, false)
            .await
            .expect("unreachable socket is Ok(NotReachable), not Err");
        assert_eq!(outcome, DreamProxyOutcome::NotReachable);
    }

    /// Captured request shape from the one-shot UDS server below.
    struct CapturedRequest {
        method: String,
        uri: String,
        agent_root: Option<String>,
        no_llm: Option<String>,
        share_personal: Option<String>,
    }

    /// Stand up a one-shot UDS server using the same `serve_connection` +
    /// `TokioIo` idiom the daemon uses (`uds_server.rs`), reply 200, and return
    /// both the proxy outcome and the request the daemon actually saw.
    async fn round_trip(
        no_llm: bool,
        share_personal: bool,
    ) -> (DreamProxyOutcome, CapturedRequest) {
        use hyper::service::service_fn;
        use hyper_util::rt::{TokioExecutor, TokioIo};

        let dir = tempfile::tempdir().expect("tempdir");
        let sock_path = dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&sock_path).expect("bind");

        let (req_tx, mut req_rx) = tokio::sync::mpsc::unbounded_channel::<CapturedRequest>();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let req_tx = req_tx.clone();
                async move {
                    let header = |name: &str| {
                        req.headers()
                            .get(name)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string)
                    };
                    let _ = req_tx.send(CapturedRequest {
                        method: req.method().to_string(),
                        uri: req.uri().to_string(),
                        agent_root: header("x-agent-root"),
                        no_llm: header(NO_LLM_HEADER),
                        share_personal: header(SHARE_PERSONAL_HEADER),
                    });
                    Ok::<_, std::convert::Infallible>(
                        hyper::Response::builder()
                            .status(200)
                            .header(
                                crate::daemon_client::VERSION_HEADER,
                                env!("CARGO_PKG_VERSION"),
                            )
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
        let outcome = proxy_dream_cycle(&sock_path, project_root, no_llm, share_personal)
            .await
            .expect("proxy ok");
        let captured = req_rx.recv().await.expect("request captured");
        (outcome, captured)
    }

    #[tokio::test]
    async fn proxy_dream_cycle_round_trips_over_uds() {
        let (outcome, req) = round_trip(false, false).await;
        assert_eq!(
            outcome,
            DreamProxyOutcome::Ran {
                autobiography: None,
                notices: Vec::new(),
            }
        );
        assert_eq!(req.method, "POST");
        assert_eq!(req.uri, "/api/v1/dream");
        assert_eq!(req.agent_root.as_deref(), Some("/some/project/root"));
        // The header is omitted by default, not sent as "0" — a daemon that
        // predates `--no-llm` (AILAB-204) must see an unchanged request.
        assert_eq!(
            req.no_llm, None,
            "{NO_LLM_HEADER} must be absent when --no-llm is not set"
        );
        // AILAB-199: same rule, and the one that matters most — the default
        // request must carry no consent at all, not a falsy consent.
        assert_eq!(
            req.share_personal, None,
            "{SHARE_PERSONAL_HEADER} must be absent when --share-personal is not set"
        );
    }

    #[tokio::test]
    async fn proxy_dream_cycle_sends_no_llm_header_when_set() {
        let (outcome, req) = round_trip(true, false).await;
        assert_eq!(
            outcome,
            DreamProxyOutcome::Ran {
                autobiography: None,
                notices: Vec::new(),
            }
        );
        assert_eq!(req.uri, "/api/v1/dream", "--no-llm must not skip the proxy");
        assert_eq!(
            req.no_llm.as_deref(),
            Some(NO_LLM_HEADER_VALUE),
            "--no-llm must travel as {NO_LLM_HEADER}: {NO_LLM_HEADER_VALUE}"
        );
        assert_eq!(
            req.share_personal, None,
            "--no-llm alone must not imply consent"
        );
    }

    /// AILAB-199: `--share-personal` travels *through* the proxy, exactly like
    /// `--no-llm`. Skipping the daemon to honor it locally would put a second
    /// writer on a JSONL the daemon's coordinator owns.
    #[tokio::test]
    async fn proxy_dream_cycle_sends_share_personal_header_when_set() {
        let (outcome, req) = round_trip(false, true).await;
        assert_eq!(
            outcome,
            DreamProxyOutcome::Ran {
                autobiography: None,
                notices: Vec::new(),
            }
        );
        assert_eq!(
            req.uri, "/api/v1/dream",
            "--share-personal must not skip the proxy"
        );
        assert_eq!(
            req.share_personal.as_deref(),
            Some(SHARE_PERSONAL_HEADER_VALUE),
            "--share-personal must travel as {SHARE_PERSONAL_HEADER}: {SHARE_PERSONAL_HEADER_VALUE}"
        );
        assert_eq!(
            req.no_llm, None,
            "--share-personal alone must not force deterministic mode"
        );
    }

    /// The two flags are independent on the wire: both set means both headers.
    #[tokio::test]
    async fn proxy_dream_cycle_sends_both_headers_when_both_are_set() {
        let (_, req) = round_trip(true, true).await;
        assert_eq!(req.no_llm.as_deref(), Some(NO_LLM_HEADER_VALUE));
        assert_eq!(
            req.share_personal.as_deref(),
            Some(SHARE_PERSONAL_HEADER_VALUE)
        );
    }

    /// BZR-1519: a daemon of another version is a proxy error carrying the
    /// client's own sentence, not the daemon's 426 body (BZR-1531). It is not
    /// `NotReachable`, which would run the cycle in-process beside that daemon.
    #[tokio::test]
    async fn proxy_dream_cycle_version_mismatch_is_an_error_not_in_process() {
        use hyper::service::service_fn;
        use hyper_util::rt::{TokioExecutor, TokioIo};

        const DAEMON_BODY: &str = "version mismatch: client 1.0.0, daemon 0.0.0. from the daemon";
        let sentence = crate::daemon_client::version_mismatch_message(
            env!("CARGO_PKG_VERSION"),
            "0.0.0",
            crate::launcher::Launcher::from_env(),
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let sock_path = dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&sock_path).expect("bind");
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let svc = service_fn(|_req: hyper::Request<hyper::body::Incoming>| async {
                let body = serde_json::json!({ "error": DAEMON_BODY }).to_string();
                Ok::<_, std::convert::Infallible>(
                    hyper::Response::builder()
                        .status(426)
                        .header(crate::daemon_client::VERSION_HEADER, "0.0.0")
                        .body(Full::new(Bytes::from(body)))
                        .unwrap(),
                )
            });
            let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(stream), svc)
                .await;
        });

        match proxy_dream_cycle(&sock_path, Path::new("/x"), false, false).await {
            Err(DreamProxyError::Transport(msg)) => {
                assert_eq!(msg, sentence);
                assert_eq!(
                    DreamProxyError::Transport(msg).to_string(),
                    format!("daemon proxy: {sentence}")
                );
            }
            other => panic!("expected Transport(sentence), got {other:?}"),
        }
    }
}
