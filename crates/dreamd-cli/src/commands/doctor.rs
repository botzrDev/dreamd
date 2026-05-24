//! `dreamd doctor` — structured health-check output (WEG-66 / DR-315).
//!
//! Prints one line per check to stdout. Exit 0 if all checks pass; exit 1 if
//! any check emits a WARNING or ERROR. Today only the dream-cycle mode line is
//! emitted; additional checks land per DR-107 (WEG-50).

use std::io::{self, Write};

use dreamd_core::config::{Config, DreamCycleMode};

/// Run `dreamd doctor` and write output to `out`.
///
/// Returns `Ok(true)` if all checks passed (exit 0 caller), `Ok(false)` if any
/// WARNING or ERROR was emitted (exit 1 caller).
pub fn run(config: &Config, out: &mut impl Write) -> io::Result<bool> {
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

    Ok(all_ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_output_contains_dream_cycle_mode_manual() {
        let cfg = Config::default(); // default is Manual
        let mut buf = Vec::new();
        let ok = run(&cfg, &mut buf).expect("run ok");
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
        let mut cfg = Config::default();
        cfg.dream_cycle_mode = DreamCycleMode::Auto;
        let mut buf = Vec::new();
        let ok = run(&cfg, &mut buf).expect("run ok");
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
}
