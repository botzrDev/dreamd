//! WEG-21 integration test: two processes coordinate via UDS.
//!
//! Spawns the `weg21_uds_helper` test bin twice — first as `writer`, then as
//! `client` — and asserts:
//!   * the writer prints `BIND_OK` (i.e. it successfully bound the socket),
//!   * the client connects and exchanges a length-prefixed JSON
//!     `AgentLearning`,
//!   * the writer's coordinator returns a freshly minted `EventId`,
//!   * the writer's `episodic/AGENT_LEARNINGS.jsonl` ends up with exactly
//!     one parseable record.
//!
//! Subprocess wiring is the only way to satisfy the AC's "two processes"
//! requirement — same-process threads can't exercise the bind/connect path
//! through the OS, and Tantivy's eventual single-IndexWriter constraint is
//! about real process boundaries.

#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use dreamd_protocol::AgentLearning;

fn helper_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_weg21_uds_helper"))
}

fn unique_tmp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("dreamd-weg21-{tag}-{pid}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn first_process_binds_socket_and_second_forwards_through_it() {
    let scratch = unique_tmp_dir("twoproc");
    let project_root = scratch.join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    // macOS caps UDS sun_path at 104 bytes; its $TMPDIR is the long
    // /private/var/folders/<…>/T/ prefix, so nesting our unique dir +
    // agent-home/dreamd.sock under it overflows the limit (Linux is 108 +
    // $TMPDIR=/tmp, so this only bites macOS CI). Bind the socket under a
    // short, dedicated tempdir; the project tree has no path-length limit and
    // stays under `scratch`. `sock_dir` must outlive the test.
    let sock_dir = tempfile::tempdir().expect("socket tempdir");
    let socket = sock_dir.path().join("dreamd.sock");

    // Spawn writer; pipe stdout so we can read its BIND_OK readiness signal.
    let mut writer = Command::new(helper_bin())
        .arg("writer")
        .arg(&socket)
        .arg(&project_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn writer");

    // Wait up to 3s for the writer to print BIND_OK. We tail one line off
    // its stdout to keep the lifecycle ordering observable.
    let stdout = writer.stdout.take().expect("writer stdout");
    let mut reader = BufReader::new(stdout);
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut line = String::new();
    let mut bound = false;
    while Instant::now() < deadline {
        line.clear();
        if reader.read_line(&mut line).expect("read writer stdout") == 0 {
            break;
        }
        if line.trim() == "BIND_OK" {
            bound = true;
            break;
        }
    }
    if !bound {
        // Surface the writer's stderr so a bind failure (e.g. a macOS
        // sun_path overflow) isn't swallowed behind an empty stdout.
        let _ = writer.kill();
        let mut err = String::new();
        if let Some(mut es) = writer.stderr.take() {
            use std::io::Read;
            let _ = es.read_to_string(&mut err);
        }
        panic!("writer never signaled BIND_OK; stdout={line:?}; stderr={err}");
    }
    assert!(socket.exists(), "writer must create the socket file");

    // Run client to completion.
    let client_out = Command::new(helper_bin())
        .arg("client")
        .arg(&socket)
        .output()
        .expect("spawn client");
    assert!(
        client_out.status.success(),
        "client exit {:?}: stderr={}",
        client_out.status,
        String::from_utf8_lossy(&client_out.stderr)
    );
    let client_stdout = String::from_utf8_lossy(&client_out.stdout);
    let client_minted = client_stdout
        .lines()
        .find_map(|l| l.strip_prefix("CLIENT_MINTED "))
        .expect("client must print CLIENT_MINTED <id>")
        .trim()
        .to_string();
    assert!(
        client_minted.starts_with("evt_") && client_minted.len() == "evt_".len() + 26,
        "client reply must be a freshly minted EventId, got {client_minted:?}"
    );

    // Drain remaining writer stdout through the BufReader we already own —
    // `wait_with_output()` can't be used here because we took stdout off the
    // child earlier to detect BIND_OK.
    let mut writer_remaining = String::new();
    let drain_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < drain_deadline {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => writer_remaining.push_str(&line),
            Err(_) => break,
        }
    }
    let writer_status = writer.wait().expect("wait writer exit");
    assert!(writer_status.success(), "writer exit {:?}", writer_status);
    let writer_minted = writer_remaining
        .lines()
        .find_map(|l| l.strip_prefix("MINTED "))
        .expect("writer must print MINTED <id>")
        .trim()
        .to_string();
    assert_eq!(
        writer_minted, client_minted,
        "writer's minted id must match what the client received"
    );

    // Socket file unlinked by writer's Drop guard.
    assert!(
        !socket.exists(),
        "writer Drop must unlink the socket file (got: still present)"
    );

    // JSONL contains exactly one parseable AgentLearning whose id is the
    // minted EventId, proving the client payload reached the writer's
    // coordinator and landed on disk.
    let jsonl = project_root
        .join(".agent")
        .join("episodic")
        .join("AGENT_LEARNINGS.jsonl");
    let raw = std::fs::read_to_string(&jsonl).expect("read jsonl");
    let lines: Vec<&str> = raw.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1, "exactly one append must have landed");
    let decoded: AgentLearning =
        serde_json::from_str(lines[0]).expect("jsonl line parses as AgentLearning");
    assert_eq!(decoded.id.as_str(), client_minted);
    assert_eq!(decoded.source_harness, "weg21-uds-helper");

    let _ = std::fs::remove_dir_all(&scratch);
}
