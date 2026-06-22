//! Privacy disclosure surface (DR-413).
//!
//! Hoisted from `dreamd-cli::commands::init` in WEG-17 so future surfaces
//! (README, MCP tool descriptions, error messages) can reuse the locked
//! disclosure text. Stdout output of `dreamd init` is byte-locked against
//! `tests/fixtures/init.golden.txt`; do not modify the text.

/// PRD §5 privacy disclosure, ASCII-rendered, 60-col wrapped (locked verbatim).
pub const DR413_DISCLOSURE: &str = "\
dreamd: first run — v0.1 makes no network calls. All memory
operations are local-only on your machine. LLM-assisted dream
cycles and cloud providers are planned for v0.1.1.
See https://github.com/botzrDev/dreamd/blob/main/SECURITY.md
for details.";

pub const PRIVACY_DISCLOSURE_LINK: &str =
    "https://github.com/botzrDev/dreamd/blob/main/SECURITY.md";

#[cfg(test)]
mod tests {
    #[test]
    fn disclosure_contains_link() {
        assert!(super::DR413_DISCLOSURE.contains(super::PRIVACY_DISCLOSURE_LINK));
    }
}
