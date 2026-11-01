//! BZR-1303: CLI text output prints control characters as their Unicode
//! control pictures, and `--json` output is unchanged.
//!
//! Subprocess tests in the shape of `dream_doctor_discover.rs` (raw
//! `std::process::Command` + `CARGO_BIN_EXE_dreamd`). The store is two
//! hand-written log lines; `dreamd doctor --repair` builds the index from them.
//! Unix-only: the repair takes the project writer lock.
#![cfg(unix)]

use std::process::Command;

/// Stored content holding NUL, ESC, U+001F, DEL, a tab and a `|`.
const CONTROL_CONTENT: &str =
    "needle NUL\u{0} ESC\u{1b}[31mred US\u{1f} DEL\u{7f} tab\there pipe | end";
const ID_CONTENT: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FA1";
const ID_SKILL: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FA2";
const CONTROL_SKILL: &str = "rust::nul\u{0}key";

/// A project and a separate daemon home, both temporary.
struct Store {
    project: tempfile::TempDir,
    home: tempfile::TempDir,
}

fn dreamd(store: &Store, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_dreamd"))
        .args(args)
        .current_dir(store.project.path())
        .env("HOME", store.home.path())
        .env("DREAMD_LOG", "off")
        .output()
        .unwrap_or_else(|e| panic!("run dreamd {args:?}: {e}"))
}

fn record(id: &str, skill_action: &str, content: &str, score: f64, ts: &str) -> String {
    let mut line = serde_json::json!({
        "schema_version": "1.0.0",
        "id": id,
        "timestamp": ts,
        "pain": score,
        "importance": score,
        "skill_action": skill_action,
        "source_harness": "claude-code",
        "content": content,
    })
    .to_string();
    line.push('\n');
    line
}

/// Two log lines, one with control characters in `content` and one in
/// `skill_action`, then `dreamd doctor --repair` to build the index.
fn seed() -> Store {
    let store = Store {
        project: tempfile::tempdir().unwrap(),
        home: tempfile::tempdir().unwrap(),
    };
    let episodic = store.project.path().join(".agent/episodic");
    std::fs::create_dir_all(&episodic).unwrap();
    let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let log = record(ID_CONTENT, "rust::needle", CONTROL_CONTENT, 8.0, &ts)
        + &record(ID_SKILL, CONTROL_SKILL, "needle second record", 7.0, &ts);
    std::fs::write(episodic.join("AGENT_LEARNINGS.jsonl"), log).unwrap();

    // Other doctor checks warn on a bare store, so the exit code is not asserted.
    let out = dreamd(&store, &["doctor", "--repair"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("repair: rebuilt index from the episodic log"),
        "doctor --repair did not build the index; stdout={stdout}\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(store.project.path().join(".agent/.dreamd/index").is_dir());
    store
}

/// Run `args`, require exit 0, and return stdout.
fn stdout_of(store: &Store, args: &[&str]) -> Vec<u8> {
    let out = dreamd(store, args);
    assert_eq!(
        out.status.code(),
        Some(0),
        "dreamd {args:?} must exit 0; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[test]
fn text_outputs_print_control_pictures() {
    let store = seed();
    for args in [
        &["recall", "needle"][..],
        &["recall", "needle", "--explain"],
        &["score"],
        &["score", "--explain"],
        &["blame", "needle"],
        &["blame", "needle", "--explain"],
        &["salience-drift"],
    ] {
        let bytes = stdout_of(&store, args);
        let raw: Vec<u8> = bytes
            .iter()
            .copied()
            .filter(|&b| (b < 0x20 && b != b'\n') || b == 0x7f)
            .collect();
        assert!(
            raw.is_empty(),
            "dreamd {args:?} printed control bytes {raw:02x?}"
        );
        let text = String::from_utf8(bytes).expect("stdout is UTF-8");
        assert!(
            text.contains('\u{2400}'),
            "dreamd {args:?}: no ␀ in {text:?}"
        );
        if matches!(args[0], "recall" | "score" | "blame") {
            for picture in ['\u{241b}', '\u{241f}', '\u{2421}'] {
                assert!(
                    text.contains(picture),
                    "dreamd {args:?}: no {picture} in {text:?}"
                );
            }
        }
        if matches!(args[0], "blame" | "salience-drift") {
            assert!(
                text.contains("rust::nul\u{2400}key"),
                "dreamd {args:?}: skill_action not escaped in {text:?}"
            );
        }
    }
}

/// Passes on the base too: JSON escapes U+0000–U+001F itself, DEL stays raw,
/// and `blame --json`'s `content` is still `truncate_content`'s text.
#[test]
fn json_outputs_are_unchanged() {
    let store = seed();
    let pictures = ['\u{2400}', '\u{241b}', '\u{241f}', '\u{2421}'];

    let blame = stdout_of(&store, &["blame", "needle", "--json"]);
    let has = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).any(|w| w == needle);
    assert!(has(&blame, br"\u0000"), "no \\u0000 escape");
    assert!(has(&blame, br"\u001b"), "no \\u001b escape");
    assert!(blame.contains(&0x7f), "DEL is written raw");
    let text = String::from_utf8(blame).expect("stdout is UTF-8");
    assert!(
        !text.contains(pictures),
        "no control picture in JSON: {text:?}"
    );
    let hits: serde_json::Value = serde_json::from_str(text.trim_end()).unwrap();
    let hits = hits.as_array().expect("one JSON array");
    assert_eq!(hits.len(), 2, "both records match: {text}");
    let by_id = |id: &str| {
        hits.iter()
            .find(|h| h["id"] == id)
            .unwrap_or_else(|| panic!("no hit {id} in {text}"))
    };
    assert_eq!(
        by_id(ID_CONTENT)["content"],
        dreamd::commands::recall::truncate_content(CONTROL_CONTENT, 80)
    );
    assert_eq!(by_id(ID_SKILL)["skill_action"], CONTROL_SKILL);

    let drift = String::from_utf8(stdout_of(&store, &["salience-drift", "--json"])).unwrap();
    assert!(drift.contains(r"rust::nul\u0000key"), "got: {drift}");
    assert!(
        !drift.contains(pictures),
        "no control picture in JSON: {drift:?}"
    );
}
