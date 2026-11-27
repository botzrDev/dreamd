//! Merge away the bytes of deleted index documents.
//!
//! A Tantivy delete writes a tombstone. The document's stored fields and its
//! terms stay in its segment's files until a merge rewrites that segment, so
//! after `dreamd forget` prunes an event, its text can still be read from
//! `.dreamd/index/`. [`compact_deletes`] merges every segment that holds a
//! deleted document, then removes the files the merge replaced. The merge
//! keeps every live document and drops every term no live document uses.
//!
//! It opens its own writer, so the caller must have shut down any
//! [`crate::server::TantivyIndexHandle`] on the same index first, and must
//! hold the project writer lock.

use std::path::PathBuf;

use tantivy::directory::MmapDirectory;
use tantivy::index::SegmentId;
use tantivy::indexer::NoMergePolicy;
use tantivy::{Index, IndexWriter, TantivyDocument};

use crate::layout::AgentRoot;
use crate::server::index_map::{tantivy_io_to_index, tantivy_to_index, IndexError};
use crate::server::tantivy_handle::{INDEX_DIR_NAME, WRITER_HEAP_BYTES};
use crate::writer_lock::WriterLock;

/// What [`compact_deletes`] found and did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Compaction {
    /// The index directory exists.
    pub present: bool,
    /// Segments that held deleted documents and were merged.
    pub merged: usize,
    /// Segments that still hold deleted documents after the merge.
    pub segments_with_deletes: usize,
    /// Files the merge replaced that could not be removed (on Windows, a
    /// file another process has mapped).
    pub files_not_removed: Vec<PathBuf>,
}

impl Compaction {
    /// No deleted document's bytes are known to be left in the index files.
    pub fn is_clean(&self) -> bool {
        self.segments_with_deletes == 0 && self.files_not_removed.is_empty()
    }
}

/// Merge every segment of the project's index that holds a deleted document,
/// then garbage-collect the files no segment uses. An absent index directory
/// is left absent and reported as not present.
pub fn compact_deletes(
    agent_root: &AgentRoot,
    lock: &WriterLock,
) -> Result<Compaction, IndexError> {
    if !lock.covers(agent_root) {
        return Err(IndexError::WriterLock(format!(
            "the project writer lock held by this process does not cover {}",
            agent_root.project_root().display()
        )));
    }
    let dir = agent_root.dreamd_dir().join(INDEX_DIR_NAME);
    if !dir.is_dir() {
        return Ok(Compaction::default());
    }
    let index = Index::open(MmapDirectory::open(&dir).map_err(tantivy_io_to_index)?)
        .map_err(tantivy_to_index)?;
    let with_deletes = |index: &Index| -> Result<Vec<SegmentId>, IndexError> {
        Ok(index
            .searchable_segment_metas()
            .map_err(tantivy_to_index)?
            .iter()
            .filter(|meta| meta.has_deletes())
            .map(|meta| meta.id())
            .collect())
    };
    let ids = with_deletes(&index)?;

    let mut writer: IndexWriter<TantivyDocument> = index
        .writer_with_num_threads(1, WRITER_HEAP_BYTES)
        .map_err(tantivy_to_index)?;
    // Only the merge below: no other merge is started while this writer runs.
    writer.set_merge_policy(Box::new(NoMergePolicy));
    if !ids.is_empty() {
        writer.merge(&ids).wait().map_err(tantivy_to_index)?;
    }
    let gc = writer
        .garbage_collect_files()
        .wait()
        .map_err(tantivy_to_index)?;
    writer.wait_merging_threads().map_err(tantivy_to_index)?;

    Ok(Compaction {
        present: true,
        merged: ids.len(),
        segments_with_deletes: with_deletes(&index)?.len(),
        files_not_removed: gc.failed_to_delete_files,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    use chrono::DateTime;
    use dreamd_protocol::{AgentLearning, EventId};

    use crate::episodic;
    use crate::server::{TantivyIndexHandle, DEFAULT_COMMIT_CADENCE};
    use crate::test_support::{unique_tmpdir, DirGuard};
    use crate::writer_lock::WriterKind;

    const NOW_SEC: i64 = 1_751_500_800;
    const MARK: &str = "compactmarkerq9";

    fn learning(suffix: char, content: &str) -> AgentLearning {
        AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: EventId::parse(&format!("evt_01ARZ3NDEKTSV4RRFFQ69G5FA{suffix}")).unwrap(),
            timestamp: DateTime::from_timestamp(NOW_SEC, 0).unwrap(),
            pain: 6.0,
            importance: 7.0,
            pinned: false,
            skill_action: "rust::forget".to_string(),
            source_harness: "test-harness".to_string(),
            content: content.to_string(),
        }
    }

    fn index_files_holding(dir: &Path, needle: &str) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .filter(|p| crate::copies::holds(&fs::read(p).unwrap(), needle.as_bytes()))
            .collect()
    }

    fn merge_all(index_dir: &Path) {
        let index = Index::open(MmapDirectory::open(index_dir).unwrap()).unwrap();
        let ids: Vec<SegmentId> = index
            .searchable_segment_ids()
            .unwrap()
            .into_iter()
            .collect();
        let mut writer: IndexWriter<TantivyDocument> =
            index.writer_with_num_threads(1, WRITER_HEAP_BYTES).unwrap();
        writer.set_merge_policy(Box::new(NoMergePolicy));
        if ids.len() > 1 {
            writer.merge(&ids).wait().unwrap();
        }
        writer.garbage_collect_files().wait().unwrap();
        writer.wait_merging_threads().unwrap();
    }

    #[test]
    fn a_pruned_event_leaves_no_bytes_after_compaction() {
        let dir = unique_tmpdir("index-compact");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        fs::create_dir_all(root.episodic_dir()).unwrap();
        let rows = [learning('A', MARK), learning('B', "kept row")];
        let bytes: String = rows
            .iter()
            .map(|r| format!("{}\n", serde_json::to_string(r).unwrap()))
            .collect();
        episodic::rewrite_atomic(&root.episodic_jsonl(), bytes.as_bytes()).unwrap();
        let lock = WriterLock::acquire(&root, WriterKind::Forget).unwrap();
        let index_dir = root.dreamd_dir().join(INDEX_DIR_NAME);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let handle = TantivyIndexHandle::open(&root, &lock, DEFAULT_COMMIT_CADENCE).unwrap();
            handle.shutdown().await.unwrap();
        });
        // Each indexing thread writes its own segment. Merge them, so A shares
        // a segment with B: a segment whose every document is deleted is
        // dropped at the commit, and the case under test is the one where it
        // is not.
        merge_all(&index_dir);
        assert!(!index_files_holding(&index_dir, MARK).is_empty());

        rt.block_on(async {
            let handle = TantivyIndexHandle::open(&root, &lock, DEFAULT_COMMIT_CADENCE).unwrap();
            handle
                .prune_decayed_events(vec![rows[0].id.clone()])
                .await
                .unwrap();
            handle.shutdown().await.unwrap();
        });
        // The delete is a tombstone: the text is still in the segment files.
        assert!(!index_files_holding(&index_dir, MARK).is_empty());

        let out = compact_deletes(&root, &lock).unwrap();

        assert!(out.present && out.merged >= 1 && out.is_clean(), "{out:?}");
        assert!(index_files_holding(&index_dir, MARK).is_empty());
        assert!(!index_files_holding(&index_dir, "kept").is_empty());
        // The index still opens, and B is still in it.
        rt.block_on(async {
            let handle = TantivyIndexHandle::open(&root, &lock, DEFAULT_COMMIT_CADENCE).unwrap();
            handle.shutdown().await.unwrap();
        });
    }

    #[test]
    fn an_absent_index_is_left_absent() {
        let dir = unique_tmpdir("index-compact-absent");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        fs::create_dir_all(root.episodic_dir()).unwrap();
        let lock = WriterLock::acquire(&root, WriterKind::Forget).unwrap();

        let out = compact_deletes(&root, &lock).unwrap();

        assert_eq!(out, Compaction::default());
        assert!(!root.dreamd_dir().join(INDEX_DIR_NAME).exists());
    }
}
