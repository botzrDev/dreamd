use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod commands;

#[derive(Parser)]
#[command(name = "dreamd", version, about = "Portable memory layer for AI coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scaffold per-project .agent/ store and register it with the daemon.
    Init,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Init => {
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
            match commands::init::run(&cwd, &daemon_home, &mut out, &mut err) {
                Ok(()) => ExitCode::SUCCESS,
                Err(commands::init::InitError::NoProjectRoot) => ExitCode::from(2),
                Err(commands::init::InitError::Io(e)) => {
                    eprintln!("dreamd: error — {e}");
                    ExitCode::from(1)
                }
            }
        }
    }
}
