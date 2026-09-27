//! Recall ingress — shared recall-result → wire JSON mapping.

use crate::RecallResult;

use super::wire::{CitationJson, RecallMeta, RecallResponse, RecallResultJson};

/// Shared recall ingress: map in-memory recall hits to the canonical wire shape.
pub struct RecallIngress;

impl RecallIngress {
    /// Map recall engine results to the canonical `{"results":[…]}` response body.
    pub fn map_results(results: Vec<RecallResult>) -> RecallResponse {
        RecallResponse {
            results: results.into_iter().map(Self::map_one).collect(),
            citations: None,
        }
    }

    /// Serialise recall results to the canonical JSON string (HTTP + MCP local).
    ///
    /// No `citations` key — callers that need the factor breakdown use
    /// [`Self::to_json_with_citations`] instead.
    pub fn to_json(results: Vec<RecallResult>) -> String {
        let resp = Self::map_results(results);
        serde_json::to_string(&resp).unwrap_or_else(|_| r#"{"results":[]}"#.to_string())
    }

    /// Serialise recall results with a `citations` array added alongside
    /// `results`, in the same order. `now_sec` is the same clock reading used
    /// to run the recall query.
    ///
    /// `GET /api/v1/recall?explain=1` only — every other caller, including
    /// MCP `search_nodes`, uses [`Self::to_json`].
    pub fn to_json_with_citations(results: Vec<RecallResult>, now_sec: i64) -> String {
        let citations = results
            .iter()
            .map(|r| Self::map_citation(r, now_sec))
            .collect();
        let mut resp = Self::map_results(results);
        resp.citations = Some(citations);
        serde_json::to_string(&resp).unwrap_or_else(|_| r#"{"results":[]}"#.to_string())
    }

    fn map_citation(r: &RecallResult, now_sec: i64) -> CitationJson {
        CitationJson {
            id: r.event_id.clone(),
            score: r.score,
            bm25_component: r.bm25,
            salience_component: r.salience,
            layer: r.layer.as_str().to_owned(),
            snippet: r.content.clone(),
            age_days: (now_sec - r.timestamp_sec as i64) as f64 / 86_400.0,
            pain: r.pain,
            importance: r.importance,
            recurrence: r.recurrence,
        }
    }

    fn map_one(r: RecallResult) -> RecallResultJson {
        RecallResultJson {
            score: r.score,
            bm25: r.bm25,
            salience: r.salience,
            // Stable wire token — use Layer::as_str, not Debug formatting.
            source: r.layer.as_str().to_owned(),
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
            event_id: "evt_test".to_string(),
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

    fn sample_result() -> RecallResult {
        RecallResult {
            score: 1.5,
            bm25: 0.8,
            salience: 1.2,
            layer: Layer::Semantic,
            content: "rust tokio full text, not truncated".to_string(),
            timestamp_sec: 1_700_000_000,
            pain: 7.0,
            importance: 8.0,
            recurrence: 2,
            skill_action: "rust::tokio::async".to_string(),
            source_harness: "cursor".to_string(),
            event_id: "evt_citation_test".to_string(),
        }
    }

    #[test]
    fn to_json_has_no_citations_key() {
        let json = RecallIngress::to_json(vec![sample_result()]);
        assert!(
            !json.contains("citations"),
            "to_json must not emit a citations key: {json}"
        );
    }

    #[test]
    fn to_json_with_citations_matches_results_order_and_factors() {
        let now_sec: i64 = 1_700_100_000;
        let r = sample_result();
        let expected_age_days = (now_sec - r.timestamp_sec as i64) as f64 / 86_400.0;

        let json = RecallIngress::to_json_with_citations(vec![r], now_sec);
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");

        let results = value["results"].as_array().expect("results array");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["source"], "semantic");
        assert!(
            results[0].get("layer").is_none(),
            "results[0] must not carry a layer key"
        );

        let citations = value["citations"].as_array().expect("citations array");
        assert_eq!(citations.len(), 1);
        let c = &citations[0];
        assert_eq!(c["id"], "evt_citation_test");
        assert_eq!(c["layer"], results[0]["source"]);
        assert_eq!(c["age_days"].as_f64().unwrap(), expected_age_days);
        assert_eq!(c["pain"].as_f64().unwrap(), 7.0);
        assert_eq!(c["importance"].as_f64().unwrap(), 8.0);
        assert_eq!(c["recurrence"].as_u64().unwrap(), 2);
        assert_eq!(c["bm25_component"].as_f64().unwrap(), 0.8);
        assert_eq!(c["salience_component"].as_f64().unwrap(), 1.2);
        assert_eq!(
            c["snippet"].as_str().unwrap(),
            "rust tokio full text, not truncated"
        );
    }
}
