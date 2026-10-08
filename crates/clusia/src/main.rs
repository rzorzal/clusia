mod cli;
mod install;
mod review;
mod run;
mod spawn;

use std::process::ExitCode;

use clap::Parser;
use clusia_core::Paths;

use crate::run::CliError;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    let home = match cli.home.as_deref().map(std::path::absolute).transpose() {
        Ok(home) => home,
        Err(e) => {
            return fail(
                cli.json,
                &CliError::Other(format!("cannot resolve --home: {e}")),
            );
        }
    };
    let paths = match &home {
        Some(home) => Paths::new(home),
        None => match Paths::from_env() {
            Ok(p) => p,
            Err(e) => return fail(cli.json, &CliError::Other(e.to_string())),
        },
    };
    if let Err(e) = clusia_protocol::launcher::check_socket_path(&paths) {
        return fail(cli.json, &e.into());
    }
    match run::run(&paths, home.as_deref(), cli.command).await {
        Ok(out) => {
            if cli.json {
                println!("{}", out.json);
            } else {
                println!("{}", out.human);
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(cli.json, &e),
    }
}

fn fail(json: bool, e: &CliError) -> ExitCode {
    if json {
        eprintln!(
            "{}",
            serde_json::json!({ "error": { "kind": e.kind(), "message": e.to_string() } })
        );
    } else {
        eprintln!("clusia: {e}");
    }
    ExitCode::from(e.exit_code())
}
