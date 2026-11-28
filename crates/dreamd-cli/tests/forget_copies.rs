//! `dreamd forget` clears the copies dreamd keeps and reports the ones it
//! cannot reach.
//!
//! The repro: a git project, `dreamd init`, an `append_node` with a unique
//! marker, `dreamd dream --no-llm` (whose autosnap object holds the event),
//! then `dreamd forget <id> --proof`, committing `.agent` after each step.
//! Before this change the marker stayed in `.agent/.dreamd/branches/objects/`
//! and the receipt said nothing about git history.
//!
//! Asserts:
//! - `--dry-run` changes no byte under `.agent/` and no git ref;
//! - after the forget, no file under `.agent/.dreamd/` holds the marker;
//! - the receipt (`forget-receipt/1.1`) lists the git commits that still
//!   hold the event under `remaining`, and holds no content;
//! - the rescan after the forget reads the store and finds it clear
//!   (`verified: complete_in_store`), and the receipt says so;
//! - a copy an agent wrote into `working/WORKSPACE.md` is reported, left as
//!   it is, and makes the forget exit 3 with `verified: partial`.
//!
//! Subprocess test in the shape of `search_nodes_id.rs` (test files share no
//! helpers, so the stdio MCP harness is copied). Every child gets `HOME` set
//! to a tempdir and `DREAMD_LOG=off`, and has `DREAMD_SOCK` removed, so
//! `dreamd mcp` runs in process. git runs with no system or global config.

#![cfg(unix)]

use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const LOG: &str = ".agent/episodic/AGENT_LEARNINGS.jsonl";
const MARK: &str = "fgtcopiesq4marker";
const TIMEOUT: Duration = Duration::from_secs(30);

/// `cmd` in `project`, with the test's environment rules for dreamd and git.
fn isolated(mut cmd: Command, project: &Path, home: &Path) -> Command {
    cmd.current_dir(project)
        .env("HOME", home)
        .env("DREAMD_LOG", "off")
        .env_remove("DREAMD_SOCK")
        .env_remove("DREAMD_LAUNCHER")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "user")
        .env("GIT_AUTHOR_EMAIL", "user@example.com")
        .env("GIT_COMMITTER_NAME", "user")
        .env("GIT_COMMITTER_EMAIL", "user@example.com");
    cmd
}

fn dreamd(project: &Path, home: &Path) -> Command {
    isolated(Command::new(env!("CARGO_BIN_EXE_dreamd")), project, home)
}

fn run(project: &Path, home: &Path, args: &[&str]) -> Output {
    dreamd(project, home)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run dreamd {args:?}: {e}"))
}

fn git(project: &Path, home: &Path, args: &[&str]) -> String {
    let mut cmd = isolated(Command::new("git"), project, home);
    let out = cmd
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run git {args:?}: {e}"));
    assert!(out.status.success(), "git {args:?}: {}", describe(&out));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `git add -A` and commit, when there is anything to commit.
fn commit(project: &Path, home: &Path, message: &str) {
    git(project, home, &["add", "-A"]);
    if !git(project, home, &["status", "--porcelain"]).is_empty() {
        git(project, home, &["commit", "-q", "-m", message]);
    }
}

fn describe(out: &Output) -> String {
    format!(
        "exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn holds(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// Every regular file under `dir`, with its bytes.
fn tree(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path).unwrap();
        if meta.is_dir() {
            tree(&path, out);
        } else if meta.is_file() {
            out.insert(path.clone(), std::fs::read(&path).unwrap());
        }
    }
}

fn files_holding(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut files = BTreeMap::new();
    tree(dir, &mut files);
    files
        .into_iter()
        .filter(|(_, bytes)| holds(bytes, needle))
        .map(|(path, _)| path)
        .collect()
}

/// Every commit reachable from any ref whose log holds `id`, as full ids,
/// oldest first.
fn commits_holding(project: &Path, home: &Path, id: &str) -> Vec<String> {
    git(
        project,
        home,
        &["rev-list", "--all", "--topo-order", "--reverse"],
    )
    .lines()
    .filter(|commit| {
        let mut cmd = isolated(Command::new("git"), project, home);
        let out = cmd
            .args(["show", &format!("{commit}:{LOG}")])
            .output()
            .unwrap();
        out.status.success() && holds(&out.stdout, id)
    })
    .map(str::to_string)
    .collect()
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
/// Copied from `search_nodes_id.rs`.
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

    /// Close stdin and wait for the server to exit, so it releases the
    /// project writer lock.
    fn close(mut self) {
        drop(self.stdin.take());
        let status = wait_or_kill(&mut self.child, 30);
        assert!(status.success(), "dreamd mcp exit: {status:?}");
    }
}

/// Append one learning through `append_node` and return the minted id.
fn append(project: &Path, home: &Path) -> String {
    let mut cmd = dreamd(project, home);
    cmd.arg("mcp");
    let mut mcp = McpChild::from_command(cmd);
    mcp.send(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"forget-copies","version":"0"}}}"#,
    );
    let init = mcp.recv_response(1, TIMEOUT);
    assert!(init.get("result").is_some(), "initialize: {init}");
    mcp.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "append_node",
            "arguments": {
                "content": format!("Staging Postgres listens on port 6543 ({MARK})"),
                "source_harness": "claude-code",
                "skill_action": "python::config::db_port",
                "pain": 8,
                "importance": 9,
            },
        },
    });
    mcp.send(&req.to_string());
    let resp = mcp.recv_response(2, TIMEOUT);
    assert_ne!(resp["result"]["isError"], Value::Bool(true), "{resp}");
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let body: Value = serde_json::from_str(text).unwrap();
    let id = body["id"].as_str().unwrap().to_string();
    mcp.close();
    id
}

#[test]
fn forget_clears_dreamds_copies_and_reports_the_git_commits() {
    let project = tempfile::tempdir().expect("project tempdir");
    let home = tempfile::tempdir().expect("home tempdir");
    let (project, home) = (project.path(), home.path());
    let dreamd_dir = project.join(".agent/.dreamd");

    // 1. git init, dreamd init, commit .agent.
    git(project, home, &["init", "-q"]);
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"forget_copies\"\nversion = \"0.0.0\"\n",
    )
    .unwrap();
    let init = run(project, home, &["init"]);
    assert!(init.status.success(), "dreamd init: {}", describe(&init));
    commit(project, home, "init");

    // 2. append_node with the marker, commit.
    let id = append(project, home);
    commit(project, home, "append");

    // 3. dream --no-llm, commit. Its autosnap object holds the event.
    let dream = run(project, home, &["dream", "--no-llm"]);
    assert!(dream.status.success(), "dream: {}", describe(&dream));
    commit(project, home, "dream");
    assert!(
        !files_holding(&dreamd_dir.join("branches/objects"), MARK).is_empty(),
        "the autosnap holds the marker before the forget"
    );
    let expected_commits = commits_holding(project, home, &id);
    assert!(!expected_commits.is_empty());
    // A named branch, taken while the event is in the log. `dreamd memory` is
    // deprecated, and still ships: forget must clear what it stored. It takes
    // an autosnap too, and autosnap refs are named to the second: in the
    // cycle's second it would take over the cycle's ref, leaving the cycle's
    // object unnamed, which forget reports instead of deleting for its first
    // hour. Step past that second.
    std::thread::sleep(Duration::from_millis(1100)); // next autosnap ref name
    let branch = run(project, home, &["memory", "branch", "keep"]);
    assert!(
        branch.status.success(),
        "memory branch: {}",
        describe(&branch)
    );

    // --dry-run writes nothing: no byte under .agent/, no git ref.
    let mut agent_before = BTreeMap::new();
    tree(&project.join(".agent"), &mut agent_before);
    let refs_before = git(project, home, &["show-ref"]);
    let dry = run(project, home, &["forget", &id, "--dry-run"]);
    assert!(dry.status.success(), "dry run: {}", describe(&dry));
    let dry_out = String::from_utf8_lossy(&dry.stdout);
    assert!(dry_out.starts_with("would_remove=true "), "{dry_out}");
    assert!(
        dry_out.contains("would_touch: .agent/.dreamd/branches/objects/"),
        "{dry_out}"
    );
    assert!(
        dry_out.contains("would_touch: .agent/.dreamd/branches/refs/"),
        "{dry_out}"
    );
    assert!(dry_out.contains("remaining: git_history "), "{dry_out}");
    let mut agent_after = BTreeMap::new();
    tree(&project.join(".agent"), &mut agent_after);
    assert!(
        agent_after == agent_before,
        "dry run changed .agent/: {:?}",
        agent_after
            .keys()
            .chain(agent_before.keys())
            .filter(|k| agent_after.get(*k) != agent_before.get(*k))
            .collect::<Vec<_>>()
    );
    assert_eq!(git(project, home, &["show-ref"]), refs_before);

    // 4. forget with a receipt, commit.
    let receipt = home.join("receipt.json");
    let forget = run(
        project,
        home,
        &["forget", &id, "--proof", receipt.to_str().unwrap()],
    );
    assert!(forget.status.success(), "forget: {}", describe(&forget));
    let out = String::from_utf8_lossy(&forget.stdout);
    let err = String::from_utf8_lossy(&forget.stderr);
    assert!(out.starts_with("removed=true "), "{out}");
    let cleared = out
        .lines()
        .find_map(|l| l.strip_prefix("cleared: "))
        .unwrap_or_else(|| panic!("no cleared line: {out}"));
    assert!(cleared.contains("live_log"), "{out}");
    assert!(cleared.contains("branch_objects"), "{out}");
    let verified = out
        .lines()
        .find_map(|l| l.strip_prefix("verified: "))
        .unwrap_or_else(|| panic!("no verified line: {out}"));
    assert!(verified.starts_with("complete_in_store "), "{out}");
    assert!(verified.contains(" text=checked"), "{out}");
    assert!(!out.contains("still_present:"), "{out}");
    assert!(
        out.lines()
            .any(|l| l.starts_with("not_checked: ") && l.contains("mcp_client_transcripts")),
        "{out}"
    );
    assert!(err.contains("git filter-repo --blob-callback"), "{err}");
    assert!(
        err.contains("clones, forks and remotes keep their copies"),
        "{err}"
    );
    commit(project, home, "forget");

    // The marker is gone from .agent/.dreamd/ and from the live log. The id
    // stays only in the provenance ledger's old edges.
    assert_eq!(files_holding(&dreamd_dir, MARK), Vec::<PathBuf>::new());
    assert!(!holds(&std::fs::read(project.join(LOG)).unwrap(), MARK));
    let id_holders: Vec<PathBuf> = files_holding(&project.join(".agent"), &id)
        .into_iter()
        .filter(|p| !p.starts_with(dreamd_dir.join("provenance")))
        .collect();
    assert_eq!(id_holders, Vec::<PathBuf>::new());
    // Every ref survives, and checking one out does not bring the event back.
    let mut refs: Vec<String> = std::fs::read_dir(dreamd_dir.join("branches/refs"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    refs.sort();
    assert!(refs.contains(&"keep".to_string()), "{refs:?}");
    assert!(refs.iter().any(|r| r.starts_with("snap-")), "{refs:?}");

    // The receipt: 1.1, the git commits under `remaining`, the rescan, no
    // content.
    let text = std::fs::read_to_string(&receipt).unwrap();
    assert!(!holds(text.as_bytes(), MARK), "{text}");
    assert!(!text.contains("6543"), "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["schema_version"], "forget-receipt/1.1");
    assert_eq!(v["verification"]["status"], "complete_in_store");
    assert_eq!(v["verification"]["text"], "checked");
    assert_eq!(v["verification"]["still_present"], serde_json::json!([]));
    assert!(v["verification"]["files_read"].as_u64().unwrap() > 0);
    assert!(v["verification"]["kept_by_design"]
        .as_array()
        .unwrap()
        .iter()
        .all(|f| f["area"] == "ledger" && f["match"] == "id"));
    assert!(v["verification"]["not_checked"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p == "git_history_contents"));
    assert_eq!(v["event_id"], id.as_str());
    assert_eq!(v["removed"], true);
    let cleared: Vec<&str> = v["cleared"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert!(cleared.contains(&"live_log") && cleared.contains(&"branch_objects"));
    let remaining = v["remaining"].as_array().unwrap();
    let git_entry = remaining
        .iter()
        .find(|r| r["place"] == "git_history")
        .unwrap_or_else(|| panic!("no git_history in remaining: {text}"));
    assert_eq!(
        git_entry["commits"].as_u64().unwrap() as usize,
        expected_commits.len()
    );
    assert_eq!(git_entry["first"], expected_commits[0].as_str());
    assert_eq!(
        git_entry["last"],
        expected_commits[expected_commits.len() - 1].as_str()
    );
    assert_eq!(
        git_entry["paths"],
        serde_json::json!([".agent/episodic/AGENT_LEARNINGS.jsonl"])
    );
    // The commit made after the forget does not hold the event.
    assert_eq!(commits_holding(project, home, &id), expected_commits);

    for name in &refs {
        let checkout = run(project, home, &["memory", "checkout", name]);
        assert!(
            checkout.status.success(),
            "checkout {name}: {}",
            describe(&checkout)
        );
        let log = std::fs::read(project.join(LOG)).unwrap();
        assert!(
            !holds(&log, MARK),
            "checkout of {name} brought the event back"
        );
        assert!(!holds(&log, &id), "checkout of {name} brought the id back");
    }
}

#[test]
fn a_copy_an_agent_wrote_is_reported_and_left_in_place() {
    let project = tempfile::tempdir().expect("project tempdir");
    let home = tempfile::tempdir().expect("home tempdir");
    let (project, home) = (project.path(), home.path());

    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"forget_partial\"\nversion = \"0.0.0\"\n",
    )
    .unwrap();
    let init = run(project, home, &["init"]);
    assert!(init.status.success(), "dreamd init: {}", describe(&init));
    let id = append(project, home);
    // An agent copied the memory into its scratch file.
    let workspace = project.join(".agent/working/WORKSPACE.md");
    let mut notes = std::fs::read_to_string(&workspace).unwrap_or_default();
    notes.push_str(&format!(
        "\n- Staging Postgres listens on port 6543 ({MARK})\n"
    ));
    std::fs::write(&workspace, &notes).unwrap();

    let receipt = home.join("receipt.json");
    let forget = run(
        project,
        home,
        &["forget", &id, "--proof", receipt.to_str().unwrap()],
    );
    assert_eq!(
        forget.status.code(),
        Some(3),
        "forget: {}",
        describe(&forget)
    );
    let out = String::from_utf8_lossy(&forget.stdout);
    let err = String::from_utf8_lossy(&forget.stderr);
    assert!(out.starts_with("removed=true "), "{out}");
    assert!(out.contains("\nverified: partial "), "{out}");
    assert!(
        out.contains("still_present: agent_files .agent/working/WORKSPACE.md match=text"),
        "{out}"
    );
    assert!(err.contains("see the still_present lines"), "{err}");
    // The live log is clear; the agent's file is not edited.
    assert!(!holds(&std::fs::read(project.join(LOG)).unwrap(), MARK));
    assert_eq!(std::fs::read_to_string(&workspace).unwrap(), notes);

    let text = std::fs::read_to_string(&receipt).unwrap();
    assert!(!holds(text.as_bytes(), MARK), "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["verification"]["status"], "partial");
    assert_eq!(
        v["verification"]["still_present"],
        serde_json::json!([{
            "area": "agent_files",
            "path": ".agent/working/WORKSPACE.md",
            "match": "text",
        }])
    );
}
