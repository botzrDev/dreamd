//! On-disk index freshness (WEG-42 / DR-202), split out of `tantivy_handle`
//! by BZR-170.
//!
//! Owns the commit watermark (`index_progress.json`) and the one function that
//! compares it to the JSONL tail. Nothing here opens Tantivy or reads the live
//! reader: freshness is what has been *committed to disk*, which is the only
//! answer that is still true after a crash.

use std::path::Path;

use dreamd_protocol::AgentLearning;
use serde::{Deserialize, Serialize};

use crate::io::write_atomic;
use crate::layout::AgentRoot;
use crate::server::index_map::IndexError;

/// Relative filename for the indexer's commit watermark, joined under the
/// project's `.dreamd/` directory. WEG-42 owns reads and writes; `dreamd
/// doctor --repair` (AILAB-223) clears it when wiping the rebuildable cache.
pub const INDEX_PROGRESS_FILENAME: &str = "index_progress.json";

/// v0.1 index-vs-JSONL contract surface (WEG-42 / DR-202).
///
/// Compares the JSONL tail against `index_progress.json`. `stale == true` when
/// the episodic log has committed events the index watermark has not caught up
/// to yet — including the normal ≤[`DEFAULT_COMMIT_CADENCE`](crate::server::tantivy_handle::DEFAULT_COMMIT_CADENCE) window after a
/// live append, a full indexer channel, or a crash between JSONL `sync_data`
/// and the next Tantivy commit. A full channel still reads as `stale` — the
/// event is durable in the JSONL and not yet committed, which is exactly what
/// this compares. What changed is that it no longer *persists*: because the
/// coordinator awaits its `send`, the message is queued rather than dropped, so
/// the staleness clears as the indexer drains the backlog instead of surviving
/// until a restart. JSONL durability is WAL-backed; index freshness is
/// best-effort and heals when the indexer commits the backlog (or, after a
/// crash, on the next [`TantivyIndexHandle::open`](crate::server::tantivy_handle::TantivyIndexHandle::open) replay).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IndexFreshness {
    /// `true` when `jsonl_tail_id` is strictly greater than `last_indexed_id`
    /// (lexicographic `EventId` order), or when JSONL has events but the
    /// watermark is absent.
    pub stale: bool,
    /// `id` of the last well-formed JSONL record, if any.
    pub jsonl_tail_id: Option<String>,
    /// `last_indexed_id` from `index_progress.json`, if present.
    pub last_indexed_id: Option<String>,
    /// Count of JSONL events strictly after the watermark (0 when fresh).
    pub unindexed_count: usize,
}

/// Assess on-disk index freshness for `agent_root` without opening Tantivy.
///
/// Operators and `GET /api/v1/health` use this to detect recall lag relative to
/// the JSONL source of truth. Does not consult the live indexer channel.
pub fn assess_index_freshness(agent_root: &AgentRoot) -> Result<IndexFreshness, IndexError> {
    let progress_path = agent_root.dreamd_dir().join(INDEX_PROGRESS_FILENAME);
    let progress = read_progress(&progress_path)?;
    let watermark = progress.last_indexed_id.as_deref();

    let events = read_jsonl_events(&agent_root.episodic_jsonl())?;
    let jsonl_tail_id = events.last().map(|ev| ev.id.as_str().to_owned());
    let unindexed_count = events
        .iter()
        .filter(|ev| match watermark {
            Some(last) => ev.id.as_str() > last,
            None => true,
        })
        .count();
    let stale = unindexed_count > 0;

    Ok(IndexFreshness {
        stale,
        jsonl_tail_id,
        last_indexed_id: progress.last_indexed_id,
        unindexed_count,
    })
}

/// Crash-recovery watermark recording the daemon-assigned `EventId` of the
/// most recently committed document. Lives at
/// `<agent_root>/.dreamd/index_progress.json`. Reads on startup;
/// writes after each successful Tantivy commit, never before.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexProgress {
    /// `evt_`-prefixed ULID string of the most recently committed document,
    /// or `None` if no commit has succeeded yet (cold start / empty JSONL).
    pub(crate) last_indexed_id: Option<String>,
}

/// Every well-formed episodic record, via the shared WEG-378 scan.
pub(crate) fn read_jsonl_events(jsonl_path: &Path) -> Result<Vec<AgentLearning>, IndexError> {
    crate::episodic::read_all(jsonl_path).map_err(|e| IndexError::Other(format!("read jsonl: {e}")))
}

pub(crate) fn read_progress(path: &Path) -> Result<IndexProgress, IndexError> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| IndexError::Other(format!("parse {INDEX_PROGRESS_FILENAME}: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(IndexProgress::default()),
        Err(e) => Err(IndexError::Other(format!(
            "read {INDEX_PROGRESS_FILENAME}: {e}"
        ))),
    }
}

pub(crate) fn write_progress(path: &Path, progress: &IndexProgress) -> Result<(), IndexError> {
    let bytes = serde_json::to_vec(progress)
        .map_err(|e| IndexError::Other(format!("serialize {INDEX_PROGRESS_FILENAME}: {e}")))?;
    write_atomic(path, &bytes)
        .map_err(|e| IndexError::Other(format!("write {INDEX_PROGRESS_FILENAME}: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{unique_tmpdir, DirGuard};
    use chrono::{DateTime, Utc};
    use dreamd_protocol::EventId;

    fn make_event_id(suffix_char: char) -> EventId {
        EventId::parse(&format!("evt_01ARZ3NDEKTSV4RRFFQ69G5FA{suffix_char}"))
            .expect("synthesize EventId")
    }

    fn sample_learning(id: EventId, skill: &str, content: &str) -> AgentLearning {
        AgentLearning {
            schema_version: "1.0.0".to_string(),
            id,
            timestamp: DateTime::parse_from_rfc3339("2026-05-14T08:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            pain: 5.0,
            importance: 6.0,
            pinned: false,
            skill_action: skill.to_string(),
            source_harness: "test-harness".to_string(),
            content: content.to_string(),
        }
    }

    fn prime_jsonl(dir: &Path, learnings: &[AgentLearning]) {
        let path = AgentRoot::new(dir).episodic_jsonl();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut bytes = Vec::new();
        for l in learnings {
            bytes.extend_from_slice(serde_json::to_string(l).unwrap().as_bytes());
            bytes.push(b'\n');
        }
        std::fs::write(&path, &bytes).unwrap();
    }

    #[test]
    fn assess_index_freshness_ok_when_watermark_matches_tail() {
        let dir = unique_tmpdir("fresh-ok");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        let id = make_event_id('A');
        prime_jsonl(&dir, &[sample_learning(id.clone(), "rust.test", "one")]);
        let progress_path = agent_root.dreamd_dir().join(INDEX_PROGRESS_FILENAME);
        std::fs::create_dir_all(agent_root.dreamd_dir()).unwrap();
        write_progress(
            &progress_path,
            &IndexProgress {
                last_indexed_id: Some(id.as_str().to_owned()),
            },
        )
        .unwrap();

        let report = assess_index_freshness(&agent_root).expect("assess");
        assert!(!report.stale, "watermark at tail must be fresh: {report:?}");
        assert_eq!(report.unindexed_count, 0);
    }

    #[test]
    fn assess_index_freshness_stale_when_jsonl_ahead_of_watermark() {
        let dir = unique_tmpdir("fresh-stale");
        let _g = DirGuard(dir.clone());
        let agent_root = AgentRoot::new(&dir);
        let id_a = make_event_id('A');
        let id_b = make_event_id('B');
        prime_jsonl(
            &dir,
            &[
                sample_learning(id_a.clone(), "rust.test", "older"),
                sample_learning(id_b.clone(), "rust.test", "newer"),
            ],
        );
        let progress_path = agent_root.dreamd_dir().join(INDEX_PROGRESS_FILENAME);
        std::fs::create_dir_all(agent_root.dreamd_dir()).unwrap();
        write_progress(
            &progress_path,
            &IndexProgress {
                last_indexed_id: Some(id_a.as_str().to_owned()),
            },
        )
        .unwrap();

        let report = assess_index_freshness(&agent_root).expect("assess");
        assert!(report.stale, "jsonl tail ahead of watermark: {report:?}");
        assert_eq!(report.unindexed_count, 1);
        assert_eq!(report.jsonl_tail_id.as_deref(), Some(id_b.as_str()));
    }
}
