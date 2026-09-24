//! Letta adapter seam (BZR-183).
//!
//! [`LettaStore`] is a third [`MemoryStore`] implementor. It delegates every
//! call to an inner store and returns that store's result unchanged: no path
//! rewriting, no second JSON shape, no network. dreamd's episodic JSONL stays
//! the source of truth.
//!
//! Letta MemFS read/write is not implemented. Neither are conflict handling
//! and install steps. See `adapters/letta/README.md`.

use std::sync::Arc;

use crate::ingress::LearnResponse;
use crate::memory_store::{AppendRequest, MemoryStore, MemoryStoreError};

/// A [`MemoryStore`] that forwards to an inner store.
pub struct LettaStore {
    inner: Arc<dyn MemoryStore>,
}

impl LettaStore {
    /// Wrap `inner`. Every call is forwarded to it.
    pub fn new(inner: Arc<dyn MemoryStore>) -> Self {
        Self { inner }
    }
}

#[async_trait::async_trait]
impl MemoryStore for LettaStore {
    async fn recall_json(&self, query: &str, k: u32) -> Result<String, MemoryStoreError> {
        self.inner.recall_json(query, k).await
    }

    async fn append(&self, req: AppendRequest) -> Result<LearnResponse, MemoryStoreError> {
        self.inner.append(req).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Records what it was handed and returns fixed values.
    #[derive(Default)]
    struct FakeStore {
        queries: Mutex<Vec<(String, u32)>>,
        appends: Mutex<Vec<AppendRequest>>,
    }

    #[async_trait::async_trait]
    impl MemoryStore for FakeStore {
        async fn recall_json(&self, query: &str, k: u32) -> Result<String, MemoryStoreError> {
            self.queries.lock().unwrap().push((query.to_owned(), k));
            Ok(r#"{"results":[]}"#.to_owned())
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

    #[tokio::test]
    async fn letta_store_forwards_recall_and_append_unchanged() {
        let fake = Arc::new(FakeStore::default());
        let store = LettaStore::new(fake.clone());

        let json = store.recall_json("axum rejection", 7).await.unwrap();
        assert_eq!(json, r#"{"results":[]}"#);

        let req = AppendRequest {
            content: "prefer typed rejections".to_owned(),
            source_harness: "letta".to_owned(),
            skill_action: "rust::error_handling::axum_rejection".to_owned(),
            pain: Some(6.0),
            importance: None,
            client_dedup_key: None,
            pinned: false,
        };
        let resp = store.append(req.clone()).await.unwrap();
        assert_eq!(resp.id, "evt_placeholder");
        assert_eq!(resp.timestamp, "2026-09-24T00:00:00+00:00");
        assert!(!resp.deduplicated);

        assert_eq!(
            *fake.queries.lock().unwrap(),
            vec![("axum rejection".to_owned(), 7)]
        );
        assert_eq!(*fake.appends.lock().unwrap(), vec![req]);
    }
}
