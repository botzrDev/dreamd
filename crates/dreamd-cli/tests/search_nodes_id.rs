//! BZR-1468: a `search_nodes` hit carries the id `append_node` minted, and
//! that id is the one `dreamd blame --json` shows and `dreamd forget` takes.
//! A lesson's `lsn_` form is still refused by `dreamd forget`.
//!
//! Subprocess test over stdio MCP, in the shape of `mcp_coexistence.rs`
//! (test files don't share helpers, so `McpChild` / `handshake` are copied).
//! Every child gets `HOME` set to a tempdir and `DREAMD_LOG=off`, and has
//! `DREAMD_SOCK` removed, so `dreamd mcp` runs in process. The test process's
//! own environment is never changed. Unix-only: the in-process MCP server
//! takes the project writer lock.

#![cfg(unix)]

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const LOG: &str = ".agent/episodic/AGENT_LEARNINGS.jsonl";
const WORD: &str = "bzr1468glorptwhistle";
const CONTENT: &str = "bzr1468glorptwhistle the hit id round trip";
const TIMEOUT: Duration = Duration::from_secs(30);

fn dreamd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dreamd")
}

/// A `dreamd` command in `project`, with the test's environment rules.
fn dreamd(project: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(dreamd_bin());
    cmd.current_dir(project)
        .env("HOME", home)
        .env("DREAMD_LOG", "off")
        .env_remove("DREAMD_SOCK");
    cmd
}

fn run(project: &Path, home: &Path, args: &[&str]) -> Output {
    dreamd(project, home)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run dreamd {args:?}: {e}"))
}

fn describe(out: &Output) -> String {
    format!(
        "exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Wait up to `secs` for `child` to exit; kill + panic on timeout.
/// Copied from `mcp_coexistence.rs`.
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

/// Stdio MCP child: line-oriented JSON-RPC over piped stdin/stdout.
/// Reader thread + mpsc avoids blocking forever on a pipe read.
/// Copied from `mcp_coexistence.rs`.
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

    /// Close stdin and wait for the server to exit, so it releases the
    /// project writer lock.
    fn close(mut self) {
        self.close_stdin();
        let status = wait_or_kill(&mut self.child, 30);
        assert!(status.success(), "dreamd mcp exit: {status:?}");
    }
}

/// Copied from `mcp_coexistence.rs`.
fn handshake(mcp: &mut McpChild, init_id: u64, timeout: Duration) {
    let init = format!(
        r#"{{"jsonrpc":"2.0","id":{init_id},"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"search-nodes-id","version":"0"}}}}}}"#
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

/// Call `tool` with `arguments` and return the JSON in `result.content[0].text`.
fn call_tool(mcp: &mut McpChild, id: u64, tool: &str, arguments: Value) -> Value {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    });
    mcp.send(&req.to_string());
    let resp = mcp.recv_response(id, TIMEOUT);
    assert!(resp.get("error").is_none(), "{tool}: error: {resp}");
    assert_ne!(
        resp["result"]["isError"],
        Value::Bool(true),
        "{tool}: tool error: {resp}"
    );
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{tool}: no text content: {resp}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{tool}: text is not JSON: {e}; {text}"))
}

fn spawn_mcp(project: &Path, home: &Path) -> McpChild {
    let mut cmd = dreamd(project, home);
    cmd.arg("mcp");
    let mut mcp = McpChild::from_command(cmd);
    handshake(&mut mcp, 1, TIMEOUT);
    mcp
}

fn search(mcp: &mut McpChild, id: u64) -> Vec<Value> {
    let body = call_tool(
        mcp,
        id,
        "search_nodes",
        serde_json::json!({ "query": WORD, "k": 5 }),
    );
    body["results"]
        .as_array()
        .unwrap_or_else(|| panic!("search_nodes: no results array: {body}"))
        .clone()
}

/// Test 9.
#[test]
fn search_nodes_id_works_with_blame_and_forget() {
    let project = tempfile::tempdir().expect("project tempdir");
    let home = tempfile::tempdir().expect("home tempdir");
    let (project, home) = (project.path(), home.path());

    // dreamd init refuses a bare tempdir — stub Cargo.toml sentinel first.
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"bzr1468\"\nversion = \"0.0.0\"\n",
    )
    .expect("write stub Cargo.toml");
    let init = run(project, home, &["init"]);
    assert!(init.status.success(), "dreamd init: {}", describe(&init));

    // Search: the hit for the append carries the minted id.
    let mut mcp = spawn_mcp(project, home);
    let appended = call_tool(
        &mut mcp,
        2,
        "append_node",
        serde_json::json!({
            "content": CONTENT,
            "source_harness": "claude-code",
            "skill_action": "testing::search_nodes_id",
        }),
    );
    let id = appended["id"]
        .as_str()
        .unwrap_or_else(|| panic!("append_node: no id: {appended}"))
        .to_owned();
    // The in-process search waits at most 1 s for the index to commit the
    // append (BZR-1437), so under load one try can answer from the last
    // commit. This test checks the id, not that bound: retry the first search
    // in the same session, at most 10 tries, 200 ms apart.
    let mut hits = Vec::new();
    for attempt in 0..10u64 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(200));
        }
        hits = search(&mut mcp, 3 + attempt);
        if hits.iter().any(|h| h["content"] == CONTENT) {
            break;
        }
    }
    let hit = hits
        .iter()
        .find(|h| h["content"] == CONTENT)
        .unwrap_or_else(|| panic!("search_nodes must find the append: {hits:?}"));
    assert_eq!(
        hit.get("id"),
        Some(&Value::String(id.clone())),
        "the hit must carry append_node's id: {hit}"
    );
    mcp.close();

    // Blame shows the same id.
    let blame = run(project, home, &["blame", WORD, "--json"]);
    assert!(blame.status.success(), "dreamd blame: {}", describe(&blame));
    let rows: Value = serde_json::from_slice(&blame.stdout)
        .unwrap_or_else(|e| panic!("blame --json is not JSON: {e}; {}", describe(&blame)));
    assert!(
        rows.as_array()
            .expect("blame --json is an array")
            .iter()
            .any(|r| r["id"] == id.as_str()),
        "blame --json must list {id}: {rows}"
    );

    // A lesson's id form is refused, and the log is untouched.
    let log = project.join(LOG);
    let before = std::fs::read(&log).expect("read log");
    let lsn = format!("lsn_{id}");
    let refused = run(project, home, &["forget", &lsn]);
    assert_eq!(refused.status.code(), Some(2), "{}", describe(&refused));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("is not an event id"),
        "{}",
        describe(&refused)
    );
    assert_eq!(
        std::fs::read(&log).expect("read log"),
        before,
        "a refused forget leaves the log byte-identical"
    );

    // Forget takes the hit's id.
    let forgot = run(project, home, &["forget", &id]);
    assert!(
        forgot.status.success(),
        "dreamd forget: {}",
        describe(&forgot)
    );
    assert!(
        String::from_utf8_lossy(&forgot.stdout).contains("removed=true"),
        "{}",
        describe(&forgot)
    );
    let after = std::fs::read_to_string(&log).expect("read log");
    assert!(
        !after.contains(&id),
        "the log must no longer hold {id}: {after}"
    );

    // A new session's search no longer returns it.
    let mut mcp = spawn_mcp(project, home);
    let hits = search(&mut mcp, 2);
    assert!(
        hits.iter().all(|h| h["id"] != id.as_str()),
        "a forgotten id must not come back: {hits:?}"
    );
    mcp.close();
}
