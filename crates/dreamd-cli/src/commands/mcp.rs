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
        Err(McpRunError::Io(e)) => {
            eprintln!("dreamd mcp: I/O error — {e}");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("dreamd mcp: {e}");
            ExitCode::from(1)
        }
    }
}
