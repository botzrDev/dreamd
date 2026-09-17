//! `dreamd mcp` — start the MCP server process (WEG-77).
//!
//! Thin CLI wrapper around [`dreamd_core::mcp::run_mcp_server`]. Builds a
//! single-threaded Tokio runtime (the MCP bridge is I/O-bound; multi-thread
//! would be overkill and the CLI crate already owns the `rt-multi-thread`
//! feature flag for the daemon path).
//!
//! The transport is stdio unless `--bind <ADDR>` is given (AILAB-206), in which
//! case core serves Streamable HTTP at `http://<ADDR>/mcp` — if this binary was
//! built with the non-default `mcp-http` cargo feature. Without it (the release
//! build, kept under NFR-2) `--bind` parses and is refused, exit 2, naming the
//! feature. `--bind` is parsed
//! by `watch`'s own [`parse_bind`], so both commands accept exactly the same IP
//! literals and neither resolves a hostname (junk exits 2). The bind policy is
//! core's `bind_listen`, also shared with `watch`: a non-loopback address
//! without `--insecure` exits 1. `--insecure` without `--bind` exits 2 — unlike
//! `watch`, whose default listener is TCP, `mcp` has no listener to loosen until
//! `--bind` asks for one.

use std::path::Path;
use std::process::ExitCode;

use dreamd_core::mcp::{run_mcp_server, McpRunError};
use dreamd_core::server::WatchListen;

use crate::cli::McpArgs;
use crate::commands::watch::parse_bind;

/// Entry point for `dreamd mcp`.
///
/// Blocks until the MCP session ends: on stdio, when the transport closes or
/// the daemon bridge disconnects; over HTTP, on Ctrl-C / SIGTERM. All MCP
/// JSON-RPC traffic on stdio uses stdout; all diagnostic / error output uses
/// stderr.
pub fn run(cwd: &Path, args: &McpArgs) -> ExitCode {
    let listen = match listen_from_args(args) {
        Ok(listen) => listen,
        Err(msg) => {
            eprintln!("dreamd mcp: {msg}");
            return ExitCode::from(2);
        }
    };
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    match rt.block_on(run_mcp_server(cwd, listen)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            match &e {
                McpRunError::Io(io) => eprintln!("dreamd mcp: I/O error — {io}"),
                _ => eprintln!("dreamd mcp: {e}"),
            }
            ExitCode::from(exit_code_for(&e))
        }
    }
}

/// Usage message for `--insecure` without `--bind` (exit 2).
const INSECURE_REQUIRES_BIND: &str =
    "--insecure requires --bind: without --bind, dreamd mcp serves stdio and has no TCP listener";

/// Map clap's `mcp` transport flags onto core's listen options.
///
/// No `--bind` is `None` — stdio. `--bind` must be an IP literal (see
/// [`parse_bind`]). `Err` carries a user-facing usage message, exit 2.
pub(crate) fn listen_from_args(args: &McpArgs) -> Result<Option<WatchListen>, String> {
    if args.insecure && args.bind.is_none() {
        return Err(INSECURE_REQUIRES_BIND.to_string());
    }
    let Some(raw) = args.bind.as_deref() else {
        return Ok(None);
    };
    let bind = parse_bind(raw).ok_or_else(|| {
        format!(
            "--bind {raw:?} is not an IP address or IP:port \
             (e.g. 127.0.0.1:8080 or [::1]:0); hostnames are not resolved"
        )
    })?;
    Ok(Some(WatchListen {
        bind: Some(bind),
        insecure: args.insecure,
    }))
}

/// Raw exit code for a failed MCP run. Split out of [`run`] so the mapping is
/// unit-testable without spawning a runtime or a stdio transport.
///
/// [`McpRunError::Unsupported`] is a usage refusal — off Unix it means no
/// reachable daemon to proxy to, and `dreamd mcp` deliberately does not boot an
/// in-process store it could not durably write (AILAB-174) — so it exits **2**.
/// `dreamd watch` no longer refuses on that platform at all (AILAB-192: it
/// binds loopback TCP), so this code is `mcp`'s own, not a shared one.
/// [`McpRunError::HttpTransportNotBuilt`] — `--bind` on a build without the
/// `mcp-http` feature — is a usage refusal too: exit 2.
/// Everything else is a runtime failure: 1 — including [`McpRunError::Bind`], a
/// valid `--bind` the gate refused at start, exactly as `watch` treats it.
pub(crate) fn exit_code_for(e: &McpRunError) -> u8 {
    match e {
        McpRunError::Unsupported | McpRunError::HttpTransportNotBuilt => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;

    fn args(bind: Option<&str>, insecure: bool) -> McpArgs {
        McpArgs {
            manual_only: false,
            project_root: None,
            bind: bind.map(str::to_owned),
            insecure,
        }
    }

    #[test]
    fn unsupported_is_a_usage_refusal_exit_2() {
        assert_eq!(exit_code_for(&McpRunError::Unsupported), 2);
    }

    #[test]
    fn http_transport_not_built_is_a_usage_refusal_exit_2() {
        assert_eq!(exit_code_for(&McpRunError::HttpTransportNotBuilt), 2);
    }

    #[test]
    fn other_errors_stay_exit_1() {
        assert_eq!(exit_code_for(&McpRunError::NoHome), 1);
        assert_eq!(exit_code_for(&McpRunError::Service("boom".to_string())), 1);
    }

    #[test]
    fn no_flags_is_stdio() {
        assert!(listen_from_args(&args(None, false))
            .expect("no flags")
            .is_none());
    }

    #[test]
    fn insecure_without_bind_exits_2() {
        let msg = listen_from_args(&args(None, true)).expect_err("--insecure alone");
        assert!(msg.contains("--insecure requires --bind"), "{msg}");
        assert_eq!(
            run(Path::new("/nonexistent"), &args(None, true)),
            ExitCode::from(2)
        );
    }

    #[test]
    fn bind_threads_the_address_and_insecure() {
        for (raw, insecure, want) in [
            ("127.0.0.1:0", false, "127.0.0.1:0"),
            ("127.0.0.1", false, "127.0.0.1:0"),
            ("[::1]:9000", false, "[::1]:9000"),
            ("0.0.0.0:8080", true, "0.0.0.0:8080"),
        ] {
            let listen = listen_from_args(&args(Some(raw), insecure))
                .unwrap_or_else(|e| panic!("{raw}: {e}"))
                .expect("--bind selects HTTP");
            assert_eq!(
                listen.bind,
                Some(want.parse::<SocketAddr>().unwrap()),
                "{raw}"
            );
            assert_eq!(listen.insecure, insecure, "{raw}");
        }
    }

    #[test]
    fn bind_refuses_hostnames_and_junk_exit_2() {
        for raw in ["localhost", "localhost:8080", "example.com:80", "", "[::1"] {
            let err = listen_from_args(&args(Some(raw), false))
                .expect_err(&format!("{raw:?} must not parse"));
            assert!(
                err.contains("--bind") && err.contains("not resolved"),
                "{raw:?}: {err}"
            );
            assert_eq!(
                run(Path::new("/nonexistent"), &args(Some(raw), false)),
                ExitCode::from(2),
                "{raw:?}"
            );
        }
    }

    /// Without the `mcp-http` feature a well-formed `--bind` reaches core and is
    /// refused there, before anything binds: exit 2.
    #[cfg(not(feature = "mcp-http"))]
    #[test]
    fn bind_without_the_mcp_http_feature_exits_2() {
        for (raw, insecure) in [("127.0.0.1:0", false), ("0.0.0.0:0", true)] {
            assert_eq!(
                run(Path::new("/nonexistent"), &args(Some(raw), insecure)),
                ExitCode::from(2),
                "{raw}"
            );
        }
    }

    /// A non-loopback `--bind` without `--insecure` reaches core, which refuses
    /// it before binding: exit 1, and the message names the flag.
    #[cfg(feature = "mcp-http")]
    #[test]
    fn non_loopback_bind_without_insecure_exits_1() {
        assert_eq!(
            run(Path::new("/nonexistent"), &args(Some("0.0.0.0:0"), false)),
            ExitCode::from(1)
        );
    }

    #[test]
    fn bind_refusal_is_a_runtime_failure_exit_1() {
        let err = McpRunError::Bind(
            "refusing to bind non-loopback address 0.0.0.0:0; \
             pass --insecure to serve TCP beyond localhost"
                .to_string(),
        );
        assert!(err.to_string().contains("--insecure"), "{err}");
        assert_eq!(exit_code_for(&err), 1);
    }
}
