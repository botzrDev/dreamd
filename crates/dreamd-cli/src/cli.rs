//! Top-level CLI dispatch for the `dreamd` binary.
//!
//! Parses args via clap, routes to subcommand handlers, and maps errors to
//! exit codes. Exit code contract:
//!   - `0` -- success
//!   - `1` -- runtime / I/O error
//!   - `2` -- usage error (missing subcommand, no project root)

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{ArgAction, Args, Parser, Subcommand};

use dreamd_core::config::{load_config, Config, DreamCycleMode};

use crate::commands;
use crate::commands::version::VERSION_SHORT;

/// Root CLI parser. Routes to subcommands or handles `--version`.
#[derive(Parser)]
#[command(
    name = "dreamd",
    disable_version_flag = true,
    about = "Portable memory layer for AI coding agents"
)]
pub struct Cli {
    /// Print version information.
    #[arg(short = 'V', long = "version", action = ArgAction::SetTrue, global = false)]
    pub version: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Arguments for the `dreamd dream` subcommand.
#[derive(Args, Debug)]
pub struct DreamArgs {
    /// Dry run: print what would change without writing.
    /// Not yet implemented; ships v0.1.1.
    #[arg(long)]
    pub dry: bool,
    /// Schedule automatic dream cycles.
    /// Not yet supported at v0.1; ships v0.1.1.
    #[arg(long, hide = true)]
    pub auto: bool,
    /// Skip the autobiography commit. The cycle still runs and writes to disk;
    /// only the git2 commit step is skipped. Useful in CI.
    #[arg(long)]
    pub no_commit: bool,
}

impl DreamArgs {
    /// Validate flag combinations. Returns `Err` with a user-facing message if
    /// the combination is invalid.
    pub fn validate(&self) -> Result<(), String> {
        if self.dry && self.auto {
            return Err("--dry and --auto are mutually exclusive: \
                 auto mode performs real writes on a schedule"
                .to_string());
        }
        Ok(())
    }
}

/// Arguments for the `dreamd watch` subcommand.
///
/// Empty for v0.1. Reserved for future flags (`--manual-only`, etc.).
#[derive(Args, Debug)]
pub struct WatchArgs {}

/// Top-level subcommands exposed by the `dreamd` binary.
#[derive(Subcommand)]
pub enum Command {
    /// Run health checks and print status (dream-cycle mode, etc.).
    Doctor,
    /// Run the deterministic dream cycle: promote top cluster to LESSONS.md,
    /// prune decayed episodic events.
    Dream(DreamArgs),
    /// Scaffold per-project .agent/ store and register it with the daemon.
    Init(InitArgs),
    /// Start the MCP server (bridges to daemon if running, otherwise in-process).
    Mcp(McpArgs),
    /// Reset scratch state (DR-113). Today only `workspace` is supported.
    Reset(ResetArgs),
    /// Print daemon liveness, resolved project, last dream cycle, and recent log.
    Status,
    /// Run the daemon in foreground mode. Blocks until SIGINT/SIGTERM.
    Watch(WatchArgs),
    /// Print structured version information (semver, commit, build date, target, schema).
    Version,
}

/// Arguments for the `dreamd init` subcommand.
#[derive(Args)]
pub struct InitArgs {
    /// Suppress non-essential output (state.json, .gitignore, registry, disclosure).
    #[arg(long)]
    pub quiet: bool,
    /// Remove this project's entry from ~/.agent/registry.toml and exit.
    /// Does not delete the project's .agent/ store.
    #[arg(long)]
    pub uninstall_project: bool,
}

/// Arguments for the `dreamd mcp` subcommand.
#[derive(Args)]
pub struct McpArgs {
    /// Hard-lock dream cycle to manual-only mode (overrides config.toml).
    #[arg(long)]
    pub manual_only: bool,
    /// Override the project root used for agent-root discovery.
    /// Required when the IDE launches the MCP server from a non-project CWD
    /// (e.g. Cursor's global ~/.cursor/mcp.json). Must be an absolute path to
    /// the directory that contains `.agent/`.
    #[arg(long, value_name = "PATH")]
    pub project_root: Option<PathBuf>,
}

/// Args for `dreamd reset`. Wraps the nested target subcommand so the shape
/// scales when more reset targets land (e.g., personal, episodic) without
/// reshuffling top-level command parsing.
#[derive(Args)]
pub struct ResetArgs {
    #[command(subcommand)]
    pub command: ResetCommand,
}

#[derive(Subcommand)]
pub enum ResetCommand {
    /// Clear working/WORKSPACE.md back to its initial scaffold contents.
    Workspace {
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}

/// Guard that rejects `Auto` dream-cycle mode at v0.1.
///
/// Returns `Ok(())` for `Manual` mode; `Err(message)` for `Auto` mode.
/// The caller is responsible for printing the error and calling
/// `std::process::exit(1)` — this function does not exit so it can be
/// unit-tested without spawning a subprocess.
pub fn check_dream_mode(config: &Config) -> Result<(), String> {
    if config.dream_cycle_mode == DreamCycleMode::Auto {
        return Err("dream_cycle_mode = auto is not supported at v0.1; \
             set dream_cycle_mode = \"manual\" in config.toml. \
             Auto mode ships at v0.1.1."
            .to_string());
    }
    Ok(())
}

/// Resolve the existing `.agent/` store for a post-init command, walking up
/// from `cwd`. On a miss, print a pointer to `dreamd init` (stderr) and return
/// the exit code the caller must propagate.
///
/// This is the *find* resolver (`AgentRoot::discover`), shared by `dream` and
/// `doctor`. `init` uses its own sentinel walk because it *creates* a store;
/// these commands *find* one. Walking up means `dreamd dream` from a project
/// subdirectory operates on the project's store, and an uninitialized dir
/// errors instead of silently scaffolding an empty `.agent/`.
fn discover_store_or_exit(
    cwd: &std::path::Path,
) -> Result<dreamd_core::layout::AgentRoot, ExitCode> {
    dreamd_core::layout::AgentRoot::discover(cwd).map_err(|_| {
        eprintln!(
            "dreamd: no .agent/ store found in {} or any parent directory — run `dreamd init` first.",
            cwd.display()
        );
        ExitCode::from(2)
    })
}

/// Parse CLI args and dispatch to the matching subcommand handler.
///
/// Returns [`ExitCode`] directly so `main` stays a one-liner.
/// Exit `2` for usage errors; exit `1` for I/O / runtime errors.
pub fn run() -> ExitCode {
    let cli = Cli::parse();

    // WEG-32 / DR-004 — install the tracing subscriber once, before dispatch,
    // so every subcommand's `tracing` callsites land on stderr + ~/.agent/dreamd.log.
    // `_log_guard` MUST live until run() returns — `let _ =` would drop it
    // immediately and discard buffered file logs. Resolve the log path via the
    // existing HOME idiom (see the Init arm below); None → console-only.
    let log_file = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| dreamd_core::layout::DaemonHome::new(h.join(".agent")).log_file());

    // WEG-103 — `dreamd status` tails the daemon log, but init_tracing opens it
    // with truncate(true) at startup (as every subcommand does). Capture the
    // tail BEFORE that truncation so status can surface a running daemon's real
    // recent lines rather than the empty file it just cleared.
    let status_log_tail = if matches!(cli.command, Some(Command::Status)) {
        log_file
            .as_deref()
            .map(commands::status::read_log_tail)
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let _log_guard = dreamd_core::observability::init_tracing(log_file);

    if cli.version {
        println!("{VERSION_SHORT}");
        return ExitCode::SUCCESS;
    }

    let Some(command) = cli.command else {
        eprintln!("dreamd: error — no subcommand given. Try `dreamd --help`.");
        return ExitCode::from(2);
    };

    match command {
        Command::Doctor => {
            let cwd = match std::env::current_dir() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("dreamd: error — could not read current directory: {e}");
                    return ExitCode::from(1);
                }
            };
            let config = match load_config(&cwd) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("dreamd: error — {e}");
                    return ExitCode::from(1);
                }
            };
            let agent_root = match discover_store_or_exit(&cwd) {
                Ok(r) => r,
                Err(code) => return code,
            };
            let skip = dreamd_core::autobiography::read_last_skip(&agent_root);
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            match commands::doctor::run(&config, &agent_root, skip.as_ref(), &mut out) {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => ExitCode::from(1),
                Err(e) => {
                    eprintln!("dreamd: error — {e}");
                    ExitCode::from(1)
                }
            }
        }
        Command::Dream(args) => {
            if let Err(msg) = args.validate() {
                eprintln!("dreamd: {msg}");
                return ExitCode::from(2);
            }
            if args.auto {
                eprintln!(
                    "dreamd: --auto is not yet supported at v0.1; \
                     set dream_cycle_mode = \"manual\" in config.toml. \
                     Auto mode ships at v0.1.1."
                );
                return ExitCode::from(2);
            }
            if args.dry {
                eprintln!("dreamd: --dry is not yet implemented (ships v0.1.1).");
                return ExitCode::from(2);
            }
            let cwd = match std::env::current_dir() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("dreamd: error — {e}");
                    return ExitCode::from(1);
                }
            };
            let config = match load_config(&cwd) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("dreamd: {e}");
                    return ExitCode::from(1);
                }
            };
            if let Err(msg) = check_dream_mode(&config) {
                eprintln!("dreamd: {msg}");
                return ExitCode::from(2);
            }
            let agent_root = match discover_store_or_exit(&cwd) {
                Ok(r) => r,
                Err(code) => return code,
            };
            match commands::dream::run(
                agent_root.project_root(),
                &mut std::io::stdout(),
                args.no_commit,
            ) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("dreamd: {e}");
                    ExitCode::from(1)
                }
            }
        }
        Command::Init(args) => {
            let cwd = match std::env::current_dir() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("dreamd: error — could not read current directory: {e}");
                    return ExitCode::from(1);
                }
            };
            let daemon_home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|h| h.join(".agent"))
                .unwrap_or_else(|| PathBuf::from(".agent"));
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut out = stdout.lock();
            let mut err = stderr.lock();
            let result = if args.uninstall_project {
                commands::init::uninstall_project(
                    &cwd,
                    &daemon_home,
                    args.quiet,
                    &mut out,
                    &mut err,
                )
            } else {
                commands::init::run(&cwd, &daemon_home, args.quiet, &mut out, &mut err)
            };
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(commands::init::InitError::NoProjectRoot) => ExitCode::from(2),
                Err(commands::init::InitError::Io(e)) => {
                    eprintln!("dreamd: error — {e}");
                    ExitCode::from(1)
                }
            }
        }
        Command::Mcp(args) => {
            let cwd = match std::env::current_dir() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("dreamd: error — could not read current directory: {e}");
                    return ExitCode::from(1);
                }
            };
            // --project-root overrides CWD-based agent-root discovery. Required when
            // an IDE launches the MCP server from a non-project CWD.
            let effective_root = match args.project_root {
                Some(p) => p,
                None => cwd,
            };
            let mut config = match load_config(&effective_root) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("dreamd: error — {e}");
                    return ExitCode::from(1);
                }
            };
            // --manual-only overrides config before the Auto guard runs.
            if args.manual_only {
                config.dream_cycle_mode = DreamCycleMode::Manual;
            }
            if let Err(msg) = check_dream_mode(&config) {
                eprintln!("dreamd: error — {msg}");
                std::process::exit(1);
            }
            commands::mcp::run(&effective_root)
        }
        Command::Reset(args) => match args.command {
            ResetCommand::Workspace { yes } => {
                let cwd = match std::env::current_dir() {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("dreamd: error — could not read current directory: {e}");
                        return ExitCode::from(1);
                    }
                };
                let stdout = std::io::stdout();
                let stderr = std::io::stderr();
                let mut out = stdout.lock();
                let mut err = stderr.lock();
                match commands::reset::run_workspace(&cwd, yes, &mut out, &mut err) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(commands::reset::ResetError::NotFound)
                    | Err(commands::reset::ResetError::NotATty)
                    | Err(commands::reset::ResetError::Declined) => ExitCode::from(2),
                    Err(commands::reset::ResetError::Io(e)) => {
                        eprintln!("dreamd: error — {e}");
                        ExitCode::from(1)
                    }
                }
            }
        },
        Command::Status => {
            let cwd = match std::env::current_dir() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("dreamd: error — could not read current directory: {e}");
                    return ExitCode::from(1);
                }
            };
            // Socket path via the shared resolver ($DREAMD_SOCK else ~/.agent/dreamd.sock);
            // registry resolves off $HOME the same way the tracing log path above does.
            // The log tail was captured into `status_log_tail` before init_tracing ran.
            let socket = dreamd_core::client::resolve_daemon_socket();
            let registry_path = std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|h| dreamd_core::layout::DaemonHome::new(h.join(".agent")).registry_toml())
                .unwrap_or_else(|| PathBuf::from("registry.toml"));
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            match commands::status::run(
                &cwd,
                socket.as_deref(),
                &registry_path,
                &status_log_tail,
                &mut out,
            ) {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => ExitCode::from(1),
                Err(e) => {
                    eprintln!("dreamd: error — {e}");
                    ExitCode::from(1)
                }
            }
        }
        Command::Watch(_args) => {
            let cwd = match std::env::current_dir() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("dreamd: error — could not read current directory: {e}");
                    return ExitCode::from(1);
                }
            };
            commands::watch::run(&cwd)
        }
        Command::Version => {
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            match commands::version::run(&mut out) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("dreamd: error — {e}");
                    ExitCode::from(1)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_mode_rejected() {
        let cfg = Config {
            dream_cycle_mode: DreamCycleMode::Auto,
            ..Default::default()
        };
        assert!(
            check_dream_mode(&cfg).is_err(),
            "auto mode must be rejected by check_dream_mode"
        );
    }

    #[test]
    fn manual_mode_accepted() {
        let cfg = Config::default(); // default is Manual
        assert!(
            check_dream_mode(&cfg).is_ok(),
            "manual mode must be accepted by check_dream_mode"
        );
    }
}
