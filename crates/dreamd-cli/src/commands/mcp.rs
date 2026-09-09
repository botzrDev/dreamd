//! `dreamd mcp` — start the MCP server process (WEG-77).
//!
//! Thin CLI wrapper around [`dreamd_core::mcp::run_mcp_server`]. Builds a
//! single-threaded Tokio runtime (the MCP bridge is I/O-bound; multi-thread
//! would be overkill and the CLI crate already owns the `rt-multi-thread`
//! feature flag for the daemon path).

use std::path::Path;
use std::process::ExitCode;

use dreamd_core::mcp::{run_mcp_server, McpRunError};

/// Entry point for `dreamd mcp`.
///
/// Blocks until the MCP session ends (either the stdio transport closes or the
/// daemon bridge disconnects). All MCP JSON-RPC traffic uses stdout; all
/// diagnostic / error output uses stderr.
pub fn run(cwd: &Path) -> ExitCode {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    match rt.block_on(run_mcp_server(cwd)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            match &e {
                McpRunError::Io(io) => eprintln!("dreamd mcp: I/O error — {io}"),
                _ => eprintln!("dreamd mcp: {e}"),
            }
            ExitCode::from(exit_code_for(&e))
        }
    }
}

/// Raw exit code for a failed MCP run. Split out of [`run`] so the mapping is
/// unit-testable without spawning a runtime or a stdio transport.
///
/// [`McpRunError::Unsupported`] is a usage refusal (AILAB-174: no Windows
/// daemon until DR-121), so it exits **2** — the same code `dreamd watch` uses
/// on that platform. Everything else is a runtime failure: 1.
pub(crate) fn exit_code_for(e: &McpRunError) -> u8 {
    match e {
        McpRunError::Unsupported => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_is_a_usage_refusal_exit_2() {
        assert_eq!(exit_code_for(&McpRunError::Unsupported), 2);
    }

    #[test]
    fn other_errors_stay_exit_1() {
        assert_eq!(exit_code_for(&McpRunError::NoHome), 1);
        assert_eq!(exit_code_for(&McpRunError::Service("boom".to_string())), 1);
    }
}
