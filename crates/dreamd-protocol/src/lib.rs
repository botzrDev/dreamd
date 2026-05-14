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

/// Reason an `EventId::parse` call rejected its input.
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

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AgentLearning {
    pub schema_version: String,
    pub id: EventId,
    pub timestamp: DateTime<Utc>,
    pub pain: f32,
    pub importance: f32,
    #[serde(default)]
    pub pinned: bool,
    pub skill_action: String,
    pub source_harness: String,
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
        // JSON round-trip exercises both Serialize and Deserialize.
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
        // Lowercase, plus I/L/O/U are not in the alphabet.
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
            skill_action: "rust.cargo.test".to_string(),
            source_harness: "claude-code".to_string(),
            content: "round-trip body".to_string(),
        };

        let encoded = serde_json::to_string(&original).expect("serialize");
        let decoded: AgentLearning = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(original, decoded);
        assert_eq!(decoded.id, id);
    }
}
