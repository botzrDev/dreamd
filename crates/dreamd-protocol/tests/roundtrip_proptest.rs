//! Property tests proving JSONL round-trip equivalence for arbitrary
//! [`AgentLearning`] records (WEG-159 / DR-802).
//!
//! The property under test is simple and load-bearing for the on-disk format:
//! for any structurally-valid learning, `to_string` → `from_str` yields an
//! equal struct, and the serialized form is a single newline-free JSONL line.
//!
//! This is a **pure serde round-trip** (`serde_json::to_string` →
//! `serde_json::from_str`). The coordinator's 4 KiB per-line append cap lives in
//! `dreamd-core` and does **not** apply here, so `content` is exercised well
//! past 4 KiB on purpose.
//!
//! Field strategies match the ground-truth struct at
//! `dreamd-protocol/src/lib.rs` (9 fields). Note the ticket's AC field list is
//! drifted: it names a nonexistent `layer` field and omits the required
//! `source_harness` — this suite follows the struct, not the AC.

use chrono::{DateTime, TimeZone, Utc};
use dreamd_protocol::{AgentLearning, EventId};
use proptest::prelude::*;

/// Crockford base32 alphabet — 32 uppercase chars, excluding I, L, O, U.
/// Mirrors `CROCKFORD_ALPHABET` in the crate; any 26 of these parse as a valid
/// `EventId` suffix (`EventId::parse` checks length + alphabet, not real ULID
/// timestamp semantics).
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Generate a valid `EventId`: `evt_` + exactly 26 Crockford base32 chars.
fn event_id_strategy() -> impl Strategy<Value = EventId> {
    proptest::collection::vec(0u8..32, 26).prop_map(|indices| {
        let mut raw = String::from("evt_");
        for i in indices {
            raw.push(char::from(CROCKFORD[i as usize]));
        }
        EventId::parse(&raw).expect("generated id must satisfy EventId::parse")
    })
}

/// Timestamps bounded to a sane 1970..2100 range with sub-second nanos.
///
/// Nanos are kept in `0..1_000_000_000` (no leap seconds) so the instant
/// survives RFC 3339 serialization exactly and `assert_eq!` on the whole struct
/// holds without a normalization carve-out.
fn timestamp_strategy() -> impl Strategy<Value = DateTime<Utc>> {
    // 0 = 1970-01-01T00:00:00Z, 4_102_444_800 ≈ 2100-01-01T00:00:00Z.
    (0i64..4_102_444_800i64, 0u32..1_000_000_000u32).prop_map(|(secs, nanos)| {
        Utc.timestamp_opt(secs, nanos)
            .single()
            .expect("bounded secs/nanos yield a unique instant")
    })
}

/// Realistic clustering keys plus arbitrary strings. `skill_action` is a plain
/// `String` on the struct (validation is ingress-only), so any string must
/// round-trip; the regex arm keeps typical `rust::borrow_checker` shapes in the
/// sample, the `any` arm stresses the codec.
fn skill_action_strategy() -> impl Strategy<Value = String> {
    prop_oneof!["[a-z0-9_]{1,16}(::[a-z0-9_]{1,16}){0,3}", any::<String>(),]
}

/// Known harness ids weighted alongside arbitrary strings.
fn source_harness_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("claude-code".to_string()),
        Just("cursor".to_string()),
        Just("cline".to_string()),
        "[a-z][a-z0-9-]{0,15}",
        any::<String>(),
    ]
}

/// `"1.0.0"` (the only v0.1 value) plus arbitrary semver-ish and free strings.
fn schema_version_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("1.0.0".to_string()),
        "[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}",
        any::<String>(),
    ]
}

/// Content strategy that deliberately hits the encoding edge cases called out
/// in the AC: empty, arbitrary Unicode (incl. control chars), embedded
/// newlines, and very long bodies (past the 4 KiB append cap — which does not
/// apply to a pure serde round-trip).
fn content_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        // Empty.
        Just(String::new()),
        // Arbitrary Unicode scalar values, including control chars.
        any::<String>(),
        // Embedded newlines — serde_json escapes these as `\n`, so the JSONL
        // line stays newline-free and framing holds.
        "(line [0-9]{1,4}\n){0,64}",
        // Very long body: up to ~4096 scalar values (multi-byte chars push the
        // UTF-8 length well beyond 4 KiB).
        proptest::collection::vec(any::<char>(), 0..4096)
            .prop_map(|chars| chars.into_iter().collect::<String>()),
    ]
}

/// Compose an arbitrary, structurally-valid `AgentLearning`.
fn agent_learning_strategy() -> impl Strategy<Value = AgentLearning> {
    (
        schema_version_strategy(),
        event_id_strategy(),
        timestamp_strategy(),
        0.0f32..=10.0f32, // pain: finite, in salience range
        0.0f32..=10.0f32, // importance: finite, in salience range
        any::<bool>(),    // pinned
        skill_action_strategy(),
        source_harness_strategy(),
        content_strategy(),
    )
        .prop_map(
            |(
                schema_version,
                id,
                timestamp,
                pain,
                importance,
                pinned,
                skill_action,
                source_harness,
                content,
            )| {
                AgentLearning {
                    schema_version,
                    id,
                    timestamp,
                    pain,
                    importance,
                    pinned,
                    skill_action,
                    source_harness,
                    content,
                }
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// The core property: serialize → parse → assert structural equality.
    ///
    /// `AgentLearning` derives `PartialEq`, and the bounded timestamp means the
    /// parsed-back instant equals the original, so we assert on the whole struct
    /// directly (no field-by-field carve-out).
    #[test]
    fn agent_learning_jsonl_round_trip(learning in agent_learning_strategy()) {
        let line = serde_json::to_string(&learning).expect("serialize to JSONL");

        // JSONL framing invariant: one record == one newline-free line. serde
        // escapes any interior `\n` in string fields, so this holds even when
        // `content` contains raw newlines.
        prop_assert!(
            !line.contains('\n'),
            "serialized JSONL line must not contain a raw newline: {line:?}"
        );

        let parsed: AgentLearning =
            serde_json::from_str(&line).expect("parse the JSONL line back");

        prop_assert_eq!(parsed, learning);
    }
}
