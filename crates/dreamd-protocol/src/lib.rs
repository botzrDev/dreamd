//! Shared serde types for the dreamd HTTP API and on-disk JSONL records.
//!
//! This crate is the **parse/validate boundary** for wire and disk shapes:
//! [`EventId`], [`SkillAction`], and [`AgentLearning`]. It intentionally
//! depends only on `serde` and `chrono` — ULID minting and coordinator logic
//! live in `dreamd-core`.

#![deny(missing_docs)]

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Crockford base32 alphabet, the canonical ULID alphabet.
/// 32 chars, uppercase, excluding I, L, O, U.
const CROCKFORD_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Daemon-assigned event identifier. Wraps `evt_` + a 26-char Crockford
/// base32 ULID (canonical uppercase form). Constructed only via [`parse`].
///
/// `dreamd-protocol` does not depend on `ulid`; minting happens in
/// `dreamd-core` and rides in through `parse`. The newtype keeps malformed
/// ids unrepresentable across the wire and on disk.
///
/// [`parse`]: EventId::parse
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EventId(String);

/// Reason an [`EventId::parse`] call rejected its input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventIdParseError {
    reason: &'static str,
}

impl fmt::Display for EventIdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid EventId: {}", self.reason)
    }
}

impl std::error::Error for EventIdParseError {}

impl EventId {
    /// Validate and wrap an `evt_<26-char Crockford ULID>` string.
    pub fn parse(s: &str) -> Result<Self, EventIdParseError> {
        let suffix = s.strip_prefix("evt_").ok_or(EventIdParseError {
            reason: "missing 'evt_' prefix",
        })?;
        let bytes = suffix.as_bytes();
        if bytes.len() != 26 {
            return Err(EventIdParseError {
                reason: "ULID suffix must be exactly 26 chars",
            });
        }
        for &b in bytes {
            if !CROCKFORD_ALPHABET.contains(&b) {
                return Err(EventIdParseError {
                    reason: "ULID suffix contains non-Crockford char",
                });
            }
        }
        Ok(Self(s.to_owned()))
    }

    /// Borrow the canonical `evt_<ULID>` string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for EventId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for EventId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        EventId::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// Schema version stamped onto every persisted episodic record.
pub const RECORD_SCHEMA_VERSION: &str = "1.0.0";

/// Validated clustering key: lowercase `[a-z0-9_]` segments joined by `::`,
/// e.g. `rust::borrow_checker`. The dream cycle splits on `::` to sub-cluster.
///
/// Unlike [`EventId`], this is a **parse-only boundary validator** — it is NOT
/// a serde field on [`AgentLearning`] (that field stays `String` so existing
/// dotted records and hand-edited files read back without error). Construct via
/// [`parse`] at API ingress, then store the inner `String`.
///
/// [`parse`]: SkillAction::parse
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillAction(String);

/// Reason a [`SkillAction::parse`] call rejected its input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillActionParseError {
    reason: &'static str,
}

impl fmt::Display for SkillActionParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid skill_action: {}", self.reason)
    }
}

impl std::error::Error for SkillActionParseError {}

impl SkillAction {
    /// Normalise then validate: trim → lowercase → collapse interior
    /// whitespace → spaces→`_` → split on `::` → each segment non-empty and
    /// `[a-z0-9_]+` → rejoin with `::`. Total ≤ 256 bytes.
    pub fn parse(s: &str) -> Result<Self, SkillActionParseError> {
        let lowercased = s.trim().to_lowercase();
        let normalised: String = lowercased.split_whitespace().collect::<Vec<_>>().join("_");
        if normalised.is_empty() {
            return Err(SkillActionParseError {
                reason: "empty after normalisation",
            });
        }
        if normalised.len() > 256 {
            return Err(SkillActionParseError {
                reason: "exceeds 256 bytes",
            });
        }
        for seg in normalised.split("::") {
            if seg.is_empty() {
                return Err(SkillActionParseError {
                    reason: "empty segment (stray ':' or leading/trailing '::')",
                });
            }
            if !seg
                .bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
            {
                return Err(SkillActionParseError {
                    reason: "segment contains characters outside [a-z0-9_]",
                });
            }
        }
        Ok(Self(normalised))
    }

    /// Borrow the validated, normalized clustering key.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume and return the inner `String` for storage on [`AgentLearning`].
    pub fn into_string(self) -> String {
        self.0
    }
}

/// Central episodic event record written to `AGENT_LEARNINGS.jsonl`.
///
/// Serialized as one JSON line per entry. The coordinator mints the [`EventId`]
/// at write time; any `id` on an inbound learning is overwritten.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AgentLearning {
    /// Forward-compatible schema tag. Always `"1.0.0"` in v0.1; bump requires
    /// a `dreamd migrate` path.
    pub schema_version: String,
    /// Daemon-assigned event identifier (`evt_` + 26-char ULID). Overwritten
    /// by the coordinator on every append; callers may pass a placeholder.
    pub id: EventId,
    /// ISO 8601 / RFC 3339 timestamp of when the learning was captured.
    pub timestamp: DateTime<Utc>,
    /// 0.0..=10.0 subjective friction score from the agent's perspective.
    /// Higher values surface the learning more aggressively in recall.
    pub pain: f32,
    /// 0.0..=10.0 estimated long-term relevance. Feeds the salience formula
    /// together with `pain` and age decay.
    pub importance: f32,
    /// Sticky flag: pinned learnings survive dream-cycle pruning.
    #[serde(default)]
    pub pinned: bool,
    /// Validated clustering key (e.g., `"rust::borrow_checker"`). Validated at
    /// ingress via [`SkillAction`]; stored as `String` for backward compatibility.
    pub skill_action: String,
    /// Which AI harness produced this learning (e.g., `"claude-code"`,
    /// `"cursor"`, `"cline"`). Used for provenance and cross-harness dedup.
    pub source_harness: String,
    /// Free-text body of the learning. Subject to the 4 KiB per-line hard cap
    /// enforced by the coordinator.
    pub content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Canonical ULID example from the ULID spec — all uppercase Crockford.
    const SAMPLE_ULID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    #[test]
    fn event_id_parse_round_trip() {
        let raw = format!("evt_{SAMPLE_ULID}");
        let id = EventId::parse(&raw).expect("parse");
        assert_eq!(id.as_str(), raw);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{raw}\""));
        let back: EventId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn event_id_rejects_missing_prefix() {
        assert!(EventId::parse(SAMPLE_ULID).is_err());
    }

    #[test]
    fn event_id_rejects_bad_length() {
        assert!(EventId::parse("evt_TOOSHORT").is_err());
        assert!(EventId::parse(&format!("evt_{SAMPLE_ULID}X")).is_err());
    }

    #[test]
    fn event_id_rejects_non_crockford_chars() {
        assert!(EventId::parse("evt_01arz3ndektsv4rrffq69g5fav").is_err());
        assert!(EventId::parse("evt_IIIIIIIIIIIIIIIIIIIIIIIIII").is_err());
    }

    #[test]
    fn event_id_deserialize_rejects_invalid() {
        let bad = "\"evt_not-a-ulid\"";
        let r: Result<EventId, _> = serde_json::from_str(bad);
        assert!(r.is_err());
    }

    #[test]
    fn agent_learning_json_round_trip() {
        let id = EventId::parse(&format!("evt_{SAMPLE_ULID}")).unwrap();
        let original = AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: id.clone(),
            timestamp: DateTime::parse_from_rfc3339("2026-05-13T08:38:00Z")
                .unwrap()
                .with_timezone(&Utc),
            pain: 7.5,
            importance: 8.25,
            pinned: true,
            skill_action: "rust::cargo::test".to_string(),
            source_harness: "claude-code".to_string(),
            content: "round-trip body".to_string(),
        };

        let encoded = serde_json::to_string(&original).expect("serialize");
        let decoded: AgentLearning = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(original, decoded);
        assert_eq!(decoded.id, id);
    }

    #[test]
    fn skill_action_accepts_valid() {
        assert_eq!(
            SkillAction::parse("rust::tokio").unwrap().as_str(),
            "rust::tokio"
        );
        assert_eq!(SkillAction::parse("rust").unwrap().as_str(), "rust");
        assert_eq!(
            SkillAction::parse("rust::borrow_checker").unwrap().as_str(),
            "rust::borrow_checker"
        );
    }

    #[test]
    fn skill_action_normalises_case_and_whitespace() {
        assert_eq!(
            SkillAction::parse("  Rust Tokio  ").unwrap().as_str(),
            "rust_tokio"
        );
    }

    #[test]
    fn skill_action_rejects_invalid() {
        assert!(SkillAction::parse("rust.tokio").is_err());
        assert!(SkillAction::parse("rust/borrow-checker").is_err());
        assert!(SkillAction::parse("rust::").is_err());
        assert!(SkillAction::parse("::rust").is_err());
        assert!(SkillAction::parse("rust:tokio").is_err());
        assert!(SkillAction::parse("").is_err());
        assert!(SkillAction::parse("   ").is_err());
        assert!(SkillAction::parse(&"a".repeat(257)).is_err());
    }
}
