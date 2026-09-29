//! `dreamd salience-drift` — salience distribution and 7-day drift (BZR-195).
//!
//! Same report as `GET /api/v1/observability/salience`: both call
//! [`dreamd_core::salience::report::build_report`], which scores every episodic
//! doc with `salience()` alone (no BM25), skips `lsn_` lesson docs, and writes
//! today's snapshot under `.agent/.dreamd/observability/`.
//!
//! Opens the index read-only with [`tantivy::Index::open_in_dir`], like
//! `dreamd score` — never the writer-spawning `TantivyIndexHandle` open
//! constructor.

use std::io::Write;
use std::path::Path;

use dreamd_core::salience::report::{build_report, ReportError, SalienceReport};
use dreamd_core::{AgentRoot, LayoutError};
use tantivy::Index;

/// Directory name of the per-project Tantivy index under `.agent/.dreamd/`.
///
/// Mirrors `dreamd_core::server::tantivy_handle::INDEX_DIR_NAME`, which is
/// `pub(crate)` and so not importable here.
const INDEX_DIR_NAME: &str = "index";

#[derive(Debug)]
pub enum SalienceError {
    /// No `.agent/` store found walking up from `cwd`.
    NotFound,
    /// The index directory is missing / empty / failed to open.
    IndexUnavailable(String),
    /// Scan or snapshot failure from `build_report`.
    Report(ReportError),
    /// Failure writing to the `out`/`err` sinks.
    Io(std::io::Error),
}

impl From<std::io::Error> for SalienceError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl std::fmt::Display for SalienceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "no .agent/ directory found"),
            Self::IndexUnavailable(hint) => write!(f, "{hint}"),
            Self::Report(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SalienceError {}

/// Default (non-`--json`) rendering: one summary line, then one line per
/// cluster in `top_clusters` order.
#[must_use]
pub fn render_text(report: &SalienceReport) -> String {
    let drift = match report.drift {
        Some(d) => format!("{d:.6}"),
        None => "none".to_string(),
    };
    let mut s = format!(
        "{} total={} mean={:.6} drift={drift}\n",
        report.date, report.total, report.mean
    );
    for c in &report.top_clusters {
        s.push_str(&format!(
            "{} count={} mean={:.6}\n",
            c.skill_action, c.count, c.mean
        ));
    }
    s
}

/// `dreamd salience-drift` entry point.
pub fn run(
    cwd: &Path,
    json: bool,
    now_sec: i64,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), SalienceError> {
    let root = match AgentRoot::discover(cwd) {
        Ok(r) => r,
        Err(LayoutError::NotFound) => {
            writeln!(
                err,
                "dreamd: error — no .agent/ directory found. Run `dreamd init` first."
            )?;
            return Err(SalienceError::NotFound);
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
        return Err(SalienceError::IndexUnavailable(hint));
    }

    // Read-only open; a writer here would race a live daemon on the same index.
    let index = match Index::open_in_dir(&index_dir) {
        Ok(i) => i,
        Err(e) => {
            let hint = format!(
                "could not open index at {} ({e}) — the index may be empty or not yet \
                 built; append some learnings (or run the daemon once), then retry.",
                index_dir.display()
            );
            writeln!(err, "dreamd: error — {hint}")?;
            return Err(SalienceError::IndexUnavailable(hint));
        }
    };
    let reader = index
        .reader()
        .map_err(|e| SalienceError::Report(ReportError::Search(e)))?;

    let report = build_report(&reader, now_sec, &root.dreamd_dir().join("observability"))
        .map_err(SalienceError::Report)?;

    if json {
        let body = serde_json::to_string(&report)
            .map_err(|e| SalienceError::Report(ReportError::Json(e)))?;
        writeln!(out, "{body}")?;
    } else {
        write!(out, "{}", render_text(&report))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dreamd_core::salience::report::ClusterSalience;

    fn report(drift: Option<f64>) -> SalienceReport {
        SalienceReport {
            schema_version: "observability/1.0".to_string(),
            date: "2026-09-29".to_string(),
            total: 3,
            mean: 0.25,
            histogram: [0, 0, 3, 0, 0, 0, 0, 0, 0, 0],
            top_clusters: vec![
                ClusterSalience {
                    skill_action: "rust::tokio".to_string(),
                    count: 2,
                    mean: 0.3,
                },
                ClusterSalience {
                    skill_action: "rust::serde".to_string(),
                    count: 1,
                    mean: 0.15,
                },
            ],
            drift,
            drift_against: "2026-09-22".to_string(),
        }
    }

    #[test]
    fn render_text_lines() {
        assert_eq!(
            render_text(&report(Some(-0.0125))),
            "2026-09-29 total=3 mean=0.250000 drift=-0.012500\n\
             rust::tokio count=2 mean=0.300000\n\
             rust::serde count=1 mean=0.150000\n"
        );
        assert!(
            render_text(&report(None)).starts_with("2026-09-29 total=3 mean=0.250000 drift=none\n")
        );
    }

    #[test]
    fn missing_store_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let r = run(dir.path(), false, 0, &mut out, &mut err);
        assert!(matches!(r, Err(SalienceError::NotFound)));
        assert!(out.is_empty());
    }
}
