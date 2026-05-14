//! Binary entry point for the `dreamd` CLI. All logic lives in [`dreamd::run`].

use std::process::ExitCode;

fn main() -> ExitCode {
    dreamd::run()
}
