//! Shared outbound HTTP transport to the dreamd daemon.
//!
//! One per-call-connect implementation used by BOTH the MCP Remote backend
//! (`mcp::send_remote`) and the dream-cycle CLI proxy (`client::proxy_dream_cycle`).
//! Owns only the transport + address resolution; each caller keeps its own
//! request construction and status→outcome mapping. Per-call connect (no pool):
//! daemon traffic is infrequent (WEG-78-A rationale).
//!
//! Compiled on every target as of AILAB-192, because the daemon's address is not
//! the same shape on every target: Unix speaks HTTP over `~/.agent/dreamd.sock`
//! and authenticates by peer UID, while Windows has no `SO_PEERCRED` and will
//! speak the same HTTP over loopback TCP with an `Authorization: Bearer` token.
//! The UDS half below therefore keeps its own `#[cfg(unix)]` gates —
//! [`DaemonTransportError`] is the one piece both transports share, so it is the
//! error type of both halves.
//!
//! So this module owns both transports and, for the TCP one, its address book
//! and its credential:
//!
//! - **UDS (`#[cfg(unix)]`)** — [`resolve_daemon_socket`] resolves
//!   `$DREAMD_SOCK` / `~/.agent/dreamd.sock`, [`send_one`] drives one request
//!   over it. Authentication is the peer UID the server reads off the socket,
//!   so nothing is sent in the request.
//! - **Loopback TCP (cfg-free)** — [`read_server_json`] / [`resolve_daemon_tcp`]
//!   read the ephemeral address the daemon published at
//!   [`DaemonHome::server_json`], refusing anything that is not loopback;
//!   [`read_auth_token`] loads the AILAB-203 token from
//!   [`DaemonHome::auth_json`]; [`send_one_tcp`] dials the endpoint with
//!   `Authorization: Bearer` attached; [`tcp_endpoint_is_live`] /
//!   [`tcp_daemon_is_live`] are the synchronous liveness probes the CLI's
//!   `status` and `archive` guards use.
//!
//! The token never leaves this module except as the 64-hex string in that one
//! header: [`AuthToken`] has a hand-written [`Debug`] that redacts, and no
//! error type here carries the bytes.

// Every import below is now reached from the cfg-free loopback-TCP half as well
// as the `#[cfg(unix)]` UDS half, so none of them may be gated (AILAB-192):
//
// - `Path` is the argument type of `read_auth_token`, `read_server_json` and
//   `tcp_daemon_is_live`; `PathBuf` is `AuthTokenError::Missing`'s payload and
//   what `resolve_daemon_tcp` builds from `dirs::home_dir()`.
// - `Bytes` + `Full` are `send_one_tcp`'s request/response body types and
//   `BodyExt` is what gives the response its `.collect()`.
// - `DaemonHome` resolves `~/.agent/server.json` in `resolve_daemon_tcp`.
//
// Gating any of them would break the Windows build; leaving an import gated that
// only the UDS half uses would be an `unused_imports` warning off Unix — which is
// why this list is exactly the intersection, no wider.
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use serde::Deserialize;

use crate::layout::DaemonHome;

/// A transport-stage failure talking to the daemon. Variant granularity lets
/// each caller reproduce its existing message + the dream proxy distinguish
/// "no live daemon" (`Unreachable`) from real failures.
///
/// Deliberately transport-agnostic (AILAB-192): "connect", "handshake", "send"
/// and "read body" are the same four stages over a socket or over TCP, and a
/// caller's mapping to an exit code or an MCP error should not have to care
/// which one it got.
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
///
/// Unix-only (AILAB-192): both variants describe a UDS address — a relative
/// `DREAMD_SOCK` or a home directory the socket path would hang off. There is no
/// socket to resolve off Unix.
#[cfg(unix)]
#[derive(Debug)]
pub enum SockPathError {
    /// `DREAMD_SOCK` was set to a relative path (never a valid daemon address).
    RelativeSockPath(PathBuf),
    /// Home directory could not be resolved.
    NoHome,
}

/// Resolve the daemon UDS path: `$DREAMD_SOCK` (must be absolute) else
/// `~/.agent/dreamd.sock`.
///
/// Unix-only (AILAB-192) — it returns a UDS address and `$DREAMD_SOCK` is a UDS
/// override. Off Unix the daemon's address is a `host:port` the daemon publishes
/// at [`DaemonHome::server_json`](crate::layout::DaemonHome::server_json), which
/// wants its own resolver with its own failure set rather than this one widened.
#[cfg(unix)]
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
///
/// Unix-only (AILAB-192): `tokio::net::UnixStream` does not exist off Unix. The
/// loopback-TCP sibling mirrors this function stage for stage.
#[cfg(unix)]
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

// ---------------------------------------------------------------------------
// AILAB-192 — loopback TCP transport: bearer token, endpoint, client.
//
// Everything below is deliberately cfg-free even though only Windows serves
// over loopback TCP today. `#[cfg(not(unix))]` bodies never compile under a
// Linux `cargo check`, so a typo in one stays invisible until a Windows CI run
// (drift catalog: `cfg(not(unix))` dead code is invisible to unix checks), and
// Windows cannot be compiled on the machines this is developed on. Keeping the
// token parsing, the loopback guard and the client cfg-free means every one of
// them is type-checked AND unit-tested by the Linux gate that guards main.
// ---------------------------------------------------------------------------

/// Lowercase hex alphabet for [`AuthToken::as_hex`].
///
/// A 16-byte table rather than the `hex` crate: AILAB-192 is explicitly a
/// no-new-dependency ticket (NFR-2 caps the stripped binary at 20 MB), and hex
/// encoding a fixed 32-byte secret is two table lookups per byte.
const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";

/// How long [`tcp_endpoint_is_live`] waits for the daemon's TCP accept.
///
/// The probe is a liveness check on `127.0.0.1`, where a listening socket
/// completes the handshake in microseconds and a closed port answers with
/// `ECONNREFUSED` immediately. 250 ms is therefore slack for a loaded machine,
/// not an expected wait — and it bounds how long `dreamd status` /
/// `dreamd ... archive` can block before deciding no daemon is up.
const TCP_LIVENESS_TIMEOUT: Duration = Duration::from_millis(250);

/// The daemon's 32-byte bearer secret, decoded from `~/.agent/auth.json`.
///
/// Minted by `dreamd service install` (AILAB-203), never by `dreamd watch` —
/// the watch reads this file and refuses to start without it, so a rotate is
/// always an explicit operator action (`dreamd service install --force`).
///
/// The inner bytes are private and there is no accessor for them beyond
/// [`Self::as_hex`] (the wire form the client must send) and
/// [`Self::matches_hex`] (the only comparison the server needs). The [`Debug`]
/// impl is hand-written so that a `{:?}` of any struct that happens to hold one
/// — an `AppState`, an MCP backend, a `tracing` field — cannot print the
/// secret.
pub struct AuthToken([u8; 32]);

// Hand-written so the secret can never reach a log line, a panic message, an
// error `Display`, or a `#[derive(Debug)]` on an enclosing type (AILAB-192
// hard rule: never log or print the bearer token). A derived `Debug` would
// print all 32 bytes.
impl std::fmt::Debug for AuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthToken(<redacted>)")
    }
}

impl AuthToken {
    /// The 64-lowercase-hex wire form, i.e. the credential that goes after
    /// `Authorization: Bearer `.
    ///
    /// Round-trips with [`decode_hex32`] by construction, which is what lets
    /// [`Self::matches_hex`] compare decoded bytes instead of strings.
    pub fn as_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            out.push(char::from(HEX_LOWER[usize::from(byte >> 4)]));
            out.push(char::from(HEX_LOWER[usize::from(byte & 0x0f)]));
        }
        out
    }

    /// Whether `presented` (a bare hex credential, no `Bearer ` prefix) is this
    /// token.
    ///
    /// `presented` is decoded first: anything that is not exactly 64 hex digits
    /// is `false` immediately, because a length or alphabet mismatch is public
    /// information about the *request*, not about the secret. The two 32-byte
    /// arrays are then folded together with XOR, so the loop always touches all
    /// 32 bytes and returns after the same amount of work whether the first
    /// byte differs or none do. That is what keeps a caller from binary
    /// searching the token one byte at a time off response latency.
    ///
    /// Written by hand rather than by pulling a constant-time-comparison crate:
    /// AILAB-192 takes no new dependency, and a 32-byte fold is small enough to
    /// review in place. Do not "simplify" this to `self.0 == presented` — array
    /// `PartialEq` is a `memcmp`, which short-circuits on the first differing
    /// byte.
    pub fn matches_hex(&self, presented: &str) -> bool {
        let Some(presented) = decode_hex32(presented) else {
            return false;
        };
        self.0
            .iter()
            .zip(presented.iter())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
    }
}

/// Decode exactly 64 ASCII hex digits into 32 bytes; `None` for anything else.
///
/// Accepts upper- and lower-case input even though AILAB-203 writes the file
/// lowercase, because the *presented* credential comes off the wire from an
/// arbitrary HTTP client and case is not a security property. Every other
/// shape — 63 or 65 digits, a non-hex byte, an empty string, multi-byte UTF-8
/// — is `None`; the length check is on bytes, so a string whose UTF-8 encoding
/// happens to be 64 bytes long still fails at the first non-ASCII digit.
pub fn decode_hex32(s: &str) -> Option<[u8; 32]> {
    let bytes = s.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (slot, pair) in out.iter_mut().zip(bytes.chunks_exact(2)) {
        let hi = hex_nibble(pair[0])?;
        let lo = hex_nibble(pair[1])?;
        *slot = (hi << 4) | lo;
    }
    Some(out)
}

/// One ASCII hex digit → its 0–15 value; `None` for any other byte.
fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Why `~/.agent/auth.json` could not be turned into an [`AuthToken`].
///
/// Three variants because the callers want three different reactions:
/// `dreamd watch` turns any of them into a start failure that points at
/// `dreamd service install`, while the request-time bearer middleware maps all
/// three to a flat 401 (a client must not be able to tell "no token file yet"
/// from "wrong token" — that is a probe oracle).
///
/// None of the messages echo the file's contents. `Missing` names the path
/// (public: it is `~/.agent/auth.json`), and `Unreadable` carries the OS error
/// string, but the token bytes never appear in any of them.
#[derive(Debug, thiserror::Error)]
pub enum AuthTokenError {
    /// The file does not exist. The token is minted by `dreamd service
    /// install` (AILAB-203); the daemon deliberately does not create it.
    #[error("no daemon auth token at {0}: run `dreamd service install` to mint one")]
    Missing(PathBuf),
    /// The file exists but could not be read (permissions, a directory in the
    /// way, an I/O error).
    #[error("could not read the daemon auth token: {0}")]
    Unreadable(String),
    /// The file was read but is not `{"token":"<64 hex>"}`. The message
    /// describes the *shape* problem only — never the bytes that were found.
    #[error("malformed daemon auth token file: {0}")]
    Malformed(String),
}

/// The AILAB-203 auth-file wire format: `{"token":"<64 lowercase hex>"}`.
///
/// Private, and deliberately not `deny_unknown_fields`: a future ticket adding
/// a `rotated_at` sibling field must not brick every already-installed
/// daemon's token file.
#[derive(Deserialize)]
struct AuthFile {
    token: String,
}

/// Read and decode the daemon's bearer token from `path`
/// (`DaemonHome::auth_json()`).
///
/// Called on **every** request by the bearer middleware rather than cached, so
/// a `dreamd service install --force` that rotates the token mid-session
/// invalidates the old credential on the very next request. The file is a few
/// dozen bytes in the daemon's own home, so the read is not worth a cache that
/// would have to be invalidated correctly.
///
/// # Errors
///
/// [`AuthTokenError::Missing`] if `path` does not exist,
/// [`AuthTokenError::Unreadable`] for any other I/O failure, and
/// [`AuthTokenError::Malformed`] if the bytes are not the AILAB-203 JSON object
/// or the `token` field is not exactly 64 hex digits. The `Malformed` message
/// is a fixed description of the expected shape: `serde_json`'s own message can
/// quote the offending value, and quoting a file whose whole job is to hold a
/// secret would defeat the point.
pub fn read_auth_token(path: &Path) -> Result<AuthToken, AuthTokenError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(AuthTokenError::Missing(path.to_path_buf()));
        }
        Err(e) => return Err(AuthTokenError::Unreadable(e.to_string())),
    };
    let parsed: AuthFile = serde_json::from_str(&raw).map_err(|_| {
        AuthTokenError::Malformed("expected a JSON object with a string `token` field".to_string())
    })?;
    let bytes = decode_hex32(&parsed.token).ok_or_else(|| {
        AuthTokenError::Malformed(
            "the `token` field is not exactly 64 hexadecimal characters".to_string(),
        )
    })?;
    Ok(AuthToken(bytes))
}

/// A loopback TCP address the daemon is listening on.
///
/// `host` is an [`IpAddr`], not a `String`, on purpose: it is parsed and
/// checked for [`IpAddr::is_loopback`] at the edge (in
/// [`read_server_json`]), so every value of this type that exists is already
/// known to be loopback and no downstream caller has to re-check before it
/// dials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonEndpoint {
    /// The loopback address the daemon bound (`127.0.0.1`, or `::1`).
    pub host: IpAddr,
    /// The ephemeral port the daemon bound. Never 0 — a 0 in `server.json`
    /// means "the OS picks", which is not an address a client can dial.
    pub port: u16,
}

impl DaemonEndpoint {
    /// The dialable `host:port`.
    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.host, self.port)
    }
}

/// The `server.json` wire format the daemon publishes: `{"host":"127.0.0.1","port":<u16>}`.
///
/// `host` is deserialized as a `String` and parsed separately so that a host
/// which is not an IP literal (a name, an empty string) is a clean refusal
/// rather than a serde error, and so the loopback check is impossible to skip.
#[derive(Deserialize)]
struct ServerJson {
    host: String,
    port: u16,
}

/// Read the daemon's published loopback address from `path`
/// (`DaemonHome::server_json()`).
///
/// The loopback check is a security boundary, not tidiness. `server.json` is a
/// plain file in the user's home that any local process can rewrite, and this
/// function's output is fed straight into a `connect`. Without the check, a
/// forged `{"host":"<somewhere routable>"}` would turn every `dreamd status`
/// and every MCP recall into an outbound connection to an attacker's host,
/// carrying the bearer token with it. So: `host` must parse as an [`IpAddr`]
/// (a name — `localhost` included — is refused, because resolving names here
/// would hand the same decision to whatever the resolver returns) and it must
/// be loopback. AILAB-197's `dreamd watch --insecure` widens only what the
/// daemon *binds*: `watch` still publishes a loopback host here, so there is no
/// configuration that makes a routable host legitimate for this client to dial.
///
/// # Errors
///
/// [`DaemonTransportError::Unreachable`] for every failure — the file is
/// absent, unreadable, not JSON, has a host that is not an IP literal, has a
/// non-loopback host, or has port 0. They collapse into one variant because
/// every caller reacts identically: there is no daemon this client may talk
/// to, which is exactly what `Unreachable` already means to
/// `commands::status`, `commands::archive` and the MCP backend.
pub fn read_server_json(path: &Path) -> Result<DaemonEndpoint, DaemonTransportError> {
    let raw = std::fs::read_to_string(path).map_err(|_| DaemonTransportError::Unreachable)?;
    let parsed: ServerJson =
        serde_json::from_str(&raw).map_err(|_| DaemonTransportError::Unreachable)?;
    let host: IpAddr = parsed
        .host
        .parse()
        .map_err(|_| DaemonTransportError::Unreachable)?;
    if !host.is_loopback() {
        return Err(DaemonTransportError::Unreachable);
    }
    if parsed.port == 0 {
        return Err(DaemonTransportError::Unreachable);
    }
    Ok(DaemonEndpoint {
        host,
        port: parsed.port,
    })
}

/// Resolve the daemon's loopback TCP address from `~/.agent/server.json`.
///
/// The TCP sibling of [`resolve_daemon_socket`], and deliberately a separate
/// function with a separate error type rather than that one widened: there is
/// no `DREAMD_SOCK`-style override here (the port is ephemeral and chosen by
/// the OS at bind time, so the daemon publishes it and clients read it), and
/// the failure set is "no address to dial" rather than "bad address".
///
/// Uses `dirs::home_dir()`, the same resolver `run_watch` uses to decide where
/// to *write* the file, so the writer and the reader cannot disagree about
/// which home directory `~/.agent` means.
///
/// # Errors
///
/// [`DaemonTransportError::Unreachable`] if the home directory cannot be
/// resolved, or for any of the [`read_server_json`] failures.
pub fn resolve_daemon_tcp() -> Result<DaemonEndpoint, DaemonTransportError> {
    let home = dirs::home_dir().ok_or(DaemonTransportError::Unreachable)?;
    read_server_json(&DaemonHome::new(home.join(".agent")).server_json())
}

/// Whether something is accepting TCP connections at `endpoint`.
///
/// **Synchronous on purpose.** `commands::status::daemon_liveness` and
/// `commands::archive::daemon_liveness` are sync fns with no runtime in scope,
/// and the UDS probe they call on Unix (`is_daemon_socket_live`) is sync too.
/// Making this `async` would force a `Runtime::new()` into two CLI paths whose
/// only question is "is a daemon up", so this uses
/// [`std::net::TcpStream::connect_timeout`] and the caller keeps its shape.
///
/// A successful connect is the whole test: it proves a listener is bound and
/// its backlog accepted us. No HTTP request is sent, so this needs no token
/// and cannot be confused by a 401.
pub fn tcp_endpoint_is_live(endpoint: &DaemonEndpoint) -> bool {
    std::net::TcpStream::connect_timeout(&endpoint.socket_addr(), TCP_LIVENESS_TIMEOUT).is_ok()
}

/// Whether a daemon is live at the address published in `server_json`.
///
/// The path is a parameter rather than resolved internally so tests (and a
/// future `--home` style override) can point it at a fixture without a real
/// watch, a real `$HOME`, or an env-var mutex. A stale `server.json` left
/// behind by a killed daemon reads fine and then fails the connect, which is
/// exactly the answer the callers want: not live.
pub fn tcp_daemon_is_live(server_json: &Path) -> bool {
    match read_server_json(server_json) {
        Ok(endpoint) => tcp_endpoint_is_live(&endpoint),
        Err(_) => false,
    }
}

/// Open a fresh TCP connection to `endpoint`, attach the bearer credential,
/// drive one HTTP/1 request to completion, and return `(status, body)`.
///
/// The loopback-TCP sibling of [`send_one`], mirrored stage for stage:
/// per-call connect (no pool — daemon traffic is infrequent, WEG-78-A), the
/// [`hyper_util::rt::TokioIo`] wrap a raw tokio stream needs before hyper will
/// take it, `http1::handshake`, a spawned connection task that runs until the
/// stream closes, `send_request`, then `collect` on the body. Keeping the two
/// identical is what lets a caller map [`DaemonTransportError`] to an exit code
/// without knowing which transport it got.
///
/// The one difference is the `Authorization: Bearer <hex>` header, inserted
/// (not appended) into `req` here rather than by each caller: Windows has no
/// `SO_PEERCRED`, so this header *is* the authentication, and putting it in the
/// transport means no request construction site can forget it. `insert`
/// overwrites any header a caller had already set, so a stale credential from
/// a rebuilt request cannot win.
///
/// # Errors
///
/// [`DaemonTransportError::Unreachable`] when the connect fails with
/// `NotFound` / `ConnectionRefused` (no daemon is listening — the same mapping
/// [`send_one`] uses, so `Unreachable` keeps meaning "run in-process / report
/// not live" regardless of transport). Otherwise the variant naming the stage
/// that failed, carrying that stage's message: `Connect`, `Handshake`, `Send`
/// (also used for the by-construction-impossible case of a credential that is
/// not a legal header value), or `ReadBody`.
pub async fn send_one_tcp(
    endpoint: &DaemonEndpoint,
    token: &AuthToken,
    mut req: hyper::Request<Full<Bytes>>,
) -> Result<(hyper::StatusCode, Bytes), DaemonTransportError> {
    use hyper::client::conn::http1;
    use hyper_util::rt::TokioIo;

    // Built before the connect so a bad credential costs no socket. `as_hex`
    // emits only `[0-9a-f]`, so `from_str` cannot actually fail here; the
    // error arm exists because the alternative is a panic in a library, and
    // the message must not quote the value it rejected.
    let credential = hyper::header::HeaderValue::from_str(&format!("Bearer {}", token.as_hex()))
        .map_err(|_| {
            DaemonTransportError::Send(
                "auth token is not representable as an HTTP header value".to_string(),
            )
        })?;

    let stream = match tokio::net::TcpStream::connect(endpoint.socket_addr()).await {
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
    // TokioIo wrap is required: a raw TcpStream does not implement hyper's IO
    // trait, same as the UDS half (WEG-72-B drift entry).
    let io = TokioIo::new(stream);
    let (mut sender, conn) = http1::handshake(io)
        .await
        .map_err(|e| DaemonTransportError::Handshake(e.to_string()))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });

    req.headers_mut()
        .insert(hyper::header::AUTHORIZATION, credential);

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

// All four tests exercise the UDS half — two resolve `$DREAMD_SOCK` into a
// socket path, two drive `send_one` over a real `UnixListener`. Gating the module
// rather than each test is what keeps `use super::*` from becoming an unused glob
// import off Unix (which rustc reports under `unused_imports`), and it preserves
// exactly today's behaviour: `daemon_client` was `#[cfg(unix)]` in `lib.rs` before
// AILAB-192, so none of these ever compiled off-target. Linux is unix, so the
// Linux count is unchanged.
#[cfg(all(test, unix))]
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

// A SECOND test module, on purpose. `mod tests` above is `#[cfg(all(test, unix))]`
// because every one of its cases needs a `UnixListener` or `$DREAMD_SOCK`, and
// gating the module (rather than each fn) is what keeps its `use super::*` from
// being an unused glob import off Unix. These cases are the opposite: the whole
// point of AILAB-192 keeping the TCP half cfg-free is that it is type-checked
// AND executed on Linux, where the gate that guards main runs — a Windows-only
// test would prove nothing here, and folding these into `mod tests` would gate
// them to unix and lose the coverage on the target that actually ships the TCP
// path. Hence `#[cfg(test)]` with no `unix`, and a distinct name.
#[cfg(test)]
mod tcp_tests {
    use super::*;
    use std::net::Ipv4Addr;

    /// 64 lowercase hex digits — the AILAB-203 wire form of a token whose bytes
    /// are all `0xab`.
    const AB_HEX: &str = "abababababababababababababababababababababababababababababababab";

    /// `127.0.0.1` as an [`IpAddr`], the only host these tests ever dial.
    fn loopback() -> IpAddr {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    }

    /// Bind, read the port, drop: a port the OS just handed out and nothing is
    /// listening on any more. Used for the "not live" / "unreachable" cases,
    /// which must dial a closed port rather than a hardcoded one.
    fn a_closed_loopback_port() -> u16 {
        let probe = std::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .expect("bind probe");
        let port = probe.local_addr().expect("probe local_addr").port();
        drop(probe);
        port
    }

    /// Write `contents` to `<dir>/<name>` and hand back the path.
    fn write_fixture(dir: &std::path::Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).expect("write fixture");
        path
    }

    // -- decode_hex32 ------------------------------------------------------

    #[test]
    fn decode_hex32_accepts_64_lowercase_digits() {
        assert_eq!(decode_hex32(AB_HEX), Some([0xab_u8; 32]));
    }

    #[test]
    fn decode_hex32_accepts_64_uppercase_digits() {
        // The file is lowercase by construction, but a presented credential
        // arrives from an arbitrary HTTP client and case is not a secret.
        assert_eq!(decode_hex32(&AB_HEX.to_uppercase()), Some([0xab_u8; 32]));
    }

    #[test]
    fn decode_hex32_rejects_63_digits() {
        assert_eq!(decode_hex32(&AB_HEX[..63]), None);
    }

    #[test]
    fn decode_hex32_rejects_65_digits() {
        let too_long = format!("{AB_HEX}a");
        assert_eq!(decode_hex32(&too_long), None);
    }

    #[test]
    fn decode_hex32_rejects_a_non_hex_digit() {
        let mut bad = AB_HEX.to_string();
        bad.pop();
        bad.push('z');
        assert_eq!(bad.len(), 64, "still the right length, wrong alphabet");
        assert_eq!(decode_hex32(&bad), None);
    }

    #[test]
    fn decode_hex32_rejects_an_empty_string() {
        assert_eq!(decode_hex32(""), None);
    }

    // -- AuthToken ---------------------------------------------------------

    #[test]
    fn as_hex_round_trips_through_decode_hex32() {
        let token = AuthToken([0xab_u8; 32]);
        assert_eq!(token.as_hex(), AB_HEX);
        assert_eq!(decode_hex32(&token.as_hex()), Some([0xab_u8; 32]));
    }

    #[test]
    fn matches_hex_accepts_the_same_token() {
        let token = AuthToken([0xab_u8; 32]);
        assert!(token.matches_hex(AB_HEX));
    }

    #[test]
    fn matches_hex_rejects_a_differing_final_nibble() {
        // The tightest case the XOR-fold has to get right: 63 of 64 digits
        // match, so a short-circuiting comparison would have done almost all
        // the work before answering.
        let token = AuthToken([0xab_u8; 32]);
        let mut presented = AB_HEX.to_string();
        presented.pop();
        presented.push('c');
        assert_eq!(presented.len(), 64);
        assert!(!token.matches_hex(&presented));
    }

    #[test]
    fn matches_hex_rejects_the_wrong_length() {
        let token = AuthToken([0xab_u8; 32]);
        assert!(!token.matches_hex(&AB_HEX[..63]));
        assert!(!token.matches_hex(""));
        assert!(!token.matches_hex(&format!("{AB_HEX}ab")));
    }

    #[test]
    fn debug_redacts_the_secret() {
        let token = AuthToken([0x7f_u8; 32]);
        let rendered = format!("{token:?}");
        assert_eq!(rendered, "AuthToken(<redacted>)");
        assert!(rendered.contains("redacted"));
        assert!(
            !rendered.contains(&token.as_hex()),
            "Debug must never print the wire form"
        );
        assert!(
            !rendered.contains("7f"),
            "Debug must never print any of the bytes"
        );
    }

    // -- read_auth_token ---------------------------------------------------

    #[test]
    fn read_auth_token_reads_the_ailab_203_wire_format() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_fixture(
            dir.path(),
            "auth.json",
            &format!("{{\"token\":\"{AB_HEX}\"}}\n"),
        );
        let token = read_auth_token(&path).expect("token");
        assert_eq!(token.as_hex(), AB_HEX);
    }

    #[test]
    fn read_auth_token_missing_file_names_service_install() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        let err = read_auth_token(&path).expect_err("absent file");
        assert!(matches!(err, AuthTokenError::Missing(ref p) if *p == path));
        // The watch's exit-1 copy and the operator's next step both hang off
        // this string; AILAB-203 owns minting, the daemon never writes it.
        assert!(
            err.to_string().contains("dreamd service install"),
            "message must point at the install verb, got: {err}"
        );
    }

    #[test]
    fn read_auth_token_rejects_bad_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_fixture(dir.path(), "auth.json", "this is not json\n");
        let err = read_auth_token(&path).expect_err("bad json");
        assert!(matches!(err, AuthTokenError::Malformed(_)), "got {err:?}");
    }

    #[test]
    fn read_auth_token_rejects_a_wrong_length_token() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_fixture(dir.path(), "auth.json", "{\"token\":\"abcd\"}\n");
        let err = read_auth_token(&path).expect_err("short token");
        assert!(matches!(err, AuthTokenError::Malformed(_)), "got {err:?}");
        // Shape only: the message must not echo what was in the file.
        assert!(!err.to_string().contains("abcd"));
    }

    // -- read_server_json --------------------------------------------------

    #[test]
    fn read_server_json_reads_a_loopback_endpoint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_fixture(
            dir.path(),
            "server.json",
            "{\"host\":\"127.0.0.1\",\"port\":54321}\n",
        );
        let endpoint = read_server_json(&path).expect("endpoint");
        assert_eq!(
            endpoint,
            DaemonEndpoint {
                host: loopback(),
                port: 54321,
            }
        );
        assert_eq!(endpoint.socket_addr().to_string(), "127.0.0.1:54321");
    }

    #[test]
    fn read_server_json_missing_file_is_unreachable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = read_server_json(&dir.path().join("server.json")).expect_err("absent");
        assert!(
            matches!(err, DaemonTransportError::Unreachable),
            "got {err:?}"
        );
    }

    #[test]
    fn read_server_json_malformed_json_is_unreachable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_fixture(dir.path(), "server.json", "{\"host\":\n");
        let err = read_server_json(&path).expect_err("malformed");
        assert!(
            matches!(err, DaemonTransportError::Unreachable),
            "got {err:?}"
        );
    }

    #[test]
    fn read_server_json_refuses_a_routable_host() {
        // A forged `server.json` must never turn this client into an outbound
        // connector: a routable host is refused before any connect is attempted.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_fixture(
            dir.path(),
            "server.json",
            "{\"host\":\"10.0.0.5\",\"port\":9999}\n",
        );
        let err = read_server_json(&path).expect_err("routable host");
        assert!(
            matches!(err, DaemonTransportError::Unreachable),
            "got {err:?}"
        );
    }

    #[test]
    fn read_server_json_refuses_the_unspecified_host() {
        // The wildcard bind address, spelled via `Ipv4Addr::UNSPECIFIED` rather
        // than as a literal: spec §5 greps added lines for that literal, since
        // this ticket must not bind or dial anything but loopback.
        let dir = tempfile::tempdir().expect("tempdir");
        let wildcard = Ipv4Addr::UNSPECIFIED;
        let path = write_fixture(
            dir.path(),
            "server.json",
            &format!("{{\"host\":\"{wildcard}\",\"port\":9999}}\n"),
        );
        let err = read_server_json(&path).expect_err("wildcard host");
        assert!(
            matches!(err, DaemonTransportError::Unreachable),
            "got {err:?}"
        );
    }

    #[test]
    fn read_server_json_refuses_a_host_name() {
        // Even `localhost`: resolving a name here would hand the loopback
        // decision to whatever the resolver returns, so only IP literals pass.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_fixture(
            dir.path(),
            "server.json",
            "{\"host\":\"localhost\",\"port\":9999}\n",
        );
        let err = read_server_json(&path).expect_err("host name");
        assert!(
            matches!(err, DaemonTransportError::Unreachable),
            "got {err:?}"
        );
    }

    #[test]
    fn read_server_json_refuses_port_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_fixture(
            dir.path(),
            "server.json",
            "{\"host\":\"127.0.0.1\",\"port\":0}\n",
        );
        let err = read_server_json(&path).expect_err("port zero");
        assert!(
            matches!(err, DaemonTransportError::Unreachable),
            "got {err:?}"
        );
    }

    // -- liveness probes ---------------------------------------------------

    #[test]
    fn tcp_endpoint_is_live_is_false_on_a_closed_port() {
        let endpoint = DaemonEndpoint {
            host: loopback(),
            port: a_closed_loopback_port(),
        };
        assert!(!tcp_endpoint_is_live(&endpoint));
    }

    #[test]
    fn tcp_endpoint_is_live_is_true_against_a_real_listener() {
        // A bound listener answers from its backlog without an `accept()`, so
        // the probe needs no server task — which is the point: it is a connect,
        // not a request.
        let listener =
            std::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        let endpoint = DaemonEndpoint {
            host: loopback(),
            port,
        };
        assert!(tcp_endpoint_is_live(&endpoint));
    }

    #[test]
    fn tcp_daemon_is_live_follows_a_published_address() {
        let listener =
            std::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_fixture(
            dir.path(),
            "server.json",
            &format!("{{\"host\":\"127.0.0.1\",\"port\":{port}}}\n"),
        );
        assert!(tcp_daemon_is_live(&path));
    }

    #[test]
    fn tcp_daemon_is_live_is_false_without_a_server_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!tcp_daemon_is_live(&dir.path().join("server.json")));
    }

    #[test]
    fn tcp_daemon_is_live_is_false_for_a_stale_server_json() {
        // A daemon killed without unlinking leaves a parseable file behind; the
        // connect is what decides, so this is "not live", not an error.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_fixture(
            dir.path(),
            "server.json",
            &format!(
                "{{\"host\":\"127.0.0.1\",\"port\":{}}}\n",
                a_closed_loopback_port()
            ),
        );
        assert!(!tcp_daemon_is_live(&path));
    }

    // -- send_one_tcp ------------------------------------------------------

    #[tokio::test]
    async fn send_one_tcp_closed_port_is_unreachable() {
        let endpoint = DaemonEndpoint {
            host: loopback(),
            port: a_closed_loopback_port(),
        };
        let token = AuthToken([0xab_u8; 32]);
        let req = hyper::Request::builder()
            .method(hyper::Method::GET)
            .uri("/")
            .body(Full::new(Bytes::new()))
            .expect("request");
        let err = send_one_tcp(&endpoint, &token, req)
            .await
            .expect_err("closed port");
        assert!(
            matches!(err, DaemonTransportError::Unreachable),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn send_one_tcp_round_trips_and_sends_the_bearer_credential() {
        use hyper::service::service_fn;
        use hyper_util::rt::{TokioExecutor, TokioIo};

        let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        assert!(
            addr.ip().is_loopback(),
            "the test server itself must be loopback-only"
        );

        // (method, uri, Authorization) as the server saw them.
        let (req_tx, mut req_rx) =
            tokio::sync::mpsc::unbounded_channel::<(String, String, Option<String>)>();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let req_tx = req_tx.clone();
                async move {
                    let method = req.method().to_string();
                    let uri = req.uri().to_string();
                    let auth = req
                        .headers()
                        .get(hyper::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    let _ = req_tx.send((method, uri, auth));
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

        let token = AuthToken([0x5a_u8; 32]);
        let endpoint = DaemonEndpoint {
            host: loopback(),
            port: addr.port(),
        };
        let req = hyper::Request::builder()
            .method(hyper::Method::POST)
            .uri("/api/v1/learn")
            .header(hyper::header::HOST, "localhost")
            .body(Full::new(Bytes::new()))
            .expect("request");
        let (status, body) = send_one_tcp(&endpoint, &token, req)
            .await
            .expect("round trip");
        assert_eq!(status, hyper::StatusCode::OK);
        assert_eq!(body.as_ref(), b"{\"ok\":true}");

        let (method, uri, auth) = req_rx.recv().await.expect("request captured");
        assert_eq!(method, "POST");
        assert_eq!(uri, "/api/v1/learn");
        assert_eq!(
            auth.as_deref(),
            Some(format!("Bearer {}", token.as_hex()).as_str()),
            "the transport must attach the bearer credential itself"
        );
    }

    #[tokio::test]
    async fn send_one_tcp_overwrites_a_caller_supplied_authorization() {
        use hyper::service::service_fn;
        use hyper_util::rt::{TokioExecutor, TokioIo};

        let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let (req_tx, mut req_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<String>>();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let req_tx = req_tx.clone();
                async move {
                    let seen = req
                        .headers()
                        .get_all(hyper::header::AUTHORIZATION)
                        .iter()
                        .filter_map(|v| v.to_str().ok())
                        .map(str::to_string)
                        .collect::<Vec<_>>();
                    let _ = req_tx.send(seen);
                    Ok::<_, std::convert::Infallible>(
                        hyper::Response::builder()
                            .status(200)
                            .body(Full::new(Bytes::new()))
                            .unwrap(),
                    )
                }
            });
            let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await;
        });

        let token = AuthToken([0x5a_u8; 32]);
        let endpoint = DaemonEndpoint {
            host: loopback(),
            port: addr.port(),
        };
        let req = hyper::Request::builder()
            .method(hyper::Method::GET)
            .uri("/")
            .header(hyper::header::HOST, "localhost")
            .header(hyper::header::AUTHORIZATION, "Bearer stale")
            .body(Full::new(Bytes::new()))
            .expect("request");
        send_one_tcp(&endpoint, &token, req).await.expect("sent");

        let seen = req_rx.recv().await.expect("request captured");
        assert_eq!(
            seen,
            vec![format!("Bearer {}", token.as_hex())],
            "insert must replace a stale credential, not append beside it"
        );
    }
}
