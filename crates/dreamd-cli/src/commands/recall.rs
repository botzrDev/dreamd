//! `dreamd recall <query>` — BM25 × salience recall as a markdown table (WEG-51 / DR-703).
//!
//! One-shot, read-only. Discovers the project `.agent/` store, opens the
//! per-project Tantivy index **read-only** with [`tantivy::Index::open_in_dir`]
//! (never the writer-spawning `TantivyIndexHandle` open constructor, which
//! starts an indexer task that would race a live daemon), runs the shared
//! [`dreamd_core::recall`] collector, and prints the top-k hits with a
//! score-breakdown column layout. `--explain` appends the per-hit DR-204
//! salience factor arithmetic.
//!
//! Modeled on [`crate::commands::archive`]: a typed error enum, [`AgentRoot::discover`]
//! for root resolution, and injectable `out`/`err` writers for testability. The
//! CLI default `-k` is **10** (a screencast-friendly demo default); the HTTP/MCP
//! wire default stays at `DEFAULT_RECALL_K = 5` — this command does not touch it.
//!
//! **Daemon lag caveat (correctness, not scope):** appends still buffered inside
//! a live daemon may not yet be committed to the on-disk index, so a
//! back-to-back append→recall can miss the newest event (same class of caveat as
//! `dreamd doctor`'s watermark warning). This command reads the committed index
//! directly; it does not proxy live recall over the daemon UDS.

use std::io::Write;
use std::path::Path;

use dreamd_core::{AgentRoot, LayoutError, RecallResult};
use tantivy::Index;

/// Directory name of the per-project Tantivy index under `.agent/.dreamd/`.
///
/// Mirrors `dreamd_core::server::tantivy_handle::INDEX_DIR_NAME`, which is
/// `pub(crate)` and so not importable here. The canonical path is
/// `AgentRoot::dreamd_dir().join(INDEX_DIR_NAME)` — identical to the path the
/// daemon's `TantivyIndexHandle` opens.
const INDEX_DIR_NAME: &str = "index";

/// Max width of the rendered `content` cell, in characters. Longer text is
/// truncated with a trailing `…` so the table stays screencast-readable.
const CONTENT_WIDTH: usize = 60;

#[derive(Debug)]
pub enum RecallError {
    /// No `.agent/` store found walking up from `cwd`.
    NotFound,
    /// The index directory is missing / empty / failed to open. Carries a
    /// user-facing hint (append learnings or run the daemon once).
    IndexUnavailable(String),
    /// Query parse or search failure from the recall collector.
    Search(tantivy::TantivyError),
    /// Failure writing to the `out`/`err` sinks.
    Io(std::io::Error),
}

impl From<std::io::Error> for RecallError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl std::fmt::Display for RecallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "no .agent/ directory found"),
            Self::IndexUnavailable(hint) => write!(f, "{hint}"),
            Self::Search(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RecallError {}

/// The six DR-204 salience factors for one hit, plus the two products the
/// `--explain` block prints. `salience_product` must equal `RecallResult::salience`
/// within float noise; `score` must equal `RecallResult::score`.
///
/// The arithmetic mirrors [`dreamd_core::salience::salience`] verbatim (the
/// `/ 10.0` shape is intentionally not factored out) so `--explain` reads the
/// same as ARCHITECTURE.md decision #2 / PRD FR-4.2.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExplainFactors {
    pub age_days: f64,
    /// `exp(-age_days / 14)`
    pub decay: f64,
    /// `pain / 10`
    pub pain_factor: f64,
    /// `importance / 10`
    pub importance_factor: f64,
    /// `1 + ln(1 + recurrence)`
    pub recurrence_factor: f64,
    /// Product of the four factors above (= `salience`).
    pub salience_product: f64,
    /// `bm25 × salience` (= `score`).
    pub score: f64,
}

/// Header row of the markdown table. Column order is locked by the AC (WEG-51):
/// `rank, id, score, bm25, salience, age_days, pain, recurrence, content`.
/// `importance` is deliberately absent — it appears only under `--explain`.
const HEADER_LINE: &str =
    "| rank | id | score | bm25 | salience | age_days | pain | recurrence | content |";
/// GitHub-flavored-markdown separator row (nine columns).
const SEPARATOR_LINE: &str = "| --- | --- | --- | --- | --- | --- | --- | --- | --- |";

/// Derive `age_days` for a hit: `(now_sec - timestamp_sec) / 86_400`.
///
/// Kept as one helper so the table cell and the `--explain` block agree.
fn age_days(now_sec: i64, timestamp_sec: u64) -> f64 {
    (now_sec as f64 - timestamp_sec as f64) / 86_400.0
}

/// Derive the DR-204 salience factor breakdown for one recall hit.
///
/// The arithmetic mirrors [`dreamd_core::salience::salience`] verbatim (same
/// factor order, `/ 10.0` not factored out) so the printed product equals the
/// stored `RecallResult::salience` bit-for-bit.
pub fn explain_factors(r: &RecallResult, now_sec: i64) -> ExplainFactors {
    let age_days = age_days(now_sec, r.timestamp_sec);
    let decay = (-age_days / 14.0).exp();
    let pain_factor = r.pain / 10.0;
    let importance_factor = r.importance / 10.0;
    let recurrence_factor = 1.0 + (1.0 + r.recurrence as f64).ln();
    let salience_product = decay * pain_factor * importance_factor * recurrence_factor;
    let score = r.bm25 * salience_product;
    ExplainFactors {
        age_days,
        decay,
        pain_factor,
        importance_factor,
        recurrence_factor,
        salience_product,
        score,
    }
}

/// Collapse whitespace and truncate `s` to at most `max` characters, appending
/// `…` when clipped. Embedded `|` is escaped afterward so it cannot split the
/// markdown row (the escape is not counted against `max`).
pub fn truncate_content(s: &str, max: usize) -> String {
    // Collapse any run of whitespace (incl. newlines) to a single space so the
    // cell stays on one row.
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let clipped = if collapsed.chars().count() <= max {
        collapsed
    } else {
        let mut out: String = collapsed.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    };
    // Escape after truncation so a `\|` pair is never split, leaving no dangling
    // backslash.
    clipped.replace('|', "\\|")
}

/// Render the markdown table (and, when `explain`, the per-hit factor blocks)
/// for `results`. Pure and deterministic given `now_sec`; unit-tested directly.
///
/// The header + separator always print (even for zero hits) so the columns stay
/// visible; the caller still exits 0 on an empty result set.
pub fn render_report(results: &[RecallResult], now_sec: i64, explain: bool) -> String {
    let mut out = String::new();
    out.push_str(HEADER_LINE);
    out.push('\n');
    out.push_str(SEPARATOR_LINE);
    out.push('\n');

    for (i, r) in results.iter().enumerate() {
        let rank = i + 1;
        let age = age_days(now_sec, r.timestamp_sec);
        let content = truncate_content(&r.content, CONTENT_WIDTH);
        out.push_str(&format!(
            "| {rank} | {id} | {score:.4} | {bm25:.4} | {sal:.4} | {age:.1} | {pain:.1} | {rec} | {content} |\n",
            id = r.event_id,
            score = r.score,
            bm25 = r.bm25,
            sal = r.salience,
            pain = r.pain,
            rec = r.recurrence,
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

/// `dreamd recall <query>` entry point.
///
/// Discovers the store, opens the index read-only, runs the salience-scored
/// recall, and writes the rendered report to `out`. `now_sec` is the query
/// instant (wall clock at the call site) used to derive `age_days` and the
/// salience decay. Errors are written to `err` as `dreamd: error — …` and
/// returned typed for exit-code mapping in `cli::run`.
pub fn run(
    cwd: &Path,
    query: &str,
    k: usize,
    explain: bool,
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

    write!(out, "{}", render_report(&results, now_sec, explain))?;
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
    /// with the DR-204 formula, so `--explain` products can be checked against
    /// the stored fields.
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

    const HEADER: &str =
        "| rank | id | score | bm25 | salience | age_days | pain | recurrence | content |";

    #[test]
    fn table_header_has_nine_ac_columns_in_order() {
        let out = render_report(&[], NOW_SEC, false);
        let header = out.lines().next().expect("at least a header line");
        assert_eq!(
            header, HEADER,
            "column order/labels must match the AC exactly"
        );
        // `importance` is an --explain-only factor, never a table column.
        assert!(
            !header.contains("importance"),
            "importance must not be a table column; got: {header}"
        );
    }

    #[test]
    fn empty_results_render_header_and_separator_no_panic() {
        let out = render_report(&[], NOW_SEC, false);
        let mut lines = out.lines();
        assert_eq!(lines.next(), Some(HEADER));
        let sep = lines.next().expect("separator row");
        assert!(
            sep.starts_with("| ---"),
            "second line is the md separator; got: {sep}"
        );
    }

    #[test]
    fn rows_are_rank_one_based_with_event_id_in_id_column() {
        let results = [
            result(
                "first hit",
                "evt_01ARZ3NDEKTSV4RRFFQ69G5FAA",
                NOW_SEC - DAY,
                8.0,
                9.0,
                3,
                2.0,
            ),
            result(
                "second hit",
                "evt_01ARZ3NDEKTSV4RRFFQ69G5FAB",
                NOW_SEC - 7 * DAY,
                6.0,
                7.0,
                3,
                1.0,
            ),
        ];
        let out = render_report(&results, NOW_SEC, false);
        let rows: Vec<&str> = out.lines().skip(2).collect();
        assert_eq!(rows.len(), 2, "two hits → two data rows; got: {out}");
        assert!(
            rows[0].starts_with("| 1 |"),
            "first row rank 1; got: {}",
            rows[0]
        );
        assert!(
            rows[1].starts_with("| 2 |"),
            "second row rank 2; got: {}",
            rows[1]
        );
        assert!(
            rows[0].contains("evt_01ARZ3NDEKTSV4RRFFQ69G5FAA"),
            "id column carries event_id; got: {}",
            rows[0]
        );
    }

    #[test]
    fn long_content_is_truncated_with_ellipsis() {
        let long = "a".repeat(200);
        let results = [result(
            &long,
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FAC",
            NOW_SEC - DAY,
            5.0,
            5.0,
            3,
            1.0,
        )];
        let out = render_report(&results, NOW_SEC, false);
        assert!(
            out.contains('…'),
            "clipped content must show an ellipsis; got: {out}"
        );
        assert!(
            !out.contains(&long),
            "the full 200-char content must not appear verbatim"
        );
    }

    #[test]
    fn truncate_content_escapes_pipe_and_collapses_whitespace() {
        let got = truncate_content("has | pipe\nand newline", CONTENT_WIDTH);
        assert!(!got.contains('\n'), "newlines collapsed; got: {got}");
        assert!(
            !got.contains(" | "),
            "raw ` | ` would split the row; got: {got}"
        );
    }

    #[test]
    fn explain_factors_product_matches_salience_and_score() {
        let ts = NOW_SEC - 3 * DAY;
        let r = result(
            "axum error handling",
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FAD",
            ts,
            8.0,
            9.0,
            3,
            1.7,
        );
        let f = explain_factors(&r, NOW_SEC);

        // The four factors multiply to the stored salience...
        let product = f.decay * f.pain_factor * f.importance_factor * f.recurrence_factor;
        assert!(
            (product - f.salience_product).abs() < 1e-12,
            "product wiring: {product} vs {}",
            f.salience_product
        );
        // ...which equals the canonical salience() and the stored field.
        let canonical = salience(NOW_SEC, ts, 8.0, 9.0, 3);
        assert!(
            (f.salience_product - canonical).abs() < 1e-12,
            "salience vs salience(): {} vs {canonical}",
            f.salience_product
        );
        assert!(
            (f.salience_product - r.salience).abs() < 1e-9,
            "salience vs result.salience"
        );
        // score = bm25 × salience matches the stored score.
        assert!(
            (f.score - r.score).abs() < 1e-9,
            "score vs result.score: {} vs {}",
            f.score,
            r.score
        );
    }

    #[test]
    fn explain_factors_use_dr204_literal_shape() {
        let ts = NOW_SEC - 14 * DAY; // age_days == 14 → decay == e^-1
        let r = result(
            "x",
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FAE",
            ts,
            10.0,
            10.0,
            0,
            1.0,
        );
        let f = explain_factors(&r, NOW_SEC);
        assert!((f.age_days - 14.0).abs() < 1e-9);
        assert!((f.decay - (-1.0_f64).exp()).abs() < 1e-12);
        assert!((f.pain_factor - 1.0).abs() < 1e-12);
        assert!((f.importance_factor - 1.0).abs() < 1e-12);
        assert!(
            (f.recurrence_factor - 1.0).abs() < 1e-12,
            "1 + ln(1+0) == 1"
        );
    }

    #[test]
    fn explain_section_renders_factor_labels_per_hit() {
        let results = [result(
            "axum",
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FAF",
            NOW_SEC - DAY,
            8.0,
            9.0,
            3,
            1.5,
        )];
        let out = render_report(&results, NOW_SEC, true);
        // The table still comes first...
        assert!(
            out.starts_with(HEADER),
            "explain output still leads with the table"
        );
        // ...then a per-hit factor block naming the id and the DR-204 factors.
        assert!(
            out.contains("evt_01ARZ3NDEKTSV4RRFFQ69G5FAF"),
            "explain names the hit id"
        );
        assert!(
            out.contains("exp(-age_days/14)"),
            "explain shows the decay factor"
        );
        assert!(
            out.contains("importance/10"),
            "explain shows importance/10 (only here)"
        );
        assert!(
            out.contains("(1 + ln(1+recurrence))"),
            "explain shows the recurrence factor"
        );
        assert!(
            out.contains("salience (product)"),
            "explain shows the salience product"
        );
        assert!(
            out.contains("score = bm25"),
            "explain shows score = bm25 × salience"
        );
    }

    #[test]
    fn no_explain_section_when_flag_off() {
        let results = [result(
            "axum",
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FAG",
            NOW_SEC - DAY,
            8.0,
            9.0,
            3,
            1.5,
        )];
        let out = render_report(&results, NOW_SEC, false);
        assert!(
            !out.contains("exp(-age_days/14)"),
            "no explain block without --explain"
        );
        assert!(
            !out.contains("importance"),
            "importance never leaks without --explain"
        );
    }

    #[test]
    fn recall_error_display_covers_variants() {
        assert_eq!(
            RecallError::NotFound.to_string(),
            "no .agent/ directory found"
        );
        assert_eq!(
            RecallError::IndexUnavailable("build the index".to_string()).to_string(),
            "build the index"
        );
        let io = RecallError::from(std::io::Error::other("disk full"));
        assert_eq!(io.to_string(), "disk full");
    }
}
