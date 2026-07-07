//! Property and corpus tests for the episodic JSONL scan path (WEG-132 / DR-812).
//!
//! Exercises [`dreamd_core::episodic::assess_bytes`] with arbitrary byte
//! sequences and a fixed corpus of hand-edit failure modes: BOM prefixes,
//! missing trailing newlines, embedded NULs, invalid UTF-8, mid-line truncation,
//! trailing whitespace, and blank lines.
//!
//! Properties under test:
//! - `assess_bytes` never panics on any input.
//! - `clean_len` invariant: torn tail (if any) contains no `\n`.
//! - Mid-file `\n`-terminated garbage is counted as malformed, not ingested.

use chrono::DateTime;
use dreamd_core::episodic::{assess_bytes, EpisodicLogHealth};
use dreamd_protocol::{AgentLearning, EventId};
use proptest::prelude::*;

const NOW_SEC: i64 = 1751500800;
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

fn make_learning(n: u8, content: &str) -> AgentLearning {
    let c = char::from(CROCKFORD[n as usize % 32]);
    let raw = format!("evt_01ARZ3NDEKTSV4RRFFQ69G5FA{c}");
    AgentLearning {
        schema_version: "1.0.0".to_string(),
        id: EventId::parse(&raw).expect("valid EventId"),
        timestamp: DateTime::from_timestamp(NOW_SEC, 0).expect("valid ts"),
        pain: 5.0,
        importance: 6.0,
        pinned: false,
        skill_action: "rust::episodic".to_string(),
        source_harness: "test-harness".to_string(),
        content: content.to_string(),
    }
}

fn line_of(l: &AgentLearning) -> Vec<u8> {
    let mut s = serde_json::to_string(l).unwrap();
    s.push('\n');
    s.into_bytes()
}

/// Recompute `clean_len` from scan invariants for property checks.
fn clean_len_from_health(bytes: &[u8], health: &EpisodicLogHealth) -> usize {
    bytes.len() - health.torn_tail_bytes as usize
}

// ── Corpus: AC-mandated failure modes ────────────────────────────────────────

#[test]
fn corpus_bom_prefixed_line_is_malformed() {
    let good = make_learning(0, "kept");
    let mut bytes = line_of(&good);
    // UTF-8 BOM before a second (invalid) line.
    bytes.extend_from_slice(b"\xEF\xBB\xBF{not json}\n");
    let health = assess_bytes(&bytes);
    assert_eq!(health.valid_record_count, 1);
    assert_eq!(health.malformed_line_count, 1);
    assert_eq!(health.torn_tail_bytes, 0);
}

#[test]
fn corpus_missing_trailing_newline_halts_at_torn_tail() {
    let good = make_learning(1, "kept");
    let mut bytes = line_of(&good);
    bytes.extend_from_slice(b"{\"schema_version\":\"1.0.0\",\"id\":\"evt_TRUNC");
    let health = assess_bytes(&bytes);
    assert_eq!(health.valid_record_count, 1);
    assert_eq!(health.malformed_line_count, 0);
    assert!(health.torn_tail_bytes > 0);
}

#[test]
fn corpus_embedded_nul_midfile_is_malformed() {
    let good = make_learning(2, "before");
    let later = make_learning(3, "after");
    let mut bytes = line_of(&good);
    bytes.extend_from_slice(b"{\"id\":\"evt_NUL\0BAD\"}\n");
    bytes.extend_from_slice(&line_of(&later));
    let health = assess_bytes(&bytes);
    assert_eq!(health.valid_record_count, 2);
    assert_eq!(health.malformed_line_count, 1);
}

#[test]
fn corpus_invalid_utf8_midfile_is_malformed() {
    let good = make_learning(4, "before");
    let later = make_learning(5, "after");
    let mut bytes = line_of(&good);
    bytes.extend_from_slice(b"\xFF\xFE{not utf8}\n");
    bytes.extend_from_slice(&line_of(&later));
    let health = assess_bytes(&bytes);
    assert_eq!(health.valid_record_count, 2);
    assert_eq!(health.malformed_line_count, 1);
}

#[test]
fn corpus_truncated_mid_line_is_torn_tail() {
    let good = make_learning(6, "kept");
    let mut bytes = line_of(&good);
    bytes.extend_from_slice(b"{\"schema_version\":\"1.0.0\",\"id\":\"evt_");
    let health = assess_bytes(&bytes);
    assert_eq!(health.valid_record_count, 1);
    assert_eq!(health.malformed_line_count, 0);
    assert!(health.torn_tail_bytes > 0);
}

#[test]
fn corpus_trailing_whitespace_on_json_is_malformed() {
    let good = make_learning(7, "before");
    let later = make_learning(8, "after");
    let mut bytes = line_of(&good);
    bytes.extend_from_slice(b"{not valid json}   \n");
    bytes.extend_from_slice(&line_of(&later));
    let health = assess_bytes(&bytes);
    assert_eq!(health.valid_record_count, 2);
    assert_eq!(health.malformed_line_count, 1);
}

#[test]
fn corpus_blank_line_is_malformed() {
    let good = make_learning(9, "before");
    let later = make_learning(10, "after");
    let mut bytes = line_of(&good);
    bytes.push(b'\n'); // blank line
    bytes.extend_from_slice(&line_of(&later));
    let health = assess_bytes(&bytes);
    assert_eq!(health.valid_record_count, 2);
    assert_eq!(health.malformed_line_count, 1);
}

#[test]
fn corpus_leading_whitespace_on_valid_json_is_accepted() {
    // serde_json accepts leading whitespace — this is not treated as malformed.
    let good = make_learning(11, "before");
    let later = make_learning(12, "after");
    let mut bytes = line_of(&good);
    let mut spaced = line_of(&later);
    spaced.insert(0, b' ');
    bytes.extend_from_slice(&spaced);
    let health = assess_bytes(&bytes);
    assert_eq!(health.valid_record_count, 2);
    assert_eq!(health.malformed_line_count, 0);
}

// ── Strategies ──────────────────────────────────────────────────────────────

fn valid_line_strategy() -> impl Strategy<Value = Vec<u8>> {
    (any::<u8>(), any::<String>()).prop_map(|(n, content)| line_of(&make_learning(n, &content)))
}

fn jsonl_bytes_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        // Pure arbitrary bytes — the fuzz backbone.
        prop::collection::vec(any::<u8>(), 0..16_384),
        // Mix of valid lines and arbitrary line fragments.
        (
            prop::collection::vec(valid_line_strategy(), 0..8),
            prop::collection::vec(any::<u8>(), 0..512),
        )
            .prop_map(|(valid, tail)| {
                let mut bytes: Vec<u8> = valid.concat();
                bytes.extend(tail);
                bytes
            }),
        // Lines built from printable ASCII with forced newlines sprinkled in.
        prop::collection::vec(any::<u8>(), 0..4096).prop_map(|mut bytes| {
            for i in (0..bytes.len()).step_by(64) {
                bytes[i] = b'\n';
            }
            bytes
        }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Core property: assess_bytes never panics and torn tail has no `\n`.
    #[test]
    fn scan_never_panics_and_torn_tail_has_no_newline(bytes in jsonl_bytes_strategy()) {
        let health = assess_bytes(&bytes);
        let clean_len = clean_len_from_health(&bytes, &health);

        // clean_len must not exceed file size.
        prop_assert!(clean_len <= bytes.len());

        // Torn tail (if any) must not contain a newline — otherwise scan should
        // have consumed it as a complete line.
        if health.torn_tail_bytes > 0 {
            let tail = &bytes[clean_len..];
            prop_assert!(
                !tail.contains(&b'\n'),
                "torn tail must not contain \\n; tail={tail:?}"
            );
        }

        // Counts are non-negative and bounded.
        prop_assert!(health.valid_record_count <= bytes.len());
        prop_assert!(health.malformed_line_count <= bytes.len());
    }

    /// Valid lines interleaved with garbage: valid records survive, garbage is
    /// counted as malformed (not silently ingested).
    #[test]
    fn valid_lines_survive_interleaved_garbage(
        good_count in 1usize..4,
        garbage in prop::collection::vec(0u8..255u8, 1..128)
            .prop_filter("no embedded newlines", |g| !g.contains(&b'\n')),
    ) {
        let mut bytes = Vec::new();
        for i in 0..good_count {
            bytes.extend_from_slice(&line_of(&make_learning(i as u8, "ok")));
            if i + 1 < good_count {
                let mut bad = garbage.clone();
                bad.push(b'\n');
                bytes.extend_from_slice(&bad);
            }
        }

        let health = assess_bytes(&bytes);
        prop_assert_eq!(health.valid_record_count, good_count);
        if good_count > 1 {
            prop_assert_eq!(health.malformed_line_count, good_count - 1);
        }
    }
}
