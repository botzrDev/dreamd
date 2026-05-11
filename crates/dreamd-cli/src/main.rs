use std::path::PathBuf;
use std::process::ExitCode;

use clap::{ArgAction, Parser, Subcommand};

mod commands;

use commands::version::VERSION_SHORT;

#[derive(Parser)]
#[command(
    name = "dreamd",
    disable_version_flag = true,
    about = "Portable memory layer for AI coding agents"
)]
struct Cli {
    /// Print version information.
    #[arg(short = 'V', long = "version", action = ArgAction::SetTrue, global = false)]
    version: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Scaffold per-project .agent/ store and register it with the daemon.
    Init,
    /// Print structured version information (semver, commit, build date, target, schema).
    Version,
}

fn main() -> ExitCode {
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
