//! `dreamd doctor` — structured health-check output (WEG-66 / DR-315).
//!
//! Prints one line per check to stdout. Exit 0 if all checks pass; exit 1 if
//! any check emits a WARNING or ERROR. Today only the dream-cycle mode line is
//! emitted; additional checks land per DR-107 (WEG-50).

use std::io::{self, Write};

use dreamd_core::autobiography::AutobiographySkip;
use dreamd_core::config::{Config, DreamCycleMode};

/// Run `dreamd doctor` and write output to `out`.
///
/// Returns `Ok(true)` if all checks passed (exit 0 caller), `Ok(false)` if any
/// WARNING or ERROR was emitted (exit 1 caller).
pub fn run(
    config: &Config,
    skip: Option<&AutobiographySkip>,
    out: &mut impl Write,
) -> io::Result<bool> {
    let mut all_ok = true;

    // DR-315 — dream-cycle mode line.
    match config.dream_cycle_mode {
        DreamCycleMode::Manual => {
            writeln!(out, "dream_cycle_mode: manual")?;
        }
        DreamCycleMode::Auto => {
            writeln!(
                out,
                "dream_cycle_mode: auto  [WARNING: not supported at v0.1]"
            )?;
            all_ok = false;
        }
    }

    // WEG-63 — last autobiography skip (if any).
    if let Some(s) = skip {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let elapsed = now.saturating_sub(s.at);
        let duration_str = format_elapsed(elapsed);
        let file_count = s.files.len();
        writeln!(
            out,
            "last autobiography skip: {} ago — {} — {} files",
            duration_str, s.reason, file_count,
        )?;
    }

    Ok(all_ok)
}

fn format_elapsed(secs: i64) -> String {
    if secs < 60 {
        format!("{secs} seconds")
    } else if secs < 3600 {
        format!("{} minutes", secs / 60)
    } else if secs < 86400 {
        format!("{} hours", secs / 3600)
    } else {
        format!("{} days", secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_output_contains_dream_cycle_mode_manual() {
        let cfg = Config::default(); // default is Manual
        let mut buf = Vec::new();
        let ok = run(&cfg, None, &mut buf).expect("run ok");
        let output = String::from_utf8(buf).expect("utf8");
        assert!(
            output.contains("dream_cycle_mode:"),
            "doctor output must contain 'dream_cycle_mode:'; got: {output:?}"
        );
        assert!(
            output.contains("manual"),
            "default config must report manual mode; got: {output:?}"
        );
        assert!(ok, "manual mode must return all_ok=true");
    }

    #[test]
    fn doctor_output_auto_mode_warning() {
        let cfg = Config {
            dream_cycle_mode: DreamCycleMode::Auto,
            ..Default::default()
        };
        let mut buf = Vec::new();
        let ok = run(&cfg, None, &mut buf).expect("run ok");
        let output = String::from_utf8(buf).expect("utf8");
        assert!(
            output.contains("dream_cycle_mode:"),
            "doctor output must contain 'dream_cycle_mode:'; got: {output:?}"
        );
        assert!(
            output.contains("WARNING"),
            "auto mode must emit WARNING; got: {output:?}"
        );
        assert!(!ok, "auto mode must return all_ok=false");
    }

    #[test]
    fn doctor_skip_some_renders_line() {
        let cfg = Config::default();
        let skip = AutobiographySkip {
            at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
                - 300, // 5 minutes ago
            reason: "user_dirty_tree".to_string(),
            files: vec![
                ".agent/semantic/LESSONS.md".to_string(),
                ".agent/episodic/AGENT_LEARNINGS.jsonl".to_string(),
            ],
        };
        let mut buf = Vec::new();
        let ok = run(&cfg, Some(&skip), &mut buf).expect("run ok");
        let output = String::from_utf8(buf).expect("utf8");
        assert!(
            output.contains("last autobiography skip:"),
            "skip line must be present; got: {output:?}"
        );
        assert!(
            output.contains("user_dirty_tree"),
            "reason must appear; got: {output:?}"
        );
        assert!(
            output.contains("2 files"),
            "file count must appear; got: {output:?}"
        );
        assert!(ok, "skip line must not change all_ok");
    }

    #[test]
    fn doctor_skip_none_no_skip_line() {
        let cfg = Config::default();
        let mut buf = Vec::new();
        run(&cfg, None, &mut buf).expect("run ok");
        let output = String::from_utf8(buf).expect("utf8");
        assert!(
            !output.contains("last autobiography skip"),
            "no skip line when skip is None; got: {output:?}"
        );
    }
}
