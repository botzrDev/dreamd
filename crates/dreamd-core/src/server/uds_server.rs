//! API socket bind helper for the writer-process HTTP server (WEG-71 / DR-406).
//!
//! Wraps [`bind_socket_raw`] with parent-directory creation and permissions so
//! the server entry point gets a ready-to-use `UnixListener` in one call.
//!
//! Unix-only. Windows path is DR-121 / WEG-135, deferred to v0.1.1.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

use crate::server::lifecycle::ServerError;
use crate::server::uds::bind_socket_raw;

/// Bind the API unix-domain socket at `path`.
///
/// Creates the parent directory (mode `0700`) if it does not yet exist, then
/// delegates to [`bind_socket_raw`] for the bind-and-recover logic and `0600`
/// permissions. Returns a `tokio::net::UnixListener` ready for async `accept`
/// calls.
///
/// The directory is `0700` (not `0755`) so other users cannot stat the socket
/// path at all — the socket `0600` alone would block connects but still expose
/// the socket's existence. Defense-in-depth per DR-101 §Security.
pub fn bind_api_socket(path: &Path) -> Result<tokio::net::UnixListener, ServerError> {
    if let Some(parent) = path.parent() {
        // Atomic: mkdir each component WITH mode 0700 in one syscall, closing the
        // create-then-chmod window where the dir briefly existed at the umask mode.
        // recursive(true) is idempotent (no-op if present) and does NOT retro-chmod
        // an existing dir — we must not silently downgrade an existing 0755 ~/.agent/.
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    let listener = bind_socket_raw(path)?;
    listener.set_nonblocking(true)?;
    let tokio_listener = tokio::net::UnixListener::from_std(listener)?;
    Ok(tokio_listener)
}

/// Accept loop for the API UDS socket.
///
/// Each accepted connection: read peer UID via `peer_cred()`, inject
/// `Extension(PeerUid(uid))`, wrap with `TokioIo`, and spawn a connection
/// task. A `peer_cred()` failure drops the connection without crashing the loop.
#[cfg(unix)]
pub async fn serve_uds(
    listener: tokio::net::UnixListener,
    router: axum::Router,
) -> std::io::Result<()> {
    use hyper_util::rt::{TokioExecutor, TokioIo};

    loop {
        let (stream, _addr) = listener.accept().await?;
        let uid = match stream.peer_cred() {
            Ok(cred) => cred.uid(),
            Err(e) => {
                tracing::warn!("peer_cred() failed on accept: {e}");
                continue;
            }
        };

        let tower_svc = router
            .clone()
            .layer(axum::Extension(crate::server::http::PeerUid(uid)));
        let svc = hyper_util::service::TowerToHyperService::new(tower_svc);
        let io = TokioIo::new(stream);

        tokio::spawn(async move {
            if let Err(e) = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await
            {
                tracing::debug!("connection error: {e}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener as StdUnixListener;

    #[tokio::test]
    async fn bind_succeeds_and_perms_are_0600() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock_path = dir.path().join("test.sock");

        let _listener = bind_api_socket(&sock_path).expect("bind should succeed");

        let meta = fs::metadata(&sock_path).expect("socket file should exist");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket perms should be 0600, got {:o}", mode);
    }

    #[tokio::test]
    async fn bind_api_socket_parent_dir_has_0700_perms() {
        let dir = tempfile::tempdir().expect("tempdir");
        // `subdir` does NOT exist before the call — forces a real mkdir.
        let sock_path = dir.path().join("subdir").join("dreamd.sock");

        let _listener = bind_api_socket(&sock_path).expect("bind should succeed");

        let parent = sock_path.parent().unwrap();
        let mode = fs::metadata(parent).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "parent dir perms should be 0700, got {:o}",
            mode
        );
    }

    #[tokio::test]
    async fn stale_socket_is_recovered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock_path = dir.path().join("stale.sock");

        // Create a socket then drop it — leaves the file on disk with no listener.
        {
            let _stale = StdUnixListener::bind(&sock_path).expect("initial bind");
            // dropped here; file persists
        }
        assert!(sock_path.exists(), "stale socket file should exist");

        // bind_api_socket should detect the stale socket and rebind successfully.
        let _listener = bind_api_socket(&sock_path).expect("rebind should succeed");
        assert!(sock_path.exists(), "socket file should exist after rebind");
    }
}
