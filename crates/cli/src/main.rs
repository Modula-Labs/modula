use std::process::ExitCode;

use clap::Parser;
use modula_cli::cli::Cli;
use modula_cli::commands;
use modula_cli::context::Session;
use modula_cli::output::{Output, UsageError};

#[tokio::main]
async fn main() -> ExitCode {
    // Usage errors exit 2, --help and --version 0 (clap's conventions).
    let cli = Cli::try_parse().unwrap_or_else(|e| e.exit());
    let out = Output::stdout(cli.global.json);
    let result = match Session::from_process() {
        Ok(session) => commands::run(cli, session, out).await,
        Err(e) => Err(e),
    };
    match result {
        Ok(0) => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            eprintln!("error: {e:#}");
            if e.downcast_ref::<UsageError>().is_some() {
                ExitCode::from(2)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}
