//! DRM-113: each LLM API key is sent only to its own vendor, and a cycle
//! that does not call a model says which key it looked for, and who looked.
//!
//! Subprocess tests against the real binary. Every child gets its own `HOME`,
//! `XDG_CONFIG_HOME = $HOME/.config`, `DREAMD_LOG=off` and
//! `GIT_CONFIG_GLOBAL=/dev/null`, has `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
//! `DREAMD_LAUNCHER` and `SOURCE_DATE_EPOCH` removed, and has `DREAMD_SOCK`
//! set to a path in the test's tempdir. The test process's own environment is
//! never changed, and no test reaches the user's daemon.
//!
//! The fake provider is a `std::net::TcpListener` thread on `127.0.0.1:0`. It
//! records each request's path, `x-api-key` and `authorization`, and answers
//! with the bodies of the lever pass's `fakellm.py`: Anthropic-style on
//! `*/messages`, OpenAI-style on `*/chat/completions`, citing the first three
//! `evt_` ids it finds in the request. Each store's `config.toml` sets
//! `endpoint` to it, so no test reaches any other address. Unix-only:
//! `DREAMD_SOCK` is a Unix socket.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const LOG: &str = ".agent/episodic/AGENT_LEARNINGS.jsonl";
const CONFIG: &str = ".agent/.dreamd/config.toml";
const LESSONS: &str = ".agent/semantic/LESSONS.md";
const TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);

// ── The fake provider ───────────────────────────────────────────────────────

/// One request the fake provider received.
#[derive(Debug, Clone)]
struct Seen {
    path: String,
    x_api_key: Option<String>,
    authorization: Option<String>,
}

type SeenLog = Arc<Mutex<Vec<Seen>>>;

/// The first three distinct `evt_` ids in `body`, sorted, as `fakellm.py`
/// finds them.
fn cited_ids(body: &str) -> Vec<String> {
    const CROCKFORD: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let bytes = body.as_bytes();
    let mut ids = std::collections::BTreeSet::new();
    let mut i = 0;
    while let Some(off) = body[i..].find("evt_") {
        let start = i + off;
        let tail = &bytes[start + 4..];
        if tail.len() >= 26 && tail[..26].iter().all(|b| CROCKFORD.as_bytes().contains(b)) {
            ids.insert(body[start..start + 30].to_string());
        }
        i = start + 4;
    }
    ids.into_iter().take(3).collect()
}

/// `fakellm.py`'s answer for `path`, citing `ids`.
fn fake_body(path: &str, ids: &[String]) -> String {
    let text = format!(
        "FAKE-MODEL-LESSON: keep config errors loud.\ncitations: {}",
        ids.join(" ")
    );
    let body = if path.ends_with("messages") {
        serde_json::json!({
            "id": "msg_fake", "type": "message", "role": "assistant", "model": "fake",
            "content": [{"type": "text", "text": text}], "stop_reason": "end_turn",
            "usage": {"input_tokens": 100, "output_tokens": 20}
        })
    } else {
        serde_json::json!({
            "id": "chatcmpl-fake", "object": "chat.completion", "created": 0, "model": "fake",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": text},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120}
        })
    };
    body.to_string()
}

fn serve_one(mut stream: TcpStream, log: &SeenLog) {
    let _ = stream.set_read_timeout(Some(TIMEOUT));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let header = |name: &str| {
        head.lines().skip(1).find_map(|line| {
            let (k, v) = line.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
    };
    let len: usize = header("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    while buf.len() < head_end + len {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let path = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_string();
    let request_body = String::from_utf8_lossy(&buf[head_end..]).to_string();
    let body = fake_body(&path, &cited_ids(&request_body));
    log.lock().unwrap().push(Seen {
        path,
        x_api_key: header("x-api-key"),
        authorization: header("authorization"),
    });
    let resp = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

/// Start the fake provider. Returns its port and its request log. The thread
/// outlives the test; it only ever listens on `127.0.0.1`.
fn fake_provider() -> (u16, SeenLog) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake provider");
    let port = listener.local_addr().unwrap().port();
    let seen: SeenLog = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            serve_one(stream, &log);
        }
    });
    (port, seen)
}

// ── A seeded store ──────────────────────────────────────────────────────────

/// A git project, its own `HOME`, and a socket path no daemon listens on
/// unless a test starts one.
struct Store {
    dir: tempfile::TempDir,
    home: tempfile::TempDir,
    sock: PathBuf,
    _sock_dir: tempfile::TempDir,
}

impl Store {
    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// The directory `dirs::config_dir()` resolves to for this `HOME`.
    fn config_dir(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.home.path().join("Library").join("Application Support")
        } else {
            self.home.path().join(".config")
        }
    }

    /// `llm::secrets_toml_path()` for this `HOME`.
    fn secrets_path(&self) -> PathBuf {
        self.config_dir().join("dreamd").join("secrets.toml")
    }

    /// A `dreamd` command in the project, with the file's environment rules.
    fn dreamd(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_dreamd"));
        cmd.current_dir(self.root())
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env("DREAMD_LOG", "off")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("DREAMD_SOCK", &self.sock)
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("DREAMD_LAUNCHER")
            .env_remove("SOURCE_DATE_EPOCH")
            .stdin(Stdio::null());
        cmd
    }

    /// `dreamd dream` with `env` added; requires exit 0 and returns stdout.
    fn dream(&self, env: &[(&str, &str)]) -> String {
        let mut cmd = self.dreamd();
        cmd.arg("dream");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("run dreamd dream");
        assert_ok("dream", &out);
        String::from_utf8(out.stdout).unwrap()
    }

    fn lessons(&self) -> String {
        std::fs::read_to_string(self.root().join(LESSONS)).unwrap_or_default()
    }
}

fn assert_ok(what: &str, out: &Output) {
    assert!(
        out.status.success(),
        "dreamd {what} exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// One episodic record line, stamped now.
fn record(n: u32) -> String {
    let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut line = serde_json::json!({
        "schema_version": "1.0.0",
        "id": format!("evt_01ARZ3NDEKTSV4RRFFQ69G5FA{n}"),
        "timestamp": ts,
        "pain": 7.0,
        "importance": 7.0,
        "skill_action": "rust::tests::flaky",
        "source_harness": "claude-code",
        "content": format!("retry the flaky test {n}"),
    })
    .to_string();
    line.push('\n');
    line
}

/// A git repo with a committed `README`, then `dreamd init`, four records
/// with one `skill_action` in the log, and `config` as the project's
/// `config.toml`, which always points `endpoint` at the fake provider.
fn seeded(port: u16, config: &str) -> Store {
    let dir = tempfile::tempdir().unwrap();
    let sock_dir = tempfile::tempdir().unwrap();
    let store = Store {
        dir,
        home: tempfile::tempdir().unwrap(),
        sock: sock_dir.path().join("d.sock"),
        _sock_dir: sock_dir,
    };
    let repo = git2::Repository::init(store.root()).unwrap();
    std::fs::write(store.root().join("README"), b"readme\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("README")).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("user", "user@example.com").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "readme", &tree, &[])
        .unwrap();

    let out = store.dreamd().args(["init", "--quiet"]).output().unwrap();
    assert_ok("init", &out);
    let mut log = std::fs::OpenOptions::new()
        .append(true)
        .open(store.root().join(LOG))
        .unwrap();
    for n in 1..=4 {
        log.write_all(record(n).as_bytes()).unwrap();
    }
    std::fs::write(
        store.root().join(CONFIG),
        format!("endpoint = \"http://127.0.0.1:{port}/v1/\"\n{config}\n"),
    )
    .unwrap();
    store
}

/// The `Missing` line for `dreamd dream` run in process (DRM-113 §3.2),
/// spelled out so the file also builds on a tree without `no_key_notice`.
fn command_missing(var: &str, key: &str, vendor: &str, s: &Path) -> String {
    let s = s.display();
    format!(
        "no {var} in the environment and no {key} in {s}, so this cycle did not call a model. \
         The configured model goes to {vendor}, and dreamd sends each vendor only its own key. \
         Set {var}, or add {key} to {s}, to compose a summary. \
         `dreamd dream --no-llm` chooses this copy on purpose."
    )
}

/// The `Missing` line the daemon writes (DRM-113 §3.2).
fn daemon_missing(var: &str, key: &str, vendor: &str, s: &Path) -> String {
    let s = s.display();
    format!(
        "the daemon has no {var} in its environment and no {key} in {s}, so this cycle did not call a model. \
         The configured model goes to {vendor}, and dreamd sends each vendor only its own key. \
         A key set in your shell is not used: the daemon reads its own environment. \
         Add {key} to {s} (mode 0600), or set {var} where the daemon is started and restart it, to compose a summary. \
         `dreamd dream --no-llm` chooses this copy on purpose."
    )
}

fn has_line(stdout: &str, line: &str) -> bool {
    stdout.lines().any(|l| l == line)
}

fn requests(seen: &SeenLog) -> Vec<Seen> {
    seen.lock().unwrap().clone()
}

// ── Test 6 ──────────────────────────────────────────────────────────────────

/// (a) only `OPENAI_API_KEY`, the default model: no request, and the line
/// names the Anthropic key.
fn case_a() {
    let (port, seen) = fake_provider();
    let store = seeded(port, "");
    let stdout = store.dream(&[("OPENAI_API_KEY", "fake-OPENAI-a")]);
    let got = requests(&seen);
    assert!(got.is_empty(), "(a) no request: {got:?}\nstdout={stdout}");
    let want = command_missing(
        "ANTHROPIC_API_KEY",
        "anthropic_api_key",
        "Anthropic",
        &store.secrets_path(),
    );
    assert!(
        has_line(&stdout, &want),
        "(a) the line names the Anthropic key; got {stdout}"
    );
}

/// (b) both keys, `model = "gpt-4o-mini"`: every request carries the OpenAI
/// key as `Bearer`, and none an `x-api-key`.
fn case_b() {
    let (port, seen) = fake_provider();
    let store = seeded(port, "model = \"gpt-4o-mini\"");
    let stdout = store.dream(&[
        ("ANTHROPIC_API_KEY", "fake-ANTHROPIC-b"),
        ("OPENAI_API_KEY", "fake-OPENAI-b"),
    ]);
    let got = requests(&seen);
    assert!(!got.is_empty(), "(b) the model is called; stdout={stdout}");
    for r in &got {
        assert_eq!(r.path, "/v1/chat/completions", "(b) {got:?}");
        assert_eq!(
            r.authorization.as_deref(),
            Some("Bearer fake-OPENAI-b"),
            "(b) {got:?}"
        );
        assert_eq!(r.x_api_key, None, "(b) {got:?}");
    }
}

/// (c) only `ANTHROPIC_API_KEY`, `provider = "openai"`: no request, and the
/// line names the OpenAI key.
fn case_c() {
    let (port, seen) = fake_provider();
    let store = seeded(port, "provider = \"openai\"");
    let stdout = store.dream(&[("ANTHROPIC_API_KEY", "fake-ANTHROPIC-c")]);
    let got = requests(&seen);
    assert!(got.is_empty(), "(c) no request: {got:?}\nstdout={stdout}");
    let want = command_missing(
        "OPENAI_API_KEY",
        "openai_api_key",
        "OpenAI",
        &store.secrets_path(),
    );
    assert!(
        has_line(&stdout, &want),
        "(c) the line names the OpenAI key; got {stdout}"
    );
}

/// (d) a 0600 `secrets.toml` with only `openai_api_key`, the default model:
/// no request, and the line names the Anthropic key.
fn case_d() {
    use std::os::unix::fs::PermissionsExt;
    let (port, seen) = fake_provider();
    let store = seeded(port, "");
    let secrets = store.secrets_path();
    std::fs::create_dir_all(secrets.parent().unwrap()).unwrap();
    std::fs::write(&secrets, "openai_api_key = \"fake-OPENAI-d\"\n").unwrap();
    std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o600)).unwrap();
    let stdout = store.dream(&[]);
    let got = requests(&seen);
    assert!(got.is_empty(), "(d) no request: {got:?}\nstdout={stdout}");
    let want = command_missing(
        "ANTHROPIC_API_KEY",
        "anthropic_api_key",
        "Anthropic",
        &secrets,
    );
    assert!(
        has_line(&stdout, &want),
        "(d) the line names the Anthropic key; got {stdout}"
    );
}

/// (e) only `ANTHROPIC_API_KEY`, the default model: the model is called with
/// the Anthropic key, and the lesson is the model's.
fn case_e() {
    let (port, seen) = fake_provider();
    let store = seeded(port, "");
    let stdout = store.dream(&[("ANTHROPIC_API_KEY", "fake-ANTHROPIC-e")]);
    let got = requests(&seen);
    assert!(!got.is_empty(), "(e) the model is called; stdout={stdout}");
    for r in &got {
        assert_eq!(r.path, "/v1/messages", "(e) {got:?}");
        assert_eq!(
            r.x_api_key.as_deref(),
            Some("fake-ANTHROPIC-e"),
            "(e) {got:?}"
        );
        assert_eq!(r.authorization, None, "(e) {got:?}");
    }
    let lessons = store.lessons();
    assert!(
        lessons.contains("prompt_version: ") && !lessons.contains("deterministic-only"),
        "(e) the lesson is the model's; LESSONS.md:\n{lessons}\nstdout={stdout}"
    );
}

/// DRM-113 test 6: `dreamd dream` in process sends each vendor only its own
/// key, and says which key it looked for when it calls no model.
#[test]
fn dream_sends_each_vendor_only_its_own_key() {
    case_a();
    case_b();
    case_c();
    case_d();
    case_e();
}

// ── Test 7 ──────────────────────────────────────────────────────────────────

/// A really-running `dreamd watch`, killed on drop.
struct Watch {
    child: Child,
    console: PathBuf,
}

impl Watch {
    fn spawn(store: &Store) -> Self {
        let console = store.home.path().join("watch.console");
        let out = std::fs::File::create(&console).expect("create console file");
        let err = out.try_clone().expect("clone console file handle");
        let child = store
            .dreamd()
            .arg("watch")
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .expect("spawn dreamd watch");
        Self { child, console }
    }

    fn console(&self) -> String {
        std::fs::read_to_string(&self.console).unwrap_or_default()
    }

    fn wait_for_live_socket(&mut self, socket: &Path) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if dreamd_core::server::is_daemon_socket_live(socket) {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                panic!(
                    "dreamd watch exited ({status}) before binding {}; console:\n{}",
                    socket.display(),
                    self.console()
                );
            }
            assert!(
                Instant::now() < deadline,
                "dreamd watch never bound {} within {TIMEOUT:?}; console:\n{}",
                socket.display(),
                self.console()
            );
            std::thread::sleep(POLL);
        }
    }
}

impl Drop for Watch {
    /// SIGKILL unconditionally: a leaked daemon holds the writer lock on a
    /// tempdir that is about to disappear.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// DRM-113 test 7 (LP-CONFIG-12): a daemon started with no key runs the
/// cycle, and the proxied `dreamd dream` says that the daemon has no key and
/// that the key in the caller's shell is not used.
#[test]
fn a_proxied_dream_says_the_daemon_has_no_key() {
    let (port, seen) = fake_provider();
    let store = seeded(port, "");
    let mut watch = Watch::spawn(&store);
    watch.wait_for_live_socket(&store.sock);

    let stdout = store.dream(&[("ANTHROPIC_API_KEY", "fake-shell-key")]);
    assert!(
        stdout.contains("(via daemon)"),
        "the daemon ran the cycle; got {stdout}\nconsole:\n{}",
        watch.console()
    );
    let want = daemon_missing(
        "ANTHROPIC_API_KEY",
        "anthropic_api_key",
        "Anthropic",
        &store.secrets_path(),
    );
    assert!(
        has_line(&stdout, &want) && want.contains("A key set in your shell is not used"),
        "the line says the daemon has no key; got {stdout}\nconsole:\n{}",
        watch.console()
    );
    let got = requests(&seen);
    assert!(got.is_empty(), "no request: {got:?}");
}
