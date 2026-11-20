//! Secret redaction for agent-written content.
//!
//! [`redact`] is the entry point every append uses; [`would_redact`] asks the
//! same question without logging, for `dreamd doctor`. Pass `enabled = false`
//! to skip all regex work; the early return only copies the input.
//!
//! The patterns are a fixed list of 14 shapes, compiled once at first use and
//! applied in this order:
//!
//! - PEM private-key blocks, including one cut off before its `END` line;
//! - the password in a URL's `user:pass@`;
//! - JSON Web Tokens;
//! - API keys with a fixed prefix: Anthropic `sk-ant-`, OpenAI and other
//!   `sk-` keys, Stripe live keys, GitHub tokens (classic and fine-grained),
//!   Slack, npm and Google API keys, and AWS access key ids;
//! - `Bearer` tokens;
//! - assignments whose name ends in a secret word (`password`, `secret`,
//!   `token`, `api_key` and the like), in env, JSON, YAML and header form.
//!
//! A prefixed key or a key block is replaced whole. For a bearer token, a URL
//! password and an assignment, only the value is replaced, so the name and
//! the quotes stay. A bearer token, or an unquoted value after `:` or a spaced
//! `=`, is replaced only when it looks like a credential: 8 or more characters
//! with a letter, and a digit, `+`, `/` or `=`, or 32 or more characters. An
//! assignment keeps numbers, booleans, variable references and values already
//! replaced.
//!
//! The list is not a guarantee. A secret in a shape it does not name is stored
//! as written.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::{Captures, Regex};

const REDACTED: &str = "[REDACTED]";

/// What a pattern's match becomes.
#[derive(Clone, Copy)]
enum Replace {
    /// The whole match.
    Whole,
    /// Group 2, the password between `user:` and `@`.
    UrlPassword,
    /// Group 2, the token after `bearer `, when it is [`token_like`].
    Bearer,
    /// Group 4, the value of a secret-named assignment, by the rules in
    /// [`assignment`].
    Assignment,
}

/// The 14 shapes, in the order [`apply`] runs them. `sk-ant-` runs before
/// `sk-`, so an Anthropic key is one match.
const PATTERN_SOURCES: [(&str, Replace); 14] = [
    // 1. PEM private-key block, up to its END line or the end of the text.
    (
        r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
        Replace::Whole,
    ),
    // 2. URL password: `scheme://user:` then the password, then `@`.
    (
        r"(?i)\b([a-z][a-z0-9+.-]*://[^\s:/@]+:)([^\s@/]+)@",
        Replace::UrlPassword,
    ),
    // 3. JSON Web Token: two base64url segments that start `eyJ`, then the
    //    signature.
    (
        r"\beyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]*",
        Replace::Whole,
    ),
    // 4. Anthropic key.
    (r"\bsk-ant-[A-Za-z0-9_-]{10,}", Replace::Whole),
    // 5. OpenAI and other `sk-` keys, `sk-proj-` and `sk-svcacct-` included.
    (r"\bsk-[A-Za-z0-9_-]{20,}", Replace::Whole),
    // 6. Stripe live secret and restricted keys.
    (r"\b[rs]k_live_[A-Za-z0-9]{10,}", Replace::Whole),
    // 7. GitHub tokens: `ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`.
    (r"\bgh[pousr]_[A-Za-z0-9]{36,}", Replace::Whole),
    // 8. GitHub fine-grained token.
    (r"\bgithub_pat_[A-Za-z0-9_]{22,}", Replace::Whole),
    // 9. Slack token.
    (r"\bxox[abposr]-[A-Za-z0-9-]{10,}", Replace::Whole),
    // 10. npm token.
    (r"\bnpm_[A-Za-z0-9]{20,}", Replace::Whole),
    // 11. Google API key.
    (r"\bAIza[0-9A-Za-z_-]{30,}", Replace::Whole),
    // 12. AWS access key id, long-term (`AKIA`) or temporary (`ASIA`).
    (r"(?:AKIA|ASIA)[0-9A-Z]{16}", Replace::Whole),
    // 13. Bearer token, any case.
    (r"(?i)\b(bearer\s+)([A-Za-z0-9._~+/=-]+)", Replace::Bearer),
    // 14. Secret-named assignment: a name that ends in a secret word, an
    //     optional closing quote (JSON, YAML), `=` or `:`, and the value.
    (
        r#"(?i)\b([A-Za-z0-9_.-]*(?:password|passwd|secret|token|api[_-]?key|secret[_-]?key|access[_-]?key|private[_-]?key))(["']?)(\s*[=:]\s*)("[^"\n]*"|'[^'\n]*'|\S+)"#,
        Replace::Assignment,
    ),
];

static PATTERNS: LazyLock<Vec<(Regex, Replace)>> = LazyLock::new(|| {
    PATTERN_SOURCES
        .iter()
        .map(|&(source, replace)| {
            let re = Regex::new(source)
                .unwrap_or_else(|e| panic!("redaction pattern {source} is invalid: {e}"));
            (re, replace)
        })
        .collect()
});

/// A bearer value, or an unquoted value after `:` or a spaced `=`, that looks
/// like a credential rather than a word: at least 8 characters, at least one
/// ASCII letter, and a digit, one of `+ / =`, or a length of 32 or more.
fn token_like(value: &str) -> bool {
    let len = value.chars().count();
    len >= 8
        && value.chars().any(|c| c.is_ascii_alphabetic())
        && (len >= 32
            || value
                .chars()
                .any(|c| c.is_ascii_digit() || matches!(c, '+' | '/' | '=')))
}

/// An assignment value that is never replaced: empty, all digits, a boolean
/// or null word, a value already replaced, or a variable reference.
fn kept_value(value: &str) -> bool {
    value.is_empty()
        || value.bytes().all(|b| b.is_ascii_digit())
        || matches!(
            value.to_ascii_lowercase().as_str(),
            "true" | "false" | "yes" | "no" | "on" | "off" | "null" | "none"
        )
        || value == REDACTED
        || is_variable_reference(value)
}

/// `$NAME` or `${NAME}`, with `NAME` matching `[A-Za-z_][A-Za-z0-9_]*`.
fn is_variable_reference(value: &str) -> bool {
    let name = match value.strip_prefix("${") {
        Some(rest) => match rest.strip_suffix('}') {
            Some(name) => name,
            None => return false,
        },
        None => match value.strip_prefix('$') {
            Some(name) => name,
            None => return false,
        },
    };
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Row 14's replacement: the name, closing quote and separator stay; the
/// value is replaced when the rules say so, keeping its quotes.
fn assignment(caps: &Captures<'_>) -> String {
    let (separator, raw) = (&caps[3], &caps[4]);
    let quote = raw
        .chars()
        .next()
        .filter(|&q| matches!(q, '"' | '\'') && raw.len() >= 2 && raw.ends_with(q));
    let value = match quote {
        Some(_) => &raw[1..raw.len() - 1],
        None => raw,
    };
    let replace = if kept_value(value) {
        false
    } else if quote.is_some() || separator == "=" {
        true
    } else {
        token_like(value)
    };
    if !replace {
        return caps[0].to_string();
    }
    let q = quote.map(String::from).unwrap_or_default();
    format!("{}{}{separator}{q}{REDACTED}{q}", &caps[1], &caps[2])
}

/// Apply every pattern, in order, and return the text and the number of
/// replacements that changed it. Pure: no logging.
fn apply(input: &str) -> (String, usize) {
    let mut current = input.to_string();
    let mut hits = 0;
    for (re, replace) in PATTERNS.iter() {
        let next = re.replace_all(&current, |caps: &Captures<'_>| {
            let whole = &caps[0];
            let out = match replace {
                Replace::Whole => REDACTED.to_string(),
                Replace::UrlPassword => format!("{}{REDACTED}@", &caps[1]),
                Replace::Bearer if token_like(&caps[2]) => format!("{}{REDACTED}", &caps[1]),
                Replace::Bearer => whole.to_string(),
                Replace::Assignment => assignment(caps),
            };
            if out != whole {
                hits += 1;
            }
            out
        });
        if let Cow::Owned(next) = next {
            current = next;
        }
    }
    (current, hits)
}

/// Redact `input` if `enabled` is true.
///
/// Returns `input` unchanged when `enabled` is false, with no regex work.
/// Otherwise applies the 14 patterns and logs `redaction_hits` at `warn` when
/// one changed the text. Idempotent: a second pass changes nothing.
pub fn redact(input: &str, enabled: bool) -> String {
    if !enabled {
        return input.to_string();
    }
    let (out, hits) = apply(input);
    if hits > 0 {
        tracing::warn!(redaction_hits = hits, "redaction applied");
    }
    out
}

/// Whether `redact(input, true)` would change `input`. Does not log; `dreamd
/// doctor` calls it once per stored record.
pub fn would_redact(input: &str) -> bool {
    apply(input).0 != input
}

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

    // ── DRM-105: the shipped shapes, value-only replacement ────────────────

    // Shapes 6, 9 and 11 use fakes shorter than a real key, or letters where a real key has digits, so secret scanning does not read them as live keys.
    /// Test 1's table: every named and prefixed shape, and what it becomes.
    const SHAPES: &[(&str, &str)] = &[
        (
            "openai project key sk-proj-FAKEfake_1234567890-abcdefghijklmnopqrst",
            "openai project key [REDACTED]",
        ),
        (
            "openai admin key sk-admin-FAKEfakeFAKEfake1234567890",
            "openai admin key [REDACTED]",
        ),
        (
            "openai service key sk-svcacct-FAKEfakeFAKEfake1234567890",
            "openai service key [REDACTED]",
        ),
        ("key=sk-ant-api03-abc123", "key=[REDACTED]"),
        (
            "Authorization: Bearer ZmFrZS1zZWNyZXQ+ZXQ/dG9rZW4=",
            "Authorization: Bearer [REDACTED]",
        ),
        (
            "header authorization: bearer fakelowertoken123456",
            "header authorization: bearer [REDACTED]",
        ),
        (
            r#"PASSWORD="fake pass phrase here""#,
            r#"PASSWORD="[REDACTED]""#,
        ),
        (
            "password=fakelowerpw1 in the yaml",
            "password=[REDACTED] in the yaml",
        ),
        (
            "Django SECRET_KEY=fakedjangosecret42",
            "Django SECRET_KEY=[REDACTED]",
        ),
        ("CLIENT_SECRET=abc123xyz", "CLIENT_SECRET=[REDACTED]"),
        ("export API_KEY=supersecret", "export API_KEY=[REDACTED]"),
        (
            r#"{"api_key": "fakejsonkey789"}"#,
            r#"{"api_key": "[REDACTED]"}"#,
        ),
        (
            "'password': 'fake-single-quoted'",
            "'password': '[REDACTED]'",
        ),
        ("token: fakeyamltoken321", "token: [REDACTED]"),
        ("Api-Key: fakeHeaderKey123", "Api-Key: [REDACTED]"),
        (
            "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "AWS_SECRET_ACCESS_KEY=[REDACTED]",
        ),
        (
            "config: AKIAIOSFODNN7EXAMPLE is set",
            "config: [REDACTED] is set",
        ),
        (
            "aws temp key ASIAFAKEFAKEFAKE5678",
            "aws temp key [REDACTED]",
        ),
        (
            "pushed with ghp_FAKEfakeFAKEfakeFAKEfakeFAKEfake1234",
            "pushed with [REDACTED]",
        ),
        (
            "github_pat_11FAKEFAKE0fakefakefake_FAKEfakeFAKEfake",
            "[REDACTED]",
        ),
        ("bot token xoxb-FAKE-fake-FAKEfake", "bot token [REDACTED]"),
        ("stripe sk_live_FAKEfake12", "stripe [REDACTED]"),
        (
            "maps key AIzaFAKEfakeFAKEfakeFAKEfakeFAKEfa",
            "maps key [REDACTED]",
        ),
        (
            "//registry.npmjs.org/:_authToken=npm_FAKEfakeFAKEfakeFAKEfake1234",
            "//registry.npmjs.org/:_authToken=[REDACTED]",
        ),
        (
            "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJmYWtlIn0.ZmFrZXNpZ2",
            "jwt [REDACTED]",
        ),
        (
            "DATABASE_URL=postgres://admin:fakedbpass@db.invalid/app",
            "DATABASE_URL=postgres://admin:[REDACTED]@db.invalid/app",
        ),
        (
            "cloned https://fakeuser:fakeghpass@github.invalid/x.git",
            "cloned https://fakeuser:[REDACTED]@github.invalid/x.git",
        ),
        (
            "-----BEGIN OPENSSH PRIVATE KEY----- b3BlbnNzaC1rZXktdjEAAAAAFAKEFAKEFAKE \
             -----END OPENSSH PRIVATE KEY-----",
            "[REDACTED]",
        ),
        (
            "pasted -----BEGIN RSA PRIVATE KEY----- MIIEFAKEFAKE truncated",
            "pasted [REDACTED]",
        ),
        (
            "auth: Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig and SECRET=hunter2",
            "auth: Bearer [REDACTED] and SECRET=[REDACTED]",
        ),
    ];

    /// Test 2's list: ordinary text that must come back unchanged.
    const ORDINARY: &[&str] = &[
        "Use Bearer authentication for the admin API, not cookies",
        "The 401 came from a missing Bearer header on /internal",
        "set MAX_TOKEN=4096 for the summarizer call",
        "the env var NO_SECRET=1 disables the vault lookup in tests",
        "NO_VAULT_TOKEN=1",
        "USE_TOKEN=true",
        "NPM_TOKEN=${NPM_TOKEN}",
        "API_KEY=$OPENAI_API_KEY",
        "TOKENIZER=cl100k_base",
        "max_tokens: 1024",
        "the secret: keep the cache warm",
        "Token: the session id expires after an hour",
        "token = get_token()",
        "if token == expected { return }",
        "task-based-approach-with-long-name",
        "tasks are in the backlog",
        "type: sk-short",
        "ssh://git@github.com/x.git",
        "see https://example.com:8080/path",
        "Authorization: Bearer <token>",
    ];

    /// Test 4's two sides of each `token_like` and keep-rule limit.
    const KEPT_EDGES: &[&str] = &[
        "Bearer abcdef1",
        "Bearer abcdefghijklmnopqrstuvwxyzabcde",
        "Bearer 12345678",
        "api_key: abcdefgh",
        r#"password="""#,
        "SECRET=no",
    ];
    const REDACTED_EDGES: &[(&str, &str)] = &[
        ("Bearer abcdefg1", "Bearer [REDACTED]"),
        (
            "Bearer abcdefghijklmnopqrstuvwxyzabcdef",
            "Bearer [REDACTED]",
        ),
        ("api_key: abcdefg1", "api_key: [REDACTED]"),
        ("SECRET=x", "SECRET=[REDACTED]"),
    ];

    /// DRM-105 test 1: every shipped shape, byte for byte.
    #[test]
    fn redacts_every_named_and_prefixed_shape() {
        for (input, expected) in SHAPES {
            assert_eq!(redact(input, true), *expected, "input: {input}");
        }
    }

    /// DRM-105 test 2: ordinary words, numbers, booleans, references and
    /// code are kept.
    #[test]
    fn keeps_ordinary_text() {
        for input in ORDINARY {
            assert_eq!(redact(input, true), *input, "input: {input}");
        }
    }

    /// DRM-105 test 3: a second pass changes nothing, and `would_redact` says
    /// exactly whether the first pass changes the text.
    #[test]
    fn redact_is_idempotent_and_would_redact_agrees() {
        let inputs = SHAPES
            .iter()
            .map(|(input, _)| *input)
            .chain(ORDINARY.iter().copied());
        for input in inputs {
            let once = redact(input, true);
            assert_eq!(redact(&once, true), once, "idempotent: {input}");
            assert_eq!(would_redact(input), once != input, "would_redact: {input}");
        }
    }

    /// DRM-105 test 4: both sides of the `token_like` limits and the keep
    /// rules.
    #[test]
    fn token_like_edges() {
        for input in KEPT_EDGES {
            assert_eq!(redact(input, true), *input, "kept: {input}");
        }
        for (input, expected) in REDACTED_EDGES {
            assert_eq!(redact(input, true), *expected, "redacted: {input}");
        }
    }

    // ── passthrough case ────────────────────────────────────────────────────

    #[test]
    fn passthrough_when_disabled() {
        let input = "AKIAIOSFODNN7EXAMPLE";
        let out = redact(input, false);
        assert_eq!(out, input, "disabled redaction must return input unchanged");
    }
}
