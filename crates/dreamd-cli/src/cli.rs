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

use crate::commands;
use crate::commands::version::VERSION_SHORT;

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

#[derive(Subcommand)]
pub enum Command {
    /// Scaffold per-project .agent/ store and register it with the daemon.
    Init,
    /// Reset scratch state (DR-113). Today only `workspace` is supported.
    Reset(ResetArgs),
    /// Print structured version information (semver, commit, build date, target, schema).
    Version,
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
        Command::Init => {
            let cwd = match std::env::current_dir() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("dreamd: error — could not read current directory: {e}");
                    return ExitCode::from(1);
                }
            };
            // Resolve daemon home to ~/.agent/. Falls back to a relative
            // ".agent" if $HOME is unset (e.g., containerized CI); real
            // registry writes are deferred to DR-412.
            let daemon_home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|h| h.join(".agent"))
                .unwrap_or_else(|| PathBuf::from(".agent"));
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut out = stdout.lock();
            let mut err = stderr.lock();
            match commands::init::run(&cwd, &daemon_home, &mut out, &mut err) {
                Ok(()) => ExitCode::SUCCESS,
                Err(commands::init::InitError::NoProjectRoot) => ExitCode::from(2),
                Err(commands::init::InitError::Io(e)) => {
                    eprintln!("dreamd: error — {e}");
                    ExitCode::from(1)
                }
            }
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
