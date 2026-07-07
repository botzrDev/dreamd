//! `dreamd doctor` — structured health-check output (WEG-66 / DR-315).
//!
//! Prints one line per check to stdout. Exit 0 if all checks pass; exit 1 if
//! any check emits a WARNING or ERROR. Today: dream-cycle mode and on-disk
//! index freshness (when a store is present).

use std::io::{self, Write};

use dreamd_core::autobiography::AutobiographySkip;
use dreamd_core::config::{Config, DreamCycleMode};
use dreamd_core::layout::AgentRoot;

/// Run `dreamd doctor` and write output to `out`.
///
/// Returns `Ok(true)` if all checks passed (exit 0 caller), `Ok(false)` if any
/// WARNING or ERROR was emitted (exit 1 caller).
pub fn run(
    config: &Config,
    agent_root: &AgentRoot,
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

    // v0.1 index-vs-JSONL contract — on-disk watermark vs episodic tail.
    #[cfg(unix)]
    {
        match dreamd_core::server::assess_index_freshness(agent_root) {
            Ok(freshness) if !freshness.stale => {
                writeln!(out, "index_freshness: ok")?;
            }
            Ok(freshness) => {
                writeln!(
                    out,
                    "index_freshness: stale  [WARNING: {} unindexed event(s); \
                     jsonl_tail={}; watermark={}; recall may miss recent events \
                     until indexer commit or daemon restart replay]",
                    freshness.unindexed_count,
                    freshness.jsonl_tail_id.as_deref().unwrap_or("(none)"),
                    freshness.last_indexed_id.as_deref().unwrap_or("(none)"),
                )?;
                all_ok = false;
            }
            Err(e) => {
                writeln!(
                    out,
                    "index_freshness: error  [WARNING: could not assess: {e}]"
                )?;
                all_ok = false;
            }
        }
    }

    // WEG-132 / DR-812 — episodic log malformed-line check (DR-107 doctor).
    let episodic_path = agent_root.episodic_jsonl();
    if episodic_path.exists() {
        match dreamd_core::episodic::assess_log_health(&episodic_path) {
            Ok(health) if health.malformed_line_count == 0 && health.torn_tail_bytes == 0 => {
                writeln!(out, "episodic_log: ok")?;
            }
            Ok(health) => {
                if health.malformed_line_count > 0 {
                    writeln!(
                        out,
                        "episodic_log: {} malformed line(s)  [WARNING: hand-edited or \
                         corrupt \\n-terminated lines in AGENT_LEARNINGS.jsonl; skipped \
                         at read time]",
                        health.malformed_line_count,
                    )?;
                }
                if health.torn_tail_bytes > 0 {
                    writeln!(
                        out,
                        "episodic_log: torn tail ({} byte(s))  [WARNING: incomplete final \
                         line with no trailing newline; truncated on daemon open]",
                        health.torn_tail_bytes,
                    )?;
                }
                all_ok = false;
            }
            Err(e) => {
                writeln!(out, "episodic_log: error  [WARNING: could not assess: {e}]")?;
                all_ok = false;
            }
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
    use dreamd_core::layout::AgentRoot;
    use std::fs;

    fn setup_agent_root(label: &str) -> (AgentRoot, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path().join(label);
        fs::create_dir_all(&project).unwrap();
        let root = AgentRoot::new(&project);
        fs::create_dir_all(root.agent_dir()).unwrap();
        fs::create_dir_all(root.dreamd_dir()).unwrap();
        fs::create_dir_all(root.episodic_dir()).unwrap();
        (root, dir)
    }

    #[test]
    fn doctor_output_contains_dream_cycle_mode_manual() {
        let cfg = Config::default(); // default is Manual
        let (root, _dir) = setup_agent_root("manual");
        let mut buf = Vec::new();
        let ok = run(&cfg, &root, None, &mut buf).expect("run ok");
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
        let (root, _dir) = setup_agent_root("auto");
        let mut buf = Vec::new();
        let ok = run(&cfg, &root, None, &mut buf).expect("run ok");
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
        let (root, _dir) = setup_agent_root("skip-some");
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
        let ok = run(&cfg, &root, Some(&skip), &mut buf).expect("run ok");
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
        let (root, _dir) = setup_agent_root("skip-none");
        let mut buf = Vec::new();
        run(&cfg, &root, None, &mut buf).expect("run ok");
        let output = String::from_utf8(buf).expect("utf8");
        assert!(
            !output.contains("last autobiography skip"),
            "no skip line when skip is None; got: {output:?}"
        );
    }

    #[test]
    fn doctor_episodic_log_ok_when_clean() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("episodic-clean");
        let jsonl = root.episodic_jsonl();
        fs::write(&jsonl, b"").unwrap();
        let mut buf = Vec::new();
        let ok = run(&cfg, &root, None, &mut buf).expect("run ok");
        let output = String::from_utf8(buf).expect("utf8");
        assert!(
            output.contains("episodic_log: ok"),
            "clean episodic log must report ok; got: {output:?}"
        );
        assert!(ok, "clean episodic log must return all_ok=true");
    }

    #[test]
    fn doctor_episodic_log_warns_on_malformed_lines() {
        let cfg = Config::default();
        let (root, _dir) = setup_agent_root("episodic-bad");
        let jsonl = root.episodic_jsonl();
        fs::write(&jsonl, b"{not valid json}\n").unwrap();
        let mut buf = Vec::new();
        let ok = run(&cfg, &root, None, &mut buf).expect("run ok");
        let output = String::from_utf8(buf).expect("utf8");
        assert!(
            output.contains("episodic_log:") && output.contains("WARNING"),
            "malformed lines must emit WARNING; got: {output:?}"
        );
        assert!(
            output.contains("1 malformed line"),
            "must report malformed line count; got: {output:?}"
        );
        assert!(!ok, "malformed episodic log must return all_ok=false");
    }
}
