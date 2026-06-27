//! Learn ingress — shared validation, redaction, and `AgentLearning` preparation.

use dreamd_protocol::{AgentLearning, EventId, SkillAction, RECORD_SCHEMA_VERSION};

use crate::redaction::redact;

/// Validation failures surfaced identically by HTTP and MCP learn ingress.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LearnValidationError {
    #[error("{0}")]
    InvalidSkillAction(String),
    #[error("pain must be in 0.0..=10.0")]
    PainOutOfRange,
    #[error("importance must be in 0.0..=10.0")]
    ImportanceOutOfRange,
}

/// Shared learn ingress: validate, redact, and map inbound payloads before the
/// coordinator receives them.
pub struct LearnIngress;

impl LearnIngress {
    /// Parse and validate a raw `skill_action` string.
    pub fn validate_skill_action(raw: &str) -> Result<SkillAction, LearnValidationError> {
        SkillAction::parse(raw).map_err(|e| LearnValidationError::InvalidSkillAction(e.to_string()))
    }

    /// Range-check a required score (HTTP body fields).
    pub fn validate_pain(pain: f32) -> Result<(), LearnValidationError> {
        if (0.0..=10.0).contains(&pain) {
            Ok(())
        } else {
            Err(LearnValidationError::PainOutOfRange)
        }
    }

    /// Range-check a required score (HTTP body fields).
    pub fn validate_importance(importance: f32) -> Result<(), LearnValidationError> {
        if (0.0..=10.0).contains(&importance) {
            Ok(())
        } else {
            Err(LearnValidationError::ImportanceOutOfRange)
        }
    }

    /// Range-check an optional score before applying the MCP default (parse, don't clamp).
    pub fn validate_optional_pain(pain: Option<f64>) -> Result<(), LearnValidationError> {
        if let Some(pain) = pain {
            if !(0.0..=10.0).contains(&pain) {
                return Err(LearnValidationError::PainOutOfRange);
            }
        }
        Ok(())
    }

    /// Range-check an optional score before applying the MCP default (parse, don't clamp).
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

    /// Validate, normalise, and redact an inbound HTTP [`AgentLearning`].
    pub fn prepare_agent_learning(
        learning: &mut AgentLearning,
        redaction_enabled: bool,
    ) -> Result<(), LearnValidationError> {
        let skill_action = Self::validate_skill_action(&learning.skill_action)?;
        learning.skill_action = skill_action.into_string();
        Self::validate_pain(learning.pain)?;
        Self::validate_importance(learning.importance)?;
        learning.content = redact(&learning.content, redaction_enabled);
        Ok(())
    }

    /// Validate, redact, and build an [`AgentLearning`] from MCP append params.
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

        Ok(AgentLearning {
            schema_version: RECORD_SCHEMA_VERSION.to_string(),
            id: EventId::parse("evt_00000000000000000000000000")
                .expect("static placeholder EventId is valid"),
            timestamp: chrono::Utc::now(),
            pain: pain.unwrap_or(5.0) as f32,
            importance: importance.unwrap_or(5.0) as f32,
            pinned: false,
            skill_action: skill_action.into_string(),
            source_harness: source_harness.to_owned(),
            content: redact(content, redaction_enabled),
        })
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

    #[test]
    fn prepare_agent_learning_redacts_content() {
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let mut learning = AgentLearning {
            schema_version: RECORD_SCHEMA_VERSION.to_string(),
            id: EventId::parse("evt_00000000000000000000000000").unwrap(),
            timestamp: chrono::Utc::now(),
            pain: 5.0,
            importance: 5.0,
            pinned: false,
            skill_action: "rust::test".to_string(),
            source_harness: "test".to_string(),
            content: format!("key is {secret}"),
        };

        LearnIngress::prepare_agent_learning(&mut learning, true).expect("prepare ok");
        assert!(!learning.content.contains(secret));
        assert!(learning.content.contains("[REDACTED]"));
    }

    #[test]
    fn build_agent_learning_matches_prepare_redaction() {
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let content = format!("key is {secret}");

        let built = LearnIngress::build_agent_learning(
            &content,
            "test-harness",
            "rust::test",
            None,
            None,
            true,
        )
        .expect("build ok");

        let mut prepared = AgentLearning {
            schema_version: RECORD_SCHEMA_VERSION.to_string(),
            id: EventId::parse("evt_00000000000000000000000000").unwrap(),
            timestamp: chrono::Utc::now(),
            pain: 5.0,
            importance: 5.0,
            pinned: false,
            skill_action: "rust::test".to_string(),
            source_harness: "test-harness".to_string(),
            content: content.clone(),
        };
        LearnIngress::prepare_agent_learning(&mut prepared, true).expect("prepare ok");

        assert_eq!(built.content, prepared.content);
        assert_eq!(built.skill_action, prepared.skill_action);
        assert_eq!(built.pain, prepared.pain);
        assert_eq!(built.importance, prepared.importance);
    }
}
