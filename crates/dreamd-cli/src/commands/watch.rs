//! `dreamd watch` — foreground daemon mode (WEG-88 / DR-702).
//!
//! Thin CLI wrapper around [`dreamd_core::server::run_watch`]. One `run` for
//! every target: the watch command boots a per-project Supervisor against
//! `cwd`'s AgentRoot, serves the API router, and blocks until a shutdown
//! signal. Only the transport differs, and that difference lives entirely in
//! core (AILAB-192):
//!
//! - **Unix** binds `~/.agent/dreamd.sock` and authenticates callers by peer
//!   UID (`SO_PEERCRED`); it blocks until SIGINT or SIGTERM.
//! - **Off Unix** there is no UDS and no `SO_PEERCRED`, so `watch` binds
//!   loopback TCP on `127.0.0.1:0`, publishes the bound port to
//!   `~/.agent/server.json` for clients to discover, and requires a bearer
//!   token at `~/.agent/auth.json` — minted by `dreamd service install`
//!   (AILAB-203), never by `watch`. A missing or malformed token file is a
//!   start failure (exit 1), because that token is the listener's only auth.
//!   It blocks until Ctrl-C.
//!
//!
//! `--bind <ADDR>` / `--insecure` (AILAB-197) configure that TCP listener only.
//! `--bind` is an IP literal with an optional port, parsed here so that a
//! hostname is a usage error (exit 2) rather than a DNS lookup. A non-loopback
//! address without `--insecure` is refused by core before it binds (exit 1). On
//! Unix either flag exits 2: there is no TCP listener for them to configure.
//!
//! `dreamd watch` is the foreground form of the daemon. OS-level service
//! registration is `dreamd service install`, which supervises this same
//! foreground process (systemd `--user` on Linux, a LaunchAgent on macOS).

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::process::ExitCode;

use dreamd_core::server::{run_watch, WatchError, WatchListen};

use crate::cli::WatchArgs;

/// Entry point for `dreamd watch`.
///
/// Blocks until a shutdown signal or a fatal server error. Returns exit code 0
/// on clean shutdown, 2 for an unparseable `--bind`, otherwise
/// [`exit_code_for`]'s verdict.
pub fn run(cwd: &Path, args: &WatchArgs) -> ExitCode {
    let listen = match listen_from_args(args) {
        Ok(listen) => listen,
        Err(msg) => {
            eprintln!("dreamd watch: {msg}");
            return ExitCode::from(2);
        }
    };
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    match rt.block_on(run_watch(cwd, listen)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dreamd watch: {e}");
            ExitCode::from(exit_code_for(&e))
        }
    }
}

/// Map clap's `watch` flags onto core's [`WatchListen`].
///
/// No `--bind` is `None` (core binds `127.0.0.1:0`). Otherwise `--bind` must be
/// an IP literal: `IP:port`, `[IPv6]:port`, or a bare `IP` / `[IPv6]` meaning
/// port 0. Anything else — a hostname, an empty string — is `Err` with a
/// user-facing message, and nothing is resolved.
fn listen_from_args(args: &WatchArgs) -> Result<WatchListen, String> {
    let bind = match args.bind.as_deref() {
        None => None,
        Some(raw) => Some(parse_bind(raw).ok_or_else(|| {
            format!(
                "--bind {raw:?} is not an IP address or IP:port \
                 (e.g. 127.0.0.1:8080 or [::1]:0); hostnames are not resolved"
            )
        })?),
    };
    Ok(WatchListen {
        bind,
        insecure: args.insecure,
    })
}

fn parse_bind(raw: &str) -> Option<SocketAddr> {
    if let Ok(addr) = raw.parse::<SocketAddr>() {
        return Some(addr);
    }
    if let Ok(ip) = raw.parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, 0));
    }
    let bracketed = raw.strip_prefix('[')?.strip_suffix(']')?;
    let ip = bracketed.parse::<Ipv6Addr>().ok()?;
    Some(SocketAddr::new(IpAddr::V6(ip), 0))
}

/// Exit code for a watch failure: 2 for "no project root" (a usage refusal —
/// the operator pointed `watch` at a directory with no store, the same class of
/// mistake `dreamd mcp` exits 2 on) and for `--bind` / `--insecure` on Unix
/// (flags for a TCP listener that target does not have), 1 for everything else.
///
/// Everything else explicitly includes [`WatchError::Bind`] — a non-loopback
/// address without `--insecure` is a valid invocation the daemon refuses at
/// start — and [`WatchError::AuthToken`], a missing or
/// malformed `~/.agent/auth.json` on a loopback-TCP target. That is an operator
/// *setup* error to be fixed with `dreamd service install`, not a malformed
/// invocation, so it exits 1 like every other start failure.
///
/// Split out of [`run`] so the mapping is unit-testable without binding a
/// listener or spawning a runtime.
fn exit_code_for(err: &WatchError) -> u8 {
    match err {
        WatchError::NoProjectRoot(_) | WatchError::TcpFlagsOnUnix => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pointing `watch` at a directory with no `.agent/` store is a usage
    /// refusal, and `dreamd mcp` exits 2 on the same class of mistake.
    #[test]
    fn no_project_root_exits_2() {
        assert_eq!(
            exit_code_for(&WatchError::NoProjectRoot("/tmp/nope".to_string())),
            2
        );
    }

    /// A missing/malformed `~/.agent/auth.json` is a setup error, not a usage
    /// error: the invocation was fine, the daemon home is not provisioned yet.
    #[test]
    fn auth_token_error_exits_1() {
        let missing = std::path::Path::new("/nonexistent/dreamd-auth-probe/auth.json");
        let err = dreamd_core::daemon_client::read_auth_token(missing)
            .expect_err("a nonexistent path cannot yield a token");
        assert_eq!(exit_code_for(&WatchError::AuthToken(err.to_string())), 1);
    }

    /// The operator has to be told which command mints the file, or "auth
    /// token: ..." is a dead end. The copy is owned by `AuthTokenError`, so
    /// this pins the sentence `watch` actually prints.
    #[test]
    fn auth_token_error_names_service_install() {
        let missing = std::path::Path::new("/nonexistent/dreamd-auth-probe/auth.json");
        let err = dreamd_core::daemon_client::read_auth_token(missing)
            .expect_err("a nonexistent path cannot yield a token");
        let line = WatchError::AuthToken(err.to_string()).to_string();
        assert!(
            line.contains("dreamd service install"),
            "watch's auth-token failure must name the minting command; got: {line}"
        );
    }

    /// A non-loopback bind is refused at start unless `--insecure` (AILAB-197);
    /// that is a runtime failure, not a usage one.
    #[test]
    fn bind_error_exits_1() {
        assert_eq!(
            exit_code_for(&WatchError::Bind(
                "bound 10.0.0.5:1234, not loopback".to_string()
            )),
            1
        );
    }

    /// The refusal core returns for a non-loopback `--bind` without
    /// `--insecure` also exits 1, same as the post-bind check.
    #[tokio::test]
    async fn non_loopback_bind_without_insecure_exits_1_and_names_the_flag() {
        let err = dreamd_core::server::bind_listen(WatchListen {
            bind: Some("0.0.0.0:0".parse().unwrap()),
            insecure: false,
        })
        .await
        .expect_err("0.0.0.0 without --insecure must be refused");
        assert!(matches!(err, WatchError::Bind(_)), "{err:?}");
        assert!(err.to_string().contains("--insecure"), "{err}");
        assert_eq!(exit_code_for(&err), 1);
    }

    /// `--bind` / `--insecure` on Unix configure a listener that does not
    /// exist: a usage error, exit 2, not the runtime `Bind` exit 1.
    #[test]
    fn tcp_flags_on_unix_exits_2() {
        assert_eq!(exit_code_for(&WatchError::TcpFlagsOnUnix), 2);
    }

    fn args(bind: Option<&str>, insecure: bool) -> WatchArgs {
        WatchArgs {
            bind: bind.map(str::to_owned),
            insecure,
        }
    }

    #[test]
    fn no_bind_is_the_default_listener() {
        let listen = listen_from_args(&args(None, false)).expect("no flags");
        assert!(listen.bind.is_none());
        assert!(!listen.insecure);

        let listen = listen_from_args(&args(None, true)).expect("--insecure alone");
        assert!(listen.bind.is_none());
        assert!(listen.insecure, "--insecure must be threaded, not dropped");
    }

    #[test]
    fn bind_parses_ip_literals_with_and_without_a_port() {
        for (raw, want) in [
            ("127.0.0.1", "127.0.0.1:0"),
            ("127.0.0.1:8080", "127.0.0.1:8080"),
            ("[::1]:0", "[::1]:0"),
            ("[::1]:9000", "[::1]:9000"),
            ("::1", "[::1]:0"),
            ("[::1]", "[::1]:0"),
            ("0.0.0.0:0", "0.0.0.0:0"),
        ] {
            let listen =
                listen_from_args(&args(Some(raw), false)).unwrap_or_else(|e| panic!("{raw}: {e}"));
            assert_eq!(listen.bind, Some(want.parse().unwrap()), "{raw}");
        }
    }

    #[test]
    fn bind_refuses_hostnames_and_junk_without_resolving() {
        for raw in [
            "localhost",
            "localhost:8080",
            "example.com:80",
            "",
            "[::1",
            "127.0.0.1:",
        ] {
            let err = listen_from_args(&args(Some(raw), true))
                .expect_err(&format!("{raw:?} must not parse"));
            assert!(err.contains("--bind"), "{raw:?}: {err}");
            assert!(err.contains("not resolved"), "{raw:?}: {err}");
        }
    }
}
