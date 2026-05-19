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
//! Cleanup: the returned [`SocketGuard`] unlinks the socket file on drop.
//! The supervisor holds it for the entire process lifetime; mid-process panic
//! or graceful shutdown both run the same cleanup path.
//!
//! Unix-only. Windows path is DR-121 / WEG-135, deferred to v0.1.1.

#![cfg(unix)]

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

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

/// Owns the bound `UnixListener` and the cleanup of the on-disk socket file.
/// Dropping the guard unlinks the file — supervisor MUST keep the guard alive
/// for the lifetime of the writer-process.
#[derive(Debug)]
pub struct SocketGuard {
    listener: UnixListener,
    path: PathBuf,
    unlink_on_drop: bool,
}

impl SocketGuard {
    pub fn listener(&self) -> &UnixListener {
        &self.listener
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Skip the on-disk unlink that Drop would otherwise perform. Tests use
    /// this to simulate an orphaned socket file: a previous writer-process
    /// that crashed before cleanup left the path behind even though the
    /// kernel socket is gone. Workspace forbids `unsafe_code`, so we can't
    /// move the listener out and skip Drop — the disarm flag is the safe
    /// equivalent. The listener inside the guard is still closed normally
    /// when the guard drops, releasing the kernel resource.
    #[cfg(test)]
    pub(crate) fn disarm_unlink(&mut self) {
        self.unlink_on_drop = false;
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if self.unlink_on_drop {
            // Best-effort: an unlink failure (e.g., path already removed by an
            // operator running `rm`) is not actionable here.
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Core bind logic: attempt to bind `path` as a `UnixListener`, handling
/// `AddrInUse` by probing the existing socket for liveness.
///
/// * Success → returns the bound `UnixListener` (already chmod'd `0600`).
/// * Live competitor → returns [`UdsBindError::AlreadyBound`] with a
///   connected stream.
/// * Stale/orphaned file → unlinks and re-binds transparently.
///
/// Exposed as `pub(crate)` so `uds_server` can call it directly without
/// duplicating the bind-and-recover logic.
pub(crate) fn bind_socket_raw(path: &Path) -> Result<UnixListener, UdsBindError> {
    match UnixListener::bind(path) {
        Ok(l) => {
            chmod_0600(path)?;
            Ok(l)
        }
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            match try_connect_existing(path) {
                Ok(stream) => Err(UdsBindError::AlreadyBound(stream)),
                Err(e) if matches!(
                    e.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) => {
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

/// Try to claim the writer-process socket at `path`.
///
/// On success, returns a [`SocketGuard`] owning the bound listener and
/// responsible for cleanup. On a live competitor, returns
/// [`UdsBindError::AlreadyBound`] with the connected stream so the caller can
/// proceed as a client. Stale/orphaned sockets (file exists but `connect()`
/// is refused) are silently unlinked and re-bound — this is the AC's
/// "never error out on a stale socket" path.
pub fn bind_writer_socket(path: &Path) -> Result<SocketGuard, UdsBindError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = bind_socket_raw(path)?;
    Ok(SocketGuard {
        listener,
        path: path.to_path_buf(),
        unlink_on_drop: true,
    })
}

/// Open a client connection to the writer-process at `path`. Used by the
/// `npx dreamd-mcp` client path after a `bind_writer_socket` call returns
/// `AlreadyBound`, and by tests asserting client/writer wiring.
pub fn try_connect_existing(path: &Path) -> io::Result<UnixStream> {
    UnixStream::connect(path)
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

        let guard = bind_writer_socket(&sock).expect("first bind ok");
        let meta = std::fs::metadata(&sock).expect("socket exists");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket must be created with 0600 perms");
        drop(guard);
        assert!(!sock.exists(), "Drop must remove the socket file from disk");
    }

    #[test]
    fn second_bind_returns_already_bound_with_connectable_stream() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("dreamd.sock");
        let guard = bind_writer_socket(&sock).expect("first bind");

        // Accept on the listener in a background thread so the second-bind's
        // connect() call returns.
        let listener = guard.listener().try_clone().expect("clone listener");
        let acceptor = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 5];
            stream.read_exact(&mut buf).expect("read");
            assert_eq!(&buf, b"hello");
            stream.write_all(b"world").expect("write");
        });

        let err = bind_writer_socket(&sock).expect_err("second bind must not succeed");
        let mut stream = match err {
            UdsBindError::AlreadyBound(s) => s,
            other => panic!("expected AlreadyBound, got {other:?}"),
        };
        stream.write_all(b"hello").unwrap();
        let mut reply = [0u8; 5];
        stream.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"world");

        acceptor.join().expect("acceptor joined");
        drop(guard);
    }

    #[test]
    fn orphaned_socket_file_is_unlinked_and_rebound() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("dreamd.sock");

        // Simulate a stale socket file: bind, then drop the guard *without*
        // its on-disk cleanup. The kernel socket goes away (listener drops),
        // but the path remains, so subsequent connect() attempts will see
        // ConnectionRefused — the exact AC scenario.
        {
            let mut guard = bind_writer_socket(&sock).expect("first bind");
            guard.disarm_unlink();
            drop(guard);
        }
        assert!(sock.exists(), "stale socket file expected for the test");

        let guard = bind_writer_socket(&sock).expect("re-bind on orphaned socket");
        assert!(sock.exists(), "re-bind must leave a fresh socket in place");
        let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        drop(guard);
    }
}
