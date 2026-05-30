//! WEG-14 / DR-112 — layered runtime configuration.
//!
//! Precedence, low to high: hardcoded `Default` → user TOML
//! (`~/.config/dreamd/config.toml`, XDG-style) → project TOML
//! (`<project>/.agent/.dreamd/config.toml`). figment's `merge` is later-wins,
//! so files are merged in that order.
//!
//! Missing files contribute nothing (silent fallthrough). Malformed TOML
//! produces a typed [`ConfigError`]; this module never panics on the load
//! path (no `unwrap`, no `expect`).
//!
//! Provider / model / cost_cap_usd are present on the struct but **inert at
//! v0.1** — they ship for v0.1.1 LLM mode and are not read elsewhere yet.
//! `DREAMD_LOG` env-var handling is owned by DR-714 (Sprint 4) and
//! deliberately not implemented here.

use std::path::{Path, PathBuf};

use figment::{
    providers::{Format, Serialized, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Layered daemon configuration. `#[serde(default)]` makes every field
/// optional in source TOML — a missing key falls through to [`Default`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// DR-111 — redact secrets/PII on `POST /api/v1/learn`. Default `true`.
    pub redaction: bool,
    /// Log level filter for the daemon. `trace | debug | info | warn | error`.
    pub log_level: String,
    /// DR-315 — dream-cycle scheduling mode. v0.1 is manual-only.
    pub dream_cycle_mode: DreamCycleMode,
    /// LLM provider id. Inert at v0.1; reserved for v0.1.1.
    pub provider: String,
    /// LLM model id. Inert at v0.1; reserved for v0.1.1.
    pub model: String,
    /// DR-307 — per-cycle USD spend cap. Inert at v0.1; reserved for v0.1.1.
    pub cost_cap_usd: f64,
}

/// Dream-cycle scheduling mode (DR-315). Flat key `dream_cycle_mode` with
/// values `"manual" | "auto"`; v0.1 hard-locks to `manual`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DreamCycleMode {
    #[default]
    Manual,
    Auto,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            redaction: true,
            log_level: "info".to_string(),
            dream_cycle_mode: DreamCycleMode::Manual,
            provider: String::new(),
            model: "claude-haiku-4-5".to_string(),
            cost_cap_usd: 0.10,
        }
    }
}

/// Failure modes for [`load_config`] / [`load_config_from`].
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Malformed TOML or type-mismatch in a present file. Boxed to keep the
    /// enum variant within the 128-byte threshold (`figment::Error` is 208 B).
    #[error("config parse error: {0}")]
    Parse(Box<figment::Error>),
}

impl From<figment::Error> for ConfigError {
    fn from(e: figment::Error) -> Self {
        ConfigError::Parse(Box::new(e))
    }
}

/// Commented-out template written by `dreamd init` (D1 of WEG-14.v2) to
/// `<project>/.agent/.dreamd/config.toml`. Every key disabled by default;
/// origin DR called out inline so editors don't have to grep.
pub const CONFIG_TEMPLATE: &str = "\
# dreamd config — all keys optional. Precedence: this file > ~/.config/dreamd/config.toml > built-in defaults.

# redaction = true              # redact secrets/PII on POST /api/v1/learn (DR-111)
# log_level = \"info\"            # trace | debug | info | warn | error
# dream_cycle_mode = \"manual\"   # \"manual\" | \"auto\" — v0.1 is manual-only (DR-315)

# --- LLM keys: present but inert until v0.1.1 ---
# provider = \"\"                 # LLM provider id
# model = \"claude-haiku-4-5\"    # model id
# cost_cap_usd = 0.10           # hard per-cycle spend cap (DR-307)
";

/// User-config path on this platform (Linux/macOS XDG, Windows Roaming).
/// `None` if the platform has no resolvable config dir.
fn user_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("dreamd").join("config.toml"))
}

/// Load layered config for a project. User path is resolved from
/// [`dirs::config_dir`]; project path is `<project>/.agent/.dreamd/config.toml`.
pub fn load_config(project_root: &Path) -> Result<Config, ConfigError> {
    let user = user_config_path();
    let project = project_root
        .join(".agent")
        .join(".dreamd")
        .join("config.toml");
    load_config_from(user.as_deref(), &project)
}

/// Loader with explicit paths. Production callers go through [`load_config`];
/// tests use this to avoid the global `dirs::config_dir` lookup, which is
/// racy under cargo's multi-threaded test runner.
pub fn load_config_from(
    user_path: Option<&Path>,
    project_path: &Path,
) -> Result<Config, ConfigError> {
    let mut fig = Figment::new().merge(Serialized::defaults(Config::default()));
    // path.exists() guards keep behavior independent of the figment minor
    // version's "missing file" semantics — missing-is-empty is the contract
    // WEG-14.v2 §3.2 calls out, not a figment internal we should depend on.
    if let Some(p) = user_path {
        if p.exists() {
            fig = fig.merge(Toml::file(p));
        }
    }
    if project_path.exists() {
        fig = fig.merge(Toml::file(project_path));
    }
    fig.extract().map_err(ConfigError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn precedence_project_overrides_user_overrides_default() {
        let tmp = tempdir().unwrap();
        let user_path = tmp.path().join("user.toml");
        let project_path = tmp.path().join("project.toml");

        // User sets log_level + redaction. Project sets log_level only.
        // Expectation: log_level comes from project (later merge wins);
        // redaction comes from user (project silent on it);
        // everything else falls through to Default.
        fs::write(&user_path, "log_level = \"warn\"\nredaction = false\n").unwrap();
        fs::write(&project_path, "log_level = \"debug\"\n").unwrap();

        let cfg = load_config_from(Some(&user_path), &project_path).expect("load ok");
        assert_eq!(cfg.log_level, "debug", "project must beat user");
        assert!(
            !cfg.redaction,
            "user must beat default when project is silent"
        );
        assert_eq!(
            cfg.dream_cycle_mode,
            DreamCycleMode::Manual,
            "untouched keys fall through to Default"
        );
        assert_eq!(cfg.cost_cap_usd, 0.10);
    }

    #[test]
    fn fallthrough_to_default_when_both_files_missing() {
        let tmp = tempdir().unwrap();
        let user_path = tmp.path().join("nope-user.toml");
        let project_path = tmp.path().join("nope-project.toml");
        // Files deliberately not created — silent fallthrough must yield Default.
        let cfg = load_config_from(Some(&user_path), &project_path).expect("load ok");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn commented_template_parses_after_uncommenting_one_key() {
        // §3.4 template — confirm a user can uncomment a key without
        // re-typing it. Flip redaction off to prove the value actually
        // landed (Default is `true`).
        let mut customized = CONFIG_TEMPLATE.replace("# redaction = true", "redaction = false");
        // Sanity-check that the replace hit exactly the line we expected.
        assert!(
            customized.contains("\nredaction = false"),
            "template line for redaction did not uncomment cleanly:\n{customized}"
        );
        // Pure template alone (no edit) must also parse — proves the
        // commented form is valid TOML on its own.
        customized.push('\n');

        let tmp = tempdir().unwrap();
        let project_path = tmp.path().join("config.toml");

        // Round 1: fully commented template.
        fs::write(&project_path, CONFIG_TEMPLATE).unwrap();
        let cfg = load_config_from(None, &project_path).expect("template parses");
        assert_eq!(
            cfg,
            Config::default(),
            "fully commented template = defaults"
        );

        // Round 2: one key uncommented and customized.
        fs::write(&project_path, &customized).unwrap();
        let cfg = load_config_from(None, &project_path).expect("uncommented parses");
        assert!(!cfg.redaction, "uncommented key takes effect");
    }

    #[test]
    fn malformed_toml_returns_err_does_not_panic() {
        let tmp = tempdir().unwrap();
        let project_path = tmp.path().join("config.toml");
        // Deliberate garbage — unterminated array, malformed key.
        fs::write(&project_path, "this is = not [valid \"toml\n").unwrap();

        let result = load_config_from(None, &project_path);
        assert!(
            matches!(result, Err(ConfigError::Parse(_))),
            "expected Err(ConfigError::Parse), got {result:?}"
        );
    }
}
