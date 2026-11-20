//! Learn ingress — shared validation, redaction, and `AgentLearning` preparation.

use dreamd_protocol::{AgentLearning, EventId, SkillAction, RECORD_SCHEMA_VERSION};

use crate::redaction::redact;

/// Validation failures surfaced identically by HTTP and MCP learn ingress.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LearnValidationError {
    /// `skill_action` failed [`SkillAction::parse`]; carries the parse error.
    #[error("{0}")]
    InvalidSkillAction(String),
    /// `pain` is outside `0.0..=10.0`.
    #[error("pain must be in 0.0..=10.0")]
    PainOutOfRange,
    /// `importance` is outside `0.0..=10.0`.
    #[error("importance must be in 0.0..=10.0")]
    ImportanceOutOfRange,
}

/// What an omitted MCP score becomes. HTTP has no default: both scores are
/// required there.
pub const DEFAULT_SCORE: f64 = 5.0;

/// The one wire body of `POST /api/v1/learn`. The daemon deserializes it in
/// `post_learn`, and the daemon-mode MCP client serializes it in
/// `memory_store::build_learn_request`, so the two cannot disagree on a field
/// (DRM-104).
///
/// Deliberately not the domain [`AgentLearning`]: `id`, `schema_version`, and
/// `timestamp` are daemon-owned (minted and stamped in `coordinator.rs` on every
/// durable write), but `AgentLearning` is also the *disk* record, where `id` is
/// authoritative and must keep parsing strictly (`episodic.rs`). One
/// `Deserialize` impl cannot serve both surfaces, so the wire gets its own type
/// rather than the protocol struct gaining a `skip_deserializing` that would
/// weaken the disk read. AILAB-175.
///
/// No `deny_unknown_fields`: a client that still sends `id` (older doc
/// examples did) has it **ignored, not rejected**, and serde's default
/// ignore-unknown gives that for free.
///
/// `pain` and `importance` are `f64`, so a score is checked as sent. The
/// record stores the nearest `f32`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LearnRequest {
    /// 0.0..=10.0 subjective friction. Required on HTTP, which has never
    /// defaulted it.
    pub pain: f64,
    /// 0.0..=10.0 long-term relevance. Required, same reasoning as `pain`.
    pub importance: f64,
    /// Sticky flag, with a serde default matching `AgentLearning::pinned`.
    /// Client-settable and preserved.
    #[serde(default)]
    pub pinned: bool,
    /// Clustering key; validated and normalized at ingress.
    pub skill_action: String,
    /// Provenance tag, e.g. `"cursor"`, `"claude-code"`.
    pub source_harness: String,
    /// Free-text body; redacted at ingress, 4 KiB cap enforced downstream.
    pub content: String,
    /// Optional retry key (DRM-106). A second append with the same key and
    /// the same learning answers the first write's id without a second
    /// write; the same key with a different learning is a new record. Any
    /// characters are allowed. Daemon-mode MCP sends it here; an HTTP client
    /// may send it here or in `X-Client-Dedup-Key`.
    #[serde(default)]
    pub client_dedup_key: Option<String>,
}

/// Shared learn ingress: validate, redact, and map inbound payloads before the
/// coordinator receives them.
pub struct LearnIngress;

impl LearnIngress {
    /// Reject malformed `skill_action` before it reaches the coordinator.
    pub fn validate_skill_action(raw: &str) -> Result<SkillAction, LearnValidationError> {
        SkillAction::parse(raw).map_err(|e| LearnValidationError::InvalidSkillAction(e.to_string()))
    }

    /// HTTP requires an explicit pain; reject out-of-range rather than clamp.
    pub fn validate_pain(pain: f32) -> Result<(), LearnValidationError> {
        if (0.0..=10.0).contains(&pain) {
            Ok(())
        } else {
            Err(LearnValidationError::PainOutOfRange)
        }
    }

    /// HTTP requires an explicit importance; reject out-of-range rather than clamp.
    pub fn validate_importance(importance: f32) -> Result<(), LearnValidationError> {
        if (0.0..=10.0).contains(&importance) {
            Ok(())
        } else {
            Err(LearnValidationError::ImportanceOutOfRange)
        }
    }

    /// MCP may omit pain ([`Self::placeholder_learning`] then uses `5.0`);
    /// reject only when present and invalid.
    pub fn validate_optional_pain(pain: Option<f64>) -> Result<(), LearnValidationError> {
        if let Some(pain) = pain {
            if !(0.0..=10.0).contains(&pain) {
                return Err(LearnValidationError::PainOutOfRange);
            }
        }
        Ok(())
    }

    /// MCP may omit importance ([`Self::placeholder_learning`] then uses
    /// `5.0`); reject only when present and invalid.
    pub fn validate_optional_importance(
        importance: Option<f64>,
    ) -> Result<(), LearnValidationError> {
        if let Some(importance) = importance {
            if !(0.0..=10.0).contains(&importance) {
                return Err(LearnValidationError::ImportanceOutOfRange);
            }
        }
        Ok(())
    }

    /// The one ingress function: every append goes through it once, in the
    /// process that owns the store. In-process MCP calls it from
    /// `InProcessStore::append`, and the daemon from `post_learn` for HTTP
    /// and daemon-mode MCP alike (DRM-104).
    ///
    /// It checks `skill_action`, then `pain`, then `importance`, and then
    /// redacts `content`; the first failure is the answer. Omitted scores
    /// become [`DEFAULT_SCORE`] (see [`Self::placeholder_learning`]).
    ///
    /// The placeholder `id` is overwritten by the coordinator on persist.
    pub fn build_agent_learning(
        content: &str,
        source_harness: &str,
        skill_action_raw: &str,
        pain: Option<f64>,
        importance: Option<f64>,
        redaction_enabled: bool,
    ) -> Result<AgentLearning, LearnValidationError> {
        let skill_action = Self::validate_skill_action(skill_action_raw)?;
        Self::validate_optional_pain(pain)?;
        Self::validate_optional_importance(importance)?;

        Ok(Self::placeholder_learning(
            &redact(content, redaction_enabled),
            source_harness,
            skill_action.as_str(),
            pain,
            importance,
        ))
    }

    /// The one placeholder [`AgentLearning`] constructor: `schema_version` is
    /// [`RECORD_SCHEMA_VERSION`], `id` is the static placeholder [`EventId`],
    /// `timestamp` is now, omitted scores default to `5.0`, `pinned` is
    /// `false`. The coordinator overwrites `id`, `timestamp`, and
    /// `schema_version` on durable write.
    ///
    /// Performs no validation or redaction: [`Self::build_agent_learning`]
    /// checks first, so the `f32` cast below only stores a score already
    /// checked as `f64`.
    pub fn placeholder_learning(
        content: &str,
        source_harness: &str,
        skill_action: &str,
        pain: Option<f64>,
        importance: Option<f64>,
    ) -> AgentLearning {
        AgentLearning {
            schema_version: RECORD_SCHEMA_VERSION.to_string(),
            id: EventId::parse("evt_00000000000000000000000000")
                .expect("static placeholder EventId is valid"),
            timestamp: chrono::Utc::now(),
            pain: pain.unwrap_or(DEFAULT_SCORE) as f32,
            importance: importance.unwrap_or(DEFAULT_SCORE) as f32,
            pinned: false,
            skill_action: skill_action.to_owned(),
            source_harness: source_harness.to_owned(),
            content: content.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_dotted_and_slashed_skill_action() {
        for bad in ["rust.tokio", "rust/borrow-checker"] {
            let err = LearnIngress::validate_skill_action(bad).unwrap_err();
            assert!(
                matches!(err, LearnValidationError::InvalidSkillAction(_)),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn accepts_double_colon_skill_action() {
        LearnIngress::validate_skill_action("rust::tokio").expect("valid skill_action");
    }

    #[test]
    fn rejects_out_of_range_required_scores() {
        assert_eq!(
            LearnIngress::validate_pain(1e9_f32).unwrap_err(),
            LearnValidationError::PainOutOfRange
        );
        assert_eq!(
            LearnIngress::validate_importance(-1.0).unwrap_err(),
            LearnValidationError::ImportanceOutOfRange
        );
    }

    #[test]
    fn rejects_out_of_range_optional_scores_before_default() {
        assert_eq!(
            LearnIngress::validate_optional_pain(Some(1e9)).unwrap_err(),
            LearnValidationError::PainOutOfRange
        );
        assert_eq!(
            LearnIngress::validate_optional_importance(Some(11.0)).unwrap_err(),
            LearnValidationError::ImportanceOutOfRange
        );
    }

    /// DRM-104: the redaction case of the removed `prepare_agent_learning`
    /// tests, through the one ingress function alone.
    #[test]
    fn build_agent_learning_redacts_content() {
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let built = LearnIngress::build_agent_learning(
            &format!("key is {secret}"),
            "test-harness",
            "rust::test",
            None,
            None,
            true,
        )
        .expect("build ok");

        assert!(!built.content.contains(secret));
        assert!(built.content.contains("[REDACTED]"));
        assert_eq!(built.skill_action, "rust::test");
        assert_eq!(built.pain, DEFAULT_SCORE as f32);
        assert_eq!(built.importance, DEFAULT_SCORE as f32);
    }
}
