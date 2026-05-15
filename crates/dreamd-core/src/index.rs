//! Tantivy schema, layer enum, and index manifest for dreamd.
//!
//! This module defines the index schema only. It does not open a
//! Tantivy index, write to disk, or implement `IndexHandle`. Those
//! responsibilities belong to WEG-42 (`TantivyIndexHandle`) and the
//! indexer entry point that will live in `dreamd-core::server`.

use serde::{Deserialize, Serialize};
use tantivy::schema::{Field, Schema, FAST, INDEXED, STORED, STRING, TEXT};

/// On-disk index schema version. Bump only with a matching `dreamd
/// migrate` path (see CLAUDE.md "Schema versioning is mandatory").
///
/// WEG-49 (DR-210) is the startup gate that compares this constant to
/// the value carried on the per-project index manifest and refuses to
/// start on mismatch.
pub const SCHEMA_VERSION: &str = "index/1.0";

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
    pub content: Field,
    pub timestamp_sec: Field,
    pub pain: Field,
    pub importance: Field,
    pub recurrence: Field,
    pub layer: Field,
    pub last_updated_sec: Field,
    pub cited_event_count: Field,
}

/// Build the canonical dreamd Tantivy schema.
///
/// See PRD Tech Schemas §3.1 and PRD Part IV §4 for the field list.
/// CLAUDE.md load-bearing decision #2 binds the salience-at-query-time
/// invariant — no indexed score field, only FastFields the collector
/// reads at query time.
pub fn build_schema() -> (Schema, SchemaFields) {
    let mut b = Schema::builder();
    let content           = b.add_text_field("content", TEXT);
    let timestamp_sec     = b.add_u64_field("timestamp_sec", INDEXED | FAST);
    let pain              = b.add_f64_field("pain", FAST);
    let importance        = b.add_f64_field("importance", FAST);
    let recurrence        = b.add_u64_field("recurrence", FAST);
    let layer             = b.add_text_field("layer", STRING | STORED);
    let last_updated_sec  = b.add_u64_field("last_updated_sec", FAST);
    let cited_event_count = b.add_u64_field("cited_event_count", FAST);

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
    fn content_is_indexed_not_stored() {
        let (schema, fields) = build_schema();
        let entry = schema.get_field_entry(fields.content);
        match entry.field_type() {
            FieldType::Str(opts) => {
                assert!(!opts.is_stored(), "content should not be stored");
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

    #[test]
    fn manifest_roundtrip() {
        let original = IndexManifest::current();
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: IndexManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
        assert_eq!(deserialized.schema_version, "index/1.0");
    }

    #[test]
    fn schema_version_constant_value() {
        assert_eq!(SCHEMA_VERSION, "index/1.0");
    }
}
