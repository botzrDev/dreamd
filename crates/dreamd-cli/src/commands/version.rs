//! `dreamd version` — structured version output (DR-707 / WEG-18).
//!
//! Locked output formats are byte-exact contracts; WEG-20 (DR-803) snapshots
//! `VERSION_SHORT` and `render_long()` directly. Metadata is baked at compile
//! time via `vergen-gitcl`; tarball builds without `.git/` fall back to
//! `"unknown"` via `option_env!`.

use std::io::{self, Write};

use const_format::{concatcp, str_index};

const VERSION: &str = env!("CARGO_PKG_VERSION");

// vergen emits this literal when `fail_on_error(false)` and an instruction
// can't be evaluated (tarball builds with no `.git/`, etc.). Treat it as
// equivalent to the `None` arm of `option_env!`.
const VERGEN_PLACEHOLDER: &str = "VERGEN_IDEMPOTENT_OUTPUT";

const fn or_unknown(s: &'static str) -> &'static str {
    if str_eq(s, VERGEN_PLACEHOLDER) {
        "unknown"
    } else {
        s
    }
}

const fn str_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

const SHA_FULL: &str = or_unknown(match option_env!("VERGEN_GIT_SHA") {
    Some(s) => s,
    None => "unknown",
});

pub const SHA: &str = str_index!(SHA_FULL, ..7);

const BUILD_DATE: &str = or_unknown(match option_env!("VERGEN_BUILD_DATE") {
    Some(s) => s,
    None => "unknown",
});

const TARGET: &str = or_unknown(match option_env!("VERGEN_CARGO_TARGET_TRIPLE") {
    Some(s) => s,
    None => "unknown",
});

const SCHEMA: &str = "1.0";

pub const VERSION_SHORT: &str = concatcp!(
    "dreamd ", VERSION, " (", SHA, " build:", BUILD_DATE, " target:", TARGET, " schema:", SCHEMA,
    ")",
);

pub fn render_long() -> String {
    format!(
        "dreamd {VERSION}\n  commit:  {SHA}\n  built:   {BUILD_DATE}\n  target:  {TARGET}\n  schema:  {SCHEMA}\n"
    )
}

pub fn run(out: &mut impl Write) -> io::Result<()> {
    out.write_all(render_long().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Defends against the WEG-18 sentinel-leak class of bug: if vergen can't
    // resolve an instruction it emits the literal "VERGEN_IDEMPOTENT_OUTPUT"
    // as the value. `or_unknown` converts that to "unknown" — these tests
    // assert no `VERGEN_` substring reaches either output form, in both the
    // with-`.git/` and without-`.git/` build paths.
    #[test]
    fn version_short_never_leaks_vergen_sentinel() {
        assert!(
            !VERSION_SHORT.contains("VERGEN_"),
            "VERSION_SHORT must not contain a raw vergen sentinel: {VERSION_SHORT}"
        );
    }

    #[test]
    fn render_long_never_leaks_vergen_sentinel() {
        let long = render_long();
        assert!(
            !long.contains("VERGEN_"),
            "render_long() must not contain a raw vergen sentinel: {long}"
        );
    }

    // `str_index!(SHA_FULL, ..7)` panics at compile time if `SHA_FULL` is
    // shorter than 7 bytes. Both branches (full 40-char git SHA and 7-char
    // "unknown" fallback) satisfy this — assert it so a future change to
    // `or_unknown`'s sentinel-replacement string can't silently truncate
    // crates.io users to a 7-char-prefix that looks like a real SHA.
    #[test]
    fn sha_is_exactly_seven_chars() {
        assert_eq!(SHA.len(), 7, "SHA must be exactly 7 chars; got {SHA:?}");
    }
}
