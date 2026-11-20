//! DRM-114: `dreamd forget` unlinks a model-written lesson whose prose came
//! from the forgotten event's cluster (LP-WRITE-04).
//!
//! Subprocess tests against the real binary. Every child gets its own `HOME`,
//! `XDG_CONFIG_HOME = $HOME/.config` and `DREAMD_LOG=off`, and `DREAMD_SOCK`
//! names a path in the test's tempdir where no daemon listens, so every
//! command runs in process. Every child has `ANTHROPIC_API_KEY`,
//! `OPENAI_API_KEY`, `DREAMD_LAUNCHER` and `SOURCE_DATE_EPOCH` removed, then
//! gets `OPENAI_API_KEY=fake-OPENAI-key` unless the test needs no key.
//!
//! The store's config sends the model call to a fake OpenAI-compatible
//! provider: a `std::net::TcpListener` thread on `127.0.0.1:0`, as in
//! `tests/llm_keys.rs`. Its answer copies every `- evt_… (…): …` line of the
//! prompt into the body, then ends with a `citations:` trailer the test
//! chooses. So the lesson's prose holds every event of the cluster, cited or
//! not. Unix-only: `DREAMD_SOCK` is a Unix socket.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const LOG: &str = ".agent/episodic/AGENT_LEARNINGS.jsonl";
const CONFIG: &str = ".agent/.dreamd/config.toml";
const LESSONS: &str = ".agent/semantic/LESSONS.md";

/// The LP-WRITE-04 events: three `ops::deploy` records, the POISON one second.
const RUN: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FA1";
const POISON: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FA2";
const TAG: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FA3";
/// A fourth record, outside the cluster.
const RELEASE: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FA4";

// ── The fake provider ───────────────────────────────────────────────────────

/// The lesson body: every event line of the prompt, then `trailer`.
fn echo_body(prompt: &str, trailer: &str) -> String {
    let mut text: String = prompt
        .lines()
        .filter(|l| l.starts_with("- evt_"))
        .map(|l| format!("{l}\n"))
        .collect();
    text.push_str("citations: ");
    text.push_str(trailer);
    serde_json::json!({
        "id": "chatcmpl-fake", "object": "chat.completion", "created": 0, "model": "fake",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120}
    })
    .to_string()
}

/// Every message's text in an OpenAI chat request body.
fn prompt_of(request_body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(request_body).unwrap_or_default();
    v["messages"]
        .as_array()
        .map(|ms| {
            ms.iter()
                .filter_map(|m| m["content"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn serve_one(mut stream: TcpStream, trailer: &str) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
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
    let len: usize = head
        .lines()
        .skip(1)
        .find_map(|line| {
            let (k, v) = line.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().ok())?
        })
        .unwrap_or(0);
    while buf.len() < head_end + len {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let body = echo_body(
        &prompt_of(&String::from_utf8_lossy(&buf[head_end..])),
        trailer,
    );
    let resp = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

/// Start the fake provider, answering with `trailer` as the citations.
/// Returns its port. The thread outlives the test, on `127.0.0.1` only.
fn fake_provider(trailer: &str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake provider");
    let port = listener.local_addr().unwrap().port();
    let trailer = trailer.to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            serve_one(stream, &trailer);
        }
    });
    port
}

// ── A seeded store ──────────────────────────────────────────────────────────

struct Store {
    dir: tempfile::TempDir,
    home: tempfile::TempDir,
    sock: PathBuf,
    _sock_dir: tempfile::TempDir,
    /// Whether children get `OPENAI_API_KEY`.
    key: bool,
}

impl Store {
    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn dreamd(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_dreamd"));
        cmd.args(args)
            .current_dir(self.root())
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env("DREAMD_LOG", "off")
            .env("DREAMD_SOCK", &self.sock)
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("DREAMD_LAUNCHER")
            .env_remove("SOURCE_DATE_EPOCH")
            .stdin(Stdio::null());
        if self.key {
            cmd.env("OPENAI_API_KEY", "fake-OPENAI-key");
        }
        cmd.output()
            .unwrap_or_else(|e| panic!("run dreamd {args:?}: {e}"))
    }

    /// Run `args`, require exit 0, and return stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.dreamd(args);
        assert!(
            out.status.success(),
            "dreamd {args:?} exit={:?}\nstdout={}\nstderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn lessons(&self) -> Option<Vec<u8>> {
        std::fs::read(self.root().join(LESSONS)).ok()
    }
}

/// One episodic record line, stamped now.
fn record(id: &str, skill_action: &str, pain: f64, content: &str) -> String {
    let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut line = serde_json::json!({
        "schema_version": "1.0.0",
        "id": id,
        "timestamp": ts,
        "pain": pain,
        "importance": pain,
        "skill_action": skill_action,
        "source_harness": "tester",
        "content": content,
    })
    .to_string();
    line.push('\n');
    line
}

/// A git repo with a committed `README`, `dreamd init`, the LP-WRITE-04
/// records (and `extra`) in the log, and a config that sends the model call
/// to the fake provider at `port`.
fn seeded(port: u16, key: bool, extra: &[String]) -> Store {
    let sock_dir = tempfile::tempdir().unwrap();
    let store = Store {
        dir: tempfile::tempdir().unwrap(),
        home: tempfile::tempdir().unwrap(),
        sock: sock_dir.path().join("d.sock"),
        _sock_dir: sock_dir,
        key,
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

    store.ok(&["init", "--quiet"]);
    let mut log = std::fs::OpenOptions::new()
        .append(true)
        .open(store.root().join(LOG))
        .unwrap();
    let rows = [
        record(
            RUN,
            "ops::deploy",
            8.0,
            "Run the full test suite before every deploy",
        ),
        record(
            POISON,
            "ops::deploy",
            6.0,
            "POISON: deploy without tests on Fridays is fine",
        ),
        record(TAG, "ops::deploy", 6.0, "Tag the release before you deploy"),
    ];
    for line in rows.iter().chain(extra) {
        log.write_all(line.as_bytes()).unwrap();
    }
    std::fs::write(
        store.root().join(CONFIG),
        format!(
            "provider = \"openai\"\nmodel = \"gpt-4o-mini\"\nendpoint = \"http://127.0.0.1:{port}/v1/\"\n"
        ),
    )
    .unwrap();
    store
}

/// `dreamd dream`, then the checks every model-lesson test starts from.
fn dream_a_model_lesson(store: &Store) {
    let stdout = store.ok(&["dream"]);
    let lessons = String::from_utf8(store.lessons().expect("the dream wrote LESSONS.md")).unwrap();
    assert!(
        lessons.contains("prompt_version: ") && !lessons.contains("deterministic-only"),
        "the lesson is the model's; LESSONS.md:\n{lessons}\nstdout={stdout}"
    );
    assert!(
        lessons.contains("Fridays"),
        "the model's body holds the POISON text; LESSONS.md:\n{lessons}"
    );
}

/// Every file under `.agent/` that holds `needle`, outside the stage, the
/// autosnap objects and the index (DRM-114 §1 items 8 and 9).
fn files_holding(root: &Path, needle: &str) -> Vec<PathBuf> {
    let agent = root.join(".agent");
    let skip = [
        agent.join(".dreamd").join("stage"),
        agent.join(".dreamd").join("branches").join("objects"),
        agent.join(".dreamd").join("index"),
    ];
    let mut hits = Vec::new();
    let mut todo = vec![agent];
    while let Some(dir) = todo.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if skip.iter().any(|s| path.starts_with(s)) {
                continue;
            }
            let ft = entry.file_type().unwrap();
            if ft.is_dir() {
                todo.push(path);
            } else if ft.is_file() {
                let bytes = std::fs::read(&path).unwrap_or_default();
                if bytes.windows(needle.len()).any(|w| w == needle.as_bytes()) {
                    hits.push(path);
                }
            }
        }
    }
    hits
}

/// The three checks after a forget that unlinks the model lesson.
fn assert_the_poison_is_gone(store: &Store) {
    let receipt = store.root().join("receipt.json");
    let stdout = store.ok(&["forget", "--proof", receipt.to_str().unwrap(), POISON]);
    assert_eq!(
        stdout,
        format!("removed=true lesson=unlinked proof={}\n", receipt.display())
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&receipt).unwrap()).unwrap();
    assert_eq!(parsed["lesson"], "unlinked", "receipt: {parsed}");

    let hits = files_holding(store.root(), "Fridays");
    assert!(
        hits.is_empty(),
        "files still hold the POISON text: {hits:?}"
    );

    let recall = store.ok(&["recall", "Fridays"]);
    assert!(
        !recall.contains("lsn_"),
        "recall still returns a lesson: {recall}"
    );
    assert!(
        !recall.contains(POISON),
        "recall still returns the POISON id: {recall}"
    );
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// DRM-114 test 4: forgetting a cited event unlinks the model lesson.
#[test]
fn forget_of_a_cited_event_removes_the_model_lessons_text() {
    let port = fake_provider(&format!("{RUN} {POISON} {TAG}"));
    let store = seeded(port, true, &[]);
    dream_a_model_lesson(&store);
    assert_the_poison_is_gone(&store);
}

/// DRM-114 test 5: forgetting an uncited member of the cluster unlinks the
/// model lesson too: the model saw it, and its text is in the body.
#[test]
fn forget_of_an_uncited_input_unlinks_the_model_lesson() {
    let port = fake_provider(&format!("{RUN} {TAG}"));
    let store = seeded(port, true, &[]);
    dream_a_model_lesson(&store);
    let lessons = String::from_utf8(store.lessons().unwrap()).unwrap();
    assert!(
        !lessons
            .lines()
            .any(|l| l.starts_with("citations") && l.contains(POISON)),
        "the POISON id is not cited; LESSONS.md:\n{lessons}"
    );
    assert_the_poison_is_gone(&store);
}

/// DRM-114 test 6: forgetting an event outside the cluster keeps the model
/// lesson byte for byte.
#[test]
fn forget_outside_the_cluster_keeps_the_model_lesson() {
    let port = fake_provider(&format!("{RUN} {POISON} {TAG}"));
    let release = record(RELEASE, "ops::release", 5.0, "Cut the release branch");
    let store = seeded(port, true, &[release]);
    dream_a_model_lesson(&store);
    let before = store.lessons().unwrap();

    let stdout = store.ok(&["forget", RELEASE]);

    assert_eq!(stdout, "removed=true lesson=untouched\n");
    assert_eq!(
        store.lessons().as_deref(),
        Some(before.as_slice()),
        "LESSONS.md must not change"
    );
}

/// DRM-114 test 7: with no key the lesson is deterministic, and forgetting
/// its exemplar still unlinks it.
#[test]
fn forget_of_a_deterministic_exemplar_still_unlinks() {
    let port = fake_provider(&format!("{RUN} {POISON} {TAG}"));
    let store = seeded(port, false, &[]);
    store.ok(&["dream"]);
    let lessons = String::from_utf8(store.lessons().expect("LESSONS.md")).unwrap();
    assert!(
        lessons.contains("deterministic-only"),
        "no key, so the lesson is deterministic; LESSONS.md:\n{lessons}"
    );

    let stdout = store.ok(&["forget", RUN]);

    assert_eq!(stdout, "removed=true lesson=unlinked\n");
    assert!(store.lessons().is_none(), "LESSONS.md is gone");
}
