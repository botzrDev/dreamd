//! Recall ingress — shared recall-result → wire JSON mapping.

use crate::RecallResult;

use super::wire::{RecallMeta, RecallResponse, RecallResultJson};

/// Shared recall ingress: map in-memory recall hits to the canonical wire shape.
pub struct RecallIngress;

impl RecallIngress {
    /// Map recall engine results to the canonical `{"results":[…]}` response body.
    pub fn map_results(results: Vec<RecallResult>) -> RecallResponse {
        RecallResponse {
            results: results.into_iter().map(Self::map_one).collect(),
        }
    }

    /// Serialise recall results to the canonical JSON string (HTTP + MCP local).
    pub fn to_json(results: Vec<RecallResult>) -> String {
        let resp = Self::map_results(results);
        serde_json::to_string(&resp).unwrap_or_else(|_| r#"{"results":[]}"#.to_string())
    }

    fn map_one(r: RecallResult) -> RecallResultJson {
        RecallResultJson {
            score: r.score,
            bm25: r.bm25,
            salience: r.salience,
            source: format!("{:?}", r.layer).to_lowercase(),
            content: r.content,
            metadata: RecallMeta {
                timestamp_sec: r.timestamp_sec,
                pain: r.pain,
                importance: r.importance,
                recurrence: r.recurrence,
                skill_action: r.skill_action,
                source_harness: r.source_harness,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::index::Layer;

    use super::*;

    #[test]
    fn map_results_sets_source_and_metadata() {
        let resp = RecallIngress::map_results(vec![RecallResult {
            score: 1.5,
            bm25: 0.8,
            salience: 1.2,
            layer: Layer::Episodic,
            content: "rust tokio".to_string(),
            timestamp_sec: 1_700_000_000,
            pain: 7.0,
            importance: 8.0,
            recurrence: 2,
            skill_action: "rust::tokio::async".to_string(),
            source_harness: "cursor".to_string(),
        }]);

        assert_eq!(resp.results.len(), 1);
        let r = &resp.results[0];
        assert_eq!(r.source, "episodic");
        assert_eq!(r.content, "rust tokio");
        assert_eq!(r.metadata.timestamp_sec, 1_700_000_000);
        assert_eq!(r.metadata.recurrence, 2);
        assert_eq!(r.metadata.skill_action, "rust::tokio::async");
        assert_eq!(r.metadata.source_harness, "cursor");
    }
}
