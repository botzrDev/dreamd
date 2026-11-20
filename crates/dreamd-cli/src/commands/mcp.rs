//! `dreamd mcp` — start the MCP server process (WEG-77).
//!
//! Thin CLI wrapper around [`dreamd_core::mcp::run_mcp_server`]. Builds Tokio's
//! multi-thread runtime (`Runtime::new()`). Nothing on this path needs it for
//! correctness (no `block_in_place`), but the in-process backend's coordinator
//! and indexer tasks run blocking file appends and Tantivy commits inline, and
//! worker threads keep that work off the thread serving the transport.
//!
//! The transport is stdio unless `--bind <ADDR>` is given (AILAB-206), in which
//! case core serves Streamable HTTP at `http://<ADDR>/mcp` — if this binary was
//! built with the non-default `mcp-http` cargo feature. Without it (the release
//! build, kept under the size gate, `MAX_BYTES` in ci.yml) `--bind` parses and
//! is refused, exit 2, naming the feature. `--bind` is parsed
//! by `watch`'s own `parse_bind`, so both commands accept exactly the same IP
//! literals and neither resolves a hostname (junk exits 2). The bind policy is
//! core's `bind_listen`, also shared with `watch`: a non-loopback address
//! without `--insecure` exits 1. `--insecure` without `--bind` exits 2 — unlike
//! `watch`, whose default listener is TCP, `mcp` has no listener to loosen until
//! `--bind` asks for one.
//!
//! `--project-root` is resolved once, by `resolve_project_root`, before any
//! transport or socket probe: it must name a folder that holds `.agent/`
//! itself, and that `.agent/` must not be the daemon home. Otherwise `mcp`
//! exits 2 and says why (DRM-102).

use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

use dreamd_core::launcher::Launcher;
use dreamd_core::layout::same_dir;
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

/// `--project-root` as `dreamd mcp` uses it (DRM-102): `raw` made absolute
/// against `cwd`, which must hold `.agent/` itself, and whose `.agent/` is not
/// `daemon_home`. `Err` is the usage message, exit 2. A leading `~` is not
/// expanded: no shell reads this argument when a harness starts the server.
pub(crate) fn resolve_project_root(
    raw: &Path,
    cwd: &Path,
    daemon_home: Option<&Path>,
    launcher: Launcher,
) -> Result<PathBuf, String> {
    // Not canonicalized: discovery and the X-Agent-Root derivation do that.
    let absolute = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };
    let store = absolute.join(".agent");
    if !store.is_dir() {
        let mut msg = format!(
            "--project-root {}: no .agent/ store in {}. Pass the folder that holds .agent/, \
             or run `{}` there first.",
            raw.display(),
            absolute.display(),
            launcher.command("setup")
        );
        let tilde = matches!(
            raw.components().next(),
            Some(Component::Normal(first)) if first.as_encoded_bytes().starts_with(b"~")
        );
        if tilde {
            msg.push_str(
                " A leading ~ is not expanded here, because no shell reads this argument; \
                 write the full path.",
            );
        }
        return Err(msg);
    }
    if daemon_home.is_some_and(|home| same_dir(&store, home)) {
        return Err(format!(
            "--project-root {}: {} is the daemon home, which holds dreamd's registry, socket \
             and log, not a project store. Pass a project folder.",
            raw.display(),
            store.display()
        ));
    }
    Ok(absolute)
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
/// in-process store there, because `dreamd watch` is the writer (AILAB-174) —
/// so it exits **2**. `dreamd watch` itself runs on that platform (AILAB-192:
/// it binds loopback TCP), so this code is `mcp`'s own, not a shared one.
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

    /// `P` holding `.agent/` as a directory, with an empty `P/src`.
    fn store_folder() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("P");
        std::fs::create_dir_all(p.join(".agent")).unwrap();
        std::fs::create_dir(p.join("src")).unwrap();
        (dir, p)
    }

    /// DRM-102 test 7: a store folder is accepted however it is spelt, made
    /// absolute against the cwd and not canonicalized.
    #[test]
    fn resolve_project_root_accepts_a_store_folder_however_spelt() {
        let (_dir, p) = store_folder();
        let src = p.join("src");
        for (raw, cwd, want) in [
            (p.clone(), Path::new("/nonexistent"), p.clone()),
            (PathBuf::from("."), p.as_path(), p.join(".")),
            (PathBuf::from(".."), src.as_path(), src.join("..")),
        ] {
            assert_eq!(
                resolve_project_root(&raw, cwd, None, Launcher::Binary),
                Ok(want),
                "{} from {}",
                raw.display(),
                cwd.display()
            );
        }
    }

    /// DRM-102 test 8: a folder without `.agent/` of its own is refused,
    /// naming the absolute folder and the launcher's setup command; only a
    /// leading `~` adds the sentence that it is not expanded.
    #[test]
    fn resolve_project_root_refuses_a_folder_without_a_store() {
        let (dir, p) = store_folder();
        let src = p.join("src");
        for launcher in [Launcher::Binary, Launcher::Npx] {
            let err = resolve_project_root(Path::new("."), &src, None, launcher)
                .expect_err("a subfolder of the store folder");
            assert!(
                err.contains(&format!("no .agent/ store in {}", src.display())),
                "{err}"
            );
            assert!(err.contains(&launcher.command("setup")), "{err}");
            assert!(!err.contains("~"), "{err}");
        }
        let err = resolve_project_root(Path::new("/nonexistent/x"), &src, None, Launcher::Binary)
            .expect_err("a path that does not exist");
        assert!(err.contains("no .agent/ store in /nonexistent/x."), "{err}");

        // `B/.agent` sits one level above the cwd, the way `~/B` is meant.
        std::fs::create_dir_all(dir.path().join("B/.agent")).unwrap();
        let cwd = dir.path().join("c");
        std::fs::create_dir(&cwd).unwrap();
        let err = resolve_project_root(Path::new("~/B"), &cwd, None, Launcher::Binary)
            .expect_err("~ is not expanded");
        assert!(err.contains("A leading ~ is not expanded"), "{err}");
        let err = resolve_project_root(Path::new("B"), &cwd, None, Launcher::Binary)
            .expect_err("B is not in the cwd");
        assert!(!err.contains("A leading ~"), "{err}");
    }

    /// DRM-102 test 9: a folder whose `.agent/` is the daemon home is
    /// refused; with no daemon home to compare, the same folder is accepted.
    #[test]
    fn resolve_project_root_refuses_the_daemon_home() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = dir.path().join("H");
        std::fs::create_dir_all(h.join(".agent")).unwrap();
        let daemon_home = h.join(".agent");
        let err = resolve_project_root(&h, dir.path(), Some(&daemon_home), Launcher::Binary)
            .expect_err("the daemon home");
        assert!(err.contains("is the daemon home"), "{err}");
        assert_eq!(
            resolve_project_root(&h, dir.path(), None, Launcher::Binary),
            Ok(h.clone())
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
