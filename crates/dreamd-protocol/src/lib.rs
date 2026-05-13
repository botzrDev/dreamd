use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AgentLearning {
    pub schema_version: String,
    pub id: String,
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

    #[test]
    fn agent_learning_json_round_trip() {
        let original = AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: "evt_9a8b7c6d".to_string(),
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
    }
}
