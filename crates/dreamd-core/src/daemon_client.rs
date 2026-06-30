//! Shared outbound HTTP-over-UDS transport to the dreamd daemon.
//!
//! One per-call-connect implementation used by BOTH the MCP Remote backend
//! (`mcp::send_remote`) and the dream-cycle CLI proxy (`client::proxy_dream_cycle`).
//! Owns only the transport + socket-path resolution; each caller keeps its own
//! request construction and status→outcome mapping. Per-call connect (no pool):
//! daemon traffic is infrequent (WEG-78-A rationale).

use std::path::{Path, PathBuf};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};

use crate::layout::DaemonHome;

/// A transport-stage failure talking to the daemon over UDS. Variant
/// granularity lets each caller reproduce its existing message + the dream
/// proxy distinguish "no live daemon" (`Unreachable`) from real failures.
#[derive(Debug)]
pub enum DaemonTransportError {
    /// Connect failed with `NotFound`/`ConnectionRefused` — no daemon is
    /// listening. The dream proxy maps this to `NotReachable` (run in-process).
    Unreachable,
    Connect(String),
    Handshake(String),
    Send(String),
    ReadBody(String),
}

/// Socket-path resolution failure.
#[derive(Debug)]
pub enum SockPathError {
    /// `DREAMD_SOCK` was set to a relative path (never a valid daemon address).
    RelativeSockPath(PathBuf),
    /// Home directory could not be resolved.
    NoHome,
}

/// Resolve the daemon UDS path: `$DREAMD_SOCK` (must be absolute) else
/// `~/.agent/dreamd.sock`.
pub fn resolve_daemon_socket() -> Result<PathBuf, SockPathError> {
    if let Some(v) = std::env::var_os("DREAMD_SOCK") {
        let path = PathBuf::from(v);
        if !path.is_absolute() {
            return Err(SockPathError::RelativeSockPath(path));
        }
        return Ok(path);
    }
    let home = dirs::home_dir().ok_or(SockPathError::NoHome)?;
    Ok(DaemonHome::new(home.join(".agent")).socket_path())
}

/// Open a fresh UDS connection to `sock_path`, drive one HTTP/1 request to
/// completion, and return `(status, body)`. The connection task is spawned and
/// runs until the stream closes. Connect `NotFound`/`ConnectionRefused` →
/// `Unreachable`; all other stage failures carry the underlying message.
pub async fn send_one(
    sock_path: &Path,
    req: hyper::Request<Full<Bytes>>,
) -> Result<(hyper::StatusCode, Bytes), DaemonTransportError> {
    use hyper::client::conn::http1;
    use hyper_util::rt::TokioIo;

    let stream = match tokio::net::UnixStream::connect(sock_path).await {
        Ok(s) => s,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Err(DaemonTransportError::Unreachable);
        }
        Err(e) => return Err(DaemonTransportError::Connect(e.to_string())),
    };
    // TokioIo wrap is required: a raw UnixStream does not implement hyper's IO
    // trait (WEG-72-B drift entry).
    let io = TokioIo::new(stream);
    let (mut sender, conn) = http1::handshake(io)
        .await
        .map_err(|e| DaemonTransportError::Handshake(e.to_string()))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let resp = sender
        .send_request(req)
        .await
        .map_err(|e| DaemonTransportError::Send(e.to_string()))?;
    let status = resp.status();
    let body = resp
        .collect()
        .await
        .map_err(|e| DaemonTransportError::ReadBody(e.to_string()))?
        .to_bytes();
    Ok((status, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes env-mutating resolver tests.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn resolve_rejects_relative_dreamd_sock() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("DREAMD_SOCK", "relative.sock");
        let result = resolve_daemon_socket();
        std::env::remove_var("DREAMD_SOCK");
        assert!(matches!(
            result,
            Err(SockPathError::RelativeSockPath(p)) if p == Path::new("relative.sock")
        ));
    }

    #[test]
    fn resolve_honors_absolute_override() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("DREAMD_SOCK", "/tmp/x.sock");
        let result = resolve_daemon_socket();
        std::env::remove_var("DREAMD_SOCK");
        assert_eq!(result.unwrap(), PathBuf::from("/tmp/x.sock"));
    }

    #[tokio::test]
    async fn send_one_unreachable_socket_is_unreachable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("nonexistent.sock");
        let req = hyper::Request::builder()
            .method(hyper::Method::GET)
            .uri("/")
            .body(Full::new(Bytes::new()))
            .expect("request");
        let err = send_one(&sock, req).await.unwrap_err();
        assert!(matches!(err, DaemonTransportError::Unreachable));
    }

    #[tokio::test]
    async fn send_one_round_trips_over_uds() {
        use hyper::service::service_fn;
        use hyper_util::rt::{TokioExecutor, TokioIo};

        let dir = tempfile::tempdir().expect("tempdir");
        let sock_path = dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&sock_path).expect("bind");

        let (req_tx, mut req_rx) = tokio::sync::mpsc::unbounded_channel::<(String, String)>();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let req_tx = req_tx.clone();
                async move {
                    let method = req.method().to_string();
                    let uri = req.uri().to_string();
                    let _ = req_tx.send((method, uri));
                    Ok::<_, std::convert::Infallible>(
                        hyper::Response::builder()
                            .status(200)
                            .body(Full::new(Bytes::from_static(b"{\"ok\":true}")))
                            .unwrap(),
                    )
                }
            });
            let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await;
        });

        let req = hyper::Request::builder()
            .method(hyper::Method::POST)
            .uri("/api/v1/dream")
            .header(hyper::header::HOST, "localhost")
            .body(Full::new(Bytes::new()))
            .expect("request");
        let (status, body) = send_one(&sock_path, req).await.expect("round trip");
        assert_eq!(status, hyper::StatusCode::OK);
        assert_eq!(body.as_ref(), b"{\"ok\":true}");

        let (method, uri) = req_rx.recv().await.expect("request captured");
        assert_eq!(method, "POST");
        assert_eq!(uri, "/api/v1/dream");
    }
}
