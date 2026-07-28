//! DR-507 / AILAB-221: dreamd MCP and Anthropic's reference memory server
//! coexist without colliding on sockets or corrupting each other's stores.
//!
//! Spawns both over stdio, completes the MCP handshake on each, asserts the
//! intentional `search_nodes` name collision is reachable on separate channels,
//! and verifies each write lands only in its own store.
//!
//! Skips cleanly when `npx` is absent (CI cargo job has no node). Set
//! `DREAMD_COEXIST_REQUIRE=1` to turn absence into a hard failure.

#![cfg(unix)]

use serde_json::Value;
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

fn dreamd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dreamd")
}

/// Wait up to `secs` for `child` to exit; kill + panic on timeout.
/// Copied from `mcp_stdin_eof.rs` (test files don't share helpers).
fn wait_or_kill(child: &mut Child, secs: u64) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("MCP child did not exit within {secs}s of stdin EOF (orphan risk)");
        }
        std::thread::sleep(Duration::from_millis(50)); // poll process exit
    }
}

fn npx_available() -> bool {
    Command::new("npx")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Stdio MCP child: line-oriented JSON-RPC over piped stdin/stdout.
/// Reader thread + mpsc avoids blocking forever on a pipe read.
struct McpChild {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<String>,
}

impl McpChild {
    fn from_command(mut cmd: Command) -> Self {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn MCP child");
        let stdout = child.stdout.take().expect("child stdout");
        let stdin = child.stdin.take().expect("child stdin");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) if !l.is_empty() => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
        Self {
            child,
            stdin: Some(stdin),
            rx,
        }
    }

    fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin still open");
        writeln!(stdin, "{line}").expect("write MCP line");
        stdin.flush().expect("flush MCP stdin");
    }

    /// Receive the next JSON-RPC response with matching `id`, skipping notifications.
    fn recv_response(&self, id: u64, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                panic!("timeout waiting for MCP response id={id}");
            }
            let line = self
                .rx
                .recv_timeout(remaining)
                .unwrap_or_else(|e| panic!("recv_timeout waiting for id={id}: {e}"));
            let v: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("invalid JSON from MCP child: {e}; line={line}"));
            if v.get("id").and_then(|i| i.as_u64()) == Some(id) {
                return v;
            }
        }
    }

    fn close_stdin(&mut self) {
        drop(self.stdin.take());
    }
}

fn handshake(mcp: &mut McpChild, init_id: u64, timeout: Duration) {
    let init = format!(
        r#"{{"jsonrpc":"2.0","id":{init_id},"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"coexistence","version":"0"}}}}}}"#
    );
    let initialized = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    mcp.send(&init);
    let resp = mcp.recv_response(init_id, timeout);
    assert!(
        resp.get("result").is_some(),
        "initialize must succeed; got {resp}"
    );
    assert!(
        resp.get("error").is_none(),
        "initialize must not error; got {resp}"
    );
    mcp.send(initialized);
}

fn assert_rpc_ok(resp: &Value, ctx: &str) {
    assert!(
        resp.get("error").is_none(),
        "{ctx}: unexpected error: {resp}"
    );
    assert!(
        resp.get("result").is_some(),
        "{ctx}: missing result: {resp}"
    );
}

fn tool_names(tools_list_resp: &Value) -> BTreeSet<String> {
    tools_list_resp["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("tool name").to_string())
        .collect()
}

fn collect_relative_paths(dir: &Path, base: &Path, out: &mut Vec<PathBuf>) {
    if !dir.exists() {
        return;
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .map(|e| e.expect("dir entry").path())
        .collect();
    entries.sort();
    for path in entries {
        let rel = path
            .strip_prefix(base)
            .expect("path under base")
            .to_path_buf();
        out.push(rel);
        if path.is_dir() {
            collect_relative_paths(&path, base, out);
        }
    }
}

fn snapshot_listing(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_relative_paths(root, root, &mut out);
    out
}

/// Listing used for ref-isolation asserts. Excludes `.dreamd/index/**` because
/// Tantivy commits asynchronously after append (~5s cadence) — those mutations
/// are dreamd's own writer, not evidence of ref-server interference. Do not
/// open or touch tantivy lock files.
fn isolation_listing(root: &Path) -> Vec<PathBuf> {
    snapshot_listing(root)
        .into_iter()
        .filter(|rel| {
            let s = rel.to_string_lossy();
            s != ".dreamd/index" && !s.starts_with(".dreamd/index/")
        })
        .collect()
}

#[test]
fn dreamd_and_ref_memory_server_coexist() {
    if !npx_available() {
        if std::env::var_os("DREAMD_COEXIST_REQUIRE").as_deref() == Some(std::ffi::OsStr::new("1"))
        {
            panic!("DREAMD_COEXIST_REQUIRE=1 but npx is not on PATH");
        }
        eprintln!(
            "SKIPPED: npx not on PATH — coexistence not exercised (CI cargo job has no node)"
        );
        return;
    }

    let sandbox_home = tempfile::tempdir().expect("sandbox HOME");
    let project = tempfile::tempdir().expect("project dir");
    let ref_store_dir = tempfile::tempdir().expect("ref store dir");
    let memory_file = ref_store_dir.path().join("memory.json");

    // dreamd init refuses a bare tempdir — stub Cargo.toml sentinel first.
    std::fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname = \"coexist-fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .expect("write stub Cargo.toml");

    let init = Command::new(dreamd_bin())
        .arg("init")
        .current_dir(project.path())
        .env("HOME", sandbox_home.path())
        .output()
        .expect("run dreamd init");
    assert!(
        init.status.success(),
        "dreamd init failed: status={:?}\nstderr={}\nstdout={}",
        init.status.code(),
        String::from_utf8_lossy(&init.stderr),
        String::from_utf8_lossy(&init.stdout)
    );

    // dreamd: sandbox HOME (no real daemon socket → in-process MCP).
    let mut dreamd = McpChild::from_command({
        let mut c = Command::new(dreamd_bin());
        c.arg("mcp")
            .current_dir(project.path())
            .env("HOME", sandbox_home.path());
        c
    });

    // Ref server: real HOME (npx cache); MEMORY_FILE_PATH in its own tempdir.
    let mut ref_srv = McpChild::from_command({
        let mut c = Command::new("npx");
        c.args(["-y", "@modelcontextprotocol/server-memory"])
            .env("MEMORY_FILE_PATH", &memory_file);
        c
    });

    handshake(&mut dreamd, 1, Duration::from_secs(20));
    handshake(&mut ref_srv, 1, Duration::from_secs(120));

    // tools/list both
    dreamd.send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#);
    let dreamd_list = dreamd.recv_response(2, Duration::from_secs(20));
    assert_rpc_ok(&dreamd_list, "dreamd tools/list");
    let dreamd_names = tool_names(&dreamd_list);
    let expected_dreamd: BTreeSet<String> = ["search_nodes", "append_node"]
        .into_iter()
        .map(str::to_string)
        .collect();
    assert_eq!(
        dreamd_names, expected_dreamd,
        "dreamd tool surface must be exactly {{search_nodes, append_node}}"
    );

    ref_srv.send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#);
    let ref_list = ref_srv.recv_response(2, Duration::from_secs(20));
    assert_rpc_ok(&ref_list, "ref tools/list");
    let ref_names = tool_names(&ref_list);
    assert!(
        ref_names.contains("search_nodes"),
        "ref tools/list must include search_nodes (intentional collision); got {ref_names:?}"
    );

    // dreamd write — durability on JSONL, not recall
    dreamd.send(
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"append_node","arguments":{"content":"coexistence probe learning","source_harness":"coexistence-test","skill_action":"testing::mcp_coexistence"}}}"#,
    );
    let append_resp = dreamd.recv_response(3, Duration::from_secs(20));
    assert_rpc_ok(&append_resp, "dreamd append_node");
    let is_error = append_resp["result"]["isError"].as_bool().unwrap_or(false);
    assert!(!is_error, "append_node isError: {append_resp}");

    let jsonl = project.path().join(".agent/episodic/AGENT_LEARNINGS.jsonl");
    let jsonl_body = std::fs::read_to_string(&jsonl).expect("read AGENT_LEARNINGS.jsonl");
    assert!(
        jsonl_body.contains("coexistence probe learning"),
        "JSONL must contain probe learning; got:\n{jsonl_body}"
    );

    // dreamd read — well-formed only; do NOT assert hits (index commit lags)
    dreamd.send(
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"search_nodes","arguments":{"query":"coexistence","k":3}}}"#,
    );
    let search_resp = dreamd.recv_response(4, Duration::from_secs(20));
    assert_rpc_ok(&search_resp, "dreamd search_nodes");
    let search_err = search_resp["result"]["isError"].as_bool().unwrap_or(false);
    assert!(!search_err, "search_nodes isError: {search_resp}");

    // Isolation: snapshot dreamd trees, then write via ref only
    let project_agent = project.path().join(".agent");
    let home_agent = sandbox_home.path().join(".agent");
    let before_project = isolation_listing(&project_agent);
    let before_home = isolation_listing(&home_agent);

    ref_srv.send(
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"create_entities","arguments":{"entities":[{"name":"CoexistProbe","entityType":"test","observations":["probe"]}]}}}"#,
    );
    let create_resp = ref_srv.recv_response(3, Duration::from_secs(20));
    assert_rpc_ok(&create_resp, "ref create_entities");
    let create_err = create_resp["result"]["isError"].as_bool().unwrap_or(false);
    assert!(!create_err, "create_entities isError: {create_resp}");

    assert!(
        memory_file.is_file(),
        "ref MEMORY_FILE_PATH must exist at {}",
        memory_file.display()
    );
    let mem_body = std::fs::read_to_string(&memory_file).expect("read ref memory file");
    assert!(
        mem_body.contains("CoexistProbe"),
        "ref store must contain CoexistProbe; got:\n{mem_body}"
    );

    let after_project = isolation_listing(&project_agent);
    let after_home = isolation_listing(&home_agent);
    assert_eq!(
        before_project, after_project,
        "ref ops must not touch project .agent/ listing"
    );
    assert_eq!(
        before_home, after_home,
        "ref ops must not touch sandbox HOME/.agent/ listing"
    );

    // Shutdown: stdin EOF
    dreamd.close_stdin();
    ref_srv.close_stdin();
    let dreamd_status = wait_or_kill(&mut dreamd.child, 20);
    assert!(
        dreamd_status.success(),
        "dreamd mcp must exit 0 on stdin EOF; got {dreamd_status:?}"
    );
    let _ = wait_or_kill(&mut ref_srv.child, 20);
}
