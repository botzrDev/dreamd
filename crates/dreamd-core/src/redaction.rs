//! PII/secret redaction for agent-written content.
//!
//! [`redact`] is the single public entry point. Pass `enabled = false` to
//! bypass all regex work entirely — the early return is zero-allocation.
//!
//! Patterns are compiled once at first use via [`std::sync::LazyLock`] static
//! statics (stable since Rust 1.80). The set is intentionally narrow: we target
//! high-confidence literals (AWS key IDs, bearer tokens, sk-/sk-ant- API keys,
//! and env-var assignments that name common secrets). Heuristic patterns that
//! generate false positives are out of scope for v0.1.

use std::sync::LazyLock;

use regex::Regex;

// ── compiled patterns ──────────────────────────────────────────────────────

/// AWS access key ID — exactly the `AKIA` prefix followed by 16 uppercase
/// alphanumerics.
static RE_AWS_KEY_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"AKIA[0-9A-Z]{16}").expect("RE_AWS_KEY_ID pattern is valid"));

/// HTTP Bearer token — `Bearer ` followed by any run of base64url + dot chars.
static RE_BEARER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Bearer [a-zA-Z0-9._-]+").expect("RE_BEARER pattern is valid"));

/// OpenAI-style secret key — `sk-` followed by at least 10 alnum chars.
/// The 10-char floor avoids false positives on short tokens like `sk-short`.
static RE_OPENAI_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"sk-[a-zA-Z0-9]{10,}").expect("RE_OPENAI_KEY pattern is valid"));

/// Anthropic-style secret key — `sk-ant-` followed by alnum / underscore / dash.
/// Listed before RE_OPENAI_KEY in the apply order so the longer prefix is
/// consumed first and avoids a partial match being left over.
static RE_ANTHROPIC_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"sk-ant-[a-zA-Z0-9_-]+").expect("RE_ANTHROPIC_KEY pattern is valid")
});

/// Common env-var assignment patterns: `API_KEY`, `SECRET`, `TOKEN`, or
/// `PASSWORD` followed by optional whitespace, `=`, and a non-whitespace run.
static RE_ENV_VAR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(API_KEY|SECRET|TOKEN|PASSWORD)\s*=\s*\S+").expect("RE_ENV_VAR pattern is valid")
});

/// AWS secret access key env-var assignment.
static RE_AWS_SECRET_ENV: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"AWS_SECRET_ACCESS_KEY\s*=\s*\S+").expect("RE_AWS_SECRET_ENV pattern is valid")
});

// ── public API ─────────────────────────────────────────────────────────────

const REDACTED: &str = "[REDACTED]";

/// Apply PII/secret redaction to `input` if `enabled` is true.
///
/// Returns the (possibly modified) string. When `enabled` is `false` the
/// function returns immediately with no regex work and no allocations beyond
/// the one `to_string()` call required by the signature.
pub fn redact(input: &str, enabled: bool) -> String {
    if !enabled {
        return input.to_string();
    }

    // Apply each pattern in sequence, counting total replacements.  We run
    // Anthropic before OpenAI so the longer `sk-ant-` prefix is consumed
    // before the shorter `sk-` pattern can fire on the same span.
    let patterns: &[&LazyLock<Regex>] = &[
        &RE_AWS_SECRET_ENV,
        &RE_AWS_KEY_ID,
        &RE_BEARER,
        &RE_ANTHROPIC_KEY,
        &RE_OPENAI_KEY,
        &RE_ENV_VAR,
    ];

    let mut current = input.to_string();
    let mut total_hits: usize = 0;

    for re in patterns {
        let count = re.find_iter(&current).count();
        if count > 0 {
            total_hits += count;
            current = re.replace_all(&current, REDACTED).into_owned();
        }
    }

    if total_hits > 0 {
        tracing::warn!(redaction_hits = total_hits, "redaction applied");
    }

    current
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── positive cases ─────────────────────────────────────────────────────

    #[test]
    fn redacts_aws_key_id() {
        let input = "config: AKIAIOSFODNN7EXAMPLE is set";
        let out = redact(input, true);
        assert!(out.contains(REDACTED), "expected REDACTED in: {out}");
        assert!(
            !out.contains("AKIAIOSFODNN7EXAMPLE"),
            "original key must not appear: {out}"
        );
    }

    #[test]
    fn redacts_bearer_token() {
        let input = "auth: Bearer eyJhbGciOiJIUzI1NiJ9.foo.bar";
        let out = redact(input, true);
        assert!(out.contains(REDACTED), "expected REDACTED in: {out}");
        assert!(
            !out.contains("eyJhbGciOiJIUzI1NiJ9"),
            "token must not appear: {out}"
        );
    }

    #[test]
    fn redacts_openai_key() {
        let input = "key=sk-abcdefghij1234567890";
        let out = redact(input, true);
        assert!(out.contains(REDACTED), "expected REDACTED in: {out}");
        assert!(
            !out.contains("sk-abcdefghij1234567890"),
            "key must not appear: {out}"
        );
    }

    #[test]
    fn redacts_anthropic_key() {
        let input = "key=sk-ant-api03-abc123";
        let out = redact(input, true);
        assert!(out.contains(REDACTED), "expected REDACTED in: {out}");
        assert!(
            !out.contains("sk-ant-api03-abc123"),
            "key must not appear: {out}"
        );
    }

    #[test]
    fn redacts_env_var_assignment() {
        let input = "export API_KEY=supersecret";
        let out = redact(input, true);
        assert!(out.contains(REDACTED), "expected REDACTED in: {out}");
        assert!(!out.contains("supersecret"), "value must not appear: {out}");
    }

    #[test]
    fn redacts_multiple_hits_in_one_pass() {
        // Two distinct patterns: bearer token + env-var assignment.
        let input = "auth: Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig and SECRET=hunter2";
        let out = redact(input, true);
        // Both original values must be gone.
        assert!(
            !out.contains("eyJhbGciOiJIUzI1NiJ9"),
            "bearer token must not appear: {out}"
        );
        assert!(
            !out.contains("hunter2"),
            "secret value must not appear: {out}"
        );
        // At least two [REDACTED] markers present.
        assert!(
            out.matches(REDACTED).count() >= 2,
            "expected ≥2 REDACTED markers: {out}"
        );
    }

    // ── negative / false-positive cases ────────────────────────────────────

    #[test]
    fn does_not_redact_plain_prose() {
        let input = "this is a normal agent output with no secrets";
        let out = redact(input, true);
        assert_eq!(out, input, "plain prose must pass through unmodified");
    }

    #[test]
    fn does_not_redact_sk_prefix_in_word() {
        // "tasks" contains "sk" but not the `sk-` pattern.
        let input = "tasks are in the backlog";
        let out = redact(input, true);
        assert_eq!(out, input, "word 'tasks' must not be redacted");
    }

    #[test]
    fn does_not_redact_short_token_lookalike() {
        // "sk-short" is only 5 chars after "sk-", below the 10-char floor.
        let input = "type: sk-short";
        let out = redact(input, true);
        assert_eq!(out, input, "short sk- token must not be redacted");
    }

    // ── passthrough case ────────────────────────────────────────────────────

    #[test]
    fn passthrough_when_disabled() {
        let input = "AKIAIOSFODNN7EXAMPLE";
        let out = redact(input, false);
        assert_eq!(out, input, "disabled redaction must return input unchanged");
    }
}
