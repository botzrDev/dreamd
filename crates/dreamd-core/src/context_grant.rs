//! Region-shaped context grant on the `MemoryStore` seam (BZR-827).
//!
//! [`ContextGrant`] says which of the two recall sources — `"episodic"` and
//! `"semantic"` (see [`crate::index::Layer`]) — a caller may see. [`GrantedStore`]
//! is a fourth [`MemoryStore`] implementor, in the shape of [`crate::letta::LettaStore`],
//! that applies the grant to an inner store: [`ContextGrant::both()`] forwards
//! every call unchanged, and a narrower grant filters recall hits by `source`
//! and refuses an append when `episodic` is not granted.
//!
//! This is not a momo `MemoryRegion` (dreamOS charter FR-2.4). That kernel
//! object is future work and is not declared here; see `docs/context-grant.md`.
//! No trust label is recorded. MCP is not wired to this type — `search_nodes`
//! and `append_node` stay on `InProcessStore` / `DaemonStore`, which is the
//! `both()` grant.

use std::sync::Arc;

use crate::ingress::LearnResponse;
use crate::memory_store::{AppendRequest, MemoryStore, MemoryStoreError};

/// Which recall sources a caller may see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextGrant {
    pub episodic: bool,
    pub semantic: bool,
}

impl ContextGrant {
    /// Both sources granted — the forwarding grant every existing MCP caller
    /// gets today, because `search_nodes` / `append_node` never construct a
    /// [`GrantedStore`].
    pub const fn both() -> Self {
        Self {
            episodic: true,
            semantic: true,
        }
    }

    /// Whether `source` (a recall hit's `"episodic"` / `"semantic"` string) is
    /// granted. Any other value, including one this grant does not recognize,
    /// is not granted.
    fn allows_source(&self, source: &str) -> bool {
        match source {
            "episodic" => self.episodic,
            "semantic" => self.semantic,
            _ => false,
        }
    }
}

/// A [`MemoryStore`] that applies a [`ContextGrant`] to an inner store.
pub struct GrantedStore {
    inner: Arc<dyn MemoryStore>,
    grant: ContextGrant,
}

impl GrantedStore {
    /// Wrap `inner`, filtering recall and gating append by `grant`.
    pub fn new(inner: Arc<dyn MemoryStore>, grant: ContextGrant) -> Self {
        Self { inner, grant }
    }

    /// Filter a `{"results":[...]}` body to hits whose `source` is granted.
    /// A missing or non-array `results` field is [`MemoryStoreError::Internal`]
    /// — the inner store's response is malformed, not a caller mistake.
    fn filter_recall_json(&self, body: &str) -> Result<String, MemoryStoreError> {
        let parsed: serde_json::Value = serde_json::from_str(body).map_err(|e| {
            MemoryStoreError::Internal(format!("recall body is not valid JSON: {e}"))
        })?;
        let results = parsed
            .get("results")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                MemoryStoreError::Internal("recall body has no results array".to_owned())
            })?;

        let filtered: Vec<&serde_json::Value> = results
            .iter()
            .filter(|hit| {
                hit.get("source")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|source| self.grant.allows_source(source))
            })
            .collect();

        serde_json::to_string(&serde_json::json!({ "results": filtered })).map_err(|e| {
            MemoryStoreError::Internal(format!("filtered recall body serialize failed: {e}"))
        })
    }
}

#[async_trait::async_trait]
impl MemoryStore for GrantedStore {
    async fn recall_json(&self, query: &str, k: u32) -> Result<String, MemoryStoreError> {
        let body = self.inner.recall_json(query, k).await?;
        if self.grant == ContextGrant::both() {
            return Ok(body);
        }
        self.filter_recall_json(&body)
    }

    async fn append(&self, req: AppendRequest) -> Result<LearnResponse, MemoryStoreError> {
        if !self.grant.episodic {
            return Err(MemoryStoreError::InvalidRequest(
                "episodic source is not granted".to_owned(),
            ));
        }
        self.inner.append(req).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Records what it was handed and returns fixed values. Same shape as
    /// `letta::tests::FakeStore`.
    #[derive(Default)]
    struct FakeStore {
        recall_body: Mutex<String>,
        appends: Mutex<Vec<AppendRequest>>,
    }

    #[async_trait::async_trait]
    impl MemoryStore for FakeStore {
        async fn recall_json(&self, _query: &str, _k: u32) -> Result<String, MemoryStoreError> {
            Ok(self.recall_body.lock().unwrap().clone())
        }

        async fn append(&self, req: AppendRequest) -> Result<LearnResponse, MemoryStoreError> {
            self.appends.lock().unwrap().push(req);
            Ok(LearnResponse {
                id: "evt_placeholder".to_owned(),
                timestamp: "2026-09-24T00:00:00+00:00".to_owned(),
                deduplicated: false,
            })
        }
    }

    fn sample_append() -> AppendRequest {
        AppendRequest {
            content: "prefer typed rejections".to_owned(),
            source_harness: "test".to_owned(),
            skill_action: "rust::error_handling::axum_rejection".to_owned(),
            pain: Some(6.0),
            importance: None,
            client_dedup_key: None,
            pinned: false,
        }
    }

    #[tokio::test]
    async fn both_forwards_recall_unchanged_and_forwards_append() {
        let fake = Arc::new(FakeStore::default());
        *fake.recall_body.lock().unwrap() = "not valid recall json at all".to_owned();
        let store = GrantedStore::new(fake.clone(), ContextGrant::both());

        let body = store.recall_json("axum rejection", 7).await.unwrap();
        assert_eq!(body, "not valid recall json at all");

        let req = sample_append();
        let resp = store.append(req.clone()).await.unwrap();
        assert_eq!(resp.id, "evt_placeholder");
        assert_eq!(*fake.appends.lock().unwrap(), vec![req]);
    }

    #[tokio::test]
    async fn semantic_only_keeps_semantic_drops_episodic_and_other_and_blocks_append() {
        let fake = Arc::new(FakeStore::default());
        *fake.recall_body.lock().unwrap() = serde_json::json!({
            "results": [
                {"source": "semantic", "content": "lesson hit"},
                {"source": "episodic", "content": "event hit"},
                {"source": "other", "content": "unknown hit"},
            ]
        })
        .to_string();
        let store = GrantedStore::new(
            fake.clone(),
            ContextGrant {
                episodic: false,
                semantic: true,
            },
        );

        let body = store.recall_json("query", 5).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        let results = parsed["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["source"], "semantic");

        let err = store.append(sample_append()).await.unwrap_err();
        assert_eq!(
            err,
            MemoryStoreError::InvalidRequest("episodic source is not granted".to_owned())
        );
        assert!(fake.appends.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn episodic_only_drops_semantic_and_forwards_append() {
        let fake = Arc::new(FakeStore::default());
        *fake.recall_body.lock().unwrap() = serde_json::json!({
            "results": [
                {"source": "semantic", "content": "lesson hit"},
                {"source": "episodic", "content": "event hit"},
            ]
        })
        .to_string();
        let store = GrantedStore::new(
            fake.clone(),
            ContextGrant {
                episodic: true,
                semantic: false,
            },
        );

        let body = store.recall_json("query", 5).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        let results = parsed["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["source"], "episodic");

        let req = sample_append();
        let resp = store.append(req.clone()).await.unwrap();
        assert_eq!(resp.id, "evt_placeholder");
        assert_eq!(*fake.appends.lock().unwrap(), vec![req]);
    }

    #[tokio::test]
    async fn narrower_grant_with_no_results_array_is_internal_error() {
        let fake = Arc::new(FakeStore::default());
        *fake.recall_body.lock().unwrap() = serde_json::json!({"other": []}).to_string();
        let store = GrantedStore::new(
            fake,
            ContextGrant {
                episodic: true,
                semantic: false,
            },
        );

        let err = store.recall_json("query", 5).await.unwrap_err();
        assert_eq!(
            err,
            MemoryStoreError::Internal("recall body has no results array".to_owned())
        );
    }
}
