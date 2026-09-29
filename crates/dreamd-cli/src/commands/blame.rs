//! `dreamd blame <query>` — show which stored memories drove a query's ranking (BZR-193).
//!
//! One-shot, read-only. Copies the discover / `Index::open_in_dir` (never the
//! writer-spawning `TantivyIndexHandle` open constructor) / `dreamd_core::recall`
//! sequence from [`crate::commands::recall::run`] and reuses its
//! [`crate::commands::recall::RecallError`], [`crate::commands::recall::truncate_content`],
//! and [`crate::commands::recall::explain_factors`] — this module never edits
//! `commands/recall.rs` or `dreamd_core::salience`, and never re-derives the
//! DR-204 arithmetic.
//!
//! The rendered shape is a different table than `dreamd recall`: `id`,
//! `timestamp`, `skill_action`, `content`, `bm25`, `salience`, `total`, `layer`
//! (no `rank`, no `age_days`/`pain`/`recurrence` columns — those live only under
//! `--explain`). Default `-k` is **5** (recall stays 10). `--json` prints one
//! compact JSON array instead of the markdown table.

use std::io::Write;
use std::path::Path;

use dreamd_core::{AgentRoot, LayoutError, RecallResult};
use tantivy::Index;

use crate::commands::recall::{explain_factors, truncate_content, RecallError};

/// Directory name of the per-project Tantivy index under `.agent/.dreamd/`.
///
/// Mirrors `dreamd_core::server::tantivy_handle::INDEX_DIR_NAME`, which is
/// `pub(crate)` and so not importable here — same duplication as
/// `commands::recall` and `commands::score`.
const INDEX_DIR_NAME: &str = "index";

/// Max width of the rendered `content` cell, in characters. Blame's cap (80) is
/// wider than recall's (60) — this is a separate table with its own column set.
const CONTENT_WIDTH: usize = 80;

/// Header row of the blame table. Column order is locked by the AC (BZR-193):
/// `id, timestamp, skill_action, content, bm25, salience, total, layer`.
const HEADER_LINE: &str =
    "| id | timestamp | skill_action | content | bm25 | salience | total | layer |";
/// GitHub-flavored-markdown separator row (eight columns).
const SEPARATOR_LINE: &str = "| --- | --- | --- | --- | --- | --- | --- | --- |";

/// Render the markdown blame table (and, when `explain`, the per-hit DR-204
/// factor blocks) for `results`. Pure and deterministic given `now_sec`.
///
/// The header + separator always print (even for zero hits) so the columns
/// stay visible; the caller still exits 0 on an empty result set.
pub fn render_report(results: &[RecallResult], now_sec: i64, explain: bool) -> String {
    let mut out = String::new();
    out.push_str(HEADER_LINE);
    out.push('\n');
    out.push_str(SEPARATOR_LINE);
    out.push('\n');

    for r in results {
        let content = truncate_content(&r.content, CONTENT_WIDTH);
        out.push_str(&format!(
            "| {id} | {ts} | {skill} | {content} | {bm25:.4} | {sal:.4} | {total:.4} | {layer} |\n",
            id = r.event_id,
            ts = r.timestamp_sec,
            skill = r.skill_action,
            bm25 = r.bm25,
            sal = r.salience,
            total = r.score,
            layer = r.layer.as_str(),
        ));
    }

    if results.is_empty() {
        out.push_str("(0 hits)\n");
    }

    if explain {
        out.push('\n');
        out.push_str("explain (DR-204 salience factors):\n");
        for (i, r) in results.iter().enumerate() {
            let rank = i + 1;
            let f = explain_factors(r, now_sec);
            out.push_str(&format!("[#{rank}] id={}\n", r.event_id));
            out.push_str(&format!("  age_days               = {:.4}\n", f.age_days));
            out.push_str(&format!("  exp(-age_days/14)      = {:.6}\n", f.decay));
            out.push_str(&format!(
                "  pain/10                = {:.6}\n",
                f.pain_factor
            ));
            out.push_str(&format!(
                "  importance/10          = {:.6}\n",
                f.importance_factor
            ));
            out.push_str(&format!(
                "  (1 + ln(1+recurrence)) = {:.6}\n",
                f.recurrence_factor
            ));
            out.push_str(&format!(
                "  salience (product)     = {:.6}\n",
                f.salience_product
            ));
            out.push_str(&format!("  score = bm25 × salience = {:.6}\n", f.score));
        }
    }

    out
}

/// Render `results` as one compact JSON array plus a trailing newline. No
/// markdown on this path. Each object carries the same eight table fields;
/// `--explain` adds the `ExplainFactors` fields (plus the raw, undivided
/// `pain` / `importance` / `recurrence` the table never shows).
pub fn render_json(results: &[RecallResult], now_sec: i64, explain: bool) -> String {
    let arr: Vec<serde_json::Value> = results
        .iter()
        .map(|r| {
            let content = truncate_content(&r.content, CONTENT_WIDTH);
            let mut obj = serde_json::json!({
                "id": r.event_id,
                "timestamp": r.timestamp_sec,
                "skill_action": r.skill_action,
                "content": content,
                "bm25": r.bm25,
                "salience": r.salience,
                "total": r.score,
                "layer": r.layer.as_str(),
            });
            if explain {
                let f = explain_factors(r, now_sec);
                let map = obj.as_object_mut().expect("object literal is always a map");
                map.insert("age_days".to_string(), serde_json::json!(f.age_days));
                map.insert("pain".to_string(), serde_json::json!(r.pain));
                map.insert("importance".to_string(), serde_json::json!(r.importance));
                map.insert("recurrence".to_string(), serde_json::json!(r.recurrence));
                map.insert("decay".to_string(), serde_json::json!(f.decay));
                map.insert("pain_factor".to_string(), serde_json::json!(f.pain_factor));
                map.insert(
                    "importance_factor".to_string(),
                    serde_json::json!(f.importance_factor),
                );
                map.insert(
                    "recurrence_factor".to_string(),
                    serde_json::json!(f.recurrence_factor),
                );
                map.insert(
                    "salience_product".to_string(),
                    serde_json::json!(f.salience_product),
                );
            }
            obj
        })
        .collect();
    let mut s =
        serde_json::to_string(&arr).expect("a Vec<Value> built from json! always serializes");
    s.push('\n');
    s
}

/// `dreamd blame <query>` entry point.
///
/// Discovers the store, opens the index read-only, runs the shared
/// salience-scored recall collector, and writes the rendered report (markdown
/// table, or one compact JSON array when `json`) to `out`. `now_sec` is the
/// query instant used to derive `age_days` and the salience decay under
/// `--explain`. Errors are written to `err` as `dreamd: error — …` and
/// returned typed for exit-code mapping in `cli::run`.
#[allow(clippy::too_many_arguments)] // CLI entry: cwd, query, flags, now_sec, out, err
pub fn run(
    cwd: &Path,
    query: &str,
    k: usize,
    explain: bool,
    json: bool,
    now_sec: i64,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), RecallError> {
    let root = match AgentRoot::discover(cwd) {
        Ok(r) => r,
        Err(LayoutError::NotFound) => {
            writeln!(
                err,
                "dreamd: error — no .agent/ directory found. Run `dreamd init` first."
            )?;
            return Err(RecallError::NotFound);
        }
    };

    let index_dir = root.dreamd_dir().join(INDEX_DIR_NAME);
    if !index_dir.exists() {
        let hint = format!(
            "index not found at {} — append some learnings (or run the daemon once) so \
             the index exists, then retry.",
            index_dir.display()
        );
        writeln!(err, "dreamd: error — {hint}")?;
        return Err(RecallError::IndexUnavailable(hint));
    }

    // Read-only open. Never the daemon's writer-spawning `TantivyIndexHandle`
    // constructor — that starts a writer + indexer task that would race a live
    // daemon holding the same index.
    let index = match Index::open_in_dir(&index_dir) {
        Ok(i) => i,
        Err(e) => {
            let hint = format!(
                "could not open index at {} ({e}) — the index may be empty or not yet \
                 built; append some learnings (or run the daemon once), then retry.",
                index_dir.display()
            );
            writeln!(err, "dreamd: error — {hint}")?;
            return Err(RecallError::IndexUnavailable(hint));
        }
    };
    let reader = index.reader().map_err(RecallError::Search)?;

    let (_schema, fields) = dreamd_core::index::build_schema();
    let results = dreamd_core::recall(&reader, &fields, query, k, None, now_sec)
        .map_err(RecallError::Search)?;

    if json {
        write!(out, "{}", render_json(&results, now_sec, explain))?;
    } else {
        write!(out, "{}", render_report(&results, now_sec, explain))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dreamd_core::index::Layer;
    use dreamd_core::salience::salience;

    const NOW_SEC: i64 = 1_750_000_000;
    const DAY: i64 = 86_400;

    /// Build a synthetic `RecallResult` with `salience`/`score` set consistently
    /// with the DR-204 formula (same helper shape as `commands::recall`'s
    /// test-only `result()`), so `--explain` / `--json` products can be checked
    /// against the stored fields without a live index.
    fn result(
        content: &str,
        event_id: &str,
        ts: i64,
        pain: f64,
        importance: f64,
        recurrence: u64,
        bm25: f64,
    ) -> RecallResult {
        let sal = salience(NOW_SEC, ts, pain, importance, recurrence);
        RecallResult {
            score: bm25 * sal,
            content: content.to_string(),
            timestamp_sec: ts as u64,
            pain,
            importance,
            recurrence,
            layer: Layer::Episodic,
            skill_action: "rust::axum::error_handling".to_string(),
            source_harness: "claude-code".to_string(),
            bm25,
            salience: sal,
            event_id: event_id.to_string(),
        }
    }

    #[test]
    fn table_header_matches_the_locked_blame_columns() {
        let out = render_report(&[], NOW_SEC, false);
        let header = out.lines().next().expect("at least a header line");
        assert_eq!(
            header, "| id | timestamp | skill_action | content | bm25 | salience | total | layer |",
            "column order/labels must match the BZR-193 AC exactly"
        );
    }

    #[test]
    fn empty_results_render_header_and_separator_and_zero_hits() {
        let out = render_report(&[], NOW_SEC, false);
        let mut lines = out.lines();
        assert_eq!(lines.next(), Some(HEADER_LINE));
        let sep = lines.next().expect("separator row");
        assert!(
            sep.starts_with("| ---"),
            "second line is the md separator; got: {sep}"
        );
        assert!(
            out.contains("(0 hits)"),
            "zero-hit runs still print (0 hits); got: {out}"
        );
    }

    #[test]
    fn long_content_cell_is_clipped_to_at_most_eighty_chars() {
        let long = "a".repeat(90);
        let clipped = truncate_content(&long, CONTENT_WIDTH);
        assert!(
            clipped.chars().count() <= 80,
            "content cell must be at most 80 characters; got {} chars",
            clipped.chars().count()
        );

        let results = [result(
            &long,
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FAA",
            NOW_SEC - DAY,
            8.0,
            9.0,
            3,
            2.0,
        )];
        let out = render_report(&results, NOW_SEC, false);
        assert!(
            out.contains('…'),
            "clipped content must show an ellipsis; got: {out}"
        );
        assert!(
            !out.contains(&long),
            "the full 90-char content must not appear verbatim"
        );
    }

    #[test]
    fn total_column_equals_the_formatted_score() {
        let results = [result(
            "axum error handling",
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FAB",
            NOW_SEC - DAY,
            8.0,
            9.0,
            3,
            1.7,
        )];
        let out = render_report(&results, NOW_SEC, false);
        let row = out.lines().nth(2).expect("one data row");
        let expected_total = format!("{:.4}", results[0].score);
        assert!(
            row.contains(&expected_total),
            "row must show total == formatted score ({expected_total}); got: {row}"
        );
    }

    #[test]
    fn json_output_is_one_compact_array_with_trailing_newline() {
        let results = [result(
            "axum error handling",
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FAC",
            NOW_SEC - DAY,
            8.0,
            9.0,
            3,
            1.7,
        )];
        let out = render_json(&results, NOW_SEC, false);
        assert!(out.ends_with('\n'), "json output ends with a newline");
        assert_eq!(out.matches('\n').count(), 1, "exactly one line");

        let parsed: serde_json::Value = serde_json::from_str(out.trim_end()).unwrap();
        let arr = parsed.as_array().expect("top-level JSON array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["layer"], "episodic");
        // JSON is decimal text, not a bit-for-bit float carrier — compare with an
        // epsilon, same convention as recall.rs's explain_factors tests.
        assert!(
            (arr[0]["total"].as_f64().unwrap() - results[0].score).abs() < 1e-9,
            "total round-trips through JSON: {} vs {}",
            arr[0]["total"],
            results[0].score
        );
        assert!(
            arr[0].get("age_days").is_none(),
            "explain fields absent without --explain"
        );
    }

    #[test]
    fn json_explain_adds_the_explain_factors_fields() {
        let results = [result(
            "axum error handling",
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FAD",
            NOW_SEC - 3 * DAY,
            8.0,
            9.0,
            3,
            1.7,
        )];
        let out = render_json(&results, NOW_SEC, true);
        let parsed: serde_json::Value = serde_json::from_str(out.trim_end()).unwrap();
        let hit = &parsed.as_array().unwrap()[0];
        let f = explain_factors(&results[0], NOW_SEC);
        // JSON is decimal text, not a bit-for-bit float carrier — compare with an
        // epsilon, same convention as recall.rs's explain_factors tests.
        let close = |v: &serde_json::Value, want: f64| (v.as_f64().unwrap() - want).abs() < 1e-9;
        assert!(close(&hit["age_days"], f.age_days));
        assert!(close(&hit["decay"], f.decay));
        assert!(close(&hit["pain_factor"], f.pain_factor));
        assert!(close(&hit["importance_factor"], f.importance_factor));
        assert!(close(&hit["recurrence_factor"], f.recurrence_factor));
        assert!(close(&hit["salience_product"], f.salience_product));
        assert!(close(&hit["pain"], results[0].pain));
        assert!(close(&hit["importance"], results[0].importance));
        assert_eq!(hit["recurrence"], results[0].recurrence);
    }

    #[test]
    fn explain_section_renders_after_the_table_naming_each_hit() {
        let results = [result(
            "axum",
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FAE",
            NOW_SEC - DAY,
            8.0,
            9.0,
            3,
            1.5,
        )];
        let out = render_report(&results, NOW_SEC, true);
        assert!(
            out.starts_with(HEADER_LINE),
            "explain output still leads with the table"
        );
        assert!(out.contains("evt_01ARZ3NDEKTSV4RRFFQ69G5FAE"));
        assert!(out.contains("exp(-age_days/14)"));
        assert!(out.contains("score = bm25"));
    }

    #[test]
    fn no_explain_section_when_flag_off() {
        let results = [result(
            "axum",
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FAF",
            NOW_SEC - DAY,
            8.0,
            9.0,
            3,
            1.5,
        )];
        let out = render_report(&results, NOW_SEC, false);
        assert!(!out.contains("exp(-age_days/14)"));
    }
}
