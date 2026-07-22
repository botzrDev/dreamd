//! Regression tests: `dreamd mcp` shuts down cleanly and promptly when the MCP
//! client disconnects (stdin EOF) — it must not hang or orphan.
//!
//! `dreamd mcp` serves over `rmcp::transport::stdio()`. The stdio-transport
//! shutdown convention is that the client closes the server's stdin; the
//! transport loop then ends and the process exits. These tests lock in two
//! properties:
//!   1. A normal lifecycle (initialize → initialized → EOF) exits 0 cleanly.
//!   2. Any disconnect (even before initialize) TERMINATES promptly — no orphan.
//!
//! If a future rmcp/transport change broke exit-on-EOF, the npx shim's blocking
//! `execFileSync` would never return and would leave an orphaned server process.

use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

fn dreamd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dreamd")
}

fn spawn_mcp(cwd: &Path, home: &Path) -> Child {
    // Bare dir (no `.agent/`) → in-process "empty" server branch. HOME points at a
    // throwaway dir so no real daemon socket is reachable (stays in-process). stdout
    // is discarded so the child never blocks on an unread response pipe.
    Command::new(dreamd_bin())
        .arg("mcp")
        .current_dir(cwd)
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dreamd mcp")
}

/// Wait up to `secs` for `child` to exit; kill + panic on timeout (the orphan bug).
fn wait_or_kill(child: &mut Child, secs: u64) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait on dreamd mcp") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("dreamd mcp did not exit within {secs}s of stdin EOF (orphan risk)");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn mcp_normal_lifecycle_exits_zero_on_eof() {
    // A full, well-behaved session: initialize, notifications/initialized, then the
    // client closes stdin. This is the shutdown IDEs actually perform — it must exit
    // 0 cleanly (no "MCP service error"), or the npx shim reports a false crash.
    let tmp = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let mut child = spawn_mcp(tmp.path(), home.path());

    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"regression","version":"0"}}}"#;
    let initialized = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;

    {
        let mut stdin = child.stdin.take().expect("child stdin");
        writeln!(stdin, "{init}").unwrap();
        writeln!(stdin, "{initialized}").unwrap();
        stdin.flush().unwrap();
        // Let rmcp process the handshake before EOF, then stdin drops → EOF.
        std::thread::sleep(Duration::from_millis(300));
    }

    let status = wait_or_kill(&mut child, 20);
    assert!(
        status.success(),
        "normal MCP disconnect must exit 0; got {status:?}"
    );
}

#[test]
fn mcp_pre_init_disconnect_terminates_promptly() {
    // Abnormal: the client connects and closes stdin before any initialize
    // handshake. rmcp reports "connection closed: initialize request" and the exit
    // code is allowed to be non-zero — but the process MUST still terminate
    // promptly. That termination is the property that prevents an orphaned server.
    let tmp = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let mut child = spawn_mcp(tmp.path(), home.path());

    drop(child.stdin.take()); // immediate EOF, no handshake

    // Panics if it doesn't exit within the deadline — that would be the orphan bug.
    let _ = wait_or_kill(&mut child, 20);
}
