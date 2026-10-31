//! Index freshness (WEG-42, BZR-170).
//!
//! Owns the commit watermark (`index_progress.json`) and two ways to compare
//! the index with the log:
//!
//! - [`assess_index_freshness`] reads the log and the watermark from disk.
//!   Nothing in it opens Tantivy, so it still answers after a crash. Windows
//!   `GET /api/v1/health` uses it.
//! - [`live_index_freshness`] reads the in-memory marks a running coordinator
//!   ([`AppendMark`]) and its indexer ([`CommitMark`]) keep, so Unix
//!   `GET /api/v1/health` reads no file (BZR-1243).

use std::path::Path;

use dreamd_protocol::{AgentLearning, EventId};
use serde::{Deserialize, Serialize};

use crate::io::write_atomic;
use crate::layout::AgentRoot;
use crate::server::index_map::IndexError;

/// Filename of the indexer's commit watermark, under the project's
/// `.agent/.dreamd/` directory (`AgentRoot::dreamd_dir`). The persist step
/// writes it; `dreamd doctor --repair` (AILAB-223) deletes it along with the
/// rest of the rebuildable cache.
pub const INDEX_PROGRESS_FILENAME: &str = "index_progress.json";

/// How far the index lags the episodic log: the body of `GET /api/v1/health`
/// (WEG-42).
///
/// [`assess_index_freshness`] fills it from disk, comparing the JSONL against
/// `index_progress.json`. It reads `stale` when the log has events the
/// watermark has not caught up to yet: the normal ≤[`DEFAULT_COMMIT_CADENCE`](crate::server::tantivy_handle::DEFAULT_COMMIT_CADENCE)
/// window after a live append, a backlog in the indexer channel, or a crash
/// between JSONL `sync_data` and the next Tantivy commit. The coordinator
/// awaits its `send`, so a backlog is queued rather than dropped, and the
/// staleness clears as the indexer drains it. An append is durable once
/// `sync_data` returns; index freshness is best effort and heals when the
/// indexer commits the backlog (or, after a crash, at the next
/// [`TantivyIndexHandle::open`](crate::server::tantivy_handle::TantivyIndexHandle::open) coverage pass).
///
/// The watermark is the largest committed id, so an event merged in below it
/// does not show here. `dreamd doctor` reports the full set difference through
/// [`assess_index_coverage`](crate::server::index_coverage::assess_index_coverage).
///
/// [`live_index_freshness`] fills the same shape from a running daemon's
/// marks (BZR-1243). The field docs give both sources.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IndexFreshness {
    /// `true` when `unindexed_count > 0`.
    pub stale: bool,
    /// On disk, the `id` of the last well-formed JSONL record. From the marks,
    /// [`AppendMark::tail_id`]: the largest id in the log as the daemon last
    /// read or wrote it.
    pub jsonl_tail_id: Option<String>,
    /// The largest committed id: `index_progress.json` on disk,
    /// [`CommitMark::last_indexed_id`] from the marks.
    pub last_indexed_id: Option<String>,
    /// On disk, the JSONL events whose id sorts after the watermark. From the
    /// marks, the appends this daemon made that the index has not committed.
    /// 0 when fresh.
    pub unindexed_count: usize,
}

/// What a coordinator has appended (BZR-1243). Shared with its `Supervisor`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppendMark {
    /// The largest id in the log as the coordinator last read or wrote it.
    pub tail_id: Option<EventId>,
    /// Appends this coordinator has made since it opened. A dedup-key hit is
    /// not an append.
    pub appended: u64,
}

/// What an indexer has committed (BZR-1243). Shared with its handle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitMark {
    /// The persist step's watermark: the largest committed id.
    pub last_indexed_id: Option<EventId>,
    /// `IndexerMsg::Append`s that a successful persist step has covered.
    pub committed_appends: u64,
}

/// `/health` from the marks. Read `commit` before `append`, so `appended` is
/// never older than `committed_appends`.
pub fn live_index_freshness(commit: &CommitMark, append: &AppendMark) -> IndexFreshness {
    let unindexed = append.appended.saturating_sub(commit.committed_appends);
    IndexFreshness {
        stale: unindexed > 0,
        jsonl_tail_id: append.tail_id.as_ref().map(|id| id.as_str().to_owned()),
        last_indexed_id: commit
            .last_indexed_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        unindexed_count: unindexed as usize,
    }
}

/// Assess on-disk index freshness for `agent_root` without opening Tantivy.
///
/// Windows `GET /api/v1/health` uses this to detect recall lag relative to
/// the JSONL source of truth. It reads the whole log; Unix `/health` uses
/// [`live_index_freshness`] instead, and `dreamd doctor` uses
/// [`assess_index_coverage`](crate::server::index_coverage::assess_index_coverage).
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

/// Watermark recording the largest daemon-assigned `EventId` the index has
/// committed. Lives at `<agent_root>/.agent/.dreamd/index_progress.json`. Read at
/// open; written by the persist step after a commit and its ledger edges,
/// never before, and only when it changes. Nothing decides what to index from
/// it: the coverage pass compares ids.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexProgress {
    /// `evt_`-prefixed ULID string of the largest committed episodic id, or
    /// `None` if nothing has been indexed (cold start / empty JSONL).
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
