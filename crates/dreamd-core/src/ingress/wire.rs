//! Shared JSON wire shapes for learn/recall ingress (HTTP + MCP).
//!
//! Field units and stamping rules live here so both surfaces stay identical.
//! Daemon-minted fields (`id`, `timestamp`) are never trusted from the client
//! on learn; see [`LearnIngress`](crate::ingress::LearnIngress).

use crate::coordinator::AppendOutcome;

/// Response body for a successful `POST /api/v1/learn` (and MCP `append_node`).
///
/// `Deserialize` lets the daemon-proxy store (`memory_store::DaemonStore`)
/// read back the body the daemon serialized with this same type.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct LearnResponse {
    /// Daemon-minted `evt_…` EventId (clients never supply this).
    pub id: String,
    /// RFC 3339 timestamp stamped at durable append.
    pub timestamp: String,
    /// `true` when `client_dedup_key` matched a prior append (no second write).
    pub deduplicated: bool,
}

impl LearnResponse {
    /// Single construction site for HTTP and MCP learn-success responses.
    pub fn from_append_outcome(outcome: &AppendOutcome) -> Self {
        Self {
            id: outcome.id.as_str().to_owned(),
            timestamp: outcome.timestamp.to_rfc3339(),
            deduplicated: outcome.deduplicated,
        }
    }
}

/// Query params for `GET /api/v1/recall`. MCP `search_nodes` takes its own
/// `mcp::SearchNodesParams` (`query`, `k`).
#[derive(serde::Deserialize)]
pub struct RecallParams {
    /// Free-text BM25 query.
    pub q: String,
    /// Max hits to return; omit for [`DEFAULT_RECALL_K`].
    pub k: Option<u32>,
    /// `explain=1` adds a `citations` array to the response; any other value,
    /// including absent, omits it. HTTP-only — MCP `search_nodes` has no
    /// equivalent parameter.
    pub explain: Option<String>,
    /// Repeatable `exclude=<event_id>`. Absent deserializes as an empty vec.
    /// Counterfactual recall (BZR-194); HTTP-only, and there is no cluster
    /// parameter.
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// Default max results for recall when `k` is omitted (HTTP and MCP).
pub const DEFAULT_RECALL_K: u32 = 5;

/// Longest recall query, in UTF-8 bytes, that `GET /api/v1/recall` answers
/// and MCP `search_nodes` accepts in either mode (BZR-1301). Percent-encoding
/// at most triples it, so `/api/v1/recall?q=…&k=4294967295` stays under the
/// 65,534-byte URI limit of the `http` crate (client) and of hyper (server).
pub const MAX_RECALL_QUERY_BYTES: usize = 16_384;

/// The one message for a query over [`MAX_RECALL_QUERY_BYTES`]: the daemon's
/// `414` body, the daemon client's `414` arm, and the checks both MCP stores
/// run before they search.
pub fn query_too_long_message() -> String {
    format!("query exceeds {MAX_RECALL_QUERY_BYTES} bytes; pass a shorter query")
}

impl RecallParams {
    /// Resolved `k` query param — [`DEFAULT_RECALL_K`] when omitted.
    pub fn k_or_default(&self) -> u32 {
        self.k.unwrap_or(DEFAULT_RECALL_K)
    }

    /// `true` only when `explain` is the exact string `"1"`.
    pub fn explain_requested(&self) -> bool {
        self.explain.as_deref() == Some("1")
    }

    /// Parse a raw URL query string, collecting every `exclude=` value.
    ///
    /// `serde_urlencoded` (axum's `Query`) cannot deserialize a repeated key
    /// into a `Vec`, so the `exclude` pairs are split off first and the rest
    /// goes through the derived `Deserialize` unchanged.
    pub fn from_query(raw: &str) -> Result<Self, serde_urlencoded::de::Error> {
        let pairs: Vec<(String, String)> = serde_urlencoded::from_str(raw)?;
        let (exclude, rest): (Vec<_>, Vec<_>) =
            pairs.into_iter().partition(|(key, _)| key == "exclude");
        let rest = serde_urlencoded::to_string(rest)
            .map_err(<serde_urlencoded::de::Error as serde::de::Error>::custom)?;
        let mut params: Self = serde_urlencoded::from_str(&rest)?;
        params.exclude = exclude.into_iter().map(|(_, value)| value).collect();
        Ok(params)
    }
}

/// Canonical recall response body shared by HTTP and MCP.
#[derive(serde::Serialize)]
pub struct RecallResponse {
    pub results: Vec<RecallResultJson>,
    /// Per-result salience factor breakdown. `None` serializes with no
    /// `citations` key (default HTTP recall, and MCP `search_nodes`); `Some`
    /// only on `GET /api/v1/recall?explain=1`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub citations: Option<Vec<CitationJson>>,
}

/// One salience-ranked hit on the wire.
#[derive(serde::Serialize)]
pub struct RecallResultJson {
    /// Final ranking score: BM25 × salience (query-time; never indexed).
    pub score: f64,
    /// Raw Tantivy BM25 component before salience multiply.
    pub bm25: f64,
    /// Query-time salience multiplier (see `crate::salience`).
    pub salience: f64,
    /// Memory layer name: `"episodic"` or `"semantic"` ([`crate::index::Layer::as_str`]).
    pub source: String,
    /// Stored learning text.
    pub content: String,
    pub metadata: RecallMeta,
}

/// Fastfield and recurrence-table metadata echoed beside each recall hit.
#[derive(serde::Serialize)]
pub struct RecallMeta {
    /// Unix seconds at index time (`AgentLearning::timestamp`).
    pub timestamp_sec: u64,
    /// Subjective friction 0.0..=10.0.
    pub pain: f64,
    /// Long-term relevance 0.0..=10.0.
    pub importance: f64,
    /// Alive episodic docs whose `skill_action` equals this hit's or extends it
    /// with `::`, computed per searcher (`crate::recurrence`).
    pub recurrence: u64,
    /// Hierarchical clustering key (language-first `::` segments).
    pub skill_action: String,
    /// Harness that authored the learning (e.g. `"claude-code"`, `"cursor"`).
    pub source_harness: String,
}

/// One result's salience-factor breakdown, only present on
/// `GET /api/v1/recall?explain=1`. See `crate::salience::salience` for the
/// formula these components feed.
#[derive(serde::Serialize)]
pub struct CitationJson {
    /// Event id (`RecallResult::event_id`); may be empty for pre-WEG-45 docs.
    pub id: String,
    /// Final ranking score: BM25 × salience.
    pub score: f64,
    /// Raw Tantivy BM25 component before salience multiply.
    pub bm25_component: f64,
    /// Query-time salience multiplier.
    pub salience_component: f64,
    /// Memory layer name: `"episodic"` or `"semantic"` ([`crate::index::Layer::as_str`]) —
    /// the same token as the paired `results[].source`.
    pub layer: String,
    /// Full stored learning text, no truncation.
    pub snippet: String,
    /// The age in days the salience formula uses ([`crate::salience::age_days`]):
    /// floored at 0.
    pub age_days: f64,
    /// The stored pain score, before the formula clamps it to 0.0..=10.0 and
    /// divides it by 10.
    pub pain: f64,
    /// The stored importance score, before the formula clamps it to 0.0..=10.0
    /// and divides it by 10.
    pub importance: f64,
    /// Alive episodic docs whose `skill_action` equals this hit's or extends it
    /// with `::`, computed per searcher (`crate::recurrence`).
    pub recurrence: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_params_exclude_is_repeatable_and_defaults_empty() {
        let params = RecallParams::from_query("q=axum&exclude=evt_a&k=3&exclude=evt_b").unwrap();
        assert_eq!(params.q, "axum");
        assert_eq!(params.k, Some(3));
        assert_eq!(params.exclude, vec!["evt_a", "evt_b"]);

        let params = RecallParams::from_query("q=axum").unwrap();
        assert!(params.exclude.is_empty());
        assert_eq!(params.k_or_default(), DEFAULT_RECALL_K);

        let params: RecallParams = serde_json::from_str(r#"{"q":"axum"}"#).unwrap();
        assert!(params.exclude.is_empty(), "serde default is an empty vec");

        assert!(
            RecallParams::from_query("exclude=evt_a").is_err(),
            "q is still required"
        );
    }

    /// BZR-1301: the only test that spells the query limit's number.
    #[test]
    fn query_too_long_message_text() {
        assert_eq!(
            query_too_long_message(),
            "query exceeds 16384 bytes; pass a shorter query"
        );
    }
}
