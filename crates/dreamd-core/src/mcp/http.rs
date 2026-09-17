//! Streamable HTTP transport for `dreamd mcp --bind <ADDR>` (AILAB-206).
//!
//! Compiled only with the non-default `mcp-http` cargo feature: rmcp's server
//! half adds ~1.5 MB to the stripped binary, and the default release build must
//! stay under NFR-2 (20 MB). CI's clippy and test jobs run `--all-features`.
//!
//! The opt-in sibling of the default stdio transport: the same
//! [`MemoryMcpServer`] — same backend decision, same `search_nodes` /
//! `append_node` tools — served at `http://<ADDR>/mcp` with rmcp's
//! [`StreamableHttpService`]. This is **not** the `dreamd watch` REST surface:
//! nothing under `/api/v1` is mounted here, and the watch router never mounts
//! this service.
//!
//! The listener comes from [`bind_listen`](crate::server::bind_listen), so the
//! bind policy is the AILAB-197 one and only that one: loopback needs no flag,
//! a non-loopback address needs `--insecure`, and `--insecure` warns on every
//! start. The port is unauthenticated — there is no bearer layer and no
//! `server.json` publish — so a loopback bind is reachable by any local process.
//!
//! rmcp's config defaults are kept (`stateful_mode`, SSE responses, the
//! `localhost` / `127.0.0.1` / `::1` `Host` allowlist that guards a loopback
//! server against DNS rebinding). Under `insecure` the `Host` allowlist is
//! cleared, because a LAN client sends the machine's own address as `Host` and
//! would otherwise be refused 403; the bind gate is the security boundary there.
//!
//! Shutdown is Ctrl-C, or SIGTERM on Unix. stdin is never read in this mode, so
//! stdin EOF does not end the server.

use std::future::Future;

use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use super::{McpRunError, MemoryMcpServer};
use crate::server::serve_tcp;

/// The path the MCP endpoint is mounted at.
pub(crate) const MCP_PATH: &str = "/mcp";

/// Serve `server` over Streamable HTTP on `listener` until Ctrl-C (or SIGTERM
/// on Unix).
///
/// `insecure` must be the flag `listener` was bound with: it clears rmcp's
/// `Host` allowlist and changes nothing else.
///
/// # Errors
///
/// [`McpRunError::Io`] if the listener's address cannot be read, a signal
/// handler cannot be installed, or the accept loop fails.
pub(crate) async fn serve_streamable_http(
    server: MemoryMcpServer,
    listener: tokio::net::TcpListener,
    insecure: bool,
) -> Result<(), McpRunError> {
    serve_until(server, listener, insecure, shutdown_signal()).await
}

/// [`serve_streamable_http`] with the shutdown trigger injected, so tests can
/// stop the server without delivering a signal to the test process.
pub(crate) async fn serve_until<F>(
    server: MemoryMcpServer,
    listener: tokio::net::TcpListener,
    insecure: bool,
    shutdown: F,
) -> Result<(), McpRunError>
where
    F: Future<Output = std::io::Result<()>>,
{
    let config = http_config(insecure);
    // Cancelling the config's token on shutdown ends every open session and
    // SSE stream, rather than leaving them to the runtime teardown.
    let sessions = config.cancellation_token.clone();
    let router = router(server, config);
    // stderr only: stdout is not a JSON-RPC channel in this mode, but keeping
    // it silent means an IDE log never mistakes this line for protocol.
    eprintln!(
        "dreamd mcp: Streamable HTTP at http://{}{MCP_PATH}",
        listener.local_addr()?
    );
    let outcome = tokio::select! {
        result = serve_tcp(listener, router) => result,
        signal = shutdown => {
            tracing::info!("dreamd mcp: shutdown signal received, stopping HTTP server");
            signal
        }
    };
    sessions.cancel();
    outcome.map_err(McpRunError::from)
}

/// rmcp's defaults, with the `Host` allowlist cleared when `insecure`.
fn http_config(insecure: bool) -> StreamableHttpServerConfig {
    let config = StreamableHttpServerConfig::default();
    if insecure {
        config.disable_allowed_hosts()
    } else {
        config
    }
}

/// The router `serve_until` serves: the MCP service at [`MCP_PATH`] and
/// nothing else. Every session gets a clone of `server`, so a `Local` backend's
/// coordinator channel and index handle are shared, not reopened.
fn router(server: MemoryMcpServer, config: StreamableHttpServerConfig) -> axum::Router {
    let service: StreamableHttpService<MemoryMcpServer, LocalSessionManager> =
        StreamableHttpService::new(move || Ok(server.clone()), Default::default(), config);
    axum::Router::new().nest_service(MCP_PATH, service)
}

/// Resolve on Ctrl-C, or on SIGTERM on Unix — the signal a service manager
/// stops a process with. `tokio::signal::unix` does not exist off Unix, so
/// there Ctrl-C is the whole set, as in `dreamd watch`.
async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = sigterm.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};
    use std::time::Duration;

    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use tokio::sync::oneshot;

    use super::*;
    use crate::server::{bind_listen, WatchListen};

    const INIT_BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"ailab-206-smoke","version":"0"}}}"#;
    const ACCEPT_BOTH: &str = "application/json, text/event-stream";

    /// A running server on `listener`; dropping `stop` shuts it down.
    struct Running {
        addr: SocketAddr,
        stop: Option<oneshot::Sender<()>>,
        task: tokio::task::JoinHandle<Result<(), McpRunError>>,
    }

    impl Running {
        fn start(listener: tokio::net::TcpListener, insecure: bool) -> Self {
            let addr = listener.local_addr().expect("local_addr");
            let (stop, stopped) = oneshot::channel::<()>();
            let task = tokio::spawn(serve_until(
                MemoryMcpServer::new(),
                listener,
                insecure,
                async move {
                    let _ = stopped.await;
                    Ok(())
                },
            ));
            Self {
                addr,
                stop: Some(stop),
                task,
            }
        }

        async fn shutdown(mut self) {
            drop(self.stop.take());
            let result = tokio::time::timeout(Duration::from_secs(10), self.task)
                .await
                .expect("server stops after the shutdown trigger")
                .expect("server task did not panic");
            assert!(result.is_ok(), "clean shutdown: {result:?}");
        }
    }

    struct Reply {
        status: hyper::StatusCode,
        session: Option<String>,
        body: String,
    }

    /// One HTTP/1.1 request to `addr` over a fresh connection. `host` is sent
    /// verbatim as the `Host` header.
    async fn send(
        addr: SocketAddr,
        host: &str,
        method: hyper::Method,
        path: &str,
        session: Option<&str>,
        body: Option<&str>,
    ) -> Reply {
        use hyper::client::conn::http1;
        use hyper_util::rt::TokioIo;

        let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let (mut sender, conn) = http1::handshake(TokioIo::new(stream))
            .await
            .expect("handshake");
        tokio::spawn(async move {
            let _ = conn.await;
        });

        let mut req = hyper::Request::builder()
            .method(method)
            .uri(path)
            .header(hyper::header::HOST, host)
            .header(hyper::header::ACCEPT, ACCEPT_BOTH);
        if body.is_some() {
            req = req.header(hyper::header::CONTENT_TYPE, "application/json");
        }
        if let Some(id) = session {
            req = req.header("mcp-session-id", id);
        }
        let req = req
            .body(Full::new(Bytes::from(body.unwrap_or("").to_owned())))
            .expect("request");

        tokio::time::timeout(Duration::from_secs(10), async {
            let resp = sender.send_request(req).await.expect("send");
            let status = resp.status();
            let session = resp
                .headers()
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let body = resp.into_body().collect().await.expect("body").to_bytes();
            Reply {
                status,
                session,
                body: String::from_utf8_lossy(&body).into_owned(),
            }
        })
        .await
        .expect("response completes")
    }

    /// The JSON-RPC message with `id` in a response body — either a bare JSON
    /// object or the `data:` lines of an SSE stream.
    fn jsonrpc_with_id(body: &str, id: u64) -> serde_json::Value {
        let candidates = std::iter::once(body.trim()).chain(
            body.lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim),
        );
        candidates
            .filter_map(|text| serde_json::from_str::<serde_json::Value>(text).ok())
            .find(|msg| msg["jsonrpc"] == "2.0" && msg["id"] == id)
            .unwrap_or_else(|| panic!("no JSON-RPC message with id {id} in body: {body:?}"))
    }

    /// Keeps a `WARN`-level subscriber live for the duration of a test that
    /// drives `bind_listen` with `insecure` set — the same discipline the
    /// `server::watch` tests document, so this test cannot cache "never" on the
    /// `--insecure` callsite that their WARN-capture test asserts on.
    fn hold_warn_subscriber() -> tracing::subscriber::DefaultGuard {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(std::io::sink)
            .with_max_level(tracing::Level::WARN)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();
        guard
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn loopback_initialize_returns_200_and_the_tool_pair_is_unchanged() {
        // AC (AILAB-206): `--bind 127.0.0.1:0` goes through the 197 bind gate
        // with no `--insecure`, and POST `initialize` to `/mcp` is a 200 with a
        // JSON-RPC result.
        let listener = bind_listen(WatchListen {
            bind: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
            insecure: false,
        })
        .await
        .expect("loopback bind needs no --insecure");
        let server = Running::start(listener, false);
        let addr = server.addr;
        let host = addr.to_string();

        let init = send(
            addr,
            &host,
            hyper::Method::POST,
            MCP_PATH,
            None,
            Some(INIT_BODY),
        )
        .await;
        assert_eq!(init.status, 200, "initialize body: {}", init.body);
        let result = &jsonrpc_with_id(&init.body, 1)["result"];
        assert_eq!(result["serverInfo"]["name"], "dreamd-mcp", "{result}");
        let session = init
            .session
            .expect("stateful mode issues an Mcp-Session-Id");

        let initialized = send(
            addr,
            &host,
            hyper::Method::POST,
            MCP_PATH,
            Some(&session),
            Some(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
        )
        .await;
        assert_eq!(initialized.status, 202, "{}", initialized.body);

        let list = send(
            addr,
            &host,
            hyper::Method::POST,
            MCP_PATH,
            Some(&session),
            Some(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#),
        )
        .await;
        assert_eq!(list.status, 200, "tools/list body: {}", list.body);
        let mut names: Vec<String> = jsonrpc_with_id(&list.body, 2)["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|tool| tool["name"].as_str().expect("tool name").to_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["append_node", "search_nodes"]);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn non_loopback_without_insecure_is_refused_before_anything_binds() {
        // AC (AILAB-206): `run_mcp_server` hands `--bind` to the 197 gate first,
        // so `0.0.0.0` without `--insecure` is `McpRunError::Bind` naming the
        // flag. The port is held on 127.0.0.1 — as in the watch test — so a
        // bind *attempt* would surface as `Io(AddrInUse)`, not `Bind`.
        let held = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("hold a port");
        let port = held.local_addr().expect("held addr").port();
        let requested = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
        let dir = tempfile::tempdir().expect("tempdir");

        let result = super::super::run_mcp_server(
            dir.path(),
            Some(WatchListen {
                bind: Some(requested),
                insecure: false,
            }),
        )
        .await;
        match result {
            Err(McpRunError::Bind(msg)) => {
                assert!(msg.contains("--insecure"), "must name --insecure: {msg}");
                assert!(msg.contains(&requested.to_string()), "{msg}");
                assert_eq!(McpRunError::Bind(msg.clone()).to_string(), msg);
            }
            other => panic!("expected a pre-bind McpRunError::Bind, got {other:?}"),
        }

        // Nothing was left bound: the port is free again once we let go of it.
        drop(held);
        std::net::TcpListener::bind(requested).expect("no listener left on the refused address");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn insecure_bind_disables_the_host_allowlist() {
        // AC (AILAB-206): with `--insecure`, `0.0.0.0` binds and a LAN `Host:`
        // is not refused by rmcp's loopback allowlist. The contrast server shows
        // the same request IS a 403 on the default (loopback) config, so the
        // 200 is the `disable_allowed_hosts` path and not a lenient default.
        let _warn = hold_warn_subscriber();
        let lan_host = "192.0.2.1";

        let listener = bind_listen(WatchListen {
            bind: Some(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))),
            insecure: true,
        })
        .await
        .expect("--insecure allows 0.0.0.0");
        let port = listener.local_addr().expect("bound").port();
        let insecure = Running::start(listener, true);
        let dial = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let reply = send(
            dial,
            lan_host,
            hyper::Method::POST,
            MCP_PATH,
            None,
            Some(INIT_BODY),
        )
        .await;
        assert_ne!(
            reply.status, 403,
            "Host allowlist not disabled: {}",
            reply.body
        );
        assert_eq!(reply.status, 200, "{}", reply.body);
        assert_eq!(
            jsonrpc_with_id(&reply.body, 1)["result"]["serverInfo"]["name"],
            "dreamd-mcp"
        );
        insecure.shutdown().await;

        let listener = bind_listen(WatchListen::default()).await.expect("loopback");
        let loopback = Running::start(listener, false);
        let reply = send(
            loopback.addr,
            lan_host,
            hyper::Method::POST,
            MCP_PATH,
            None,
            Some(INIT_BODY),
        )
        .await;
        assert_eq!(
            reply.status, 403,
            "loopback keeps rmcp's Host allowlist: {}",
            reply.body
        );
        loopback.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rest_api_is_not_mounted_on_the_mcp_port() {
        // AC (AILAB-206): the MCP port serves `/mcp` only — the watch REST
        // surface (`/api/v1`) is a different server.
        let listener = bind_listen(WatchListen::default()).await.expect("loopback");
        let server = Running::start(listener, false);
        let host = server.addr.to_string();
        for path in ["/api/v1/recall?q=x", "/api/v1/recall"] {
            let reply = send(server.addr, &host, hyper::Method::GET, path, None, None).await;
            assert_eq!(reply.status, 404, "GET {path}: {}", reply.body);
        }
        server.shutdown().await;
    }
}
