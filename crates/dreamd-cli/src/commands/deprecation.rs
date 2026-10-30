//! One-line stderr notices for the surfaces deprecated in 1.2.0 and removed in
//! 2.0 (BZR-1245). The text is stable: `tests/cli_help.rs` snapshots it.

pub const MEMORY_BRANCH: &str = "dreamd: warning — `dreamd memory branch` is deprecated and will be removed in dreamd 2.0; .agent/ is plain files, so use git to branch, diff and bisect it";
pub const MEMORY_CHECKOUT: &str = "dreamd: warning — `dreamd memory checkout` is deprecated and will be removed in dreamd 2.0; .agent/ is plain files, so use git to branch, diff and bisect it";
pub const MEMORY_BRANCHES: &str = "dreamd: warning — `dreamd memory branches` is deprecated and will be removed in dreamd 2.0; .agent/ is plain files, so use git to branch, diff and bisect it";
pub const MEMORY_DELETE: &str = "dreamd: warning — `dreamd memory delete` is deprecated and will be removed in dreamd 2.0; .agent/ is plain files, so use git to branch, diff and bisect it";
pub const MEMORY_DIFF: &str = "dreamd: warning — `dreamd memory diff` is deprecated and will be removed in dreamd 2.0; .agent/ is plain files, so use git to branch, diff and bisect it";
pub const MEMORY_BISECT: &str = "dreamd: warning — `dreamd memory bisect` is deprecated and will be removed in dreamd 2.0; .agent/ is plain files, so use git to branch, diff and bisect it";
pub const VECTORS_ENABLE: &str = "dreamd: warning — `dreamd vectors enable` and the `vectors` cargo feature are deprecated and will be removed in dreamd 2.0; recall never used the model";
pub const DREAM_AUTO: &str = "dreamd: warning — `dreamd dream --auto` is deprecated and will be removed in dreamd 2.0; run `dreamd dream` when you want a cycle";

/// Every notice, in the order above.
pub const ALL: [&str; 8] = [
    MEMORY_BRANCH,
    MEMORY_CHECKOUT,
    MEMORY_BRANCHES,
    MEMORY_DELETE,
    MEMORY_DIFF,
    MEMORY_BISECT,
    VECTORS_ENABLE,
    DREAM_AUTO,
];
