//! `dreamd score` — top-N by query-time salience with no lexical filter (WEG-52 / DR-704).
//!
//! One-shot, read-only. Discovers the project `.agent/` store, opens the
//! per-project Tantivy index **read-only** with [`tantivy::Index::open_in_dir`]
//! (never the writer-spawning `TantivyIndexHandle` open constructor), runs
//! [`dreamd_core::score_by_salience`] (`AllQuery` + `SalienceCollector`), and
//! prints via [`crate::commands::recall::render_report`] — same nine locked
//! columns as `dreamd recall`. `--explain` appends the same per-hit DR-204
//! blocks (default **off**).
//!
//! Under `AllQuery`, Tantivy scores every doc **1.0**, so printed rows show
//! `bm25 ≈ 1` and `score ≈ salience`. That is expected collector behavior, not
//! a bug and not a second scoring path.
//!
//! Modeled on [`crate::commands::recall`]: typed errors, [`AgentRoot::discover`],
//! injectable writers. Default `-n` is **100** (recall keeps its own `-k`).

use std::io::Write;
use std::path::Path;

use dreamd_core::{AgentRoot, LayoutError};
use tantivy::Index;

use crate::commands::recall::render_report;

/// Directory name of the per-project Tantivy index under `.agent/.dreamd/`.
///
/// Mirrors `dreamd_core::server::tantivy_handle::INDEX_DIR_NAME`, which is
/// `pub(crate)` and so not importable here.
const INDEX_DIR_NAME: &str = "index";

#[derive(Debug)]
pub enum ScoreError {
    /// No `.agent/` store found walking up from `cwd`.
    NotFound,
    /// The index directory is missing / empty / failed to open.
    IndexUnavailable(String),
    /// Search failure from the score_by_salience collector.
    Search(tantivy::TantivyError),
    /// Failure writing to the `out`/`err` sinks.
    Io(std::io::Error),
}

impl From<std::io::Error> for ScoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl std::fmt::Display for ScoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "no .agent/ directory found"),
            Self::IndexUnavailable(hint) => write!(f, "{hint}"),
            Self::Search(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ScoreError {}

/// `dreamd score` entry point.
///
/// Discovers the store, opens the index read-only, ranks by salience with no
/// lexical filter, and writes the rendered report to `out`. `now_sec` is the
/// query instant used for age/salience decay.
pub fn run(
    cwd: &Path,
    n: usize,
    explain: bool,
    now_sec: i64,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), ScoreError> {
    let root = match AgentRoot::discover(cwd) {
        Ok(r) => r,
        Err(LayoutError::NotFound) => {
            writeln!(
                err,
                "dreamd: error — no .agent/ directory found. Run `dreamd init` first."
            )?;
            return Err(ScoreError::NotFound);
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
        return Err(ScoreError::IndexUnavailable(hint));
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
            return Err(ScoreError::IndexUnavailable(hint));
        }
    };
    let reader = index.reader().map_err(ScoreError::Search)?;

    let (_schema, fields) = dreamd_core::index::build_schema();
    let results =
        dreamd_core::score_by_salience(&reader, &fields, n, now_sec).map_err(ScoreError::Search)?;

    write!(out, "{}", render_report(&results, now_sec, explain))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_error_display_covers_variants() {
        assert_eq!(
            ScoreError::NotFound.to_string(),
            "no .agent/ directory found"
        );
        assert_eq!(
            ScoreError::IndexUnavailable("build the index".to_string()).to_string(),
            "build the index"
        );
        let io = ScoreError::from(std::io::Error::other("disk full"));
        assert_eq!(io.to_string(), "disk full");
    }
}
