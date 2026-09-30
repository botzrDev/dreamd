//! `dreamd vectors enable` — download the embedding model (BZR-181).
//!
//! The default binary is built without the `vectors` feature: the command
//! prints one sentence naming that feature, exits 2, and creates nothing.
//! A `--features vectors` build downloads [`dreamd_core::vectors::MODEL_ID`]
//! into `~/.agent/models` and loads it once. Recall is unchanged either way
//! (BM25 × salience); nothing here embeds text or writes an index.
//!
//! No stdout lock is held across the download: loading the model spins up
//! ONNX Runtime, which may log through tracing to stderr.

use std::io::Write;
use std::process::ExitCode;

#[cfg(not(feature = "vectors"))]
const NOT_COMPILED: &str = "dreamd vectors enable: this binary was built without the vectors feature; rebuild with --features vectors";

/// Run `dreamd vectors enable`. `out` / `err` are unlocked sinks.
#[cfg(not(feature = "vectors"))]
pub fn run(_out: &mut impl Write, err: &mut impl Write) -> ExitCode {
    let _ = writeln!(err, "{NOT_COMPILED}");
    ExitCode::from(2)
}

/// Run `dreamd vectors enable`. `out` / `err` are unlocked sinks.
#[cfg(feature = "vectors")]
pub fn run(out: &mut impl Write, err: &mut impl Write) -> ExitCode {
    use dreamd_core::layout::{home_dir, DaemonHome};

    let Some(home) = home_dir() else {
        let _ = writeln!(err, "dreamd vectors enable: HOME is unset");
        return ExitCode::from(2);
    };
    let models = DaemonHome::new(home.join(".agent")).root().join("models");
    if let Err(e) = std::fs::create_dir_all(&models) {
        let _ = writeln!(err, "dreamd: error — {e}");
        return ExitCode::from(1);
    }
    if let Err(e) = dreamd_core::vectors::download_model(&models) {
        let _ = writeln!(err, "dreamd: error — {e}");
        return ExitCode::from(1);
    }
    // fastembed lets a non-empty HF_HOME win over the cache dir we passed.
    let cache = match std::env::var_os("HF_HOME") {
        Some(v) if !v.is_empty() => std::path::PathBuf::from(v),
        _ => models,
    };
    let res = writeln!(out, "vectors=compiled")
        .and_then(|()| writeln!(out, "model={}", dreamd_core::vectors::MODEL_ID))
        .and_then(|()| writeln!(out, "cache={}", cache.display()));
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(err, "dreamd: error — {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "vectors"))]
    #[test]
    fn enable_without_feature_exits_2_and_names_the_feature() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = super::run(&mut out, &mut err);
        assert_eq!(code, std::process::ExitCode::from(2));
        assert!(out.is_empty());
        assert_eq!(
            String::from_utf8(err).unwrap(),
            format!("{}\n", super::NOT_COMPILED)
        );
        assert_eq!(
            super::NOT_COMPILED,
            "dreamd vectors enable: this binary was built without the vectors feature; rebuild with --features vectors"
        );
    }

    /// The feature build must not reach the network from a unit test: this
    /// only pins the model id the command prints.
    #[cfg(feature = "vectors")]
    #[test]
    fn enable_with_feature_names_the_model_without_downloading() {
        assert_eq!(dreamd_core::vectors::MODEL_ID, "BAAI/bge-small-en-v1.5");
    }
}
