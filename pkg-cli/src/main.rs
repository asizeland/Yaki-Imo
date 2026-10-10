//! pkg-cli/src/main.rs — `pkg` command-line interface (SPEC "pkg-cli";
//! v11 §16, design doc Issues 1/2/4/5 CLI surfaces).
//!
//! ## v1 changes
//!
//! - Initial implementation: clap-4 derive over the full subcommand
//!   surface (SPEC "pkg-cli": install remove upgrade sync search info
//!   why explain pin unpin check autoremove clean history undo env{...}
//!   build-env audit{,--glibc,--supply-chain} isolate fetch build key
//!   db{check,regenerate} bootstrap apply-pending-restarts cascade).
//!   Process exit codes come from `PkgError::exit_code` (§16.1:
//!   1 = resolution, 2 = build, 3 = gate, 70 = everything else).

use std::process::ExitCode;

use clap::Parser;

mod cli;
mod commands;
mod preview;

use cli::{Cli, Command};

fn main() -> ExitCode {
    let cli = Cli::parse();
    match commands::dispatch(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            // §16.1: the error type carries the exit code, so a
            // resolution failure is distinguishable from a build
            // failure is distinguishable from "something else" by
            // scripts and CI without parsing message text.
            ExitCode::from(e.exit_code() as u8)
        }
    }
}
