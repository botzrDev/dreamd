//! UDS bind / connect / cleanup for `~/.agent/dreamd.sock` (WEG-21 / DR-118).
//!
//! Bind protocol:
//!   1. Try `UnixListener::bind(path)`.
//!   2. On `AddrInUse`, try `UnixStream::connect(path)`:
//!        - Success → another writer-process is alive; the caller becomes a
//!          client. Returns [`UdsBindError::AlreadyBound`].
//!        - Connection refused → orphaned socket file. Unlink + retry bind.
//!   3. On bind success, chmod the socket to `0600` so only the daemon user
//!      can connect (paired with `SO_PEERCRED` validation in DR-407).
//!
//! Cleanup: the bind does not own the file. `dreamd watch` unlinks it on
//! shutdown (`watch.rs`); a file left by a crash or a `SIGKILL` is an orphan,
//! which the next bind removes (step 2).
//!
//! Unix-only. Windows path is DR-121 / WEG-135, deferred to v0.1.1.

#![cfg(unix)]

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

/// Reason a bind attempt did not become the writer-process.
#[derive(Debug, thiserror::Error)]
pub enum UdsBindError {
    /// Another writer-process answered on the socket. Caller should connect
    /// as a client instead of retrying. Carries the connected stream so the
    /// caller doesn't have to reopen it.
    #[error("socket already bound by another writer-process")]
    AlreadyBound(UnixStream),

    /// Underlying syscall failed for a reason other than `AddrInUse`.
    #[error("UDS bind failed: {0}")]
    Io(#[from] io::Error),
}

/// Core bind logic: attempt to bind `path` as a `UnixListener`, handling
/// `AddrInUse` by probing the existing socket for liveness.
///
/// * Success → returns the bound `UnixListener` (already chmod'd `0600`).
/// * Live competitor → returns [`UdsBindError::AlreadyBound`] with a
///   connected stream.
/// * Stale/orphaned file → unlinks and re-binds transparently.
///
/// `pub(crate)` for `uds_server::bind_api_socket`, the production bind.
pub(crate) fn bind_socket_raw(path: &Path) -> Result<UnixListener, UdsBindError> {
    match UnixListener::bind(path) {
        Ok(l) => {
            chmod_0600(path)?;
            Ok(l)
        }
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            match try_connect_existing(path) {
                Ok(stream) => Err(UdsBindError::AlreadyBound(stream)),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
                {
                    // Orphaned socket file: previous writer-process exited
                    // without cleanup. Unlink and re-bind. We treat NotFound
                    // as orphaned too — a race where the file vanished
                    // between the AddrInUse bind and the connect attempt is
                    // safe to retry as a fresh bind.
                    std::fs::remove_file(path)?;
                    let l = UnixListener::bind(path)?;
                    chmod_0600(path)?;
                    Ok(l)
                }
                Err(e) => Err(UdsBindError::Io(e)),
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// Open a client connection to the socket at `path`. [`bind_socket_raw`] uses
/// it to tell a live writer from an orphaned socket file.
pub fn try_connect_existing(path: &Path) -> io::Result<UnixStream> {
    UnixStream::connect(path)
}

/// Upper bound for a daemon liveness probe (`dreamd status`, etc.). A wedged
/// listener that never completes `connect()` must not hang the caller.
const LIVENESS_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Returns `true` only when a UDS connect to `path` succeeds within
/// [`LIVENESS_PROBE_TIMEOUT`].
///
/// Probe-only: the stream is dropped immediately on success. Orphan socket
/// files (`ConnectionRefused` / `NotFound`) and connect timeouts both map to
/// `false`, matching MCP daemon-proxy fallback and `bind_socket_raw` recovery.
pub fn is_daemon_socket_live(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }

    let path = path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let live = match UnixStream::connect(&path) {
            Ok(_stream) => true,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                false
            }
            Err(_) => false,
        };
        let _ = tx.send(live);
    });

    rx.recv_timeout(LIVENESS_PROBE_TIMEOUT).unwrap_or_default()
}

fn chmod_0600(path: &Path) -> io::Result<()> {
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use tempfile::tempdir;

    #[test]
    fn bind_creates_socket_with_0600_perms() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("dreamd.sock");

        let listener = bind_socket_raw(&sock).expect("first bind ok");
        let meta = std::fs::metadata(&sock).expect("socket exists");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket must be created with 0600 perms");
        drop(listener);
    }

    #[test]
    fn second_bind_returns_already_bound_with_connectable_stream() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("dreamd.sock");
        let listener = bind_socket_raw(&sock).expect("first bind");

        // Accept on the listener in a background thread so the second-bind's
        // connect() call returns.
        let acceptor_listener = listener.try_clone().expect("clone listener");
        let acceptor = std::thread::spawn(move || {
            let (mut stream, _) = acceptor_listener.accept().expect("accept");
            let mut buf = [0u8; 5];
            stream.read_exact(&mut buf).expect("read");
            assert_eq!(&buf, b"hello");
            stream.write_all(b"world").expect("write");
        });

        let err = bind_socket_raw(&sock).expect_err("second bind must not succeed");
        let mut stream = match err {
            UdsBindError::AlreadyBound(s) => s,
            other => panic!("expected AlreadyBound, got {other:?}"),
        };
        stream.write_all(b"hello").unwrap();
        let mut reply = [0u8; 5];
        stream.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"world");

        acceptor.join().expect("acceptor joined");
        drop(listener);
    }

    #[test]
    fn is_daemon_socket_live_false_when_path_missing() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("missing.sock");
        assert!(!is_daemon_socket_live(&sock));
    }

    #[test]
    fn is_daemon_socket_live_false_for_orphan_socket_file() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("dreamd.sock");

        drop(bind_socket_raw(&sock).expect("first bind"));
        assert!(sock.exists(), "stale socket file expected for the test");
        assert!(
            !is_daemon_socket_live(&sock),
            "orphan socket file must not report live"
        );
    }

    #[test]
    fn is_daemon_socket_live_true_for_bound_listener() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("dreamd.sock");
        let listener = bind_socket_raw(&sock).expect("bind ok");
        assert!(
            is_daemon_socket_live(&sock),
            "bound listener must answer connect probe"
        );
        drop(listener);
    }

    #[test]
    fn orphaned_socket_file_is_unlinked_and_rebound() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("dreamd.sock");

        // Simulate a stale socket file: bind, then drop the listener. The
        // kernel socket goes away, but the path remains, so subsequent
        // connect() attempts will see ConnectionRefused — the exact AC scenario.
        drop(bind_socket_raw(&sock).expect("first bind"));
        assert!(sock.exists(), "stale socket file expected for the test");

        let listener = bind_socket_raw(&sock).expect("re-bind on orphaned socket");
        assert!(sock.exists(), "re-bind must leave a fresh socket in place");
        let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        drop(listener);
    }
}
