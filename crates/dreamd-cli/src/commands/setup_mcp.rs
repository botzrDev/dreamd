//! MCP config merge writer for `dreamd setup` (AILAB-550).
//!
//! Splits into a pure classifier ([`classify`]) and the disk-touching
//! [`plan`] / [`apply`] pair so the ratified AILAB-548 §3 taxonomy is
//! unit-testable without a filesystem.

use std::path::{Path, PathBuf};

use dreamd_core::io::write_atomic;
use serde_json::{Map, Value};

/// Verdict for an existing `mcpServers.dreamd` entry (AILAB-548 §3).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// `command == "npx"` and some `args` element is exactly `dreamd-mcp`.
    Compatible,
    /// Anything else. Carries the reason shown on stderr.
    Conflict(String),
}

/// The unscoped npm package we wire, matched exactly (AGENTS.md
/// `npm-dreamd-mcp-unscoped`).
const PACKAGE: &str = "dreamd-mcp";

/// Classify an existing `mcpServers.dreamd` block against the floating pin.
///
/// Compatible iff BOTH `command == "npx"` AND some `args` element is exactly
/// the string `dreamd-mcp`. Everything else conflicts, and the match is exact:
/// a version-pinned token, a scoped name, or a longer package name all conflict
/// because none of them re-resolve to the `latest` dist-tag we ship.
pub(crate) fn classify(block: &Value) -> Verdict {
    let Some(obj) = block.as_object() else {
        return Verdict::Conflict("entry is not a JSON object".into());
    };
    let command = match obj.get("command").and_then(Value::as_str) {
        Some(c) => c,
        None => return Verdict::Conflict("entry has no string \"command\" field".into()),
    };
    if command != "npx" {
        return Verdict::Conflict(format!("command is {command:?}, expected \"npx\""));
    }
    let args = obj.get("args").and_then(Value::as_array);
    let runs_package = args.is_some_and(|args| {
        args.iter()
            .any(|a| a.as_str().is_some_and(|a| a == PACKAGE))
    });
    if !runs_package {
        let found = args.map_or_else(
            || "none".to_string(),
            |args| serde_json::Value::Array(args.clone()).to_string(),
        );
        return Verdict::Conflict(format!(
            "args do not run the floating {PACKAGE} package (found: {found})"
        ));
    }
    Verdict::Compatible
}

/// What [`apply`] would do to one target file.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// No file yet — write a fresh document.
    Create,
    /// File exists without a compatible `dreamd` entry to keep — merge ours in.
    Merge,
    /// Already points at the floating `dreamd-mcp` package — leave it alone.
    AlreadyWired,
    /// Occupied by an incompatible entry; only `--force` may replace it.
    Conflict(String),
}

/// One target file plus the document that would be written to it.
#[derive(Debug)]
pub(crate) struct TargetPlan {
    /// Absolute path on disk.
    pub path: PathBuf,
    /// Project-relative label used in every user-facing message.
    pub label: &'static str,
    pub action: Action,
    /// Exact bytes [`apply`] would write. `None` for [`Action::AlreadyWired`].
    pub contents: Option<String>,
}

/// Failure modes of the MCP merge. All map to exit code 1 (AILAB-548 §5).
#[derive(Debug)]
pub enum McpError {
    /// Existing config is not parseable JSON. `--force` cannot override this
    /// (AILAB-548 §6C) — we never overwrite a file we could not read.
    Malformed { label: &'static str, detail: String },
    /// An incompatible `dreamd` entry blocked the write and `--force` was absent.
    Conflict { label: &'static str, reason: String },
    Io {
        label: &'static str,
        source: std::io::Error,
    },
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed { label, detail } => {
                write!(f, "{label} is not valid JSON: {detail}")
            }
            Self::Conflict { label, reason } => {
                write!(f, "{label} has a conflicting \"dreamd\" entry: {reason}")
            }
            Self::Io { label, source } => write!(f, "{label}: {source}"),
        }
    }
}

impl std::error::Error for McpError {}

/// Read + classify one target without writing anything.
///
/// `label` is the project-relative path (`.mcp.json`, `.cursor/mcp.json`); it
/// doubles as the user-facing name in every message.
pub(crate) fn plan(project_root: &Path, label: &'static str) -> Result<TargetPlan, McpError> {
    let path = project_root.join(label);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut servers = Map::new();
            servers.insert("dreamd".into(), floating_block());
            let mut doc = Map::new();
            doc.insert("mcpServers".into(), Value::Object(servers));
            return Ok(TargetPlan {
                path,
                label,
                action: Action::Create,
                contents: Some(render(&Value::Object(doc))),
            });
        }
        Err(source) => return Err(McpError::Io { label, source }),
    };

    // A file we cannot parse is never overwritten — not even with `--force`
    // (AILAB-548 §6C).
    let parsed: Value = serde_json::from_str(&raw).map_err(|e| McpError::Malformed {
        label,
        detail: e.to_string(),
    })?;
    let Value::Object(mut doc) = parsed else {
        return Err(McpError::Malformed {
            label,
            detail: "top-level value is not a JSON object".into(),
        });
    };
    let mut servers = match doc.remove("mcpServers") {
        None => Map::new(),
        Some(Value::Object(servers)) => servers,
        Some(_) => {
            return Err(McpError::Malformed {
                label,
                detail: "\"mcpServers\" is not a JSON object".into(),
            })
        }
    };

    let action = match servers.get("dreamd") {
        None => Action::Merge,
        Some(existing) => match classify(existing) {
            Verdict::Compatible => Action::AlreadyWired,
            Verdict::Conflict(reason) => Action::Conflict(reason),
        },
    };
    if action == Action::AlreadyWired {
        // Byte-preserving no-op: we do not reformat a file we agree with.
        return Ok(TargetPlan {
            path,
            label,
            action,
            contents: None,
        });
    }

    servers.insert("dreamd".into(), floating_block());
    doc.insert("mcpServers".into(), Value::Object(servers));
    Ok(TargetPlan {
        path,
        label,
        action,
        contents: Some(render(&Value::Object(doc))),
    })
}

/// Write the planned document, creating parent directories (`.cursor/`) as
/// needed. A no-op for [`Action::AlreadyWired`].
///
/// The write is atomic ([`write_atomic`], as `init` does for `registry.toml`)
/// because the target holds OTHER people's MCP servers: a truncating write that
/// dies mid-flight would destroy them and leave JSON we can no longer parse —
/// which is exactly the §6C malformed path `--force` deliberately cannot clear,
/// so the user would be locked out of their own repair route.
pub(crate) fn apply(planned: &TargetPlan) -> Result<(), McpError> {
    let Some(contents) = planned.contents.as_deref() else {
        return Ok(());
    };
    let label = planned.label;
    if let Some(parent) = planned.path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| McpError::Io { label, source })?;
    }
    write_atomic(&planned.path, contents.as_bytes())
        .map_err(|source| McpError::Io { label, source })
}

/// 2-space pretty JSON plus a trailing newline (AILAB-548 §6B). The user's
/// original formatting is deliberately not preserved.
fn render(doc: &Value) -> String {
    let mut rendered =
        serde_json::to_string_pretty(doc).expect("a serde_json::Value always serializes");
    rendered.push('\n');
    rendered
}

/// The floating-pin block: `npx -y dreamd-mcp`, never a hard `@version`
/// (AGENTS.md `npm-dreamd-mcp-unscoped`).
fn floating_block() -> Value {
    let mut block = Map::new();
    block.insert("command".into(), Value::String("npx".into()));
    block.insert(
        "args".into(),
        Value::Array(vec![
            Value::String("-y".into()),
            Value::String("dreamd-mcp".into()),
        ]),
    );
    Value::Object(block)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reason(v: Verdict) -> String {
        match v {
            Verdict::Compatible => panic!("expected a conflict, got Compatible"),
            Verdict::Conflict(r) => r,
        }
    }

    #[test]
    fn exact_floating_block_is_compatible() {
        let block = json!({"command": "npx", "args": ["-y", "dreamd-mcp"]});
        assert_eq!(classify(&block), Verdict::Compatible);
    }

    #[test]
    fn floating_block_with_extra_args_is_compatible() {
        let block = json!({"command": "npx", "args": ["-y", "dreamd-mcp", "--project-root", "/x"]});
        assert_eq!(classify(&block), Verdict::Compatible);
    }

    #[test]
    fn hard_pinned_package_conflicts() {
        let pinned = json!({"command": "npx", "args": ["-y", "dreamd-mcp@1.2.3"]});
        assert!(reason(classify(&pinned)).contains("dreamd-mcp"));
    }

    #[test]
    fn scoped_package_name_conflicts() {
        let block = json!({"command": "npx", "args": ["-y", "@dataprime1/dreamd-mcp"]});
        assert!(matches!(classify(&block), Verdict::Conflict(_)));
    }

    #[test]
    fn package_name_prefix_conflicts() {
        let block = json!({"command": "npx", "args": ["-y", "dreamd-mcp-foo"]});
        assert!(matches!(classify(&block), Verdict::Conflict(_)));
    }

    #[test]
    fn non_npx_command_conflicts() {
        let block = json!({"command": "cargo", "args": ["run", "-p", "dreamd", "--", "mcp"]});
        assert!(reason(classify(&block)).contains("cargo"));
    }

    #[test]
    fn absolute_binary_command_conflicts() {
        let block = json!({"command": "/usr/local/bin/dreamd", "args": ["mcp"]});
        assert!(reason(classify(&block)).contains("/usr/local/bin/dreamd"));
    }

    #[test]
    fn npx_without_the_package_conflicts() {
        let block = json!({"command": "npx", "args": ["-y", "some-other-mcp"]});
        assert!(matches!(classify(&block), Verdict::Conflict(_)));
    }

    #[test]
    fn missing_command_conflicts() {
        let block = json!({"args": ["-y", "dreamd-mcp"]});
        assert!(matches!(classify(&block), Verdict::Conflict(_)));
    }

    #[test]
    fn non_object_entry_conflicts() {
        assert!(matches!(
            classify(&json!("npx -y dreamd-mcp")),
            Verdict::Conflict(_)
        ));
    }

    #[test]
    fn missing_args_conflicts() {
        let block = json!({"command": "npx"});
        assert!(matches!(classify(&block), Verdict::Conflict(_)));
    }

    #[test]
    fn floating_block_is_never_hard_pinned() {
        let block = floating_block();
        assert_eq!(classify(&block), Verdict::Compatible);
        assert_eq!(block["command"], json!("npx"));
        assert_eq!(block["args"], json!(["-y", "dreamd-mcp"]));
    }
}
