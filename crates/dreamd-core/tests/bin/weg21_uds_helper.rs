//! Test-only helper binary used by the WEG-21 two-process integration test.
//!
//! NOT a shipping binary: `Cargo.toml` flags it `test = false, bench = false,
//! doc = false`, and it lives under `tests/bin/`. Compiled only when the
//! dreamd-core test suite is built.
//!
//! Modes (selected by argv[1]):
//!
//!   * `writer <socket-path> <agent-root>` — try to bind the socket; on
//!     success, accept one client connection, read one length-prefixed JSON
//!     AgentLearning, forward it through the in-process MemoryCoordinator,
//!     then exit `0`. Prints the minted EventId on stdout.
//!
//!   * `client <socket-path> <agent-root>` — connect to the socket and send
//!     one length-prefixed JSON AgentLearning. Prints the writer's stdout
//!     reply (the minted EventId) on its own stdout. Exits `0` on success.
//!
//! No double-fork in this helper — the integration test wants the parent
//! process visible so it can wait on the child PID. Detachment is exercised
//! separately by the production `server::run()` path.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use dreamd_core::coordinator::MemoryCoordinatorMsg;
use dreamd_core::layout::AgentRoot;
use dreamd_core::server::{bind_writer_socket, Supervisor};
use dreamd_protocol::{AgentLearning, EventId};
use tokio::sync::oneshot;

fn read_frame(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok(payload)
}

fn write_frame(stream: &mut UnixStream, bytes: &[u8]) -> std::io::Result<()> {
    let len = (bytes.len() as u32).to_be_bytes();
    stream.write_all(&len)?;
    stream.write_all(bytes)?;
    stream.flush()
}

async fn run_writer(socket_path: PathBuf, agent_root: AgentRoot) -> ExitCode {
    // Make sure the per-project directories exist; the coordinator's open
    // does this for the episodic dir but the integration test asserts on
    // disk paths, so be explicit.
    if let Err(e) = std::fs::create_dir_all(agent_root.episodic_dir()) {
        eprintln!("writer: create_dir_all failed: {e}");
        return ExitCode::from(1);
    }

    let guard = match bind_writer_socket(&socket_path) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("writer: bind failed: {e}");
            return ExitCode::from(2);
        }
    };
    // Signal readiness to the integration-test parent.
    println!("BIND_OK");
    let _ = std::io::stdout().flush();

    let supervisor = match Supervisor::start(&agent_root, 8, None) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("writer: supervisor start failed: {e}");
            return ExitCode::from(3);
        }
    };

    // Accept one connection (the test only spawns one client).
    let listener: &UnixListener = guard.listener();
    listener
        .set_nonblocking(false)
        .expect("set_nonblocking false");
    let (mut stream, _) = match listener.accept() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("writer: accept failed: {e}");
            return ExitCode::from(4);
        }
    };
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();

    let payload = match read_frame(&mut stream) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("writer: read_frame failed: {e}");
            return ExitCode::from(5);
        }
    };
    let learning: AgentLearning = match serde_json::from_slice(&payload) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("writer: parse failed: {e}");
            return ExitCode::from(6);
        }
    };

    let client_tx = supervisor.sender();
    let (resp_tx, resp_rx) = oneshot::channel();
    if let Err(e) = client_tx
        .send(MemoryCoordinatorMsg::AppendLearning {
            learning,
            client_dedup_key: None,
            response_tx: resp_tx,
        })
        .await
    {
        eprintln!("writer: forward failed: {e}");
        return ExitCode::from(7);
    }
    drop(client_tx); // satisfy shutdown-drain invariant

    let minted: EventId = match resp_rx.await {
        Ok(Ok(outcome)) => outcome.id,
        Ok(Err(e)) => {
            eprintln!("writer: coordinator error: {e}");
            return ExitCode::from(8);
        }
        Err(e) => {
            eprintln!("writer: oneshot recv error: {e}");
            return ExitCode::from(9);
        }
    };

    if let Err(e) = write_frame(&mut stream, minted.as_str().as_bytes()) {
        eprintln!("writer: reply failed: {e}");
        return ExitCode::from(10);
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);

    supervisor.shutdown().await;
    println!("MINTED {minted}");
    ExitCode::SUCCESS
}

fn run_client(socket_path: PathBuf) -> ExitCode {
    let mut stream = match UnixStream::connect(&socket_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("client: connect failed: {e}");
            return ExitCode::from(1);
        }
    };
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();

    // Send a minimal AgentLearning. Use a placeholder id; the writer's
    // coordinator overwrites it.
    let placeholder = EventId::parse("evt_01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
    let learning = AgentLearning {
        schema_version: "1.0.0".to_string(),
        id: placeholder,
        timestamp: chrono::Utc::now(),
        pain: 4.0,
        importance: 4.0,
        pinned: false,
        skill_action: "weg21.integration".to_string(),
        source_harness: "weg21-uds-helper".to_string(),
        content: "weg21 client payload".to_string(),
    };
    let body = match serde_json::to_vec(&learning) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("client: serialize failed: {e}");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = write_frame(&mut stream, &body) {
        eprintln!("client: write_frame failed: {e}");
        return ExitCode::from(3);
    }
    let reply = match read_frame(&mut stream) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("client: read_frame failed: {e}");
            return ExitCode::from(4);
        }
    };
    let minted = String::from_utf8_lossy(&reply).to_string();
    println!("CLIENT_MINTED {minted}");
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: weg21_uds_helper <writer|client> <socket-path> [<agent-root>]");
        return ExitCode::from(64);
    }
    let mode = args[1].as_str();
    let socket_path = PathBuf::from(&args[2]);
    match mode {
        "writer" => {
            if args.len() < 4 {
                eprintln!("writer mode requires <agent-root>");
                return ExitCode::from(64);
            }
            let agent_root = AgentRoot::new(PathBuf::from(&args[3]));
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("writer: tokio runtime: {e}");
                    return ExitCode::from(1);
                }
            };
            rt.block_on(run_writer(socket_path, agent_root))
        }
        "client" => run_client(socket_path),
        other => {
            eprintln!("unknown mode: {other}");
            ExitCode::from(64)
        }
    }
}
