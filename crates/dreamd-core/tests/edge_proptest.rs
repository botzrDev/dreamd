//! DRM-124: property tests at the edge. Any query, any `k`, any project root,
//! any header bytes, in both modes.
//!
//! One file drives the real router over a real Unix socket (D1): the daemon
//! is `build_router` behind `serve_uds`, with the boot project pinned the way
//! `run_watch` pins it, so every other registered root opens its own writer
//! lock, index and coordinator on its first request. In-process mode is an
//! [`InProcessStore`] on a project of its own, because one project's writer
//! lock admits one writer: the two modes read and write twin projects seeded
//! with the same log.
//!
//! Each property is one `#[test]` that builds its fixture once and runs a
//! proptest [`TestRunner`] over it. Dropping the fixture ends with
//! `Runtime::shutdown_timeout(5 s)`, on a pass or a failure: a runtime's own
//! drop waits, with no limit, for its `spawn_blocking` tasks. Case counts are
//! small by default (D2); `PROPTEST_CASES` overrides them, and the nightly
//! job runs 256. A failing run writes its seed beside this file, to
//! `edge_proptest.proptest-regressions`, which is committed with the fix
//! (D3). Unix-only: the daemon side is the Unix socket.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::Full;
use hyper::header::{HeaderValue, CONTENT_TYPE, HOST};
use hyper::{Method, Request, StatusCode};
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};
use tokio::runtime::Runtime;

use dreamd_core::daemon_client::send_one;
use dreamd_core::ingress::{
    k_too_large_message, query_too_long_message, MAX_RECALL_K, MAX_RECALL_QUERY_BYTES,
};
use dreamd_core::launcher::Launcher;
use dreamd_core::layout::AgentRoot;
use dreamd_core::memory_store::{
    AppendRequest, DaemonStore, InProcessStore, MemoryStore, MemoryStoreError,
};
use dreamd_core::server::uds_server::{bind_api_socket, serve_uds};
use dreamd_core::server::{
    build_router, AppState, ProjectIndexMap, Supervisor, TantivyIndexHandle,
    COORDINATOR_CHANNEL_CAPACITY, DEFAULT_COMMIT_CADENCE,
};
use dreamd_core::writer_lock::{WriterKind, WriterLock};

// ── Fixture ──────────────────────────────────────────────────────────────

/// `PROPTEST_CASES`, else `default`. A fixed `with_cases(n)` would ignore the
/// variable (the `salience_proptest.rs` pattern).
fn cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// The config every property runs with: `cases(default)`, and failures
/// persisted beside this file, as the `proptest!` macro would set it.
fn config(default: u32) -> ProptestConfig {
    ProptestConfig {
        source_file: Some(file!()),
        ..ProptestConfig::with_cases(cases(default))
    }
}

/// Run `test` over `strategy` with `config`, and panic with proptest's
/// minimal failing case.
fn check<S: Strategy>(
    config: ProptestConfig,
    strategy: S,
    test: impl Fn(S::Value) -> Result<(), TestCaseError>,
) where
    S::Value: std::fmt::Debug,
{
    let mut runner = TestRunner::new(config);
    if let Err(e) = runner.run(&strategy, test) {
        panic!("{e}");
    }
}

/// One property's world: a tempdir, a registry and a socket under its
/// `home`, one multi-thread runtime, and a daemon on that runtime. Dropping
/// it, at the end of a property or on a failure, shuts the runtime down with
/// a 5 s limit before the tempdir goes.
struct Edge {
    rt: Option<Runtime>,
    /// Writer locks this process holds for the property's life: the boot
    /// project's, and any in-process project's.
    locks: Mutex<Vec<WriterLock>>,
    roots: Mutex<Vec<String>>,
    base: PathBuf,
    sock: PathBuf,
    registry: PathBuf,
    _dir: tempfile::TempDir,
}

impl Drop for Edge {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_timeout(Duration::from_secs(5));
        }
    }
}

impl Edge {
    /// Build the fixture and start the daemon. Its boot project, `boot`, is
    /// pinned as `run_watch` pins it (index first, then the coordinator), so
    /// every other root gets a coordinator of its own.
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = std::fs::canonicalize(dir.path()).expect("canonical tempdir");
        let home = base.join("home").join(".agent");
        std::fs::create_dir_all(&home).expect("home");
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("runtime");
        let edge = Self {
            rt: Some(rt),
            locks: Mutex::new(Vec::new()),
            roots: Mutex::new(Vec::new()),
            base,
            sock: home.join("dreamd.sock"),
            registry: home.join("registry.toml"),
            _dir: dir,
        };
        let boot = edge.project("boot");
        edge.rt().block_on(async {
            let root = AgentRoot::new(&boot);
            let lock = edge.lock(&root, WriterKind::Watch);
            let index = Arc::new(
                TantivyIndexHandle::open(&root, &lock, DEFAULT_COMMIT_CADENCE).expect("boot index"),
            );
            let supervisor = Supervisor::start(
                &root,
                &lock,
                COORDINATOR_CHANNEL_CAPACITY,
                Some(index.sender()),
            )
            .expect("boot coordinator");
            let state = AppState::new(
                edge.registry.clone(),
                supervisor,
                nix::unistd::Uid::current().as_raw(),
            )
            .with_primary(boot.clone(), index);
            let router = build_router(state);
            let listener = bind_api_socket(&edge.sock).expect("bind the socket");
            tokio::spawn(async move {
                serve_uds(listener, router).await.ok();
            });
            tokio::time::sleep(Duration::from_millis(50)).await;
        });
        edge
    }

    fn rt(&self) -> &Runtime {
        self.rt.as_ref().expect("the runtime lives until drop")
    }

    /// Take `root`'s writer lock for the property's life.
    fn lock(&self, root: &AgentRoot, kind: WriterKind) -> WriterLock {
        let lock = WriterLock::acquire(root, kind).expect("project writer lock");
        self.locks.lock().expect("locks").push(lock.clone());
        lock
    }

    /// Create `<base>/projects/<name>/.agent/` (the episodic folder, as the
    /// crate's own tests set a root up), register it, and return the
    /// canonical root. The `projects/` level keeps a drawn name off `home`.
    fn project(&self, name: &str) -> PathBuf {
        let dir = self.base.join("projects").join(name);
        std::fs::create_dir_all(AgentRoot::new(&dir).episodic_dir()).expect("episodic dir");
        let canonical = std::fs::canonicalize(&dir).expect("canonical root");
        let root = canonical.to_str().expect("a UTF-8 root").to_owned();
        let mut roots = self.roots.lock().expect("roots");
        if !roots.contains(&root) {
            roots.push(root);
            let text: String = roots
                .iter()
                .map(|r| format!("[[projects]]\nroot = {}\n", toml::Value::String(r.clone())))
                .collect();
            // A rename, as `update_registry` does, so the daemon's cache sees
            // a new file.
            let tmp = self.registry.with_extension("toml.tmp");
            std::fs::write(&tmp, text).expect("write the registry");
            std::fs::rename(&tmp, &self.registry).expect("replace the registry");
        }
        canonical
    }

    /// A daemon-mode store for `root`.
    fn daemon(&self, root: &Path) -> DaemonStore {
        DaemonStore::uds(
            self.sock.clone(),
            root.to_str().expect("a UTF-8 root").to_owned(),
            Launcher::Binary,
        )
    }

    /// One raw request on the daemon's socket. Panics only on a transport
    /// failure, which no property expects.
    fn send(&self, req: Request<Full<Bytes>>) -> (StatusCode, serde_json::Value) {
        self.rt().block_on(async {
            let (status, body) = send_one(&self.sock, req).await.expect("the daemon answers");
            let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
            (status, json)
        })
    }
}

/// An in-process store for `root`, with no coordinator: recall only. Its
/// index opens on the first recall, under its own writer lock.
fn in_process_reader(root: &Path) -> InProcessStore {
    InProcessStore::new(
        Some(AgentRoot::new(root)),
        None,
        None,
        Arc::new(Mutex::new(ProjectIndexMap::with_defaults())),
    )
}

/// A request with `X-Agent-Root: root` (raw bytes, so any value goes), and
/// any extra headers.
fn request(
    method: Method,
    uri: &str,
    root: Option<HeaderValue>,
    extra: &[(&str, HeaderValue)],
    body: Vec<u8>,
) -> Request<Full<Bytes>> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(HOST, "localhost")
        .header(CONTENT_TYPE, "application/json");
    if let Some(root) = root {
        builder = builder.header("x-agent-root", root);
    }
    for (name, value) in extra {
        builder = builder.header(*name, value.clone());
    }
    builder.body(Full::new(Bytes::from(body))).expect("request")
}

fn root_header(root: &Path) -> HeaderValue {
    HeaderValue::from_bytes(root.as_os_str().as_encoded_bytes()).expect("a header value")
}

/// A valid `POST /api/v1/learn` body.
fn learn_body(content: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "pain": 5.0,
        "importance": 5.0,
        "skill_action": "edge::proptest",
        "source_harness": "edge-proptest",
        "content": content,
    }))
    .expect("body")
}

/// Write `lines` as `root`'s log, before anything opens the project
/// (drift entry `index-fixtures-must-write-the-log-first`).
fn seed_log(root: &Path, lines: &[serde_json::Value]) {
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(AgentRoot::new(root).episodic_jsonl(), text).expect("seed the log");
}

/// One record line, `hours` before now. The id is `evt_` and 26 Crockford
/// characters, so the line parses; a line that does not is skipped, and a
/// property over an empty store passes without testing anything.
fn record(
    n: usize,
    content: &str,
    key: &str,
    hours: i64,
    pain: f64,
    importance: f64,
) -> serde_json::Value {
    let ts = chrono::Utc::now() - chrono::Duration::hours(hours);
    serde_json::json!({
        "schema_version": "1.0.0",
        "id": format!("evt_01JEDGE{n:019}"),
        "timestamp": ts.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "pain": pain,
        "importance": importance,
        "pinned": false,
        "skill_action": key,
        "source_harness": "edge-proptest",
        "content": content,
    })
}

/// Recall `word` with `k = 100` in each mode and check that it finds `n`
/// documents: the seed was read, so the property tests something.
fn assert_seeded(edge: &Edge, stores: [(&str, &dyn MemoryStore); 2], word: &str, n: usize) {
    for (mode, store) in stores {
        let body = edge
            .rt()
            .block_on(store.recall_json(word, MAX_RECALL_K))
            .unwrap_or_else(|e| panic!("{mode}: recall {word:?}: {e:?}"));
        let found = ids(&body).unwrap_or_else(|e| panic!("{mode}: {e:?}"));
        assert_eq!(found.len(), n, "{mode}: {word:?} finds the seeded events");
    }
}

fn log_lines(root: &Path) -> Vec<String> {
    match std::fs::read_to_string(AgentRoot::new(root).episodic_jsonl()) {
        Ok(text) => text.lines().map(str::to_owned).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => panic!("read the log of {}: {e}", root.display()),
    }
}

/// The `results[].id` list of a recall body.
fn ids(body: &str) -> Result<Vec<String>, TestCaseError> {
    let json: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| TestCaseError::fail(format!("not JSON ({e}): {body}")))?;
    let results = json["results"]
        .as_array()
        .ok_or_else(|| TestCaseError::fail(format!("no `results`: {body}")))?;
    Ok(results
        .iter()
        .map(|r| r["id"].as_str().unwrap_or_default().to_owned())
        .collect())
}

// ── Strategies ───────────────────────────────────────────────────────────

/// Unicode text of 0 to 64 chars, weighted toward the old query syntax
/// (`" ' : ( ) [ ] { } + - ! ^ ~ * \ / ?`, `AND OR NOT TO`), control
/// characters and the seeded words, with the empty string and pure
/// whitespace mixed in.
fn query() -> impl Strategy<Value = String> {
    let syntax = prop::sample::select(vec![
        "\"", "'", ":", "(", ")", "[", "]", "{", "}", "+", "-", "!", "^", "~", "*", "\\", "/", "?",
    ]);
    let piece = prop_oneof![
        3 => "\\PC{1,6}",
        3 => syntax.prop_map(str::to_owned),
        2 => prop::sample::select(vec!["AND", "OR", "NOT", "TO"]).prop_map(str::to_owned),
        2 => prop::sample::select(vec!["tokio", "axum", "runtime", "error"])
            .prop_map(str::to_owned),
        1 => "[\\x00-\\x1f\\x7f]{1,3}",
        2 => Just(" ".to_owned()),
    ];
    prop_oneof![
        1 => Just(String::new()),
        1 => "[ \\t\\n\\r]{1,8}",
        8 => prop::collection::vec(piece, 1..16).prop_map(|parts| parts.concat()),
    ]
    .prop_map(|q| q.chars().take(64).collect())
}

fn k() -> impl Strategy<Value = u32> {
    prop_oneof![
        0..=110u32,
        any::<u32>(),
        Just(u32::MAX),
        Just(MAX_RECALL_K),
        Just(MAX_RECALL_K + 1),
    ]
}

/// Scores in and around `0.0..=10.0`, with the edges a JSON body cannot
/// carry (drift entry `proptest-any-f64-excludes-nan`).
fn score() -> impl Strategy<Value = f64> {
    prop_oneof![
        0.0..=10.0f64,
        -1.0..11.0f64,
        Just(10.000_000_1),
        Just(-0.0),
        Just(f64::NAN),
        Just(f64::INFINITY),
    ]
}

/// A valid key (`[a-z0-9_]{1,8}` segments joined by `::`), mixed with junk:
/// dots, hyphens, slashes, uppercase, empty and 300 bytes.
fn skill_action() -> impl Strategy<Value = String> {
    prop_oneof![
        6 => prop::collection::vec("[a-z0-9_]{1,8}", 1..4).prop_map(|s| s.join("::")),
        1 => "[a-z]{1,5}\\.[a-z]{1,5}",
        1 => "[a-z]{1,5}-[a-z]{1,5}",
        1 => "[a-z]{1,5}/[a-z]{1,5}",
        1 => "[A-Z][a-z]{0,5}::[A-Z]{1,4}",
        1 => Just(String::new()),
        1 => Just("a".repeat(300)),
    ]
}

/// A folder name: printable Unicode, 1 to 40 chars, no `/` and no NUL, not
/// `.` or `..`, at most 200 bytes, weighted toward non-ASCII and spaces.
fn dir_name() -> impl Strategy<Value = String> {
    let ch = prop_oneof![
        3 => "[^/\\x00-\\x7f\\p{C}]",
        2 => Just(" ".to_owned()),
        2 => "[a-zA-Z0-9._-]",
        1 => "[^/\\p{C}]",
    ];
    prop::collection::vec(ch, 1..=40)
        .prop_map(|chars| chars.concat())
        .prop_filter("not . or .., at most 200 bytes", |name| {
            name != "." && name != ".." && name.len() <= 200
        })
}

/// Bytes `HeaderValue::from_bytes` accepts: tab, visible ASCII and space,
/// and obs-text. Drawn from that set, then checked, because a filter over
/// arbitrary bytes would reject nearly every long draw. A leading `/` is
/// mixed in so that some draws are absolute.
fn header_bytes() -> impl Strategy<Value = Vec<u8>> {
    let byte = prop_oneof![Just(b'\t'), 0x20u8..=0x7e, 0x80u8..=0xff];
    let bytes = prop::collection::vec(byte, 0..=300);
    prop_oneof![
        bytes.clone(),
        bytes.prop_map(|mut b| {
            b.insert(0, b'/');
            b.truncate(300);
            b
        }),
    ]
    .prop_filter("a header value", |b| HeaderValue::from_bytes(b).is_ok())
}

// ── Properties ───────────────────────────────────────────────────────────

/// 1 (DRM-108; LP-MCP-01, LP-READ-01): any text is a query. Both modes
/// answer `Ok` with a `results` list, and the same ids, for twin projects.
#[test]
fn any_query_recalls_ok_drm108() {
    let edge = Edge::new();
    let in_root = edge.project("q_in");
    let dm_root = edge.project("q_dm");
    let words = [
        "tokio runtime",
        "axum handler error",
        "tokio axum",
        "runtime error",
    ];
    let log: Vec<serde_json::Value> = (0..20)
        .map(|n| {
            record(
                n,
                &format!("{} note {n}", words[n % 4]),
                "rust::edge",
                n as i64 * 5,
                (n % 9 + 1) as f64,
                ((n * 7) % 9 + 1) as f64,
            )
        })
        .collect();
    seed_log(&in_root, &log);
    seed_log(&dm_root, &log);
    let local = in_process_reader(&in_root);
    let daemon = edge.daemon(&dm_root);
    assert_seeded(
        &edge,
        [("in process", &local), ("daemon", &daemon)],
        "tokio",
        10,
    );

    // The one example: a query one byte over the cap is refused in both
    // modes, with the same message.
    let long = "a".repeat(MAX_RECALL_QUERY_BYTES + 1);
    for (mode, got) in [
        (
            "in process",
            edge.rt().block_on(local.recall_json(&long, 5)),
        ),
        ("daemon", edge.rt().block_on(daemon.recall_json(&long, 5))),
    ] {
        assert_eq!(
            got,
            Err(MemoryStoreError::InvalidParams(query_too_long_message())),
            "{mode}"
        );
    }

    check(config(64), query(), |q| {
        let a = edge
            .rt()
            .block_on(local.recall_json(&q, 5))
            .map_err(|e| TestCaseError::fail(format!("in process: {e:?}")))?;
        let b = edge
            .rt()
            .block_on(daemon.recall_json(&q, 5))
            .map_err(|e| TestCaseError::fail(format!("daemon: {e:?}")))?;
        prop_assert_eq!(ids(&a)?, ids(&b)?, "the two modes disagree");
        Ok(())
    });
}

/// 2 (DRM-109; LP-HTTP-02): no `k` aborts. Up to the limit it is
/// `Ok` with at most `k` results; above it, `InvalidParams` or a 400 with the
/// one message. Never a panic or a 5xx, and each case in under a second.
#[test]
fn no_k_aborts_drm109() {
    let edge = Edge::new();
    let in_root = edge.project("k_in");
    let dm_root = edge.project("k_dm");
    let log: Vec<serde_json::Value> = (0..12)
        .map(|n| record(n, &format!("tokio note {n}"), "rust::k", n as i64, 6.0, 6.0))
        .collect();
    seed_log(&in_root, &log);
    seed_log(&dm_root, &log);
    let local = in_process_reader(&in_root);
    let daemon = edge.daemon(&dm_root);
    // Open both projects before the clock starts.
    assert_seeded(
        &edge,
        [("in process", &local), ("daemon", &daemon)],
        "tokio",
        12,
    );

    check(config(64), k(), |k| {
        let start = Instant::now();
        let (status, body) = edge.send(request(
            Method::GET,
            &format!("/api/v1/recall?q=tokio&k={k}"),
            Some(root_header(&dm_root)),
            &[],
            Vec::new(),
        ));
        for (mode, got) in [
            (
                "in process",
                edge.rt().block_on(local.recall_json("tokio", k)),
            ),
            ("daemon", edge.rt().block_on(daemon.recall_json("tokio", k))),
        ] {
            if k <= MAX_RECALL_K {
                let body = got.map_err(|e| TestCaseError::fail(format!("{mode}: {e:?}")))?;
                let n = ids(&body)?.len();
                prop_assert!(n as u32 <= k, "{}: {} results for k={}", mode, n, k);
            } else {
                prop_assert_eq!(
                    got,
                    Err(MemoryStoreError::InvalidParams(k_too_large_message())),
                    "{}",
                    mode
                );
            }
        }
        if k <= MAX_RECALL_K {
            prop_assert_eq!(status, StatusCode::OK, "HTTP k={}: {}", k, body);
            let n = body["results"].as_array().map_or(usize::MAX, Vec::len);
            prop_assert!(n as u64 <= u64::from(k), "HTTP: {} results for k={}", n, k);
        } else {
            prop_assert_eq!(status, StatusCode::BAD_REQUEST, "HTTP k={}: {}", k, body);
            let message = k_too_large_message();
            prop_assert_eq!(body["error"].as_str(), Some(message.as_str()));
        }
        let elapsed = start.elapsed();
        prop_assert!(
            elapsed < Duration::from_secs(1),
            "k={} took {:?}",
            k,
            elapsed
        );
        Ok(())
    });
}

/// 3 (DRM-110; LP-READ-10): on a store of exact ties, `recall(k)` is a prefix
/// of `recall(k + 1)`, in both modes.
#[test]
fn recall_k_is_a_prefix_of_k_plus_one_drm110() {
    const GROUPS: [&str; 4] = [
        "tokio runtime",
        "tokio axum handler",
        "axum runtime error",
        "tokio",
    ];
    const WORDS: [&str; 5] = ["tokio", "runtime", "axum", "handler", "error"];
    let edge = Edge::new();
    let in_root = edge.project("t_in");
    let dm_root = edge.project("t_dm");
    // Four groups of ten. Within a group, content, scores and timestamp are
    // the same, so their scores tie exactly. Ids interleave the groups.
    let log: Vec<serde_json::Value> = (0..40)
        .map(|n| {
            let g = n % 4;
            record(
                n,
                GROUPS[g],
                "rust::ties",
                g as i64 * 3,
                [3.0, 6.0, 9.0, 6.0][g],
                [9.0, 6.0, 3.0, 6.0][g],
            )
        })
        .collect();
    seed_log(&in_root, &log);
    seed_log(&dm_root, &log);
    let local = in_process_reader(&in_root);
    let daemon = edge.daemon(&dm_root);
    assert_seeded(
        &edge,
        [("in process", &local), ("daemon", &daemon)],
        "tokio",
        30,
    );

    check(
        config(32),
        (0..MAX_RECALL_K, prop::sample::select(WORDS.to_vec())),
        |(k, word)| {
            for (mode, store) in [
                ("in process", &local as &dyn MemoryStore),
                ("daemon", &daemon as &dyn MemoryStore),
            ] {
                let at = |k: u32| -> Result<Vec<String>, TestCaseError> {
                    let body = edge
                        .rt()
                        .block_on(store.recall_json(word, k))
                        .map_err(|e| TestCaseError::fail(format!("{mode}: {e:?}")))?;
                    ids(&body)
                };
                let (shorter, longer) = (at(k)?, at(k + 1)?);
                prop_assert!(
                    longer.starts_with(&shorter),
                    "{}: k={} {:?} is not a prefix of {:?}",
                    mode,
                    k,
                    shorter,
                    longer
                );
            }
            Ok(())
        },
    );
}

/// 4 (DRM-102, DRM-161; LP-HTTP-03, LP-MCP-03, LP-MCP-02): a learning lands
/// in the root its header names, and a header that is not an absolute path is
/// a 400 that writes nothing. A root whose last folder name ends in a space
/// or tab is refused by the client as `InvalidRequest` and writes nothing:
/// HTTP would drop that character from the header.
#[test]
fn a_learning_lands_in_the_root_it_names_drm102() {
    let edge = Edge::new();
    let a = edge.project("fixed/a");
    let b = edge.project("fixed/b");
    let relative = prop_oneof![dir_name(), Just(String::new()), Just("./x".to_owned())];

    check(config(16), (dir_name(), relative), |(name, rel)| {
        let target = edge.project(&format!("drawn/{name}"));
        let before = [
            log_lines(&target).len(),
            log_lines(&a).len(),
            log_lines(&b).len(),
        ];
        let appended = edge
            .rt()
            .block_on(edge.daemon(&target).append(AppendRequest {
                content: format!("lands in {name}"),
                source_harness: "edge-proptest".to_owned(),
                skill_action: "edge::root".to_owned(),
                pain: Some(5.0),
                importance: Some(5.0),
                client_dedup_key: None,
                pinned: false,
            }));
        let after = [
            log_lines(&target).len(),
            log_lines(&a).len(),
            log_lines(&b).len(),
        ];
        if name.ends_with([' ', '\t']) {
            // The client's own refusal. A daemon's 404 for the root without
            // the space is `InvalidRequest` too, so the kind alone would pass
            // with no check at all.
            prop_assert!(
                matches!(
                    &appended,
                    Err(MemoryStoreError::InvalidRequest(m)) if m.contains("ends in a space or tab")
                ),
                "append to {:?}: {:?}",
                target,
                appended
            );
            prop_assert_eq!(after, before, "no line in {:?}, a or b", target);
        } else {
            prop_assert!(appended.is_ok(), "append to {:?}: {:?}", target, appended);
            prop_assert_eq!(
                after,
                [before[0] + 1, before[1], before[2]],
                "one line in {:?}, none in a or b",
                target
            );
        }

        let header = HeaderValue::from_bytes(rel.as_bytes())
            .map_err(|e| TestCaseError::fail(format!("{rel:?} is no header: {e}")))?;
        let logs = [log_lines(&target), log_lines(&a), log_lines(&b)];
        let (status, body) = edge.send(request(
            Method::POST,
            "/api/v1/learn",
            Some(header),
            &[],
            learn_body("a relative root"),
        ));
        prop_assert_eq!(status, StatusCode::BAD_REQUEST, "{:?}: {}", rel, body);
        let error = body["error"].as_str().unwrap_or_default();
        prop_assert!(error.starts_with("X-Agent-Root"), "{:?}: {}", rel, error);
        prop_assert_eq!(
            [log_lines(&target), log_lines(&a), log_lines(&b)],
            logs,
            "no log changes"
        );
        Ok(())
    });
}

/// DRM-161, found by property 4: a root whose last folder name ends in a
/// space or a tab. HTTP drops that character from `X-Agent-Root`, so before
/// the fix a daemon-mode append for `x ` was written to its registered
/// sibling `x`. The client now refuses such a root as `InvalidRequest`
/// before it sends anything, so neither `x` nor the spaced root gets a line.
#[test]
fn a_root_ending_in_a_space_is_not_aliased_drm161() {
    let edge = Edge::new();
    let sibling = edge.project("drm161/x");
    for name in ["drm161/x ", "drm161/x\t"] {
        let spaced = edge.project(name);
        let appended = edge
            .rt()
            .block_on(edge.daemon(&spaced).append(AppendRequest {
                content: format!("meant for {name:?}"),
                source_harness: "edge-proptest".to_owned(),
                skill_action: "edge::root".to_owned(),
                pain: Some(5.0),
                importance: Some(5.0),
                client_dedup_key: None,
                pinned: false,
            }));
        assert!(
            matches!(appended, Err(MemoryStoreError::InvalidRequest(_))),
            "{name:?}: expected InvalidRequest, got {appended:?}"
        );
        assert_eq!(
            log_lines(&sibling),
            Vec::<String>::new(),
            "{name:?}: nothing in `x`"
        );
        assert_eq!(
            log_lines(&spaced),
            Vec::<String>::new(),
            "{name:?}: nothing in {spaced:?}"
        );
    }
}

/// 5 (DRM-102, DRM-106; LP-MCP-02, LP-MCP-05): any header bytes get a 2xx or
/// a 4xx, as `X-Agent-Root` or as `X-Client-Dedup-Key`, and the daemon keeps
/// answering.
#[test]
fn any_header_bytes_get_an_answer_drm102_drm106() {
    let edge = Edge::new();
    let valid = edge.project("h_valid");

    check(config(128), header_bytes(), |bytes| {
        let value = HeaderValue::from_bytes(&bytes).expect("filtered");
        for (what, req) in [
            (
                "X-Agent-Root",
                request(
                    Method::POST,
                    "/api/v1/learn",
                    Some(value.clone()),
                    &[],
                    learn_body("root"),
                ),
            ),
            (
                "X-Client-Dedup-Key",
                request(
                    Method::POST,
                    "/api/v1/learn",
                    Some(root_header(&valid)),
                    &[("x-client-dedup-key", value.clone())],
                    learn_body("dedup"),
                ),
            ),
        ] {
            let (status, body) = edge.send(req);
            prop_assert!(
                status.is_success() || status.is_client_error(),
                "{} {:?}: {} {}",
                what,
                bytes,
                status,
                body
            );
        }
        let (status, body) = edge.send(request(
            Method::GET,
            "/api/v1/health",
            Some(root_header(&valid)),
            &[],
            Vec::new(),
        ));
        prop_assert_eq!(status, StatusCode::OK, "health: {}", body);
        Ok(())
    });
}

/// 6 (DRM-104; LP-MCP-10, LP-HTTP-13): both modes store or refuse alike. The same `Ok` or
/// `Err`, the same variant and message, and on `Ok` the same stored fields.
#[test]
fn both_modes_store_or_refuse_alike_drm104() {
    let edge = Edge::new();
    let in_root = edge.project("p_in");
    let dm_root = edge.project("p_dm");
    // In process: this process holds `p_in`'s writer lock and runs its
    // coordinator, as `dreamd mcp` does when no daemon is live.
    let local = edge.rt().block_on(async {
        let root = AgentRoot::new(&in_root);
        let lock = edge.lock(&root, WriterKind::Mcp);
        let supervisor = Supervisor::start(&root, &lock, COORDINATOR_CHANNEL_CAPACITY, None)
            .expect("p_in coordinator");
        let store = InProcessStore::new(
            Some(root),
            Some(supervisor.sender()),
            None,
            Arc::new(Mutex::new(ProjectIndexMap::with_defaults())),
        );
        (store, supervisor)
    });
    let daemon = edge.daemon(&dm_root);
    let content = prop_oneof![
        8 => "\\PC{0,80}",
        1 => Just(String::new()),
        1 => Just("x".repeat(5000)),
    ];

    check(
        config(64),
        (content, skill_action(), score(), score(), any::<bool>()),
        |(content, key, pain, importance, pinned)| {
            let req = AppendRequest {
                content: content.clone(),
                source_harness: "edge-proptest".to_owned(),
                skill_action: key.clone(),
                pain: Some(pain),
                importance: Some(importance),
                client_dedup_key: None,
                pinned,
            };
            let a = edge.rt().block_on(local.0.append(req.clone()));
            let b = edge.rt().block_on(daemon.append(req));
            match (&a, &b) {
                (Ok(_), Ok(_)) => {
                    let last = |root: &Path| -> Result<serde_json::Value, TestCaseError> {
                        let lines = log_lines(root);
                        let line = lines
                            .last()
                            .ok_or_else(|| TestCaseError::fail("an empty log after an Ok"))?;
                        serde_json::from_str(line)
                            .map_err(|e| TestCaseError::fail(format!("{e}: {line}")))
                    };
                    let (x, y) = (last(&in_root)?, last(&dm_root)?);
                    for field in [
                        "content",
                        "skill_action",
                        "pain",
                        "importance",
                        "pinned",
                        "source_harness",
                    ] {
                        prop_assert_eq!(&x[field], &y[field], "field {}", field);
                    }
                }
                (Err(x), Err(y)) => prop_assert_eq!(x, y),
                _ => prop_assert!(false, "in process {:?}, daemon {:?}", a, b),
            }
            Ok(())
        },
    );
    edge.rt().block_on(local.1.shutdown());
}
