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

/// Top-level subcommands exposed by the `dreamd` binary.
#[derive(Subcommand)]
pub enum Command {
    /// Run health checks and print status (dream-cycle mode, etc.).
    Doctor,
    /// Scaffold per-project .agent/ store and register it with the daemon.
    Init(InitArgs),
    /// Start the MCP server (bridges to daemon if running, otherwise in-process).
    Mcp(McpArgs),
    /// Reset scratch state (DR-113). Today only `workspace` is supported.
    Reset(ResetArgs),
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
        return Err(
            "dream_cycle_mode = auto is not supported at v0.1; \
             set dream_cycle_mode = \"manual\" in config.toml. \
             Auto mode ships at v0.1.1."
                .to_string(),
        );
    }
    Ok(())
}

/// Parse CLI args and dispatch to the matching subcommand handler.
///
/// Returns [`ExitCode`] directly so `main` stays a one-liner.
/// Exit `2` for usage errors; exit `1` for I/O / runtime errors.
pub fn run() -> ExitCode {
    let cli = Cli::parse();

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
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            match commands::doctor::run(&config, &mut out) {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => ExitCode::from(1),
                Err(e) => {
                    eprintln!("dreamd: error — {e}");
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
                commands::init::uninstall_project(&cwd, &daemon_home, args.quiet, &mut out, &mut err)
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
            let mut config = match load_config(&cwd) {
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
            commands::mcp::run(&cwd)
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
        let mut cfg = Config::default();
        cfg.dream_cycle_mode = DreamCycleMode::Auto;
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
