//! Privacy disclosure surface (DR-413).
//!
//! Hoisted from `dreamd-cli::commands::init` in WEG-17 so future surfaces
//! (README, MCP tool descriptions, error messages) can reuse the locked
//! disclosure text. Stdout output of `dreamd init` is byte-locked against
//! `tests/fixtures/init.golden.txt`. Keep both in lockstep — a release that
//! changes whether the daemon can make network calls must rewrite this text.

/// Privacy disclosure, ASCII-rendered, 60-col wrapped (locked against
/// `tests/fixtures/init.golden.txt`).
pub const DR413_DISCLOSURE: &str = "\
dreamd: first run — memory stays local. No network calls
unless you opt into an LLM-assisted dream cycle (API key
present; --no-llm stays offline).
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
