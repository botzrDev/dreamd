//! Tantivy schema, layer enum, and index manifest for dreamd (WEG-41 / DR-202).
//!
//! Defines the index schema only; it does not open a Tantivy index, write to
//! disk, or implement `IndexHandle`. Those responsibilities belong to WEG-42
//! (`TantivyIndexHandle`) and the indexer that lives in `dreamd-core::server`.
//!
//! Key invariant (ARCHITECTURE.md decision #2): **no indexed score field.** All four
//! salience inputs (`pain`, `importance`, `recurrence`, `timestamp_sec`) are
//! stored as FastFields and reweighted at query time by `collector::recall`.
//! Do NOT add an indexed `score` field without re-reading DR-202 §4.2 and
//! updating `crate::collector::SalienceCollector`.

use serde::{Deserialize, Serialize};
use tantivy::schema::{Field, Schema, FAST, INDEXED, STORED, STRING, TEXT};

/// On-disk index schema version. Bump only with a matching `dreamd
/// migrate` path (see ARCHITECTURE.md "Schema versioning is mandatory").
///
/// WEG-49 (DR-210) is the startup gate that compares this constant to
/// the value carried on the per-project index manifest and refuses to
/// start on mismatch.
// bumped in WEG-43 when content gained STORED
// bumped in WEG-45 when event_id (STRING | STORED) was added for delete-and-re-add
pub const SCHEMA_VERSION: &str = "index/1.2";

/// Canonical field-name strings for the dreamd Tantivy schema.
///
/// Tantivy 0.26 `FastFieldReader::u64` / `::f64` take `&str` field names
/// — not `Field` IDs from [`SchemaFields`]. These constants are the
/// single source of truth used in both `build_schema()` and the
/// salience collector (WEG-43), so a typo cannot drift across the two
/// call sites.
pub const CONTENT_FIELD: &str = "content";
pub const TIMESTAMP_SEC_FIELD: &str = "timestamp_sec";
pub const PAIN_FIELD: &str = "pain";
pub const IMPORTANCE_FIELD: &str = "importance";
pub const RECURRENCE_FIELD: &str = "recurrence";
pub const LAYER_FIELD: &str = "layer";
pub const LAST_UPDATED_SEC_FIELD: &str = "last_updated_sec";
pub const CITED_EVENT_COUNT_FIELD: &str = "cited_event_count";
/// Exact-match indexed field carrying the `EventId` string (`evt_` + ULID).
/// `STRING | STORED` — not tokenized — so `Term::from_field_text` resolves to
/// exactly one document during the delete-and-re-add cycle (WEG-45 / DR-205′).
pub const EVENT_ID_FIELD: &str = "event_id";

/// Memory layer for an indexed document.
///
/// At v0.1, every indexed document carries `Layer::Episodic`. The
/// `Semantic` variant exists for forward-compatibility with WEG-136
/// (DR-211, v0.1.1) which adds the LESSONS.md → Tantivy semantic
/// indexing pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    Episodic,
    Semantic,
}

impl Layer {
    /// Stable lowercase layer name stored in the Tantivy `layer` field and
    /// returned as `source` in recall JSON (`"episodic"` or `"semantic"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Layer::Episodic => "episodic",
            Layer::Semantic => "semantic",
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("unknown layer: {0}")]
pub struct LayerParseError(String);

impl std::str::FromStr for Layer {
    type Err = LayerParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "episodic" => Ok(Layer::Episodic),
            "semantic" => Ok(Layer::Semantic),
            other => Err(LayerParseError(other.to_owned())),
        }
    }
}

/// Field handles for the dreamd Tantivy schema.
///
/// Returned alongside the `Schema` by [`build_schema`] so the indexer
/// (WEG-42) and the custom collector (WEG-43) can address fields by
/// name without re-resolving them through `schema.get_field()` on every
/// document write.
#[derive(Debug, Clone, Copy)]
pub struct SchemaFields {
    /// `TEXT | STORED` -- BM25 source and hydration target (WEG-43 promoted to STORED).
    pub content: Field,
    /// `INDEXED | FAST` -- range queries and salience decay input.
    pub timestamp_sec: Field,
    /// `FAST` only -- 0.0..=10.0 subjective friction score; salience input.
    pub pain: Field,
    /// `FAST` only -- 0.0..=10.0 long-term relevance score; salience input.
    pub importance: Field,
    /// `FAST` only -- cluster occurrence count at index time; salience input.
    pub recurrence: Field,
    /// `STRING | STORED` -- raw-tokenized layer tag, e.g. `"episodic"`. Used
    /// by `collector::recall` to filter results by [`Layer`].
    pub layer: Field,
    /// `FAST` only -- Unix seconds of last cluster reconciliation (future use).
    pub last_updated_sec: Field,
    /// `FAST` only -- number of times this event has been cited (future use).
    pub cited_event_count: Field,
    /// `STRING | STORED` -- daemon-assigned `EventId` (`evt_` + ULID) for
    /// targeted delete-and-re-add during recurrence sidecar application (WEG-45).
    pub event_id: Field,
}

/// Build the canonical dreamd Tantivy schema.
///
/// See PRD Tech Schemas §3.1 and PRD Part IV §4 for the field list.
/// ARCHITECTURE.md load-bearing decision #2 binds the salience-at-query-time
/// invariant — no indexed score field, only FastFields the collector
/// reads at query time.
pub fn build_schema() -> (Schema, SchemaFields) {
    let mut b = Schema::builder();
    // content gains STORED in WEG-43 so recall results can be hydrated
    // back to the original text without a separate JSONL lookup.
    let content = b.add_text_field(CONTENT_FIELD, TEXT | STORED);
    let timestamp_sec = b.add_u64_field(TIMESTAMP_SEC_FIELD, INDEXED | FAST);
    let pain = b.add_f64_field(PAIN_FIELD, FAST);
    let importance = b.add_f64_field(IMPORTANCE_FIELD, FAST);
    let recurrence = b.add_u64_field(RECURRENCE_FIELD, FAST);
    let layer = b.add_text_field(LAYER_FIELD, STRING | STORED);
    let last_updated_sec = b.add_u64_field(LAST_UPDATED_SEC_FIELD, FAST);
    let cited_event_count = b.add_u64_field(CITED_EVENT_COUNT_FIELD, FAST);
    // WEG-45: STRING | STORED (not TEXT) for exact-match delete-and-re-add.
    let event_id = b.add_text_field(EVENT_ID_FIELD, STRING | STORED);

    let schema = b.build();
    (
        schema,
        SchemaFields {
            content,
            timestamp_sec,
            pain,
            importance,
            recurrence,
            layer,
            last_updated_sec,
            cited_event_count,
            event_id,
        },
    )
}

/// On-disk manifest for a per-project Tantivy index.
///
/// **WEG-41 defines the type and constants only.** First-write to disk
/// is owned by WEG-42 (the indexer creates the manifest the first time
/// it opens an index for a project). Startup enforcement — refusing to
/// start when `binary.expected < manifest.version` — is owned by WEG-49.
///
/// Canonical on-disk path (locked here so WEG-42 and WEG-49 agree):
/// `<agent_root>/.dreamd/index_manifest.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexManifest {
    pub schema_version: String,
}

impl IndexManifest {
    /// A manifest carrying the version the current binary expects.
    pub fn current() -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_owned(),
        }
    }
}

/// Relative filename for the index manifest, joined under the project's
/// `.dreamd/` directory. Used by WEG-42 to write and WEG-49 to read.
pub const INDEX_MANIFEST_FILENAME: &str = "index_manifest.json";

/// On-disk sidecar that records per-cluster recurrence counts, written by the
/// dream cycle (WEG-45 / DR-205′). Consumed by
/// [`crate::server::tantivy_handle::TantivyIndexHandle::apply_recurrence_sidecar`]
/// which performs a delete-and-re-add pass to refresh the `recurrence`
/// FastField on all documents in each cluster.
///
/// Canonical path: `<agent_root>/.agent/semantic/recurrence_counts.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecurrenceSidecar {
    pub schema_version: String,
    pub clusters: Vec<ClusterCount>,
}

/// One entry in [`RecurrenceSidecar::clusters`]: maps a `skill_action`
/// clustering key to its authoritative global recurrence count after the
/// current dream cycle.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClusterCount {
    pub skill_action: String,
    pub count: u32,
}

/// Result of a startup manifest version check.
///
/// All three outcomes mean "daemon may proceed." The hard-fail case
/// (manifest newer than binary) is reported via [`ManifestVersionError::TooNew`]
/// rather than a variant on this enum, so callers can `?`-propagate the
/// abort cleanly while pattern-matching only the proceed cases.
#[derive(Debug, PartialEq)]
pub enum ManifestCheckOutcome {
    /// Manifest file absent — project not yet indexed. Caller: log warn + continue.
    Absent,
    /// Schema version matches binary. Caller: proceed.
    Current,
    /// Binary is newer than manifest — migration pending. Caller: log warn + continue.
    NeedsMigration { from: String },
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestVersionError {
    #[error(
        "index schema {manifest:?} is newer than binary {binary:?}; \
         downgrade dreamd or run `dreamd migrate`"
    )]
    TooNew { manifest: String, binary: String },
    #[error("index manifest is corrupt: {0}")]
    Corrupt(#[from] serde_json::Error),
    #[error("reading index manifest: {0}")]
    Io(std::io::Error),
}

/// Read `<agent_root>/.dreamd/index_manifest.json` and compare its
/// `schema_version` against [`SCHEMA_VERSION`].
///
/// Returns [`ManifestVersionError::TooNew`] when the on-disk index was
/// written by a newer binary — the only case that must abort startup.
/// Missing-manifest is reported as [`ManifestCheckOutcome::Absent`] (not
/// an error) so cold starts on a fresh `agent_root` succeed before
/// WEG-42 has written the first manifest.
pub fn check_manifest_version(
    manifest_path: &std::path::Path,
) -> Result<ManifestCheckOutcome, ManifestVersionError> {
    let text = match std::fs::read_to_string(manifest_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ManifestCheckOutcome::Absent);
        }
        Err(e) => return Err(ManifestVersionError::Io(e)),
    };

    let manifest: IndexManifest = serde_json::from_str(&text)?;
    if manifest.schema_version == SCHEMA_VERSION {
        return Ok(ManifestCheckOutcome::Current);
    }

    match (
        parse_index_version(&manifest.schema_version),
        parse_index_version(SCHEMA_VERSION),
    ) {
        (Some(mv), Some(bv)) if mv > bv => Err(ManifestVersionError::TooNew {
            manifest: manifest.schema_version,
            binary: SCHEMA_VERSION.to_owned(),
        }),
        _ => Ok(ManifestCheckOutcome::NeedsMigration {
            from: manifest.schema_version,
        }),
    }
}

/// Parse `"index/MAJOR.MINOR"` → `(MAJOR, MINOR)`. Returns `None` on any
/// parse failure — callers treat unparseable versions as "needs migration"
/// (the fallback in `check_manifest_version`).
fn parse_index_version(s: &str) -> Option<(u32, u32)> {
    let ver = s.strip_prefix("index/")?;
    let (major, minor) = ver.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::schema::FieldType;

    #[test]
    fn build_schema_has_all_fields() {
        let (schema, _) = build_schema();
        for name in &[
            "content",
            "timestamp_sec",
            "pain",
            "importance",
            "recurrence",
            "layer",
            "last_updated_sec",
            "cited_event_count",
        ] {
            assert!(schema.get_field(name).is_ok(), "missing field: {name}");
        }
        assert!(
            schema.get_field("event_id").is_ok(),
            "missing field: event_id"
        );
    }

    #[test]
    fn build_schema_fastfield_types() {
        let (schema, fields) = build_schema();

        for field in [
            fields.timestamp_sec,
            fields.recurrence,
            fields.last_updated_sec,
            fields.cited_event_count,
        ] {
            let entry = schema.get_field_entry(field);
            match entry.field_type() {
                FieldType::U64(opts) => assert!(opts.is_fast(), "u64 field not fast"),
                other => panic!("expected U64, got {other:?}"),
            }
        }

        for field in [fields.pain, fields.importance] {
            let entry = schema.get_field_entry(field);
            match entry.field_type() {
                FieldType::F64(opts) => assert!(opts.is_fast(), "f64 field not fast"),
                other => panic!("expected F64, got {other:?}"),
            }
        }
    }

    #[test]
    fn content_is_indexed_and_stored() {
        // WEG-43 promoted `content` from TEXT to TEXT | STORED so recall
        // results can hydrate the document text without a JSONL lookup.
        let (schema, fields) = build_schema();
        let entry = schema.get_field_entry(fields.content);
        match entry.field_type() {
            FieldType::Str(opts) => {
                assert!(opts.is_stored(), "content should be stored (WEG-43)");
                assert!(
                    opts.get_indexing_options().is_some(),
                    "content should be indexed (BM25-scorable)"
                );
            }
            other => panic!("expected Str, got {other:?}"),
        }
    }

    #[test]
    fn layer_is_string_and_stored() {
        let (schema, fields) = build_schema();
        let entry = schema.get_field_entry(fields.layer);
        match entry.field_type() {
            FieldType::Str(opts) => {
                assert!(opts.is_stored(), "layer should be stored");
                let indexing = opts
                    .get_indexing_options()
                    .expect("layer should be indexed");
                assert_eq!(
                    indexing.tokenizer(),
                    "raw",
                    "layer should use raw tokenizer (STRING, not TEXT)"
                );
            }
            other => panic!("expected Str, got {other:?}"),
        }
    }

    #[test]
    fn layer_roundtrip() {
        assert_eq!(Layer::Episodic.as_str(), "episodic");
        assert_eq!(Layer::Semantic.as_str(), "semantic");
        assert_eq!("episodic".parse::<Layer>().unwrap(), Layer::Episodic);
        assert_eq!("semantic".parse::<Layer>().unwrap(), Layer::Semantic);
        assert!("weird".parse::<Layer>().is_err());
    }

    #[test]
    fn layer_serde_lowercase() {
        assert_eq!(
            serde_json::to_string(&Layer::Episodic).unwrap(),
            r#""episodic""#
        );
        assert_eq!(
            serde_json::to_string(&Layer::Semantic).unwrap(),
            r#""semantic""#
        );
    }

    /// Round-trip `IndexManifest::current()` through JSON and verify the
    /// hardcoded version survives serde without truncation or mutation.
    #[test]
    fn manifest_roundtrip() {
        let original = IndexManifest::current();
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: IndexManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
        assert_eq!(deserialized.schema_version, "index/1.2");
    }

    /// `SCHEMA_VERSION` must equal the hardcoded string so accidental bumps
    /// (e.g. whitespace, prefix change) are caught before they reach CI.
    #[test]
    fn schema_version_constant_value() {
        assert_eq!(SCHEMA_VERSION, "index/1.2");
    }

    #[test]
    fn manifest_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index_manifest.json");
        let outcome = check_manifest_version(&path).unwrap();
        assert_eq!(outcome, ManifestCheckOutcome::Absent);
    }

    #[test]
    fn manifest_current() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index_manifest.json");
        std::fs::write(&path, r#"{"schema_version":"index/1.2"}"#).unwrap();
        let outcome = check_manifest_version(&path).unwrap();
        assert_eq!(outcome, ManifestCheckOutcome::Current);
    }

    #[test]
    fn manifest_needs_migration() {
        // index/1.1 was the pre-WEG-45 schema (no event_id field); any
        // on-disk index at 1.1 must be rebuilt before delete-and-re-add works.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index_manifest.json");
        std::fs::write(&path, r#"{"schema_version":"index/1.1"}"#).unwrap();
        let outcome = check_manifest_version(&path).unwrap();
        assert_eq!(
            outcome,
            ManifestCheckOutcome::NeedsMigration {
                from: "index/1.1".to_owned(),
            }
        );
    }

    #[test]
    fn manifest_too_new() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index_manifest.json");
        std::fs::write(&path, r#"{"schema_version":"index/2.0"}"#).unwrap();
        let err = check_manifest_version(&path).unwrap_err();
        match err {
            ManifestVersionError::TooNew { manifest, binary } => {
                assert_eq!(manifest, "index/2.0");
                assert_eq!(binary, "index/1.2");
            }
            other => panic!("expected TooNew, got {other:?}"),
        }
    }

    #[test]
    fn manifest_corrupt_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index_manifest.json");
        std::fs::write(&path, "not json").unwrap();
        let result = check_manifest_version(&path);
        assert!(matches!(result, Err(ManifestVersionError::Corrupt(_))));
    }

    #[test]
    fn parse_index_version_valid() {
        assert_eq!(parse_index_version("index/1.0"), Some((1, 0)));
    }

    #[test]
    fn parse_index_version_no_prefix() {
        assert_eq!(parse_index_version("1.0"), None);
    }

    #[test]
    fn parse_index_version_bad_semver() {
        assert_eq!(parse_index_version("index/x.y"), None);
    }

    #[test]
    fn cluster_count_default() {
        let cc = ClusterCount::default();
        assert_eq!(cc.skill_action, "");
        assert_eq!(cc.count, 0);
    }
}
