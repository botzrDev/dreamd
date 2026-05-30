//! Privacy disclosure surface (DR-413).
//!
//! Hoisted from `dreamd-cli::commands::init` in WEG-17 so future surfaces
//! (README, MCP tool descriptions, error messages) can reuse the locked
//! disclosure text. Stdout output of `dreamd init` is byte-locked against
//! `tests/fixtures/init.golden.txt`; do not modify the text.

/// PRD §5 privacy disclosure, ASCII-rendered, 60-col wrapped (locked verbatim).
pub const DR413_DISCLOSURE: &str = "\
dreamd: first run — When LLM mode is enabled, the content
of AGENT_LEARNINGS.jsonl entries above the relevance
threshold is sent to the configured LLM provider. No data
is sent in --no-llm mode. Users working with sensitive
codebases should use --no-llm or a local model via Ollama.
The personal/ layer is excluded from LLM calls unless
--share-personal is passed.
See docs/security.md#privacy-disclosure for details.";

pub const PRIVACY_DISCLOSURE_LINK: &str = "docs/security.md#privacy-disclosure";

#[cfg(test)]
mod tests {
    #[test]
    fn disclosure_contains_link() {
        assert!(super::DR413_DISCLOSURE.contains(super::PRIVACY_DISCLOSURE_LINK));
    }
}
