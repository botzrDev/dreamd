//! API socket bind helper for the writer-process HTTP server (WEG-71 / DR-406).
//!
//! Wraps [`bind_socket_raw`] with parent-directory creation and permissions so
//! the server entry point gets a ready-to-use `UnixListener` in one call.
//!
//! Unix-only. Windows path is DR-121 / WEG-135, deferred to v0.1.1.

#![cfg(unix)]

use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::server::lifecycle::ServerError;
use crate::server::uds::bind_socket_raw;

/// Bind the API unix-domain socket at `path`.
///
/// Creates the parent directory (mode `0700`) if it does not yet exist, then
/// delegates to [`bind_socket_raw`] for the bind-and-recover logic and `0600`
/// permissions. Returns a `UnixListener` ready for `accept` calls.
pub fn bind_api_socket(
    path: &Path,
) -> Result<std::os::unix::net::UnixListener, ServerError> {
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)?;
            fs::set_permissions(parent, Permissions::from_mode(0o700))?;
        }
    }
    let listener = bind_socket_raw(path)?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener as StdUnixListener;

    #[test]
    fn bind_succeeds_and_perms_are_0600() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock_path = dir.path().join("test.sock");

        let _listener = bind_api_socket(&sock_path).expect("bind should succeed");

        let meta = fs::metadata(&sock_path).expect("socket file should exist");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket perms should be 0600, got {:o}", mode);
    }

    #[test]
    fn stale_socket_is_recovered() {
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
