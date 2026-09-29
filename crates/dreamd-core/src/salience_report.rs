//! BZR-195 — salience distribution over the whole index.
//!
//! Shared by `GET /api/v1/observability/salience` and `dreamd salience-drift`.
//! Every episodic document is scored with [`super::salience`] alone — there is
//! no query, so there is no BM25 term. `score_by_salience` is not used: it is a
//! top-k collector and a histogram needs every doc.
//!
//! Lesson documents (`event_id` starting with `lsn_`) are derived, not events,
//! and are skipped.
//!
//! Each call writes `salience-<UTC date>.json` under the observability
//! directory, overwriting that day's file. `drift` compares today's mean with
//! the file dated exactly seven UTC days earlier; any other day leaves it null.
//! The file is a gitignored cache written with [`std::fs::write`] —
//! the atomic-write helper in `io.rs` is `Unsupported` on Windows.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tantivy::collector::{Collector, SegmentCollector};
use tantivy::columnar::Column;
use tantivy::query::AllQuery;
use tantivy::schema::{Field, TantivyDocument, Value};
use tantivy::store::StoreReader;
use tantivy::{DocId, IndexReader, Score, SegmentOrdinal, SegmentReader};

use crate::index::{
    EVENT_ID_FIELD, IMPORTANCE_FIELD, PAIN_FIELD, RECURRENCE_FIELD, SKILL_ACTION_FIELD,
    TIMESTAMP_SEC_FIELD,
};

use super::salience;

/// Report schema token. Not episodic `"1.0.0"`, not daemon `"1.0"`.
pub const REPORT_SCHEMA_VERSION: &str = "observability/1.0";

/// Number of histogram buckets. Index `i` is `[i/10, (i+1)/10)`; the last is
/// `[0.9, +∞)`.
pub const HISTOGRAM_BUCKETS: usize = 10;

/// At most this many clusters in `top_clusters`.
pub const TOP_CLUSTERS: usize = 5;

/// Days back to the snapshot `drift` compares against.
pub const DRIFT_DAYS: u64 = 7;

/// Prefix of derived lesson documents, excluded from the report.
const LESSON_ID_PREFIX: &str = "lsn_";

/// One cluster in `top_clusters`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterSalience {
    pub skill_action: String,
    pub count: u64,
    pub mean: f64,
}

/// The HTTP body and the `--json` output. Field order is the wire order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SalienceReport {
    pub schema_version: String,
    pub date: String,
    pub total: u64,
    pub mean: f64,
    pub histogram: [u64; HISTOGRAM_BUCKETS],
    pub top_clusters: Vec<ClusterSalience>,
    /// Today's mean minus the mean in `salience-<drift_against>.json`; `None`
    /// when that exact file is absent or does not parse.
    pub drift: Option<f64>,
    /// `date` minus [`DRIFT_DAYS`], present even when `drift` is null.
    pub drift_against: String,
}

/// The on-disk snapshot: the measurement without `drift` / `drift_against`.
#[derive(Serialize)]
struct Snapshot<'a> {
    schema_version: &'a str,
    date: &'a str,
    total: u64,
    mean: f64,
    histogram: &'a [u64; HISTOGRAM_BUCKETS],
    top_clusters: &'a [ClusterSalience],
}

/// The only field `drift` reads back from an older snapshot.
#[derive(Deserialize)]
struct SnapshotMean {
    mean: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    #[error("index scan failed: {0}")]
    Search(#[from] tantivy::TantivyError),
    #[error("snapshot write failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("snapshot encode failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("timestamp {0} is out of range")]
    Timestamp(i64),
}

/// Snapshot file name for a `YYYY-MM-DD` date.
#[must_use]
pub fn snapshot_file_name(date: &str) -> String {
    format!("salience-{date}.json")
}

/// Histogram bucket for a score. Negative (and NaN) scores go in bucket 0.
fn bucket(score: f64) -> usize {
    if score.is_nan() || score <= 0.0 {
        return 0;
    }
    ((score * 10.0).floor() as usize).min(HISTOGRAM_BUCKETS - 1)
}

/// Build the report over every episodic doc, write today's snapshot, and fill
/// `drift` from the snapshot dated exactly [`DRIFT_DAYS`] earlier.
pub fn build_report(
    reader: &IndexReader,
    now_sec: i64,
    observability_dir: &Path,
) -> Result<SalienceReport, ReportError> {
    let today = chrono::DateTime::from_timestamp(now_sec, 0)
        .ok_or(ReportError::Timestamp(now_sec))?
        .date_naive();
    let against = today
        .checked_sub_days(chrono::Days::new(DRIFT_DAYS))
        .ok_or(ReportError::Timestamp(now_sec))?;
    let date = today.format("%Y-%m-%d").to_string();
    let drift_against = against.format("%Y-%m-%d").to_string();

    let scored = reader
        .searcher()
        .search(&AllQuery, &FullScanCollector { now_sec })?;

    let mut histogram = [0u64; HISTOGRAM_BUCKETS];
    let mut sum = 0.0;
    let mut clusters: BTreeMap<String, (u64, f64)> = BTreeMap::new();
    for (skill_action, score) in &scored {
        histogram[bucket(*score)] += 1;
        sum += score;
        let entry = clusters.entry(skill_action.clone()).or_insert((0, 0.0));
        entry.0 += 1;
        entry.1 += score;
    }
    let total = scored.len() as u64;
    let mean = if total == 0 { 0.0 } else { sum / total as f64 };

    // BTreeMap iterates skill_action ascending; the stable sort keeps that as
    // the tie-break under mean descending.
    let mut top_clusters: Vec<ClusterSalience> = clusters
        .into_iter()
        .map(|(skill_action, (count, sum))| ClusterSalience {
            skill_action,
            count,
            mean: sum / count as f64,
        })
        .collect();
    top_clusters.sort_by(|a, b| b.mean.total_cmp(&a.mean));
    top_clusters.truncate(TOP_CLUSTERS);

    let snapshot = Snapshot {
        schema_version: REPORT_SCHEMA_VERSION,
        date: &date,
        total,
        mean,
        histogram: &histogram,
        top_clusters: &top_clusters,
    };
    let mut bytes = serde_json::to_vec(&snapshot)?;
    bytes.push(b'\n');
    std::fs::create_dir_all(observability_dir)?;
    std::fs::write(observability_dir.join(snapshot_file_name(&date)), bytes)?;

    let drift = std::fs::read(observability_dir.join(snapshot_file_name(&drift_against)))
        .ok()
        .and_then(|b| serde_json::from_slice::<SnapshotMean>(&b).ok())
        .map(|old| mean - old.mean);

    Ok(SalienceReport {
        schema_version: REPORT_SCHEMA_VERSION.to_string(),
        date,
        total,
        mean,
        histogram,
        top_clusters,
        drift,
        drift_against,
    })
}

/// Visits every doc and yields `(skill_action, salience)` for each episodic one.
struct FullScanCollector {
    now_sec: i64,
}

struct FullScanSegmentCollector {
    timestamp_col: Column<u64>,
    pain_col: Column<f64>,
    importance_col: Column<f64>,
    recurrence_col: Column<u64>,
    store: StoreReader,
    event_id: Field,
    skill_action: Field,
    now_sec: i64,
    out: Vec<(String, f64)>,
}

impl SegmentCollector for FullScanSegmentCollector {
    type Fruit = Vec<(String, f64)>;

    fn collect(&mut self, doc: DocId, _score: Score) {
        // A store read error skips the doc; it does not fail the report.
        let Ok(stored) = self.store.get::<TantivyDocument>(doc) else {
            return;
        };
        let text = |field| {
            stored
                .get_first(field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        if text(self.event_id).starts_with(LESSON_ID_PREFIX) {
            return;
        }

        // Same fastfield reads as `collector.rs`.
        let ts = self.timestamp_col.first(doc).unwrap_or(0) as i64;
        let p = self.pain_col.first(doc).unwrap_or(0.0);
        let imp = self.importance_col.first(doc).unwrap_or(0.0);
        let rec = self.recurrence_col.first(doc).unwrap_or(0);

        let score = salience(self.now_sec, ts, p, imp, rec);
        self.out.push((text(self.skill_action), score));
    }

    fn harvest(self) -> Self::Fruit {
        self.out
    }
}

impl Collector for FullScanCollector {
    type Fruit = Vec<(String, f64)>;
    type Child = FullScanSegmentCollector;

    fn for_segment(
        &self,
        _segment_local_id: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<FullScanSegmentCollector> {
        let ff = segment.fast_fields();
        let schema = segment.schema();
        Ok(FullScanSegmentCollector {
            timestamp_col: ff.u64(TIMESTAMP_SEC_FIELD)?,
            pain_col: ff.f64(PAIN_FIELD)?,
            importance_col: ff.f64(IMPORTANCE_FIELD)?,
            recurrence_col: ff.u64(RECURRENCE_FIELD)?,
            store: segment.get_store_reader(1)?,
            event_id: schema.get_field(EVENT_ID_FIELD)?,
            skill_action: schema.get_field(SKILL_ACTION_FIELD)?,
            now_sec: self.now_sec,
            out: Vec::new(),
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(&self, fruits: Vec<Vec<(String, f64)>>) -> tantivy::Result<Self::Fruit> {
        Ok(fruits.into_iter().flatten().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{build_schema, Layer};
    use tantivy::{doc, Index};

    /// 2026-09-29T12:00:00Z.
    const NOW_SEC: i64 = 1_790_683_200;
    const DAY_SECS: i64 = 86_400;

    /// `(event_id, skill_action, timestamp, pain, importance, recurrence)`.
    type Row<'a> = (&'a str, &'a str, i64, f64, f64, u64);

    fn build_index(rows: &[Row]) -> (Index, IndexReader) {
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema);
        let mut writer = index.writer(15_000_000).unwrap();
        for &(id, skill, ts, pain, imp, rec) in rows {
            let layer = if id.starts_with(LESSON_ID_PREFIX) {
                Layer::Semantic
            } else {
                Layer::Episodic
            };
            writer
                .add_document(doc!(
                    fields.content => "body",
                    fields.timestamp_sec => ts as u64,
                    fields.pain => pain,
                    fields.importance => imp,
                    fields.recurrence => rec,
                    fields.layer => layer.as_str(),
                    fields.event_id => id,
                    fields.skill_action => skill,
                ))
                .unwrap();
        }
        writer.commit().unwrap();
        let reader = index.reader().unwrap();
        reader.reload().unwrap();
        (index, reader)
    }

    fn write_snapshot_mean(dir: &Path, date: &str, mean: f64) {
        std::fs::create_dir_all(dir).unwrap();
        let body = format!(
            "{{\"schema_version\":\"observability/1.0\",\"date\":\"{date}\",\"total\":1,\
             \"mean\":{mean},\"histogram\":[1,0,0,0,0,0,0,0,0,0],\"top_clusters\":[]}}\n"
        );
        std::fs::write(dir.join(snapshot_file_name(date)), body).unwrap();
    }

    #[test]
    fn hand_computed_doc_lands_in_its_bucket_and_lsn_is_excluded() {
        // age 14 days, pain 10, importance 10, recurrence 0 → e^-1 ≈ 0.3679.
        let (_idx, reader) = build_index(&[
            (
                "evt_a",
                "rust::tokio",
                NOW_SEC - 14 * DAY_SECS,
                10.0,
                10.0,
                0,
            ),
            ("lsn_evt_a", "rust::tokio", NOW_SEC, 10.0, 10.0, 5),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let report = build_report(&reader, NOW_SEC, dir.path()).unwrap();

        let expected = salience(NOW_SEC, NOW_SEC - 14 * DAY_SECS, 10.0, 10.0, 0);
        assert!((expected - (-1.0_f64).exp()).abs() < 1e-12);
        assert_eq!(report.total, 1, "lsn_ doc must not count");
        assert!((report.mean - expected).abs() < 1e-12);
        let mut want = [0u64; HISTOGRAM_BUCKETS];
        want[3] = 1;
        assert_eq!(report.histogram, want);
        assert_eq!(report.top_clusters.len(), 1);
        assert_eq!(report.top_clusters[0].skill_action, "rust::tokio");
        assert_eq!(report.top_clusters[0].count, 1);
        assert_eq!(report.schema_version, "observability/1.0");
        assert_eq!(report.date, "2026-09-29");
        assert_eq!(report.drift_against, "2026-09-22");
    }

    #[test]
    fn bucket_edges() {
        assert_eq!(bucket(-0.5), 0);
        assert_eq!(bucket(0.0), 0);
        assert_eq!(bucket(0.0999), 0);
        assert_eq!(bucket(0.1), 1);
        assert_eq!(bucket(0.8999), 8);
        assert_eq!(bucket(0.9), 9);
        assert_eq!(bucket(3.7), 9);
    }

    #[test]
    fn empty_index_is_all_zero() {
        let (_idx, reader) = build_index(&[]);
        let dir = tempfile::tempdir().unwrap();
        let report = build_report(&reader, NOW_SEC, dir.path()).unwrap();
        assert_eq!(report.total, 0);
        assert_eq!(report.mean, 0.0);
        assert_eq!(report.histogram, [0u64; HISTOGRAM_BUCKETS]);
        assert!(report.top_clusters.is_empty());
        assert_eq!(report.drift, None);
    }

    #[test]
    fn top_clusters_sort_by_mean_then_name_and_cap_at_five() {
        let ts = NOW_SEC;
        let (_idx, reader) = build_index(&[
            ("evt_1", "b", ts, 5.0, 5.0, 0),
            ("evt_2", "a", ts, 5.0, 5.0, 0),
            ("evt_3", "c", ts, 9.0, 9.0, 0),
            ("evt_4", "c", ts, 1.0, 1.0, 0),
            ("evt_5", "d", ts, 2.0, 2.0, 0),
            ("evt_6", "e", ts, 1.0, 1.0, 0),
            ("evt_7", "f", ts, 1.0, 1.0, 0),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let report = build_report(&reader, NOW_SEC, dir.path()).unwrap();
        let names: Vec<&str> = report
            .top_clusters
            .iter()
            .map(|c| c.skill_action.as_str())
            .collect();
        // c: (0.81 + 0.01)/2 = 0.41; a = b = 0.25; d = 0.04; e = f = 0.01.
        assert_eq!(names, vec!["c", "a", "b", "d", "e"]);
        assert_eq!(report.top_clusters[0].count, 2);
    }

    #[test]
    fn same_day_calls_leave_one_snapshot_without_drift_fields() {
        let (_idx, reader) = build_index(&[("evt_a", "x", NOW_SEC, 5.0, 5.0, 0)]);
        let dir = tempfile::tempdir().unwrap();
        build_report(&reader, NOW_SEC, dir.path()).unwrap();
        build_report(&reader, NOW_SEC + 3600, dir.path()).unwrap();

        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(files, vec!["salience-2026-09-29.json".to_string()]);

        let body = std::fs::read_to_string(dir.path().join("salience-2026-09-29.json")).unwrap();
        assert!(body.ends_with("}\n") && !body.ends_with("\n\n"));
        assert!(body.starts_with(
            "{\"schema_version\":\"observability/1.0\",\"date\":\"2026-09-29\",\"total\":1,"
        ));
        assert!(
            !body.contains("drift"),
            "snapshot omits drift fields: {body}"
        );
    }

    #[test]
    fn drift_uses_the_file_dated_exactly_seven_days_earlier() {
        let (_idx, reader) = build_index(&[("evt_a", "x", NOW_SEC, 5.0, 5.0, 0)]);
        let dir = tempfile::tempdir().unwrap();
        write_snapshot_mean(dir.path(), "2026-09-22", 0.05);

        let report = build_report(&reader, NOW_SEC, dir.path()).unwrap();
        let drift = report.drift.expect("7-day file present");
        assert!((drift - (report.mean - 0.05)).abs() < 1e-12);
        assert!((drift - 0.20).abs() < 1e-12);
    }

    #[test]
    fn drift_is_null_without_the_seven_day_file() {
        let (_idx, reader) = build_index(&[("evt_a", "x", NOW_SEC, 5.0, 5.0, 0)]);
        let dir = tempfile::tempdir().unwrap();
        let report = build_report(&reader, NOW_SEC, dir.path()).unwrap();
        assert_eq!(report.drift, None);
        assert_eq!(report.drift_against, "2026-09-22");

        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"drift\":null,\"drift_against\":\"2026-09-22\"}"));
    }

    #[test]
    fn drift_ignores_six_and_eight_day_files() {
        let (_idx, reader) = build_index(&[("evt_a", "x", NOW_SEC, 5.0, 5.0, 0)]);
        let dir = tempfile::tempdir().unwrap();
        write_snapshot_mean(dir.path(), "2026-09-23", 0.05);
        write_snapshot_mean(dir.path(), "2026-09-21", 0.05);
        let report = build_report(&reader, NOW_SEC, dir.path()).unwrap();
        assert_eq!(report.drift, None);
    }

    #[test]
    fn report_json_field_order() {
        let (_idx, reader) = build_index(&[]);
        let dir = tempfile::tempdir().unwrap();
        let report = build_report(&reader, NOW_SEC, dir.path()).unwrap();
        let json = serde_json::to_string(&report).unwrap();
        assert_eq!(
            json,
            "{\"schema_version\":\"observability/1.0\",\"date\":\"2026-09-29\",\"total\":0,\
             \"mean\":0.0,\"histogram\":[0,0,0,0,0,0,0,0,0,0],\"top_clusters\":[],\
             \"drift\":null,\"drift_against\":\"2026-09-22\"}"
        );
    }
}
